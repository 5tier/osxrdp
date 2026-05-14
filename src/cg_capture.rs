//! CoreGraphics-based screen capture as an alternative to SCKit.
//!
//! SCKit crashes (SIGSEGV in `createContentFilterWithDisplay`) after any
//! display mode change, even from a separate process. This module provides
//! a `CGDisplayCapture` that uses `CGDisplayCreateImage` for screen capture,
//! which works regardless of display mode changes.
//!
//! The capture uses `CGDisplay::image()` to capture the screen as a `CGImage`,
//! then draws it into a BGRA bitmap context. This produces BGRA pixel data
//! compatible with the existing frame handling pipeline.

use core_graphics::base::{kCGBitmapByteOrder32Little, kCGImageAlphaPremultipliedFirst};
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::CGContext;
use core_graphics::display::CGDisplay;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use tracing::warn;

/// BGRA bitmap data captured from the display.
pub struct CapturedFrame {
    /// The BGRA pixel data (BGRA byte order, 4 bytes per pixel).
    pub data: Vec<u8>,
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// Bytes per row (may include padding for alignment).
    pub stride: usize,
}

/// Captures a single frame from the main display using CoreGraphics.
///
/// This uses `CGDisplayCreateImage` followed by drawing into a BGRA bitmap
/// context. It does NOT use SCKit and is safe to call after display mode
/// changes.
///
/// The output is always BGRA (Blue, Green, Red, Alpha) with alpha = 255,
/// matching the format expected by the RDP bitmap pipeline.
pub fn capture_display_bgra() -> Option<CapturedFrame> {
    let display = CGDisplay::main();
    capture_display_bgra_with_id(display.id)
}

/// Captures a single frame from a specific display using CoreGraphics.
pub fn capture_display_bgra_with_id(display_id: u32) -> Option<CapturedFrame> {
    let display = CGDisplay::new(display_id);
    let cg_image = display.image()?;

    let width = cg_image.width() as usize;
    let height = cg_image.height() as usize;

    if width == 0 || height == 0 {
        warn!("CGDisplayCreateImage returned zero-size image");
        return None;
    }

    // Create a BGRA bitmap context.
    // kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little gives us
    // ARGB on big-endian but on little-endian (all modern Macs) this produces
    // BGRA in memory, which is exactly what RDP expects.
    let stride = width * 4;
    // Align stride to 16 bytes for better SIMD performance
    let stride = (stride + 15) & !15;
    let buffer_size = stride * height;
    let mut buffer = vec![0u8; buffer_size];

    let color_space = CGColorSpace::create_device_rgb();
    let bitmap_info = kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little;

    let mut context = CGContext::create_bitmap_context(
        Some(buffer.as_mut_ptr() as *mut std::ffi::c_void),
        width,
        height,
        8, // bits per component
        stride,
        &color_space,
        bitmap_info,
    );

    // Draw the captured image into the bitmap context.
    let rect = CGRect::new(
        &CGPoint::new(0.0, 0.0),
        &CGSize::new(width as f64, height as f64),
    );
    context.draw_image(rect, &cg_image);

    // The bitmap context has drawn the image into our buffer in BGRA format.
    // Read it back from the context data pointer (context may have used its
    // own internal buffer rather than ours).
    let context_data = context.data();
    buffer[..context_data.len()].copy_from_slice(context_data);

    // Fix premultiplied alpha: RDP expects alpha=255, but CGImage may have
    // premultiplied alpha values. Force alpha channel to 255 for all pixels.
    for pixel in buffer.chunks_exact_mut(4) {
        pixel[3] = 255; // Alpha
    }

    Some(CapturedFrame {
        data: buffer,
        width,
        height,
        stride,
    })
}