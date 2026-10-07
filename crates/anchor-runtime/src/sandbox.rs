//! Self-sandboxing of the runtime process.
//!
//! On Linux the runtime restricts itself with Landlock before reading its
//! first frame: no write access anywhere, and read/execute access only to
//! system directories needed for DNS resolution, TLS and dynamic loading.
//! The user's home and the workspace are unreadable from this process. A
//! seccomp filter then refuses unix sockets and the syscall denylist shared
//! with agent commands. If either cannot be enforced the runtime refuses to
//! run (review finding S-6).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxStatus {
    FullyEnforced,
    PartiallyEnforced,
    NotEnforced(String),
}

/// System paths the runtime may read (never write).
pub const READABLE_SYSTEM_PATHS: &[&str] =
    &["/etc", "/usr", "/lib", "/lib64", "/lib32", "/nix/store"];

#[cfg(target_os = "linux")]
pub fn restrict_self() -> SandboxStatus {
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, Scope, path_beneath_rules,
    };
    let abi = ABI::V5;
    let existing: Vec<&str> = READABLE_SYSTEM_PATHS
        .iter()
        .copied()
        .filter(|p| std::path::Path::new(p).exists())
        .collect();
    let result = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        // Abstract unix sockets and signals outside the runtime's domain
        // (Linux 6.12+; best effort, the seccomp filter below already
        // refuses unix sockets).
        .and_then(|r| {
            r.set_compatibility(CompatLevel::BestEffort)
                .scope(Scope::from_all(ABI::V6))
        })
        .and_then(|r| r.create())
        .and_then(|r| r.add_rules(path_beneath_rules(&existing, AccessFs::from_read(abi))))
        .and_then(|r| r.restrict_self());
    let fs = match result {
        Ok(s) => match s.ruleset {
            RulesetStatus::FullyEnforced => SandboxStatus::FullyEnforced,
            RulesetStatus::PartiallyEnforced => SandboxStatus::PartiallyEnforced,
            RulesetStatus::NotEnforced => {
                return SandboxStatus::NotEnforced("kernel does not support Landlock".into());
            }
        },
        Err(e) => return SandboxStatus::NotEnforced(e.to_string()),
    };
    // The syscall denylist, and no unix sockets: the user's D-Bus or ssh
    // agent would otherwise be reachable (review finding S-1). The network
    // stays allowed: the runtime talks to model providers.
    match anchor_seccomp::install_self(false) {
        Ok(()) => fs,
        Err(e) => SandboxStatus::NotEnforced(e),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn restrict_self() -> SandboxStatus {
    SandboxStatus::NotEnforced(
        "no self-sandbox on this platform yet; isolation relies on the process boundary (ADR-0003)"
            .into(),
    )
}
