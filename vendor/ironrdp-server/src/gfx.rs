//! RDPGFX dynamic virtual channel server processor.
//!
//! Implements the [MS-RDPEGFX] graphics pipeline for sending H.264 AVC420
//! encoded frames from the server to the client.
//!
//! # Protocol flow
//!
//! 1. Client advertises capabilities → Server confirms
//! 2. Server sends `ResetGraphics` + `CreateSurface` + `MapSurfaceToOutput`
//! 3. Per frame: `StartFrame` → `WireToSurface1(AVC420)` → `EndFrame`
//! 4. Client acknowledges with `FrameAcknowledge`

use ironrdp_core::{decode, encode_vec, impl_as_any};
use ironrdp_dvc::{DvcEncode, DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_pdu::PduResult;
use ironrdp_pdu::rdp::vc::dvc::gfx::{
    Avc420BitmapStream, CapabilitiesAdvertisePdu, CapabilitiesConfirmPdu, CapabilitiesV81Flags,
    CapabilitiesV8Flags, CapabilitiesV10Flags, CapabilitiesV103Flags,
    CapabilitySet, Codec1Type, CreateSurfacePdu, EndFramePdu, FrameAcknowledgePdu,
    MapSurfaceToOutputPdu, PixelFormat, QuantQuality, ResetGraphicsPdu, ServerPdu, StartFramePdu,
    Timestamp, WireToSurface1Pdu,
};
use ironrdp_pdu::rdp::vc::dvc::gfx::ClientPdu;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

// ─── GFX channel name ──────────────────────────────────────────────────────

pub const GFX_CHANNEL_NAME: &str = "Microsoft::Windows::RDS::Graphics";

// ─── Server-side GFX state machine ─────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GfxState {
    /// Waiting for client capabilities.
    WaitingCapabilities,
    /// Capabilities exchanged; surface not yet created.
    Ready,
    /// Surface created and mapped; ready to send frames.
    Active,
}

/// The `RDPGFX` DVC channel processor.
///
/// Shared via `Arc<Mutex<>>` so the display update loop can enqueue frames
/// while the DVC channel processes client PDUs.
pub struct GfxServer {
    state: GfxState,
    /// Capabilities the client advertised.
    client_caps: Option<CapabilitiesAdvertisePdu>,
    /// The GFX surface ID we allocate (currently single-surface).
    surface_id: u16,
    /// Current frame ID counter.
    frame_id: u32,
    /// Queue of PDUs to send to the client (populated by both
    /// client-PDU responses and proactive frame sends).
    pending: VecDeque<DvcMessage>,
    /// Desktop size for surface creation.
    desktop_width: u16,
    desktop_height: u16,
    /// The DVC channel ID assigned by drdynvc when the channel starts.
    channel_id: Option<u32>,
    /// The drdynvc static channel ID (MCS channel ID).
    drdynvc_channel_id: Option<u16>,
    /// Whether the server needs to send a keyframe before P-frames.
    /// Set to true on init_surface (new surface = new keyframe needed).
    needs_keyframe: bool,
    /// Shared flag: true when the client supports AVC420.
    /// Set to false during capability negotiation when the client
    /// advertises AVC_DISABLED. The display module reads this flag
    /// to fall back to BGRA bitmap updates when H.264 is not supported.
    avc420_enabled: Arc<AtomicBool>,
}

impl std::fmt::Debug for GfxServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GfxServer")
            .field("state", &self.state)
            .field("surface_id", &self.surface_id)
            .field("frame_id", &self.frame_id)
            .finish()
    }
}

