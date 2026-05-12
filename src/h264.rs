//! H.264 hardware encoder via macOS VideoToolbox.
//!
//! Accepts `CVPixelBuffer` pointers (e.g. from ScreenCaptureKit's capture
//! output in NV12 / YCbCr 4:2:0 format) and produces AVCC-format H.264 NAL
//! units suitable for the RDP `Avc420BitmapStream` PDU.
//!
//! # Safety
//!
//! VideoToolbox calls the encoding callback asynchronously on a separate thread.
//! We use `Box::into_raw` / `Box::from_raw` to pass a `mpsc::Sender` through
//! the `void*` `sourceFrameRefCon` parameter, ensuring the sender lives until
//! the callback fires and no dangling pointers remain.

use anyhow::{anyhow, Result};
use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc;
use std::time::Duration;
use tracing::{debug, warn};

// ─── VideoToolbox FFI declarations ──────────────────────────────────────────

pub(crate) const K_CM_VIDEO_CODEC_TYPE_H264: u32 = 0x61766331; // 'avc1'

#[allow(dead_code)]
pub(crate) const KVPIXEL_FORMAT_420V: u32 = 0x34323076; // '420v'
#[allow(dead_code)]
pub(crate) const KVPIXEL_FORMAT_420F: u32 = 0x34323066; // '420f'
#[allow(dead_code)]
pub(crate) const KVPIXEL_FORMAT_BGRA: u32 = 0x42475241; // 'BGRA'

type VTCompressionSessionRef = *mut c_void;
type CVImageBufferRef = *const c_void;

/// `[CMTime](https://developer.apple.com/documentation/coremedia/cmtime)`
///
/// Layout matches the C struct on ARM64 macOS:
/// `{ value: i64, timescale: i32, flags: u32, epoch: i64 }` = 24 bytes.
/// `CMTimeEpoch` is `long long` (i64), NOT i32.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

/// `kCMTIME_IS_VALID(flags)` — true when `flags & kCMTIME_FLAGS_VALID != 0`.
#[allow(dead_code)]
const K_CM_TIME_FLAGS_VALID: u32 = 1;

impl CMTime {
    /// An invalid CMTime (value=0, timescale=0, flags=0, epoch=0).
    const INVALID: CMTime = CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 };

    /// Zero time with timescale=1 (epoch=0, flags=VALID).
    #[allow(dead_code)]
    const ZERO: CMTime = CMTime { value: 0, timescale: 1, flags: K_CM_TIME_FLAGS_VALID, epoch: 0 };
}

extern "C" {
    fn VTCompressionSessionCreate(
        allocator: *const c_void,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: *const c_void,
        source_image_buffer_attributes: *const c_void,
        compressed_data_allocator: *const c_void,
        callback: extern "C" fn(
            *mut c_void,
            *mut c_void,
            i32,
            u32,
            *mut c_void,
        ),
        output_callback_ref_con: *mut c_void,
        session_out: *mut VTCompressionSessionRef,
    ) -> i32;

    fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image_buffer: CVImageBufferRef,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: *const c_void,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> i32;

    fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);

    fn VTSessionSetProperty(
        session: VTCompressionSessionRef,
        key: *const c_void,
        value: *const c_void,
    ) -> i32;

    fn CFStringCreateWithCString(
        allocator: *const c_void,
        c_str: *const u8,
        encoding: u32,
    ) -> *const c_void;

    fn CFRelease(cf: *const c_void);

    fn CMSampleBufferGetDataBuffer(sample_buffer: *const c_void) -> *const c_void;
    #[allow(dead_code)]
    fn CMBlockBufferGetDataLength(block_buffer: *const c_void) -> usize;
    fn CMBlockBufferGetDataPointer(
        block_buffer: *const c_void,
        offset: usize,
        data_length_out: *mut usize,
        total_length_out: *mut usize,
        data_pointer_out: *mut *const u8,
    ) -> i32;

    fn CMSampleBufferGetSampleAttachmentsArray(
        sample_buffer: *const c_void,
        create_if_necessary: bool,
    ) -> *const c_void;

    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFBooleanGetValue(boolean: *const c_void) -> bool;

    fn CFNumberCreate(
        allocator: *const c_void,
        the_type: i32,
        value_ptr: *const c_void,
    ) -> *const c_void;
}

