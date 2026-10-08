//! The proxy's configuration (`proxy.toml`), checked before it serves:
//!
//! * exactly one account per provider (ADR-0032): a second account on a
//!   provider to stretch its free tier breaks most providers' terms;
//! * every provider in the pool has a provider ADR recording that its
//!   terms were read and permit this use, with the date
//!   (`Verdict: INCLUDE`, `Terms verified: YYYY-MM-DD`);
//! * every provider is in the model registry and serves path A.

use std::collections::BTreeSet;
use std::path::{Path as FsPath, PathBuf};

use anchor_inference::{Path, Registry};
use serde::{Deserialize, Serialize};

use crate::caps::CapConfig;
use crate::ledger::LedgerConfig;
use crate::pool::{Limits, PoolConfig};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    /// A provider id from the model registry.
    pub provider: String,
    /// The environment variable holding this account's API key.
    pub key_env: String,
    /// The provider ADR (relative to the config file).
    pub adr: PathBuf,
    /// The free tier's documented limits, as recorded in the ADR.
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    /// Loopback port; a TLS-terminating reverse proxy sits in front.
    pub port: u16,
    /// The public host name requests must carry in `Host`.
    pub public_host: String,
    /// The model registry (relative to the config file).
    pub registry: PathBuf,
    /// The SQLite database (relative to the config file).
    pub database: PathBuf,
    #[serde(default)]
    pub caps: CapConfig,
    #[serde(default)]
    pub ledger: LedgerConfig,
    pub pool: PoolConfig,
    #[serde(default, rename = "account")]
    pub accounts: Vec<Account>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ConfigError {
    #[error("proxy config: {0}")]
    Invalid(String),
}

fn invalid(s: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(s.into())
}

/// What a provider ADR must say for the provider to be in the pool.
pub fn check_provider_adr(text: &str) -> Result<String, String> {
    let field = |name: &str| {
        text.lines()
            .filter_map(|l| {
                l.trim()
                    .trim_start_matches(['-', '*', ' '])
                    .strip_prefix(name)
            })
            .map(|v| v.trim().trim_matches('*').trim().to_string())
            .next()
    };
    let verdict = field("Verdict:").ok_or("no `Verdict:` line")?;
    if verdict != "INCLUDE" {
        return Err(format!("its verdict is {verdict:?}, not INCLUDE"));
    }
    let date = field("Terms verified:").ok_or("no `Terms verified:` line")?;
    let b = date.as_bytes();
    let dated = b.len() >= 10
        && b[..10].iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        });
    if !dated {
        return Err(format!("`Terms verified: {date}` is not a YYYY-MM-DD date"));
    }
    Ok(date[..10].to_string())
}

impl ProxyConfig {
    pub fn from_toml(text: &str) -> Result<ProxyConfig, ConfigError> {
        toml::from_str(text).map_err(|e| invalid(e.to_string()))
    }

