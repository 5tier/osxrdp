use screencapturekit::async_api::AsyncSCShareableContent;
use tracing::{error, info};

/// Check Screen Recording permission by probing SCShareableContent.
/// Returns true if granted (or if the error is unrelated to permission).
async fn check_screen_recording() -> bool {
    match AsyncSCShareableContent::get().await {
        Ok(_) => true,
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            if msg.contains("denied") || msg.contains("permission") || msg.contains("not entitled") {
                false
            } else {
                true
            }
        }
    }
}

/// Check permissions at startup and log actionable guidance for anything missing.
/// Does not abort — the server starts regardless so users can grant permissions
/// and reconnect without restarting.
pub async fn check_and_warn() {
    if check_screen_recording().await {
        info!("Screen Recording permission OK");
    } else {
        error!(
            "Screen Recording permission NOT granted.\n\
             \n\
             Without it osxrdp cannot capture the screen; RDP clients will\n\
             disconnect immediately after connecting.\n\
             \n\
             Grant it to osxrdp (or your terminal when using `cargo run`):\n\
             \n\
               System Settings → Privacy & Security → Screen Recording\n\
             \n\
             Enable the toggle, then restart osxrdp."
        );
    }
}
