//! Cross-platform sandbox validation (spec §14 Phase 7): every attack the
//! probe knows, run through `anchor_exec::run` with the confinement the
//! backend gives agent commands (network denied, the workspace read/write,
//! system and PATH directories read-only, the default limits tightened).
//! Each must be stopped; the outcome table is written to
//! `target/sandbox-report.md` whatever happens, then the test fails if any
//! attack got through.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anchor_exec::{CommandSpec, ExitKind, FsConfinement, Limits, Network, Output, run};

const PROBE: &str = env!("CARGO_BIN_EXE_probe");

struct Env {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    outside: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = std::fs::canonicalize(dir.path()).expect("canonical tempdir");
    let ws = base.join("ws");
    let outside = base.join("outside");
    std::fs::create_dir_all(&ws).expect("ws");
    std::fs::create_dir_all(outside.join("home/.ssh")).expect("outside");
    std::fs::write(outside.join("secret.txt"), "outside secret").expect("secret");
    std::fs::write(outside.join("home/.ssh/id_ed25519"), "PRIVATE KEY").expect("key");
    #[cfg(unix)]
    {
        // Readable by the unprivileged user commands run as under root.
        use std::os::unix::fs::PermissionsExt;
        for d in [&base, &ws, &outside] {
            let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o777));
        }
    }
    Env {
        _dir: dir,
        ws,
        outside,
    }
}

/// The backend's confinement for an agent command in `ws`.
fn spec(e: &Env, args: &[&str]) -> CommandSpec {
    let probe = std::fs::canonicalize(PROBE).expect("probe binary");
    let mut read_only = FsConfinement::system_read_only();
    read_only.push(probe.parent().expect("probe dir").to_path_buf());
    CommandSpec {
        program: probe,
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: e.ws.clone(),
        env: vec![("PATH".into(), std::env::var("PATH").unwrap_or_default())],
        limits: Limits {
            cpu_seconds: 2,
            address_space_bytes: 512 << 20,
            max_processes: 32,
            max_file_bytes: 1 << 20,
            wall_timeout: Duration::from_secs(30),
            max_output_bytes: 64 << 10,
        },
        network: Network::Deny,
        fs: Some(FsConfinement {
            read_write: vec![e.ws.clone()],
            read_only,
        }),
        stdin: Vec::new(),
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout).trim(),
        String::from_utf8_lossy(&o.stderr).trim()
    )
}

/// One row of the report.
struct Row {
    attack: &'static str,
    stopped: bool,
    how: String,
}

/// Stopped when the probe reports BLOCKED or never got to report at all
/// because a limit killed it.
fn judged(attack: &'static str, o: &Output) -> Row {
    let t = text(o);
    let stopped =
        t.contains("BLOCKED") || (!t.contains("ALLOWED") && o.exit != ExitKind::Exited(0));
    Row {
        attack,
        stopped,
        how: format!("{:?}: {}", o.exit, t.lines().next().unwrap_or("")),
    }
}

fn platform() -> &'static str {
    if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(windows) {
        "Windows"
    } else {
        "other"
    }
}

fn report_path() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    target.join("sandbox-report.md")
}

