/// Mouse event injection via CGEventPost.
///
/// Real implementation uses objc2-core-graphics:
///   CGEventCreateMouseEvent(null, eventType, point, button)
///   CGEventPost(kCGHIDEventTap, event)

use crate::rdp::MouseButton;
use anyhow::Result;

pub fn move_to(x: f64, y: f64) -> Result<()> {
    tracing::debug!("mouse move → ({}, {})", x, y);
    // TODO: CGEventCreateMouseEvent(NULL, kCGEventMouseMoved, CGPoint{x,y}, 0)
    Ok(())
}

pub fn click(button: MouseButton, pressed: bool, x: f64, y: f64) -> Result<()> {
    tracing::debug!("mouse {:?} {} @ ({}, {})", button, if pressed { "down" } else { "up" }, x, y);
    // TODO: map button + pressed → kCGEventLeftMouseDown / kCGEventLeftMouseUp / etc.
    Ok(())
}
