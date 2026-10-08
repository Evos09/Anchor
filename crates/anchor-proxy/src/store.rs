//! The proxy's database (SQLite). What it holds is exactly: accounts
//! (a random id, the hash of the access token, the hash of the device
//! id, the creation time, the cap counters), each key's sliding-window
//! state, the pool's spend, and the metadata log ([`crate::log`]).
//!
//! No method takes request or response content: the type signatures are
//! the guarantee that none is written (Update #1 §4.3), and the proxy's
//! tests scan the database files for a request's text to check it.

use std::path::Path as FsPath;

use rusqlite::{Connection, OptionalExtension, params};

use crate::caps::UserCaps;
use crate::log::{FIELDS, LogRecord};
use crate::pool::KeyState;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("proxy database: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("proxy database: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct Store {
    db: Connection,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Store")
    }
}

/// A registered free-tier account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub user_id: String,
    pub created_ms: u64,
    pub caps: UserCaps,
}

impl Store {
    pub fn open(path: &FsPath) -> Result<Store, StoreError> {
        let db = Connection::open(path)?;
        Self::init(db)
    }

    pub fn memory() -> Result<Store, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(db: Connection) -> Result<Store, StoreError> {
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS users (
                 user_id TEXT PRIMARY KEY,
                 token_sha256 TEXT NOT NULL UNIQUE,
                 device_sha256 TEXT NOT NULL UNIQUE,
                 created_ms INTEGER NOT NULL,
                 caps TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS keys (
                 provider TEXT PRIMARY KEY,
                 state TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS pool (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 state TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS log (
                 user_id TEXT NOT NULL,
                 timestamp_ms INTEGER NOT NULL,
                 provider TEXT NOT NULL,
                 model TEXT NOT NULL,
                 input_tokens INTEGER NOT NULL,
                 output_tokens INTEGER NOT NULL,
                 http_status INTEGER NOT NULL,
                 latency_ms INTEGER NOT NULL,
                 path TEXT NOT NULL
             );",
        )?;
        Ok(Store { db })
    }

    /// The `log` table's columns, in order.
    pub fn log_columns(&self) -> Result<Vec<String>, StoreError> {
        let mut st = self
            .db
            .prepare("SELECT name FROM pragma_table_info('log') ORDER BY cid")?;
        let cols = st
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        debug_assert_eq!(cols, FIELDS);
        Ok(cols)
    }

    pub fn account_by_device(&self, device_sha256: &str) -> Result<Option<Account>, StoreError> {
        self.account_where("device_sha256", device_sha256)
    }

    pub fn account_by_token(&self, token_sha256: &str) -> Result<Option<Account>, StoreError> {
        self.account_where("token_sha256", token_sha256)
    }

    fn account_where(&self, col: &str, v: &str) -> Result<Option<Account>, StoreError> {
        let sql = format!("SELECT user_id, created_ms, caps FROM users WHERE {col} = ?1");
        let row = self
            .db
            .query_row(&sql, params![v], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .optional()?;
        row.map(|(user_id, created, caps)| {
            Ok(Account {
                user_id,
                created_ms: created as u64,
                caps: serde_json::from_str(&caps)?,
            })
        })
        .transpose()
    }

    pub fn insert_account(
        &self,
        a: &Account,
        token_sha256: &str,
        device_sha256: &str,
    ) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT INTO users (user_id, token_sha256, device_sha256, created_ms, caps)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                a.user_id,
                token_sha256,
                device_sha256,
                a.created_ms as i64,
                serde_json::to_string(&a.caps)?
            ],
        )?;
        Ok(())
    }

    pub fn rotate_token(&self, user_id: &str, token_sha256: &str) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE users SET token_sha256 = ?2 WHERE user_id = ?1",
            params![user_id, token_sha256],
        )?;
        Ok(())
    }

    pub fn save_caps(&self, user_id: &str, caps: &UserCaps) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE users SET caps = ?2 WHERE user_id = ?1",
            params![user_id, serde_json::to_string(caps)?],
        )?;
        Ok(())
    }

    pub fn all_caps(&self) -> Result<Vec<(String, UserCaps)>, StoreError> {
        let mut st = self.db.prepare("SELECT user_id, caps FROM users")?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(u, c)| Ok((u, serde_json::from_str(&c)?)))
            .collect()
    }

    pub fn save_key(&self, provider: &str, state: &KeyState) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT INTO keys (provider, state) VALUES (?1, ?2)
             ON CONFLICT(provider) DO UPDATE SET state = excluded.state",
            params![provider, serde_json::to_string(state)?],
        )?;
        Ok(())
    }

    pub fn load_key(&self, provider: &str) -> Result<Option<KeyState>, StoreError> {
        let s: Option<String> = self
            .db
            .query_row(
                "SELECT state FROM keys WHERE provider = ?1",
                params![provider],
                |r| r.get(0),
            )
            .optional()?;
        s.map(|s| Ok(serde_json::from_str(&s)?)).transpose()
    }

    pub fn save_pool(&self, state: &serde_json::Value) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT INTO pool (id, state) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET state = excluded.state",
            params![state.to_string()],
        )?;
        Ok(())
    }

    pub fn load_pool(&self) -> Result<Option<serde_json::Value>, StoreError> {
        let s: Option<String> = self
            .db
            .query_row("SELECT state FROM pool WHERE id = 1", [], |r| r.get(0))
            .optional()?;
        s.map(|s| Ok(serde_json::from_str(&s)?)).transpose()
    }

    /// Append one metadata record: the only per-request write.
    pub fn log(&self, r: &LogRecord) -> Result<(), StoreError> {
        let path = serde_json::to_value(r.path)?;
        self.db.execute(
            "INSERT INTO log (user_id, timestamp_ms, provider, model, input_tokens,
                              output_tokens, http_status, latency_ms, path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                r.user_id,
                r.timestamp_ms as i64,
                r.provider,
                r.model,
                r.input_tokens as i64,
                r.output_tokens as i64,
                i64::from(r.http_status),
                r.latency_ms as i64,
                path.as_str().unwrap_or("A"),
            ],
        )?;
        Ok(())
    }

    pub fn log_count(&self) -> Result<u64, StoreError> {
        Ok(self
            .db
            .query_row("SELECT COUNT(*) FROM log", [], |r| r.get::<_, i64>(0))? as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_table_has_exactly_the_metadata_columns() {
        let s = Store::memory().unwrap();
        assert_eq!(s.log_columns().unwrap(), FIELDS);
    }

    #[test]
    fn accounts_round_trip() {
        let s = Store::memory().unwrap();
        let a = Account {
            user_id: "u1".into(),
            created_ms: 5,
            caps: UserCaps::new(5),
        };
        s.insert_account(&a, "tok", "dev").unwrap();
        assert_eq!(s.account_by_device("dev").unwrap(), Some(a.clone()));
        assert_eq!(s.account_by_token("tok").unwrap(), Some(a.clone()));
        s.rotate_token("u1", "tok2").unwrap();
        assert_eq!(s.account_by_token("tok").unwrap(), None);
        assert!(s.account_by_token("tok2").unwrap().is_some());
        // One account per device.
        assert!(
            s.insert_account(
                &Account {
                    user_id: "u2".into(),
                    ..a
                },
                "t3",
                "dev"
            )
            .is_err()
        );
    }
}
