use core_graphics::event::KeyCode;

/// Translate an RDP scancode (set 1, as delivered by ironrdp) to a macOS CGKeyCode.
///
/// `extended` is the ironrdp `KeyboardEvent::Pressed { extended, .. }` flag,
/// equivalent to the RDP 0x0100 extended-key bit.
///
/// Returns `None` for unmapped keys (they are silently dropped).
pub fn scancode_to_cg(code: u8, extended: bool) -> Option<u16> {
    let k = match (code, extended) {
        // ── Row 1: digit row ─────────────────────────────────────────────────
        (0x29, _) => KeyCode::ANSI_GRAVE,
        (0x02, _) => KeyCode::ANSI_1,
        (0x03, _) => KeyCode::ANSI_2,
        (0x04, _) => KeyCode::ANSI_3,
        (0x05, _) => KeyCode::ANSI_4,
        (0x06, _) => KeyCode::ANSI_5,
        (0x07, _) => KeyCode::ANSI_6,
        (0x08, _) => KeyCode::ANSI_7,
        (0x09, _) => KeyCode::ANSI_8,
        (0x0A, _) => KeyCode::ANSI_9,
        (0x0B, _) => KeyCode::ANSI_0,
        (0x0C, _) => KeyCode::ANSI_MINUS,
        (0x0D, _) => KeyCode::ANSI_EQUAL,
        (0x0E, _) => KeyCode::DELETE,           // Backspace

        // ── Row 2: QWERTY ────────────────────────────────────────────────────
        (0x0F, _) => KeyCode::TAB,
        (0x10, _) => KeyCode::ANSI_Q,
        (0x11, _) => KeyCode::ANSI_W,
        (0x12, _) => KeyCode::ANSI_E,
        (0x13, _) => KeyCode::ANSI_R,
        (0x14, _) => KeyCode::ANSI_T,
        (0x15, _) => KeyCode::ANSI_Y,
        (0x16, _) => KeyCode::ANSI_U,
        (0x17, _) => KeyCode::ANSI_I,
        (0x18, _) => KeyCode::ANSI_O,
        (0x19, _) => KeyCode::ANSI_P,
        (0x1A, _) => KeyCode::ANSI_LEFT_BRACKET,
        (0x1B, _) => KeyCode::ANSI_RIGHT_BRACKET,
        (0x2B, _) => KeyCode::ANSI_BACKSLASH,

        // ── Row 3: ASDF ──────────────────────────────────────────────────────
        (0x3A, _) => KeyCode::CAPS_LOCK,
        (0x1E, _) => KeyCode::ANSI_A,
        (0x1F, _) => KeyCode::ANSI_S,
        (0x20, _) => KeyCode::ANSI_D,
        (0x21, _) => KeyCode::ANSI_F,
        (0x22, _) => KeyCode::ANSI_G,
        (0x23, _) => KeyCode::ANSI_H,
        (0x24, _) => KeyCode::ANSI_J,
        (0x25, _) => KeyCode::ANSI_K,
        (0x26, _) => KeyCode::ANSI_L,
        (0x27, _) => KeyCode::ANSI_SEMICOLON,
        (0x28, _) => KeyCode::ANSI_QUOTE,
        (0x1C, false) => KeyCode::RETURN,
        (0x1C, true)  => KeyCode::ANSI_KEYPAD_ENTER,

        // ── Row 4: ZXCV ──────────────────────────────────────────────────────
        (0x2A, _) => KeyCode::SHIFT,
        (0x2C, _) => KeyCode::ANSI_Z,
        (0x2D, _) => KeyCode::ANSI_X,
        (0x2E, _) => KeyCode::ANSI_C,
        (0x2F, _) => KeyCode::ANSI_V,
        (0x30, _) => KeyCode::ANSI_B,
        (0x31, _) => KeyCode::ANSI_N,
        (0x32, _) => KeyCode::ANSI_M,
        (0x33, _) => KeyCode::ANSI_COMMA,
        (0x34, _) => KeyCode::ANSI_PERIOD,
        (0x35, false) => KeyCode::ANSI_SLASH,
        (0x36, _) => KeyCode::RIGHT_SHIFT,

        // ── Bottom row ───────────────────────────────────────────────────────
        (0x1D, false) => KeyCode::CONTROL,
        (0x1D, true)  => KeyCode::RIGHT_CONTROL,
        (0x38, false) => KeyCode::OPTION,         // Left Alt → Option
        (0x38, true)  => KeyCode::RIGHT_OPTION,
        (0x39, _) => KeyCode::SPACE,
        (0x5B, _) => KeyCode::COMMAND,            // Left Win → Cmd
        (0x5C, _) => KeyCode::RIGHT_COMMAND,

        // ── Function keys ────────────────────────────────────────────────────
        (0x01, _) => KeyCode::ESCAPE,
        (0x3B, _) => KeyCode::F1,
        (0x3C, _) => KeyCode::F2,
        (0x3D, _) => KeyCode::F3,
        (0x3E, _) => KeyCode::F4,
        (0x3F, _) => KeyCode::F5,
        (0x40, _) => KeyCode::F6,
        (0x41, _) => KeyCode::F7,
        (0x42, _) => KeyCode::F8,
        (0x43, _) => KeyCode::F9,
        (0x44, _) => KeyCode::F10,
        (0x57, _) => KeyCode::F11,
        (0x58, _) => KeyCode::F12,
        (0x64, _) => KeyCode::F13,
        (0x65, _) => KeyCode::F14,
        (0x66, _) => KeyCode::F15,

        // ── Navigation cluster (extended scancodes) ───────────────────────────
        // 0x47..0x53 without extended bit = numpad; with extended = nav cluster
        (0x47, true) => KeyCode::HOME,
        (0x48, true) => KeyCode::UP_ARROW,
        (0x49, true) => KeyCode::PAGE_UP,
        (0x4B, true) => KeyCode::LEFT_ARROW,
        (0x4D, true) => KeyCode::RIGHT_ARROW,
        (0x4F, true) => KeyCode::END,
        (0x50, true) => KeyCode::DOWN_ARROW,
        (0x51, true) => KeyCode::PAGE_DOWN,
        (0x52, true) => KeyCode::HELP,            // Insert → Help (closest macOS equiv)
        (0x53, true) => KeyCode::FORWARD_DELETE,

        // ── Numpad (non-extended) ─────────────────────────────────────────────
        (0x45, _) => KeyCode::ANSI_KEYPAD_CLEAR,  // Num Lock → Clear
        (0x4A, _) => KeyCode::ANSI_KEYPAD_MINUS,
        (0x37, false) => KeyCode::ANSI_KEYPAD_MULTIPLY,
        (0x35, true)  => KeyCode::ANSI_KEYPAD_DIVIDE,
        (0x4E, _) => KeyCode::ANSI_KEYPAD_PLUS,
        (0x4C, _) => KeyCode::ANSI_KEYPAD_ENTER,
        (0x47, false) => KeyCode::ANSI_KEYPAD_7,
        (0x48, false) => KeyCode::ANSI_KEYPAD_8,
        (0x49, false) => KeyCode::ANSI_KEYPAD_9,
        (0x4B, false) => KeyCode::ANSI_KEYPAD_4,
        (0x4D, false) => KeyCode::ANSI_KEYPAD_6,
        (0x4F, false) => KeyCode::ANSI_KEYPAD_1,
        (0x50, false) => KeyCode::ANSI_KEYPAD_2,
        (0x51, false) => KeyCode::ANSI_KEYPAD_3,
        (0x52, false) => KeyCode::ANSI_KEYPAD_0,
        (0x53, false) => KeyCode::ANSI_KEYPAD_DECIMAL,

        // ── Print Screen / Scroll Lock (no direct macOS equivalent) ──────────────
        (0x37, true) => KeyCode::F13,   // PrtSc → F13
        (0x46, _)    => KeyCode::F16,   // Scroll Lock → F16

        _ => return None,
    };
    Some(k)
}
