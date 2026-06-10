use core::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _, Result};
use ironrdp_acceptor::{Acceptor, AcceptorResult, BeginResult, DesktopSize};
use ironrdp_async::Framed;
use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_cliprdr::CliprdrServer;
use ironrdp_core::{decode, encode_vec, impl_as_any};
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_displaycontrol::server::{DisplayControlHandler, DisplayControlServer};
use ironrdp_pdu::input::fast_path::{FastPathInput, FastPathInputEvent};
use ironrdp_pdu::input::InputEventPdu;
use ironrdp_pdu::mcs::{SendDataIndication, SendDataRequest};
use ironrdp_pdu::rdp::capability_sets::{BitmapCodecs, CapabilitySet, CmdFlags, CodecProperty, GeneralExtraFlags};
pub use ironrdp_pdu::rdp::client_info::Credentials;
use ironrdp_pdu::rdp::headers::{ServerDeactivateAll, ShareControlPdu};
use ironrdp_pdu::x224::X224;
use ironrdp_pdu::{decode_err, mcs, nego, rdp, Action, PduResult};
use ironrdp_svc::{server_encode_svc_messages, StaticChannelId, StaticChannelSet, SvcProcessor};
use ironrdp_tokio::{split_tokio_framed, unsplit_tokio_framed, FramedRead, FramedWrite, TokioFramed};
use rdpsnd::server::{RdpsndServer, RdpsndServerMessage};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// Set TCP keepalive on a TCP stream so the server detects client disconnects.
/// Without keepalive, a dropped RDP client can leave the server running
/// indefinitely at the switched display resolution.
fn set_tcp_keepalive(stream: &TcpStream) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let on: libc::c_int = 1;
    let idle: libc::c_int = 15; // seconds before first probe
    let interval: libc::c_int = 5; // seconds between probes
    let count: libc::c_int = 3; // number of failed probes before disconnect

    unsafe {
        // SO_KEEPALIVE
        let ret = libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, &on as *const _ as *const libc::c_void, size_of_val(&on) as libc::socklen_t);
        if ret < 0 { return Err(std::io::Error::last_os_error()); }

        // TCP_KEEPALIVE (macOS) / TCP_KEEPIDLE (Linux)
        #[cfg(target_os = "macos")]
        let idle_opt = libc::TCP_KEEPALIVE;
        #[cfg(not(target_os = "macos"))]
        let idle_opt = libc::TCP_KEEPIDLE;
        let ret = libc::setsockopt(fd, libc::IPPROTO_TCP, idle_opt, &idle as *const _ as *const libc::c_void, size_of_val(&idle) as libc::socklen_t);
        if ret < 0 { return Err(std::io::Error::last_os_error()); }

        // TCP_KEEPINTVL
        let ret = libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, &interval as *const _ as *const libc::c_void, size_of_val(&interval) as libc::socklen_t);
        if ret < 0 { return Err(std::io::Error::last_os_error()); }

        // TCP_KEEPCNT
        let ret = libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT, &count as *const _ as *const libc::c_void, size_of_val(&count) as libc::socklen_t);
        if ret < 0 { return Err(std::io::Error::last_os_error()); }
    }

    Ok(())
}
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, trace, warn};
use {ironrdp_dvc as dvc, ironrdp_rdpsnd as rdpsnd};

use crate::clipboard::CliprdrServerFactory;
use crate::display::{DisplayUpdate, RdpServerDisplay};
use crate::encoder::{UpdateEncoder, UpdateEncoderCodecs};
use crate::handler::RdpServerInputHandler;
use crate::{builder, capabilities, SoundServerFactory};

#[derive(Clone)]
pub struct RdpServerOptions {
    pub addr: SocketAddr,
    pub security: RdpServerSecurity,
    pub codecs: BitmapCodecs,
}

impl RdpServerOptions {
    fn has_image_remote_fx(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::ImageRemoteFx(_)))
    }

    fn has_remote_fx(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::RemoteFx(_)))
    }

    #[cfg(feature = "qoi")]
    fn has_qoi(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::Qoi))
    }

    #[cfg(feature = "qoiz")]
    fn has_qoiz(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::QoiZ))
    }
}

#[derive(Clone)]
pub enum RdpServerSecurity {
    None,
    Tls(TlsAcceptor),
    /// Used for both hybrid + hybrid-ex.
    Hybrid((TlsAcceptor, Vec<u8>)),
}

impl RdpServerSecurity {
    pub fn flag(&self) -> nego::SecurityProtocol {
        match self {
            RdpServerSecurity::None => nego::SecurityProtocol::empty(),
            RdpServerSecurity::Tls(_) => nego::SecurityProtocol::SSL,
            RdpServerSecurity::Hybrid(_) => nego::SecurityProtocol::HYBRID | nego::SecurityProtocol::HYBRID_EX,
        }
    }
}

struct AInputHandler {
    handler: Arc<Mutex<Box<dyn RdpServerInputHandler>>>,
}

impl_as_any!(AInputHandler);

