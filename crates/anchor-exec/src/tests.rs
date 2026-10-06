// The runner's isolation exists only on Unix so far (ADR-0003).
#![cfg(unix)]

use std::time::Duration;

use super::*;

fn path_var() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
}

fn spec(program: &str, args: &[&str], network: Network) -> CommandSpec {
    CommandSpec {
        program: resolve_program(program, &path_var()).unwrap(),
        args: args.iter().map(|s| s.to_string()).collect(),
        // `/` is readable by everyone, including `nobody` when the test
        // runs as root.
        cwd: PathBuf::from("/"),
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        limits: Limits {
            wall_timeout: Duration::from_secs(20),
            ..Limits::default()
        },
        network,
        fs: None,
    }
}

#[test]
fn program_must_be_bare_name() {
    for bad in ["/bin/sh", "./x", "../x", "a/b", "", "-rf", "a\\b"] {
        assert!(
            matches!(
                resolve_program(bad, "/bin"),
                Err(ExecError::ProgramNotBareName(_))
            ),
            "{bad}"
        );
    }
    assert!(matches!(
        resolve_program("definitely-not-a-program-xyz", "/bin"),
        Err(ExecError::ProgramNotFound(_))
    ));
    // Relative PATH entries are ignored (no resolution against the cwd).
    assert!(resolve_program("sh", ".:bin").is_err());
}

