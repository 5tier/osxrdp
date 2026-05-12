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
use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_pdu::PduResult;
use ironrdp_pdu::rdp::vc::dvc::gfx::{
    Avc420BitmapStream, CapabilitiesAdvertisePdu, CapabilitiesConfirmPdu, CapabilitiesV81Flags,
    CapabilitiesV8Flags, CapabilitySet, Codec1Type, CreateSurfacePdu, EndFramePdu, FrameAcknowledgePdu,
    MapSurfaceToOutputPdu, PixelFormat, QuantQuality, ResetGraphicsPdu, ServerPdu, StartFramePdu,
    Timestamp, WireToSurface1Pdu,
};
use ironrdp_pdu::rdp::vc::dvc::gfx::ClientPdu;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

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

    /// Check if the client advertised AVC420 support.
    pub fn client_supports_avc420(&self) -> bool {
        self.client_caps
            .as_ref()
            .is_some_and(|caps| caps.0.iter().any(|cs| {
                matches!(cs, CapabilitySet::V8_1 { flags } if flags.contains(CapabilitiesV81Flags::AVC420_ENABLED))
                    || matches!(cs, CapabilitySet::V10_2 { .. })
                    || matches!(cs, CapabilitySet::V10_3 { .. })
                    || matches!(cs, CapabilitySet::V10_4 { .. })
                    || matches!(cs, CapabilitySet::V10_5 { .. })
                    || matches!(cs, CapabilitySet::V10_6 { .. })
                    || matches!(cs, CapabilitySet::V10_6Err { .. })
                    || matches!(cs, CapabilitySet::V10_7 { .. })
            }))
    }

    /// Whether the GFX pipeline is active and ready to send frames.
    pub fn is_active(&self) -> bool {
        self.state == GfxState::Active
    }

    fn handle_capabilities_advertise(&mut self, caps: CapabilitiesAdvertisePdu) {
        debug!("GFX: client capabilities: {:?}", caps.0);
        self.client_caps = Some(caps.clone());

        // Pick the highest version the client supports that we also support.
        let confirmed = if let Some(best) = caps.0.iter().find(|cs| {
            matches!(cs, CapabilitySet::V10_7 { .. })
        }) {
            best.clone()
        } else if let Some(best) = caps.0.iter().find(|cs| {
            matches!(cs, CapabilitySet::V10_4 { .. } | CapabilitySet::V10_5 { .. } | CapabilitySet::V10_6 { .. } | CapabilitySet::V10_6Err { .. })
        }) {
            best.clone()
        } else if let Some(best) = caps.0.iter().find(|cs| {
            matches!(cs, CapabilitySet::V8_1 { .. })
        }) {
            best.clone()
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
    }

    fn init_surface(&mut self) {
        // Reset graphics
        self.pending.push_back(wrap_pdu(ServerPdu::ResetGraphics(ResetGraphicsPdu {
            width: self.desktop_width as u32,
            height: self.desktop_height as u32,
            monitors: vec![],
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

    /// Update the desktop size (for deactivation-reactivation).
    pub fn set_desktop_size(&mut self, width: u16, height: u16) {
        self.desktop_width = width;
        self.desktop_height = height;
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

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        debug!("GFX DVC channel started");
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