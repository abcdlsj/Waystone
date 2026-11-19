//! Persistent configuration and device identity of the host agent.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};

use waystone_proto::device::{DeviceId, DeviceInfo, Platform, Role};
use waystone_proto::identity::encode_public_key;

/// Everything the agent needs to reconnect without user interaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Authority of the signaling server, e.g. `home.example.com:8443`.
    pub endpoint: String,
    /// STUN/TURN authority.
    pub turn: String,
    /// Pinned certificate fingerprint, lowercase hex.
    pub fingerprint: String,
    /// Display name shown in the phone's device list.
    pub name: String,
    /// Video bitrate ceiling before relay clamping, in bits per second.
    #[serde(default = "default_bitrate")]
    pub max_bitrate: u32,
    /// Frame rate to request from the capture layer.
    #[serde(default = "default_fps")]
    pub fps: u32,
    /// Capture resolution.
    #[serde(default = "default_size")]
    pub capture: (u32, u32),
}

fn default_bitrate() -> u32 {
    8_000_000
}

fn default_fps() -> u32 {
    60
}

fn default_size() -> (u32, u32) {
    (1920, 1080)
}

impl Config {
    /// Loads the configuration from `path`.
    pub fn load(path: &Path) -> Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(config)
    }

    /// Writes the configuration back, keeping the file private.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, &text)?;
        restrict(path)?;
        Ok(())
    }

    /// Hostname with a sensible default.
    pub fn default_name() -> String {
        hostname().unwrap_or_else(|| "Waystone Host".to_string())
    }
}

/// The device key pair, persisted as a hex seed.
pub struct DeviceKey {
    signing: SigningKey,
    pub info_key: String,
}

impl DeviceKey {
    /// Loads the key from `path`, generating one when missing.
    pub fn load_or_create(path: &Path) -> Result<DeviceKey> {
        let signing = if path.exists() {
            let text = std::fs::read_to_string(path)?;
            let bytes = hex::decode(text.trim()).context("device key is not valid hex")?;
            let seed: [u8; 32] = bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("device key must be 32 bytes"))?;
            SigningKey::from_bytes(&seed)
        } else {
            let mut seed = [0u8; 32];
            rand::Rng::fill(&mut rand::rng(), &mut seed);
            let signing = SigningKey::from_bytes(&seed);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, hex::encode(seed))?;
            restrict(path)?;
            signing
        };

        let public = signing.verifying_key().to_bytes();
        Ok(DeviceKey {
            signing,
            info_key: encode_public_key(&public),
        })
    }

    /// Public description handed to the server during pairing and auth.
    pub fn info(&self, name: &str) -> DeviceInfo {
        let public = self.signing.verifying_key().to_bytes();
        DeviceInfo {
            id: DeviceId::from_public_key(&public),
            name: name.to_string(),
            platform: Platform::Macos,
            role: Role::Host,
            public_key: self.info_key.clone(),
        }
    }

    /// Signs a challenge payload, base64 encoding the signature.
    pub fn sign(&self, payload: &[u8]) -> String {
        use base64::Engine;
        use ed25519_dalek::Signer;
        let signature = self.signing.sign(payload);
        base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
    }
}

/// Default state directory: `~/Library/Application Support/Waystone`.
pub fn state_dir() -> PathBuf {
    match std::env::var_os("WAYSTONE_STATE_DIR") {
        Some(value) => PathBuf::from(value),
        None => home_dir()
            .map(|home| home.join("Library/Application Support/Waystone"))
            .unwrap_or_else(|| PathBuf::from(".")),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn hostname() -> Option<String> {
    let mut buffer = [0u8; 256];
    let status = unsafe { libc_gethostname(buffer.as_mut_ptr() as *mut i8, buffer.len()) };
    if status != 0 {
        return None;
    }
    let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(0);
    Some(String::from_utf8_lossy(&buffer[..end]).to_string())
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

// `gethostname` is not exposed by std, and pulling in the whole libc crate for
// one call is not worth it.
extern "C" {
    #[link_name = "gethostname"]
    fn libc_gethostname(name: *mut i8, len: usize) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable_across_loads() {
        let dir = std::env::temp_dir().join(format!("waystone-host-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("device.key");

        let first = DeviceKey::load_or_create(&path).unwrap();
        let second = DeviceKey::load_or_create(&path).unwrap();
        assert_eq!(first.info("Mac").id, second.info("Mac").id);
        assert_eq!(first.info_key, second.info_key);

        // The signature verifies with the published key.
        let payload = b"challenge";
        let signature = first.sign(payload);
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(signature)
            .unwrap();
        assert!(waystoned_verify(&first.info_key, payload, &raw));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Small local verifier so the test does not depend on the server crate.
    fn waystoned_verify(public_key: &str, payload: &[u8], signature: &[u8]) -> bool {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let key = waystone_proto::identity::decode_public_key(public_key).unwrap();
        let key = VerifyingKey::from_bytes(&key).unwrap();
        let signature = Signature::from_slice(signature).unwrap();
        key.verify(payload, &signature).is_ok()
    }

    #[test]
    fn config_roundtrip() {
        let dir = std::env::temp_dir().join(format!("waystone-host-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let config = Config {
            endpoint: "example:8443".into(),
            turn: "example:3478".into(),
            fingerprint: "a".repeat(64),
            name: "Mac Studio".into(),
            max_bitrate: 6_000_000,
            fps: 45,
            capture: (2560, 1440),
        };
        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.endpoint, config.endpoint);
        assert_eq!(loaded.max_bitrate, 6_000_000);
        assert_eq!(loaded.capture, (2560, 1440));
        std::fs::remove_dir_all(&dir).ok();
    }
}