impl GfxServer {
    pub fn new(desktop_width: u16, desktop_height: u16) -> Self {
        Self {
            state: GfxState::WaitingCapabilities,
            client_caps: None,
            surface_id: 1,
            frame_id: 0,
            pending: VecDeque::new(),
            desktop_width,
            desktop_height,
            channel_id: None,
            drdynvc_channel_id: None,
            needs_keyframe: true,
            avc420_enabled: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Convenience: wrap in `Arc<Mutex<>>` for sharing.
    pub fn shared(desktop_width: u16, desktop_height: u16) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::new(desktop_width, desktop_height)))
    }

    /// Enqueue a `WireToSurface1Pdu` with AVC420 data for a single frame region.
    pub fn send_avc420_frame(
        &mut self,
        h264_data: Vec<u8>,
        width: u16,
        height: u16,
        quant_parameter: u8,
        is_keyframe: bool,
    ) {
        if self.state != GfxState::Active {
            warn!("GfxServer::send_avc420_frame called but state is {:?}", self.state);
            return;
        }

        let frame_id = self.frame_id;
        self.frame_id += 1;

        // Start frame
        let now = std::time::SystemTime::now();
        let duration = now
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let ms = duration.as_millis() as u64;
        let timestamp = Timestamp {
            milliseconds: (ms % 1000) as u16,
            seconds: ((ms / 1000) % 60) as u8,
            minutes: ((ms / 60_000) % 60) as u8,
            hours: ((ms / 3_600_000) % 24) as u16,
        };

        self.pending.push_back(wrap_pdu(ServerPdu::StartFrame(StartFramePdu {
            timestamp,
            frame_id,
        })));

        // Build the Avc420BitmapStream (single full-frame rectangle)
        let destination_rect = InclusiveRectangle {
            left: 0,
            top: 0,
            right: width.saturating_sub(1),
            bottom: height.saturating_sub(1),
        };

        let quality = if is_keyframe { quant_parameter } else { quant_parameter.saturating_add(4).min(51) };

        let avc420_stream = Avc420BitmapStream {
            rectangles: vec![destination_rect],
            quant_qual_vals: vec![QuantQuality {
                quantization_parameter: quant_parameter,
                progressive: false,
                quality,
            }],
            data: &h264_data,
        };

        // Encode Avc420BitmapStream → bytes for WireToSurface1Pdu.bitmap_data
        let bitmap_data = match encode_vec(&avc420_stream) {
            Ok(data) => data,
            Err(e) => {
                warn!("Failed to encode Avc420BitmapStream: {e}");
                return;
            }
        };

        let wire_pdu = WireToSurface1Pdu {
            surface_id: self.surface_id,
            codec_id: Codec1Type::Avc420,
            pixel_format: PixelFormat::XRgb,
            destination_rectangle: InclusiveRectangle {
                left: 0,
                top: 0,
                right: width.saturating_sub(1),
                bottom: height.saturating_sub(1),
            },
            bitmap_data,
        };

        self.pending.push_back(wrap_pdu(ServerPdu::WireToSurface1(wire_pdu)));

        // End frame
        self.pending.push_back(wrap_pdu(ServerPdu::EndFrame(EndFramePdu {
            frame_id,
        })));
    }

    /// Check if the client advertised AVC420 support **on a version the
    /// server can confirm**.
    ///
    /// The server always confirms V8.1 because its `ResetGraphicsPdu`
    /// only encodes the simple V8-format Monitor entries (no
    /// `desktopSurfaceId`/`surfaceId` fields required by V10.4+).
    /// Therefore we can only use AVC420 if the client has V8.1 with
    /// `AVC420_ENABLED`, or V10/V10.2/V10.3 without `AVC_DISABLED`
    /// (these versions also use the simple monitor format).
    ///
    /// V10.4+ without `AVC_DISABLED` is **not** sufficient because
    /// confirming V10.4 requires the extended monitor format.
    pub fn client_supports_avc420(&self) -> bool {
        self.client_caps
            .as_ref()
            .is_some_and(|caps| caps.0.iter().any(|cs| {
                match cs {
                    // V8.1: explicit AVC420_ENABLED flag
                    CapabilitySet::V8_1 { flags } => flags.contains(CapabilitiesV81Flags::AVC420_ENABLED),
                    // V10 / V10.2: CapabilitiesV10Flags — AVC420 implied if AVC_DISABLED is NOT set
                    // (these versions use the simple V8-format monitor entries)
                    CapabilitySet::V10 { flags }
                    | CapabilitySet::V10_2 { flags } => !flags.contains(CapabilitiesV10Flags::AVC_DISABLED),
                    // V10.3: CapabilitiesV103Flags (also simple monitor format)
                    CapabilitySet::V10_3 { flags } => !flags.contains(CapabilitiesV103Flags::AVC_DISABLED),
                    // V10.4+ requires extended monitor format — server can't confirm these,
                    // so we don't consider them as AVC420 support.
                    _ => false,
                }
            }))
    }

    /// Set the shared AVC420-enabled flag.
    ///
    /// The display module holds a clone of the same `Arc` and checks it
    /// each frame. When the client advertises `AVC_DISABLED`, this flag
    /// is set to `false`, causing the display to fall back to BGRA.
    pub fn set_avc420_flag(&mut self, flag: Arc<AtomicBool>) {
        self.avc420_enabled = flag;
    }

    /// Get a clone of the shared AVC420-enabled flag.
    pub fn avc420_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.avc420_enabled)
    }

    /// Whether the client actually supports AVC420 (read from the shared flag).
    pub fn avc420_enabled(&self) -> bool {
        self.avc420_enabled.load(Ordering::Relaxed)
    }

    /// Whether the GFX pipeline is active and ready to send frames.
    pub fn is_active(&self) -> bool {
        self.state == GfxState::Active
    }

    fn handle_capabilities_advertise(&mut self, caps: CapabilitiesAdvertisePdu) {
        debug!("GFX: client capabilities: {:?}", caps.0);
        self.client_caps = Some(caps.clone());

        // Check whether the client actually supports AVC420.
        // Some clients (e.g. Microsoft Remote Desktop on macOS) advertise
        // V10/V10.2/V10.3 with AVC_DISABLED, meaning they explicitly
        // opt out of H.264 even though they support the GFX pipeline.
        let avc_ok = self.client_supports_avc420();
        self.avc420_enabled.store(avc_ok, Ordering::Relaxed);
        if !avc_ok {
            info!(
                "GFX: client does not support AVC420 (AVC_DISABLED), \
                 falling back to BGRA bitmap updates"
            );
            // Don't confirm capabilities or init the surface.
            // Sending GFX init PDUs (ResetGraphics/CreateSurface/MapSurfaceToOutput)
            // to a client that doesn't want AVC420 causes the client to close
            // the GFX channel and sometimes disconnect entirely.
            //
            // Instead, leave the GFX channel in WaitingCapabilities state.
            // The display module detects avc420_enabled=false and falls back
            // to BGRA bitmap updates via the normal SurfaceCommand path.
            //
            // We still return the pending PDUs (empty) so the caller knows
            // we processed the capabilities.
            return;
        }

        // Confirm the best matching capability version.
        // We prefer V8.1 because our Monitor struct in ResetGraphicsPdu
        // only encodes V8-format entries (no desktopSurfaceId/surfaceId
        // fields needed by V10.4+).
        let confirmed = if let Some(_best) = caps.0.iter().find(|cs| {
            matches!(cs, CapabilitySet::V8_1 { .. })
        }) {
            CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::SMALL_CACHE | CapabilitiesV81Flags::AVC420_ENABLED,
            }
        } else if let Some(best) = caps.0.iter().find(|cs| {
            matches!(cs, CapabilitySet::V8 { .. })
        }) {
            best.clone()
        } else {
            CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::empty(),
            }
        };

        self.pending
            .push_back(wrap_pdu(ServerPdu::CapabilitiesConfirm(CapabilitiesConfirmPdu(
                confirmed,
            ))));

        self.state = GfxState::Ready;
        self.init_surface();

        // Log the PDUs we're sending for debugging
        for pdu in &self.pending {
            let size = pdu.size();
            let mut buf = vec![0u8; size];
            use ironrdp_pdu::cursor::WriteCursor;
            if let Err(e) = pdu.encode(&mut WriteCursor::new(&mut buf)) {
                warn!("Failed to encode init PDU: {e}");
            } else {
                debug!("Init PDU bytes ({} bytes): {:02x?}", buf.len(), &buf[..buf.len().min(64)]);
            }
        }
    }

    pub fn init_surface(&mut self) {
        // Reset graphics
        self.needs_keyframe = true; // New surface = need keyframe
        self.pending.push_back(wrap_pdu(ServerPdu::ResetGraphics(ResetGraphicsPdu {
            width: self.desktop_width as u32,
            height: self.desktop_height as u32,
            monitors: vec![Monitor {
                left: 0,
                top: 0,
                right: self.desktop_width as i32 - 1,
                bottom: self.desktop_height as i32 - 1,
                flags: MonitorFlags::PRIMARY,
            }],
        })));

        // Create a single surface
        self.pending.push_back(wrap_pdu(ServerPdu::CreateSurface(CreateSurfacePdu {
            surface_id: self.surface_id,
            width: self.desktop_width,
            height: self.desktop_height,
            pixel_format: PixelFormat::XRgb,
        })));

        // Map surface to output at origin
        self.pending.push_back(wrap_pdu(ServerPdu::MapSurfaceToOutput(
            MapSurfaceToOutputPdu {
                surface_id: self.surface_id,
                output_origin_x: 0,
                output_origin_y: 0,
            },
        )));

        self.state = GfxState::Active;
        debug!(
            "GFX surface initialized: {}×{}",
            self.desktop_width, self.desktop_height
        );
    }

    fn handle_frame_acknowledge(&mut self, ack: FrameAcknowledgePdu) {
        debug!("GFX: frame acknowledge: id={}, depth={:?}", ack.frame_id, ack.queue_depth);
    }

    /// Update the desktop size (for deactivation-reactivation or
    /// resizing when the client requests a different resolution).
    pub fn set_desktop_size(&mut self, width: u16, height: u16) {
        self.desktop_width = width;
        self.desktop_height = height;
    }

    /// Returns the current desktop width configured for the GFX surface.
    pub fn desktop_width(&self) -> u16 {
        self.desktop_width
    }

    /// Returns the current desktop height configured for the GFX surface.
    pub fn desktop_height(&self) -> u16 {
        self.desktop_height
    }

    /// Returns the DVC channel ID assigned by drdynvc, or None if not yet started.
    pub fn channel_id(&self) -> Option<u32> {
        self.channel_id
    }

    /// Set the drdynvc static channel ID (MCS channel ID) for outbound PDU encoding.
    pub fn set_drdynvc_channel_id(&mut self, id: u16) {
        self.drdynvc_channel_id = Some(id);
    }

    /// Returns the drdynvc static channel ID (MCS channel ID).
    pub fn drdynvc_channel_id(&self) -> Option<u16> {
        self.drdynvc_channel_id
    }

    /// Whether a keyframe is needed before sending P-frames.
    /// Returns true after init_surface() until a keyframe is sent.
    pub fn needs_keyframe(&self) -> bool {
        self.needs_keyframe
    }

    /// Mark that a keyframe has been sent (no longer needs one).
    pub fn clear_needs_keyframe(&mut self) {
        self.needs_keyframe = false;
    }

    /// Drain all pending server PDUs.
    pub fn drain_pending(&mut self) -> Vec<DvcMessage> {
        self.pending.drain(..).collect()
    }
}

