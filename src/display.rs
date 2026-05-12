use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_graphics::diff::find_different_rects_sub;
use ironrdp_server::{
    Avc420Update, BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat, RdpServerDisplay,
    RdpServerDisplayUpdates,
};
use screencapturekit::async_api::{AsyncSCShareableContent, AsyncSCStream};
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::{
    PixelFormat as SckPixelFormat, SCContentFilter, SCStreamConfiguration, SCStreamOutputType,
};
use std::collections::VecDeque;
use std::num::{NonZeroU16, NonZeroUsize};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time;
// MissedTickBehavior is used in start() to configure the fps_interval
#[allow(unused_imports)]
use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};

use crate::h264::{H264Frame, VtH264Encoder};

// ─── Capture mode ───────────────────────────────────────────────────────────

/// Whether to capture in BGRA (fall back) or YCbCr 4:2:0 (H.264) mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    /// Capture BGRA, send as raw/BitmapUpdate (default).
    Bgra,
    /// Capture YCbCr 4:2:0, encode with VideoToolbox H.264, send as Avc420Update.
    H264,
}

impl Default for CaptureMode {
    fn default() -> Self {
        Self::Bgra
    }
}

// ─── Target frame rate ──────────────────────────────────────────────────────

/// Default target frame rate (matches `with_fps(30)` on `SCStreamConfiguration`).
pub const TARGET_FPS: u32 = 30;

/// Frame interval in milliseconds derived from [`TARGET_FPS`].
#[allow(dead_code)]
pub const FRAME_INTERVAL_MS: u64 = 1000 / TARGET_FPS as u64; // 33 ms

// ─── Public display handler ──────────────────────────────────────────────────

pub struct MacDisplay {
    /// Last size requested by the client via DisplayControl. None = use native.
    current_size: Option<DesktopSize>,
    resize_tx: watch::Sender<Option<DesktopSize>>,
    resize_rx: watch::Receiver<Option<DesktopSize>>,
    /// Capture mode: BGRA fallback or H.264 via VideoToolbox.
    mode: CaptureMode,
    /// Target FPS (default: 30).
    target_fps: u32,
}

impl MacDisplay {
    #[allow(dead_code)]
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(None);
        Self { current_size: None, resize_tx: tx, resize_rx: rx, mode: CaptureMode::default(), target_fps: TARGET_FPS }
    }

    /// Create with a specific capture mode.
    pub fn with_mode(mode: CaptureMode) -> Self {
        let (tx, rx) = watch::channel(None);
        Self { current_size: None, resize_tx: tx, resize_rx: rx, mode, target_fps: TARGET_FPS }
    }
}

#[async_trait]
impl RdpServerDisplay for MacDisplay {
    async fn size(&mut self) -> DesktopSize {
        // Return the client-requested size if we have one, so that after a
        // DisplayControl resize the reactivated session uses the right dimensions.
        if let Some(size) = self.current_size {
            return size;
        }
        match primary_display_size().await {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    "Could not query display size ({e:#}); defaulting to 1920×1080. \
                     Grant Screen Recording in System Settings → Privacy & Security."
                );
                DesktopSize { width: 1920, height: 1080 }
            }
        }
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        let size = self.size().await;
        debug!(
            "Starting display stream at {}×{} (mode={:?}, target_fps={})",
            size.width, size.height, self.mode, self.target_fps
        );
        // Mark the current resize value as seen before cloning.
        let _ = self.resize_rx.borrow_and_update();
        let updates =
            MacDisplayUpdates::start(size.width, size.height, self.mode, self.target_fps, self.resize_rx.clone()).await?;
        Ok(Box::new(updates))
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        if let Some(monitor) = layout.monitors().first() {
            let (w, h) = monitor.dimensions();
            // Cap resolution to prevent issues with Microsoft RDP which doesn't
            // properly handle dynamic resolution changes. Retina displays can
            // request very high resolutions (e.g. 2940×1846) that cause disconnects.
            let max_width = 2560u32;
            let max_height = 1440u32;
            let size = DesktopSize {
                width:  w.min(max_width).min(u16::MAX as u32) as u16,
                height: h.min(max_height).min(u16::MAX as u32) as u16,
            };
            debug!(
                "request_layout: client wants {}×{} (capped to {}×{}), current_size={:?}",
                w, h, size.width, size.height, self.current_size
            );
            if self.current_size == Some(size) {
                debug!("request_layout: already at {}×{}, skipping", size.width, size.height);
                return;
            }
            self.current_size = Some(size);
            let _ = self.resize_tx.send(Some(size));
        }
    }
}

