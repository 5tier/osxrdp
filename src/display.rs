use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;
use ironrdp_server::{BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat, RdpServerDisplay, RdpServerDisplayUpdates};
use screencapturekit::async_api::{AsyncSCShareableContent, AsyncSCStream};
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::{PixelFormat as SckPixelFormat, SCContentFilter, SCStreamConfiguration, SCStreamOutputType};
use std::num::{NonZeroU16, NonZeroUsize};
use tracing::{debug, warn};

/// The desktop display sent to RDP clients.
///
/// On `updates()` the actual macOS display dimensions are queried via
/// ScreenCaptureKit. Falls back to 1920×1080 if the query fails (e.g. when
/// Screen Recording permission has not been granted yet).
pub struct MacDisplay;

impl MacDisplay {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl RdpServerDisplay for MacDisplay {
    async fn size(&mut self) -> DesktopSize {
        match primary_display_size().await {
            Ok(s) => s,
            Err(e) => {
                warn!("Could not query display size ({e:#}); defaulting to 1920×1080. \
                       Grant Screen Recording permission in System Settings → Privacy & Security.");
                DesktopSize { width: 1920, height: 1080 }
            }
        }
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        let size = self.size().await;
        let updates = MacDisplayUpdates::start(size.width, size.height).await?;
        Ok(Box::new(updates))
    }
}

/// Streams captured frames from the primary display using ScreenCaptureKit.
pub struct MacDisplayUpdates {
    stream: AsyncSCStream,
}

impl MacDisplayUpdates {
    async fn start(width: u16, height: u16) -> Result<Self> {
        let content = AsyncSCShareableContent::get().await
            .map_err(|e| anyhow!("ScreenCaptureKit unavailable: {e}. \
                Grant Screen Recording permission in System Settings → Privacy & Security."))?;

        let display = content.displays().into_iter().next()
            .ok_or_else(|| anyhow!("No displays found"))?;

        let actual_w = display.width().max(1) as u16;
        let actual_h = display.height().max(1) as u16;

        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();

        let config = SCStreamConfiguration::new()
            .with_width(actual_w as u32)
            .with_height(actual_h as u32)
            .with_pixel_format(SckPixelFormat::BGRA)
            .with_shows_cursor(true);

        // Buffer up to 4 frames; older frames are dropped if the client is slow.
        let stream = AsyncSCStream::new(&filter, &config, 4, SCStreamOutputType::Screen);
        stream.start_capture()
            .map_err(|e| anyhow!("Failed to start capture: {e}"))?;

        debug!("SCKit stream started at {}×{}", actual_w, actual_h);
        let _ = (width, height); // currently unused; reserved for downscaling
        Ok(Self { stream })
    }
}

impl Drop for MacDisplayUpdates {
    fn drop(&mut self) {
        if let Err(e) = self.stream.stop_capture() {
            debug!("stop_capture error: {e}");
        }
    }
}

#[async_trait]
impl RdpServerDisplayUpdates for MacDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        // next() suspends until a frame is available; returns None on stream close.
        let Some(sample) = self.stream.next().await else {
            return Ok(None);
        };

        let Some(pixel_buf) = sample.image_buffer() else {
            return Ok(None); // idle / dropped frame — skip
        };

        let guard = pixel_buf
            .lock(CVPixelBufferLockFlags::READ_ONLY)
            .map_err(|code| anyhow!("CVPixelBuffer lock failed: {code}"))?;

        let w = guard.width();
        let h = guard.height();
        let bytes_per_row = guard.bytes_per_row();

        // Screencapturekit delivers kCVPixelFormatType_32BGRA (bytes: B G R A).
        // ironrdp's ARgb32 encodes the same layout when interpreted as a
        // little-endian 32-bit integer (B=byte0 → LSB, A=byte3 → MSB).
        let data = Bytes::copy_from_slice(guard.as_slice());

        let bitmap = BitmapUpdate {
            x: 0,
            y: 0,
            width:  NonZeroU16::new(w as u16).ok_or_else(|| anyhow!("zero width"))?,
            height: NonZeroU16::new(h as u16).ok_or_else(|| anyhow!("zero height"))?,
            format: PixelFormat::ARgb32,
            data,
            stride: NonZeroUsize::new(bytes_per_row).ok_or_else(|| anyhow!("zero stride"))?,
        };

        Ok(Some(DisplayUpdate::Bitmap(bitmap)))
    }
}

/// Query the primary display size from ScreenCaptureKit synchronously.
async fn primary_display_size() -> Result<DesktopSize> {
    let content = AsyncSCShareableContent::get().await
        .map_err(|e| anyhow!("{e}"))?;
    let display = content.displays().into_iter().next()
        .ok_or_else(|| anyhow!("no display"))?;
    Ok(DesktopSize {
        width:  (display.width()  as u16).max(1),
        height: (display.height() as u16).max(1),
    })
}