impl dvc::DvcProcessor for AInputHandler {
    fn channel_name(&self) -> &str {
        ironrdp_ainput::CHANNEL_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<dvc::DvcMessage>> {
        use ironrdp_ainput::{ServerPdu, VersionPdu};

        let pdu = ServerPdu::Version(VersionPdu::default());

        Ok(vec![Box::new(pdu)])
    }

    fn close(&mut self, _channel_id: u32) {}

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<dvc::DvcMessage>> {
        use ironrdp_ainput::ClientPdu;

        match decode(payload).map_err(|e| decode_err!(e))? {
            ClientPdu::Mouse(pdu) => {
                let handler = Arc::clone(&self.handler);
                task::spawn_blocking(move || {
                    handler.blocking_lock().mouse(pdu.into());
                });
            }
        }

        Ok(Vec::new())
    }
}

impl dvc::DvcServerProcessor for AInputHandler {}

struct DisplayControlBackend {
    display: Arc<Mutex<Box<dyn RdpServerDisplay>>>,
}

impl DisplayControlBackend {
    fn new(display: Arc<Mutex<Box<dyn RdpServerDisplay>>>) -> Self {
        Self { display }
    }
}

impl DisplayControlHandler for DisplayControlBackend {
    fn monitor_layout(&self, layout: DisplayControlMonitorLayout) {
        let display = Arc::clone(&self.display);
        task::spawn_blocking(move || display.blocking_lock().request_layout(layout));
    }
}

/// RDP Server
///
/// A server is created to listen for connections.
/// After the connection sequence is finalized using the provided security mechanism, the server can:
///  - receive display updates from a [`RdpServerDisplay`] and forward them to the client
///  - receive input events from a client and forward them to an [`RdpServerInputHandler`]
///
/// # Example
///
/// ```
/// use ironrdp_server::{RdpServer, RdpServerInputHandler, RdpServerDisplay, RdpServerDisplayUpdates};
///
///# use anyhow::Result;
///# use ironrdp_server::{DisplayUpdate, DesktopSize, KeyboardEvent, MouseEvent};
///# use tokio_rustls::TlsAcceptor;
///# struct NoopInputHandler;
///# impl RdpServerInputHandler for NoopInputHandler {
///#     fn keyboard(&mut self, _: KeyboardEvent) {}
///#     fn mouse(&mut self, _: MouseEvent) {}
///# }
///# struct NoopDisplay;
///# #[async_trait::async_trait]
///# impl RdpServerDisplay for NoopDisplay {
///#     async fn size(&mut self) -> DesktopSize {
///#         todo!()
///#     }
///#     async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
///#         todo!()
///#     }
///# }
///# async fn stub() -> Result<()> {
/// fn make_tls_acceptor() -> TlsAcceptor {
///    /* snip */
///#    todo!()
/// }
///
/// fn make_input_handler() -> impl RdpServerInputHandler {
///    /* snip */
///#    NoopInputHandler
/// }
///
/// fn make_display_handler() -> impl RdpServerDisplay {
///    /* snip */
///#    NoopDisplay
/// }
///
/// let tls_acceptor = make_tls_acceptor();
/// let input_handler = make_input_handler();
/// let display_handler = make_display_handler();
///
/// let mut server = RdpServer::builder()
///     .with_addr(([127, 0, 0, 1], 3389))
///     .with_tls(tls_acceptor)
///     .with_input_handler(input_handler)
///     .with_display_handler(display_handler)
///     .build();
///
/// server.run().await;
/// Ok(())
///# }
/// ```
pub struct RdpServer {
    opts: RdpServerOptions,
    // FIXME: replace with a channel and poll/process the handler?
    handler: Arc<Mutex<Box<dyn RdpServerInputHandler>>>,
    display: Arc<Mutex<Box<dyn RdpServerDisplay>>>,
    gfx_server: Option<Arc<std::sync::Mutex<crate::gfx::GfxServer>>>,
    /// Shared flag to force the H.264 encoder to produce a keyframe.
    /// Set by the GFX pipeline when GfxServer::needs_keyframe() is true,
    /// checked by MacDisplayUpdates before each encode call.
    force_keyframe: Option<Arc<std::sync::atomic::AtomicBool>>,
    static_channels: StaticChannelSet,
    sound_factory: Option<Box<dyn SoundServerFactory>>,
    cliprdr_factory: Option<Box<dyn CliprdrServerFactory>>,
    ev_sender: mpsc::UnboundedSender<ServerEvent>,
    ev_receiver: Arc<Mutex<mpsc::UnboundedReceiver<ServerEvent>>>,
    creds: Option<Credentials>,
    authenticate: Option<Arc<dyn Fn(&str, &str) -> bool + Send + Sync>>,
    local_addr: Option<SocketAddr>,
}

#[derive(Debug)]
pub enum ServerEvent {
    Quit(String),
    Clipboard(ClipboardMessage),
    Rdpsnd(RdpsndServerMessage),
    SetCredentials(Credentials),
    GetLocalAddr(oneshot::Sender<Option<SocketAddr>>),
}

pub trait ServerEventSender {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>);
}

impl ServerEvent {
    pub fn create_channel() -> (mpsc::UnboundedSender<Self>, mpsc::UnboundedReceiver<Self>) {
        mpsc::unbounded_channel()
    }
}

#[derive(Debug, PartialEq)]
enum RunState {
    Continue,
    Disconnect,
    DeactivationReactivation { desktop_size: DesktopSize },
}

impl RdpServer {
    pub fn new(
        opts: RdpServerOptions,
        handler: Box<dyn RdpServerInputHandler>,
        display: Box<dyn RdpServerDisplay>,
        mut sound_factory: Option<Box<dyn SoundServerFactory>>,
        mut cliprdr_factory: Option<Box<dyn CliprdrServerFactory>>,
    ) -> Self {
        let (ev_sender, ev_receiver) = ServerEvent::create_channel();
        if let Some(cliprdr) = cliprdr_factory.as_mut() {
            cliprdr.set_sender(ev_sender.clone());
        }
        if let Some(snd) = sound_factory.as_mut() {
            snd.set_sender(ev_sender.clone());
        }
        Self {
            opts,
            handler: Arc::new(Mutex::new(handler)),
            display: Arc::new(Mutex::new(display)),
            gfx_server: None,
            static_channels: StaticChannelSet::new(),
            sound_factory,
            cliprdr_factory,
            ev_sender,
            ev_receiver: Arc::new(Mutex::new(ev_receiver)),
            creds: None,
            authenticate: None,
            local_addr: None,
            force_keyframe: None,
        }
    }

    pub fn builder() -> builder::RdpServerBuilder<builder::WantsAddr> {
        builder::RdpServerBuilder::new()
    }

