//! Self-sandboxing of the runtime process.
//!
//! On Linux the runtime restricts itself with Landlock before reading its
//! first frame: no write access anywhere, and read/execute access only to
//! system directories needed for DNS resolution, TLS and dynamic loading.
//! The user's home and the workspace are unreadable from this process.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxStatus {
    FullyEnforced,
    PartiallyEnforced,
    NotEnforced(String),
}

/// System paths the runtime may read (never write).
pub const READABLE_SYSTEM_PATHS: &[&str] = &["/etc", "/usr", "/lib", "/lib64", "/lib32", "/nix/store"];

#[cfg(target_os = "linux")]
pub fn restrict_self() -> SandboxStatus {
    use landlock::{ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, path_beneath_rules};
    let abi = ABI::V5;
    let existing: Vec<&str> = READABLE_SYSTEM_PATHS
        .iter()
        .copied()
        .filter(|p| std::path::Path::new(p).exists())
        .collect();
    let result = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r| r.create())
        .and_then(|r| r.add_rules(path_beneath_rules(&existing, AccessFs::from_read(abi))))
        .and_then(|r| r.restrict_self());
    match result {
        Ok(s) => match s.ruleset {
            RulesetStatus::FullyEnforced => SandboxStatus::FullyEnforced,
            RulesetStatus::PartiallyEnforced => SandboxStatus::PartiallyEnforced,
            RulesetStatus::NotEnforced => SandboxStatus::NotEnforced("kernel does not support Landlock".into()),
        },
        Err(e) => SandboxStatus::NotEnforced(e.to_string()),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn restrict_self() -> SandboxStatus {
    SandboxStatus::NotEnforced(
        "no self-sandbox on this platform yet; isolation relies on the process boundary (ADR-0003)".into(),
    )
}
