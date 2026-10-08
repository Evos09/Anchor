//! Where secrets live (Update #1 §2.3): the operating system's keychain
//! (macOS Keychain, Windows Credential Manager, the Secret Service on
//! Linux), through `keyring` (ADR-0029). Nothing else in Anchor stores a
//! key: `app.json` records only *which* providers have one.
//!
//! A key leaves the keychain for exactly two things: the verify call to
//! its own provider, and the environment of the runtime that calls that
//! provider. It is never logged, audited, shown, or sent to Anchor.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// The keychain service name every Anchor secret is filed under.
pub const SERVICE: &str = "dev.anchor.app";

/// The free tier's access token (Path A).
pub const FREE_TOKEN: &str = "free-tier-token";

/// The keychain entry for a provider's BYOK key.
pub fn byok_entry(provider: &str) -> String {
    format!("byok:{provider}")
}

/// A secret store. Implementations never put a secret in an error.
pub trait KeyStore: Send + Sync + std::fmt::Debug {
    fn set(&self, name: &str, secret: &str) -> Result<(), String>;
    fn get(&self, name: &str) -> Result<Option<String>, String>;
    /// Remove the entry; removing an absent entry is not an error.
    fn delete(&self, name: &str) -> Result<(), String>;
}

/// The operating system's keychain.
#[derive(Debug, Default)]
pub struct Keychain;

fn describe(e: keyring::Error) -> String {
    match e {
        keyring::Error::NoDefaultStore => "no system keychain is available (on Linux a Secret \
             Service, such as GNOME Keyring or KWallet, must be running)"
            .to_string(),
        keyring::Error::NoStorageAccess(_) => {
            "the system keychain refused access (is it locked?)".to_string()
        }
        // These carry the stored bytes: never format them.
        keyring::Error::BadEncoding(_) | keyring::Error::BadDataFormat(..) => {
            "the stored key is unreadable".to_string()
        }
        keyring::Error::Ambiguous(_) => {
            "the keychain holds more than one matching entry".to_string()
        }
        other => format!("keychain: {other}"),
    }
}

impl KeyStore for Keychain {
    fn set(&self, name: &str, secret: &str) -> Result<(), String> {
        keyring::Entry::new(SERVICE, name)
            .and_then(|e| e.set_password(secret))
            .map_err(describe)
    }

    fn get(&self, name: &str) -> Result<Option<String>, String> {
        let entry = keyring::Entry::new(SERVICE, name).map_err(describe)?;
        match entry.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(describe(e)),
        }
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, name).map_err(describe)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(describe(e)),
        }
    }
}

/// An in-memory store (tests, and `--keys memory` for demos).
#[derive(Debug, Default)]
pub struct MemoryKeys(Mutex<BTreeMap<String, String>>);

impl KeyStore for MemoryKeys {
    fn set(&self, name: &str, secret: &str) -> Result<(), String> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name.to_string(), secret.to_string());
        Ok(())
    }

    fn get(&self, name: &str) -> Result<Option<String>, String> {
        Ok(self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .cloned())
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(name);
        Ok(())
    }
}

/// Environment variables carrying secrets into the runtime. Its `Debug`
/// prints the names only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretEnv(pub Vec<(String, String)>);

impl std::fmt::Debug for SecretEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(k, _)| format!("{k}=<redacted>")))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(store: &dyn KeyStore, name: &str) {
        assert_eq!(store.get(name).unwrap(), None);
        store.set(name, "sk-test-secret").unwrap();
        assert_eq!(store.get(name).unwrap().as_deref(), Some("sk-test-secret"));
        store.set(name, "sk-test-other").unwrap();
        assert_eq!(store.get(name).unwrap().as_deref(), Some("sk-test-other"));
        // Removal wipes it, and removing again is fine.
        store.delete(name).unwrap();
        assert_eq!(store.get(name).unwrap(), None);
        store.delete(name).unwrap();
    }

    #[test]
    fn the_memory_store_sets_gets_and_wipes() {
        round_trip(&MemoryKeys::default(), "byok:alpha");
    }

    /// The real keychain, where CI has one (`ANCHOR_TEST_KEYCHAIN=1` on
    /// the macOS and Windows runners). Elsewhere a missing keychain must
    /// say so plainly instead of failing obscurely.
    #[test]
    fn the_system_keychain_sets_gets_and_wipes() {
        let name = format!("test:{}", std::process::id());
        if std::env::var_os("ANCHOR_TEST_KEYCHAIN").is_some() {
            round_trip(&Keychain, &name);
        } else if let Err(e) = Keychain.get(&name) {
            assert!(e.contains("keychain"), "{e}");
            assert!(!e.contains("sk-"), "{e}");
        }
    }

    #[test]
    fn secret_env_never_prints_values() {
        let e = SecretEnv(vec![("ANCHOR_KEY_EXECUTOR".into(), "sk-live-123".into())]);
        let s = format!("{e:?}");
        assert!(s.contains("ANCHOR_KEY_EXECUTOR"));
        assert!(!s.contains("sk-live-123"));
    }
}
