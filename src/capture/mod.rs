mod screencapturekit;

use anyhow::Result;

/// A single captured frame: raw BGRA pixels, width × height.
pub struct Frame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Screen capturer backed by ScreenCaptureKit on macOS 12.3+.
pub struct Capturer {
    inner: screencapturekit::SckCapturer,
}

impl Capturer {
    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: screencapturekit::SckCapturer::new()?,
        })
    }

    /// Await the next frame from the display.
    pub async fn next_frame(&mut self) -> Result<Frame> {
        self.inner.next_frame().await
    }
}
