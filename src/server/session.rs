use crate::capture::Capturer;
use crate::codec::BitmapEncoder;
use crate::input::InputInjector;
use crate::rdp::{self, RdpSession};
use anyhow::Result;
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tracing::{debug, info};

pub async fn handle(stream: TcpStream, peer: SocketAddr) -> Result<()> {
    info!("Starting RDP session for {}", peer);

    let tls_stream = rdp::upgrade_to_tls(stream).await?;
    debug!("TLS handshake complete for {}", peer);

    let mut session = RdpSession::accept(tls_stream).await?;
    debug!("RDP negotiation complete for {}", peer);

    let mut capturer = Capturer::new()?;
    let mut encoder = BitmapEncoder::new(session.desktop_width(), session.desktop_height());
    let injector = InputInjector::new()?;

    loop {
        tokio::select! {
            // Send a screen update when a new frame is ready
            frame = capturer.next_frame() => {
                let frame = frame?;
                let update = encoder.encode(&frame)?;
                session.send_bitmap_update(update).await?;
            }
            // Process incoming client input events
            event = session.next_event() => {
                match event? {
                    rdp::Event::MouseMove { x, y } => injector.move_mouse(x, y)?,
                    rdp::Event::MouseButton { button, pressed, x, y } => {
                        injector.mouse_button(button, pressed, x, y)?;
                    }
                    rdp::Event::Key { scancode, pressed, flags } => {
                        injector.key_event(scancode, pressed, flags)?;
                    }
                    rdp::Event::Disconnect => {
                        info!("Client {} disconnected", peer);
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}
