//! Process execution for agent commands (threats 4, 6 and 7).
//!
//! * **No shell.** Commands are an absolute program path plus an argument
//!   vector. Model output is never concatenated into a shell string; an
//!   argument such as `"; rm -rf /"` reaches the program as one argv element.
//! * **Clean environment.** The child gets only the variables passed in.
//! * **Resource limits** (Unix): `RLIMIT_NPROC`, `RLIMIT_AS`, `RLIMIT_CPU`,
//!   `RLIMIT_FSIZE`, `RLIMIT_CORE=0`, plus a wall-clock timeout after which
//!   the whole process group is killed. Because the kernel exempts uid 0 from
//!   `RLIMIT_NPROC`, a backend running as root drops the child to
//!   `nobody` (65534) so the limit is actually enforced.
//! * **Network** (capability layer): with [`Network::Deny`] the child is
//!   placed in fresh user + network namespaces on Linux (loopback only), or
//!   under a `sandbox-exec` profile denying network on macOS. If isolation
//!   cannot be established the command is **refused**, never run without it.
//!   Windows has no implementation yet and refuses `Network::Deny`
//!   (ADR-0003).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub cpu_seconds: u64,
    pub address_space_bytes: u64,
    pub max_processes: u64,
    pub max_file_bytes: u64,
    pub wall_timeout: Duration,
    pub max_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            cpu_seconds: 120,
            address_space_bytes: 4 << 30,
            max_processes: 256,
            max_file_bytes: 1 << 30,
            wall_timeout: Duration::from_secs(300),
            max_output_bytes: 1 << 20,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Deny,
    Allow,
}

