mod session;

use anyhow::Result;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::{error, info};

pub struct Config {
    pub bind_addr: SocketAddr,
}

pub async fn run(config: Config) -> Result<()> {
    let listener = TcpListener::bind(config.bind_addr).await?;
    info!("Listening on {}", config.bind_addr);

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                info!("New connection from {}", peer);
                tokio::spawn(async move {
                    if let Err(e) = session::handle(stream, peer).await {
                        error!("Session error from {}: {:#}", peer, e);
                    }
                });
            }
            Err(e) => error!("Accept error: {}", e),
        }
    }
}
