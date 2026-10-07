//! Self-sandboxing of the runtime process.
//!
//! On Linux the runtime restricts itself with Landlock before reading its
//! first frame: no write access anywhere, and read/execute access only to
//! system directories needed for DNS resolution, TLS and dynamic loading.
//! The user's home and the workspace are unreadable from this process. A
//! seccomp filter then refuses unix sockets and the syscall denylist shared
//! with agent commands. If either cannot be enforced the runtime refuses to
//! run (review finding S-6).
//!
//! On macOS the runtime applies a Seatbelt profile to itself
//! (`sandbox_init`, [`MACOS_PROFILE`]) with the same intent, and likewise
//! refuses to run without it.

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
    // Taken first: Landlock below hides `/proc` (thread ids).
    let protect = anchor_seccomp::protected_pids(false);
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
    match anchor_seccomp::install_self(false, &protect) {
        Ok(()) => fs,
        Err(e) => SandboxStatus::NotEnforced(e),
    }
}

/// The runtime's Seatbelt profile on macOS: nothing by default; reads of
/// system paths only (dyld, frameworks, resolver and timezone data), no
/// writes, outbound IP connections (model providers) and the DNS
/// resolver. Unix-socket connects other than the resolver's are refused,
/// as are Mach services beyond name lookup and logging, process creation
/// and signals. `/System/Library`, not `/System`: `/System/Volumes/Data`
/// reaches every home directory (round 2 M-1), and is denied last (in
/// SBPL the last matching rule wins).
#[cfg(any(target_os = "macos", test))]
pub const MACOS_PROFILE: &str = "(version 1)(deny default)\
(allow sysctl-read)\
(allow file-read-metadata)\
(allow file-read* (literal \"/\") (subpath \"/usr\") (subpath \"/System/Library\") \
  (subpath \"/Library/Apple\") (subpath \"/Library/Preferences\") \
  (subpath \"/private/etc\") (subpath \"/private/var/db/dyld\") \
  (subpath \"/private/var/db/timezone\") (subpath \"/System/Volumes/Preboot/Cryptexes\") \
  (subpath \"/System/Cryptexes\") (literal \"/dev/urandom\") (literal \"/dev/random\") \
  (literal \"/dev/null\"))\
(allow file-write-data (literal \"/dev/null\"))\
(allow system-socket)\
(allow network-outbound (remote ip \"*:*\") \
  (literal \"/private/var/run/mDNSResponder\"))\
(allow mach-lookup (global-name \"com.apple.dnssd.service\") \
  (global-name \"com.apple.SystemConfiguration.configd\") \
  (global-name \"com.apple.system.opendirectoryd.libinfo\") \
  (global-name \"com.apple.system.notification_center\") \
  (global-name \"com.apple.system.logger\") (global-name \"com.apple.logd\"))\
(allow ipc-posix-shm-read-data (ipc-posix-name \"apple.shm.notification_center\"))\
(deny file-read* (subpath \"/System/Volumes/Data\"))";

#[cfg(target_os = "macos")]
pub fn restrict_self() -> SandboxStatus {
    use std::ffi::{CStr, CString, c_char, c_int};
    unsafe extern "C" {
        // libSystem; flags 0 means `profile` is SBPL source.
        fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
        fn sandbox_free_error(errorbuf: *mut c_char);
    }
    let Ok(profile) = CString::new(MACOS_PROFILE) else {
        return SandboxStatus::NotEnforced("profile contains NUL".into());
    };
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: valid NUL-terminated profile and out-pointer; the error
    // buffer, if set, is read once and freed with its own deallocator.
    let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut err) };
    if rc == 0 {
        return SandboxStatus::FullyEnforced;
    }
    let msg = if err.is_null() {
        "sandbox_init failed".to_string()
    } else {
        // SAFETY: sandbox_init set a NUL-terminated message.
        let m = unsafe { CStr::from_ptr(err) }
            .to_string_lossy()
            .into_owned();
        unsafe { sandbox_free_error(err) };
        m
    };
    SandboxStatus::NotEnforced(msg)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn restrict_self() -> SandboxStatus {
    SandboxStatus::NotEnforced(
        "no self-sandbox on this platform yet; isolation relies on the process boundary (ADR-0003)"
            .into(),
    )
}

#[cfg(test)]
mod tests {
    /// The macOS profile is well formed (balanced, deny by default); it
    /// is applied only on macOS.
    #[test]
    fn macos_profile_is_well_formed() {
        let p = super::MACOS_PROFILE;
        assert!(p.starts_with("(version 1)(deny default)"));
        let (mut depth, mut in_str) = (0i32, false);
        for c in p.chars() {
            match (in_str, c) {
                (_, '"') => in_str = !in_str,
                (false, '(') => depth += 1,
                (false, ')') => {
                    depth -= 1;
                    assert!(depth >= 0);
                }
                _ => {}
            }
        }
        assert_eq!((depth, in_str), (0, false));
        assert!(!p.contains("file-write*"));
        assert!(!p.contains("process-exec"));
    }
}
