use crate::keyboard::scancode_to_cg;
use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use tracing::{debug, warn};

/// Injects keyboard and mouse events into macOS via CoreGraphics CGEventPost.
///
/// Requires Accessibility permission:
///   System Settings → Privacy & Security → Accessibility → osxrdp ✓
pub struct MacInputHandler {
    /// Last known pointer position — needed for button-click events that don't
    /// carry a position in some RDP encodings.
    cursor_x: f64,
    cursor_y: f64,
}

impl MacInputHandler {
    pub fn new() -> Self {
        Self { cursor_x: 0.0, cursor_y: 0.0 }
    }

    fn source() -> Option<CGEventSource> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()
    }

    fn post(event: CGEvent) {
        event.post(CGEventTapLocation::HID);
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
                if let Some(cg_code) = scancode_to_cg(code, extended) {
                    if let Some(src) = Self::source() {
                        match CGEvent::new_keyboard_event(src, cg_code, true) {
                            Ok(ev) => Self::post(ev),
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
                            Ok(ev) => Self::post(ev),
                            Err(()) => warn!("CGEvent keyboard up failed (code={cg_code})"),
                        }
                    }
                }
            }
            KeyboardEvent::UnicodePressed(ch) | KeyboardEvent::UnicodeReleased(ch) => {
                // Unicode input: post a keyboard event with keycode=0 and set the string.
                // This handles IME / non-Latin input methods correctly.
                let pressed = matches!(event, KeyboardEvent::UnicodePressed(_));
                if let Some(src) = Self::source() {
                    if let Ok(ev) = CGEvent::new_keyboard_event(src, 0, pressed) {
                        ev.set_string_from_utf16_unchecked(&[ch]);
                        Self::post(ev);
                    }
                }
            }
            KeyboardEvent::Synchronize(_flags) => {
                // Caps Lock / Num Lock / Scroll Lock sync — handled by macOS automatically.
            }
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        match event {
            MouseEvent::Move { x, y } => {
                self.cursor_x = f64::from(x);
                self.cursor_y = f64::from(y);
                self.post_mouse(
                    CGEventType::MouseMoved,
                    self.cursor_x,
                    self.cursor_y,
                    CGMouseButton::Left,
                );
            }
            MouseEvent::LeftPressed => {
                self.post_mouse(
                    CGEventType::LeftMouseDown,
                    self.cursor_x,
                    self.cursor_y,
                    CGMouseButton::Left,
                );
            }
            MouseEvent::LeftReleased => {
                self.post_mouse(
                    CGEventType::LeftMouseUp,
                    self.cursor_x,
                    self.cursor_y,
                    CGMouseButton::Left,
                );
            }
            MouseEvent::RightPressed => {
                self.post_mouse(
                    CGEventType::RightMouseDown,
                    self.cursor_x,
                    self.cursor_y,
                    CGMouseButton::Right,
                );
            }
            MouseEvent::RightReleased => {
                self.post_mouse(
                    CGEventType::RightMouseUp,
                    self.cursor_x,
                    self.cursor_y,
                    CGMouseButton::Right,
                );
            }
            MouseEvent::MiddlePressed => {
                self.post_mouse(
                    CGEventType::OtherMouseDown,
                    self.cursor_x,
                    self.cursor_y,
                    CGMouseButton::Center,
                );
            }
            MouseEvent::MiddleReleased => {
                self.post_mouse(
                    CGEventType::OtherMouseUp,
                    self.cursor_x,
                    self.cursor_y,
                    CGMouseButton::Center,
                );
            }
            MouseEvent::VerticalScroll { value } => {
                // value > 0 = scroll up, value < 0 = scroll down
                // CGEvent scroll uses line units; divide by typical wheel delta (120).
                let lines = i32::from(value) / 120;
                if let Some(src) = MacInputHandler::source() {
                    if let Ok(ev) = CGEvent::new_scroll_event(
                        src,
                        core_graphics::event::ScrollEventUnit::LINE,
                        1,   // wheel count
                        lines,
                        0,   // no horizontal
                        0,
                    ) {
                        MacInputHandler::post(ev);
                    }
                }
            }
            MouseEvent::Scroll { x: _, y } => {
                let lines = y / 120;
                if let Some(src) = MacInputHandler::source() {
                    if let Ok(ev) = CGEvent::new_scroll_event(
                        src,
                        core_graphics::event::ScrollEventUnit::LINE,
                        1,
                        lines,
                        0,
                        0,
                    ) {
                        MacInputHandler::post(ev);
                    }
                }
            }
            MouseEvent::RelMove { x, y } => {
                self.cursor_x += f64::from(x as i16);
                self.cursor_y += f64::from(y as i16);
                self.post_mouse(
                    CGEventType::MouseMoved,
                    self.cursor_x,
                    self.cursor_y,
                    CGMouseButton::Left,
                );
            }
            // Extended buttons (Button4/5) — not commonly needed, skip.
            MouseEvent::Button4Pressed
            | MouseEvent::Button4Released
            | MouseEvent::Button5Pressed
            | MouseEvent::Button5Released => {}
        }
    }
}
