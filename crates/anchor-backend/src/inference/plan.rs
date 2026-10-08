//! Turning a routing decision into the runtime's models file (roles
//! TOML) for one task: an executor on the chosen path and a reviewer of a
//! **different model family** (spec §9.1; the rule is the runtime's and
//! the verify gate's, and this only picks models that satisfy it).
//!
//! Secrets never go in the file: each keyed provider names an
//! environment variable, and the values travel separately
//! ([`SecretEnv`]) into the runtime's environment.

use anchor_inference::Path;

use super::keys::SecretEnv;

/// Context the runtime is told it has for a local model: Ollama's own
/// default context is smaller than most models' maximum, and a runtime
/// that believes in more than the server keeps gets silently truncated.
pub const LOCAL_CONTEXT_CAP: u64 = 32_768;

/// One model a task could run on.
#[derive(Clone, PartialEq)]
pub struct Candidate {
    pub path: Path,
    /// Where the label came from: registry id, or the provider's own id.
    pub label: String,
    pub base_url: String,
    pub model_id: String,
    pub family: String,
    pub context_window: u64,
    pub price_in: f64,
    pub price_out: f64,
    /// The bearer key (BYOK) or free-tier token, if the endpoint needs one.
    pub key: Option<String>,
}

impl std::fmt::Debug for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Candidate")
            .field("path", &self.path)
            .field("model_id", &self.model_id)
            .field("family", &self.family)
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The models a task will use, and how to give them to the runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub path: Path,
    pub executor: Candidate,
    pub reviewer: Candidate,
    pub roles_toml: String,
    pub env: SecretEnv,
}

/// Pick the executor (the preferred model if it is a candidate, else the
/// first) and a reviewer of another family: first from the same path,
/// then from `others` (other available paths, in routing order).
pub fn choose(
    path: Path,
    on_path: &[Candidate],
    preferred: Option<&str>,
    others: &[Candidate],
) -> Result<(Candidate, Candidate), String> {
    let exec = preferred
        .and_then(|p| on_path.iter().find(|c| c.model_id == p))
        .or_else(|| on_path.first())
        .ok_or_else(|| format!("no model is ready on {}", path.describe()))?;
    let reviewer = on_path
        .iter()
        .chain(others)
        .find(|c| c.family != exec.family)
        .ok_or_else(|| {
            format!(
                "the reviewer must be a different model family from the executor ({}), and no \
                 other family is available. Add one: install a second local model, add an API \
                 key, or use the free tier.",
                exec.family
            )
        })?;
    Ok((exec.clone(), reviewer.clone()))
}