impl_as_any!(GfxServer);

impl DvcProcessor for GfxServer {
    fn channel_name(&self) -> &str {
        GFX_CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        debug!("GFX DVC channel started, id={channel_id}");
        self.state = GfxState::WaitingCapabilities;
        self.channel_id = Some(channel_id);
        self.client_caps = None;
        self.pending.clear();
        // We don't send anything until the client advertises capabilities.
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        debug!(len = payload.len(), "GFX: processing client PDU");
        let pdu = match decode::<ClientPdu>(payload) {
            Ok(pdu) => pdu,
            Err(e) => {
                warn!("GFX: failed to decode client PDU: {e}");
                return Ok(Vec::new());
            }
        };

        match pdu {
            ClientPdu::CapabilitiesAdvertise(caps) => {
                self.handle_capabilities_advertise(caps);
            }
            ClientPdu::FrameAcknowledge(ack) => {
                self.handle_frame_acknowledge(ack);
            }
        }

        // Drain any pending messages
        Ok(self.drain_pending())
    }

    fn close(&mut self, _channel_id: u32) {
        debug!("GFX DVC channel closed");
        self.state = GfxState::WaitingCapabilities;
        self.channel_id = None;
    }
}

// Implement DvcEncode for ServerPdu via newtype (orphan rule)
struct GfxServerPdu(ServerPdu);

