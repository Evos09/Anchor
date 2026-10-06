//! Deterministic role router (spec §9.2). Given the same request kind and
//! configuration it always returns the same role, and states why.

use crate::config::RolesConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    /// File reads, grep, small mechanical edits: cheapest capable model.
    Mechanical,
    /// Architecture, multi-file refactors, spec adherence.
    Architecture,
    /// Terminal and tool-driven execution.
    Agentic,
    /// Review of another role's output: always a different family.
    Review,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDecision {
    pub kind: TaskKind,
    pub role: String,
    pub provider: String,
    pub model_id: String,
    pub reason: &'static str,
}

#[derive(Debug, Clone)]
pub struct Router {
    table: [(TaskKind, RouteDecision); 4],
}

impl Router {
    /// Precompute the routing table so `route` is a constant-time lookup.
    pub fn new(cfg: &RolesConfig) -> Self {
        let mk = |kind: TaskKind, role: &str, reason: &'static str| {
            let r = &cfg.roles[role];
            (
                kind,
                RouteDecision {
                    kind,
                    role: role.to_string(),
                    provider: r.provider.clone(),
                    model_id: r.model_id.clone(),
                    reason,
                },
            )
        };
        Router {
            table: [
                mk(
                    TaskKind::Mechanical,
                    &cfg.routing.mechanical,
                    "mechanical work routes to the configured cheapest capable role",
                ),
                mk(
                    TaskKind::Architecture,
                    &cfg.routing.architecture,
                    "architecture and multi-file work routes to the strongest reasoning role",
                ),
                mk(
                    TaskKind::Agentic,
                    &cfg.routing.agentic,
                    "tool-driven execution routes to the role measured best on agentic tasks",
                ),
                mk(
                    TaskKind::Review,
                    &cfg.routing.review,
                    "review routes to a role of a different family than the executor",
                ),
            ],
        }
    }

    pub fn route(&self, kind: TaskKind) -> &RouteDecision {
        // The table always holds all four kinds (constructed above).
        match self.table.iter().find(|(k, _)| *k == kind) {
            Some((_, d)) => d,
            None => &self.table[0].1,
        }
    }
}