    /// Check the config against the registry and the provider ADRs (paths
    /// relative to `base`).
    pub fn validate(&self, registry: &Registry, base: &FsPath) -> Result<(), ConfigError> {
        let mut seen = BTreeSet::new();
        for a in &self.accounts {
            if !seen.insert(a.provider.as_str()) {
                return Err(invalid(format!(
                    "two accounts for provider {:?}: exactly one account per provider is allowed \
                     (add a provider for capacity, never a second account)",
                    a.provider
                )));
            }
            let Some(p) = registry.provider(&a.provider) else {
                return Err(invalid(format!(
                    "account provider {:?} is not in the registry",
                    a.provider
                )));
            };
            if !p.paths.contains(&Path::A) {
                return Err(invalid(format!(
                    "provider {:?} does not serve the free pool (path A)",
                    a.provider
                )));
            }
            if a.key_env.is_empty() {
                return Err(invalid(format!(
                    "account {:?}: key_env is empty",
                    a.provider
                )));
            }
            let adr = base.join(&a.adr);
            let text = std::fs::read_to_string(&adr).map_err(|e| {
                invalid(format!(
                    "provider {:?} has no readable terms ADR at {}: {e}",
                    a.provider,
                    adr.display()
                ))
            })?;
            check_provider_adr(&text).map_err(|why| {
                invalid(format!(
                    "provider {:?} is not cleared by {}: {why}",
                    a.provider,
                    adr.display()
                ))
            })?;
        }
        let mut chain = BTreeSet::new();
        for c in &self.pool.chain {
            if !chain.insert(c.as_str()) {
                return Err(invalid(format!("{c:?} appears twice in the pool chain")));
            }
            if !seen.contains(c.as_str()) {
                return Err(invalid(format!(
                    "pool chain names {c:?}, which has no account"
                )));
            }
        }
        if !(0.0..=1.0).contains(&self.pool.saturation) || self.pool.daily_cap == 0 {
            return Err(invalid(
                "pool.saturation must be in 0..=1 and pool.daily_cap > 0",
            ));
        }
        if !(0.0..=10.0).contains(&self.ledger.overrun_margin) {
            return Err(invalid("ledger.overrun_margin must be in 0..=10"));
        }
        if self.public_host.is_empty() {
            return Err(invalid("public_host is required"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry::from_toml(
            r#"
[[provider]]
id = "alpha"
display_name = "Alpha"
base_url = "https://alpha.example/v1"
paths = ["A", "B"]
family = "pool-family"

[[provider]]
id = "beta"
display_name = "Beta"
base_url = "https://beta.example/v1"
paths = ["B"]
family = "pool-family"
"#,
        )
        .unwrap()
    }

    fn cfg(accounts: &str) -> ProxyConfig {
        ProxyConfig::from_toml(&format!(
            r#"
port = 8787
public_host = "free.example"
registry = "models.toml"
database = "proxy.db"
[pool]
chain = ["alpha"]
daily_cap = 1000000
{accounts}
"#
        ))
        .unwrap()
    }

    const ALPHA: &str = "[[account]]\nprovider = \"alpha\"\nkey_env = \"K\"\nadr = \"alpha.md\"\n";

    fn adr_dir(verdict: &str) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("alpha.md"),
            format!("# Alpha\n\n- Verdict: {verdict}\n- Terms verified: 2026-10-08\n"),
        )
        .unwrap();
        d
    }

    #[test]
    fn one_account_per_provider() {
        let d = adr_dir("INCLUDE");
        assert_eq!(cfg(ALPHA).validate(&registry(), d.path()), Ok(()));
        let e = cfg(&format!("{ALPHA}{}", ALPHA.replace("\"K\"", "\"K2\"")))
            .validate(&registry(), d.path())
            .unwrap_err();
        assert!(
            e.to_string().contains("exactly one account per provider"),
            "{e}"
        );
    }

    #[test]
    fn every_provider_needs_a_terms_adr_that_clears_it() {
        let d = tempfile::tempdir().unwrap();
        let e = cfg(ALPHA).validate(&registry(), d.path()).unwrap_err();
        assert!(e.to_string().contains("no readable terms ADR"), "{e}");
        for (verdict, why) in [("EXCLUDE", "not INCLUDE"), ("AMBIGUOUS", "not INCLUDE")] {
            let d = adr_dir(verdict);
            let e = cfg(ALPHA).validate(&registry(), d.path()).unwrap_err();
            assert!(e.to_string().contains(why), "{e}");
        }
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("alpha.md"), "- Verdict: INCLUDE\n").unwrap();
        let e = cfg(ALPHA).validate(&registry(), d.path()).unwrap_err();
        assert!(e.to_string().contains("Terms verified"), "{e}");
    }

    #[test]
    fn pool_providers_must_serve_path_a_and_have_accounts() {
        let d = adr_dir("INCLUDE");
        let beta = ALPHA.replace("alpha\"\nkey", "beta\"\nkey");
        let e = cfg(&beta).validate(&registry(), d.path()).unwrap_err();
        assert!(e.to_string().contains("path A"), "{e}");
        let mut c = cfg(ALPHA);
        c.pool.chain.push("gamma".into());
        assert!(c.validate(&registry(), d.path()).is_err());
    }

    #[test]
    fn the_adr_fields_are_read_as_written() {
        assert_eq!(
            check_provider_adr("- **Verdict:** INCLUDE\n- **Terms verified:** 2026-10-08\n"),
            Ok("2026-10-08".into())
        );
        assert!(check_provider_adr("Verdict: INCLUDE\nTerms verified: last week\n").is_err());
    }

    /// The shipped provider ADRs (docs/adr/providers) all parse, are dated,
    /// and (as of 2026-10-08) none clears the gate: the free tier ships off.
    #[test]
    fn no_shipped_provider_adr_clears_the_gate_yet() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/adr/providers");
        let mut n = 0;
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.file_name().and_then(|f| f.to_str()) == Some("README.md") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            assert!(
                text.contains("- **Terms verified:** 2026-"),
                "{}",
                p.display()
            );
            let why = check_provider_adr(&text).unwrap_err();
            assert!(why.contains("not INCLUDE"), "{}: {why}", p.display());
            n += 1;
        }
        assert!(n >= 10, "{n} provider ADRs");
    }

    /// The shipped example config is valid, with no accounts.
    #[test]
    fn the_example_config_is_valid_and_has_no_accounts() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
        let cfg = ProxyConfig::from_toml(
            &std::fs::read_to_string(dir.join("proxy.example.toml")).unwrap(),
        )
        .unwrap();
        let reg = Registry::from_toml(&std::fs::read_to_string(dir.join(&cfg.registry)).unwrap())
            .unwrap();
        assert_eq!(cfg.validate(&reg, &dir), Ok(()));
        assert!(cfg.accounts.is_empty());
    }
}
