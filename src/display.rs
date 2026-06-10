use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_server::{
    Avc420Update, DesktopSize, DisplayUpdate, RdpServerDisplay, RdpServerDisplayUpdates,
};
use screencapturekit::async_api::{AsyncSCShareableContent, AsyncSCStream};
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::{
    PixelFormat as SckPixelFormat, SCContentFilter, SCStreamConfiguration, SCStreamOutputType,
};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time;
// MissedTickBehavior is used in start() to configure the fps_interval
#[allow(unused_imports)]
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use crate::frame_pipeline::BgraFramePipeline;

use crate::cg_capture;
use crate::display_mode::DisplayModeSwitcher;
use crate::h264::{H264Frame, VtH264Encoder};

// ─── Aspect ratio mode ─────────────────────────────────────────────────────

/// Controls how the desktop aspect ratio is adjusted relative to the client.
///
/// - `Fit`: Letterbox the native desktop to fit inside the client's window.
///   The full desktop is visible with black bars filling the remaining space.
///   No content is cropped or discarded.
/// - `Native`: Preserve the server's native aspect ratio, allowing
///   the client to handle letterboxing when the ratios differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AspectMode {
    Fit,
    Native,
}

impl Default for AspectMode {
    fn default() -> Self {
        Self::Fit
    }
}

/// Parse aspect mode from environment variable.
/// `OSXRDP_ASPECT=fit` (default) or `OSXRDP_ASPECT=native`.
pub fn aspect_mode_from_env() -> AspectMode {
    match std::env::var("OSXRDP_ASPECT")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "native" => {
            tracing::info!("Aspect mode: native (preserve server aspect ratio)");
            AspectMode::Native
        }
        _ => {
            tracing::info!("Aspect mode: fit (letterbox — full desktop visible, black bars where ratios differ)");
            AspectMode::Fit
        }
    }
}

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
    /// Size the RDP client wants / the desktop size we report to the client.
    /// Set by `request_layout`, reported via `size()`. None = use native.
    current_size: Option<DesktopSize>,
    /// Native physical display size, cached from the first successful query.
    native_size: Option<DesktopSize>,
    resize_tx: watch::Sender<Option<DesktopSize>>,
    resize_rx: watch::Receiver<Option<DesktopSize>>,
    /// Capture mode: BGRA fallback or H.264 via VideoToolbox.
    mode: CaptureMode,
    /// Target FPS (default: 30).
    target_fps: u32,
    /// How to handle aspect ratio mismatch between native display and client.
    aspect_mode: AspectMode,
    /// Switches the physical display to a resolution matching the client.
    /// Lives here for the whole server lifetime; restored to the original
    /// mode in client_disconnected() when a connection ends.
    mode_switcher: Option<DisplayModeSwitcher>,
    /// When a mode switch was performed, we must wait for macOS to propagate
    /// the change before creating an SCKit stream. This field stores the
    /// switched resolution, and `updates()` sleeps before creating the stream.
    mode_switch_pending: Option<DesktopSize>,
    /// True after any display mode switch. CGDisplay must be used for ALL
    /// subsequent streams because SCKit crashes after a mode switch (macOS
    /// bug). This persists across deactivation-reactivation cycles.
    cg_display_forced: bool,
    /// Shared flag to force H.264 keyframe. Server.rs sets this when the
    /// GfxServer needs a keyframe; the encoder polls it before encode().
    force_keyframe: Arc<AtomicBool>,
    /// Shared flag: true when the client supports AVC420.
    /// The GfxServer sets this to `false` during capability negotiation
    /// when the client advertises AVC_DISABLED. The display module checks
    /// it each frame and falls back to BGRA bitmap updates if false.
    avc420_enabled: Arc<AtomicBool>,
}