#[test]
fn every_attack_is_stopped() {
    let e = env();
    let mut rows: Vec<Row> = Vec::new();
    // A leading "+net" runs the attack with the network allowed (a
    // command the user approved for network access).
    let mut run_attack = |attack: &'static str, args: &[&str], judge: &dyn Fn(&Output) -> Row| {
        let (args, network) = match args.split_first() {
            Some((&"+net", rest)) => (rest, Network::Allow),
            _ => (args, Network::Deny),
        };
        let mut s = spec(&e, args);
        s.network = network;
        match run(&s) {
            Ok(o) => rows.push(judge(&o)),
            Err(err) => rows.push(Row {
                attack,
                stopped: false,
                how: format!("the sandbox refused to run: {err}"),
            }),
        }
    };
    let plain = |attack: &'static str| move |o: &Output| judged(attack, o);

    // Threat 6: network egress, external and loopback.
    run_attack(
        "TCP to an external address",
        &["connect", "1.1.1.1:80"],
        &plain("TCP to an external address"),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let lo = listener.local_addr().expect("addr").to_string();
    run_attack(
        "TCP to a loopback listener",
        &["connect", &lo],
        &plain("TCP to a loopback listener"),
    );

    // Threat 5: files outside the workspace.
    let secret = e.outside.join("secret.txt").display().to_string();
    let key = e.outside.join("home/.ssh/id_ed25519").display().to_string();
    let planted = e.outside.join("planted.txt");
    let planted_s = planted.display().to_string();
    let outside_s = e.outside.display().to_string();
    run_attack(
        "read a file outside the workspace",
        &["read", &secret],
        &plain("read a file outside the workspace"),
    );
    run_attack(
        "read a fake ~/.ssh key",
        &["read", &key],
        &plain("read a fake ~/.ssh key"),
    );
    run_attack(
        "list a directory outside",
        &["list", &outside_s],
        &plain("list a directory outside"),
    );
    run_attack(
        "write outside the workspace",
        &["write", &planted_s],
        &|o: &Output| {
            let mut r = judged("write outside the workspace", o);
            r.stopped &= !planted.exists();
            r
        },
    );

    // Threat 7: resource limits.
    run_attack(
        "unbounded process creation",
        &["spawn-many", "200"],
        &plain("unbounded process creation"),
    );
    run_attack(
        "allocate past the memory limit",
        &["alloc", &(2usize << 30).to_string()],
        &|o: &Output| {
            let mut r = judged("allocate past the memory limit", o);
            r.stopped |= o.exit == ExitKind::MemoryExceeded;
            r
        },
    );
    let t = Instant::now();
    run_attack("spin the CPU", &["spin"], &|o: &Output| Row {
        attack: "spin the CPU",
        stopped: o.exit != ExitKind::Exited(0) && t.elapsed() < Duration::from_secs(25),
        how: format!("{:?} after {:?}", o.exit, t.elapsed()),
    });
    let big = e.ws.join("big.bin").display().to_string();
    run_attack(
        "write a file past the size limit",
        &["big-file", &big, &(8usize << 20).to_string()],
        &|o: &Output| {
            let mut r = judged("write a file past the size limit", o);
            if cfg!(windows) && !r.stopped {
                // No per-file size limit exists on Windows: an accepted risk
                // (docs/audit/accepted-risks.md), reported, not failed.
                r.stopped = true;
                r.how = format!("not enforced on Windows (accepted risk): {}", r.how);
            }
            r
        },
    );

    // The environment is exactly what the backend passes.
    // SAFETY (edition 2024): set before any command runs; tests in this
    // binary do not read the environment concurrently.
    unsafe { std::env::set_var("ANCHOR_VALIDATION_SECRET", "s3cret") };
    run_attack(
        "inherit the caller's environment",
        &["env"],
        &|o: &Output| {
            let t = text(o);
            Row {
                attack: "inherit the caller's environment",
                stopped: t.starts_with("ENV ") && !t.contains("ANCHOR_VALIDATION_SECRET"),
                how: t.lines().next().unwrap_or("").to_string(),
            }
        },
    );

    #[cfg(unix)]
    {
        let sock = e.outside.join("agent.sock");
        let _l = std::os::unix::net::UnixListener::bind(&sock).expect("unix listener");
        let sock_s = sock.display().to_string();
        run_attack(
            "connect to a unix socket outside",
            &["unix-connect", &sock_s],
            &plain("connect to a unix socket outside"),
        );
        run_attack(
            "unix socket outside, network allowed",
            &["+net", "unix-connect", &sock_s],
            &plain("unix socket outside, network allowed"),
        );
        let me = std::process::id().to_string();
        run_attack(
            "signal the backend",
            &["signal", &me],
            &plain("signal the backend"),
        );
    }
    #[cfg(target_os = "linux")]
    for (name, label) in [
        ("ptrace", "ptrace"),
        ("bpf", "bpf"),
        ("keyctl", "keyctl"),
        ("mount", "mount"),
        ("unshare", "unshare a user namespace"),
    ] {
        run_attack(label, &["syscall", name], &plain(label));
    }
    drop(listener);

    let mut md = String::new();
    let _ = writeln!(md, "### Sandbox validation: {}\n", platform());
    let _ = writeln!(md, "| Attack | Result | How |\n|---|---|---|");
    for r in &rows {
        let _ = writeln!(
            md,
            "| {} | {} | {} |",
            r.attack,
            if r.stopped {
                "stopped"
            } else {
                "**GOT THROUGH**"
            },
            r.how.replace('|', "\\|")
        );
    }
    let path = report_path();
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
    let _ = std::fs::write(&path, &md);
    eprintln!("{md}");
    let through: Vec<&str> = rows
        .iter()
        .filter(|r| !r.stopped)
        .map(|r| r.attack)
        .collect();
    assert!(through.is_empty(), "attacks got through: {through:?}\n{md}");
}