// ─── Display update stream ───────────────────────────────────────────────────

struct PrevFrame {
    data:   Bytes,
    stride: usize,
    width:  usize,
    height: usize,
}

pub struct MacDisplayUpdates {
    stream:       AsyncSCStream,
    stream_size:  DesktopSize,
    mode:         CaptureMode,
    prev:         Option<PrevFrame>,
    pending:      VecDeque<BitmapUpdate>,
    resize_rx:    watch::Receiver<Option<DesktopSize>>,
    /// VideoToolbox H.264 encoder (only for H264 mode).
    h264_encoder: Option<VtH264Encoder>,
    /// Rate-limiting interval — ensures we process at most `target_fps` frames/sec.
    fps_interval:  time::Interval,
    /// SCKit sometimes delivers samples with null image buffers even though
    /// the stream is running. Track consecutive failures to detect this.
    missed_frames: u32,
    target_fps:    u32,
}

impl MacDisplayUpdates {
    async fn start(
        width: u16,
        height: u16,
        mode: CaptureMode,
        target_fps: u32,
        resize_rx: watch::Receiver<Option<DesktopSize>>,
    ) -> Result<Self> {
        let stream = create_stream(width, height, mode, target_fps).await?;
        let h264_encoder = match mode {
            CaptureMode::H264 => Some(VtH264Encoder::new(width, height)?),
            CaptureMode::Bgra => None,
        };

        // Software rate limiter: ticks at the target frame rate.
        // `MissedTickBehavior::Skip` means if we fall behind we don't try to
        // "catch up" — we just skip missed ticks, preventing frame bursts.
        let mut fps_interval = time::interval(Duration::from_millis(1000 / target_fps as u64));
        fps_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Note: we don't `.tick()` on creation; the first `tick()` returns
        // immediately so the very first frame is never delayed.

        Ok(Self {
            stream,
            stream_size: DesktopSize { width, height },
            mode,
            prev: None,
            pending: VecDeque::new(),
            resize_rx,
            h264_encoder,
            fps_interval,
            missed_frames: 0,
            target_fps,
        })
    }
}

impl Drop for MacDisplayUpdates {
    fn drop(&mut self) {
        if let Err(e) = self.stream.stop_capture() {
            debug!("stop_capture: {e}");
        }
    }
}

#[async_trait]
impl RdpServerDisplayUpdates for MacDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        loop {
            // ── 1. Handle pending resize FIRST (before waiting for frames) ────
            if self.resize_rx.has_changed().unwrap_or(false) {
                let new_size = self.resize_rx.borrow_and_update().clone();
                if let Some(size) = new_size {
                    if size != self.stream_size {
                        debug!("Signalling resize to {}×{}", size.width, size.height);
                        return Ok(Some(DisplayUpdate::Resize(size)));
                    }
                }
            }

            // ── 2. Drain buffered sub-region updates (BGRA mode) ──────────────
            if let Some(update) = self.pending.pop_front() {
                return Ok(Some(DisplayUpdate::Bitmap(update)));
            }

            // ── 3. Rate-limit: wait until the next frame slot ─────────────────
            // This prevents encoding more than `target_fps` frames/sec even if
            // SCKit delivers faster (e.g. 120 Hz ProMotion displays). The
            // `MissedTickBehavior::Skip` policy (set in `start()`) means we
            // never burst-compensate — we just process the freshest frame.
            //
            // Cancellation safety: `tokio::time::Interval::tick()` is
            // cancellation-safe, so this works correctly inside `tokio::select!`.
            self.fps_interval.tick().await;

            // ── 4. Wait for the next SCKit frame ──────────────────────────────
            let Some(sample) = self.stream.next().await else {
                debug!("Stream ended (no more samples)");
                return Ok(None);
            };

