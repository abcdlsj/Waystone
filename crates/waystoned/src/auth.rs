//! Admin authentication for the panel and the `/api` routes.
//!
//! * The first visitor claims the instance by choosing a password. The claim is
//!   one-shot: the database row is only written while it is still empty, and the
//!   in-memory copy is updated in the same step so a second claim is refused
//!   without a restart.
//! * Passwords are hashed with Argon2id (PHC string format); verification uses
//!   the crate's constant-time comparison.
//! * A successful login mints a random 256-bit session token kept server side
//!   (only its SHA-256 is stored) and handed to the browser as an `HttpOnly`,
//!   `Secure`, `SameSite=Strict` cookie.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::http::{header, HeaderMap};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::store::Store;

/// Name of the session cookie. The `__Host-` prefix makes browsers refuse it
/// unless it is `Secure`, has `Path=/` and no `Domain`, so a sibling host can
/// never plant or overwrite it.
pub const SESSION_COOKIE: &str = "__Host-waystone_session";

/// Absolute lifetime of an admin session.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

/// Shortest admin password accepted at claim time.
pub const MIN_PASSWORD_LEN: usize = 8;

/// Longest password accepted, so a huge body cannot keep the KDF busy.
pub const MAX_PASSWORD_LEN: usize = 1024;

/// Argon2 runs with ~19 MiB of memory; capping parallel runs bounds the memory
/// an attacker can make the daemon allocate with concurrent login attempts.
const MAX_CONCURRENT_KDF: usize = 2;

/// Why a claim was refused.
#[derive(Debug)]
pub enum ClaimError {
    AlreadyClaimed,
    InvalidPassword(&'static str),
    Internal(anyhow::Error),
}

/// Admin password and live panel sessions.
pub struct AdminAuth {
    store: Arc<Store>,
    password_hash: RwLock<Option<String>>,
    sessions: Mutex<HashMap<[u8; 32], Instant>>,
    kdf: Semaphore,
}

impl AdminAuth {
    /// Loads the stored password hash, if the instance is already claimed.
    pub fn load(store: Arc<Store>) -> Result<Self> {
        let password_hash = store.admin_password_hash()?;
        Ok(AdminAuth {
            store,
            password_hash: RwLock::new(password_hash),
            sessions: Mutex::new(HashMap::new()),
            kdf: Semaphore::new(MAX_CONCURRENT_KDF),
        })
    }

    /// True once an admin password is set.
    pub fn is_claimed(&self) -> bool {
        self.password_hash
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// Sets the admin password if nobody did yet, and logs the caller in.
    ///
    /// Returns the session token for the cookie.
    pub async fn claim(&self, password: String) -> Result<String, ClaimError> {
        if self.is_claimed() {
            return Err(ClaimError::AlreadyClaimed);
        }
        if password.chars().count() < MIN_PASSWORD_LEN {
            return Err(ClaimError::InvalidPassword(
                "the password must be at least 8 characters",
            ));
        }
        if password.len() > MAX_PASSWORD_LEN {
            return Err(ClaimError::InvalidPassword("the password is too long"));
        }
        let hash = self
            .run_kdf(move || hash_password(&password))
            .await
            .map_err(ClaimError::Internal)?;

        // The database decides who wins a race between two claims.
        let won = self
            .store
            .claim_admin(&hash)
            .map_err(ClaimError::Internal)?;
        let mut slot = self
            .password_hash
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !won {
            *slot = self
                .store
                .admin_password_hash()
                .map_err(ClaimError::Internal)?;
            return Err(ClaimError::AlreadyClaimed);
        }
        *slot = Some(hash);
        drop(slot);
        Ok(self.open_session())
    }

    /// Checks `password` and returns a fresh session token when it matches.
    pub async fn login(&self, password: String) -> Option<String> {
        if password.len() > MAX_PASSWORD_LEN {
            return None;
        }
        let hash = self
            .password_hash
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()?;
        let valid = self
            .run_kdf(move || Ok(verify_password(&password, &hash)))
            .await
            .unwrap_or(false);
        valid.then(|| self.open_session())
    }

    /// True when `token` belongs to a live session.
    pub fn validate(&self, token: &str) -> bool {
        let key = session_key(token);
        let mut sessions = self.lock_sessions();
        match sessions.get(&key) {
            Some(expires) if *expires > Instant::now() => true,
            Some(_) => {
                sessions.remove(&key);
                false
            }
            None => false,
        }
    }

    /// Ends the session behind `token`.
    pub fn logout(&self, token: &str) {
        self.lock_sessions().remove(&session_key(token));
    }

    fn open_session(&self) -> String {
        let mut raw = [0u8; 32];
        rand::Rng::fill(&mut rand::rng(), &mut raw);
        let token = hex::encode(raw);
        let now = Instant::now();
        let mut sessions = self.lock_sessions();
        sessions.retain(|_, expires| *expires > now);
        sessions.insert(session_key(&token), now + SESSION_TTL);
        token
    }

    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], Instant>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs a password hash off the async runtime, bounded in parallelism.
    async fn run_kdf<T, F>(&self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        let _permit = self.kdf.acquire().await.context("kdf semaphore closed")?;
        tokio::task::spawn_blocking(work)
            .await
            .context("password hashing task failed")?
    }
}

/// Hashes `password` with Argon2id and a random salt, as a PHC string.
pub fn hash_password(password: &str) -> Result<String> {
    let mut salt = [0u8; 16];
    rand::Rng::fill(&mut rand::rng(), &mut salt);
    let salt = SaltString::encode_b64(&salt).map_err(|error| anyhow!("{error}"))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| anyhow!("hashing the admin password: {error}"))?
        .to_string())
}

