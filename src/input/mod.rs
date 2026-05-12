mod keyboard;
mod mouse;

use anyhow::Result;
use crate::rdp::MouseButton;

/// Injects keyboard and mouse events into macOS via CoreGraphics CGEventPost.
///
/// Required: Accessibility permission (System Settings → Privacy & Security
/// → Accessibility). Without it, CGEventPost silently does nothing.
pub struct InputInjector;

impl InputInjector {
    pub fn new() -> Result<Self> {
        check_accessibility_permission()?;
        Ok(Self)
    }

    pub fn move_mouse(&self, x: u16, y: u16) -> Result<()> {
        mouse::move_to(x as f64, y as f64)
    }

    pub fn mouse_button(&self, button: MouseButton, pressed: bool, x: u16, y: u16) -> Result<()> {
        mouse::click(button, pressed, x as f64, y as f64)
    }

    /// Injects a key event.
    /// `scancode` is the Windows scancode from the RDP PDU.
    /// `flags` carries extended-key and other RDP modifiers.
    pub fn key_event(&self, scancode: u16, pressed: bool, flags: u16) -> Result<()> {
        let keycode = keyboard::scancode_to_cg(scancode, flags)?;
        keyboard::post(keycode, pressed)
    }
}

fn check_accessibility_permission() -> Result<()> {
    // TODO: call AXIsProcessTrustedWithOptions via objc2-app-kit to check
    // and prompt for Accessibility access.
    tracing::warn!(
        "Accessibility permission check not yet implemented. \
         Grant access in System Settings → Privacy & Security → Accessibility."
    );
    Ok(())
}
