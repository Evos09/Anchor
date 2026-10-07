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
//! * **Filesystem confinement** (capability layer): with an
//!   [`FsConfinement`], the child may write only beneath `read_write` paths
//!   and read only beneath those plus `read_only` paths. Linux uses Landlock
//!   (ruleset built in the parent, applied in the child with two raw
//!   syscalls); macOS adds the equivalent rules to the `sandbox-exec`
//!   profile. If confinement cannot be enforced the command is refused.

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

/// The uid commands run as when the backend runs as root.
pub const NOBODY_UID: u32 = 65534;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Deny,
    Allow,
}

/// Filesystem view granted to a child process.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FsConfinement {
    pub read_write: Vec<PathBuf>,
    pub read_only: Vec<PathBuf>,
}

impl FsConfinement {
    /// Typical system locations a build tool needs to read and execute.
    pub fn system_read_only() -> Vec<PathBuf> {
        [
            "/usr",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/lib32",
            "/etc",
            "/opt",
            "/nix/store",
            "/proc",
            // Device nodes one by one: never `/dev` itself, which holds the
            // user's terminals (`/dev/tty`, `/dev/pts/*`) and `/dev/shm`.
            "/dev/null",
            "/dev/zero",
            "/dev/full",
            "/dev/random",
            "/dev/urandom",
            // Not `/System` itself: `/System/Volumes/Data` is the whole
            // data volume, home directories included (round 2 M-1).
            "/System/Library",
            "/Library/Developer",
            "/private/etc",
            "/Applications/Xcode.app",
        ]
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect()
    }
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
    /// `None` leaves the filesystem unconfined (only for trusted callers;
    /// the backend always confines agent commands).
    pub fs: Option<FsConfinement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitKind {
    Exited(i32),
    Signaled(i32),
    TimedOut,
    /// Killed for exceeding `address_space_bytes` where the kernel cannot
    /// enforce it directly (macOS: resident size of the process group,
    /// polled; Windows: the job's memory limit).
    MemoryExceeded,
}

/// How network access was denied to a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkMethod {
    /// Fresh user + network namespace (loopback only).
    Namespace,
    /// Seccomp filter refusing non-`AF_UNIX` sockets (used where user
    /// namespaces are unavailable).
    Seccomp,
    /// macOS Seatbelt `(deny network*)`.
    Seatbelt,
    /// Windows AppContainer without network capabilities.
    AppContainer,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Isolation {
    /// `Some` iff network access was denied, and how.
    pub network: Option<NetworkMethod>,
    pub fs_confined: bool,
    pub dropped_root: bool,
    /// Linux: the general seccomp denylist (ptrace, mount, bpf, keyctl,
    /// namespaces, io_uring, ...) was installed.
    pub syscall_filter: bool,
    /// Linux: Landlock ABI v6 scopes (abstract unix sockets, signals) were
    /// applied; `false` on kernels before 6.12.
    pub landlock_scopes: bool,
}

impl Isolation {
    pub fn network_isolated(&self) -> bool {
        self.network.is_some()
    }
}

/// Resources the command's leader used, with any descendants it waited
/// for. Descendants killed through the process group without being waited
/// for are not counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rusage {
    pub user_cpu: Duration,
    pub sys_cpu: Duration,
    /// Peak resident set size, in bytes on every platform.
    pub max_rss_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Output {
    pub exit: ExitKind,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
    pub isolation: Isolation,
    pub elapsed: Duration,
    /// `None` where the platform does not report it (Windows).
    pub rusage: Option<Rusage>,
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
    #[error("invalid environment variable: {0:?}")]
    InvalidEnvironment(String),
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
    // `:` names a drive-relative path (`C:python.exe`) or a data stream
    // on Windows; a trailing dot or space is dropped by Windows, so the
    // policy would judge a different name (round 2 W-2).
    if name.is_empty()
        || name.contains(['/', '\\', '\0', ':'])
        || (cfg!(windows) && name.ends_with(['.', ' ']))
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
    // A NUL or a `=` in a name would let one entry smuggle in another
    // (round 2 W-9).
    if let Some((k, _)) = spec
        .env
        .iter()
        .find(|(k, v)| k.is_empty() || k.contains(['=', '\0']) || v.contains('\0'))
    {
        return Err(ExecError::InvalidEnvironment(k.clone()));
    }
    #[cfg(windows)]
    if spec.network == Network::Deny || spec.fs.is_some() {
        return windows::run_confined(spec);
    }
    let (mut cmd, isolation, steps) = sys::build(spec)?;
    cmd.current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd.spawn().map_err(|e| {
        // A pre_exec failure surfaces here; the status pipe names the step.
        match steps.as_ref().and_then(|s| s.failed_step()) {
            Some(step) => ExecError::SandboxUnavailable(format!("{step} failed: {e}")),
            None => ExecError::Spawn(e.to_string()),
        }
    })?;
    drop(cmd);
    let cap = spec.limits.max_output_bytes;
    let out_reader = spawn_reader(child.stdout.take(), cap);
    let err_reader = spawn_reader(child.stderr.take(), cap);

    let mut polls = 0u32;
    let (exit, rusage) = loop {
        match sys::try_reap(&mut child).map_err(|e| ExecError::Io(e.to_string()))? {
            Some((status, usage)) => break (sys::exit_kind(status), usage),
            None if start.elapsed() >= spec.limits.wall_timeout => {
                let usage = sys::kill_and_reap(&mut child);
                break (ExitKind::TimedOut, usage);
            }
            // Every 20 ms where the memory limit is supervised, not set.
            None if polls.is_multiple_of(4)
                && sys::group_memory(&child)
                    .is_some_and(|b| b > spec.limits.address_space_bytes) =>
            {
                let usage = sys::kill_and_reap(&mut child);
                break (ExitKind::MemoryExceeded, usage);
            }
            None => {
                polls = polls.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    };
    // Background children may outlive the leader and hold the pipes open.
    sys::kill_tree(&mut child);
    // Bounded: a process that escaped the kill (only possible where the
    // platform cannot stop `setsid`, i.e. macOS) must not hang the caller
    // by holding the pipes open.
    let deadline = Instant::now() + Duration::from_secs(2);
    let (stdout, t1) = out_reader.take(deadline);
    let (stderr, t2) = err_reader.take(deadline);
    Ok(Output {
        exit,
        stdout,
        stderr,
        truncated: t1 || t2,
        isolation,
        elapsed: start.elapsed(),
        rusage,
    })
}

/// Output collected by a reader thread, readable before the thread ends.
struct Reader {
    buf: std::sync::Arc<std::sync::Mutex<(Vec<u8>, bool)>>,
    done: std::sync::mpsc::Receiver<()>,
}

impl Reader {
    /// What was read, once the pipe closes or at `deadline`, whichever is
    /// first (the thread is then left to finish on its own).
    fn take(self, deadline: Instant) -> (Vec<u8>, bool) {
        let wait = deadline.saturating_duration_since(Instant::now());
        let closed = self.done.recv_timeout(wait).is_ok();
        let mut g = self.buf.lock().unwrap_or_else(|p| p.into_inner());
        let (out, truncated) = std::mem::take(&mut *g);
        (out, truncated || !closed)
    }
}

fn spawn_reader<R: Read + Send + 'static>(r: Option<R>, cap: usize) -> Reader {
    let buf = std::sync::Arc::new(std::sync::Mutex::new((Vec::new(), false)));
    let (tx, done) = std::sync::mpsc::channel();
    let b = buf.clone();
    std::thread::spawn(move || {
        if let Some(mut r) = r {
            let mut chunk = [0u8; 8192];
            loop {
                match r.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut g = b.lock().unwrap_or_else(|p| p.into_inner());
                        let room = cap.saturating_sub(g.0.len());
                        if n > room {
                            g.1 = true;
                        }
                        g.0.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
        }
        let _ = tx.send(());
    });
    Reader { buf, done }
}

#[cfg(unix)]
mod sys {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, Command, ExitStatus};

    use super::*;

    const NOBODY: libc::uid_t = 65534;

    /// Whether this process runs as root, so its commands run as
    /// [`NOBODY_UID`](super::NOBODY_UID).
    pub fn running_as_root() -> bool {
        effective_uid() == Some(0)
    }

    pub fn effective_uid() -> Option<u32> {
        // SAFETY: geteuid has no preconditions.
        Some(unsafe { libc::geteuid() })
    }

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

    /// Steps of child setup, reported through the status pipe when one
    /// fails so the error names exactly what could not be established.
    pub(super) const STEPS: &[&str] = &[
        "?",
        "setsid",
        "drop root to uid 65534",
        "restore dumpable flag",
        "set resource limits",
        "unshare(CLONE_NEWUSER|CLONE_NEWNET)",
        "write /proc/self/setgroups",
        "write /proc/self/uid_map",
        "write /proc/self/gid_map",
        "set no_new_privs",
        "landlock_restrict_self",
        "install seccomp filter",
        "set RLIMIT_NPROC",
        "set RLIMIT_AS",
        "set RLIMIT_CPU",
        "set RLIMIT_FSIZE",
        "set RLIMIT_CORE",
    ];

    /// Status pipe: the child writes one byte (the failing step) before
    /// returning an error from `pre_exec`.
    pub(super) struct StepPipe {
        read: std::os::fd::OwnedFd,
    }

    impl StepPipe {
        pub(super) fn failed_step(&self) -> Option<&'static str> {
            use std::os::fd::AsRawFd;
            let mut b = [0u8; 1];
            // SAFETY: valid fd and buffer; the read end is non-blocking.
            let n = unsafe { libc::read(self.read.as_raw_fd(), b.as_mut_ptr().cast(), 1) };
            (n == 1).then(|| STEPS.get(b[0] as usize).copied().unwrap_or("?"))
        }
    }

    fn pipe() -> Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd), ExecError> {
        use std::os::fd::FromRawFd;
        let mut fds = [0 as libc::c_int; 2];
        // Atomically close-on-exec where available, so a concurrent spawn
        // on another thread cannot inherit the status pipe.
        #[cfg(target_os = "linux")]
        // SAFETY: `fds` is a valid out-array of two ints.
        let r = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        #[cfg(not(target_os = "linux"))]
        // SAFETY: `fds` is a valid out-array of two ints.
        let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if r != 0 {
            return Err(ExecError::Io(std::io::Error::last_os_error().to_string()));
        }
        for (fd, extra) in [(fds[0], libc::O_NONBLOCK), (fds[1], 0)] {
            // SAFETY: fcntl on fds we just created.
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                if extra != 0 {
                    let fl = libc::fcntl(fd, libc::F_GETFL);
                    libc::fcntl(fd, libc::F_SETFL, fl | extra);
                }
            }
        }
        // SAFETY: both fds are freshly created and owned by us.
        Ok(unsafe {
            (
                std::os::fd::OwnedFd::from_raw_fd(fds[0]),
                std::os::fd::OwnedFd::from_raw_fd(fds[1]),
            )
        })
    }

    /// `min(wanted, current hard limit)`: an unprivileged process cannot
    /// raise its hard limit, and asking to would fail the whole spawn.
    fn clamp_to_hard(resource: Resource, wanted: u64) -> u64 {
        let mut cur = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: valid out-pointer.
        if unsafe { libc::getrlimit(resource, &mut cur) } != 0
            || cur.rlim_max == libc::RLIM_INFINITY
        {
            return wanted;
        }
        wanted.min(cur.rlim_max)
    }

    pub(super) fn build(
        spec: &CommandSpec,
    ) -> Result<(Command, Isolation, Option<StepPipe>), ExecError> {
        let mut isolation = Isolation::default();
        let is_root = running_as_root();
        isolation.dropped_root = is_root;

        let mut cmd =
            if cfg!(target_os = "macos") && (spec.network == Network::Deny || spec.fs.is_some()) {
                let mut c = Command::new("/usr/bin/sandbox-exec");
                c.arg("-p")
                    .arg(macos_profile(spec))
                    .arg(&spec.program)
                    .args(&spec.args);
                if spec.network == Network::Deny {
                    isolation.network = Some(NetworkMethod::Seatbelt);
                }
                isolation.fs_confined = spec.fs.is_some();
                c
            } else {
                let mut c = Command::new(&spec.program);
                c.args(&spec.args);
                c
            };

        // Linux network denial: namespaces when the system allows them,
        // otherwise a seccomp filter on socket families (ADR-0003).
        #[cfg(target_os = "linux")]
        let (use_netns, use_seccomp) = if spec.network == Network::Deny {
            match linux_net::namespaces_available(is_root) {
                Ok(()) => {
                    isolation.network = Some(NetworkMethod::Namespace);
                    (true, false)
                }
                Err(_) => {
                    isolation.network = Some(NetworkMethod::Seccomp);
                    (false, true)
                }
            }
        } else {
            (false, false)
        };
        // Every Linux command gets the general syscall denylist; the
        // socket-family rule is added when it is the network method.
        #[cfg(target_os = "linux")]
        let seccomp_prog: Vec<libc::sock_filter> =
            linux_net::filter(use_seccomp, &anchor_seccomp::protected_pids(true))
                .map_err(ExecError::SandboxUnavailable)?;
        #[cfg(target_os = "linux")]
        {
            isolation.syscall_filter = true;
        }
        #[cfg(not(target_os = "linux"))]
        let (use_netns, use_seccomp) = (false, false);

        // (resource, value, step code for error reporting). macOS does not
        // support lowering RLIMIT_AS (setrlimit fails with EINVAL; memory
        // limits are not enforced there), so it is skipped on macOS and the
        // gap is recorded in ADR-0003.
        let limits = [
            (
                libc::RLIMIT_NPROC as Resource,
                nproc_limit(spec.limits.max_processes, is_root),
                12u8,
            ),
            (
                libc::RLIMIT_AS as Resource,
                spec.limits.address_space_bytes,
                13,
            ),
            (libc::RLIMIT_CPU as Resource, spec.limits.cpu_seconds, 14),
            (
                libc::RLIMIT_FSIZE as Resource,
                spec.limits.max_file_bytes,
                15,
            ),
            (libc::RLIMIT_CORE as Resource, 0, 16),
        ]
        .map(|(r, v, step)| {
            let skip = cfg!(target_os = "macos") && step == 13;
            (r, clamp_to_hard(r, v), step, skip)
        });

        // Pre-format everything the child needs: no allocation after fork.
        let (uid, gid) = if is_root {
            (NOBODY, NOBODY)
        } else {
            // SAFETY: no preconditions.
            unsafe { (libc::getuid(), libc::getgid()) }
        };
        let uid_map: Vec<u8> = format!("{uid} {uid} 1").into_bytes();
        let gid_map: Vec<u8> = format!("{gid} {gid} 1").into_bytes();

        #[cfg(target_os = "linux")]
        let landlock_fd: Option<std::os::fd::OwnedFd> = match &spec.fs {
            None => None,
            Some(fs) => {
                isolation.fs_confined = true;
                let (fd, scoped) = landlock_ruleset(fs)?;
                isolation.landlock_scopes = scoped;
                Some(fd)
            }
        };
        #[cfg(target_os = "linux")]
        let landlock_raw: libc::c_int = {
            use std::os::fd::AsRawFd;
            landlock_fd.as_ref().map(|f| f.as_raw_fd()).unwrap_or(-1)
        };

        let (read_end, write_end) = pipe()?;
        let status_fd = {
            use std::os::fd::AsRawFd;
            write_end.as_raw_fd()
        };

        // SAFETY: the closure only performs async-signal-safe syscalls on
        // pre-allocated data.
        unsafe {
            cmd.pre_exec(move || {
                let _keep_pipe = &write_end;
                let fail = |step: u8| {
                    let e = std::io::Error::last_os_error();
                    libc::write(status_fd, [step].as_ptr().cast(), 1);
                    e
                };
                // A new session: the command leads its own process group
                // (the kill handle) and has no controlling terminal, so it
                // cannot inject keystrokes into the user's (TIOCSTI).
                if libc::setsid() < 0 {
                    return Err(fail(1));
                }
                if is_root {
                    if libc::setgroups(0, std::ptr::null()) != 0
                        || libc::setgid(NOBODY) != 0
                        || libc::setuid(NOBODY) != 0
                    {
                        return Err(fail(2));
                    }
                    // Changing uid clears the dumpable flag, which makes
                    // /proc/self/{uid,gid}_map root-owned; restore it so the
                    // namespace maps below can be written.
                    #[cfg(target_os = "linux")]
                    if libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) != 0 {
                        return Err(fail(3));
                    }
                }
                for (r, v, step, skip) in limits {
                    if !skip && rlimit(r, v).is_err() {
                        return Err(fail(step));
                    }
                }
                #[cfg(target_os = "linux")]
                if use_netns {
                    if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) != 0 {
                        return Err(fail(5));
                    }
                    if write_proc(c"/proc/self/setgroups", b"deny").is_err() {
                        return Err(fail(6));
                    }
                    if write_proc(c"/proc/self/uid_map", &uid_map).is_err() {
                        return Err(fail(7));
                    }
                    if write_proc(c"/proc/self/gid_map", &gid_map).is_err() {
                        return Err(fail(8));
                    }
                }
                #[cfg(target_os = "linux")]
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(fail(9));
                }
                #[cfg(target_os = "linux")]
                if landlock_raw >= 0 {
                    // Keeps the OwnedFd alive for as long as the Command.
                    let _keep = &landlock_fd;
                    if libc::syscall(libc::SYS_landlock_restrict_self, landlock_raw, 0) != 0 {
                        return Err(fail(10));
                    }
                }
                // Last: the filter also refuses the namespace syscalls used
                // above.
                #[cfg(target_os = "linux")]
                {
                    let prog = libc::sock_fprog {
                        len: seccomp_prog.len() as libc::c_ushort,
                        filter: seccomp_prog.as_ptr() as *mut libc::sock_filter,
                    };
                    if libc::prctl(
                        libc::PR_SET_SECCOMP,
                        libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                        &prog as *const libc::sock_fprog as libc::c_ulong,
                        0,
                        0,
                    ) != 0
                    {
                        return Err(fail(11));
                    }
                }
                let _ = (&uid_map, &gid_map, use_netns, use_seccomp);
                Ok(())
            });
        }
        Ok((cmd, isolation, Some(StepPipe { read: read_end })))
    }

    /// Build a Landlock ruleset in the parent (allocation is fine here); the
    /// child only applies it.
    #[cfg(target_os = "linux")]
    fn landlock_ruleset(fs: &FsConfinement) -> Result<(std::os::fd::OwnedFd, bool), ExecError> {
        use landlock::{
            ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr,
            RulesetCreatedAttr, Scope, path_beneath_rules,
        };
        // Scopes (ABI v6, Linux 6.12) stop the command from connecting to
        // abstract unix sockets (D-Bus, X11, ...) and from signalling
        // processes outside its domain. Best effort: older kernels lack them.
        let scoped = landlock_abi() >= 6;
        let abi = ABI::V3;
        let existing =
            |v: &[PathBuf]| -> Vec<PathBuf> { v.iter().filter(|p| p.exists()).cloned().collect() };
        let err =
            |e: landlock::RulesetError| ExecError::SandboxUnavailable(format!("landlock: {e}"));
        let created = Ruleset::default()
            // Refuse rather than silently run unconfined on old kernels.
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(abi))
            .map_err(err)?
            .set_compatibility(if scoped {
                CompatLevel::HardRequirement
            } else {
                CompatLevel::BestEffort
            })
            .scope(Scope::from_all(ABI::V6))
            .map_err(err)?
            .set_compatibility(CompatLevel::HardRequirement)
            .create()
            .map_err(err)?
            .add_rules(path_beneath_rules(
                existing(&fs.read_only),
                AccessFs::from_read(abi),
            ))
            .map_err(err)?
            .add_rules(path_beneath_rules(
                existing(&fs.read_write),
                AccessFs::from_all(abi),
            ))
            .map_err(err)?
            .add_rules(path_beneath_rules(
                ["/dev/null", "/dev/zero"],
                AccessFs::from_all(abi),
            ))
            .map_err(err)?;
        let fd: Option<std::os::fd::OwnedFd> = created.into();
        fd.map(|fd| (fd, scoped)).ok_or_else(|| {
            ExecError::SandboxUnavailable("Landlock is not supported by this kernel".into())
        })
    }

    /// The kernel's Landlock ABI version (0 if unsupported).
    #[cfg(target_os = "linux")]
    fn landlock_abi() -> i64 {
        const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
        // SAFETY: with a null attribute and size 0 this only queries.
        let v = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<u8>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        v.max(0)
    }

    /// What a process needs to start on macOS beyond the granted roots:
    /// the dyld shared cache, cryptexes, timezone data and frameworks.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    const MACOS_READ: &[&str] = &[
        "/Library/Frameworks",
        "/Library/Apple",
        "/Library/Preferences",
        "/private/var/db/dyld",
        "/private/var/db/timezone",
        "/System/Volumes/Preboot/Cryptexes",
        "/System/Cryptexes",
        "/private/var/select",
    ];

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(super) fn macos_profile(spec: &CommandSpec) -> String {
        // Seatbelt profile (ADR-0003). Paths are quoted with escaping.
        // Seatbelt matches real paths: `/var/...` is `/private/var/...`.
        let q = |p: &Path| quote_real(p);
        // Everything not allowed below is denied (review finding S-4):
        // in particular Mach services (the pasteboard, Apple Events to
        // other apps, the keychain daemon, launchd job control), IOKit,
        // and signals or process inspection outside the sandbox.
        let mut prof = String::from(
            "(version 1)(deny default)\
             (allow process-fork)\
             (allow signal (target same-sandbox))\
             (allow process-info* (target same-sandbox))\
             (allow sysctl-read)\
             (allow user-preference-read)\
             (allow ipc-posix-sem)\
             (allow ipc-posix-shm-read-data (ipc-posix-name \"apple.shm.notification_center\"))\
             (allow mach-lookup (global-name \"com.apple.system.opendirectoryd.libinfo\") \
                (global-name \"com.apple.system.notification_center\") \
                (global-name \"com.apple.system.logger\") \
                (global-name \"com.apple.logd\") \
                (global-name \"com.apple.cfprefsd.daemon\") \
                (global-name \"com.apple.cfprefsd.agent\"))\
             (allow file-read-metadata)\
             (allow file-read* file-write-data file-ioctl \
                (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/dtracehelper\"))\
             (allow file-read* (literal \"/dev/random\") (literal \"/dev/urandom\"))\
             (allow file-read* file-write* (subpath \"/dev/fd\"))",
        );
        if spec.network == Network::Deny {
            prof.push_str("(deny network*)");
        } else {
            // IP only, plus the resolver's socket: never other unix
            // sockets (docker.sock, an ssh agent), as on Linux (round 2
            // M-2).
            prof.push_str(
                "(allow system-socket)\
                 (allow network-outbound (remote ip \"*:*\"))\
                 (allow network-bind network-inbound (local ip \"*:*\"))\
                 (allow network-outbound (literal \"/private/var/run/mDNSResponder\"))\
                 (allow mach-lookup (global-name \"com.apple.dnssd.service\") \
                    (global-name \"com.apple.SystemConfiguration.configd\"))",
            );
        }
        match &spec.fs {
            None => prof.push_str("(allow process-exec file-read* file-write*)"),
            Some(fs) => {
                // A rule without a filter would allow everything: emit
                // one only with paths (round 2 M-3).
                let writable: String = fs
                    .read_write
                    .iter()
                    .filter_map(|p| q(p))
                    .map(|p| format!(" (subpath {p})"))
                    .collect();
                if !writable.is_empty() {
                    prof.push_str(&format!("(allow file-write*{writable})"));
                }
                // Reads mirror Landlock: nothing but the granted roots and
                // what macOS needs to start a process. Metadata stays
                // readable, as path lookup needs it (Landlock likewise does
                // not gate stat). Programs run only from readable roots.
                let mut roots = String::new();
                for p in fs
                    .read_write
                    .iter()
                    .chain(fs.read_only.iter())
                    .map(PathBuf::as_path)
                    .chain(MACOS_READ.iter().map(Path::new))
                {
                    if let Some(p) = q(p).filter(|_| p.exists()) {
                        roots.push_str(&format!(" (subpath {p})"));
                    }
                }
                prof.push_str(&format!("(allow file-read* (literal \"/\"){roots})"));
                if !roots.is_empty() {
                    prof.push_str(&format!("(allow process-exec{roots})"));
                }
            }
        }
        if spec.fs.is_some() {
            // Last, so they win over any grant above (in SBPL the last
            // matching rule decides): keychains, and the data volume's
            // second path to every home directory (round 2 M-1, M-5).
            prof.push_str(
                "(deny file-read* file-write* process-exec \
                    (subpath \"/Library/Keychains\") (subpath \"/System/Volumes/Data\"))",
            );
            if let Some(k) = account_home().and_then(|h| q(&h.join("Library/Keychains"))) {
                prof.push_str(&format!("(deny file-read* file-write* (subpath {k}))"));
            }
        }
        prof
    }

    /// `p` as an SBPL string literal naming its real path (Seatbelt
    /// matches real paths: `/var/...` is `/private/var/...`). A path that
    /// does not exist yet is resolved through its deepest existing
    /// ancestor. `None` for a path that is not UTF-8, which cannot be
    /// written into a profile faithfully (round 2 M-6).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn quote_real(p: &Path) -> Option<String> {
        let mut real = None;
        let mut rest = Vec::new();
        let mut cur = p;
        while real.is_none() {
            match std::fs::canonicalize(cur) {
                Ok(r) => real = Some(r),
                Err(_) => match (cur.parent(), cur.file_name()) {
                    (Some(parent), Some(name)) => {
                        rest.push(name.to_os_string());
                        cur = parent;
                    }
                    _ => real = Some(p.to_path_buf()),
                },
            }
        }
        let mut real = real.unwrap_or_else(|| p.to_path_buf());
        for name in rest.iter().rev() {
            real.push(name);
        }
        let s = real.to_str()?;
        Some(format!(
            "\"{}\"",
            s.replace('\\', "\\\\").replace('"', "\\\"")
        ))
    }

    /// The account's home directory from the user database, not `$HOME`
    /// (which the caller's environment controls).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn account_home() -> Option<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let mut buf = vec![0 as libc::c_char; 16 * 1024];
        // SAFETY: zeroed POD out-structure.
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: valid buffers and out-pointers; on success `pw_dir`
        // points into `buf`, read before `buf` is dropped.
        unsafe {
            let rc = libc::getpwuid_r(
                libc::getuid(),
                &mut pw,
                buf.as_mut_ptr(),
                buf.len(),
                &mut found,
            );
            if rc != 0 || found.is_null() || pw.pw_dir.is_null() {
                return None;
            }
            let dir = std::ffi::CStr::from_ptr(pw.pw_dir);
            Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())))
        }
    }

    /// The `RLIMIT_NPROC` value that lets a command start at most `max`
    /// new processes. The kernel compares the limit with *every* process
    /// (on Linux, every thread) of the uid, so for a backend running as
    /// the user the user's existing count is added; otherwise a busy
    /// desktop session leaves no room at all (macOS CI run 37608878226:
    /// `fork: Resource temporarily unavailable`). As root, commands run as
    /// `nobody`, whose count starts near zero. Commands running at the
    /// same time share the headroom (AR-2).
    pub(super) fn nproc_limit(max: u64, is_root: bool) -> u64 {
        if is_root {
            return max;
        }
        // SAFETY: no preconditions.
        let uid = unsafe { libc::getuid() };
        user_process_count(uid).saturating_add(max)
    }

    /// Processes (Linux: threads) the kernel counts against `uid`.
    #[cfg(target_os = "macos")]
    pub(super) fn user_process_count(uid: libc::uid_t) -> u64 {
        const PROC_UID_ONLY: u32 = 4;
        // SAFETY: a null buffer asks for the size needed.
        let need = unsafe { libc::proc_listpids(PROC_UID_ONLY, uid, std::ptr::null_mut(), 0) };
        if need <= 0 {
            return 0;
        }
        // Room for processes started between the two calls.
        let mut pids =
            vec![0 as libc::pid_t; need as usize / std::mem::size_of::<libc::pid_t>() + 64];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer holds `bytes` bytes.
        let got =
            unsafe { libc::proc_listpids(PROC_UID_ONLY, uid, pids.as_mut_ptr().cast(), bytes) };
        if got <= 0 {
            return 0;
        }
        pids[..got as usize / std::mem::size_of::<libc::pid_t>()]
            .iter()
            .filter(|&&p| p > 0)
            .count() as u64
    }

    #[cfg(target_os = "linux")]
    pub(super) fn user_process_count(uid: libc::uid_t) -> u64 {
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return 0;
        };
        let mut n = 0u64;
        for e in dir.flatten() {
            if !e
                .file_name()
                .to_str()
                .is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit()))
            {
                continue;
            }
            let Ok(status) = std::fs::read_to_string(e.path().join("status")) else {
                continue;
            };
            let field = |name: &str| {
                status
                    .lines()
                    .find_map(|l| l.strip_prefix(name))
                    .and_then(|v| v.split_whitespace().next())
                    .and_then(|v| v.parse::<u64>().ok())
            };
            if field("Uid:") == Some(u64::from(uid)) {
                n += field("Threads:").unwrap_or(1);
            }
        }
        n
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn user_process_count(_uid: libc::uid_t) -> u64 {
        0
    }

    #[cfg(target_os = "linux")]
    pub(super) mod linux_net {
        use std::sync::OnceLock;

        #[cfg(test)]
        thread_local! {
            /// Test hook: force the seccomp fallback on this thread.
            pub(crate) static FORCE_SECCOMP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }

        /// Whether user+network namespaces can be created here, probed once
        /// in a forked child (e.g. Ubuntu >= 23.10 restricts unprivileged
        /// user namespaces through AppArmor).
        pub(crate) fn namespaces_available(is_root: bool) -> Result<(), String> {
            #[cfg(test)]
            if FORCE_SECCOMP.with(|f| f.get()) {
                return Err("forced by test".into());
            }
            #[cfg(debug_assertions)]
            if std::env::var_os("ANCHOR_EXEC_TEST_DISABLE_USERNS").is_some() {
                return Err("disabled by ANCHOR_EXEC_TEST_DISABLE_USERNS".into());
            }
            static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
            PROBE.get_or_init(|| probe(is_root)).clone()
        }

        fn probe(is_root: bool) -> Result<(), String> {
            let (uid, gid) = if is_root {
                (super::NOBODY, super::NOBODY)
            } else {
                // SAFETY: no preconditions.
                unsafe { (libc::getuid(), libc::getgid()) }
            };
            let uid_map = format!("{uid} {uid} 1").into_bytes();
            let gid_map = format!("{gid} {gid} 1").into_bytes();
            // SAFETY: the child only makes async-signal-safe calls on
            // pre-allocated data, then _exits.
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                return Err("fork failed".into());
            }
            if pid == 0 {
                // SAFETY: async-signal-safe calls only.
                unsafe {
                    if is_root
                        && (libc::setgroups(0, std::ptr::null()) != 0
                            || libc::setgid(super::NOBODY) != 0
                            || libc::setuid(super::NOBODY) != 0
                            || libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) != 0)
                    {
                        libc::_exit(2);
                    }
                    if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) != 0 {
                        libc::_exit(5);
                    }
                    if super::write_proc(c"/proc/self/setgroups", b"deny").is_err() {
                        libc::_exit(6);
                    }
                    if super::write_proc(c"/proc/self/uid_map", &uid_map).is_err() {
                        libc::_exit(7);
                    }
                    if super::write_proc(c"/proc/self/gid_map", &gid_map).is_err() {
                        libc::_exit(8);
                    }
                    libc::_exit(0);
                }
            }
            let mut status = 0;
            // SAFETY: waiting on our own child.
            unsafe { libc::waitpid(pid, &mut status, 0) };
            if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
                Ok(())
            } else {
                let code = libc::WEXITSTATUS(status) as usize;
                Err(format!(
                    "namespace probe failed at: {}",
                    super::STEPS.get(code).unwrap_or(&"?")
                ))
            }
        }

        #[cfg(test)]
        pub(crate) use anchor_seccomp::denied_syscalls;
        pub(crate) use anchor_seccomp::filter;
    }

    pub(super) fn exit_kind(s: ExitStatus) -> ExitKind {
        match (s.code(), s.signal()) {
            (Some(c), _) => ExitKind::Exited(c),
            (None, Some(sig)) => ExitKind::Signaled(sig),
            _ => ExitKind::Exited(-1),
        }
    }

    /// `wait4` the leader without blocking. `Child::try_wait` would reap it
    /// and lose its resource usage, so the leader is reaped here instead;
    /// later `try_wait` calls on `child` then fail harmlessly (ECHILD).
    pub(super) fn try_reap(
        child: &mut Child,
    ) -> std::io::Result<Option<(ExitStatus, Option<Rusage>)>> {
        wait4(child.id() as libc::pid_t, libc::WNOHANG)
    }

    /// Kill the leader, reap it with its usage, then kill the rest of the
    /// group.
    pub(super) fn kill_and_reap(child: &mut Child) -> Option<Rusage> {
        let pid = child.id() as libc::pid_t;
        // SAFETY: killpg has no memory-safety preconditions.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        let usage = wait4(pid, 0).ok().flatten().and_then(|(_, u)| u);
        kill_tree(child);
        usage
    }

    fn wait4(
        pid: libc::pid_t,
        flags: libc::c_int,
    ) -> std::io::Result<Option<(ExitStatus, Option<Rusage>)>> {
        let mut status: libc::c_int = 0;
        // SAFETY: an all-zero rusage is a valid value of this plain C struct.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: both pointers refer to live, properly typed locals.
            let r = unsafe { libc::wait4(pid, &mut status, flags, &mut ru) };
            if r == 0 {
                return Ok(None);
            }
            if r < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            return Ok(Some((ExitStatus::from_raw(status), Some(rusage_of(&ru)))));
        }
    }

    fn rusage_of(ru: &libc::rusage) -> Rusage {
        let tv = |t: libc::timeval| {
            Duration::new(t.tv_sec.max(0) as u64, (t.tv_usec.max(0) as u32) * 1000)
        };
        let rss = ru.ru_maxrss.max(0) as u64;
        // Linux reports kilobytes; macOS reports bytes.
        #[cfg(target_os = "linux")]
        let rss = rss * 1024;
        Rusage {
            user_cpu: tv(ru.ru_utime),
            sys_cpu: tv(ru.ru_stime),
            max_rss_bytes: rss,
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

    /// Resident memory of the child's process group, where the memory limit
    /// is supervised rather than set (macOS rejects lowering `RLIMIT_AS`).
    /// `None` where the kernel enforces the limit itself.
    #[cfg(target_os = "macos")]
    pub(super) fn group_memory(child: &Child) -> Option<u64> {
        let pgid = child.id() as libc::pid_t;
        let mut pids = [0 as libc::pid_t; 4096];
        // SAFETY: the buffer is valid for its full size in bytes.
        let n = unsafe {
            libc::proc_listpgrppids(
                pgid,
                pids.as_mut_ptr().cast(),
                std::mem::size_of_val(&pids) as libc::c_int,
            )
        };
        if n <= 0 {
            return None;
        }
        let mut total = 0u64;
        for &pid in &pids[..(n as usize).min(pids.len())] {
            // SAFETY: an all-zero proc_taskinfo is a valid value.
            let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
            // SAFETY: `info` is a properly sized out-buffer.
            let r = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTASKINFO,
                    0,
                    (&mut info as *mut libc::proc_taskinfo).cast(),
                    size,
                )
            };
            if r == size {
                total = total.saturating_add(info.pti_resident_size);
            }
        }
        Some(total)
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn group_memory(_child: &Child) -> Option<u64> {
        None
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

    pub(super) struct StepPipe;
    impl StepPipe {
        pub(super) fn failed_step(&self) -> Option<&'static str> {
            None
        }
    }

    pub(super) fn build(
        spec: &CommandSpec,
    ) -> Result<(Command, Isolation, Option<StepPipe>), ExecError> {
        // Confined commands never get here (`windows::run_confined`).
        if spec.network == Network::Deny || spec.fs.is_some() {
            return Err(ExecError::SandboxUnavailable(
                "confined commands run through the AppContainer path".into(),
            ));
        }
        let mut c = Command::new(&spec.program);
        c.args(&spec.args);
        Ok((c, Isolation::default(), None))
    }

    pub(super) fn exit_kind(s: ExitStatus) -> ExitKind {
        ExitKind::Exited(s.code().unwrap_or(-1))
    }

    pub(super) fn try_reap(
        child: &mut Child,
    ) -> std::io::Result<Option<(ExitStatus, Option<Rusage>)>> {
        Ok(child.try_wait()?.map(|s| (s, None)))
    }

    pub(super) fn kill_and_reap(child: &mut Child) -> Option<Rusage> {
        let _ = child.kill();
        let _ = child.wait();
        None
    }

    pub(super) fn kill_tree(child: &mut Child) {
        let _ = child.kill();
    }

    pub(super) fn group_memory(_child: &Child) -> Option<u64> {
        None
    }

    pub fn group_alive(_pgid: u32) -> bool {
        false
    }

    pub fn running_as_root() -> bool {
        false
    }

    pub fn effective_uid() -> Option<u32> {
        None
    }
}

