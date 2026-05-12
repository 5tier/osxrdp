use crate::keyboard::scancode_to_cg;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use tracing::{debug, warn};

/// Injects keyboard and mouse events into macOS via CoreGraphics CGEventPost.
///
/// Requires Accessibility permission:
///   System Settings → Privacy & Security → Accessibility → osxrdp ✓
pub struct MacInputHandler {
    cursor_x: f64,
    cursor_y: f64,
    /// Track modifier key state so Cmd+Tab and similar combinations work.
    cmd_pressed: bool,
    alt_pressed: bool,
    shift_pressed: bool,
    ctrl_pressed: bool,
}

impl MacInputHandler {
    pub fn new() -> Self {
        Self {
            cursor_x: 0.0,
            cursor_y: 0.0,
            cmd_pressed: false,
            alt_pressed: false,
            shift_pressed: false,
            ctrl_pressed: false,
        }
    }

    fn source() -> Option<CGEventSource> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()
    }

    fn post(event: CGEvent) {
        event.post(CGEventTapLocation::HID);
    }

    fn modifiers(&self) -> CGEventFlags {
        let mut f = CGEventFlags::empty();
        if self.cmd_pressed {
            f.insert(CGEventFlags::CGEventFlagCommand);
        }
        if self.alt_pressed {
            f.insert(CGEventFlags::CGEventFlagAlternate);
        }
        if self.shift_pressed {
            f.insert(CGEventFlags::CGEventFlagShift);
        }
        if self.ctrl_pressed {
            f.insert(CGEventFlags::CGEventFlagControl);
        }
        f
    }

    /// RDP scancodes for modifier keys (set 1).
    const SCAN_CMD: u8 = 0x5B;
    const SCAN_CMD_RIGHT: u8 = 0x5C;
    const SCAN_ALT: u8 = 0x38;
    const SCAN_SHIFT: u8 = 0x2A;
    const SCAN_SHIFT_RIGHT: u8 = 0x36;
    const SCAN_CTRL: u8 = 0x1D;

    fn flash_key(&mut self, code: u8, pressed: bool) {
        match code {
            Self::SCAN_CMD | Self::SCAN_CMD_RIGHT => self.cmd_pressed = pressed,
            Self::SCAN_ALT => self.alt_pressed = pressed,
            Self::SCAN_SHIFT | Self::SCAN_SHIFT_RIGHT => self.shift_pressed = pressed,
            Self::SCAN_CTRL => self.ctrl_pressed = pressed,
            _ => {}
        }
    }

    fn post_mouse(
        &self,
        event_type: CGEventType,
        x: f64,
        y: f64,
        button: CGMouseButton,
    ) {
        let Some(src) = Self::source() else { return };
        match CGEvent::new_mouse_event(src, event_type, CGPoint::new(x, y), button) {
            Ok(ev) => Self::post(ev),
            Err(()) => warn!("CGEvent::new_mouse_event failed"),
        }
    }
}

impl RdpServerInputHandler for MacInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Pressed { code, extended } => {
                self.flash_key(code, true);
                if let Some(cg_code) = scancode_to_cg(code, extended) {
                    if let Some(src) = Self::source() {
                        match CGEvent::new_keyboard_event(src, cg_code, true) {
                            Ok(ev) => {
                                ev.set_flags(self.modifiers());
                                Self::post(ev);
                            }
                            Err(()) => warn!("CGEvent keyboard down failed (code={cg_code})"),
                        }
                    }
                } else {
                    debug!("Unmapped scancode 0x{code:02X} extended={extended}");
                }
            }
            KeyboardEvent::Released { code, extended } => {
                if let Some(cg_code) = scancode_to_cg(code, extended) {
                    if let Some(src) = Self::source() {
                        match CGEvent::new_keyboard_event(src, cg_code, false) {
                            Ok(ev) => {
                                ev.set_flags(self.modifiers());
                                Self::post(ev);
                            }
                            Err(()) => warn!("CGEvent keyboard up failed (code={cg_code})"),
                        }
                    }
                }
                self.flash_key(code, false);
            }
            KeyboardEvent::UnicodePressed(ch) | KeyboardEvent::UnicodeReleased(ch) => {
                let pressed = matches!(event, KeyboardEvent::UnicodePressed(_));
                if let Some(src) = Self::source() {
                    if let Ok(ev) = CGEvent::new_keyboard_event(src, 0, pressed) {
                        ev.set_string_from_utf16_unchecked(&[ch]);
                        Self::post(ev);
                    }
                }
            }
            KeyboardEvent::Synchronize(_flags) => {}
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        match event {
            MouseEvent::Move { x, y } => {
                self.cursor_x = f64::from(x);
                self.cursor_y = f64::from(y);
                self.post_mouse(CGEventType::MouseMoved, self.cursor_x, self.cursor_y, CGMouseButton::Left);
            }
            MouseEvent::LeftPressed => {
                self.post_mouse(CGEventType::LeftMouseDown, self.cursor_x, self.cursor_y, CGMouseButton::Left);
            }
            MouseEvent::LeftReleased => {
                self.post_mouse(CGEventType::LeftMouseUp, self.cursor_x, self.cursor_y, CGMouseButton::Left);
            }
            MouseEvent::RightPressed => {
                self.post_mouse(CGEventType::RightMouseDown, self.cursor_x, self.cursor_y, CGMouseButton::Right);
            }
            MouseEvent::RightReleased => {
                self.post_mouse(CGEventType::RightMouseUp, self.cursor_x, self.cursor_y, CGMouseButton::Right);
            }
            MouseEvent::MiddlePressed => {
                self.post_mouse(CGEventType::OtherMouseDown, self.cursor_x, self.cursor_y, CGMouseButton::Center);
            }
            MouseEvent::MiddleReleased => {
                self.post_mouse(CGEventType::OtherMouseUp, self.cursor_x, self.cursor_y, CGMouseButton::Center);
            }
            MouseEvent::VerticalScroll { value } => {
                let lines = i32::from(value) / 120;
                if let Some(src) = Self::source() {
                    if let Ok(ev) = CGEvent::new_scroll_event(
                        src,
                        core_graphics::event::ScrollEventUnit::LINE,
                        1,
                        lines,
                        0,
                        0,
                    ) {
                        Self::post(ev);
                    }
                }
            }
            MouseEvent::Scroll { x: _, y } => {
                let lines = y / 120;
                if let Some(src) = Self::source() {
                    if let Ok(ev) = CGEvent::new_scroll_event(
                        src,
                        core_graphics::event::ScrollEventUnit::LINE,
                        1,
                        lines,
                        0,
                        0,
                    ) {
                        Self::post(ev);
                    }
                }
            }
            MouseEvent::RelMove { x, y } => {
                self.cursor_x += f64::from(x);
                self.cursor_y += f64::from(y);
                self.post_mouse(CGEventType::MouseMoved, self.cursor_x, self.cursor_y, CGMouseButton::Left);
            }
            _ => {
                debug!("Unhandled mouse event: {event:?}");
            }
        }
    }
}