impl MacDisplay {
    #[allow(dead_code)]
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(None);
        Self {
            current_size: None,
            native_size: None,
            resize_tx: tx,
            resize_rx: rx,
            mode: CaptureMode::default(),
            target_fps: TARGET_FPS,
            aspect_mode: aspect_mode_from_env(),
            mode_switcher: None,
            mode_switch_pending: None,
            cg_display_forced: false,
            force_keyframe: Arc::new(AtomicBool::new(false)),
            avc420_enabled: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Create with a specific capture mode.
    pub fn with_mode(mode: CaptureMode) -> Self {
        let (tx, rx) = watch::channel(None);
        Self {
            current_size: None,
            native_size: None,
            resize_tx: tx,
            resize_rx: rx,
            mode,
            target_fps: TARGET_FPS,
            aspect_mode: aspect_mode_from_env(),
            mode_switcher: None,
            mode_switch_pending: None,
            cg_display_forced: false,
            force_keyframe: Arc::new(AtomicBool::new(false)),
            avc420_enabled: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Ensure `native_size` is populated.
    ///
    /// Uses CGDisplay (not SCKit) to query the display size. SCKit must
    /// NOT be queried before the display mode is finalized — SCKit
    /// caches display info and crashes if the mode changes later.
    fn ensure_native_size(&mut self) {
        if self.native_size.is_none() {
            let native = cg_display_size();
            debug!(
                "Cached native display size: {}×{} (aspect ratio: {:.3})",
                native.width,
                native.height,
                native.width as f64 / native.height as f64
            );
            self.native_size = Some(native);
        }
    }

    /// Get a reference to the shared force_keyframe flag.
    /// The server sets this when the GfxServer needs a keyframe;
    /// the encoder checks it before each encode call.
    pub fn force_keyframe_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.force_keyframe)
    }

    /// Set the shared AVC420-enabled flag.
    ///
    /// The GfxServer holds a clone of the same `Arc`. During GFX
    /// capability negotiation, if the client advertises `AVC_DISABLED`,
    /// the flag is set to `false`. The display module checks it each
    /// frame and falls back to BGRA bitmap updates when false.
    pub fn set_avc420_flag(&mut self, flag: Arc<AtomicBool>) {
        self.avc420_enabled = flag;
    }
}

#[async_trait]
impl RdpServerDisplay for MacDisplay {
    async fn size(&mut self) -> DesktopSize {
        // Return the client-requested / effective size if available.
        // This is the desktop size reported to the RDP client.
        if let Some(size) = self.current_size {
            return size;
        }
        self.ensure_native_size();
        self.native_size.unwrap()
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        // NOTE: the mode switcher stays in MacDisplay. The update stream is
        // recreated on every deactivation-reactivation (e.g. resize), so tying
        // the display-mode restore to the stream's Drop would undo a mode
        // switch one second after it was made. Restore happens in
        // client_disconnected() instead, when the connection truly ends.

        // Wait briefly for DisplayControl layout from the client.
        let mut polled: u32 = 0;
        loop {
            if self.current_size.is_some() || polled >= 50 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            polled += 1;
        }

        // If a display mode switch was performed by request_layout(), we must
        // use CGDisplay capture instead of SCKit. SCKit crashes (SIGSEGV)
        // after any display mode change — even from a separate process.
        // cg_display_forced persists across deactivation-reactivation cycles
        // because SCKit remains unusable after the first mode switch.
        let use_cgdisplay = self.mode_switch_pending.is_some() || self.cg_display_forced;

        if let Some(pending) = self.mode_switch_pending.take() {
            info!(
                "Mode switch to {}×{} — using CGDisplay capture \
                 (SCKit crashes after mode change)",
                pending.width, pending.height,
            );
            // Brief delay for macOS to stabilize the display change.
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        self.ensure_native_size();
        let native_size = self.native_size.unwrap();

        // The target size is what the RDP client sees — the desktop dimensions.
        // In Fit mode: the client's requested dimensions (no aspect-ratio adjustment).
        // In Native mode: adjusted to match the server's native aspect ratio.
        let target_size = self.current_size.unwrap_or(native_size);

        // Stream at native resolution. SCKit delivers the best quality at
        // native size, and we crop+scale to target in the frame handler.
        // If aspects match, crop+scale is a no-op.
        let stream_size = native_size;

        debug!(
            "Starting display stream: native {}×{}, target {}×{}, mode={:?}, aspect={:?}",
            stream_size.width, stream_size.height,
            target_size.width, target_size.height,
            self.mode, self.aspect_mode,
        );

        // Clone the receiver WITHOUT calling borrow_and_update() first,
        // so that the pending resize notification (from request_layout's
        // resize_tx.send) is inherited by the clone.
        // next_update() will then see has_changed() = true and emit
        // DisplayUpdate::Resize to inform the client of the desktop size.
        let updates = MacDisplayUpdates::start(
            stream_size.width,
            stream_size.height,
            target_size,
            self.mode,
            self.aspect_mode,
            self.target_fps,
            self.resize_rx.clone(),
            use_cgdisplay,
            Arc::clone(&self.force_keyframe),
            Arc::clone(&self.avc420_enabled),
        )
        .await?;
        // After cloning, update the original receiver's version so that
        // subsequent reactivation connections don't inherit the stale
        // version and trigger a spurious resize loop.
        let _ = self.resize_rx.borrow_and_update();
        Ok(Box::new(updates))
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        if let Some(monitor) = layout.monitors().first() {
            let (client_w, client_h) = monitor.dimensions();

            let client_size = DesktopSize {
                width: (client_w as u16).max(1).min(u16::MAX),
                height: (client_h as u16).max(1).min(u16::MAX),
            };

            let native = self.native_size.unwrap_or_else(|| cg_display_size());

            // ── In Fit mode: try switching the physical display ──────────────
            if self.aspect_mode == AspectMode::Fit {
                let target_w = client_size.width as u32;
                let target_h = client_size.height as u32;

                // Lazily create the mode switcher on first call
                if self.mode_switcher.is_none() {
                    self.mode_switcher = Some(DisplayModeSwitcher::new());
                }

                if let Some(ref mut switcher) = self.mode_switcher {
                    match switcher.switch_to_best_mode(target_w, target_h) {
                        Some((sw_w, sw_h)) => {
                            let switched_size = DesktopSize {
                                width: sw_w as u16,
                                height: sw_h as u16,
                            };
                            if self.current_size == Some(switched_size) {
                                debug!("already at {}×{}, no change", sw_w, sw_h);
                                return;
                            }
                            info!(
                                "Display switch to {}×{} (client wants {}×{}). \
                                 Using CGDisplay capture to avoid SCKit crash.",
                                sw_w, sw_h, client_w, client_h,
                            );
                            // Do NOT call SCKit or create streams yet — macOS needs
                            // time to propagate the display change. Set a flag so
                            // that `updates()` waits before creating the SCKit stream.
                            self.native_size = None; // will re-query via CGDisplay
                            self.current_size = Some(switched_size);
                            self.mode_switch_pending = Some(switched_size);
                            self.cg_display_forced = true; // SCKit unusable after mode switch
                            let _ = self.resize_tx.send(Some(switched_size));
                            return;
                        }
                        None => {
                            debug!(
                                "No matching display mode for {}×{}, using letterbox",
                                target_w, target_h,
                            );
                        }
                    }
                }
            }

            // ── Compute effective size (letterbox or native ratio) ───────────
            let effective_size = match self.aspect_mode {
                AspectMode::Fit => {
                    // Client dimensions become the desktop size.
                    // If a matching display mode exists, the physical display
                    // switches to match (handled above). Otherwise letterbox.
                    debug!(
                        "request_layout: client {}×{}, native {}×{}, Fit mode → {}×{}",
                        client_w, client_h,
                        native.width, native.height,
                        client_size.width, client_size.height,
                    );
                    client_size
                }
                AspectMode::Native => {
                    // Preserve native aspect ratio, fitting within the
                    // client's window. Black bars appear if ratios differ.
                    let native_ratio = native.width as f64 / native.height as f64;
                    let (adj_w, adj_h) = if (client_w as f64 / client_h as f64) > native_ratio {
                        (client_h as f64 * native_ratio, client_h as f64)
                    } else {
                        (client_w as f64, client_w as f64 / native_ratio)
                    };
                    let adj_size = DesktopSize {
                        width: (adj_w.round() as u16).max(1).min(u16::MAX),
                        height: (adj_h.round() as u16).max(1).min(u16::MAX),
                    };
                    debug!(
                        "request_layout: client {}×{}, native {}×{}, Native mode → {}×{}",
                        client_w, client_h,
                        native.width, native.height,
                        adj_size.width, adj_size.height,
                    );
                    adj_size
                }
            };

            if self.current_size == Some(effective_size) {
                debug!(
                    "request_layout: already at {}×{}, no change",
                    effective_size.width, effective_size.height
                );
                return;
            }
            self.current_size = Some(effective_size);
            let _ = self.resize_tx.send(Some(effective_size));
        }
    }

    fn client_disconnected(&mut self) {
        if let Some(ref mut switcher) = self.mode_switcher {
            info!("Client disconnected, restoring original display mode");
            if let Err(e) = switcher.restore() {
                warn!("Failed to restore display mode on disconnect: {e}");
            }
        }
        // Forget per-connection state so the next client negotiates from
        // scratch. The desktop is back at native resolution; a stale
        // current_size would make the server skip request_layout when the
        // next client requests the same dimensions, and a stale native_size
        // would reflect the previously switched mode.
        self.current_size = None;
        self.native_size = None;
        self.mode_switch_pending = None;
    }
}

// ─── Capture source ─────────────────────────────────────────────────────────

/// Which capture backend to use for screen capture.
///
/// After a display mode change, SCKit crashes (macOS bug). Use CGDisplay
/// capture instead, which is immune to mode changes.
enum CaptureSource {
    /// ScreenCaptureKit — the preferred backend when no mode switch has occurred.
    Sckit(AsyncSCStream),
    /// CoreGraphics CGDisplayCreateImage — used after a mode switch to avoid
    /// the SCKit crash. Polls the display at the target FPS.
    CGDisplay,
}

// ─── Display update stream ───────────────────────────────────────────────────

pub struct MacDisplayUpdates {
    /// The capture backend (SCKit or CGDisplay).
    capture: CaptureSource,
    /// The size of the capture stream (typically the display resolution).
    stream_size: DesktopSize,
    /// The desktop size reported to the RDP client.
    /// In Fit mode: the client's requested dimensions.
    /// In Native mode: the aspect-ratio-adjusted dimensions.
    target_size: DesktopSize,
    mode: CaptureMode,
    bgra_pipeline: BgraFramePipeline,
    resize_rx: watch::Receiver<Option<DesktopSize>>,
    /// VideoToolbox H.264 encoder (only for H264 mode).
    h264_encoder: Option<VtH264Encoder>,
    /// Rate-limiting interval — ensures we process at most `target_fps` frames/sec.
    fps_interval: time::Interval,
    /// SCKit sometimes delivers samples with null image buffers even though
    /// the stream is running. Track consecutive failures to detect this.
    missed_frames: u32,
    target_fps: u32,
    /// Shared flag to signal the H.264 encoder that a keyframe is needed.
    /// Set by server.rs when GfxServer::needs_keyframe() is true,
    /// checked by the encoder before each encode call.
    force_keyframe: Arc<AtomicBool>,
    /// Shared flag: true when the client supports AVC420.
    /// Set to false by the GfxServer during capability negotiation when
    /// the client advertises AVC_DISABLED. Checked each frame; when false,
    /// the display falls back to BGRA bitmap updates.
    avc420_enabled: Arc<AtomicBool>,
}

impl MacDisplayUpdates {
    async fn start(
        width: u16,
        height: u16,
        target_size: DesktopSize,
        mode: CaptureMode,
        aspect_mode: AspectMode,
        target_fps: u32,
        resize_rx: watch::Receiver<Option<DesktopSize>>,
        use_cgdisplay: bool,
        force_keyframe: Arc<AtomicBool>,
        avc420_enabled: Arc<AtomicBool>,
    ) -> Result<Self> {
        let capture = if use_cgdisplay {
            info!(
                "Using CGDisplay capture (mode switch occurred, avoiding SCKit crash)");
            CaptureSource::CGDisplay
        } else {
            let stream = create_stream(width, height, mode, target_fps).await?;
            CaptureSource::Sckit(stream)
        };
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
            capture,
            stream_size: DesktopSize { width, height },
            target_size,
            mode,
            bgra_pipeline: BgraFramePipeline::new(target_size, aspect_mode),
            resize_rx,
            h264_encoder,
            fps_interval,
            missed_frames: 0,
            target_fps,
            force_keyframe,
            avc420_enabled,
        })
    }
}

