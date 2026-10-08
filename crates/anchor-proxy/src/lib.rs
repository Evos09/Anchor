//! Anchor's free-tier proxy (Update #1 §4–5; ADR-0031, ADR-0032).
//!
//! The vendor holds one API key per provider; free users' requests come
//! here, are checked against their caps and the pool's health, are
//! forwarded, and are settled on the provider's own token count. This
//! crate is open source so that what it claims about privacy can be read
//! in the code: request content is read in memory to estimate its size
//! and is never written, logged or kept; the only record is metadata
//! ([`log::LogRecord`]).

pub mod caps;
pub mod config;
pub mod ledger;
pub mod log;
pub mod pool;
pub mod server;
pub mod store;
pub mod upstream;
