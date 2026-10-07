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
    for bad in [
        "/bin/sh",
        "./x",
        "../x",
        "a/b",
        "",
        "-rf",
        "a\\b",
        "C:sh",
        "sh:stream",
    ] {
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
    #[cfg(windows)]
    for bad in ["cmd.", "cmd ", "x.bat."] {
        assert!(
            matches!(
                resolve_program(bad, "C:\\Windows\\System32"),
                Err(ExecError::ProgramNotBareName(_))
            ),
            "{bad}"
        );
    }
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
    for force_seccomp in [false, true] {
        sys::linux_net::FORCE_SECCOMP.with(|f| f.set(force_seccomp));
        let out = run(&spec("cat", &["/proc/net/dev"], Network::Deny)).unwrap();
        let method = out.isolation.network.expect("network denied");
        if force_seccomp {
            assert_eq!(method, NetworkMethod::Seccomp);
        }
        if method == NetworkMethod::Namespace {
            let text = String::from_utf8_lossy(&out.stdout);
            let ifaces: Vec<_> = text
                .lines()
                .skip(2)
                .filter_map(|l| l.split(':').next())
                .map(|s| s.trim().to_string())
                .collect();
            assert_eq!(ifaces, vec!["lo".to_string()], "{text}");
        }
        // Real egress attempts fail (TCP and UDP); local AF_UNIX still works.
        if resolve_program("python3", &path_var()).is_ok() {
            let probe = r#"
import socket
def attempt(kind):
    try:
        s = socket.socket(socket.AF_INET, kind)
        s.settimeout(3)
        if kind == socket.SOCK_STREAM:
            s.connect(("1.1.1.1", 80))
        else:
            s.sendto(b"x", ("1.1.1.1", 53))
        print(kind, "EGRESS_OK")
    except OSError as e:
        print(kind, "blocked", e.errno)
attempt(socket.SOCK_STREAM)
attempt(socket.SOCK_DGRAM)
a, b = socket.socketpair(socket.AF_UNIX)
a.send(b"ok"); print("unix", b.recv(2).decode())
"#;
            let out = run(&spec("python3", &["-c", probe], Network::Deny)).unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(!text.contains("EGRESS_OK"), "{method:?}: {text}");
            // Blocked by the mechanism itself, not by an outside firewall:
            // seccomp refuses the socket (EACCES); an empty namespace has no
            // route (ENETUNREACH).
            let expected = match method {
                NetworkMethod::Seccomp => libc::EACCES,
                _ => libc::ENETUNREACH,
            };
            assert_eq!(
                text.matches(&format!("blocked {expected}")).count(),
                2,
                "{method:?}: {text}"
            );
            assert!(
                text.contains("unix ok"),
                "{method:?}: {text} {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    sys::linux_net::FORCE_SECCOMP.with(|f| f.set(false));
}

/// Without network denial, the seccomp filter is not applied.
#[cfg(target_os = "linux")]
#[test]
fn network_allowed_process_can_open_inet_sockets() {
    if resolve_program("python3", &path_var()).is_err() {
        return;
    }
    let out = run(&spec(
        "python3",
        &[
            "-c",
            "import socket; socket.socket(socket.AF_INET); print('inet ok')",
        ],
        Network::Allow,
    ))
    .unwrap();
    assert!(out.isolation.network.is_none());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "inet ok\n");
}

// Threat 7's fork bomb and process-limit tests are in
// `tests/fork_limits.rs`: macOS counts the process limit per user, so
// they must not run alongside the tests here.

/// Linux enforces the limit with `RLIMIT_AS`; macOS rejects lowering that,
/// so the process group's resident size is supervised and the group killed
/// (ADR-0003).
#[test]
fn memory_limit_enforced() {
    if resolve_program("python3", &path_var()).is_err() {
        eprintln!("SKIPPED: python3 not available");
        return;
    }
    // Written, not just reserved, so the pages are resident.
    let mut s = spec(
        "python3",
        &[
            "-c",
            "import time\nx = b'\\x01' * (1 << 30)\nprint('allocated', flush=True)\ntime.sleep(5)\nprint('survived')",
        ],
        Network::Deny,
    );
    s.limits.address_space_bytes = 256 << 20;
    let out = run(&s).unwrap();
    assert_ne!(out.exit, ExitKind::Exited(0));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("survived"));
    if cfg!(target_os = "macos") {
        assert_eq!(out.exit, ExitKind::MemoryExceeded);
    } else {
        assert!(!String::from_utf8_lossy(&out.stdout).contains("allocated"));
    }
}

#[test]
fn wall_timeout_kills() {
    let mut s = spec("sleep", &["30"], Network::Deny);
    s.limits.wall_timeout = Duration::from_millis(300);
    let out = run(&s).unwrap();
    assert_eq!(out.exit, ExitKind::TimedOut);
    assert!(out.elapsed < Duration::from_secs(10));
    // The killed leader is still reaped with its usage.
    assert!(out.rusage.is_some());
}

#[test]
fn resource_usage_is_reported() {
    if resolve_program("python3", &path_var()).is_err() {
        eprintln!("SKIPPED: python3 not available");
        return;
    }
    // Busy for about 0.3 s of CPU, holding about 64 MiB.
    let s = spec(
        "python3",
        &[
            "-c",
            "import time\nb = bytearray(64 << 20)\nfor i in range(0, len(b), 4096): b[i] = 1\nt = time.process_time()\nwhile time.process_time() - t < 0.3: pass\nprint('done')",
        ],
        Network::Deny,
    );
    let out = run(&s).unwrap();
    assert_eq!(out.exit, ExitKind::Exited(0), "{out:?}");
    let u = out.rusage.expect("rusage on Unix");
    assert!(
        u.user_cpu + u.sys_cpu >= Duration::from_millis(200),
        "{u:?}"
    );
    assert!(u.max_rss_bytes >= 64 << 20, "{u:?}");
    assert!(u.max_rss_bytes < 4 << 30, "{u:?}");
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
/// credential directory). Landlock on Linux, Seatbelt on macOS.
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

/// The general syscall denylist (ADR-0003): process inspection, BPF,
/// keyrings, namespaces and io_uring fail with the filter's errno, in both
/// network tiers, while ordinary process creation keeps working.
#[cfg(target_os = "linux")]
#[test]
fn dangerous_syscalls_are_refused() {
    if resolve_program("python3", &path_var()).is_err() {
        eprintln!("python3 not found: skipped");
        return;
    }
    let calls: Vec<(String, i64)> = sys::linux_net::denied_syscalls()
        .into_iter()
        .filter(|(_, n)| {
            [
                "ptrace",
                "bpf",
                "keyctl",
                "unshare",
                "setns",
                "io_uring_setup",
                "perf_event_open",
                "mount",
                "process_vm_readv",
            ]
            .contains(n)
        })
        .map(|(nr, n)| (n.to_string(), nr))
        .collect();
    assert_eq!(calls.len(), 9);
    let script = format!(
        r#"
import ctypes, errno, os, subprocess
libc = ctypes.CDLL(None, use_errno=True)
for name, nr in {calls:?}:
    r = libc.syscall(nr, 0, 0, 0, 0, 0)
    print(name, r, errno.errorcode.get(ctypes.get_errno(), "0") if r < 0 else "ok")
# clone with CLONE_NEWUSER through os.unshare-like raw clone is refused...
r = libc.syscall({clone}, 0x10000000 | 17, 0, 0, 0, 0)
print("clone_newuser", r, errno.errorcode.get(ctypes.get_errno(), "0") if r < 0 else "ok")
# ...while ordinary children and threads still work.
print("child", subprocess.run(["/bin/echo", "hi"], capture_output=True).stdout.decode().strip())
import threading; t = threading.Thread(target=lambda: None); t.start(); t.join(); print("thread ok")
"#,
        calls = calls,
        clone = libc::SYS_clone,
    );
    for force_seccomp in [false, true] {
        sys::linux_net::FORCE_SECCOMP.with(|f| f.set(force_seccomp));
        let mut s = spec("python3", &["-c", &script], Network::Deny);
        s.env.push(("PYTHONDONTWRITEBYTECODE".into(), "1".into()));
        let out = run(&s).unwrap();
        sys::linux_net::FORCE_SECCOMP.with(|f| f.set(false));
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.isolation.syscall_filter);
        assert_eq!(
            out.exit,
            ExitKind::Exited(0),
            "{text}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        for (name, _) in &calls {
            assert!(
                text.contains(&format!("{name} -1 EPERM")),
                "{name} not refused:\n{text}"
            );
        }
        assert!(text.contains("clone_newuser -1 EPERM"), "{text}");
        assert!(text.contains("child hi"), "{text}");
        assert!(text.contains("thread ok"), "{text}");
    }
}

/// The filter program itself: every jump lands inside the program and the
/// last instruction returns.
#[cfg(target_os = "linux")]
#[test]
fn seccomp_program_is_well_formed() {
    for net in [false, true] {
        let p = sys::linux_net::filter(net, &[1234, 5678, 91011, 1213]).unwrap();
        for (i, ins) in p.iter().enumerate() {
            if ins.code & 0x07 == 0x05 {
                assert!(i + 1 + (ins.jt as usize) < p.len());
                assert!(i + 1 + (ins.jf as usize) < p.len());
            }
        }
        assert_eq!(p.last().unwrap().code, 0x06);
        assert!(p.len() < 4096);
    }
}

/// Landlock scopes (Linux 6.12+): a confined command cannot reach an
/// abstract unix socket outside its domain (D-Bus, X11, ...) nor signal
/// processes outside it. On older kernels the gap is recorded instead.
#[cfg(target_os = "linux")]
#[test]
fn unix_sockets_and_signals_outside_the_sandbox_are_unreachable() {
    use std::os::linux::net::SocketAddrExt;
    if resolve_program("python3", &path_var()).is_err() {
        eprintln!("python3 not found: skipped");
        return;
    }
    let name = format!("anchor-test-{}", std::process::id());
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind_addr(&addr).unwrap();
    let d = tempfile::tempdir().unwrap();
    let ws = std::fs::canonicalize(d.path()).unwrap();
    // A pathname socket outside the grant (D-Bus, an ssh agent, docker).
    let outside = tempfile::tempdir().unwrap();
    let sock = outside.path().join("agent.sock");
    let path_listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(outside.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o777)).unwrap();
    }
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o777)).unwrap();
    }
    let script = format!(
        r#"
import os, socket, signal
for label, addr in (("abstract", "\0{name}"), ("pathname", "{sock}")):
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(addr); print(label, "connected")
    except OSError as e:
        print(label, "refused", e.errno)
try:
    os.kill({parent}, 0); print("signal delivered")
except OSError as e:
    print("signal refused", e.errno)
for label, pid in (("all", -1), ("group", -{pgrp})):
    try:
        os.kill(pid, 0); print(label, "delivered")
    except OSError as e:
        print(label, "refused", e.errno)
import subprocess
c = subprocess.Popen(["sleep", "30"])
c.kill(); c.wait(); print("own child killed", c.returncode)
"#,
        parent = std::process::id(),
        // SAFETY: no preconditions.
        pgrp = unsafe { libc::getpgrp() },
        sock = sock.display(),
    );
    let mut s = spec("python3", &["-c", &script], Network::Allow);
    s.cwd = ws.clone();
    s.fs = Some(FsConfinement {
        read_write: vec![ws.clone()],
        read_only: FsConfinement::system_read_only(),
    });
    let out = run(&s).unwrap();
    drop((listener, path_listener));
    let text = String::from_utf8_lossy(&out.stdout);
    // The seccomp filter refuses AF_UNIX sockets on every kernel.
    assert!(text.contains("abstract refused"), "{text}");
    assert!(text.contains("pathname refused"), "{text}");
    // The backend, its group and every-process kill are refused on every
    // kernel (seccomp); Landlock's scope adds the rest on 6.12+.
    assert!(text.contains("signal refused"), "{text}");
    assert!(text.contains("all refused"), "{text}");
    assert!(text.contains("group refused"), "{text}");
    assert!(text.contains("own child killed -9"), "{text}");
    if !out.isolation.landlock_scopes {
        eprintln!("Landlock scopes unavailable on this kernel (ABI < 6)");
    }
}

