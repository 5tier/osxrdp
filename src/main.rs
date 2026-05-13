mod auth;
mod display;
mod h264;
mod input;
mod keyboard;
mod permissions;
mod tls;

use std::sync::Arc;

use anyhow::Result;
use display::CaptureMode;
use ironrdp_server::{GfxServer, RdpServer, RdpServerDisplay};
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

/// Install a signal handler that prints a backtrace on SIGSEGV/SIGBUS.
fn install_crash_handler() {
    unsafe {
        libc::sigaction(
            libc::SIGSEGV,
            &libc::sigaction {
                sa_sigaction: crash_handler as *const () as usize,
                sa_flags: libc::SA_SIGINFO,
                sa_mask: std::mem::zeroed(),
            },
            std::ptr::null_mut(),
        );
        libc::sigaction(
            libc::SIGBUS,
            &libc::sigaction {
                sa_sigaction: crash_handler as *const () as usize,
                sa_flags: libc::SA_SIGINFO,
                sa_mask: std::mem::zeroed(),
            },
            std::ptr::null_mut(),
        );
    }
}

extern "C" fn crash_handler(
    sig: i32,
    _info: *mut libc::siginfo_t,
    _context: *mut libc::c_void,
) {
    eprintln!("\n=== CRASH: signal {} (SIG{}) ===", sig,
        match sig {
            libc::SIGSEGV => "SEGV",
            libc::SIGBUS => "BUS",
            _ => "???",
        });
    eprintln!("Backtrace:");
    // Force-enable backtrace capture (normally gated on RUST_BACKTRACE=1 env)
    std::env::set_var("RUST_BACKTRACE", "1");
    let bt = std::backtrace::Backtrace::capture();
    eprintln!("{bt:?}");
    // Re-raise so the default handler produces a macOS crash report with full symbols
    unsafe {
        libc::raise(sig);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("osxrdp=debug".parse()?),
        )
        .init();

    install_crash_handler();

    permissions::check_and_warn().await;

    let addr = std::env::var("OSXRDP_ADDR").unwrap_or_else(|_| "0.0.0.0:3389".to_string());
    // H.264 mode is disabled by default because the GFX server doesn't
    // handle dynamic resize properly (size mismatch between display stream
    // and GFX surface). Use BGRA mode until this is fixed.
    let h264 = std::env::var("OSXRDP_H264").unwrap_or_else(|_| "0".to_string()) == "1";

    let mode = if h264 { CaptureMode::H264 } else { CaptureMode::Bgra };

    info!(%addr, ?mode, "Starting osxrdp");
    info!("Authentication: macOS system accounts via OpenDirectory");

    let tls_acceptor = tls::build_acceptor()?;

    let mut mac_display = display::MacDisplay::with_mode(mode);
    let init_size = mac_display.size().await;

    let mut server = RdpServer::builder()
        .with_addr(addr.parse::<std::net::SocketAddr>()?)
        .with_tls(tls_acceptor)
        .with_input_handler(input::MacInputHandler::new())
        .with_display_handler(mac_display)
        .build();

    // If H.264 mode is enabled, create a shared GfxServer for the RDPGFX pipeline
    if h264 {
        let gfx = GfxServer::shared(init_size.width, init_size.height);
        server.set_gfx_server(gfx);
        info!("RDPGFX H.264 pipeline enabled (OSXRDP_H264=0 to disable)");
    }

    // Authenticate against macOS system accounts via opendirectoryd.
    // The authenticator is shared across all incoming connections.
    server.set_authenticator(Arc::new(|username, password| {
        auth::verify_user_password(username, password)
    }));

    server.run().await
}