impl Drop for MacDisplayUpdates {
    fn drop(&mut self) {
        // Do NOT restore the display mode here: this Drop also fires on
        // deactivation-reactivation, which recreates the stream mid-connection.
        // Restore is handled by MacDisplay::client_disconnected().
        if let CaptureSource::Sckit(stream) = &self.capture {
            if let Err(e) = stream.stop_capture() {
                debug!("stop_capture: {e}");
            }
        }
    }
}

#[async_trait]
impl RdpServerDisplayUpdates for MacDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        loop {
            // ── 1. Handle pending resize FIRST (before waiting for frames) ────
            // In BGRA mode, emit a Resize event so the server can resize
            // the desktop via deactivation-reactivation.
            // In H264 mode, skip the Resize event — the GfxServer surface is
            // updated separately via the GFX pipeline, and deactivation-
            // reactivation would break the DVC channels (including GFX).
            if self.resize_rx.has_changed().unwrap_or(false) {
                let new_size = self.resize_rx.borrow_and_update().clone();
                if let Some(size) = new_size {
                    debug!(
                        "Client resize notification: {}×{} (was {}×{})",
                        size.width, size.height,
                        self.target_size.width, self.target_size.height,
                    );
                    self.target_size = size;
                    self.bgra_pipeline.set_target(size);
                    if self.mode == CaptureMode::Bgra || !self.avc420_enabled.load(Ordering::Relaxed) {
                        return Ok(Some(DisplayUpdate::Resize(size)));
                    }
                    // In H264 mode, the GfxServer is resized in place via the
                    // GFX pipeline. No deactivation-reactivation is needed.
                    debug!("Resize in H264 mode: skipping deactivation-reactivation");
                }
            }

