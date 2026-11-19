//! Instance identity: the server key pair and the self-signed TLS certificate.
//!
//! Both are generated on first boot and then reloaded from disk. The certificate
//! fingerprint is pinned by every controller, which is what allows a self-signed
//! certificate without any scary warning on the phone.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use sha2::{Digest, Sha256};

/// Material stored on disk for one instance.
pub struct Instance {
    /// Stable id used inside challenge signatures.
    pub server_id: String,
    /// PEM encoded certificate chain.
    pub cert_pem: String,
    /// PEM encoded private key.
    pub key_pem: String,
    /// Lowercase hex SHA-256 of the DER certificate.
    pub fingerprint: String,
}

/// Paths used by the daemon.
pub fn cert_path(data_dir: &Path) -> PathBuf {
    data_dir.join("server.crt")
}

/// Path of the private key.
pub fn key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("server.key")
}

/// Path of the persisted server id.
pub fn server_id_path(data_dir: &Path) -> PathBuf {
    data_dir.join("server.id")
}

impl Instance {
    /// Loads the instance material, generating it when missing.
    ///
    /// `hostnames` become subject alternative names so a phone can connect with
    /// either a LAN IP or a DNS name.
    pub fn load_or_create(data_dir: &Path, hostnames: &[String]) -> Result<Instance> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating {}", data_dir.display()))?;

        let cert_file = cert_path(data_dir);
        let key_file = key_path(data_dir);
        let id_file = server_id_path(data_dir);

        let server_id = if id_file.exists() {
            std::fs::read_to_string(&id_file)?.trim().to_string()
        } else {
            let id = random_id();
            std::fs::write(&id_file, &id)?;
            id
        };

        if cert_file.exists() && key_file.exists() {
            let cert_pem = std::fs::read_to_string(&cert_file)?;
            let key_pem = std::fs::read_to_string(&key_file)?;
            let fingerprint = fingerprint_from_pem(&cert_pem)?;
            return Ok(Instance {
                server_id,
                cert_pem,
                key_pem,
                fingerprint,
            });
        }

        let key_pair = KeyPair::generate().context("generating the TLS key pair")?;
        let mut params = CertificateParams::new(hostnames.to_vec())
            .context("building certificate parameters")?;
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, "waystone");
        name.push(DnType::OrganizationName, "Waystone");
        params.distinguished_name = name;
        // Self-signed instances live longer than the usual 90 days on purpose:
        // rotating the certificate forces every phone to re-pair.
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2099, 1, 1);

        let cert = params
            .self_signed(&key_pair)
            .context("signing the certificate")?;
        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();

        std::fs::write(&cert_file, &cert_pem)?;
        std::fs::write(&key_file, &key_pem)?;
        // The private key must not be world readable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&key_file)?.permissions();
            permissions.set_mode(0o600);
            std::fs::set_permissions(&key_file, permissions)?;
        }

        let fingerprint = fingerprint_from_pem(&cert_pem)?;
        Ok(Instance {
            server_id,
            cert_pem,
            key_pem,
            fingerprint,
        })
    }
}

/// SHA-256 fingerprint of a PEM encoded certificate, lowercase hex.
pub fn fingerprint_from_pem(cert_pem: &str) -> Result<String> {
    let der = pem_to_der(cert_pem)?;
    let digest = Sha256::digest(&der);
    Ok(hex::encode(digest))
}

/// Extracts the DER body from the first PEM block.
fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let start = pem
        .find("-----BEGIN CERTIFICATE-----")
        .context("certificate PEM has no BEGIN marker")?;
    let end = pem[start..]
        .find("-----END CERTIFICATE-----")
        .context("certificate PEM has no END marker")?;
    let body_start = start + "-----BEGIN CERTIFICATE-----".len();
    let body = &pem[body_start..start + end];
    let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .context("certificate PEM body is not valid base64")
}

fn random_id() -> String {
    let mut raw = [0u8; 8];
    rand::Rng::fill(&mut rand::rng(), &mut raw);
    hex::encode(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_and_reloads_material() {
        let dir = std::env::temp_dir().join(format!("waystone-instance-{}", random_id()));
        let hostnames = vec!["localhost".to_string(), "192.168.1.10".to_string()];

        let first = Instance::load_or_create(&dir, &hostnames).unwrap();
        assert_eq!(first.fingerprint.len(), 64);
        assert!(first.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(first.key_pem.contains("PRIVATE KEY"));

        let second = Instance::load_or_create(&dir, &hostnames).unwrap();
        assert_eq!(first.server_id, second.server_id);
        assert_eq!(first.fingerprint, second.fingerprint);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn private_key_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("waystone-perm-{}", random_id()));
        Instance::load_or_create(&dir, &["localhost".to_string()]).unwrap();
        let mode = std::fs::metadata(key_path(&dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