// CoreFoundation boolean constants (global symbols, NOT functions).
// On ARM64 macOS, declaring these as extern fn causes SIGBUS.
extern "C" {
    static kCFBooleanTrue: *const c_void;
    static kCFBooleanFalse: *const c_void;
}

const K_CF_ENCODING_UTF8: u32 = 0x08000100;
const K_CF_NUMBER_SINT32_TYPE: i32 = 3;
const K_CF_NUMBER_SINT64_TYPE: i32 = 4;

/// Maximum time to wait for VideoToolbox to deliver an encoded frame.
// VideoToolbox callbacks are async and can take 100-500ms with real-time config.
// Too short = lost frames, too long = visible lag.
const ENCODE_TIMEOUT: Duration = Duration::from_millis(500);

// ─── CFString key wrapper (Send+Sync safe) ──────────────────────────────────

/// A CoreFoundation string constant wrapped as a raw address (usize).
/// `usize` is `Send + Sync`, unlike `*const c_void`.
#[derive(Clone, Copy)]
struct CfKey(usize);

impl CfKey {
    /// Create a CFString key from a C string. Leaked intentionally — CF singletons.
    ///
    /// # Safety
    /// Must be called from a single thread (or guarded by `OnceLock`).
    unsafe fn new(s: &[u8]) -> Self {
        let ptr = CFStringCreateWithCString(ptr::null(), s.as_ptr(), K_CF_ENCODING_UTF8);
        Self(ptr as usize)
    }

    fn as_ptr(self) -> *const c_void {
        self.0 as *const c_void
    }
}

// SAFETY: The CFString pointer is a CoreFoundation singleton (immutable after creation).
unsafe impl Send for CfKey {}
unsafe impl Sync for CfKey {}

mod vt_keys {
    use super::CfKey;
    use std::sync::OnceLock;

    static REAL_TIME: OnceLock<CfKey> = OnceLock::new();
    static PROFILE_LEVEL: OnceLock<CfKey> = OnceLock::new();
    static AVERAGE_BIT_RATE: OnceLock<CfKey> = OnceLock::new();
    static MAX_KEY_FRAME_INTERVAL: OnceLock<CfKey> = OnceLock::new();
    static ALLOW_FRAME_REORDERING: OnceLock<CfKey> = OnceLock::new();
    static ALLOW_TEMPORAL_COMPRESSION: OnceLock<CfKey> = OnceLock::new();
static EXPECT_REALTIME: OnceLock<CfKey> = OnceLock::new();
    static DEPENDS_ON_OTHERS: OnceLock<CfKey> = OnceLock::new();
    static NOT_SYNC: OnceLock<CfKey> = OnceLock::new();

    pub unsafe fn realtime() -> CfKey {
        *REAL_TIME.get_or_init(|| CfKey::new(b"VTCompressionPropertyKey_RealTime\0"))
    }
    pub unsafe fn profile_level() -> CfKey {
        *PROFILE_LEVEL.get_or_init(|| CfKey::new(b"VTCompressionPropertyKey_ProfileLevel\0"))
    }
    pub unsafe fn average_bit_rate() -> CfKey {
        *AVERAGE_BIT_RATE.get_or_init(|| CfKey::new(b"VTCompressionPropertyKey_AverageBitRate\0"))
    }
    pub unsafe fn max_key_frame_interval() -> CfKey {
        *MAX_KEY_FRAME_INTERVAL.get_or_init(|| CfKey::new(b"VTCompressionPropertyKey_MaxKeyFrameInterval\0"))
    }
    pub unsafe fn allow_frame_reordering() -> CfKey {
        *ALLOW_FRAME_REORDERING.get_or_init(|| CfKey::new(b"VTCompressionPropertyKey_AllowFrameReordering\0"))
    }
    pub unsafe fn allow_temporal_compression() -> CfKey {
        *ALLOW_TEMPORAL_COMPRESSION.get_or_init(|| CfKey::new(b"VTCompressionPropertyKey_AllowTemporalCompression\0"))
    }
    pub unsafe fn expect_realtime() -> CfKey {
        *EXPECT_REALTIME.get_or_init(|| CfKey::new(b"VTCompressionPropertyKey_ExpectRealTime\0"))
    }
    pub unsafe fn depends_on_others() -> CfKey {
        *DEPENDS_ON_OTHERS.get_or_init(|| CfKey::new(b"DependsOnOthers\0"))
    }
    pub unsafe fn not_sync() -> CfKey {
        *NOT_SYNC.get_or_init(|| CfKey::new(b"NotSync\0"))
    }
}

