//! Persistent configuration and device identity of the host agent.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};

use waystone_proto::device::{DeviceId, DeviceInfo, Platform, Role};
use waystone_proto::identity::encode_public_key;
use waystone_proto::signal::ByeReason;

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
    /// Video bitrate for 1080p at 60 fps; other sizes scale from it.
    #[serde(default = "default_bitrate")]
    pub max_bitrate: u32,
    /// Lock the screen when a session ends without the user saying
    /// otherwise: on a network timeout, or when the phone went away
    /// (design doc 4.5, "离开自动锁屏").
    #[serde(default = "default_true")]
    pub lock_on_disconnect: bool,
    /// Mute the speakers while streaming (design doc 4.4).
    #[serde(default = "default_true")]
    pub mute_speakers: bool,
}

fn default_bitrate() -> u32 {
    8_000_000
}

fn default_true() -> bool {
    true
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
        write_private(path, serde_json::to_string_pretty(self)?.as_bytes())
    }

    pub fn new(endpoint: String, turn: String, fingerprint: String, name: String) -> Config {
        Config {
            endpoint,
            turn,
            fingerprint,
            name,
            max_bitrate: default_bitrate(),
            lock_on_disconnect: true,
            mute_speakers: true,
        }
    }

    pub fn session_options(&self) -> crate::session::Options {
        crate::session::Options {
            bitrate_budget: self.max_bitrate,
            mute_speakers: self.mute_speakers,
        }
    }

    /// Whether ending a session for `reason` locks the screen.
    pub fn locks_after(&self, reason: ByeReason) -> bool {
        match reason {
            ByeReason::LockAndExit => true,
            ByeReason::UserHangup | ByeReason::HostError => false,
            ByeReason::NetworkTimeout | ByeReason::PeerOffline => self.lock_on_disconnect,
        }
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
            write_private(path, hex::encode(seed).as_bytes())?;
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

// `gethostname` is not exposed by std, and pulling in the whole libc crate for
// one call is not worth it.
extern "C" {
    #[link_name = "gethostname"]
    fn libc_gethostname(name: *mut i8, len: usize) -> i32;
}

/// Writes a file only the owner can read, without a window where it is
/// briefly readable by others: a temporary file is created 0600, filled,
/// and renamed over the target.
fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("writing {}", temporary.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    Ok(())
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
    fn lock_policy_follows_the_reason() {
        let mut config = Config::new(String::new(), String::new(), String::new(), String::new());
        assert!(config.locks_after(ByeReason::LockAndExit));
        assert!(!config.locks_after(ByeReason::UserHangup));
        assert!(config.locks_after(ByeReason::NetworkTimeout));
        config.lock_on_disconnect = false;
        assert!(!config.locks_after(ByeReason::PeerOffline));
        assert!(config.locks_after(ByeReason::LockAndExit));
    }

    #[test]
    fn config_roundtrip() {
        let dir = std::env::temp_dir().join(format!("waystone-host-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let mut config = Config::new(
            "example:8443".into(),
            "example:3478".into(),
            "a".repeat(64),
            "Mac Studio".into(),
        );
        config.max_bitrate = 6_000_000;
        config.lock_on_disconnect = false;
        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.endpoint, config.endpoint);
        assert_eq!(loaded.max_bitrate, 6_000_000);
        assert!(!loaded.lock_on_disconnect);
        assert!(loaded.mute_speakers);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }
}