/// Review finding S-4: the Seatbelt profile denies by default and is well
/// formed (checked on every Unix; only macOS runs it).
#[cfg(unix)]
#[test]
fn seatbelt_profile_denies_by_default() {
    let d = tempfile::tempdir().unwrap();
    let mut s = spec("sh", &["-c", "true"], Network::Deny);
    s.fs = Some(FsConfinement {
        read_write: vec![d.path().join("we \"quote\" (paren)")],
        read_only: FsConfinement::system_read_only(),
    });
    std::fs::create_dir(&s.fs.as_ref().unwrap().read_write[0]).unwrap();
    for (net, fs) in [
        (Network::Deny, true),
        (Network::Allow, true),
        (Network::Deny, false),
    ] {
        let mut s = s.clone();
        s.network = net;
        if !fs {
            s.fs = None;
        }
        let p = sys::macos_profile(&s);
        assert!(p.starts_with("(version 1)(deny default)"), "{p}");
        assert!(!p.contains("(allow default)"), "{p}");
        assert_eq!(p.contains("(deny network*)"), net == Network::Deny, "{p}");
        // Balanced parentheses outside string literals.
        let (mut depth, mut in_str, mut esc) = (0i32, false, false);
        for c in p.chars() {
            match (in_str, esc, c) {
                (true, true, _) => esc = false,
                (true, false, '\\') => esc = true,
                (_, false, '"') => in_str = !in_str,
                (false, _, '(') => depth += 1,
                (false, _, ')') => {
                    depth -= 1;
                    assert!(depth >= 0, "{p}");
                }
                _ => {}
            }
        }
        assert!(depth == 0 && !in_str, "{p}");
        if fs {
            assert!(p.contains("(allow process-exec (subpath"), "{p}");
            assert!(p.contains("Keychains"), "{p}");
            // Round 2 M-1: the data volume is denied, and last.
            assert!(p.ends_with(")") && p.contains("(subpath \"/System/Volumes/Data\")"));
            let last_allow = p.rfind("(allow").unwrap();
            assert!(p.rfind("/System/Volumes/Data").unwrap() > last_allow, "{p}");
        }
        // Round 2 M-2: never every network operation (unix sockets).
        assert!(!p.contains("(allow network*"), "{p}");
    }
    // Round 2 M-3: no write grant at all, not an unfiltered one.
    let mut none = s.clone();
    none.fs.as_mut().unwrap().read_write.clear();
    let p = sys::macos_profile(&none);
    assert!(!p.contains("(allow file-write*)"), "{p}");
    assert!(!p.contains("(allow file-write* "), "{p}");
    assert!(!p.contains("(allow process-exec)"), "{p}");
}