    /// Set the shared GfxServer for H.264 AVC420 frame transport.
    pub fn set_gfx_server(&mut self, gfx: Arc<std::sync::Mutex<crate::gfx::GfxServer>>) {
        self.gfx_server = Some(gfx);
    }

    /// Set a shared flag to force H.264 keyframe generation.
    /// The display loop checks this flag before encoding each frame.
    pub fn set_force_keyframe(&mut self, flag: Arc<std::sync::atomic::AtomicBool>) {
        self.force_keyframe = Some(flag);
    }

    pub fn event_sender(&self) -> &mpsc::UnboundedSender<ServerEvent> {
        &self.ev_sender
    }

    fn attach_channels(&mut self, acceptor: &mut Acceptor) {
        if let Some(cliprdr_factory) = self.cliprdr_factory.as_deref() {
            let backend = cliprdr_factory.build_cliprdr_backend();

            let cliprdr = CliprdrServer::new(backend);

            acceptor.attach_static_channel(cliprdr);
        }

        if let Some(factory) = self.sound_factory.as_deref() {
            let backend = factory.build_backend();

            acceptor.attach_static_channel(RdpsndServer::new(backend));
        }

        let dcs_backend = DisplayControlBackend::new(Arc::clone(&self.display));
        let mut dvc = dvc::DrdynvcServer::new()
            .with_dynamic_channel(AInputHandler {
                handler: Arc::clone(&self.handler),
            })
            .with_dynamic_channel(DisplayControlServer::new(Box::new(dcs_backend)));

        // Register GfxServer as a DVC channel if configured.
        // Use Arc::clone (not take) so gfx_server remains available for
        // deactivation-reactivation cycles.
        if let Some(gfx) = self.gfx_server.as_ref() {
            dvc = dvc.with_dynamic_channel(crate::gfx::SharedGfxServer::new(Arc::clone(gfx)));
        }

        acceptor.attach_static_channel(dvc);
    }

    pub async fn run_connection(&mut self, stream: TcpStream) -> Result<()> {
        let framed = TokioFramed::new(stream);

        let size = self.display.lock().await.size().await;
        let capabilities = capabilities::capabilities(&self.opts, size);
        let mut acceptor = Acceptor::new(self.opts.security.flag(), size, capabilities, self.creds.clone());

        self.attach_channels(&mut acceptor);

        if let Some(ref authenticator) = self.authenticate {
            acceptor.set_authenticator(authenticator.clone());
        }

        let res = ironrdp_acceptor::accept_begin(framed, &mut acceptor)
            .await
            .context("accept_begin failed")?;

        match res {
            BeginResult::ShouldUpgrade(stream) => {
                let tls_acceptor = match &self.opts.security {
                    RdpServerSecurity::Tls(acceptor) => acceptor,
                    RdpServerSecurity::Hybrid((acceptor, _)) => acceptor,
                    RdpServerSecurity::None => unreachable!(),
                };
                let accept = match tls_acceptor.accept(stream).await {
                    Ok(accept) => accept,
                    Err(e) => {
                        warn!("Failed to TLS accept: {}", e);
                        return Ok(());
                    }
                };
                let mut framed = TokioFramed::new(accept);

                acceptor.mark_security_upgrade_as_done();

                if let RdpServerSecurity::Hybrid((_, pub_key)) = &self.opts.security {
                    // how to get the client name?
                    // doesn't seem to matter yet
                    let client_name = framed.get_inner().0.get_ref().0.peer_addr()?.to_string();

                    ironrdp_acceptor::accept_credssp(
                        &mut framed,
                        &mut acceptor,
                        &mut ironrdp_tokio::reqwest::ReqwestNetworkClient::new(),
                        client_name.into(),
                        pub_key.clone(),
                        None,
                    )
                    .await?;
                }

                let framed = self.accept_finalize(framed, acceptor).await?;
                debug!("Shutting down TLS connection");
                let (mut tls_stream, _) = framed.into_inner();
                if let Err(e) = tls_stream.shutdown().await {
                    debug!(?e, "TLS shutdown error");
                }
            }

            BeginResult::Continue(framed) => {
                self.accept_finalize(framed, acceptor).await?;
            }
        };

        Ok(())
    }

    pub async fn run(&mut self) -> Result<()> {
        let listener = TcpListener::bind(self.opts.addr).await?;
        let local_addr = listener.local_addr()?;

        debug!("Listening for connections on {local_addr}");
        self.local_addr = Some(local_addr);

        loop {
            let ev_receiver = Arc::clone(&self.ev_receiver);
            let mut ev_receiver = ev_receiver.lock().await;
            tokio::select! {
                Some(event) = ev_receiver.recv() => {
                    match event {
                        ServerEvent::Quit(reason) => {
                            debug!("Got quit event {reason}");
                            break;
                        }
                        ServerEvent::GetLocalAddr(tx) => {
                            let _ = tx.send(self.local_addr);
                        }
                        ServerEvent::SetCredentials(creds) => {
                            self.set_credentials(Some(creds));
                        }
                        ev => {
                            debug!("Unexpected event {:?}", ev);
                        }
                    }
                },
                Ok((stream, peer)) = listener.accept() => {
                    debug!(?peer, "Received connection");
                    // Enable TCP keepalive so the server detects client disconnects
                    // (especially important for RDP clients that close without
                    // sending a disconnect PDU).
                    if let Err(e) = set_tcp_keepalive(&stream) {
                        warn!("Failed to set TCP keepalive: {e}");
                    }
                    drop(ev_receiver);
                    if let Err(error) = self.run_connection(stream).await {
                        error!(?error, "Connection error");
                    }
                    self.display.lock().await.client_disconnected();
                    self.static_channels = StaticChannelSet::new();
                }
                else => break,
            }
        }

        Ok(())
    }

