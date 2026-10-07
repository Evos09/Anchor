//! The sandbox validation suite (spec §14 Phase 7, plan step 6).
//!
//! `src/bin/probe.rs` performs one attack per invocation and prints
//! `BLOCKED <detail>` or `ALLOWED <detail>`; `tests/validate.rs` runs every
//! attack through `anchor_exec::run` under the confinement the backend
//! gives agent commands, asserts each is stopped, and writes a per-platform
//! table to `target/sandbox-report.md` (CI appends it to the job summary).