impl ironrdp_core::Encode for GfxServerPdu {
    fn encode(&self, dst: &mut ironrdp_core::WriteCursor<'_>) -> ironrdp_core::EncodeResult<()> {
        self.0.encode(dst)
    }
    fn name(&self) -> &'static str { self.0.name() }
    fn size(&self) -> usize { self.0.size() }
}

impl DvcEncode for GfxServerPdu {}

// Helper to wrap server PDUs into DvcMessages
fn wrap_pdu(pdu: ServerPdu) -> DvcMessage {
    Box::new(GfxServerPdu(pdu))
}

// ─── Arc<Mutex> wrapper for sharing GfxServer with DrdynvcServer ────────────

/// Wrapper that allows `GfxServer` to be registered as a DVC channel
/// while remaining accessible via `Arc<Mutex<GfxServer>>` from the display loop.
pub struct SharedGfxServer {
    inner: Arc<Mutex<GfxServer>>,
}

impl SharedGfxServer {
    pub fn new(inner: Arc<Mutex<GfxServer>>) -> Self {
        Self { inner }
    }
}

impl std::fmt::Debug for SharedGfxServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedGfxServer").finish()
    }
}

impl_as_any!(SharedGfxServer);

impl DvcProcessor for SharedGfxServer {
    fn channel_name(&self) -> &str {
        GFX_CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.inner.lock().unwrap().start(channel_id)
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        self.inner.lock().unwrap().process(channel_id, payload)
    }

    fn close(&mut self, channel_id: u32) {
        self.inner.lock().unwrap().close(channel_id)
    }
}

impl DvcServerProcessor for SharedGfxServer {}