// ─── Callback ───────────────────────────────────────────────────────────────

/// Result of encoding a single frame, sent from the VideoToolbox callback
/// to the `encode()` caller via an `mpsc` channel.
struct EncodeResult {
    data: Vec<u8>,
    is_keyframe: bool,
}

/// VideoToolbox encoding callback.
///
/// # Safety
///
/// `source_frame_ref_con` must be a `Box<mpsc::Sender<EncodeResult>>` that was
/// created by `Box::into_raw()` in `encode()`. This function takes ownership
/// of the Box, so it must not be used after this callback returns.
extern "C" fn encoding_callback(
    _ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: i32,
    info_flags: u32,
    sample_buffer: *mut c_void,
) {
    // Reconstruct the Sender from the raw pointer. This takes ownership and
    // will drop it at the end of this function, so it must not be used after this function returns.
    let tx = unsafe { Box::from_raw(source_frame_ref_con as *mut mpsc::Sender<EncodeResult>) };

    if status != 0 {
        warn!("VideoToolbox encoding callback error: {status}");
        // Sender is dropped here, which closes the channel.
        // The receiver will get a RecvError, which encode() handles.
        return;
    }
    if sample_buffer.is_null() {
        debug!("VideoToolbox callback: no sample buffer (info_flags={info_flags:#x})");
        // No sample buffer — signal "no output" by dropping the Sender.
        // Encode will see RecvError and treat it as "no frame this call".
        return;
    }
    if sample_buffer.is_null() {
        // No sample buffer — signal "no output" by dropping the Sender.
        // Encode will see RecvError and treat it as "no frame this call".
        return;
    }

    let result = unsafe { extract_nal_data(sample_buffer) };
    let Some(EncodeResult { data, is_keyframe }) = result else {
        // Failed to extract data — drop Sender to signal "no output".
        return;
    };

    // Send the result to the encode() caller. If the receiver was dropped
    // (e.g. encoder was dropped while a frame was in-flight), the send
    // fails silently, which is fine.
    let _ = tx.send(EncodeResult { data, is_keyframe });
}

/// Extract NAL unit data and keyframe status from a CMSampleBuffer.
unsafe fn extract_nal_data(sample_buffer: *const c_void) -> Option<EncodeResult> {
    let data_buffer = CMSampleBufferGetDataBuffer(sample_buffer);
    if data_buffer.is_null() {
        return None;
    }

    let mut data_ptr: *const u8 = ptr::null();
    let mut total_length: usize = 0;
    let result = CMBlockBufferGetDataPointer(
        data_buffer,
        0,
        ptr::null_mut(),
        &mut total_length,
        &mut data_ptr,
    );
    if result != 0 || data_ptr.is_null() || total_length == 0 {
        return None;
    }

    let nal_data = std::slice::from_raw_parts(data_ptr, total_length).to_vec();
    let is_keyframe = is_keyframe(sample_buffer);

    Some(EncodeResult {
        data: nal_data,
        is_keyframe,
    })
}

