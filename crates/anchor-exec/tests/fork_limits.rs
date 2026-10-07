//! Threat 7: process limits. In a test binary of their own, and run one
//! after the other: macOS counts `RLIMIT_NPROC` over all of the user's
//! processes, so a fork bomb running alongside other tests made their
//! commands fail to fork (CI run 37605620548).
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Duration;

use anchor_exec::{CommandSpec, ExitKind, Limits, Network, resolve_program, run};

fn path_var() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
}

fn spec(program: &str, args: &[&str], network: Network) -> CommandSpec {
    CommandSpec {
        program: resolve_program(program, &path_var()).unwrap(),
        args: args.iter().map(|s| s.to_string()).collect(),
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
fn process_limits_contain_forks() {
    fork_bomb_is_contained();
    process_limit_refuses_excess_forks();
}

/// A fork bomb is stopped at the process limit and the whole process
/// group is killed at the wall-clock limit.
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
