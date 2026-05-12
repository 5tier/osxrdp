mod display;
mod input;
mod keyboard;
mod permissions;
mod tls;

use anyhow::Result;
use ironrdp_server::{Credentials, RdpServer};
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("osxrdp=debug".parse()?),
        )
        .init();

    permissions::check_and_warn().await;

    let addr = std::env::var("OSXRDP_ADDR").unwrap_or_else(|_| "0.0.0.0:3389".to_string());
    let username = std::env::var("OSXRDP_USER").unwrap_or_else(|_| "admin".to_string());
    let password = std::env::var("OSXRDP_PASSWORD").unwrap_or_else(|_| "admin".to_string());

    info!(%addr, %username, "Starting osxrdp");
    info!("Connect with username={username}  password=<OSXRDP_PASSWORD env, default: admin>");

    let tls_acceptor = tls::build_acceptor()?;

    let mut server = RdpServer::builder()
        .with_addr(addr.parse::<std::net::SocketAddr>()?)
        .with_tls(tls_acceptor)
        .with_input_handler(input::MacInputHandler::new())
        .with_display_handler(display::MacDisplay::new())
        .build();

    server.set_credentials(Some(Credentials {
        username,
        password,
        domain: None,
    }));

    server.run().await
}
