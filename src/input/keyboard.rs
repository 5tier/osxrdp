/// Windows scancode → macOS CGKeyCode translation + event posting.
///
/// Scancode set 1 (used by RDP) maps to macOS virtual keycodes.
/// Reference: https://download.microsoft.com/download/1/6/1/161ba512-40e2-4cc9-843a-923143f3456c/translate.pdf

use anyhow::{bail, Result};

/// macOS CGKeyCode type alias (u16 in CoreGraphics).
pub type CgKeyCode = u16;

/// Translate an RDP scancode (set 1) to a macOS CGKeyCode.
/// `flags` bit 0x0100 indicates an extended key (e0 prefix).
pub fn scancode_to_cg(scancode: u16, flags: u16) -> Result<CgKeyCode> {
    let extended = (flags & 0x0100) != 0;
    let code = match (scancode, extended) {
        // Row 1 – digits
        (0x02, _) => 18,  // 1
        (0x03, _) => 19,  // 2
        (0x04, _) => 20,  // 3
        (0x05, _) => 21,  // 4
        (0x06, _) => 23,  // 5
        (0x07, _) => 22,  // 6
        (0x08, _) => 26,  // 7
        (0x09, _) => 28,  // 8
        (0x0A, _) => 25,  // 9
        (0x0B, _) => 29,  // 0
        // Row 2 – QWERTY
        (0x10, _) => 12,  // Q
        (0x11, _) => 13,  // W
        (0x12, _) => 14,  // E
        (0x13, _) => 15,  // R
        (0x14, _) => 17,  // T
        (0x15, _) => 16,  // Y
        (0x16, _) => 32,  // U
        (0x17, _) => 34,  // I
        (0x18, _) => 31,  // O
        (0x19, _) => 35,  // P
        // Row 3 – ASDF
        (0x1E, _) => 0,   // A
        (0x1F, _) => 1,   // S
        (0x20, _) => 2,   // D
        (0x21, _) => 3,   // F
        (0x22, _) => 5,   // G
        (0x23, _) => 4,   // H
        (0x24, _) => 38,  // J
        (0x25, _) => 40,  // K
        (0x26, _) => 37,  // L
        // Row 4 – ZXCV
        (0x2C, _) => 6,   // Z
        (0x2D, _) => 7,   // X
        (0x2E, _) => 8,   // C
        (0x2F, _) => 9,   // V
        (0x30, _) => 11,  // B
        (0x31, _) => 45,  // N
        (0x32, _) => 46,  // M
        // Control keys
        (0x01, _) => 53,  // Escape
        (0x0E, _) => 51,  // Backspace
        (0x0F, _) => 48,  // Tab
        (0x1C, false) => 36, // Return
        (0x1C, true)  => 76, // Numpad Enter
        (0x39, _) => 49,  // Space
        (0x2A, _) => 56,  // Left Shift
        (0x36, _) => 60,  // Right Shift
        (0x1D, false) => 59, // Left Control
        (0x1D, true)  => 62, // Right Control
        (0x38, false) => 58, // Left Alt  → Left Option
        (0x38, true)  => 61, // Right Alt → Right Option
        (0x5B, _) => 55,  // Left Win  → Left Command
        (0x5C, _) => 54,  // Right Win → Right Command
        // Navigation
        (0x48, true)  => 126, // Up arrow
        (0x50, true)  => 125, // Down arrow
        (0x4B, true)  => 123, // Left arrow
        (0x4D, true)  => 124, // Right arrow
        (0x47, true)  => 115, // Home
        (0x4F, true)  => 119, // End
        (0x49, true)  => 116, // Page Up
        (0x51, true)  => 121, // Page Down
        (0x52, true)  => 117, // Delete (forward)
        // Function keys
        (0x3B, _) => 122, // F1
        (0x3C, _) => 120, // F2
        (0x3D, _) => 99,  // F3
        (0x3E, _) => 118, // F4
        (0x3F, _) => 96,  // F5
        (0x40, _) => 97,  // F6
        (0x41, _) => 98,  // F7
        (0x42, _) => 100, // F8
        (0x43, _) => 101, // F9
        (0x44, _) => 109, // F10
        (0x57, _) => 103, // F11
        (0x58, _) => 111, // F12
        _ => bail!("unmapped scancode 0x{:02X} extended={}", scancode, extended),
    };
    Ok(code)
}

pub fn post(keycode: CgKeyCode, pressed: bool) -> Result<()> {
    tracing::debug!("key {} CGKeyCode={}", if pressed { "down" } else { "up" }, keycode);
    // TODO: CGEventCreateKeyboardEvent(NULL, keycode, pressed)
    //       CGEventPost(kCGHIDEventTap, event)
    Ok(())
}
