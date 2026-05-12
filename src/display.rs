use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_graphics::diff::find_different_rects_sub;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat, RdpServerDisplay,
    RdpServerDisplayUpdates,
};
use screencapturekit::async_api::{AsyncSCShareableContent, AsyncSCStream};
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::{
    PixelFormat as SckPixelFormat, SCContentFilter, SCStreamConfiguration, SCStreamOutputType,
};
use std::collections::VecDeque;
use std::num::{NonZeroU16, NonZeroUsize};
use tokio::sync::watch;
use tracing::{debug, warn};

// ─── Public display handler ──────────────────────────────────────────────────

pub struct MacDisplay {
    /// Last size requested by the client via DisplayControl. None = use native.
    current_size: Option<DesktopSize>,
    resize_tx: watch::Sender<Option<DesktopSize>>,
    resize_rx: watch::Receiver<Option<DesktopSize>>,
}

impl MacDisplay {
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(None);
        Self { current_size: None, resize_tx: tx, resize_rx: rx }
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
        debug!("Starting display stream at {}×{}", size.width, size.height);
        // Mark the current resize value as seen before cloning. The stream is
        // already starting at `size` (which reflects any pending resize via
        // current_size), so the new MacDisplayUpdates must not re-fire a Resize
        // for the same request.
        let _ = self.resize_rx.borrow_and_update();
        let updates =
            MacDisplayUpdates::start(size.width, size.height, self.resize_rx.clone()).await?;
        Ok(Box::new(updates))
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        if let Some(monitor) = layout.monitors().first() {
            let (w, h) = monitor.dimensions();
            let size = DesktopSize {
                width:  w.min(u16::MAX as u32) as u16,
                height: h.min(u16::MAX as u32) as u16,
            };
            debug!(
                "request_layout: client wants {}×{}, current_size={:?}",
                size.width, size.height, self.current_size
            );
            // Ignore if we're already at this size; avoids a spurious
            // Deactivation-Reactivation loop when the client re-sends its
            // current layout after each reactivation.
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
    prev:         Option<PrevFrame>,
    pending:      VecDeque<BitmapUpdate>,
    resize_rx:    watch::Receiver<Option<DesktopSize>>,
}

impl MacDisplayUpdates {
    async fn start(
        width: u16,
        height: u16,
        resize_rx: watch::Receiver<Option<DesktopSize>>,
    ) -> Result<Self> {
        let stream = create_stream(width, height).await?;
        Ok(Self {
            stream,
            stream_size: DesktopSize { width, height },
            prev: None,
            pending: VecDeque::new(),
            resize_rx,
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
            // ── 1. Drain buffered sub-region updates ──────────────────────────
            if let Some(update) = self.pending.pop_front() {
                return Ok(Some(DisplayUpdate::Bitmap(update)));
            }

            // ── 2. Handle pending resize ──────────────────────────────────────
            // Only signal a resize if the new size actually differs from the
            // size this stream was created with.  The client re-sends its
            // current layout after every reactivation, so without this check
            // we'd loop forever (resize → reactivate → resize → …).
            if self.resize_rx.has_changed().unwrap_or(false) {
                let new_size = self.resize_rx.borrow_and_update().clone();
                if let Some(size) = new_size {
                    if size != self.stream_size {
                        debug!("Signalling resize to {}×{}", size.width, size.height);
                        return Ok(Some(DisplayUpdate::Resize(size)));
                    }
                }
            }

            // ── 3. Wait for the next SCKit frame ──────────────────────────────
            let Some(sample) = self.stream.next().await else {
                return Ok(None);
            };

            let Some(pixel_buf) = sample.image_buffer() else {
                continue;
            };

            // ── 4. Lock pixel buffer and copy raw BGRA bytes ─────────────────
            let guard = pixel_buf
                .lock(CVPixelBufferLockFlags::READ_ONLY)
                .map_err(|code| anyhow!("CVPixelBuffer lock failed: {code}"))?;

            let w      = guard.width();
            let h      = guard.height();
            let stride = guard.bytes_per_row();
            let new_data = Bytes::copy_from_slice(guard.as_slice());
            drop(guard);

            let new_bitmap = make_bitmap(new_data.clone(), w, h, stride)?;

            // ── 5. First frame or resolution change → full refresh ────────────
            let prev = match &self.prev {
                None => {
                    self.prev = Some(PrevFrame { data: new_data, stride, width: w, height: h });
                    return Ok(Some(DisplayUpdate::Bitmap(new_bitmap)));
                }
                Some(p) if p.width != w || p.height != h => {
                    self.prev = Some(PrevFrame { data: new_data, stride, width: w, height: h });
                    return Ok(Some(DisplayUpdate::Bitmap(new_bitmap)));
                }
                Some(p) => p,
            };

            // ── 6. Compute dirty rectangles ───────────────────────────────────
            let diffs = find_different_rects_sub::<4>(
                &prev.data, prev.stride, prev.width, prev.height,
                &new_data,  stride,      w,          h,
                0, 0,
            );

            self.prev = Some(PrevFrame { data: new_data, stride, width: w, height: h });

            if diffs.is_empty() {
                continue;
            }

            debug!("{} dirty rect(s) this frame", diffs.len());

            // ── 7. Carve out sub-region BitmapUpdates and queue them ──────────
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
                continue;
            }
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

async fn create_stream(width: u16, height: u16) -> Result<AsyncSCStream> {
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

    let config = SCStreamConfiguration::new()
        .with_width(width as u32)
        .with_height(height as u32)
        .with_pixel_format(SckPixelFormat::BGRA)
        .with_shows_cursor(true);

    let stream = AsyncSCStream::new(&filter, &config, 4, SCStreamOutputType::Screen);
    stream
        .start_capture()
        .map_err(|e| anyhow!("Failed to start capture: {e}"))?;

    debug!("SCKit stream started at {}×{}", width, height);
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