/// The roles TOML for an executor and reviewer.
pub fn roles(path: Path, exec: &Candidate, reviewer: &Candidate) -> Plan {
    let mut env = Vec::new();
    let mut providers = toml::Table::new();
    let mut provider = |name: &str, c: &Candidate, env: &mut Vec<(String, String)>| {
        let mut p = toml::Table::new();
        p.insert("kind".into(), "openai_compatible".into());
        p.insert("base_url".into(), c.base_url.clone().into());
        p.insert("family".into(), c.family.clone().into());
        if let Some(k) = &c.key {
            let var = format!("ANCHOR_KEY_{}", name.to_ascii_uppercase());
            p.insert("api_key_env".into(), var.clone().into());
            env.push((var, k.clone()));
        }
        providers.insert(name.into(), p.into());
    };
    provider("executor", exec, &mut env);
    provider("reviewer", reviewer, &mut env);
    let role = |provider: &str, c: &Candidate| {
        let mut r = toml::Table::new();
        r.insert("provider".into(), provider.into());
        r.insert("model_id".into(), c.model_id.clone().into());
        r.insert(
            "context_window".into(),
            toml::Value::Integer(c.context_window as i64),
        );
        r.insert("price_in_per_mtok".into(), toml::Value::Float(c.price_in));
        r.insert("price_out_per_mtok".into(), toml::Value::Float(c.price_out));
        toml::Value::Table(r)
    };
    let mut roles = toml::Table::new();
    for name in ["planner", "executor", "tester"] {
        roles.insert(name.into(), role("executor", exec));
    }
    roles.insert("reviewer".into(), role("reviewer", reviewer));
    let mut routing = toml::Table::new();
    for (k, v) in [
        ("mechanical", "executor"),
        ("architecture", "planner"),
        ("agentic", "executor"),
        ("review", "reviewer"),
    ] {
        routing.insert(k.into(), v.into());
    }
    let mut t = toml::Table::new();
    t.insert("providers".into(), providers.into());
    t.insert("roles".into(), roles.into());
    t.insert("routing".into(), routing.into());
    Plan {
        path,
        executor: exec.clone(),
        reviewer: reviewer.clone(),
        roles_toml: format!(
            "# Generated by the Anchor app for one task ({}).\n{}",
            path.describe(),
            toml::to_string(&t).unwrap_or_default()
        ),
        env: SecretEnv(env),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(path: Path, model: &str, family: &str, key: Option<&str>) -> Candidate {
        Candidate {
            path,
            label: model.into(),
            base_url: "http://127.0.0.1:11434/v1".into(),
            model_id: model.into(),
            family: family.into(),
            context_window: 32768,
            price_in: 0.0,
            price_out: 0.0,
            key: key.map(str::to_string),
        }
    }

    #[test]
    fn the_reviewer_is_always_another_family() {
        let local = [
            c(Path::C, "q7", "qwen", None),
            c(Path::C, "q14", "qwen", None),
            c(Path::C, "l8", "llama", None),
        ];
        let (e, r) = choose(Path::C, &local, None, &[]).unwrap();
        assert_eq!((e.model_id.as_str(), r.model_id.as_str()), ("q7", "l8"));
        // The preferred executor wins when present.
        let (e, r) = choose(Path::C, &local, Some("l8"), &[]).unwrap();
        assert_eq!((e.model_id.as_str(), r.model_id.as_str()), ("l8", "q7"));
        // One family alone cannot run a task: the reviewer comes from
        // another path, or the user is told what to add.
        let one = [c(Path::C, "q7", "qwen", None)];
        let byok = [c(Path::B, "gpt", "openai", Some("sk-x"))];
        let (_, r) = choose(Path::C, &one, None, &byok).unwrap();
        assert_eq!(r.path, Path::B);
        let e = choose(Path::C, &one, None, &[]).unwrap_err();
        assert!(e.contains("different model family"), "{e}");
    }

    #[test]
    fn the_roles_file_carries_no_secret_and_the_runtime_accepts_it() {
        let exec = c(Path::B, "gpt-x", "openai", Some("sk-live-secret"));
        let rev = c(Path::C, "qwen2.5-coder:7b", "qwen", None);
        let plan = roles(Path::B, &exec, &rev);
        assert!(!plan.roles_toml.contains("sk-live-secret"));
        assert_eq!(
            plan.env.0,
            [(
                "ANCHOR_KEY_EXECUTOR".to_string(),
                "sk-live-secret".to_string()
            )]
        );
        assert!(!format!("{plan:?}").contains("sk-live-secret"));
        let t: toml::Table = toml::from_str(&plan.roles_toml).unwrap();
        assert_eq!(
            t["providers"]["executor"]["api_key_env"].as_str(),
            Some("ANCHOR_KEY_EXECUTOR")
        );
        assert!(t["providers"]["reviewer"].get("api_key_env").is_none());
        assert_eq!(
            t["roles"]["reviewer"]["model_id"].as_str(),
            Some("qwen2.5-coder:7b")
        );
        // The app's own check (the runtime validates the rest).
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("r.toml");
        std::fs::write(&f, &plan.roles_toml).unwrap();
        assert_eq!(crate::ui::app::check_models(&f), Ok(()));
    }
}
