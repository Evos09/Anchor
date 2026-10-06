//! Policy file format (TOML, ADR-0015) and rule matching.

use std::collections::HashSet;
use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::{Operation, PermissionRequest, Tool};

pub const POLICY_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Allow,
    Prompt,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Workspace,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("policy is not valid UTF-8")]
    NotUtf8,
    #[error("policy parse error: {0}")]
    Parse(String),
    #[error("unsupported policy version {0} (expected {POLICY_VERSION})")]
    Version(u32),
    #[error("rule #{index}: {msg}")]
    Rule { index: usize, msg: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    version: u32,
    default: DefaultSection,
    #[serde(default, rename = "rule")]
    rules: Vec<RuleDef>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultSection {
    action: Action,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleDef {
    name: String,
    action: Action,
    #[serde(rename = "match")]
    matcher: MatchDef,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchDef {
    tool: Option<Vec<Tool>>,
    operation: Option<Vec<Operation>>,
    target_within: Option<Scope>,
    target_outside: Option<Scope>,
    target_matches: Option<Vec<String>>,
    target_exists: Option<bool>,
    append_only: Option<bool>,
    network: Option<bool>,
    /// File names of resolved programs (for `run_command`).
    program: Option<Vec<String>>,
    min_blast_radius: Option<u32>,
}

/// A compiled rule. All specified conditions must hold (logical AND).
#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub action: Action,
    tools: Option<Vec<Tool>>,
    operations: Option<Vec<Operation>>,
    target_within: Option<Scope>,
    target_outside: Option<Scope>,
    target_matches: Option<GlobSet>,
    target_exists: Option<bool>,
    append_only: Option<bool>,
    network: Option<bool>,
    programs: Option<Vec<String>>,
    min_blast_radius: Option<u32>,
}

impl Rule {
    pub fn matches(&self, req: &PermissionRequest) -> bool {
        if let Some(t) = &self.tools
            && !t.contains(&req.tool)
        {
            return false;
        }
        if let Some(o) = &self.operations
            && !o.contains(&req.operation)
        {
            return false;
        }
        let target = req.canonical_target.as_deref();
        if let Some(Scope::Workspace) = self.target_within {
            match target {
                Some(t) if t.starts_with(&req.workspace_root) => {}
                _ => return false,
            }
        }
        if let Some(Scope::Workspace) = self.target_outside {
            match target {
                Some(t) if !t.starts_with(&req.workspace_root) => {}
                _ => return false,
            }
        }
        if let Some(set) = &self.target_matches {
            match target {
                Some(t) if set.is_match(t) => {}
                _ => return false,
            }
        }
        if let Some(e) = self.target_exists
            && req.target_exists != e
        {
            return false;
        }
        if let Some(a) = self.append_only
            && req.is_append_only != a
        {
            return false;
        }
        if let Some(n) = self.network
            && req.network != n
        {
            return false;
        }
        if let Some(progs) = &self.programs {
            if req.tool != Tool::RunCommand {
                return false;
            }
            let name = target.and_then(|t| t.file_name()).and_then(|n| n.to_str());
            match name {
                Some(n) if progs.iter().any(|p| p == n) => {}
                _ => return false,
            }
        }
        if let Some(min) = self.min_blast_radius
            && req.blast_radius_count < min
        {
            return false;
        }
        true
    }
}

#[derive(Debug, Clone)]
pub struct Policy {
    default: Action,
    rules: Vec<Rule>,
}

impl Policy {
    pub fn default_action(&self) -> Action {
        self.default
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Parse and compile a policy. `home` is used to expand a leading `~/`
    /// in `target_matches` patterns.
    pub fn parse(text: &str, home: &Path) -> Result<Self, PolicyError> {
        let file: PolicyFile =
            toml::from_str(text).map_err(|e| PolicyError::Parse(e.to_string()))?;
        if file.version != POLICY_VERSION {
            return Err(PolicyError::Version(file.version));
        }
        let mut names = HashSet::new();
        let mut rules = Vec::with_capacity(file.rules.len());
        for (index, def) in file.rules.into_iter().enumerate() {
            let err = |msg: String| PolicyError::Rule { index, msg };
            if def.name.trim().is_empty() {
                return Err(err("rule name is empty".into()));
            }
            if !names.insert(def.name.clone()) {
                return Err(err(format!("duplicate rule name {:?}", def.name)));
            }
            let m = def.matcher;
            if m.target_within.is_some() && m.target_outside.is_some() {
                return Err(err(
                    "target_within and target_outside are mutually exclusive".into(),
                ));
            }
            let target_matches = match m.target_matches {
                None => None,
                Some(pats) => {
                    if pats.is_empty() {
                        return Err(err("target_matches is empty".into()));
                    }
                    let mut b = GlobSetBuilder::new();
                    for p in &pats {
                        let expanded = expand_home(p, home);
                        let g = GlobBuilder::new(&expanded)
                            // `*` crosses directory separators: a deny rule
                            // for `~/.ssh/*` must cover nested files too.
                            .literal_separator(false)
                            .case_insensitive(cfg!(any(target_os = "macos", windows)))
                            .build()
                            .map_err(|e| err(format!("bad glob {p:?}: {e}")))?;
                        b.add(g);
                    }
                    Some(b.build().map_err(|e| err(e.to_string()))?)
                }
            };
            for (field, empty) in [
                ("tool", m.tool.as_ref().is_some_and(|v| v.is_empty())),
                (
                    "operation",
                    m.operation.as_ref().is_some_and(|v| v.is_empty()),
                ),
                ("program", m.program.as_ref().is_some_and(|v| v.is_empty())),
            ] {
                if empty {
                    return Err(err(format!("{field} list is empty")));
                }
            }
            rules.push(Rule {
                name: def.name,
                action: def.action,
                tools: m.tool,
                operations: m.operation,
                target_within: m.target_within,
                target_outside: m.target_outside,
                target_matches,
                target_exists: m.target_exists,
                append_only: m.append_only,
                network: m.network,
                programs: m.program,
                min_blast_radius: m.min_blast_radius,
            });
        }
        Ok(Policy {
            default: file.default.action,
            rules,
        })
    }
}

fn expand_home(pattern: &str, home: &Path) -> String {
    if let Some(rest) = pattern.strip_prefix("~/") {
        // globset matches against `/`-separated candidates on every
        // platform, so a Windows home (`C:\\Users\\x`) must use `/` too, or
        // `~/.ssh/**` would never match there.
        let h = home.to_string_lossy().replace('\\', "/");
        let h = h.trim_end_matches('/');
        format!("{h}/{rest}")
    } else {
        pattern.to_string()
    }
}