#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// Absolute path of the program (see [`resolve_program`]).
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub limits: Limits,
    pub network: Network,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitKind {
    Exited(i32),
    Signaled(i32),
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Isolation {
    pub network_isolated: bool,
    pub dropped_root: bool,
}

#[derive(Debug, Clone)]
pub struct Output {
    pub exit: ExitKind,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
    pub isolation: Isolation,
    pub elapsed: Duration,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExecError {
    #[error("program must be a bare name (no path separators): {0:?}")]
    ProgramNotBareName(String),
    #[error("program not found on PATH: {0:?}")]
    ProgramNotFound(String),
    #[error("program path must be absolute")]
    ProgramNotAbsolute,
    #[error("argument contains NUL")]
    NulInArgument,
    #[error("required isolation unavailable: {0}")]
    SandboxUnavailable(String),
    #[error("failed to spawn: {0}")]
    Spawn(String),
    #[error("i/o error: {0}")]
    Io(String),
}

/// Resolve a bare program name against `path_var` to a canonical absolute
/// path. Names containing separators are refused, so the model cannot point
/// at arbitrary binaries by path; the policy then decides on the resolved
/// program.
pub fn resolve_program(name: &str, path_var: &str) -> Result<PathBuf, ExecError> {
    if name.is_empty()
        || name.contains(['/', '\\', '\0'])
        || name == "."
        || name == ".."
        || name.starts_with('-')
    {
        return Err(ExecError::ProgramNotBareName(name.to_string()));
    }
    for dir in std::env::split_paths(path_var) {
        if !dir.is_absolute() {
            continue; // never resolve relative to the cwd
        }
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return std::fs::canonicalize(&candidate).map_err(|e| ExecError::Io(e.to_string()));
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if is_executable(&exe) {
                return std::fs::canonicalize(&exe).map_err(|e| ExecError::Io(e.to_string()));
            }
        }
    }
    Err(ExecError::ProgramNotFound(name.to_string()))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Run a command to completion under the spec's limits.
pub fn run(spec: &CommandSpec) -> Result<Output, ExecError> {
    if !spec.program.is_absolute() {
        return Err(ExecError::ProgramNotAbsolute);
    }
    if spec.args.iter().any(|a| a.contains('\0')) {
        return Err(ExecError::NulInArgument);
    }
    let (mut cmd, isolation) = sys::build(spec)?;
    cmd.current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd.spawn().map_err(|e| {
        // A pre_exec failure (e.g. unshare refused) surfaces here.
        if e.raw_os_error().is_some() && spec.network == Network::Deny {
            ExecError::SandboxUnavailable(format!("could not isolate network: {e}"))
        } else {
            ExecError::Spawn(e.to_string())
        }
    })?;
    let cap = spec.limits.max_output_bytes;
    let out_reader = spawn_reader(child.stdout.take(), cap);
    let err_reader = spawn_reader(child.stderr.take(), cap);

    let exit = loop {
        match child.try_wait().map_err(|e| ExecError::Io(e.to_string()))? {
            Some(status) => break sys::exit_kind(status),
            None if start.elapsed() >= spec.limits.wall_timeout => {
                sys::kill_tree(&mut child);
                let _ = child.wait();
                break ExitKind::TimedOut;
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    // Background children may outlive the leader and hold the pipes open.
    sys::kill_tree(&mut child);
    let (stdout, t1) = out_reader.join().unwrap_or_default();
    let (stderr, t2) = err_reader.join().unwrap_or_default();
    Ok(Output {
        exit,
        stdout,
        stderr,
        truncated: t1 || t2,
        isolation,
        elapsed: start.elapsed(),
    })
}

fn spawn_reader<R: Read + Send + 'static>(
    r: Option<R>,
    cap: usize,
) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let Some(mut r) = r else {
            return (Vec::new(), false);
        };
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        let mut truncated = false;
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(out.len());
                    if n > room {
                        truncated = true;
                    }
                    out.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
        (out, truncated)
    })
}

#[cfg(unix)]
mod sys {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, Command, ExitStatus};

    use super::*;

    const NOBODY: libc::uid_t = 65534;

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    type Resource = libc::__rlimit_resource_t;
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    type Resource = libc::c_int;

    fn rlimit(resource: Resource, v: u64) -> std::io::Result<()> {
        let lim = libc::rlimit {
            rlim_cur: v as libc::rlim_t,
            rlim_max: v as libc::rlim_t,
        };
        // SAFETY: setrlimit is async-signal-safe and `lim` is valid.
        if unsafe { libc::setrlimit(resource, &lim) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn write_proc(path: &std::ffi::CStr, data: &[u8]) -> std::io::Result<()> {
        // SAFETY: open/write/close are async-signal-safe; buffers are valid.
        unsafe {
            let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let n = libc::write(fd, data.as_ptr().cast(), data.len());
            libc::close(fd);
            if n != data.len() as isize {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub(super) fn build(spec: &CommandSpec) -> Result<(Command, Isolation), ExecError> {
        let mut isolation = Isolation::default();
        // SAFETY: geteuid has no preconditions.
        let is_root = unsafe { libc::geteuid() } == 0;
        isolation.dropped_root = is_root;

        let mut cmd = if cfg!(target_os = "macos") && spec.network == Network::Deny {
            let mut c = Command::new("/usr/bin/sandbox-exec");
            c.arg("-p")
                .arg("(version 1)(allow default)(deny network*)")
                .arg(&spec.program)
                .args(&spec.args);
            isolation.network_isolated = true;
            c
        } else {
            let mut c = Command::new(&spec.program);
            c.args(&spec.args);
            c
        };

        let limits = spec.limits.clone();
        let deny_net = spec.network == Network::Deny && cfg!(target_os = "linux");
        if deny_net {
            isolation.network_isolated = true;
        }
        // Pre-format everything the child needs: no allocation after fork.
        let (uid, gid) = if is_root {
            (NOBODY, NOBODY)
        } else {
            // SAFETY: no preconditions.
            unsafe { (libc::getuid(), libc::getgid()) }
        };
        let uid_map: Vec<u8> = format!("{uid} {uid} 1").into_bytes();
        let gid_map: Vec<u8> = format!("{gid} {gid} 1").into_bytes();

        // SAFETY: the closure only performs async-signal-safe syscalls on
        // pre-allocated data.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if is_root {
                    if libc::setgroups(0, std::ptr::null()) != 0
                        || libc::setgid(NOBODY) != 0
                        || libc::setuid(NOBODY) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    // Changing uid clears the dumpable flag, which makes
                    // /proc/self/{uid,gid}_map root-owned; restore it so the
                    // namespace maps below can be written.
                    #[cfg(target_os = "linux")]
                    if libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                rlimit(libc::RLIMIT_NPROC as _, limits.max_processes)?;
                rlimit(libc::RLIMIT_AS as _, limits.address_space_bytes)?;
                rlimit(libc::RLIMIT_CPU as _, limits.cpu_seconds)?;
                rlimit(libc::RLIMIT_FSIZE as _, limits.max_file_bytes)?;
                rlimit(libc::RLIMIT_CORE as _, 0)?;
                #[cfg(target_os = "linux")]
                if deny_net {
                    if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    write_proc(c"/proc/self/setgroups", b"deny")?;
                    write_proc(c"/proc/self/uid_map", &uid_map)?;
                    write_proc(c"/proc/self/gid_map", &gid_map)?;
                }
                let _ = (&uid_map, &gid_map, deny_net);
                Ok(())
            });
        }
        Ok((cmd, isolation))
    }

    pub(super) fn exit_kind(s: ExitStatus) -> ExitKind {
        match (s.code(), s.signal()) {
            (Some(c), _) => ExitKind::Exited(c),
            (None, Some(sig)) => ExitKind::Signaled(sig),
            _ => ExitKind::Exited(-1),
        }
    }

    /// Kill the child's whole process group until nothing is left.
    pub(super) fn kill_tree(child: &mut Child) {
        let pgid = child.id() as libc::pid_t;
        for _ in 0..1000 {
            // SAFETY: killpg has no memory-safety preconditions.
            let r = unsafe { libc::killpg(pgid, libc::SIGKILL) };
            if r != 0 {
                break; // ESRCH: group is empty
            }
            let _ = child.try_wait();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Whether any process remains in the group (for tests).
    pub fn group_alive(pgid: u32) -> bool {
        // SAFETY: signal 0 only checks for existence.
        unsafe { libc::killpg(pgid as libc::pid_t, 0) == 0 }
    }
}

#[cfg(windows)]
mod sys {
    use std::process::{Child, Command, ExitStatus};

    use super::*;

    pub(super) fn build(spec: &CommandSpec) -> Result<(Command, Isolation), ExecError> {
        if spec.network == Network::Deny {
            return Err(ExecError::SandboxUnavailable(
                "network isolation on Windows (AppContainer) is not implemented yet; refusing"
                    .into(),
            ));
        }
        let mut c = Command::new(&spec.program);
        c.args(&spec.args);
        Ok((c, Isolation::default()))
    }

    pub(super) fn exit_kind(s: ExitStatus) -> ExitKind {
        ExitKind::Exited(s.code().unwrap_or(-1))
    }

    pub(super) fn kill_tree(child: &mut Child) {
        let _ = child.kill();
    }

    pub fn group_alive(_pgid: u32) -> bool {
        false
    }
}

pub use sys::group_alive;

#[cfg(test)]
mod tests;
