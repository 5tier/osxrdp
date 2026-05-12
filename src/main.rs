mod capture;
mod codec;
mod input;
mod rdp;
mod server;

use anyhow::Result;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> Result<()> {
    fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("osxrdp=debug".parse()?))
        .init();

    let config = server::Config {
        bind_addr: "0.0.0.0:3389".parse()?,
    };

    info!("Starting osxrdp on {}", config.bind_addr);
    server::run(config).await
}