unsafe fn is_keyframe(sample_buffer: *const c_void) -> bool {
    let attachments = CMSampleBufferGetSampleAttachmentsArray(sample_buffer, false);
    if attachments.is_null() {
        return true;
    }
    let dict = CFArrayGetValueAtIndex(attachments, 0);
    if dict.is_null() {
        return true;
    }

    let depends = CFDictionaryGetValue(dict, vt_keys::depends_on_others().as_ptr());
    if !depends.is_null() {
        let val = CFBooleanGetValue(depends);
        return !val;
    }

    let not_sync = CFDictionaryGetValue(dict, vt_keys::not_sync().as_ptr());
    if !not_sync.is_null() {
        let val = CFBooleanGetValue(not_sync);
        return !val;
    }

    true
}

// ─── Public encoder ─────────────────────────────────────────────────────────

pub struct VtH264Encoder {
    session: VTCompressionSessionRef,
    width: u16,
    height: u16,
    frame_id: u32,
    frame_index: i64,
    /// Ensure only one encode operation at a time
    encoding: bool,
}

impl VtH264Encoder {
    /// Create a new H.264 encoder for the given dimensions.
    pub fn new(width: u16, height: u16) -> Result<Self> {
        let mut session: VTCompressionSessionRef = ptr::null_mut();

        let status = unsafe {
            VTCompressionSessionCreate(
                ptr::null(),
                width as i32,
                height as i32,
                K_CM_VIDEO_CODEC_TYPE_H264,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                encoding_callback,
                ptr::null_mut(),
                &mut session,
            )
        };

        if status != 0 {
            return Err(anyhow!("VTCompressionSessionCreate failed: {status}"));
        }
        if session.is_null() {
            return Err(anyhow!("VTCompressionSessionCreate returned null session"));
        }

        unsafe {
            let _ = VTSessionSetProperty(session, vt_keys::realtime().as_ptr(), kCFBooleanTrue);
            let _ = VTSessionSetProperty(session, vt_keys::allow_frame_reordering().as_ptr(), kCFBooleanFalse);
            // Disable temporal compression for real-time screen sharing - each frame is unique
            let _ = VTSessionSetProperty(session, vt_keys::allow_temporal_compression().as_ptr(), kCFBooleanFalse);

            // Force encoder to produce output immediately (no buffering)
            let _ = VTSessionSetProperty(session, vt_keys::expect_realtime().as_ptr(), kCFBooleanTrue);

            // H.264 Baseline profile
            let profile: i32 = 0x1E;
            let num = CFNumberCreate(ptr::null(), K_CF_NUMBER_SINT32_TYPE, &profile as *const i32 as *const c_void);
            let _ = VTSessionSetProperty(session, vt_keys::profile_level().as_ptr(), num);
            CFRelease(num);

            // 4 Mbps average bitrate
            let bitrate: i64 = 4_000_000;
            let num = CFNumberCreate(ptr::null(), K_CF_NUMBER_SINT64_TYPE, &bitrate as *const i64 as *const c_void);
            let _ = VTSessionSetProperty(session, vt_keys::average_bit_rate().as_ptr(), num);
            CFRelease(num);

            // Keyframe every ~2 seconds at 30fps
            let key_interval: i32 = 60;
            let num = CFNumberCreate(ptr::null(), K_CF_NUMBER_SINT32_TYPE, &key_interval as *const i32 as *const c_void);
            let _ = VTSessionSetProperty(session, vt_keys::max_key_frame_interval().as_ptr(), num);
            CFRelease(num);
        }

        debug!("VideoToolbox H.264 encoder created: {width}×{height}");

        Ok(Self {
            session,
            width,
            height,
            frame_id: 0,
            frame_index: 0,
            encoding: false,
        })
    }

