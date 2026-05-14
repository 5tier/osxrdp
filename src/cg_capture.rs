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
//!
//! Two output formats are supported:
//! - `capture_display_bgra()` → raw BGRA bytes (for BitmapUpdate PDUs)
//! - `capture_display_as_cv_pixel_buffer()` → CVPixelBuffer (for H.264 encoding)

use core_graphics::base::{kCGBitmapByteOrder32Little, kCGImageAlphaPremultipliedFirst};
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::CGContext;
use core_graphics::display::CGDisplay;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use std::ffi::c_void;
use std::ptr;
use tracing::warn;

// ─── CoreVideo FFI ──────────────────────────────────────────────────────────

/// BGRA pixel format: 32-bit BGRA, 8 bits per component.
/// <https://developer.apple.com/documentation/corevideo/kcvpixelformattype_32bgra>
const K_CV_PIXEL_FORMAT_TYPE_32BGRA: u32 = 0x42475241; // 'BGRA'

extern "C" {
    fn CVPixelBufferCreate(
        allocator: *const c_void,
        width: i32,
        height: i32,
        pixel_format_type: u32,
        pixel_buffer_attributes: *const c_void,
        pixel_buffer_out: *mut *mut c_void,
    ) -> i32;

    fn CVPixelBufferLockBaseAddress(
        pixel_buffer: *const c_void,
        lock_flags: u32,
    ) -> i32;

    fn CVPixelBufferUnlockBaseAddress(
        pixel_buffer: *const c_void,
        unlock_flags: u32,
    ) -> i32;

    fn CVPixelBufferGetBaseAddress(pixel_buffer: *const c_void) -> *mut c_void;

    fn CVPixelBufferGetBytesPerRow(pixel_buffer: *const c_void) -> usize;

    fn CVBufferRelease(buffer: *const c_void);

    // These are extern global symbols (global variables, not functions).
    // On ARM64 macOS, declaring them as `fn` causes SIGBUS.
    static kCVPixelBufferWidthKey: *const c_void;
    static kCVPixelBufferHeightKey: *const c_void;
    static kCVPixelBufferBytesPerRowAlignmentKey: *const c_void;
    static kCVPixelBufferCGImageCompatibilityKey: *const c_void;
    static kCVPixelBufferCGBitmapContextCompatibilityKey: *const c_void;

    // CFDictionary callbacks — CoreFoundation singleton globals.
    static kCFTypeDictionaryKeyCallBacks: *const c_void;
    static kCFTypeDictionaryValueCallBacks: *const c_void;

    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: usize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;

    fn CFNumberCreate(
        allocator: *const c_void,
        the_type: i32,
        value_ptr: *const c_void,
    ) -> *const c_void;

    fn CFRelease(cf: *const c_void);
}

const K_CF_NUMBER_SINT32_TYPE: i32 = 3;

/// A CVPixelBuffer containing BGRA pixel data, created from a CGDisplay capture.
///
/// This wrapper manages the CVPixelBuffer's lifetime: it locks the base address
/// on creation and unlocks + releases on drop. The locked base address can be
/// used to copy BGRA pixel data into the buffer for VideoToolbox encoding.
///
/// # Safety
///
/// The inner pointer is a valid CVPixelBufferRef. The base address is locked
/// for the lifetime of this struct. Dropping releases the lock and the buffer.
pub struct CVPixelBufferWrapper {
    ptr: *const c_void,
    base_address: *mut c_void,
    bytes_per_row: usize,
    #[allow(dead_code)]
    width: usize,
    #[allow(dead_code)]
    height: usize,
}

// SAFETY: The CVPixelBuffer is a CoreFoundation object that is safe to access
// from any thread (we lock the base address and never mutate it concurrently).
unsafe impl Send for CVPixelBufferWrapper {}
unsafe impl Sync for CVPixelBufferWrapper {}

impl CVPixelBufferWrapper {
    /// Returns the raw CVPixelBufferRef pointer for use with VideoToolbox.
    ///
    /// This pointer remains valid for the lifetime of this wrapper.
    pub fn as_ptr(&self) -> *const c_void {
        self.ptr
    }

    /// Returns the width of the pixel buffer in pixels.
    #[allow(dead_code)]
    pub fn width(&self) -> usize {
        self.width
    }

    /// Returns the height of the pixel buffer in pixels.
    #[allow(dead_code)]
    pub fn height(&self) -> usize {
        self.height
    }

    /// Returns the bytes per row (stride) of the pixel buffer.
    #[allow(dead_code)]
    pub fn bytes_per_row(&self) -> usize {
        self.bytes_per_row
    }
}

impl Drop for CVPixelBufferWrapper {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                CVPixelBufferUnlockBaseAddress(self.ptr, 0);
                CVBufferRelease(self.ptr);
            }
        }
    }
}

