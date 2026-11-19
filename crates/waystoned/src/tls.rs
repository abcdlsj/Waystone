//! TLS material loading for the HTTPS listener.

use std::io::Cursor;
use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;

use crate::instance::Instance;

/// Installs the process wide crypto provider.
///
/// Both `ring` and `aws-lc-rs` end up in the dependency graph (axum-server pulls
/// one, the test client the other), so rustls cannot pick a provider on its own.
pub fn install_crypto_provider() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Builds the rustls configuration from the persisted certificate.
pub async fn config(instance: &Instance) -> Result<axum_server::tls_rustls::RustlsConfig> {
    install_crypto_provider();
    let certs = parse_chain(&instance.cert_pem)?;
    let key = parse_key(&instance.key_pem)?;
    let _ = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs.clone(), key.clone_key())
        .context("certificate and key do not match")?;

    Ok(axum_server::tls_rustls::RustlsConfig::from_der(
        certs
            .iter()
            .map(|cert| cert.to_vec())
            .collect::<Vec<Vec<u8>>>(),
        key.secret_der().to_vec(),
    )
    .await
    .context("loading TLS material")?)
}

/// Parses a PEM certificate chain.
pub fn parse_chain(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = Cursor::new(pem.as_bytes());
    let certs = rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        anyhow::bail!("no certificate found in the PEM file");
    }
    Ok(certs)
}

/// Parses a PEM private key.
pub fn parse_key(pem: &str) -> Result<PrivateKeyDer<'static>> {
    let mut reader = Cursor::new(pem.as_bytes());
    let key = rustls_pemfile::private_key(&mut reader)?
        .context("no private key found in the PEM file")?;
    Ok(key)
}

/// Convenience wrapper that builds a `ServerConfig` for tests.
pub fn server_config(instance: &Instance) -> Result<Arc<ServerConfig>> {
    install_crypto_provider();
    let certs = parse_chain(&instance.cert_pem)?;
    let key = parse_key(&instance.key_pem)?;
    Ok(Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_generated_material() {
        let dir = std::env::temp_dir().join(format!(
            "waystone-tls-{}",
            rand::random::<u16>()
        ));
        let instance = Instance::load_or_create(&dir, &["localhost".to_string()]).unwrap();
        let certs = parse_chain(&instance.cert_pem).unwrap();
        assert_eq!(certs.len(), 1);
        assert!(parse_key(&instance.key_pem).is_ok());
        assert!(server_config(&instance).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }
}
