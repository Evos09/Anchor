//! Path selection (Update #1 §2, ADR-0029): which of the three paths a
//! task runs on, from the user's preference, a one-task override (`/local`,
//! `/byok`, `/free`), a per-project pin, and which paths are available
//! right now. When none is, the answer is a prompt for the user (add a
//! key, start Ollama, or wait), never an error.

use serde::{Deserialize, Serialize};

use crate::registry::Path;

/// The user's standing choice (Settings → Models).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preference {
    /// Simple tasks to the cheapest path, complex ones to the strongest.
    #[default]
    Auto,
    PreferLocal,
    PreferByok,
    FreeOnly,
}

/// A one-task override, from a slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Override {
    Local,
    Byok,
    Free,
}

impl Override {
    pub fn path(self) -> Path {
        match self {
            Override::Local => Path::C,
            Override::Byok => Path::B,
            Override::Free => Path::A,
        }
    }
}

/// The free pool's state, as the proxy reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FreeState {
    Available,
    /// A personal cap (5-hour, weekly, monthly) or the pool is used up.
    Exhausted {
        reason: String,
    },
    /// No free tier is configured or reachable.
    Unavailable {
        reason: String,
    },
}

/// What can serve a request right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Availability {
    /// Ollama answers on its port and has the models chosen.
    pub local: bool,
    /// A key is stored for the chosen provider and has not been refused
    /// for quota.
    pub byok: bool,
    pub free: FreeState,
}

