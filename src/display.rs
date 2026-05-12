use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;
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
use tracing::{debug, warn};

// ─── Public display handler ──────────────────────────────────────────────────

/// Queries the primary display size and drives the SCKit capture stream.
///
/// Falls back to 1920×1080 when Screen Recording permission is not yet granted.
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
        let updates = MacDisplayUpdates::start(size.width, size.height).await?;
        Ok(Box::new(updates))
    }
}

// ─── Display update stream ───────────────────────────────────────────────────

/// Stores the previous frame so we can diff against it.
struct PrevFrame {
    data:   Bytes,
    stride: usize,
    width:  usize,
    height: usize,
}

/// Pulls frames from ScreenCaptureKit, diffs against the last sent frame, and
/// emits only the changed sub-regions as `BitmapUpdate`s.
pub struct MacDisplayUpdates {
    stream:  AsyncSCStream,
    prev:    Option<PrevFrame>,
    /// Sub-region updates queued from the last diff; drained one per poll.
    pending: VecDeque<BitmapUpdate>,
}

impl MacDisplayUpdates {
    async fn start(width: u16, height: u16) -> Result<Self> {
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

        let stream = AsyncSCStream::new(&filter, &config, 4, SCStreamOutputType::Screen);
        stream
            .start_capture()
            .map_err(|e| anyhow!("Failed to start capture: {e}"))?;

        debug!("SCKit stream started at {}×{}", actual_w, actual_h);
        let _ = (width, height); // reserved for future downscaling

        Ok(Self {
            stream,
            prev:    None,
            pending: VecDeque::new(),
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
            // ── 1. Drain buffered sub-region updates before fetching a new frame ──
            if let Some(update) = self.pending.pop_front() {
                return Ok(Some(DisplayUpdate::Bitmap(update)));
            }

            // ── 2. Wait for the next SCKit frame ──────────────────────────────────
            let Some(sample) = self.stream.next().await else {
                return Ok(None); // stream closed
            };

            let Some(pixel_buf) = sample.image_buffer() else {
                continue; // idle / dropped frame — try again immediately
            };

            // ── 3. Lock the pixel buffer and copy raw BGRA bytes ─────────────────
            let guard = pixel_buf
                .lock(CVPixelBufferLockFlags::READ_ONLY)
                .map_err(|code| anyhow!("CVPixelBuffer lock failed: {code}"))?;

            let w      = guard.width();
            let h      = guard.height();
            let stride = guard.bytes_per_row();
            let new_data = Bytes::copy_from_slice(guard.as_slice());
            drop(guard); // release the lock before any heavy computation

            let new_bitmap = make_bitmap(new_data.clone(), w, h, stride)?;

            // ── 4. First frame or resolution change → full refresh ────────────────
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

            // ── 5. Compute dirty rectangles ───────────────────────────────────────
            //
            // find_different_rects_sub compares `prev` (image1) against `new`
            // (image2 at offset 0,0).  With equal dims and zero offset this is
            // equivalent to a full-frame tile diff.  Returns rects in image2 coords
            // (absolute screen coords since image2 starts at 0,0).
            let diffs = find_different_rects_sub::<4>(
                &prev.data,   prev.stride,  prev.width,  prev.height,
                &new_data,    stride,       w,           h,
                0, 0,
            );

            // Update stored frame before borrowing `new_bitmap`
            self.prev = Some(PrevFrame { data: new_data, stride, width: w, height: h });

            if diffs.is_empty() {
                continue; // nothing changed — skip frame
            }

            debug!("{} dirty rect(s) this frame", diffs.len());

            // ── 6. Carve out sub-region BitmapUpdates and queue them ──────────────
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
                continue; // all rects were degenerate — skip
            }
            // Fall through to drain the queue at the top of the loop
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn make_bitmap(data: Bytes, w: usize, h: usize, stride: usize) -> Result<BitmapUpdate> {
    // SCKit delivers kCVPixelFormatType_32BGRA (bytes: B G R A).
    // ironrdp ARgb32 describes the same layout as a little-endian 32-bit int:
    // A=byte3 (MSB), R=byte2, G=byte1, B=byte0 (LSB).
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