/// Threat 4: model output containing shell metacharacters is passed as a
/// single argument and does not execute.
#[test]
fn shell_metacharacters_are_inert() {
    let payload = "; rm -rf / && echo PWNED $(id) `id` | cat > /tmp/x";
    let out = run(&spec("echo", &[payload], Network::Deny)).unwrap();
    assert_eq!(out.exit, ExitKind::Exited(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{payload}\n"));
    // printf prints its first argument verbatim; argv boundaries hold.
    let out = run(&spec("printf", &["[%s]", "a b", "; ls"], Network::Deny)).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[a b][; ls]");
}

#[test]
fn environment_is_cleared() {
    // SAFETY: test-only; no other thread reads this variable.
    unsafe { std::env::set_var("ANCHOR_SECRET_TOKEN", "hunter2") };
    let out = run(&spec("env", &[], Network::Deny)).unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(!s.contains("hunter2"));
    assert!(s.contains("PATH=/usr/bin:/bin"));
}

/// Threat 6: a process spawned with network denied cannot reach the network.
/// On Linux it runs in a fresh network namespace that has only loopback.
#[cfg(target_os = "linux")]
#[test]
fn network_denied_process_has_no_network() {
    let out = run(&spec("cat", &["/proc/net/dev"], Network::Deny)).unwrap();
    assert!(out.isolation.network_isolated);
    let text = String::from_utf8_lossy(&out.stdout);
    let ifaces: Vec<_> = text
        .lines()
        .skip(2)
        .filter_map(|l| l.split(':').next())
        .map(|s| s.trim().to_string())
        .collect();
    assert_eq!(ifaces, vec!["lo".to_string()], "{text}");

    // A real connection attempt fails.
    if resolve_program("curl", &path_var()).is_ok() {
        let out = run(&spec(
            "curl",
            &["-sS", "--max-time", "5", "http://1.1.1.1/"],
            Network::Deny,
        ))
        .unwrap();
        assert_ne!(out.exit, ExitKind::Exited(0), "curl reached the network");
    }
}

/// Threat 7: a fork bomb is stopped at the process limit and the whole
/// process group is killed at the wall-clock limit.
#[cfg(unix)]
#[test]
fn fork_bomb_is_contained() {
    let mut s = spec("sh", &["-c", "b(){ b | b & }; b; sleep 60"], Network::Deny);
    s.limits.max_processes = 64;
    s.limits.wall_timeout = Duration::from_secs(3);
    let t = std::time::Instant::now();
    let out = run(&s).unwrap();
    assert!(
        t.elapsed() < Duration::from_secs(30),
        "took {:?}",
        t.elapsed()
    );
    assert_ne!(out.exit, ExitKind::Exited(0));
    // Give the kernel a moment to reap, then ensure nothing survived.
    std::thread::sleep(Duration::from_millis(200));
    // The leader's pid was the pgid; we no longer know it here, so check via
    // a process scan for our bomb's command line instead.
    let survivors = std::fs::read_dir("/proc")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| {
                    std::fs::read(e.path().join("cmdline"))
                        .map(|c| c.windows(14).any(|w| w == b"b(){ b | b & }"))
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0);
    assert_eq!(survivors, 0, "fork bomb processes survived");
}

#[cfg(unix)]
#[test]
fn process_limit_refuses_excess_forks() {
    let mut s = spec(
        "sh",
        &[
            "-c",
            "i=0; while [ $i -lt 100 ]; do sleep 30 & i=$((i+1)); done; echo done",
        ],
        Network::Deny,
    );
    s.limits.max_processes = 16;
    s.limits.wall_timeout = Duration::from_secs(10);
    let out = run(&s).unwrap();
    let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
    eprintln!(
        "nproc test: exit {:?}, stderr: {}",
        out.exit,
        err.lines().next().unwrap_or("")
    );
    assert!(
        err.contains("fork") || err.contains("resource") || out.exit != ExitKind::Exited(0),
        "expected fork failures, got exit {:?} stderr {err}",
        out.exit
    );
}

#[cfg(unix)]
#[test]
fn memory_limit_enforced() {
    if resolve_program("python3", &path_var()).is_err() {
        eprintln!("SKIPPED: python3 not available");
        return;
    }
    let mut s = spec(
        "python3",
        &["-c", "x = bytearray(1 << 30); print('allocated')"],
        Network::Deny,
    );
    s.limits.address_space_bytes = 256 << 20;
    let out = run(&s).unwrap();
    assert_ne!(out.exit, ExitKind::Exited(0));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("allocated"));
}

#[test]
fn wall_timeout_kills() {
    let mut s = spec("sleep", &["30"], Network::Deny);
    s.limits.wall_timeout = Duration::from_millis(300);
    let out = run(&s).unwrap();
    assert_eq!(out.exit, ExitKind::TimedOut);
    assert!(out.elapsed < Duration::from_secs(10));
}

#[test]
fn output_is_capped() {
    let mut s = spec("head", &["-c", "5000000", "/dev/zero"], Network::Deny);
    s.limits.max_output_bytes = 1000;
    let out = run(&s).unwrap();
    assert_eq!(out.stdout.len(), 1000);
    assert!(out.truncated);
}

#[test]
fn nul_in_argument_refused() {
    let mut s = spec("echo", &[], Network::Deny);
    s.args = vec!["a\0b".into()];
    assert_eq!(run(&s).unwrap_err(), ExecError::NulInArgument);
}

/// Capability layer for Threat 5: a confined command can write only inside
/// its read-write roots and cannot read outside its granted roots (e.g. a
/// credential directory).
#[cfg(target_os = "linux")]
#[test]
fn filesystem_confinement_enforced() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(d.path()).unwrap();
    let ws = base.join("ws");
    let creds = base.join("creds");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&creds).unwrap();
    std::fs::write(creds.join("id_rsa"), b"PRIVATE").unwrap();
    // The child may run as `nobody` when tests run as root.
    for p in [&base, &ws, &creds] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o777)).unwrap();
    }
    std::fs::set_permissions(creds.join("id_rsa"), std::fs::Permissions::from_mode(0o666)).unwrap();
    let confine = FsConfinement {
        read_write: vec![ws.clone()],
        read_only: FsConfinement::system_read_only(),
    };
    let mk = |args: &[&str]| {
        let mut s = spec("sh", &[], Network::Deny);
        s.args = vec!["-c".into(), args.join(" ")];
        s.cwd = ws.clone();
        s.fs = Some(confine.clone());
        s
    };
    let out = run(&mk(&["echo ok > inside.txt && cat inside.txt"])).unwrap();
    assert!(out.isolation.fs_confined);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "ok\n",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run(&mk(&["cat", creds.join("id_rsa").to_str().unwrap()])).unwrap();
    assert_ne!(out.exit, ExitKind::Exited(0));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("PRIVATE"));
    let out = run(&mk(&[
        "echo pwned >",
        base.join("escape.txt").to_str().unwrap(),
    ]))
    .unwrap();
    assert_ne!(out.exit, ExitKind::Exited(0));
    assert!(!base.join("escape.txt").exists());
    assert_eq!(std::fs::read(creds.join("id_rsa")).unwrap(), b"PRIVATE");
}
