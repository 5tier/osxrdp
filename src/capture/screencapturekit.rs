/// ScreenCaptureKit backend.
///
/// Real implementation will call SCShareableContent, SCStreamConfiguration,
/// and SCStream via objc2 / objc2-screen-capture-kit crates.
///
/// Integration points:
///   - SCShareableContent.getWithCompletionHandler  → pick display 0
///   - SCStreamConfiguration: width, height, pixelFormat kCVPixelFormatType_32BGRA
///   - SCStream.addStreamOutput(self, type: .screen) for frame callbacks
///   - Each callback delivers a CMSampleBuffer → CVImageBuffer → raw bytes
///
/// Required entitlement: com.apple.security.screen-recording (user must approve
/// in System Settings → Privacy & Security → Screen Recording)

use super::Frame;
use anyhow::Result;
use tokio::sync::mpsc;

pub struct SckCapturer {
    rx: mpsc::Receiver<Frame>,
}

impl SckCapturer {
    pub fn new() -> Result<Self> {
        // TODO: initialise SCStream on a dedicated dispatch queue and forward
        // CMSampleBuffer data through this channel.
        let (_tx, rx) = mpsc::channel(4);

        // Placeholder: verify Screen Recording permission is granted before
        // attempting to create a stream, otherwise macOS silently delivers
        // blank frames.
        check_screen_recording_permission()?;

        Ok(Self { rx })
    }

    pub async fn next_frame(&mut self) -> Result<Frame> {
        self.rx.recv().await.ok_or_else(|| anyhow::anyhow!("capture stream closed"))
    }
}

fn check_screen_recording_permission() -> Result<()> {
    // CGWindowListCreateImage returns NULL when Screen Recording is not granted.
    // On first run, this triggers the permission dialog.
    // TODO: call CGPreflightScreenCaptureAccess() via objc2-core-graphics.
    tracing::warn!(
        "Screen Recording permission check not yet implemented. \
         Grant access in System Settings → Privacy & Security → Screen Recording."
    );
    Ok(())
}