/// Create an empty CVPixelBuffer with BGRA pixel format.
///
/// The buffer is locked for writing — the caller should write BGRA pixel data
/// to the base address and then pass it to VideoToolbox for encoding.
unsafe fn create_bgra_pixel_buffer(width: usize, height: usize) -> Option<CVPixelBufferWrapper> {
    let mut pixel_buffer: *mut c_void = ptr::null_mut();

    // Build pixel buffer attributes dictionary.
    // We specify width, height, and bytes-per-row alignment so that
    // VideoToolbox can use the buffer efficiently.
    let width_num = CFNumberCreate(
        ptr::null(),
        K_CF_NUMBER_SINT32_TYPE,
        &(width as i32) as *const i32 as *const c_void,
    );
    let height_num = CFNumberCreate(
        ptr::null(),
        K_CF_NUMBER_SINT32_TYPE,
        &(height as i32) as *const i32 as *const c_void,
    );
    // Align stride to 64 bytes for good SIMD/cache performance
    let aligned_stride = ((width * 4 + 63) & !63) as i32;
    let stride_num = CFNumberCreate(
        ptr::null(),
        K_CF_NUMBER_SINT32_TYPE,
        &aligned_stride as *const i32 as *const c_void,
    );
    let compat_num = CFNumberCreate(
        ptr::null(),
        K_CF_NUMBER_SINT32_TYPE,
        &(1i32) as *const i32 as *const c_void,
    );
    // CGBitmapContext compatibility is needed to draw into this pixel buffer
    // via CGContext.
    let cg_compat_num = CFNumberCreate(
        ptr::null(),
        K_CF_NUMBER_SINT32_TYPE,
        &(1i32) as *const i32 as *const c_void,
    );

    let keys: [*const c_void; 5] = [
        unsafe { kCVPixelBufferWidthKey },
        unsafe { kCVPixelBufferHeightKey },
        unsafe { kCVPixelBufferBytesPerRowAlignmentKey },
        unsafe { kCVPixelBufferCGImageCompatibilityKey },
        unsafe { kCVPixelBufferCGBitmapContextCompatibilityKey },
    ];
    let values: [*const c_void; 5] = [width_num, height_num, stride_num, compat_num, cg_compat_num];

    let attrs = unsafe {
        CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            5,
            kCFTypeDictionaryKeyCallBacks,
            kCFTypeDictionaryValueCallBacks,
        )
    };

    // Release the CFNumbers — the dictionary has retained them.
    unsafe {
        CFRelease(width_num);
        CFRelease(height_num);
        CFRelease(stride_num);
        CFRelease(compat_num);
        CFRelease(cg_compat_num);
    }

    let status = unsafe {
        CVPixelBufferCreate(
            ptr::null(),
            width as i32,
            height as i32,
            K_CV_PIXEL_FORMAT_TYPE_32BGRA,
            attrs,
            &mut pixel_buffer,
        )
    };

    unsafe { CFRelease(attrs) };

    if status != 0 {
        warn!("CVPixelBufferCreate failed with status {status}");
        return None;
    }
    if pixel_buffer.is_null() {
        warn!("CVPixelBufferCreate returned null buffer");
        return None;
    }

    // Lock the base address for writing
    let lock_status = unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, 0) };
    if lock_status != 0 {
        warn!("CVPixelBufferLockBaseAddress failed with status {lock_status}");
        unsafe { CVBufferRelease(pixel_buffer) };
        return None;
    }

    let base_address = unsafe { CVPixelBufferGetBaseAddress(pixel_buffer) };
    let bytes_per_row = unsafe { CVPixelBufferGetBytesPerRow(pixel_buffer) };

    Some(CVPixelBufferWrapper {
        ptr: pixel_buffer,
        base_address,
        bytes_per_row,
        width,
        height,
    })
}

// ─── Capture functions ──────────────────────────────────────────────────────

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

/// Captures a single frame from the main display as a CVPixelBuffer (BGRA format).
///
/// This is the H.264 encoder path: captures the screen via CGDisplay, draws
/// it into a CVPixelBuffer, and returns the buffer ready for VideoToolbox encoding.
/// The returned buffer is locked and its base address is filled with BGRA pixel data.
///
/// Returns `None` if the capture or CVPixelBuffer creation fails.
pub fn capture_display_as_cv_pixel_buffer() -> Option<CVPixelBufferWrapper> {
    let display = CGDisplay::main();
    let cg_image = display.image()?;

    let width = cg_image.width() as usize;
    let height = cg_image.height() as usize;

    if width == 0 || height == 0 {
        warn!("CGDisplayCreateImage returned zero-size image");
        return None;
    }

    // Create a CVPixelBuffer with BGRA format.
    let cv_buffer = unsafe { create_bgra_pixel_buffer(width, height) }?;

    // Draw the CGImage into the CVPixelBuffer using a CGContext.
    let base_address = cv_buffer.base_address;
    let bytes_per_row = cv_buffer.bytes_per_row;

    // Zero the buffer first (CVPixelBuffer memory may be uninitialized)
    unsafe {
        ptr::write_bytes(base_address as *mut u8, 0, bytes_per_row * height);
    }

    let color_space = CGColorSpace::create_device_rgb();
    let bitmap_info = kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little;

    let context = CGContext::create_bitmap_context(
            Some(base_address),
            width,
            height,
            8, // bits per component
            bytes_per_row,
            &color_space,
            bitmap_info,
        );

    let rect = CGRect::new(
        &CGPoint::new(0.0, 0.0),
        &CGSize::new(width as f64, height as f64),
    );
    context.draw_image(rect, &cg_image);

    // Fix premultiplied alpha: VideoToolbox expects alpha=255
    unsafe {
        let dst = base_address as *mut u8;
        for row in 0..height {
            let row_start = dst.add(row * bytes_per_row);
            for col in 0..width {
                let pixel = row_start.add(col * 4);
                *pixel.add(3) = 255; // Alpha channel = opaque
            }
        }
    }

    Some(cv_buffer)
}