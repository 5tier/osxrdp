use anyhow::Result;
use rcgen::generate_simple_self_signed;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

/// Builds a TLS acceptor with a freshly generated self-signed certificate.
///
/// For production, load a real cert/key pair from disk instead.
pub fn build_acceptor() -> Result<TlsAcceptor> {
    let certified = generate_simple_self_signed(vec!["localhost".to_string()])?;

    let cert: CertificateDer<'static> =
        CertificateDer::from(certified.cert.der().to_vec());

    let key: PrivateKeyDer<'static> =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            certified.key_pair.serialize_der(),
        ));

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}