/// Verifies `password` against a PHC string produced by [`hash_password`].
pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

fn session_key(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// The session token carried by the request's cookies, if any.
pub fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.to_string())
}

/// `Set-Cookie` value that installs `token`.
pub fn session_cookie(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={}; HttpOnly; Secure; SameSite=Strict",
        SESSION_TTL.as_secs()
    )
}

/// `Set-Cookie` value that deletes the session cookie.
pub fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Strict")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> AdminAuth {
        let store = Arc::new(Store::open(":memory:").unwrap());
        store.ensure_instance("s", &"f".repeat(64)).unwrap();
        AdminAuth::load(store).unwrap()
    }

    #[test]
    fn hashes_verify_and_are_salted() {
        let first = hash_password("correct horse").unwrap();
        let second = hash_password("correct horse").unwrap();
        assert!(first.starts_with("$argon2id$"));
        assert_ne!(first, second, "salts must differ");
        assert!(verify_password("correct horse", &first));
        assert!(!verify_password("wrong horse", &first));
        assert!(!verify_password("correct horse", "not a hash"));
    }

    #[tokio::test]
    async fn claim_is_one_shot_and_logs_in() {
        let auth = auth();
        assert!(!auth.is_claimed());
        assert!(matches!(
            auth.claim("short".into()).await,
            Err(ClaimError::InvalidPassword(_))
        ));
        let token = auth.claim("first password".into()).await.unwrap();
        assert!(auth.validate(&token));
        assert!(auth.is_claimed());
        assert!(matches!(
            auth.claim("second password".into()).await,
            Err(ClaimError::AlreadyClaimed)
        ));
        // The first password still logs in; the second never took effect.
        assert!(auth.login("first password".into()).await.is_some());
        assert!(auth.login("second password".into()).await.is_none());
    }

    #[tokio::test]
    async fn a_claim_lost_in_the_database_refreshes_memory() {
        let auth = auth();
        // Another process (or a racing request) claimed through the database.
        auth.store
            .claim_admin(&hash_password("winner password").unwrap())
            .unwrap();
        assert!(matches!(
            auth.claim("loser password".into()).await,
            Err(ClaimError::AlreadyClaimed)
        ));
        assert!(auth.is_claimed());
        assert!(auth.login("winner password".into()).await.is_some());
    }

    #[tokio::test]
    async fn logout_ends_the_session() {
        let auth = auth();
        let token = auth.claim("first password".into()).await.unwrap();
        auth.logout(&token);
        assert!(!auth.validate(&token));
        assert!(!auth.validate("made-up"));
    }

    #[test]
    fn cookie_roundtrip() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("other=1; {SESSION_COOKIE}=abc123").parse().unwrap(),
        );
        assert_eq!(session_token(&headers).as_deref(), Some("abc123"));
        let cookie = session_cookie("abc123");
        for attribute in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/"] {
            assert!(cookie.contains(attribute), "missing {attribute}");
        }
    }
}
