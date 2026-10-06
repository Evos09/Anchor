//! Model-family resolution shared by the backend and the runtime (spec
//! §8.2(a), ADR-0024). A route's families are its role's primary family
//! plus the family of every fallback provider: a reviewer that could fall
//! back into the executor's family is as bad as one that starts there.
//!
//! [`RolesView`] reads only what family resolution needs from a roles file
//! and ignores the rest; the runtime's `RolesConfig` remains the strict
//! parser. Callers deserialize it from TOML themselves (this crate has no
//! TOML dependency).

use std::collections::BTreeMap;

use serde::Deserialize;

/// The routing kinds of a roles file (`[routing]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Mechanical,
    Architecture,
    Agentic,
    Review,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RolesView {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderView>,
    #[serde(default)]
    pub roles: BTreeMap<String, RoleView>,
    #[serde(default)]
    pub routing: RoutingView,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProviderView {
    #[serde(default)]
    pub family: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoleView {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model_id: String,
    pub family: Option<String>,
    #[serde(default)]
    pub fallback: Vec<String>,
    /// USD per million input / output tokens (the runtime's cost basis).
    #[serde(default)]
    pub price_in_per_mtok: f64,
    #[serde(default)]
    pub price_out_per_mtok: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoutingView {
    pub mechanical: Option<String>,
    pub architecture: Option<String>,
    pub agentic: Option<String>,
    pub review: Option<String>,
}

/// What a route resolves to: the role, its model and its primary family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteIdentity {
    pub role: String,
    pub model_id: String,
    pub family: String,
}

/// Families compare trimmed and case-insensitively.
pub fn normalize_family(f: &str) -> String {
    f.trim().to_ascii_lowercase()
}

/// The first family present in both sets, if any.
pub fn shared_family(a: &[String], b: &[String]) -> Option<String> {
    a.iter().find(|f| b.contains(f)).cloned()
}

impl RolesView {
    /// The role a route uses (the defaults match the runtime's).
    pub fn role_for(&self, r: Route) -> &str {
        let (set, default) = match r {
            Route::Mechanical => (&self.routing.mechanical, "executor"),
            Route::Architecture => (&self.routing.architecture, "planner"),
            Route::Agentic => (&self.routing.agentic, "executor"),
            Route::Review => (&self.routing.review, "reviewer"),
        };
        set.as_deref().unwrap_or(default)
    }

    /// A role's primary family (its own override, else its provider's).
    pub fn role_family(&self, role: &str) -> Option<String> {
        let r = self.roles.get(role)?;
        let f = match &r.family {
            Some(f) => f.clone(),
            None => self.providers.get(&r.provider)?.family.clone(),
        };
        let f = normalize_family(&f);
        (!f.is_empty()).then_some(f)
    }

    /// Every family a role can run on: primary first, then fallbacks.
    pub fn role_families(&self, role: &str) -> Vec<String> {
        let Some(r) = self.roles.get(role) else {
            return Vec::new();
        };
        let mut out: Vec<String> = self.role_family(role).into_iter().collect();
        for p in &r.fallback {
            if let Some(pc) = self.providers.get(p) {
                let f = normalize_family(&pc.family);
                if !f.is_empty() && !out.contains(&f) {
                    out.push(f);
                }
            }
        }
        out
    }

    /// Each role's `(price_in, price_out)` in USD per million tokens: what
    /// the user configured, against which reported costs are checked.
    pub fn prices(&self) -> BTreeMap<String, (f64, f64)> {
        self.roles
            .iter()
            .map(|(n, r)| (n.clone(), (r.price_in_per_mtok, r.price_out_per_mtok)))
            .collect()
    }

    /// Every family a route can run on.
    pub fn families(&self, r: Route) -> Vec<String> {
        self.role_families(self.role_for(r))
    }

    pub fn identity(&self, r: Route) -> Result<RouteIdentity, String> {
        let role = self.role_for(r);
        let rv = self
            .roles
            .get(role)
            .ok_or_else(|| format!("roles: no [roles.{role}]"))?;
        if rv.model_id.trim().is_empty() {
            return Err(format!("roles: [roles.{role}] has no model_id"));
        }
        let family = self
            .role_family(role)
            .ok_or_else(|| format!("roles: [roles.{role}] has no resolvable family"))?;
        Ok(RouteIdentity {
            role: role.to_string(),
            model_id: rv.model_id.clone(),
            family,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(v: serde_json::Value) -> RolesView {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn families_follow_routes_and_fallbacks() {
        let v = view(serde_json::json!({
            "providers": {
                "a": {"family": " Fam-A ", "kind": "mock"},
                "b": {"family": "fam-b"},
                "c": {"family": "fam-c"}
            },
            "roles": {
                "executor": {"provider": "a", "model_id": "e", "fallback": ["c"]},
                "reviewer": {"provider": "b", "model_id": "r"},
                "planner": {"provider": "b", "model_id": "p", "family": "fam-x"}
            },
            "routing": {"agentic": "executor"}
        }));
        // Unknown keys (kind) are ignored: this is a view, not the parser.
        assert_eq!(v.families(Route::Agentic), ["fam-a", "fam-c"]);
        assert_eq!(v.families(Route::Review), ["fam-b"]);
        assert_eq!(v.role_family("planner").as_deref(), Some("fam-x"));
        assert_eq!(
            shared_family(&v.families(Route::Agentic), &v.families(Route::Review)),
            None
        );
        let id = v.identity(Route::Agentic).unwrap();
        assert_eq!(
            (id.role.as_str(), id.family.as_str()),
            ("executor", "fam-a")
        );
        // A reviewer whose fallback is in the executor's family overlaps.
        let v = view(serde_json::json!({
            "providers": {"a": {"family": "fam-a"}, "b": {"family": "fam-b"}},
            "roles": {
                "executor": {"provider": "a", "model_id": "e"},
                "reviewer": {"provider": "b", "model_id": "r", "fallback": ["a"]}
            }
        }));
        assert_eq!(
            shared_family(&v.families(Route::Agentic), &v.families(Route::Review)),
            Some("fam-a".into())
        );
        assert!(v.identity(Route::Architecture).is_err());
    }
}
