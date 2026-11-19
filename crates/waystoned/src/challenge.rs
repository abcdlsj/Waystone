//! Per connection authentication challenges and Ed25519 signature verification.
//!
//! Kept separate from the hub so the crypto boundary is obvious: the hub decides
//! *who* may connect, this module decides *whether a signature is genuine*.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use waystone_proto::identity::Challenge;

/// How long a challenge stays valid. A slow phone on a bad network still has
/// plenty of time, while a recorded handshake becomes useless quickly.
pub const CHALLENGE_TTL: Duration = Duration::from_secs(60);

/// Issued challenges, keyed by connection id.
pub struct ChallengeStore {
    inner: Mutex<StoreInner>,
}

struct StoreInner {
    next_connection: u64,
    issued: HashMap<u64, (Challenge, Instant)>,
}

impl ChallengeStore {
    pub fn new() -> Self {
        ChallengeStore {
            inner: Mutex::new(StoreInner {
                next_connection: 1,
                issued: HashMap::new(),
            }),
        }
    }

    /// Allocates the next connection id.
    ///
    /// Ids only need to be unique inside one process, so a counter is enough and
    /// avoids the risk of a collision that random ids would carry.
    pub fn next_connection(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_connection;
        inner.next_connection += 1;
        id
    }

    /// Issues a fresh challenge for `connection_id`.
    pub fn issue(&self, connection_id: u64, server_id: &str) -> Challenge {
        self.prune();
        let mut nonce = [0u8; 16];
        rand::Rng::fill(&mut rand::rng(), &mut nonce);
        let challenge = Challenge {
            nonce: hex::encode(nonce),
            server_id: server_id.to_string(),
        };
        let mut inner = self.inner.lock().unwrap();
        inner
            .issued
            .insert(connection_id, (challenge.clone(), Instant::now()));
        challenge
    }

    /// Takes the challenge for `connection_id`, so it can only be used once.
    pub fn take(&self, connection_id: u64) -> Option<Challenge> {
        let mut inner = self.inner.lock().unwrap();
        let (challenge, issued_at) = inner.issued.remove(&connection_id)?;
        if issued_at.elapsed() > CHALLENGE_TTL {
            return None;
        }
        Some(challenge)
    }

    /// Drops state for a closed connection.
    pub fn forget(&self, connection_id: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.issued.remove(&connection_id);
    }

    fn prune(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner
            .issued
            .retain(|_, (_, issued_at)| issued_at.elapsed() <= CHALLENGE_TTL);
    }
}

impl Default for ChallengeStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Verifies an Ed25519 signature over `payload`.
pub fn verify(public_key: &str, payload: &[u8], signature: &[u8]) -> bool {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let Ok(key_bytes) = waystone_proto::identity::decode_public_key(public_key) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&key_bytes) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(signature) else {
        return false;
    };
    key.verify(payload, &signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn keypair() -> (SigningKey, String) {
        let signing = SigningKey::from_bytes(&[42u8; 32]);
        let public = waystone_proto::identity::encode_public_key(&signing.verifying_key().to_bytes());
        (signing, public)
    }

    #[test]
    fn challenge_is_single_use() {
        let store = ChallengeStore::new();
        let id = store.next_connection();
        let challenge = store.issue(id, "server");
        assert_eq!(challenge.nonce.len(), 32);
        assert_eq!(store.take(id).unwrap(), challenge);
        assert!(store.take(id).is_none());
    }

    #[test]
    fn ids_are_unique() {
        let store = ChallengeStore::new();
        let first = store.next_connection();
        let second = store.next_connection();
        assert_ne!(first, second);
    }

    #[test]
    fn signature_verification() {
        let (signing, public) = keypair();
        let challenge = Challenge {
            nonce: "aabb".into(),
            server_id: "server".into(),
        };
        let payload = challenge.signing_payload();
        let signature = signing.sign(&payload).to_bytes();

        assert!(verify(&public, &payload, &signature));
        // A different server id must not validate.
        let other = Challenge {
            nonce: "aabb".into(),
            server_id: "other".into(),
        };
        assert!(!verify(&public, &other.signing_payload(), &signature));
        // Truncated signatures are rejected instead of panicking.
        assert!(!verify(&public, &payload, &signature[..10]));
    }
}
