mod display;
mod input;
mod keyboard;
mod tls;

use anyhow::Result;
use ironrdp_server::RdpServer;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("osxrdp=debug".parse()?),
        )
        .init();

    let addr = "0.0.0.0:3389";
    let tls_acceptor = tls::build_acceptor()?;

    info!("Starting osxrdp on {addr}");

    let mut server = RdpServer::builder()
        .with_addr(addr.parse::<std::net::SocketAddr>()?)
        .with_tls(tls_acceptor)
        .with_input_handler(input::MacInputHandler::new())
        .with_display_handler(display::MacDisplay::new())
        .build();

    server.run().await
}