impl Availability {
    fn has(&self, p: Path) -> bool {
        match p {
            Path::A => self.free == FreeState::Available,
            Path::B => self.byok,
            Path::C => self.local,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Complexity {
    Simple,
    Complex,
}

/// A rough read of how big a task is: long tasks, and tasks that ask for
/// structural work, go to the strongest path under `Auto`.
pub fn classify(task: &str) -> Complexity {
    const BIG: [&str; 9] = [
        "refactor",
        "implement",
        "migrate",
        "architecture",
        "redesign",
        "rewrite",
        "across the",
        "whole project",
        "entire",
    ];
    let lower = task.to_lowercase();
    if task.len() > 280 || BIG.iter().any(|w| lower.contains(w)) {
        Complexity::Complex
    } else {
        Complexity::Simple
    }
}

/// A leading `/local`, `/byok` or `/free`, and the task without it.
pub fn parse_override(task: &str) -> (Option<Override>, &str) {
    let t = task.trim_start();
    for (cmd, o) in [
        ("/local", Override::Local),
        ("/byok", Override::Byok),
        ("/free", Override::Free),
    ] {
        if let Some(rest) = t.strip_prefix(cmd)
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return (Some(o), rest.trim_start());
        }
    }
    (None, task)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromptReason {
    /// Nothing can serve the task: offer to add a key or start Ollama.
    NothingAvailable { free: FreeState },
    /// The task (or project) asks for one path and it is not available.
    PathUnavailable { path: Path, why: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Decision {
    /// Run on `path`. `skipped` lists the paths tried first and why.
    Use {
        path: Path,
        skipped: Vec<(Path, String)>,
    },
    /// Ask the user what to do.
    Prompt(PromptReason),
}

/// What one task asks of the router.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub preference: Preference,
    /// A custom fallback order replacing the preference's default.
    pub order: Option<Vec<Path>>,
    pub override_: Option<Override>,
    /// The project is pinned to one path (e.g. a private repo to local).
    pub pin: Option<Path>,
    pub complexity: Complexity,
}

/// The order paths are tried in for a preference.
pub fn default_order(pref: Preference, complexity: Complexity) -> Vec<Path> {
    match pref {
        Preference::PreferLocal => vec![Path::C, Path::B, Path::A],
        Preference::PreferByok => vec![Path::B, Path::A],
        Preference::FreeOnly => vec![Path::A],
        Preference::Auto => match complexity {
            // Cheapest first: local and free cost the user nothing.
            Complexity::Simple => vec![Path::C, Path::A, Path::B],
            // Strongest first: the user's own (frontier) key.
            Complexity::Complex => vec![Path::B, Path::A, Path::C],
        },
    }
}

fn why_not(p: Path, a: &Availability) -> String {
    match p {
        Path::A => match &a.free {
            FreeState::Available => "available".into(),
            FreeState::Exhausted { reason } | FreeState::Unavailable { reason } => reason.clone(),
        },
        Path::B => "no API key is set up (or it was refused for quota)".into(),
        Path::C => "Ollama is not running, or the chosen models are not installed".into(),
    }
}

pub fn route(req: &Request, avail: &Availability) -> Decision {
    // A pin or an override allows exactly one path.
    if let Some(only) = req.pin.or(req.override_.map(Override::path)) {
        return if avail.has(only) {
            Decision::Use {
                path: only,
                skipped: Vec::new(),
            }
        } else {
            Decision::Prompt(PromptReason::PathUnavailable {
                path: only,
                why: why_not(only, avail),
            })
        };
    }
    let order = match (&req.order, req.preference) {
        // "Free only" never touches the user's key or local models.
        (_, Preference::FreeOnly) => vec![Path::A],
        (Some(o), _) if !o.is_empty() => o.clone(),
        _ => default_order(req.preference, req.complexity),
    };
    let mut skipped = Vec::new();
    for p in order {
        if avail.has(p) {
            return Decision::Use { path: p, skipped };
        }
        skipped.push((p, why_not(p, avail)));
    }
    Decision::Prompt(PromptReason::NothingAvailable {
        free: avail.free.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avail(local: bool, byok: bool, free: bool) -> Availability {
        Availability {
            local,
            byok,
            free: if free {
                FreeState::Available
            } else {
                FreeState::Exhausted {
                    reason: "the 5-hour limit is reached".into(),
                }
            },
        }
    }

    fn req(preference: Preference) -> Request {
        Request {
            preference,
            order: None,
            override_: None,
            pin: None,
            complexity: Complexity::Simple,
        }
    }

    fn used(d: &Decision) -> Option<Path> {
        match d {
            Decision::Use { path, .. } => Some(*path),
            Decision::Prompt(_) => None,
        }
    }

    #[test]
    fn prefer_local_uses_ollama_when_it_runs() {
        assert_eq!(
            used(&route(
                &req(Preference::PreferLocal),
                &avail(true, true, true)
            )),
            Some(Path::C)
        );
    }

    #[test]
    fn falls_back_to_byok_when_ollama_is_unreachable() {
        let d = route(&req(Preference::PreferLocal), &avail(false, true, true));
        assert_eq!(used(&d), Some(Path::B));
        let Decision::Use { skipped, .. } = d else {
            unreachable!()
        };
        assert_eq!(skipped[0].0, Path::C);
    }

    #[test]
    fn falls_back_to_the_free_pool_when_byok_is_exhausted() {
        assert_eq!(
            used(&route(
                &req(Preference::PreferByok),
                &avail(false, false, true)
            )),
            Some(Path::A)
        );
        assert_eq!(
            used(&route(
                &req(Preference::PreferLocal),
                &avail(false, false, true)
            )),
            Some(Path::A)
        );
    }

    #[test]
    fn prompts_when_nothing_is_available() {
        for pref in [
            Preference::Auto,
            Preference::PreferLocal,
            Preference::PreferByok,
            Preference::FreeOnly,
        ] {
            let d = route(&req(pref), &avail(false, false, false));
            assert!(
                matches!(d, Decision::Prompt(PromptReason::NothingAvailable { .. })),
                "{pref:?}: {d:?}"
            );
        }
    }

    #[test]
    fn free_only_never_touches_byok_or_local() {
        let d = route(&req(Preference::FreeOnly), &avail(true, true, false));
        assert!(matches!(d, Decision::Prompt(_)), "{d:?}");
        let mut r = req(Preference::FreeOnly);
        r.order = Some(vec![Path::C, Path::B]);
        assert!(matches!(
            route(&r, &avail(true, true, false)),
            Decision::Prompt(_)
        ));
    }

    #[test]
    fn a_slash_command_overrides_the_setting_for_one_task() {
        let (o, rest) = parse_override("/local explain main.rs");
        assert_eq!((o, rest), (Some(Override::Local), "explain main.rs"));
        let mut r = req(Preference::FreeOnly);
        r.override_ = o;
        assert_eq!(used(&route(&r, &avail(true, true, true))), Some(Path::C));
        // The next task, without the command, follows the setting again.
        assert_eq!(
            used(&route(&req(Preference::FreeOnly), &avail(true, true, true))),
            Some(Path::A)
        );
        // An unavailable override prompts instead of falling back.
        let mut r = req(Preference::Auto);
        r.override_ = Some(Override::Byok);
        assert!(matches!(
            route(&r, &avail(true, false, true)),
            Decision::Prompt(PromptReason::PathUnavailable { path: Path::B, .. })
        ));
        assert_eq!(parse_override("/localhost is fine").0, None);
        assert_eq!(parse_override("please /free this").0, None);
        assert_eq!(parse_override("/free").0, Some(Override::Free));
    }

    #[test]
    fn a_project_pin_forces_one_path() {
        let mut r = req(Preference::Auto);
        r.pin = Some(Path::C);
        assert_eq!(used(&route(&r, &avail(true, true, true))), Some(Path::C));
        // A pinned private repo never leaves the machine, even if Ollama is down.
        assert!(matches!(
            route(&r, &avail(false, true, true)),
            Decision::Prompt(PromptReason::PathUnavailable { path: Path::C, .. })
        ));
        // The pin wins over a slash command.
        r.override_ = Some(Override::Free);
        assert_eq!(used(&route(&r, &avail(true, true, true))), Some(Path::C));
    }

    #[test]
    fn auto_routes_by_complexity() {
        let all = avail(true, true, true);
        assert_eq!(used(&route(&req(Preference::Auto), &all)), Some(Path::C));
        let mut r = req(Preference::Auto);
        r.complexity = classify("Refactor the storage layer to use one connection pool");
        assert_eq!(r.complexity, Complexity::Complex);
        assert_eq!(used(&route(&r, &all)), Some(Path::B));
        assert_eq!(classify("explain this file"), Complexity::Simple);
    }

    #[test]
    fn a_custom_order_replaces_the_default() {
        let mut r = req(Preference::PreferLocal);
        r.order = Some(vec![Path::A, Path::C]);
        assert_eq!(used(&route(&r, &avail(true, true, true))), Some(Path::A));
    }
}