    /// Encode a single frame from a `CVPixelBuffer` pointer.
    ///
    /// The callback fires asynchronously on a VideoToolbox thread. We use an
    /// `mpsc` channel to synchronize: `encode()` blocks on `rx.recv_timeout()`
    /// until the callback delivers the encoded data (or times out).
    ///
    /// # Memory safety
    ///
    /// The `mpsc::Sender` is heap-allocated and passed to VideoToolbox as a
    /// raw pointer (`Box::into_raw`). The callback reclaims it with
    /// `Box::from_raw`, taking ownership. This guarantees no dangling pointers:
    /// if `VTCompressionSessionEncodeFrame` returns an error, we reclaim the
    /// Box ourselves; otherwise, the callback always reclaims it.
    pub fn encode(&mut self, pixel_buffer_ptr: *const c_void) -> Result<Option<H264Frame>> {
        // Prevent concurrent encode calls - VideoToolbox session is not thread-safe
        if self.encoding {
            warn!("Encode called while already encoding, skipping frame");
            return Ok(None);
        }
        self.encoding = true;

        let (tx, rx) = mpsc::channel::<EncodeResult>();
        let tx_ptr = Box::into_raw(Box::new(tx)) as *mut c_void;

        // Let VideoToolbox assign timestamps automatically
        // Manual timestamps can cause buffering issues with real-time content
        debug!("Encoding frame {}", self.frame_index);
        self.frame_index += 1;

        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session,
                pixel_buffer_ptr,
                CMTime::INVALID, // let VideoToolbox assign timestamp
                CMTime::INVALID, // no duration specified
                ptr::null(),
                tx_ptr, // sourceFrameRefCon — reclaimed by callback
                ptr::null_mut(),
            )
        };

        if status != 0 {
            // VTCompressionSessionEncodeFrame failed — reclaim the Box to
            // avoid leaking memory. The callback will NOT fire.
            self.encoding = false;
            unsafe { let _ = Box::from_raw(tx_ptr as *mut mpsc::Sender<EncodeResult>); }
            warn!("VTCompressionSessionEncodeFrame failed: {status}");
            return Err(anyhow!("VTCompressionSessionEncodeFrame failed: {status}"));
        }

        // Wait for the callback to deliver the encoded frame.
        // VideoToolbox typically calls the callback on a separate thread,
        // so this recv_timeout ensures we don't block forever.
        debug!("Waiting for encode callback for frame {}", self.frame_index);
        let result = rx.recv_timeout(ENCODE_TIMEOUT);
        
        // Mark encoding as complete regardless of result
        self.encoding = false;
        match result {
            Ok(EncodeResult { data, is_keyframe }) => {
                if data.is_empty() {
                    return Ok(None);
                }
                let frame_id = self.frame_id;
                self.frame_id += 1;
                debug!(
                    "H.264 frame {}: {} bytes, {}",
                    frame_id,
                    data.len(),
                    if is_keyframe { "keyframe" } else { "P-frame" }
                );
                Ok(Some(H264Frame {
                    frame_id,
                    data,
                    is_keyframe,
                    width: self.width,
                    height: self.height,
                }))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                debug!("VideoToolbox encoding callback timed out after {ENCODE_TIMEOUT:?} (frame skipped)");
                // Reset encoding flag to allow next frame
                self.encoding = false;
                Ok(None)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Channel disconnected — the callback saw an error or null buffer,
                // meaning no output for this frame (e.g. VideoToolbox decided
                // not to produce output for this frame). This can happen for
                // "skip" frames or when the encoder decides to drop.
                debug!("VideoToolbox produced no output for this frame (disconnected)");
                // Reset encoding flag to allow next frame
                self.encoding = false;
                Ok(None)
            }
        }
    }

    #[allow(dead_code)]
    pub fn width(&self) -> u16 { self.width }
    #[allow(dead_code)]
    pub fn height(&self) -> u16 { self.height }
}

impl Drop for VtH264Encoder {
    fn drop(&mut self) {
        if !self.session.is_null() {
            unsafe {
                VTCompressionSessionInvalidate(self.session);
            }
            debug!("VideoToolbox H.264 encoder destroyed");
        }
    }
}

// SAFETY: The VideoToolbox session is safe to send across threads.
// The encode() method uses mpsc::channel for thread-safe callback sync.
unsafe impl Send for VtH264Encoder {}
unsafe impl Sync for VtH264Encoder {}

// ─── Encoded frame output ───────────────────────────────────────────────────

#[derive(Debug)]
#[allow(dead_code)]
pub struct H264Frame {
    pub frame_id: u32,
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    pub width: u16,
    pub height: u16,
}