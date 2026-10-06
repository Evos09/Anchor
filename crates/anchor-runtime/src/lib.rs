//! Anchor agent runtime (Layer 2).
//!
//! This crate runs in its own OS process and has **no filesystem or shell
//! authority**: it never opens files or spawns processes (enforced by
//! `clippy.toml` in this crate, and on Linux by a Landlock ruleset applied
//! at startup). Every effect on the user's machine is a [`anchor_protocol`]
//! request to the backend (Layer 1), which may refuse it.

pub mod agent;
pub mod config;
pub mod prompt;
pub mod provider;
pub mod router;
pub mod sandbox;

#[cfg(test)]
mod tests;
