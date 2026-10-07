//! `anchor-agent`: the Layer 2 runtime process. Spawned by the backend; talks
//! NDJSON over stdin/stdout. Diagnostics go to stderr.

use std::io::{BufReader, stdin, stdout};
use std::process::ExitCode;
use std::time::Duration;

use anchor_runtime::agent::{StdioTransport, default_factory, run_session};
use anchor_runtime::provider::detect_local_servers;
use anchor_runtime::sandbox::{SandboxStatus, restrict_self};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--detect-local") => {
            for s in detect_local_servers(Duration::from_millis(300)) {
                println!("{}\t{}", s.base_url, s.models.join(","));
            }
            return ExitCode::SUCCESS;
        }
        Some("--selftest-fs-denied") => return selftest(),
        Some(other) => {
            eprintln!("anchor-agent: unknown argument {other:?}");
            return ExitCode::from(2);
        }
        None => {}
    }
    let status = restrict_self();
    eprintln!("anchor-agent: sandbox: {status:?}");
    // Fail closed where a sandbox exists (Linux, macOS): an unconfined
    // runtime would read the user's files and reach their terminal and
    // sockets.
    if cfg!(any(target_os = "linux", target_os = "macos"))
        && matches!(status, SandboxStatus::NotEnforced(_))
    {
        eprintln!("anchor-agent: refusing to run without the sandbox");
        return ExitCode::from(3);
    }
    let mut t = StdioTransport::new(BufReader::new(stdin().lock()), stdout().lock());
    let factory = default_factory();
    match run_session(&mut t, &*factory) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("anchor-agent: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Apply the sandbox, then try to read and write a path given on stdin's
/// first line. Prints `DENIED` or `ALLOWED` for each. Used by the backend's
/// integration tests to verify the runtime has no filesystem authority.
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
fn selftest() -> ExitCode {
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    let target = line.trim().to_string();
    // An optional second line: a listening unix socket outside the
    // sandbox (the user's D-Bus, an ssh agent) to try to connect to.
    let mut sock = String::new();
    let _ = std::io::stdin().read_line(&mut sock);
    let sock = sock.trim().to_string();
    let status = restrict_self();
    println!(
        "sandbox {}",
        match status {
            SandboxStatus::NotEnforced(_) => "not-enforced",
            _ => "enforced",
        }
    );
    let read = std::fs::read(&target).is_ok();
    let write = std::fs::write(format!("{target}.agent-write"), b"x").is_ok();
    println!("read {}", if read { "ALLOWED" } else { "DENIED" });
    println!("write {}", if write { "ALLOWED" } else { "DENIED" });
    #[cfg(unix)]
    {
        let unix = std::os::unix::net::UnixDatagram::unbound().is_ok();
        println!("unix_socket {}", if unix { "ALLOWED" } else { "DENIED" });
        if !sock.is_empty() {
            let c = std::os::unix::net::UnixStream::connect(&sock).is_ok();
            println!("unix_connect {}", if c { "ALLOWED" } else { "DENIED" });
        }
    }
    ExitCode::SUCCESS
}
