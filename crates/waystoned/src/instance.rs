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

        // Key first: a crash between the two writes then leaves no certificate,
        // so the next boot regenerates both instead of loading a mismatch.
        write_private(&key_file, key_pem.as_bytes())?;
        std::fs::write(&cert_file, &cert_pem)?;

        let fingerprint = fingerprint_from_pem(&cert_pem)?;
        Ok(Instance {
            server_id,
            cert_pem,
            key_pem,
            fingerprint,
        })
    }
}

/// Path of the shared secret the TURN server derives credentials from.
pub fn turn_secret_path(data_dir: &Path) -> PathBuf {
    data_dir.join("turn.secret")
}

/// Loads the TURN shared secret, generating 256 random bits on first boot.
///
/// The secret is never derived from anything the server publishes (the server
/// id travels in clear inside every challenge), otherwise anybody could mint
/// relay credentials.
pub fn load_or_create_turn_secret(data_dir: &Path) -> Result<String> {
    let path = turn_secret_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(secret) if !secret.trim().is_empty() => return Ok(secret.trim().to_string()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", path.display()));
        }
    }
    let mut raw = [0u8; 32];
    rand::Rng::fill(&mut rand::rng(), &mut raw);
    let secret = hex::encode(raw);
    write_private(&path, secret.as_bytes())?;
    Ok(secret)
}

/// Writes a secret file that is readable by the owner only.
///
/// The file is created with mode 0600 from the start (no window where it is
/// world readable) under a temporary name, synced, then renamed over `path`
/// so readers never observe a half written secret.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .context("secret path has no file name")?
        .to_string_lossy();
    let temporary = parent.join(format!(".{file_name}.{}.tmp", random_id()));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("creating {}", temporary.display()))?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
            .with_context(|| format!("moving the secret into {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
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

    #[test]
    fn turn_secret_is_random_private_and_stable() {
        let dir = std::env::temp_dir().join(format!("waystone-secret-{}", random_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let instance = Instance::load_or_create(&dir, &["localhost".to_string()]).unwrap();
        let first = load_or_create_turn_secret(&dir).unwrap();
        assert_eq!(first.len(), 64);
        assert_ne!(first, instance.server_id, "must not reuse the public id");
        assert_eq!(load_or_create_turn_secret(&dir).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(turn_secret_path(&dir))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
