//! Role configuration (`config/roles.toml`, spec §9.1). No model name is
//! hardcoded anywhere in the code; everything comes from this file.

use std::collections::BTreeMap;

use anchor_protocol::roles::{
    ProviderView, RoleView, RolesView, Route, RoutingView, shared_family,
};
use serde::Deserialize;

pub const ROLES: [&str; 4] = ["planner", "executor", "tester", "reviewer"];

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Any server speaking the OpenAI-compatible chat completions API
    /// (hosted frontier APIs via BYOK, or local inference servers).
    OpenaiCompatible,
    /// Scripted responses supplied by the backend (tests and demos).
    Mock,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    #[serde(default)]
    pub base_url: String,
    /// Name of the environment variable holding the API key (BYOK). The key
    /// itself never appears in config files.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Model family, used to enforce reviewer != executor family.
    pub family: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    300
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Params {
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleConfig {
    pub provider: String,
    pub model_id: String,
    pub context_window: u64,
    #[serde(default)]
    pub params: Params,
    /// Overrides the provider's family for this role.
    #[serde(default)]
    pub family: Option<String>,
    /// USD per million input / output tokens, as configured by the user.
    #[serde(default)]
    pub price_in_per_mtok: f64,
    #[serde(default)]
    pub price_out_per_mtok: f64,
    /// Providers to try, in order, if `provider` fails. Every fallback is
    /// announced to the user.
    #[serde(default)]
    pub fallback: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
    #[serde(default = "executor")]
    pub mechanical: String,
    #[serde(default = "planner")]
    pub architecture: String,
    #[serde(default = "executor")]
    pub agentic: String,
    #[serde(default = "reviewer")]
    pub review: String,
}

fn executor() -> String {
    "executor".into()
}
fn planner() -> String {
    "planner".into()
}
fn reviewer() -> String {
    "reviewer".into()
}

impl Default for RoutingConfig {
    fn default() -> Self {
        RoutingConfig {
            mechanical: executor(),
            architecture: planner(),
            agentic: executor(),
            review: reviewer(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolesConfig {
    pub providers: BTreeMap<String, ProviderConfig>,
    pub roles: BTreeMap<String, RoleConfig>,
    #[serde(default)]
    pub routing: RoutingConfig,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("roles config parse error: {0}")]
    Parse(String),
    #[error("missing role [{0}]")]
    MissingRole(String),
    #[error("unknown role {0:?}")]
    UnknownRole(String),
    #[error("role {role} references unknown provider {provider:?}")]
    UnknownProvider { role: String, provider: String },
    #[error("{0}")]
    Invalid(String),
    #[error(
        "executor and reviewer resolve to the same model family ({0:?}); same-family review is \
         theater (spec §8.2(a)). Configure a reviewer from a different family."
    )]
    SameFamilyReview(String),
}

impl RolesConfig {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let cfg: RolesConfig =
            toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn family_of(&self, role: &str) -> Option<String> {
        self.view().role_family(role)
    }

    /// Families of a role's primary provider and all its fallbacks.
    pub fn families_of(&self, role: &str) -> Vec<String> {
        self.view().role_families(role)
    }

    /// The shared family-resolution view (anchor-protocol), so the backend
    /// and the runtime resolve families identically.
    fn view(&self) -> RolesView {
        RolesView {
            providers: self
                .providers
                .iter()
                .map(|(k, p)| {
                    (
                        k.clone(),
                        ProviderView {
                            family: p.family.clone(),
                        },
                    )
                })
                .collect(),
            roles: self
                .roles
                .iter()
                .map(|(k, r)| {
                    (
                        k.clone(),
                        RoleView {
                            provider: r.provider.clone(),
                            model_id: r.model_id.clone(),
                            family: r.family.clone(),
                            fallback: r.fallback.clone(),
                            price_in_per_mtok: r.price_in_per_mtok,
                            price_out_per_mtok: r.price_out_per_mtok,
                        },
                    )
                })
                .collect(),
            routing: RoutingView {
                mechanical: Some(self.routing.mechanical.clone()),
                architecture: Some(self.routing.architecture.clone()),
                agentic: Some(self.routing.agentic.clone()),
                review: Some(self.routing.review.clone()),
            },
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        for r in ROLES {
            if !self.roles.contains_key(r) {
                return Err(ConfigError::MissingRole(r.into()));
            }
        }
        for (name, role) in &self.roles {
            if !ROLES.contains(&name.as_str()) {
                return Err(ConfigError::UnknownRole(name.clone()));
            }
            for p in std::iter::once(&role.provider).chain(role.fallback.iter()) {
                if !self.providers.contains_key(p) {
                    return Err(ConfigError::UnknownProvider {
                        role: name.clone(),
                        provider: p.clone(),
                    });
                }
            }
            if role.model_id.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "role {name}: model_id is empty"
                )));
            }
            if role.context_window == 0 {
                return Err(ConfigError::Invalid(format!(
                    "role {name}: context_window is 0"
                )));
            }
            if !(role.price_in_per_mtok >= 0.0 && role.price_out_per_mtok >= 0.0) {
                return Err(ConfigError::Invalid(format!("role {name}: negative price")));
            }
        }
        for (name, p) in &self.providers {
            if p.family.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "provider {name}: family is empty"
                )));
            }
            if p.kind == ProviderKind::OpenaiCompatible
                && !(p.base_url.starts_with("http://") || p.base_url.starts_with("https://"))
            {
                return Err(ConfigError::Invalid(format!(
                    "provider {name}: base_url must be http(s)"
                )));
            }
        }
        for target in [
            &self.routing.mechanical,
            &self.routing.architecture,
            &self.routing.agentic,
            &self.routing.review,
        ] {
            if !self.roles.contains_key(target) {
                return Err(ConfigError::UnknownRole(target.clone()));
            }
        }
        // Every family the executor can run on (including fallbacks) must
        // differ from every family the reviewer can run on: by role name,
        // and by what the agentic and review routes actually resolve to.
        let v = self.view();
        for (exec, rev) in [
            (v.role_families("executor"), v.role_families("reviewer")),
            (v.families(Route::Agentic), v.families(Route::Review)),
        ] {
            if let Some(shared) = shared_family(&exec, &rev) {
                return Err(ConfigError::SameFamilyReview(shared));
            }
        }
        Ok(())
    }
}