pub use sys::{effective_uid, group_alive, running_as_root};

#[cfg(windows)]
mod windows;

#[cfg(test)]
mod tests;

#[cfg(all(test, windows))]
#[allow(clippy::unwrap_used)]
mod tests_windows {
    use super::*;

    fn path_var() -> String {
        std::env::var("PATH").unwrap_or_default()
    }

    /// A confined spec in `ws`, with system and PATH directories readable
    /// (as the backend grants them).
    fn confined(program: &str, args: &[&str], ws: &Path) -> CommandSpec {
        CommandSpec {
            program: resolve_program(program, &path_var()).unwrap(),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: ws.to_path_buf(),
            env: vec![("PATH".into(), path_var())],
            limits: Limits {
                wall_timeout: Duration::from_secs(60),
                ..Limits::default()
            },
            network: Network::Deny,
            fs: Some(FsConfinement {
                read_write: vec![ws.to_path_buf()],
                read_only: std::env::split_paths(&path_var())
                    .filter(|p| p.is_absolute() && p.exists())
                    .collect(),
            }),
        }
    }

    fn text(o: &Output) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    }

    #[test]
    fn command_lines_round_trip_through_argv_quoting() {
        let mut s = String::new();
        for (arg, want) in [
            ("plain", "plain"),
            ("", "\"\""),
            ("two words", "\"two words\""),
            (r#"say "hi""#, r#""say \"hi\"""#),
            (r"C:\dir\", r"C:\dir\"),
            (r"C:\a dir\", r#""C:\a dir\\""#),
            (r#"x\"y"#, r#""x\\\"y""#),
        ] {
            s.clear();
            windows::quote_arg(arg, &mut s);
            assert_eq!(s, want, "{arg:?}");
        }
        assert_eq!(
            windows::plain(Path::new(r"\\?\C:\Windows\cmd.exe")),
            PathBuf::from(r"C:\Windows\cmd.exe")
        );
    }

    /// Threats 5 and 7 on Windows: the AppContainer reads and writes the
    /// workspace only.
    #[test]
    fn confined_commands_reach_the_workspace_only() {
        let ws_dir = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(ws_dir.path()).unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = std::fs::canonicalize(outside_dir.path()).unwrap();
        std::fs::write(ws.join("in.txt"), "inside-content").unwrap();
        std::fs::write(outside.join("secret.txt"), "outside-secret").unwrap();
        let p = |d: &Path, f: &str| windows::plain(&d.join(f)).display().to_string();

        let o = run(&confined(
            "cmd",
            &["/c", &format!("type {}", p(&ws, "in.txt"))],
            &ws,
        ))
        .unwrap();
        assert_eq!(o.exit, ExitKind::Exited(0), "{}", text(&o));
        assert!(text(&o).contains("inside-content"), "{}", text(&o));
        assert_eq!(o.isolation.network, Some(NetworkMethod::AppContainer));
        assert!(o.isolation.fs_confined);

        let o = run(&confined(
            "cmd",
            &["/c", &format!("type {}", p(&outside, "secret.txt"))],
            &ws,
        ))
        .unwrap();
        assert_ne!(o.exit, ExitKind::Exited(0), "{}", text(&o));
        assert!(!text(&o).contains("outside-secret"), "{}", text(&o));

        let o = run(&confined(
            "cmd",
            &["/c", &format!("echo x> {}", p(&ws, "out.txt"))],
            &ws,
        ))
        .unwrap();
        assert_eq!(o.exit, ExitKind::Exited(0), "{}", text(&o));
        assert!(ws.join("out.txt").exists());

        let o = run(&confined(
            "cmd",
            &["/c", &format!("echo x> {}", p(&outside, "planted.txt"))],
            &ws,
        ))
        .unwrap();
        assert_ne!(o.exit, ExitKind::Exited(0), "{}", text(&o));
        assert!(!outside.join("planted.txt").exists());

        // The user's profile (credentials live there) is out of reach.
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            let o = run(&confined(
                "cmd",
                &[
                    "/c",
                    &format!("dir /b \"{}\"", PathBuf::from(profile).display()),
                ],
                &ws,
            ))
            .unwrap();
            assert_ne!(o.exit, ExitKind::Exited(0), "{}", text(&o));
        }
    }

    /// Threat 6 on Windows: no network, loopback included.
    #[test]
    fn confined_commands_have_no_network() {
        let Ok(curl) = resolve_program("curl", &path_var()) else {
            eprintln!("SKIPPED: curl.exe not available");
            return;
        };
        let _ = curl;
        let ws_dir = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(ws_dir.path()).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        for url in [
            "http://1.1.1.1/".to_string(),
            format!("http://127.0.0.1:{port}/"),
        ] {
            let o = run(&confined("curl", &["-s", "-m", "5", &url], &ws)).unwrap();
            assert_ne!(o.exit, ExitKind::Exited(0), "{url}: {}", text(&o));
        }
        drop(listener);
    }

    /// Threat 7 on Windows: the wall clock ends the whole job.
    #[test]
    fn confined_commands_are_killed_at_the_deadline() {
        let ws_dir = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(ws_dir.path()).unwrap();
        let mut s = confined("cmd", &["/c", "for /l %i in (0,0,1) do @rem"], &ws);
        s.limits.wall_timeout = Duration::from_secs(2);
        let t = Instant::now();
        let o = run(&s).unwrap();
        assert_eq!(o.exit, ExitKind::TimedOut);
        assert!(t.elapsed() < Duration::from_secs(15), "{:?}", t.elapsed());
    }

    /// The job's memory limit holds, and is reported as such.
    #[test]
    fn confined_memory_is_limited() {
        let Ok(_) = resolve_program("python", &path_var()) else {
            eprintln!("SKIPPED: python not available");
            return;
        };
        let ws_dir = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(ws_dir.path()).unwrap();
        let mut s = confined(
            "python",
            &[
                "-c",
                "x = bytearray(1024 * 1024 * 1024); print('allocated')",
            ],
            &ws,
        );
        s.limits.address_space_bytes = 256 << 20;
        let o = run(&s).unwrap();
        assert!(!text(&o).contains("allocated"), "{}", text(&o));
        assert_eq!(o.exit, ExitKind::MemoryExceeded, "{}", text(&o));
        assert!(o.rusage.is_some());
    }

    /// Batch files would be re-parsed by cmd.exe; they are refused.
    #[test]
    fn confined_batch_files_are_refused() {
        let ws_dir = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(ws_dir.path()).unwrap();
        std::fs::write(ws.join("x.bat"), "@echo hi\r\n").unwrap();
        let mut s = confined("cmd", &[], &ws);
        s.program = ws.join("x.bat");
        assert!(matches!(run(&s), Err(ExecError::SandboxUnavailable(_))));
    }
}