    pub fn get_svc_processor<T: SvcProcessor + 'static>(&mut self) -> Option<&mut T> {
        self.static_channels
            .get_by_type_mut::<T>()
            .and_then(|svc| svc.channel_processor_downcast_mut())
    }

    pub fn get_channel_id_by_type<T: SvcProcessor + 'static>(&self) -> Option<StaticChannelId> {
        self.static_channels.get_channel_id_by_type::<T>()
    }

    async fn dispatch_pdu(
        &mut self,
        action: Action,
        bytes: bytes::BytesMut,
        writer: &mut impl FramedWrite,
        io_channel_id: u16,
        user_channel_id: u16,
    ) -> Result<RunState> {
        match action {
            Action::FastPath => {
                let input = decode(&bytes)?;
                self.handle_fastpath(input).await;
            }

            Action::X224 => {
                if self
                    .handle_x224(writer, io_channel_id, user_channel_id, &bytes)
                    .await
                    .context("X224 input error")?
                {
                    debug!("Got disconnect request");
                    return Ok(RunState::Disconnect);
                }
            }
        }

        Ok(RunState::Continue)
    }

    async fn dispatch_display_update(
        update: DisplayUpdate,
        writer: &mut impl FramedWrite,
        user_channel_id: u16,
        io_channel_id: u16,
        buffer: &mut Vec<u8>,
        mut encoder: UpdateEncoder,
    ) -> Result<(RunState, UpdateEncoder)> {
        if let DisplayUpdate::Resize(desktop_size) = update {
            debug!(?desktop_size, "Display resize");
            encoder.set_desktop_size(desktop_size);
            deactivate_all(io_channel_id, user_channel_id, writer).await?;
            return Ok((RunState::DeactivationReactivation { desktop_size }, encoder));
        }

        // Avc420 updates are handled externally via the GfxServer DVC channel.
        // The dispatch_display loop in client_loop intercepts them before calling here.
        if matches!(update, DisplayUpdate::Avc420(_)) {
            return Ok((RunState::Continue, encoder));
        }

        let mut encoder_iter = encoder.update(update);
        loop {
            let Some(fragmenter) = encoder_iter.next().await else {
                break;
            };

            let mut fragmenter = fragmenter.context("error while encoding")?;
            if fragmenter.size_hint() > buffer.len() {
                buffer.resize(fragmenter.size_hint(), 0);
            }

            while let Some(len) = fragmenter.next(buffer) {
                writer
                    .write_all(&buffer[..len])
                    .await
                    .context("failed to write display update")?;
            }
        }

        Ok((RunState::Continue, encoder))
    }

    async fn dispatch_server_events(
        &mut self,
        events: &mut Vec<ServerEvent>,
        writer: &mut impl FramedWrite,
        user_channel_id: u16,
    ) -> Result<RunState> {
        // Avoid wave message queuing up and causing extra delays.
        // This is a naive solution, better solutions should compute the actual delay, add IO priority, encode audio, use UDP etc.
        // 4 frames should roughly corresponds to hundreds of ms in regular setups.
        let mut wave_limit = 4;
        for event in events.drain(..) {
            trace!(?event, "Dispatching");
            match event {
                ServerEvent::Quit(reason) => {
                    debug!("Got quit event: {reason}");
                    return Ok(RunState::Disconnect);
                }
                ServerEvent::GetLocalAddr(tx) => {
                    let _ = tx.send(self.local_addr);
                }
                ServerEvent::SetCredentials(creds) => {
                    self.set_credentials(Some(creds));
                }
                ServerEvent::Rdpsnd(s) => {
                    let Some(rdpsnd) = self.get_svc_processor::<RdpsndServer>() else {
                        warn!("No rdpsnd channel, dropping event");
                        continue;
                    };
                    let msgs = match s {
                        RdpsndServerMessage::Wave(data, ts) => {
                            if wave_limit == 0 {
                                debug!("Dropping wave");
                                continue;
                            }
                            wave_limit -= 1;
                            rdpsnd.wave(data, ts)
                        }
                        RdpsndServerMessage::SetVolume { left, right } => rdpsnd.set_volume(left, right),
                        RdpsndServerMessage::Close => rdpsnd.close(),
                        RdpsndServerMessage::Error(error) => {
                            error!(?error, "Handling rdpsnd event");
                            continue;
                        }
                    }
                    .context("failed to send rdpsnd event")?;
                    let channel_id = self
                        .get_channel_id_by_type::<RdpsndServer>()
                        .ok_or_else(|| anyhow!("SVC channel not found"))?;
                    let data = server_encode_svc_messages(msgs.into(), channel_id, user_channel_id)?;
                    writer.write_all(&data).await?;
                }
                ServerEvent::Clipboard(c) => {
                    let Some(cliprdr) = self.get_svc_processor::<CliprdrServer>() else {
                        warn!("No clipboard channel, dropping event");
                        continue;
                    };
                    let msgs = match c {
                        ClipboardMessage::SendInitiateCopy(formats) => cliprdr.initiate_copy(&formats),
                        ClipboardMessage::SendFormatData(data) => cliprdr.submit_format_data(data),
                        ClipboardMessage::SendInitiatePaste(format) => cliprdr.initiate_paste(format),
                        ClipboardMessage::Error(error) => {
                            error!(?error, "Handling clipboard event");
                            continue;
                        }
                    }
                    .context("failed to send clipboard event")?;
                    let channel_id = self
                        .get_channel_id_by_type::<CliprdrServer>()
                        .ok_or_else(|| anyhow!("SVC channel not found"))?;
                    let data = server_encode_svc_messages(msgs.into(), channel_id, user_channel_id)?;
                    writer.write_all(&data).await?;
                }
            }
        }

        Ok(RunState::Continue)
    }

    async fn client_loop<R, W>(
        &mut self,
        reader: &mut Framed<R>,
        writer: &mut Framed<W>,
        io_channel_id: u16,
        user_channel_id: u16,
        mut encoder: UpdateEncoder,
    ) -> Result<RunState>
    where
        R: FramedRead,
        W: FramedWrite,
    {
        debug!("Starting client loop");
        let mut display_updates = self.display.lock().await.updates().await?;
        let gfx_server = self.gfx_server.clone();
        let force_keyframe = self.force_keyframe.clone();
        let mut writer = SharedWriter::new(writer);
        let mut display_writer = writer.clone();
        let mut event_writer = writer.clone();
        let ev_receiver = Arc::clone(&self.ev_receiver);
        let s = Rc::new(Mutex::new(self));

        let this = Rc::clone(&s);
        let dispatch_pdu = async move {
            loop {
                let (action, bytes) = reader.read_pdu().await?;
                let mut this = this.lock().await;
                match this
                    .dispatch_pdu(action, bytes, &mut writer, io_channel_id, user_channel_id)
                    .await?
                {
                    RunState::Continue => continue,
                    state => break Ok(state),
                }
            }
        };

        let dispatch_display = async move {
            let mut buffer = vec![0u8; 4096];

            loop {
                match display_updates.next_update().await {
                    Ok(Some(update)) => {
                    // Intercept Resize updates when GFX is active.
                    // Instead of deactivation-reactivation (which breaks DVC channels),
                    // resize the GFX surface in-place and continue.
                    if let DisplayUpdate::Resize(desktop_size) = &update {
                        debug!(?desktop_size, "Display resize");
                        if let Some(ref gfx) = gfx_server {
                            let mut gfx_lock = gfx.lock().unwrap();
                            // Only use the GFX resize path if AVC420 is enabled.
                            // When AVC420 is disabled, bitmap updates go through
                            // SurfaceCommands and need deactivation-reactivation.
                            if gfx_lock.is_active() && gfx_lock.avc420_enabled() {
                                debug!(
                                    "GFX resize: {}×{} → {}×{}",
                                    gfx_lock.desktop_width(), gfx_lock.desktop_height(),
                                    desktop_size.width, desktop_size.height,
                                );
                                gfx_lock.set_desktop_size(desktop_size.width, desktop_size.height);
                                gfx_lock.init_surface();
                                // Flush init_surface PDUs to the client.
                                let pending = gfx_lock.drain_pending();
                                if !pending.is_empty() {
                                    let dynamic_channel_id = gfx_lock.channel_id();
                                    let drdynvc_id = gfx_lock.drdynvc_channel_id();
                                    if let (Some(dyn_id), Some(drdynvc_id)) = (dynamic_channel_id, drdynvc_id) {
                                        debug!("Flushing {} pending GFX PDUs (resize)", pending.len());
                                        match dvc::encode_dvc_messages(dyn_id, pending, ironrdp_svc::ChannelFlags::SHOW_PROTOCOL) {
                                            Ok(svc_messages) => {
                                                match server_encode_svc_messages(svc_messages, drdynvc_id, user_channel_id) {
                                                    Ok(encoded) => {
                                                        if let Err(e) = display_writer.write_all(&encoded).await {
                                                            warn!("Failed to write GFX PDUs: {e}");
                                                        }
                                                    }
                                                    Err(e) => warn!("Failed to encode GFX SVC messages: {e}"),
                                                }
                                            }
                                            Err(e) => warn!("Failed to encode GFX DVC messages: {e}"),
                                        }
                                    }
                                }
                                continue; // Resize handled by GFX, skip dispatch_display_update
                            }
                        }
                        // GFX not active — fall through to dispatch_display_update
                        // which will trigger deactivation-reactivation.
                    }
                    // Intercept Avc420 updates and route through the GFX channel.
                    // When the GFX channel is active, send frames directly.
                    // When not yet active (channel setup still in progress),
                    // skip the frame — the next frame will go through once active.
                    if let DisplayUpdate::Avc420(ref avc) = update {
                        debug!("Avc420 update: {}×{} keyframe={}", avc.width, avc.height, avc.is_keyframe);
                        if let Some(ref gfx) = gfx_server {
                            let mut gfx_lock = gfx.lock().unwrap();
                            debug!("GFX active={}, channel_id={:?}, drdynvc_id={:?}, needs_keyframe={}",
                                gfx_lock.is_active(), gfx_lock.channel_id(), gfx_lock.drdynvc_channel_id(),
                                gfx_lock.needs_keyframe());
                            if gfx_lock.is_active() {
                                // Skip P-frames until a keyframe arrives. The client
                                // can't decode P-frames without a preceding keyframe.
                                // Force the encoder to produce a keyframe on the next
                                // frame so we don't wait for the natural keyframe interval.
                                if gfx_lock.needs_keyframe() && !avc.is_keyframe {
                                    debug!("Skipping P-frame: need keyframe first, signaling encoder");
                                    if let Some(ref flag) = force_keyframe {
                                        flag.store(true, std::sync::atomic::Ordering::Relaxed);
                                    }
                                    continue;
                                }
                                // Mark keyframe received.
                                if avc.is_keyframe {
                                    gfx_lock.clear_needs_keyframe();
                                }
                                // Update GFX surface to match frame dimensions if needed.
                                if gfx_lock.desktop_width() != avc.width || gfx_lock.desktop_height() != avc.height {
                                    debug!(
                                        "GFX surface resize: {}×{} → {}×{}",
                                        gfx_lock.desktop_width(), gfx_lock.desktop_height(),
                                        avc.width, avc.height,
                                    );
                                    gfx_lock.set_desktop_size(avc.width, avc.height);
                                    gfx_lock.init_surface();
                                }
                                gfx_lock.send_avc420_frame(
                                    avc.data.clone(),
                                    avc.width,
                                    avc.height,
                                    28, // quantization parameter
                                    avc.is_keyframe,
                                );
                                // Proactively flush GFX PDUs to the client.
                                // The GFX channel PDUs accumulate in pending until
                                // flushed. Without proactive flushing, the client
                                // would only receive PDUs when it sends data to
                                // the GFX channel, causing severe latency.
                                let pending = gfx_lock.drain_pending();
                                if !pending.is_empty() {
                                    let dynamic_channel_id = gfx_lock.channel_id();
                                    let drdynvc_id = gfx_lock.drdynvc_channel_id();
                                    if let (Some(dyn_id), Some(drdynvc_id)) = (dynamic_channel_id, drdynvc_id) {
                                        debug!("Flushing {} pending GFX PDUs", pending.len());
                                        match dvc::encode_dvc_messages(dyn_id, pending, ironrdp_svc::ChannelFlags::SHOW_PROTOCOL) {
                                            Ok(svc_messages) => {
                                                match server_encode_svc_messages(svc_messages, drdynvc_id, user_channel_id) {
                                                    Ok(encoded) => {
                                                        if let Err(e) = display_writer.write_all(&encoded).await {
                                                            warn!("Failed to write GFX PDUs: {e}");
                                                        }
                                                    }
                                                    Err(e) => warn!("Failed to encode GFX SVC messages: {e}"),
                                                }
                                            }
                                            Err(e) => warn!("Failed to encode GFX DVC messages: {e}"),
                                        }
                                    }
                                }
                            } else {
                                debug!("GFX not active, skipping Avc420 frame (will retry on next frame)");
                                // Signal the encoder to produce a keyframe when the GFX
                                // channel becomes active, so we can send it immediately.
                                if let Some(ref flag) = force_keyframe {
                                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                        } else {
                            warn!("Avc420 update but no GfxServer configured");
                        }
                        continue; // skip the normal SurfaceCommand path
                    }
                        match Self::dispatch_display_update(
                            update,
                            &mut display_writer,
                            user_channel_id,
                            io_channel_id,
                            &mut buffer,
                            encoder,
                        )
                        .await?
                        {
                            (RunState::Continue, enc) => {
                                encoder = enc;
                                continue;
                            }
                            (state, _) => {
                                break Ok(state);
                            }
                        }
                    }
                    Ok(None) => {
                        break Ok(RunState::Disconnect);
                    }
                    Err(error) => {
                        warn!(error = format!("{error:#}"), "next_updated failed");
                    }
                }
            }
        };

        let this = Rc::clone(&s);
        let mut ev_receiver = ev_receiver.lock().await;
        let dispatch_events = async move {
            let mut events = Vec::with_capacity(100);
            loop {
                let nevents = ev_receiver.recv_many(&mut events, 100).await;
                if nevents == 0 {
                    debug!("No sever events.. stopping");
                    break Ok(RunState::Disconnect);
                }
                while let Ok(ev) = ev_receiver.try_recv() {
                    events.push(ev);
                }
                let mut this = this.lock().await;
                match this
                    .dispatch_server_events(&mut events, &mut event_writer, user_channel_id)
                    .await?
                {
                    RunState::Continue => continue,
                    state => break Ok(state),
                }
            }
        };

        let state = tokio::select!(
            state = dispatch_pdu => state,
            state = dispatch_display => state,
            state = dispatch_events => state,
        );

        debug!("End of client loop: {state:?}");
        state
    }

    async fn client_accepted<R, W>(
        &mut self,
        reader: &mut Framed<R>,
        writer: &mut Framed<W>,
        result: AcceptorResult,
    ) -> Result<RunState>
    where
        R: FramedRead,
        W: FramedWrite,
    {
        debug!("Client accepted");

        if !result.input_events.is_empty() {
            debug!("Handling input event backlog from acceptor sequence");
            self.handle_input_backlog(
                writer,
                result.io_channel_id,
                result.user_channel_id,
                result.input_events,
            )
            .await?;
        }

        self.static_channels = result.static_channels;

        // Set the drdynvc static channel ID on the GfxServer so it can flush
        // PDUs proactively without waiting for client messages.
        if let Some(gfx) = self.gfx_server.as_ref() {
            let drdynvc_id = self.static_channels
                .get_channel_id_by_type::<ironrdp_dvc::DrdynvcServer>();
            if let Some(id) = drdynvc_id {
                gfx.lock().unwrap().set_drdynvc_channel_id(id);
                debug!("GFX: set drdynvc static channel id={id}");
            }
        }
        if !result.reactivation {
            for (_type_id, channel, channel_id) in self.static_channels.iter_mut() {
                debug!(?channel, ?channel_id, "Start");
                let Some(channel_id) = channel_id else {
                    continue;
                };
                let svc_responses = channel.start()?;
                let response = server_encode_svc_messages(svc_responses, channel_id, result.user_channel_id)?;
                writer.write_all(&response).await?;
            }
        }

        let mut update_codecs = UpdateEncoderCodecs::new();
        let mut surface_flags = CmdFlags::empty();
        for c in result.capabilities {
            match c {
                CapabilitySet::General(c) => {
                    let fastpath = c.extra_flags.contains(GeneralExtraFlags::FASTPATH_OUTPUT_SUPPORTED);
                    if !fastpath {
                        bail!("Fastpath output not supported!");
                    }
                }
                CapabilitySet::Bitmap(b) => {
                    if !b.desktop_resize_flag {
                        debug!("Desktop resize is not supported by the client");
                        continue;
                    }

                    let client_size = DesktopSize {
                        width: b.desktop_width,
                        height: b.desktop_height,
                    };
                    let display_size = self.display.lock().await.size().await;

                    // If the client size is different from the display size,
                    // resize the display to match the client.
                    // This prevents sending oversized frames that the client can't handle.
                    if client_size.width != display_size.width || client_size.height != display_size.height {
                        debug!(
                            "Resizing display to match client: {:?} -> {:?}",
                            display_size, client_size
                        );
                        // Create a synthetic monitor layout to trigger the resize
                        // No scale factor or physical dimensions needed for resize
                        if let Ok(layout) = DisplayControlMonitorLayout::new_single_primary_monitor(
                            client_size.width as u32,
                            client_size.height as u32,
                            None,  // scale_factor
                            None,  // physical_dims
                        ) {
                            self.display.lock().await.request_layout(layout);
                        }
                    }

                    // Update the GfxServer desktop size AFTER request_layout() so that
                    // any display mode switch is reflected. The GFX surface must match
                    // the H264 frame dimensions, which come from the physical display
                    // (e.g. after mode switching to 1280×800).
                    let new_display_size = self.display.lock().await.size().await;
                    if let Some(gfx) = self.gfx_server.as_ref() {
                        let mut gfx_lock = gfx.lock().unwrap();
                        if gfx_lock.desktop_width() != new_display_size.width || gfx_lock.desktop_height() != new_display_size.height {
                            debug!(
                                "GFX desktop size update to display size: {}×{} → {}×{}",
                                gfx_lock.desktop_width(), gfx_lock.desktop_height(),
                                new_display_size.width, new_display_size.height
                            );
                            gfx_lock.set_desktop_size(new_display_size.width, new_display_size.height);
                        }
                    }
                }
                CapabilitySet::SurfaceCommands(c) => {
                    surface_flags = c.flags;
                }
                CapabilitySet::BitmapCodecs(BitmapCodecs(codecs)) => {
                    for codec in codecs {
                        match codec.property {
                            // FIXME: The encoder operates in image mode only.
                            //
                            // See [MS-RDPRFX] 3.1.1.1 "State Machine" for
                            // implementation of the video mode. which allows to
                            // skip sending Header for each image.
                            //
                            // We should distinguish parameters for both modes,
                            // and somehow choose the "best", instead of picking
                            // the last parsed here.
                            CodecProperty::RemoteFx(rdp::capability_sets::RemoteFxContainer::ClientContainer(c))
                                if self.opts.has_remote_fx() =>
                            {
                                for caps in c.caps_data.0 .0 {
                                    update_codecs.set_remotefx(Some((caps.entropy_bits, codec.id)));
                                }
                            }
                            CodecProperty::ImageRemoteFx(rdp::capability_sets::RemoteFxContainer::ClientContainer(
                                c,
                            )) if self.opts.has_image_remote_fx() => {
                                for caps in c.caps_data.0 .0 {
                                    update_codecs.set_remotefx(Some((caps.entropy_bits, codec.id)));
                                }
                            }
                            CodecProperty::NsCodec(_) => (),
                            #[cfg(feature = "qoi")]
                            CodecProperty::Qoi if self.opts.has_qoi() => {
                                update_codecs.set_qoi(Some(codec.id));
                            }
                            #[cfg(feature = "qoiz")]
                            CodecProperty::QoiZ if self.opts.has_qoiz() => {
                                update_codecs.set_qoiz(Some(codec.id));
                            }
                            _ => (),
                        }
                    }
                }
                _ => {}
            }
        }

        let desktop_size = self.display.lock().await.size().await;
        let encoder = UpdateEncoder::new(desktop_size, surface_flags, update_codecs)
            .context("failed to initialize update encoder")?;

        let state = self
            .client_loop(reader, writer, result.io_channel_id, result.user_channel_id, encoder)
            .await
            .context("client loop failure")?;

        Ok(state)
    }

    async fn handle_input_backlog(
        &mut self,
        writer: &mut impl FramedWrite,
        io_channel_id: u16,
        user_channel_id: u16,
        frames: Vec<Vec<u8>>,
    ) -> Result<()> {
        for frame in frames {
            match Action::from_fp_output_header(frame[0]) {
                Ok(Action::FastPath) => {
                    let input = decode(&frame)?;
                    self.handle_fastpath(input).await;
                }

                Ok(Action::X224) => {
                    let _ = self.handle_x224(writer, io_channel_id, user_channel_id, &frame).await;
                }

                // the frame here is always valid, because otherwise it would
                // have failed during the acceptor loop
                Err(_) => unreachable!(),
            }
        }

        Ok(())
    }

    async fn handle_fastpath(&mut self, input: FastPathInput) {
        for event in input.input_events().iter().copied() {
            let mut handler = self.handler.lock().await;
            match event {
                FastPathInputEvent::KeyboardEvent(flags, key) => {
                    handler.keyboard((key, flags).into());
                }

                FastPathInputEvent::UnicodeKeyboardEvent(flags, key) => {
                    handler.keyboard((key, flags).into());
                }

                FastPathInputEvent::SyncEvent(flags) => {
                    handler.keyboard(flags.into());
                }

                FastPathInputEvent::MouseEvent(mouse) => {
                    handler.mouse(mouse.into());
                }

                FastPathInputEvent::MouseEventEx(mouse) => {
                    handler.mouse(mouse.into());
                }

                FastPathInputEvent::MouseEventRel(mouse) => {
                    handler.mouse(mouse.into());
                }

                FastPathInputEvent::QoeEvent(quality) => {
                    warn!("Received QoE: {}", quality);
                }
            }
        }
    }

    async fn handle_io_channel_data(&mut self, data: SendDataRequest<'_>) -> Result<bool> {
        let control: rdp::headers::ShareControlHeader = decode(data.user_data.as_ref())?;

        match control.share_control_pdu {
            ShareControlPdu::Data(header) => match header.share_data_pdu {
                rdp::headers::ShareDataPdu::Input(pdu) => {
                    self.handle_input_event(pdu).await;
                }

                rdp::headers::ShareDataPdu::ShutdownRequest => {
                    return Ok(true);
                }

                unexpected => {
                    warn!(?unexpected, "Unexpected share data pdu");
                }
            },

            unexpected => {
                warn!(?unexpected, "Unexpected share control");
            }
        }

        Ok(false)
    }

    async fn handle_x224(
        &mut self,
        writer: &mut impl FramedWrite,
        io_channel_id: u16,
        user_channel_id: u16,
        frame: &[u8],
    ) -> Result<bool> {
        let message = decode::<X224<mcs::McsMessage<'_>>>(frame)?;
        match message.0 {
            mcs::McsMessage::SendDataRequest(data) => {
                debug!(?data, "McsMessage::SendDataRequest");
                if data.channel_id == io_channel_id {
                    return self.handle_io_channel_data(data).await;
                }

                if let Some(svc) = self.static_channels.get_by_channel_id_mut(data.channel_id) {
                    debug!(channel_id = data.channel_id, len = data.user_data.len(), "Processing SVC message");
                    let response_pdus = svc.process(&data.user_data)?;
                    debug!(channel_id = data.channel_id, n_response_pdus = response_pdus.len(), "SVC response PDUs");
                    let response = server_encode_svc_messages(response_pdus, data.channel_id, user_channel_id)?;
                    debug!(channel_id = data.channel_id, response_len = response.len(), "SVC response encoded");
                    writer.write_all(&response).await?;
                } else {
                    warn!(channel_id = data.channel_id, "Unexpected channel received: ID",);
                }
            }

            mcs::McsMessage::DisconnectProviderUltimatum(disconnect) => {
                if disconnect.reason == mcs::DisconnectReason::UserRequested {
                    return Ok(true);
                }
            }

            _ => {
                warn!(name = ironrdp_core::name(&message), "Unexpected mcs message");
            }
        }

        Ok(false)
    }

    async fn handle_input_event(&mut self, input: InputEventPdu) {
        for event in input.0 {
            let mut handler = self.handler.lock().await;
            match event {
                ironrdp_pdu::input::InputEvent::ScanCode(key) => {
                    handler.keyboard((key.key_code, key.flags).into());
                }

                ironrdp_pdu::input::InputEvent::Unicode(key) => {
                    handler.keyboard((key.unicode_code, key.flags).into());
                }

                ironrdp_pdu::input::InputEvent::Sync(sync) => {
                    handler.keyboard(sync.flags.into());
                }

                ironrdp_pdu::input::InputEvent::Mouse(mouse) => {
                    handler.mouse(mouse.into());
                }

                ironrdp_pdu::input::InputEvent::MouseX(mouse) => {
                    handler.mouse(mouse.into());
                }

                ironrdp_pdu::input::InputEvent::MouseRel(mouse) => {
                    handler.mouse(mouse.into());
                }

                ironrdp_pdu::input::InputEvent::Unused(_) => {}
            }
        }
    }

    async fn accept_finalize<S>(&mut self, mut framed: TokioFramed<S>, mut acceptor: Acceptor) -> Result<TokioFramed<S>>
    where
        S: AsyncRead + AsyncWrite + Sync + Send + Unpin,
    {
        loop {
            let (new_framed, result) = ironrdp_acceptor::accept_finalize(framed, &mut acceptor)
                .await
                .context("failed to accept client during finalize")?;

            let (mut reader, mut writer) = split_tokio_framed(new_framed);

            match self.client_accepted(&mut reader, &mut writer, result).await? {
                RunState::Continue => {
                    unreachable!();
                }
                RunState::DeactivationReactivation { desktop_size } => {
                    // No description of such behavior was found in the
                    // specification, but apparently, we must keep the channel
                    // state as they were during reactivation. This fixes
                    // various state issues during client resize.
                    acceptor = Acceptor::new_deactivation_reactivation(
                        acceptor,
                        core::mem::take(&mut self.static_channels),
                        desktop_size,
                    )?;
                    framed = unsplit_tokio_framed(reader, writer);
                    continue;
                }
                RunState::Disconnect => {
                    let final_framed = unsplit_tokio_framed(reader, writer);
                    return Ok(final_framed);
                }
            }
        }
    }

    pub fn set_credentials(&mut self, creds: Option<Credentials>) {
        debug!(?creds, "Changing credentials");
        self.creds = creds
    }

    /// Install a system-authentication callback.
    ///
    /// When set the callback will be invoked with the username and password
    /// sent by the RDP client.  Return `true` to accept the connection,
    /// `false` to reject it.
    pub fn set_authenticator(&mut self, f: Arc<dyn Fn(&str, &str) -> bool + Send + Sync>) {
        self.authenticate = Some(f);
    }
}

async fn deactivate_all(
    io_channel_id: u16,
    user_channel_id: u16,
    writer: &mut impl FramedWrite,
) -> Result<(), anyhow::Error> {
    let pdu = ShareControlPdu::ServerDeactivateAll(ServerDeactivateAll);
    let pdu = rdp::headers::ShareControlHeader {
        share_id: 0,
        pdu_source: io_channel_id,
        share_control_pdu: pdu,
    };
    let user_data = encode_vec(&pdu)?.into();
    let pdu = SendDataIndication {
        initiator_id: user_channel_id,
        channel_id: io_channel_id,
        user_data,
    };
    let msg = encode_vec(&X224(pdu))?;
    writer.write_all(&msg).await?;
    Ok(())
}

struct SharedWriter<'w, W: FramedWrite> {
    writer: Rc<Mutex<&'w mut W>>,
}

impl<W: FramedWrite> Clone for SharedWriter<'_, W> {
    fn clone(&self) -> Self {
        Self {
            writer: Rc::clone(&self.writer),
        }
    }
}

impl<W> FramedWrite for SharedWriter<'_, W>
where
    W: FramedWrite,
{
    type WriteAllFut<'write>
        = core::pin::Pin<Box<dyn core::future::Future<Output = std::io::Result<()>> + 'write>>
    where
        Self: 'write;

    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> Self::WriteAllFut<'a> {
        Box::pin(async {
            let mut writer = self.writer.lock().await;

            writer.write_all(buf).await?;
            Ok(())
        })
    }
}

impl<'a, W: FramedWrite> SharedWriter<'a, W> {
    fn new(writer: &'a mut W) -> Self {
        Self {
            writer: Rc::new(Mutex::new(writer)),
        }
    }
}