/// Round 2 findings M-1, M-2 and M-5 on macOS: with the network allowed a
/// command still cannot connect to a unix socket outside (only IP), the
/// data volume's path to the home directory is closed, and keychains stay
/// unreadable even when the whole home is granted (the deny comes last).
#[cfg(target_os = "macos")]
#[test]
fn seatbelt_closes_unix_sockets_data_volume_and_keychains_macos() {
    if resolve_program("python3", &path_var()).is_err() {
        eprintln!("SKIPPED: python3 not available");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(d.path()).unwrap();
    let ws = base.join("ws");
    std::fs::create_dir(&ws).unwrap();
    let sock = base.join("agent.sock");
    let _unix = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = tcp.local_addr().unwrap().port();
    let home = std::env::var("HOME").unwrap();
    let script = format!(
        r#"
import os, socket
def tryit(label, f):
    try:
        f(); print(label, "allowed")
    except OSError as e:
        print(label, "refused", e.errno)
def unix():
    s = socket.socket(socket.AF_UNIX); s.connect("{sock}")
def tcp():
    s = socket.create_connection(("127.0.0.1", {port}), timeout=3)
tryit("unix", unix)
tryit("tcp", tcp)
tryit("data", lambda: os.listdir("/System/Volumes/Data{home}"))
tryit("home", lambda: os.listdir("{home}"))
tryit("keychains", lambda: os.listdir("{home}/Library/Keychains"))
"#,
        sock = sock.display(),
    );
    let mut s = spec("python3", &["-c", &script], Network::Allow);
    s.cwd = ws.clone();
    s.fs = Some(FsConfinement {
        read_write: vec![ws],
        read_only: FsConfinement::system_read_only()
            .into_iter()
            .chain(path_var().split(':').map(PathBuf::from))
            .chain([PathBuf::from(&home)])
            .filter(|p| p.is_absolute() && p.exists())
            .collect(),
    });
    let out = run(&s).unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("unix refused"), "{text} {err}");
    assert!(text.contains("tcp allowed"), "{text} {err}");
    assert!(text.contains("data refused"), "{text} {err}");
    assert!(text.contains("home allowed"), "{text} {err}");
    assert!(text.contains("keychains refused"), "{text} {err}");
    drop(tcp);
}

