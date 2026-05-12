use anyhow::Result;
use rcgen::generate_simple_self_signed;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;

/// Wraps a raw TCP stream in TLS using a self-signed certificate.
///
/// Production use: load a real cert from disk via `--cert` / `--key` flags.
pub async fn upgrade_to_tls(
    stream: TcpStream,
) -> Result<tokio_rustls::server::TlsStream<TcpStream>> {
    let acceptor = build_acceptor()?;
    let tls_stream = acceptor.accept(stream).await?;
    Ok(tls_stream)
}

fn build_acceptor() -> Result<TlsAcceptor> {
    let certified = generate_simple_self_signed(vec!["localhost".to_string()])?;

    let cert_der: CertificateDer<'static> =
        CertificateDer::from(certified.cert.der().to_vec());

    let key_der: PrivateKeyDer<'static> =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            certified.key_pair.serialize_der(),
        ));

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}
