//! Persistent state of a Waystone instance.
//!
//! Everything the server must remember lives in a single SQLite file next to the
//! TLS material: the instance identity, the pinned certificate fingerprint, the
//! device pool and the single use pairing tokens.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use waystone_proto::device::{DeviceId, DeviceInfo, Platform, Role};
use waystone_proto::identity::Identity;

/// How long a pairing token stays valid.
pub const PAIRING_TOKEN_TTL: Duration = Duration::from_secs(600);

/// A device that belongs to the private pool.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredDevice {
    pub info: DeviceInfo,
    pub created_at: u64,
    pub last_seen_at: Option<u64>,
}

/// A one time enrollment token.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredToken {
    pub token: String,
    pub role: Role,
    pub expires_at: u64,
    pub used_at: Option<u64>,
}

/// Handle to the instance database.
///
/// The connection is behind a mutex because `rusqlite` connections are not `Sync`;
/// statements are short so the lock is never held across an await point.
pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    /// Opens (or creates) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let connection =
            Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        // Write ahead logging keeps the daemon responsive while a phone syncs.
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        // Several short lived connections may race during startup and tests; a
        // busy timeout turns a transient lock into a short wait instead of an
        // error.
        connection.busy_timeout(Duration::from_secs(5))?;
        let store = Store {
            connection: Mutex::new(connection),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Default database path under `data_dir`.
    pub fn default_path(data_dir: impl AsRef<Path>) -> PathBuf {
        data_dir.as_ref().join("waystone.sqlite3")
    }

    /// Locks the connection for one short statement.
    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn migrate(&self) -> Result<()> {
        self.lock().execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS instance (
                id               INTEGER PRIMARY KEY CHECK (id = 1),
                server_id        TEXT NOT NULL,
                fingerprint      TEXT NOT NULL,
                admin_token_hash TEXT,
                created_at       INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS devices (
                id           TEXT PRIMARY KEY,
                name         TEXT NOT NULL,
                platform     TEXT NOT NULL,
                role         TEXT NOT NULL,
                public_key   TEXT NOT NULL,
                created_at   INTEGER NOT NULL,
                last_seen_at INTEGER
            );

            CREATE TABLE IF NOT EXISTS pairing_tokens (
                token      TEXT PRIMARY KEY,
                role       TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                used_at    INTEGER
            );
            "#,
        )?;
        Ok(())
    }

    /// Creates the instance row on first boot and returns the stored values.
    pub fn ensure_instance(&mut self, server_id: &str, fingerprint: &str) -> Result<()> {
        let now = unix_now();
        self.lock().execute(
            "INSERT OR IGNORE INTO instance (id, server_id, fingerprint, created_at)
             VALUES (1, ?1, ?2, ?3)",
            params![server_id, fingerprint, now],
        )?;
        // A certificate reissue keeps the instance id but must update the pin.
        self.lock().execute(
            "UPDATE instance SET fingerprint = ?1 WHERE id = 1",
            params![fingerprint],
        )?;
        Ok(())
    }

    /// The instance id used inside challenge signatures.
    pub fn server_id(&self) -> Result<String> {
        self.lock()
            .query_row("SELECT server_id FROM instance WHERE id = 1", [], |row| {
                row.get::<_, String>(0)
            })
            .context("instance row is missing")
    }

    /// The pinned certificate fingerprint.
    pub fn fingerprint(&self) -> Result<String> {
        self.lock()
            .query_row("SELECT fingerprint FROM instance WHERE id = 1", [], |row| {
                row.get::<_, String>(0)
            })
            .context("instance row is missing")
    }

    /// Hashes and stores the admin password. Only the hash is persisted.
    pub fn set_admin_password_hash(&self, hash: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE instance SET admin_token_hash = ?1 WHERE id = 1",
            params![hash],
        )?;
        Ok(())
    }

    /// The stored admin password hash, when the instance is already claimed.
    pub fn admin_password_hash(&self) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT admin_token_hash FROM instance WHERE id = 1",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Creates a pairing token for a device that is about to enroll.
    pub fn create_pairing_token(&self, role: Role, ttl: Duration) -> Result<StoredToken> {
        // 128 bits of entropy, hex encoded, is what the pairing URI carries.
        let mut raw = [0u8; 16];
        rand::Rng::fill(&mut rand::rng(), &mut raw);
        let token = hex::encode(raw);
        let expires_at = unix_now() + ttl.as_secs();
        self.lock().execute(
            "INSERT INTO pairing_tokens (token, role, expires_at) VALUES (?1, ?2, ?3)",
            params![token, role.to_string(), expires_at as i64],
        )?;
        Ok(StoredToken {
            token,
            role,
            expires_at,
            used_at: None,
        })
    }

    /// Validates and consumes a pairing token.
    ///
    /// Returns the role the token was issued for, or `None` when the token is
    /// unknown, expired or already used.
    pub fn consume_pairing_token(&self, token: &str) -> Result<Option<Role>> {
        let now = unix_now();
        let row: Option<(String, i64, Option<i64>)> = self
            .lock()
            .query_row(
                "SELECT role, expires_at, used_at FROM pairing_tokens WHERE token = ?1",
                params![token],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        let Some((role, expires_at, used_at)) = row else {
            return Ok(None);
        };
        if used_at.is_some() || (expires_at as u64) <= now {
            return Ok(None);
        }
        self.lock().execute(
            "UPDATE pairing_tokens SET used_at = ?1 WHERE token = ?2",
            params![now as i64, token],
        )?;
        Ok(Some(role.parse().unwrap_or(Role::Controller)))
    }

    /// Inserts or refreshes a device.
    pub fn upsert_device(&self, info: &DeviceInfo) -> Result<()> {
        let now = unix_now();
        self.lock().execute(
            "INSERT INTO devices (id, name, platform, role, public_key, created_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                platform = excluded.platform,
                role = excluded.role,
                public_key = excluded.public_key,
                last_seen_at = excluded.last_seen_at",
            params![
                info.id.as_str(),
                info.name,
                info.platform.to_string(),
                info.role.to_string(),
                info.public_key,
                now as i64
            ],
        )?;
        Ok(())
    }

    /// Records that a device just connected.
    pub fn touch_device(&self, id: &DeviceId) -> Result<()> {
        self.lock().execute(
            "UPDATE devices SET last_seen_at = ?1 WHERE id = ?2",
            params![unix_now() as i64, id.as_str()],
        )?;
        Ok(())
    }

    /// Looks up one device.
    pub fn device(&self, id: &DeviceId) -> Result<Option<StoredDevice>> {
        let row = self
            .lock()
            .query_row(
                "SELECT id, name, platform, role, public_key, created_at, last_seen_at
                 FROM devices WHERE id = ?1",
                params![id.as_str()],
                map_device,
            )
            .optional()?;
        Ok(row)
    }

    /// Every device in the pool.
    pub fn devices(&self) -> Result<Vec<StoredDevice>> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT id, name, platform, role, public_key, created_at, last_seen_at
             FROM devices ORDER BY created_at",
        )?;
        let rows = statement.query_map([], map_device)?;
        let mut devices = Vec::new();
        for row in rows {
            devices.push(row?);
        }
        Ok(devices)
    }

    /// Removes a device from the pool.
    pub fn remove_device(&self, id: &DeviceId) -> Result<bool> {
        let affected = self
            .lock()
            .execute("DELETE FROM devices WHERE id = ?1", params![id.as_str()])?;
        Ok(affected > 0)
    }

    /// The identity of a device, ready for challenge verification.
    pub fn identity(&self, id: &DeviceId) -> Result<Option<Identity>> {
        Ok(self.device(id)?.map(|device| Identity {
            id: device.info.id.clone(),
            public_key: device.info.public_key.clone(),
        }))
    }

    /// Number of devices, used to render the admin summary.
    pub fn device_count(&self) -> Result<i64> {
        Ok(self
            .lock()
            .query_row("SELECT COUNT(*) FROM devices", [], |row| row.get(0))?)
    }

    /// Prunes expired pairing tokens; called on a timer.
    pub fn prune_tokens(&self) -> Result<usize> {
        let now = unix_now() as i64;
        let removed = self.lock().execute(
            "DELETE FROM pairing_tokens WHERE expires_at < ?1 OR used_at IS NOT NULL",
            params![now],
        )?;
        Ok(removed)
    }
}

