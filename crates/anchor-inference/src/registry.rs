//! The model registry (Update #1 §6, ADR-0030): which models exist, on
//! which providers and paths, under which license, and what a free-pool
//! token of each costs the vendor.
//!
//! The registry is data (`config/models.toml`), checked when it loads:
//! a model may be offered on the free pool only if its license permits
//! commercial use without a revenue or user-count gate. Eligibility is
//! decided from the license fields alone, never from a model's name: the
//! same family ships under different licenses at different sizes.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The three inference paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Path {
    /// The vendor's capped free pool (`anchor-proxy`).
    A,
    /// The user's own API key, straight to the provider.
    B,
    /// A local model through Ollama.
    C,
}

impl Path {
    pub fn describe(self) -> &'static str {
        match self {
            Path::A => "the free tier",
            Path::B => "your own API key",
            Path::C => "a local model",
        }
    }
}

/// A provider a model is served by.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub id: String,
    pub display_name: String,
    /// The OpenAI-compatible base URL (`…/v1`).
    pub base_url: String,
    /// Paths this provider can be used on.
    pub paths: Vec<Path>,
    /// Where a user gets a key (BYOK).
    #[serde(default)]
    pub key_url: String,
    /// The provider's API terms, as read when the entry was verified.
    #[serde(default)]
    pub terms_url: String,
    /// For a provider serving one model family (a lab's own API): the
    /// family of any of its models the registry does not list, so a BYOK
    /// model picked from the provider's live list still obeys the
    /// reviewer-is-another-family rule. Required for path B.
    #[serde(default)]
    pub family: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    pub id: String,
    pub display_name: String,
    pub provider: String,
    pub provider_model_id: String,
    /// The model family, for the reviewer-is-another-family rule (§9.1).
    pub family: String,
    /// Entries naming the same weights on different providers share this
    /// key, so the free pool can fall over between them.
    #[serde(default)]
    pub same_as: String,
    pub context_window: u64,
    /// USD per million tokens (BYOK cost shown to the user).
    #[serde(default)]
    pub input_price: f64,
    #[serde(default)]
    pub output_price: f64,
    /// SPDX identifier, or the license's own name (e.g. "Llama-3.1-Community").
    pub license_id: String,
    pub license_commercial_ok: bool,
    /// A revenue, user-count or similar gate on commercial use.
    pub license_revenue_gate: bool,
    #[serde(default)]
    pub license_notes: String,
    /// Weighted tokens charged per token on the free pool.
    #[serde(default)]
    pub free_tier_weight: f64,
    pub free_tier_eligible: bool,
    pub available_on_paths: Vec<Path>,
    /// When and by whom the license and availability were checked.
    pub verified_at: String,
    pub verified_by: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    #[serde(default, rename = "provider")]
    pub providers: Vec<ProviderEntry>,
    #[serde(default, rename = "model")]
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum RegistryError {
    #[error("the model registry is not valid TOML: {0}")]
    Parse(String),
    #[error("model {id}: {why}")]
    Model { id: String, why: String },
    #[error("provider {id}: {why}")]
    Provider { id: String, why: String },
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

impl Registry {
    /// Parse and validate. The app and the proxy refuse to start with a
    /// registry that fails here.
    pub fn from_toml(text: &str) -> Result<Registry, RegistryError> {
        let r: Registry = toml::from_str(text).map_err(|e| RegistryError::Parse(e.to_string()))?;
        r.validate()?;
        Ok(r)
    }

    pub fn validate(&self) -> Result<(), RegistryError> {
        let mut ids = BTreeSet::new();
        for p in &self.providers {
            let bad = |why: &str| RegistryError::Provider {
                id: p.id.clone(),
                why: why.to_string(),
            };
            if p.id.is_empty() || !ids.insert(format!("provider:{}", p.id)) {
                return Err(bad("empty or duplicate id"));
            }
            if !(p.base_url.starts_with("https://") || p.base_url.starts_with("http://127.0.0.1")) {
                return Err(bad("base_url must be https (or loopback for local)"));
            }
            if p.paths.is_empty() {
                return Err(bad("no paths"));
            }
            if p.paths.contains(&Path::B) && p.family.is_empty() {
                return Err(bad("a BYOK (path B) provider needs a family"));
            }
        }
        for m in &self.models {
            let bad = |why: &str| RegistryError::Model {
                id: m.id.clone(),
                why: why.to_string(),
            };
            if m.id.is_empty() || !ids.insert(format!("model:{}", m.id)) {
                return Err(bad("empty or duplicate id"));
            }
            let Some(provider) = self.providers.iter().find(|p| p.id == m.provider) else {
                return Err(bad("unknown provider"));
            };
            if m.available_on_paths.is_empty() {
                return Err(bad("available on no path"));
            }
            if let Some(p) = m
                .available_on_paths
                .iter()
                .find(|p| !provider.paths.contains(p))
            {
                return Err(bad(&format!("path {p:?} is not one its provider serves")));
            }
            if m.family.is_empty() || m.license_id.is_empty() || m.provider_model_id.is_empty() {
                return Err(bad("family, license_id and provider_model_id are required"));
            }
            if !is_date(&m.verified_at) || m.verified_by.is_empty() {
                return Err(bad("verified_at (YYYY-MM-DD) and verified_by are required"));
            }
            for (what, v) in [
                ("free_tier_weight", m.free_tier_weight),
                ("input_price", m.input_price),
                ("output_price", m.output_price),
            ] {
                if !v.is_finite() || v < 0.0 {
                    return Err(bad(&format!("{what} must be a finite number >= 0")));
                }
            }
            let on_pool = m.available_on_paths.contains(&Path::A);
            if m.free_tier_eligible {
                if !m.license_commercial_ok {
                    return Err(bad(
                        "free-tier eligible, but its license does not permit commercial use",
                    ));
                }
                if m.license_revenue_gate {
                    return Err(bad(
                        "free-tier eligible, but its license has a revenue or user-count gate",
                    ));
                }
                if m.free_tier_weight <= 0.0 {
                    return Err(bad("free-tier eligible models need a free_tier_weight > 0"));
                }
                if !on_pool {
                    return Err(bad("free-tier eligible, but not available on path A"));
                }
            } else if on_pool {
                return Err(bad("available on path A, but not free-tier eligible"));
            }
        }
        for m in self.models.iter().filter(|m| !m.same_as.is_empty()) {
            for o in self.models.iter().filter(|o| o.same_as == m.same_as) {
                if o.family != m.family || o.license_id != m.license_id {
                    return Err(RegistryError::Model {
                        id: o.id.clone(),
                        why: format!(
                            "same_as {:?} groups it with {}, but family or license differ",
                            m.same_as, m.id
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// The free-tier entries a request for `id` may be served by: the
    /// entry itself and those naming the same weights elsewhere.
    pub fn free_tier_equivalents(&self, id: &str) -> Vec<&ModelEntry> {
        let Some(m) = self.free_tier().find(|m| m.id == id) else {
            return Vec::new();
        };
        self.free_tier()
            .filter(|o| o.id == m.id || (!m.same_as.is_empty() && o.same_as == m.same_as))
            .collect()
    }

    pub fn model(&self, id: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|m| m.id == id)
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderEntry> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// The entry for a provider's own model id, by exact match.
    pub fn by_provider_model(
        &self,
        provider: &str,
        provider_model_id: &str,
    ) -> Option<&ModelEntry> {
        self.models
            .iter()
            .find(|m| m.provider == provider && m.provider_model_id == provider_model_id)
    }

    /// Models offered on `path`.
    pub fn on_path(&self, path: Path) -> impl Iterator<Item = &ModelEntry> {
        self.models
            .iter()
            .filter(move |m| m.available_on_paths.contains(&path))
    }

    /// Models the free pool may serve: chosen by their license fields,
    /// which [`Registry::validate`] has checked.
    pub fn free_tier(&self) -> impl Iterator<Item = &ModelEntry> {
        self.models
            .iter()
            .filter(|m| m.free_tier_eligible && m.license_commercial_ok && !m.license_revenue_gate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> String {
        r#"
[[provider]]
id = "pool"
display_name = "Pool"
base_url = "https://pool.example/v1"
paths = ["A", "B"]
family = "pool-family"

[[provider]]
id = "ollama"
display_name = "Ollama"
base_url = "http://127.0.0.1:11434/v1"
paths = ["C"]
"#
        .to_string()
    }

    fn model(
        id: &str,
        license: &str,
        commercial: bool,
        gate: bool,
        eligible: bool,
        paths: &str,
    ) -> String {
        format!(
            r#"
[[model]]
id = "{id}"
display_name = "{id}"
provider = "pool"
provider_model_id = "{id}"
family = "f-{id}"
context_window = 8192
license_id = "{license}"
license_commercial_ok = {commercial}
license_revenue_gate = {gate}
free_tier_weight = 1.0
free_tier_eligible = {eligible}
available_on_paths = {paths}
verified_at = "2026-10-08"
verified_by = "test"
"#
        )
    }

    #[test]
    fn a_permissive_model_may_be_on_the_free_tier() {
        let r = Registry::from_toml(
            &(base() + &model("m1", "Apache-2.0", true, false, true, r#"["A","B"]"#)),
        )
        .unwrap();
        assert_eq!(r.free_tier().count(), 1);
        assert_eq!(r.on_path(Path::B).count(), 1);
    }

    #[test]
    fn the_registry_refuses_a_free_tier_model_without_commercial_use() {
        let e = Registry::from_toml(
            &(base() + &model("nc", "CC-BY-NC-4.0", false, false, true, r#"["A"]"#)),
        )
        .unwrap_err();
        assert!(
            e.to_string().contains("does not permit commercial use"),
            "{e}"
        );
    }

    #[test]
    fn the_registry_refuses_a_free_tier_model_with_a_revenue_gate() {
        let e = Registry::from_toml(
            &(base() + &model("gated", "Llama-3.1-Community", true, true, true, r#"["A"]"#)),
        )
        .unwrap_err();
        assert!(e.to_string().contains("revenue or user-count gate"), "{e}");
    }

    #[test]
    fn a_byok_provider_needs_a_family() {
        let text = base().replace("family = \"pool-family\"\n", "");
        let e = Registry::from_toml(&text).unwrap_err();
        assert!(e.to_string().contains("needs a family"), "{e}");
    }

    #[test]
    fn a_gated_model_is_fine_off_the_free_tier() {
        let r = Registry::from_toml(
            &(base()
                + &model(
                    "gated",
                    "Llama-3.1-Community",
                    true,
                    true,
                    false,
                    r#"["B"]"#,
                )),
        )
        .unwrap();
        assert_eq!(r.free_tier().count(), 0);
        let e = Registry::from_toml(
            &(base()
                + &model(
                    "sneaky",
                    "Llama-3.1-Community",
                    true,
                    true,
                    false,
                    r#"["A","B"]"#,
                )),
        )
        .unwrap_err();
        assert!(e.to_string().contains("not free-tier eligible"), "{e}");
    }

    /// Eligibility follows the license fields, whatever the name says.
    #[test]
    fn selection_is_by_license_not_by_name() {
        let r = Registry::from_toml(
            &(base()
                + &model(
                    "apache-free-open",
                    "Llama-3.1-Community",
                    true,
                    true,
                    false,
                    r#"["B"]"#,
                )
                + &model(
                    "llama-gated-looking",
                    "Apache-2.0",
                    true,
                    false,
                    true,
                    r#"["A"]"#,
                )),
        )
        .unwrap();
        let free: Vec<_> = r.free_tier().map(|m| m.id.as_str()).collect();
        assert_eq!(free, ["llama-gated-looking"]);
    }

    /// Code inspection: no selection code in this crate compares model
    /// names by pattern.
    #[test]
    fn no_name_pattern_matching_in_selection_code() {
        for (file, src) in [
            ("registry.rs", include_str!("registry.rs")),
            ("router.rs", include_str!("router.rs")),
        ] {
            let code = src.split("#[cfg(test)]").next().unwrap_or(src);
            for banned in [
                ".contains(\"",
                ".starts_with(\"",
                ".ends_with(\"",
                "Regex",
                ".find(\"",
            ] {
                let hits: Vec<_> = code
                    .lines()
                    .filter(|l| l.contains(banned) && !l.trim_start().starts_with("//"))
                    .filter(|l| !l.contains("base_url"))
                    .collect();
                assert!(hits.is_empty(), "{file} uses {banned}: {hits:?}");
            }
        }
    }

    #[test]
    fn equivalents_share_family_and_license() {
        let a = model("a", "Apache-2.0", true, false, true, r#"["A"]"#)
            .replace("verified_by", "same_as = \"w\"\nverified_by");
        let b = model("b", "Apache-2.0", true, false, true, r#"["A"]"#)
            .replace("verified_by", "same_as = \"w\"\nverified_by")
            .replace("family = \"f-b\"", "family = \"f-a\"");
        let r = Registry::from_toml(&(base() + &a + &b)).unwrap();
        let eq: Vec<_> = r
            .free_tier_equivalents("a")
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(eq, ["a", "b"]);
        let c = model("c", "Apache-2.0", true, false, true, r#"["A"]"#)
            .replace("verified_by", "same_as = \"w\"\nverified_by");
        assert!(
            Registry::from_toml(&(base() + &a + &c)).is_err(),
            "families differ"
        );
    }

    #[test]
    fn structural_mistakes_are_refused() {
        let unknown =
            model("x", "MIT", true, false, false, r#"["B"]"#).replace("\"pool\"", "\"nope\"");
        assert!(Registry::from_toml(&(base() + &unknown)).is_err());
        let dup = model("x", "MIT", true, false, false, r#"["B"]"#);
        assert!(Registry::from_toml(&(base() + &dup + &dup)).is_err());
        let wrong_path = model("x", "MIT", true, false, false, r#"["C"]"#);
        assert!(Registry::from_toml(&(base() + &wrong_path)).is_err());
        let undated =
            model("x", "MIT", true, false, false, r#"["B"]"#).replace("2026-10-08", "recently");
        assert!(Registry::from_toml(&(base() + &undated)).is_err());
        let insecure = base().replace("https://pool.example", "http://pool.example");
        assert!(Registry::from_toml(&insecure).is_err());
    }
}