            // ── 2. Drain buffered sub-region updates (BGRA mode) ──────────────
            if let Some(update) = self.bgra_pipeline.pop_update() {
                return Ok(Some(update));
            }

            // ── 3. Rate-limit: wait until the next frame slot ─────────────────
            self.fps_interval.tick().await;

            // ── 4. Capture the next frame ──────────────────────────────────────
            match &mut self.capture {
                CaptureSource::Sckit(stream) => {
                    let Some(sample) = stream.next().await else {
                        debug!("Stream ended (no more samples)");
                        return Ok(None);
                    };

                    let Some(pixel_buf) = sample.image_buffer() else {
                        self.missed_frames += 1;
                        if self.missed_frames == 1 {
                            debug!("SCKit null buffer (frame {})", self.missed_frames);
                        }
                        if self.missed_frames >= 30 {
                            warn!(
                                "SCKit stream stalled ({} missed frames), recreating",
                                self.missed_frames
                            );
                            if let Err(e) = self
                                .recreate_stream(self.stream_size.width, self.stream_size.height)
                                .await
                            {
                                warn!("Failed to recreate stalled stream: {e}");
                            } else {
                                self.missed_frames = 0;
                            }
                        }
                        continue;
                    };
                    self.missed_frames = 0;

                    // Check if the SCKit frame size changed (e.g. display hotplug).
                    let frame_w = pixel_buf.width() as u16;
                    let frame_h = pixel_buf.height() as u16;
                    if frame_w != self.stream_size.width || frame_h != self.stream_size.height {
                        debug!(
                            "Frame size {}×{} differs from stream {}×{} — updating",
                            frame_w, frame_h,
                            self.stream_size.width, self.stream_size.height,
                        );
                        if let Err(e) = self.recreate_stream(frame_w, frame_h).await {
                            warn!("Failed to recreate stream: {e}");
                            continue;
                        }
                    }

                    match self.mode {
                        CaptureMode::Bgra => {
                            let guard = pixel_buf
                                .lock(CVPixelBufferLockFlags::READ_ONLY)
                                .map_err(|code| anyhow!("CVPixelBuffer lock failed: {code}"))?;
                            let (src_w, src_h, src_stride) =
                                (guard.width(), guard.height(), guard.bytes_per_row());
                            self.bgra_pipeline.push_frame(guard.as_slice(), src_w, src_h, src_stride);
                            drop(guard);
                            if let Some(update) = self.bgra_pipeline.pop_update() {
                                return Ok(Some(update));
                            }
                            continue;
                        }
                        CaptureMode::H264 => {
                            // Fall back to BGRA if the client doesn't support AVC420.
                            if !self.avc420_enabled.load(Ordering::Relaxed) {
                                let guard = pixel_buf
                                    .lock(CVPixelBufferLockFlags::READ_ONLY)
                                    .map_err(|code| anyhow!("CVPixelBuffer lock failed: {code}"))?;
                                let (src_w, src_h, src_stride) =
                                    (guard.width(), guard.height(), guard.bytes_per_row());
                                self.bgra_pipeline.push_frame(guard.as_slice(), src_w, src_h, src_stride);
                                drop(guard);
                                if let Some(update) = self.bgra_pipeline.pop_update() {
                                    return Ok(Some(update));
                                }
                                continue;
                            }
                            if let Some(update) = self.handle_ycbcr_frame(pixel_buf)? {
                                return Ok(Some(update));
                            }
                            continue;
                        }
                    }
                }
                CaptureSource::CGDisplay => {
                    match self.mode {
                        CaptureMode::Bgra => {
                            if let Some(frame) = cg_capture::capture_display_bgra() {
                                self.bgra_pipeline.push_frame(
                                    &frame.data, frame.width, frame.height, frame.stride,
                                );
                                if let Some(update) = self.bgra_pipeline.pop_update() {
                                    return Ok(Some(update));
                                }
                            }
                            continue;
                        }
                        CaptureMode::H264 => {
                            // Fall back to BGRA if the client doesn't support AVC420.
                            if !self.avc420_enabled.load(Ordering::Relaxed) {
                                if let Some(frame) = cg_capture::capture_display_bgra() {
                                    self.bgra_pipeline.push_frame(
                                        &frame.data, frame.width, frame.height, frame.stride,
                                    );
                                    if let Some(update) = self.bgra_pipeline.pop_update() {
                                        return Ok(Some(update));
                                    }
                                }
                                continue;
                            }
                            // H264 encoding via CGDisplay capture:
                            // Capture screen into CVPixelBuffer (BGRA),
                            // then encode with VideoToolbox.
                            match cg_capture::capture_display_as_cv_pixel_buffer() {
                                Some(cv_buffer) => {
                                    debug!("H264 CGDisplay: captured CVPixelBuffer {}×{}",
                                           cv_buffer.width(), cv_buffer.height());
                                    if let Some(encoder) = self.h264_encoder.as_mut() {
                                        // Recreate the encoder if forced keyframe is requested.
                                        // VTSessionSetProperty(ForceKeyFrame) is not supported
                                        // (returns -12900), so we recreate the session instead.
                                        // A new session always produces a keyframe first.
                                        if self.force_keyframe.load(Ordering::Relaxed) {
                                            debug!("Recreating H.264 encoder for forced keyframe");
                                            let width = encoder.width();
                                            let height = encoder.height();
                                            match VtH264Encoder::new(width, height) {
                                                Ok(new_encoder) => {
                                                    self.h264_encoder = Some(new_encoder);
                                                }
                                                Err(e) => {
                                                    warn!("Failed to recreate H.264 encoder: {e}, using existing");
                                                }
                                            }
                                            self.force_keyframe.store(false, Ordering::Relaxed);
                                        }
                                        // Re-borrow after possible replacement.
                                        let encoder = self.h264_encoder.as_mut().unwrap();
                                        match encoder.encode(cv_buffer.as_ptr()) {
                                            Ok(Some(H264Frame {
                                                data,
                                                is_keyframe,
                                                width,
                                                height,
                                                ..
                                            })) => {
                                                return Ok(Some(DisplayUpdate::Avc420(Avc420Update {
                                                    data,
                                                    width,
                                                    height,
                                                    is_keyframe,
                                                })));
                                            }
                                            Ok(None) => {
                                                debug!("H264 CGDisplay encode produced no output");
                                                continue;
                                            }
                                            Err(e) => {
                                                warn!("H264 CGDisplay encode error: {e}, falling back to BGRA");
                                                if let Some(frame) = cg_capture::capture_display_bgra() {
                                                    self.bgra_pipeline.push_frame(
                                                        &frame.data, frame.width, frame.height, frame.stride,
                                                    );
                                                    if let Some(update) = self.bgra_pipeline.pop_update() {
                                                        return Ok(Some(update));
                                                    }
                                                }
                                                continue;
                                            }
                                        }
                                    } else {
                                        // No encoder available, fall back to BGRA.
                                        if let Some(frame) = cg_capture::capture_display_bgra() {
                                            self.bgra_pipeline.push_frame(
                                                &frame.data, frame.width, frame.height, frame.stride,
                                            );
                                            if let Some(update) = self.bgra_pipeline.pop_update() {
                                                return Ok(Some(update));
                                            }
                                        }
                                        continue;
                                    }
                                }
                                None => {
                                    // CVPixelBuffer capture failed, fall back to BGRA.
                                    if let Some(frame) = cg_capture::capture_display_bgra() {
                                        self.bgra_pipeline.push_frame(
                                            &frame.data, frame.width, frame.height, frame.stride,
                                        );
                                        if let Some(update) = self.bgra_pipeline.pop_update() {
                                            return Ok(Some(update));
                                        }
                                    }
                                    continue;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

impl MacDisplayUpdates {
    /// Recreate the stream and encoder at a new resolution.
    /// Only meaningful for SCKit; CGDisplay adapts automatically.
    async fn recreate_stream(&mut self, new_width: u16, new_height: u16) -> Result<()> {
        match &mut self.capture {
            CaptureSource::Sckit(stream) => {
                debug!(
                    "Recreating SCKit stream: {}×{} → {}×{}",
                    self.stream_size.width, self.stream_size.height, new_width, new_height
                );
                if let Err(e) = stream.stop_capture() {
                    debug!("stop_capture during recreate: {e}");
                }
                *stream = create_stream(new_width, new_height, self.mode, self.target_fps).await?;
            }
            CaptureSource::CGDisplay => {
                debug!(
                    "CGDisplay capture: stream size {}×{} → {}×{} (no recreate needed)",
                    self.stream_size.width, self.stream_size.height, new_width, new_height
                );
            }
        }
        self.stream_size = DesktopSize {
            width: new_width,
            height: new_height,
        };
        if self.mode == CaptureMode::H264 {
            self.h264_encoder = Some(VtH264Encoder::new(new_width, new_height)?);
        }
        self.bgra_pipeline.reset();
        Ok(())
    }

    /// Handle a YCbCr 4:2:0 (NV12) frame from ScreenCaptureKit.
    /// Encode with VideoToolbox and return as Avc420Update.
    ///
    /// Note: When aspect modes differ, H264 streams the native frame
    /// without crop+scale. For correct aspect ratio in H264 mode,
    /// switch to BGRA mode (OSXRDP_H264=0) or use AspectMode::Native.
    fn handle_ycbcr_frame(
        &mut self,
        pixel_buf: screencapturekit::cv::CVPixelBuffer,
    ) -> Result<Option<DisplayUpdate>> {
        let Some(encoder) = self.h264_encoder.as_mut() else {
            debug!("H264 encoder not available, skipping YCbCr frame");
            return Ok(None);
        };

        // Recreate the encoder if forced keyframe is requested.
        // VTSessionSetProperty(ForceKeyFrame) is not supported
        // (returns -12900), so we recreate the session instead.
        // A new session always produces a keyframe first.
        if self.force_keyframe.load(Ordering::Relaxed) {
            debug!("Recreating H.264 encoder for forced keyframe (SCKit path)");
            let width = encoder.width();
            let height = encoder.height();
            match VtH264Encoder::new(width, height) {
                Ok(new_encoder) => {
                    self.h264_encoder = Some(new_encoder);
                }
                Err(e) => {
                    warn!("Failed to recreate H.264 encoder: {e}, using existing");
                }
            }
            self.force_keyframe.store(false, Ordering::Relaxed);
        }
        // Re-borrow after possible replacement.
        let encoder = self.h264_encoder.as_mut().unwrap();

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
            Ok(Some(H264Frame {
                data,
                is_keyframe,
                width,
                height,
                ..
            })) => Ok(Some(DisplayUpdate::Avc420(Avc420Update {
                data,
                width,
                height,
                is_keyframe,
            }))),
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

// ─── SCKit + size helpers ────────────────────────────────────────────────────

/// Get the native display size using CGDisplay (not SCKit).
///
/// SCKit MUST NOT be called before the display mode is finalized, because
/// SCKit caches display info and permanently crashes if the mode later
/// changes. CGDisplay is safe to call at any time.
fn cg_display_size() -> DesktopSize {
    let display = core_graphics::display::CGDisplay::main();
    let width = display.pixels_wide() as u16;
    let height = display.pixels_high() as u16;
    debug!("cg_display_size: {}×{}", width, height);
    DesktopSize {
        width: width.max(1),
        height: height.max(1),
    }
}

/// Synchronous fallback for native display size using SCKit.
///
/// WARNING: This calls SCKit and should ONLY be used when the display
/// mode is finalized (no pending mode switch). Used in contexts where
/// async is not available and CGDisplay is not suitable.
#[allow(dead_code)]
fn native_display_size_fallback() -> DesktopSize {
    match screencapturekit::shareable_content::SCShareableContent::get() {
        Ok(content) => match content.displays().into_iter().next() {
            Some(d) => {
                let w = (d.width() as u16).max(1);
                let h = (d.height() as u16).max(1);
                debug!("native_display_size_fallback: {}×{}", w, h);
                DesktopSize { width: w, height: h }
            }
            None => {
                warn!(
                    "native_display_size_fallback: no displays found, using 1920×1080"
                );
                DesktopSize {
                    width: 1920,
                    height: 1080,
                }
            }
        },
        Err(e) => {
            warn!(
                "native_display_size_fallback: SCKit error ({e}), using 1920×1080"
            );
            DesktopSize {
                width: 1920,
                height: 1080,
            }
        }
    }
}

async fn create_stream(
    width: u16,
    height: u16,
    mode: CaptureMode,
    target_fps: u32,
) -> Result<AsyncSCStream> {
    // Retry SCKit stream creation up to 3 times with a delay.
    // After a display mode change (even by a child process), SCKit may need
    // time to stabilize. Retrying gives macOS time to update internal state.
    let max_attempts = 3;
    for attempt in 1..=max_attempts {
        match create_stream_inner(width, height, mode, target_fps).await {
            Ok(stream) => return Ok(stream),
            Err(e) if attempt < max_attempts => {
                warn!(
                    "SCKit stream creation attempt {}/{} failed: {e:#}. \
                     Retrying in {}s...",
                    attempt, max_attempts, attempt,
                );
                tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

async fn create_stream_inner(
    width: u16,
    height: u16,
    mode: CaptureMode,
    target_fps: u32,
) -> Result<AsyncSCStream> {
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
        CaptureMode::Bgra => SckPixelFormat::BGRA,
        CaptureMode::H264 => SckPixelFormat::YCbCr_420v,
    };

    let config = SCStreamConfiguration::new()
        .with_width(width as u32)
        .with_height(height as u32)
        .with_pixel_format(sck_format)
        .with_shows_cursor(true)
        .with_fps(target_fps);

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the reconnect half of the "display does not adapt"
    /// bug: current_size persisted across connections, so on reconnect the
    /// server saw client_size == display_size and never called
    /// request_layout — no mode switch was attempted and the client got a
    /// letterboxed native desktop.
    ///
    /// The other half (MacDisplayUpdates::drop restoring the display mode on
    /// every deactivation-reactivation) has no test seam: DisplayModeSwitcher
    /// is concrete and switches the physical display via a child process.
    #[test]
    fn client_disconnected_clears_per_connection_state() {
        let mut display = MacDisplay::new();
        display.current_size = Some(DesktopSize { width: 1920, height: 1080 });
        display.native_size = Some(DesktopSize { width: 2560, height: 1600 });
        display.mode_switch_pending = Some(DesktopSize { width: 1920, height: 1080 });

        display.client_disconnected();

        assert_eq!(display.current_size, None);
        assert_eq!(display.native_size, None);
        assert_eq!(display.mode_switch_pending, None);
    }
}

