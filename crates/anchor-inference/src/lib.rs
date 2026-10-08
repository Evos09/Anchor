//! Inference routing (Update #1, ADR-0029).
//!
//! Every model call goes through one of three paths, all speaking the
//! OpenAI-compatible chat completions API:
//!
//! * **A, the free pool**: the vendor's capped proxy (`anchor-proxy`);
//! * **B, the user's own key (BYOK)**: straight from the user's machine to
//!   the provider, the key read from the OS keychain;
//! * **C, local**: Ollama on the user's machine; nothing leaves it.
//!
//! This crate is pure: the [`registry`] of models (with load-time license
//! checks), the [`router`] that picks a path per task, and the token
//! [`estimate`] the proxy uses before forwarding.

pub mod estimate;
pub mod registry;
pub mod router;

pub use registry::{ModelEntry, Path, Registry, RegistryError};
pub use router::{Availability, Decision, FreeState, Override, Preference, PromptReason};