            let Some(pixel_buf) = sample.image_buffer() else {
                self.missed_frames += 1;
                // SCKit sometimes enters a dead state delivering null buffers.
                // Recreate the stream after 5 consecutive misses (~160ms at 30fps).
                if self.missed_frames >= 5 {
                    warn!("SCKit stream stalled ({} missed frames), recreating", self.missed_frames);
                    match create_stream(self.stream_size.width, self.stream_size.height, self.mode, self.target_fps).await {
                        Ok(new_stream) => {
                            self.stream = new_stream;
                            self.missed_frames = 0;
                            // Keep previous frame data for dirty-rect diffing
                            // instead of forcing a full refresh.
                        }
                        Err(e) => warn!("Failed to recreate SCKit stream: {e}"),
                    }
                }
                continue;
            };
            self.missed_frames = 0;

            match self.mode {
                CaptureMode::Bgra => {
                    if let Some(update) = self.handle_bgra_frame(pixel_buf)? {
                        return Ok(Some(update));
                    }
                    // No update this tick (e.g. frame identical to prev) → loop
                    continue;
                }
                CaptureMode::H264 => {
                    if let Some(update) = self.handle_ycbcr_frame(pixel_buf)? {
                        return Ok(Some(update));
                    }
                    // No update this tick (e.g. encoding skipped/dropped) → loop
                    continue;
                }
            }
        }
    }
}

impl MacDisplayUpdates {
    /// Handle a BGRA frame (the original path).
    /// Returns `Some(DisplayUpdate)` when there's something to send,
    /// or `None` if the frame was identical to the previous one.
    fn handle_bgra_frame(&mut self, pixel_buf: screencapturekit::cv::CVPixelBuffer) -> Result<Option<DisplayUpdate>> {
        let guard = pixel_buf
            .lock(CVPixelBufferLockFlags::READ_ONLY)
            .map_err(|code| anyhow!("CVPixelBuffer lock failed: {code}"))?;

        let w      = guard.width();
        let h      = guard.height();
        let stride = guard.bytes_per_row();
        let new_data = Bytes::copy_from_slice(guard.as_slice());
        drop(guard);

        let new_bitmap = make_bitmap(new_data.clone(), w, h, stride)?;

        // First frame or resolution change → full refresh
        let prev = match &self.prev {
            None => {
                debug!("First frame at {}×{} (full refresh)", w, h);
                self.prev = Some(PrevFrame { data: new_data, stride, width: w, height: h });
                return Ok(Some(DisplayUpdate::Bitmap(new_bitmap)));
            }
            Some(p) if p.width != w || p.height != h => {
                debug!("Resolution change detected: {}×{} → {}×{} (full refresh)", p.width, p.height, w, h);
                self.prev = Some(PrevFrame { data: new_data, stride, width: w, height: h });
                return Ok(Some(DisplayUpdate::Bitmap(new_bitmap)));
            }
            Some(p) => p,
        };

        // Compute dirty rectangles
        let diffs = find_different_rects_sub::<4>(
            &prev.data, prev.stride, prev.width, prev.height,
            &new_data,  stride,      w,          h,
            0, 0,
        );

        self.prev = Some(PrevFrame { data: new_data, stride, width: w, height: h });

        if diffs.is_empty() {
            return Ok(None); // No changes — skip this frame entirely
        }

        debug!("{} dirty rect(s) this frame", diffs.len());

        // Fast path: large change → send full frame
        let total_dirty: u64 = diffs
            .iter()
            .map(|r| r.width as u64 * r.height as u64)
            .sum();
        if total_dirty > (w as u64 * h as u64) / 2 {
            return Ok(Some(DisplayUpdate::Bitmap(new_bitmap)));
        }

        // Carve out sub-region BitmapUpdates and queue them
        let mut enqueued = 0usize;
        for rect in &diffs {
            let Some(rw) = NonZeroU16::new(rect.width  as u16) else { continue };
            let Some(rh) = NonZeroU16::new(rect.height as u16) else { continue };
            if let Some(sub) = new_bitmap.sub(rect.x as u16, rect.y as u16, rw, rh) {
                self.pending.push_back(sub);
                enqueued += 1;
            }
        }

        if enqueued == 0 {
            return Ok(None);
        }

        // The first sub-region is returned from the pending queue on the
        // next iteration of the loop (step 2).
        Ok(None)
    }

