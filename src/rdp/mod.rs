mod negotiation;
mod tls;

pub use tls::upgrade_to_tls;

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};

/// Decoded input event from the RDP client.
#[derive(Debug)]
pub enum Event {
    MouseMove {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: MouseButton,
        pressed: bool,
        x: u16,
        y: u16,
    },
    Key {
        scancode: u16,
        pressed: bool,
        /// RDP keyboard flags (extended key, etc.)
        flags: u16,
    },
    Disconnect,
}

#[derive(Debug)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// An accepted RDP session after negotiation is complete.
pub struct RdpSession<S> {
    stream: S,
    width: u16,
    height: u16,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> RdpSession<S> {
    pub async fn accept(stream: S) -> Result<Self> {
        let (width, height) = negotiation::perform(&stream).await?;
        Ok(Self { stream, width, height })
    }

    pub fn desktop_width(&self) -> u16 {
        self.width
    }

    pub fn desktop_height(&self) -> u16 {
        self.height
    }

    pub async fn send_bitmap_update(&mut self, _data: Vec<u8>) -> Result<()> {
        // TODO: wrap in RDP UpdateBitmapPDU and write to stream
        Ok(())
    }

    pub async fn next_event(&mut self) -> Result<Event> {
        // TODO: read and decode incoming RDP PDUs from stream
        // Stub: just park forever so the select! in session.rs doesn't busy-loop
        std::future::pending::<Result<Event>>().await
    }
}
