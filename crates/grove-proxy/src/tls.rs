use crate::ca::CertAuthority;
use rustls::ServerConfig;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::sync::Arc;

/// Issues a leaf for the SNI host of each handshake, or for `localhost` when
/// the client sends no SNI (e.g. when connecting to an IP address).
#[derive(Debug)]
pub struct SniResolver {
    ca: Arc<CertAuthority>,
}

impl SniResolver {
    pub fn new(ca: Arc<CertAuthority>) -> Self {
        Self { ca }
    }
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let host = hello.server_name().unwrap_or("localhost");
        self.ca
            .issue(host)
            .inspect_err(|e| tracing::warn!(host, "can't issue certificate: {e}"))
            .ok()
    }
}

pub(crate) fn server_config(ca: Arc<CertAuthority>) -> Result<ServerConfig, rustls::Error> {
    let mut config = ServerConfig::builder_with_provider(ca.provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(SniResolver::new(ca)));
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}