    /// Handle a YCbCr 4:2:0 (NV12) frame from ScreenCaptureKit.
    /// Encode with VideoToolbox and return as Avc420Update.
    fn handle_ycbcr_frame(&mut self, pixel_buf: screencapturekit::cv::CVPixelBuffer) -> Result<Option<DisplayUpdate>> {
        let Some(encoder) = self.h264_encoder.as_mut() else {
            debug!("H264 encoder not available, skipping YCbCr frame");
            return Ok(None);
        };

        // Lock the pixel buffer before encoding. VideoToolbox reads the buffer
        // asynchronously — without the lock, SCKit may overwrite it mid-encode.
        let guard = pixel_buf
            .lock(CVPixelBufferLockFlags::READ_ONLY)
            .map_err(|code| anyhow!("CVPixelBuffer lock failed: {code}"))?;

        let result = encoder.encode(pixel_buf.as_ptr());

        // Unlock immediately after VTCompressionSessionEncodeFrame returns.
        // VideoToolbox retains its own reference to the pixel buffer.
        drop(guard);

        match result {
            Ok(Some(H264Frame { data, is_keyframe, width, height, .. })) => {
                Ok(Some(DisplayUpdate::Avc420(Avc420Update {
                    data,
                    width,
                    height,
                    is_keyframe,
                })))
            }
            Ok(None) => {
                debug!("H264 encoding produced no output, frame skipped");
                Ok(None)
            }
            Err(e) => {
                warn!("H264 encoding error: {e}, frame skipped");
                Ok(None)
            }
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

async fn create_stream(width: u16, height: u16, mode: CaptureMode, target_fps: u32) -> Result<AsyncSCStream> {
    let content = AsyncSCShareableContent::get().await.map_err(|e| {
        anyhow!(
            "ScreenCaptureKit unavailable: {e}. \
             Grant Screen Recording in System Settings → Privacy & Security."
        )
    })?;

    let display = content
        .displays()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("No displays found"))?;

    let filter = SCContentFilter::create()
        .with_display(&display)
        .with_excluding_windows(&[])
        .build();

    let sck_format = match mode {
        CaptureMode::Bgra  => SckPixelFormat::BGRA,
        CaptureMode::H264  => SckPixelFormat::YCbCr_420v,
    };

    let config = SCStreamConfiguration::new()
        .with_width(width as u32)
        .with_height(height as u32)
        .with_pixel_format(sck_format)
        .with_shows_cursor(true)
        .with_fps(target_fps);

    // Use a small buffer (2 frames) to minimize latency:
    // - Frame 0 is the frame we're currently processing
    // - Frame 1 is the most recent frame waiting to be picked up
    // Older frames are dropped, which is correct for real-time remoting.
    // This replaces the previous queue_depth of 8 which could add up to
    // ~267 ms of lag at 30 fps.
    let buffer_capacity = 2;

    let stream = AsyncSCStream::new(&filter, &config, buffer_capacity, SCStreamOutputType::Screen);
    stream
        .start_capture()
        .map_err(|e| anyhow!("Failed to start capture: {e}"))?;

    debug!(
        "SCKit stream started at {}×{} (format={:?}, fps={}, buffer={})",
        width, height, sck_format, target_fps, buffer_capacity
    );
    Ok(stream)
}

fn make_bitmap(data: Bytes, w: usize, h: usize, stride: usize) -> Result<BitmapUpdate> {
    Ok(BitmapUpdate {
        x: 0,
        y: 0,
        width:  NonZeroU16::new(w as u16).ok_or_else(|| anyhow!("zero width"))?,
        height: NonZeroU16::new(h as u16).ok_or_else(|| anyhow!("zero height"))?,
        format: PixelFormat::ARgb32,
        data,
        stride: NonZeroUsize::new(stride).ok_or_else(|| anyhow!("zero stride"))?,
    })
}

async fn primary_display_size() -> Result<DesktopSize> {
    let content = AsyncSCShareableContent::get()
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let display = content
        .displays()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no display"))?;
    Ok(DesktopSize {
        width:  (display.width()  as u16).max(1),
        height: (display.height() as u16).max(1),
    })
}