/// Review finding S-4 on macOS: a confined command cannot reach Mach
/// services outside its allowlist (the pasteboard, Apple Events, the
/// keychain daemon), which `(allow default)` left open.
#[cfg(target_os = "macos")]
#[test]
fn mach_services_are_unreachable_macos() {
    if resolve_program("python3", &path_var()).is_err() {
        eprintln!("SKIPPED: python3 not available");
        return;
    }
    let probe = r#"
import ctypes
lib = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
bp = ctypes.c_uint.in_dll(lib, "bootstrap_port")
for name in ("com.apple.securityd", "com.apple.coreservices.launchservicesd",
             "com.apple.pasteboard.1", "com.apple.coreservices.appleevents"):
    port = ctypes.c_uint(0)
    r = lib.bootstrap_look_up(bp, name.encode(), ctypes.byref(port))
    print(name, "reached" if r == 0 else "refused")
"#;
    let d = tempfile::tempdir().unwrap();
    let ws = std::fs::canonicalize(d.path()).unwrap();
    let open = run(&spec("python3", &["-c", probe], Network::Allow)).unwrap();
    let mut s = spec("python3", &["-c", probe], Network::Deny);
    s.cwd = ws.clone();
    s.fs = Some(FsConfinement {
        read_write: vec![ws],
        read_only: FsConfinement::system_read_only()
            .into_iter()
            .chain(path_var().split(':').map(PathBuf::from))
            .filter(|p| p.is_absolute() && p.exists())
            .collect(),
    });
    let out = run(&s).unwrap();
    let (free, confined) = (
        String::from_utf8_lossy(&open.stdout).into_owned(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    );
    assert!(
        confined.lines().count() == 4,
        "{confined} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reachable: Vec<&str> = free
        .lines()
        .filter(|l| l.ends_with(" reached"))
        .map(|l| l.split(' ').next().unwrap())
        .collect();
    assert!(
        !reachable.is_empty(),
        "no probe service exists here: {free}"
    );
    for name in reachable {
        assert!(
            confined.contains(&format!("{name} refused")),
            "{name} reachable from the sandbox: {confined}"
        );
    }
}

/// Threat 6 on macOS: Seatbelt `(deny network*)` blocks TCP and UDP egress
/// while local `AF_UNIX` pairs keep working.
#[cfg(target_os = "macos")]
#[test]
fn network_denied_process_has_no_network_macos() {
    if resolve_program("python3", &path_var()).is_err() {
        eprintln!("SKIPPED: python3 not available");
        return;
    }
    let probe = r#"
import socket
def attempt(kind):
    try:
        s = socket.socket(socket.AF_INET, kind)
        s.settimeout(3)
        if kind == socket.SOCK_STREAM:
            s.connect(("1.1.1.1", 80))
        else:
            s.sendto(b"x", ("1.1.1.1", 53))
        print(kind, "EGRESS_OK")
    except OSError as e:
        print(kind, "blocked", e.errno)
attempt(socket.SOCK_STREAM)
attempt(socket.SOCK_DGRAM)
a, b = socket.socketpair(socket.AF_UNIX)
a.send(b"ok"); print("unix", b.recv(2).decode())
"#;
    let out = run(&spec("python3", &["-c", probe], Network::Deny)).unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.isolation.network, Some(NetworkMethod::Seatbelt));
    assert!(!text.contains("EGRESS_OK"), "{text}");
    assert_eq!(
        text.matches("blocked").count(),
        2,
        "{text} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("unix ok"), "{text}");
}

/// A command cannot leave its session or process group (which would escape
/// the kill and outlive the command, writing to the workspace afterwards),
/// and it has no controlling terminal to inject keystrokes into (TIOCSTI).
#[cfg(target_os = "linux")]
#[test]
fn commands_cannot_escape_the_kill_or_reach_the_terminal() {
    if resolve_program("python3", &path_var()).is_err() {
        eprintln!("python3 not found: skipped");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let ws = std::fs::canonicalize(d.path()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o777)).unwrap();
    }
    let script = r#"
import os, subprocess, time
for name, f in (("setsid", os.setsid), ("setpgid", lambda: os.setpgid(0, 0))):
    try:
        f(); print(name, "escaped")
    except OSError as e:
        print(name, "refused", e.errno)
try:
    os.open("/dev/tty", os.O_RDWR); print("tty opened")
except OSError as e:
    print("tty refused", e.errno)
# A background child that tries to outlive the command.
subprocess.Popen(["/bin/sh", "-c", "sleep 3; echo late > late.txt"],
                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
print("done", os.getpgrp() == os.getpid(), os.getsid(0) == os.getpid())
"#;
    for force_seccomp in [false, true] {
        sys::linux_net::FORCE_SECCOMP.with(|f| f.set(force_seccomp));
        let mut s = spec("python3", &["-c", script], Network::Deny);
        s.cwd = ws.clone();
        s.fs = Some(FsConfinement {
            read_write: vec![ws.clone()],
            read_only: FsConfinement::system_read_only(),
        });
        let out = run(&s).unwrap();
        sys::linux_net::FORCE_SECCOMP.with(|f| f.set(false));
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains(&format!("setsid refused {}", libc::EPERM)),
            "{text}"
        );
        assert!(
            text.contains(&format!("setpgid refused {}", libc::EPERM)),
            "{text}"
        );
        assert!(text.contains("tty refused"), "{text}");
        assert!(text.contains("done True True"), "{text}");
    }
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        !ws.join("late.txt").exists(),
        "a background process outlived the command"
    );
}

/// Round 2 finding W-9: an environment entry cannot smuggle in another.
#[test]
fn malformed_environment_is_refused() {
    let base = spec("sh", &["-c", "true"], Network::Allow);
    for (k, v) in [("A=B", "x"), ("", "x"), ("A\0B", "x"), ("A", "x\0B=y")] {
        let mut s = base.clone();
        s.env.push((k.into(), v.into()));
        assert!(
            matches!(run(&s), Err(ExecError::InvalidEnvironment(_))),
            "{k:?}={v:?}"
        );
    }
}
