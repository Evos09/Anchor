//! Verification pipeline logic (spec §8, ADR-0024), free of I/O so the
//! merge gate, the seeded-bug harness and tests share one implementation:
//!
//! * [`parse`]: tester output (unittest results, trace coverage);
//! * [`review`]: claim resolution (§8.2(b)), the deterministic
//!   [`RuleReviewer`], and the merge [`decide`] (§8.2(d)) with structured
//!   feedback for the executor (§8.2(c));
//! * [`rules`]: hard rules that keep changes away from the test runner.

pub mod parse;
pub mod review;
pub mod rules;

pub use review::{
    Decision, Finding, Reviewer, RuleReviewer, Stage, decide, is_test_path, render_feedback,
    resolve,
};

#[cfg(test)]
mod tests;