fn map_device(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredDevice> {
    let platform: String = row.get(2)?;
    let role: String = row.get(3)?;
    let created_at: i64 = row.get(5)?;
    let last_seen_at: Option<i64> = row.get(6)?;
    Ok(StoredDevice {
        info: DeviceInfo {
            id: DeviceId::new(row.get::<_, String>(0)?),
            name: row.get(1)?,
            platform: platform.parse().unwrap_or(Platform::Other),
            role: role.parse().unwrap_or(Role::Controller),
            public_key: row.get(4)?,
        },
        created_at: created_at as u64,
        last_seen_at: last_seen_at.map(|value| value as u64),
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, role: Role) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::new(format!("id-{name}")),
            name: name.to_string(),
            platform: Platform::Macos,
            role,
            public_key: waystone_proto::identity::encode_public_key(&[3u8; 32]),
        }
    }

    fn store() -> Store {
        Store::open(":memory:").expect("in memory store")
    }

    #[test]
    fn instance_identity_is_created_once() {
        let mut store = store();
        store.ensure_instance("server-a", &"a".repeat(64)).unwrap();
        assert_eq!(store.server_id().unwrap(), "server-a");

        // Reissuing the certificate updates only the fingerprint.
        store.ensure_instance("server-a", &"b".repeat(64)).unwrap();
        assert_eq!(store.server_id().unwrap(), "server-a");
        assert_eq!(store.fingerprint().unwrap(), "b".repeat(64));
    }

    #[test]
    fn adming_password_hash_roundtrip() {
        let mut store = store();
        store.ensure_instance("s", &"f".repeat(64)).unwrap();
        assert!(store.admin_password_hash().unwrap().is_none());
        store.set_admin_password_hash("hash").unwrap();
        assert_eq!(
            store.admin_password_hash().unwrap().as_deref(),
            Some("hash")
        );
    }

    #[test]
    fn pairing_token_is_single_use() {
        let store = store();
        let token = store
            .create_pairing_token(Role::Controller, Duration::from_secs(60))
            .unwrap();
        assert_eq!(token.token.len(), 32);
        assert_eq!(
            store.consume_pairing_token(&token.token).unwrap(),
            Some(Role::Controller)
        );
        // Second use is rejected.
        assert_eq!(store.consume_pairing_token(&token.token).unwrap(), None);
    }

    #[test]
    fn expired_token_is_rejected() {
        let store = store();
        let token = store
            .create_pairing_token(Role::Host, Duration::from_secs(0))
            .unwrap();
        assert_eq!(store.consume_pairing_token(&token.token).unwrap(), None);
    }

    #[test]
    fn devices_roundtrip() {
        let store = store();
        store.upsert_device(&device("mac", Role::Host)).unwrap();
        store
            .upsert_device(&device("phone", Role::Controller))
            .unwrap();
        assert_eq!(store.device_count().unwrap(), 2);

        let mac = store.device(&DeviceId::new("id-mac")).unwrap().unwrap();
        assert_eq!(mac.info.role, Role::Host);

        // Upsert refreshes the name without duplicating the row.
        let mut renamed = device("mac", Role::Host);
        renamed.name = "Mac Studio".into();
        store.upsert_device(&renamed).unwrap();
        assert_eq!(store.device_count().unwrap(), 2);
        assert_eq!(
            store
                .device(&DeviceId::new("id-mac"))
                .unwrap()
                .unwrap()
                .info
                .name,
            "Mac Studio"
        );
        assert!(store.remove_device(&DeviceId::new("id-mac")).unwrap());
        assert!(!store.remove_device(&DeviceId::new("id-mac")).unwrap());
    }
}
