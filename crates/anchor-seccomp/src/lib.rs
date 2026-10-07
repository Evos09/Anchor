//! Seccomp-BPF filters for Anchor's sandboxes (Linux only; ADR-0003).
//!
//! One program, shared by agent commands (`anchor-exec`) and the agent
//! runtime: a syscall denylist, namespace-flag checks on `clone`, and a
//! socket-family rule. Hand-assembled; jumps are resolved by a small
//! assembler that refuses one that would not fit.
#![cfg(target_os = "linux")]

const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
const AUDIT_ARCH_AARCH64: u32 = 0xC000_00B7;
/// BPF_LD | BPF_W | BPF_ABS (BPF_LD and BPF_W are 0).
const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x05 | 0x10;
const BPF_JMP_JGE_K: u16 = 0x05 | 0x30;
const BPF_JMP_JSET_K: u16 = 0x05 | 0x40;
const BPF_RET_K: u16 = 0x06;
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0: u32 = 16; // low 32 bits on little-endian

/// Syscalls a build or test command has no business making: process
/// inspection, mounts, kernel modules and kexec, BPF and perf,
/// kernel keyrings, handle-based opens (which bypass path checks),
/// swap, reboot, namespace changes and `io_uring` (whose operations
/// are not seen by seccomp).
pub fn denied_syscalls() -> Vec<(i64, &'static str)> {
    #[allow(unused_mut)]
    let mut v = vec![
        (libc::SYS_ptrace, "ptrace"),
        (libc::SYS_process_vm_readv, "process_vm_readv"),
        (libc::SYS_process_vm_writev, "process_vm_writev"),
        (libc::SYS_mount, "mount"),
        (libc::SYS_umount2, "umount2"),
        (libc::SYS_pivot_root, "pivot_root"),
        (libc::SYS_fsopen, "fsopen"),
        (libc::SYS_fsconfig, "fsconfig"),
        (libc::SYS_fsmount, "fsmount"),
        (libc::SYS_fspick, "fspick"),
        (libc::SYS_move_mount, "move_mount"),
        (libc::SYS_open_tree, "open_tree"),
        (libc::SYS_mount_setattr, "mount_setattr"),
        (libc::SYS_kexec_load, "kexec_load"),
        (libc::SYS_kexec_file_load, "kexec_file_load"),
        (libc::SYS_init_module, "init_module"),
        (libc::SYS_finit_module, "finit_module"),
        (libc::SYS_delete_module, "delete_module"),
        (libc::SYS_bpf, "bpf"),
        (libc::SYS_perf_event_open, "perf_event_open"),
        (libc::SYS_userfaultfd, "userfaultfd"),
        (libc::SYS_keyctl, "keyctl"),
        (libc::SYS_add_key, "add_key"),
        (libc::SYS_request_key, "request_key"),
        (libc::SYS_open_by_handle_at, "open_by_handle_at"),
        (libc::SYS_name_to_handle_at, "name_to_handle_at"),
        (libc::SYS_swapon, "swapon"),
        (libc::SYS_swapoff, "swapoff"),
        (libc::SYS_reboot, "reboot"),
        (libc::SYS_setns, "setns"),
        (libc::SYS_unshare, "unshare"),
        (libc::SYS_io_uring_setup, "io_uring_setup"),
        (libc::SYS_io_uring_enter, "io_uring_enter"),
        (libc::SYS_io_uring_register, "io_uring_register"),
        (libc::SYS_acct, "acct"),
        (libc::SYS_quotactl, "quotactl"),
        (libc::SYS_syslog, "syslog"),
        (libc::SYS_settimeofday, "settimeofday"),
        (libc::SYS_clock_settime, "clock_settime"),
        (libc::SYS_clock_adjtime, "clock_adjtime"),
        (libc::SYS_adjtimex, "adjtimex"),
        (libc::SYS_sethostname, "sethostname"),
        (libc::SYS_setdomainname, "setdomainname"),
        (libc::SYS_lookup_dcookie, "lookup_dcookie"),
        (libc::SYS_fanotify_init, "fanotify_init"),
        (libc::SYS_kcmp, "kcmp"),
        (libc::SYS_pidfd_getfd, "pidfd_getfd"),
        // A pidfd for any process is one open of /proc/<pid> away; the
        // pid-based signal calls below are filtered instead.
        (libc::SYS_pidfd_send_signal, "pidfd_send_signal"),
        // After the setup's own setsid: leaving the session or
        // process group would escape the kill.
        (libc::SYS_setsid, "setsid"),
        (libc::SYS_setpgid, "setpgid"),
    ];
    #[cfg(target_arch = "x86_64")]
    v.extend([
        (libc::SYS_iopl, "iopl"),
        (libc::SYS_ioperm, "ioperm"),
        (libc::SYS_modify_ldt, "modify_ldt"),
        (libc::SYS_uselib, "uselib"),
        (libc::SYS__sysctl, "_sysctl"),
    ]);
    v
}

/// Processes a confined process may never signal, besides every process
/// at once (`kill(-1)`): the caller (unless `include_self` is false), its
/// process group and session leaders, and its parent. On kernels without
/// Landlock's signal scope (before 6.12) this keeps a command from
/// killing the backend or the user's shell (review finding S-5); other
/// processes of the same user remain reachable there.
pub fn protected_pids(include_self: bool) -> Vec<i32> {
    // SAFETY: these calls have no preconditions.
    let mut v = unsafe {
        let mut v = vec![libc::getpgrp(), libc::getsid(0), libc::getppid()];
        if include_self {
            v.push(libc::getpid());
        }
        v
    };
    v.retain(|&p| p > 1);
    v.sort_unstable();
    v.dedup();
    v
}

/// The syscalls that signal (or open a handle to) a process named by pid
/// in their first argument.
fn signal_syscalls() -> [(i64, bool); 6] {
    // (nr, whether a negative pid names a process group)
    [
        (libc::SYS_kill, true),
        (libc::SYS_tkill, false),
        (libc::SYS_tgkill, false),
        (libc::SYS_rt_sigqueueinfo, false),
        (libc::SYS_rt_tgsigqueueinfo, false),
        (libc::SYS_pidfd_open, false),
    ]
}

/// Namespace flags `clone` may not carry.
const CLONE_NS_FLAGS: u32 = (libc::CLONE_NEWNS
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUSER
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET
    | libc::CLONE_NEWCGROUP) as u32;

#[derive(Clone, Copy)]
enum To {
    Next,
    Eperm,
    Eacces,
    Enosys,
    Allow,
    Kill,
    Skip(u8),
}

/// A tiny assembler: jumps name a terminal (or skip n), offsets are
/// resolved once the program is laid out.
struct Asm(Vec<(u16, To, To, u32)>);
impl Asm {
    fn op(&mut self, code: u16, t: To, f: To, k: u32) {
        self.0.push((code, t, f, k));
    }
    fn finish(self) -> Result<Vec<libc::sock_filter>, String> {
        let body = self.0.len();
        // Terminals follow the body, in this order.
        let term = |t: To| match t {
            To::Allow => Some(0),
            To::Eperm => Some(1),
            To::Eacces => Some(2),
            To::Enosys => Some(3),
            To::Kill => Some(4),
            _ => None,
        };
        let mut out = Vec::with_capacity(body + 5);
        for (i, (code, t, f, k)) in self.0.iter().enumerate() {
            let off = |to: To| -> Result<u8, String> {
                let target = match to {
                    To::Next => i + 1,
                    To::Skip(n) => i + 1 + n as usize,
                    other => body + term(other).unwrap_or(0),
                };
                u8::try_from(target - i - 1).map_err(|_| "seccomp filter: jump too far".to_string())
            };
            out.push(libc::sock_filter {
                code: *code,
                jt: off(*t)?,
                jf: off(*f)?,
                k: *k,
            });
        }
        for k in [
            libc::SECCOMP_RET_ALLOW,
            libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
            libc::SECCOMP_RET_ERRNO | libc::EACCES as u32,
            libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32,
            libc::SECCOMP_RET_KILL_PROCESS,
        ] {
            out.push(libc::sock_filter {
                code: BPF_RET_K,
                jt: 0,
                jf: 0,
                k,
            });
        }
        Ok(out)
    }
}

/// The seccomp program for a confined command:
/// * a foreign architecture or the x32 ABI kills the process;
/// * [`denied_syscalls`] fail with `EPERM`;
/// * `clone` with a namespace flag fails with `EPERM`; `clone3`
///   (whose flags live in memory the filter cannot read) fails with
///   `ENOSYS`, so libc falls back to `clone`;
/// * `socket(AF_UNIX)` fails with `EACCES`; with `network_deny` (the
///   fallback network method) `socket` fails for every family;
///   `socketpair` is allowed;
/// * signalling `kill(-1)` or a [`protected_pids`] process (or its group)
///   fails with `EPERM`.
pub fn filter(network_deny: bool, protect: &[i32]) -> Result<Vec<libc::sock_filter>, String> {
    let arch = if cfg!(target_arch = "x86_64") {
        AUDIT_ARCH_X86_64
    } else if cfg!(target_arch = "aarch64") {
        AUDIT_ARCH_AARCH64
    } else {
        return Err("seccomp filter: unsupported architecture".into());
    };
    let mut a = Asm(Vec::new());
    a.op(BPF_LD_W_ABS, To::Next, To::Next, OFF_ARCH);
    a.op(BPF_JMP_JEQ_K, To::Next, To::Kill, arch);
    a.op(BPF_LD_W_ABS, To::Next, To::Next, OFF_NR);
    a.op(BPF_JMP_JGE_K, To::Kill, To::Next, 0x4000_0000);
    for (nr, _) in denied_syscalls() {
        a.op(BPF_JMP_JEQ_K, To::Eperm, To::Next, nr as u32);
    }
    for (nr, groups) in signal_syscalls() {
        let per_pid = if groups { 2 } else { 1 };
        let body = 1 + usize::from(groups) + per_pid * protect.len() + 1;
        let skip = u8::try_from(body).map_err(|_| "too many protected pids".to_string())?;
        a.op(BPF_JMP_JEQ_K, To::Next, To::Skip(skip), nr as u32);
        a.op(BPF_LD_W_ABS, To::Next, To::Next, OFF_ARG0);
        if groups {
            a.op(BPF_JMP_JEQ_K, To::Eperm, To::Next, (-1i32) as u32);
        }
        for &p in protect {
            a.op(BPF_JMP_JEQ_K, To::Eperm, To::Next, p as u32);
            if groups {
                a.op(BPF_JMP_JEQ_K, To::Eperm, To::Next, (-p) as u32);
            }
        }
        a.op(BPF_JMP_JEQ_K, To::Allow, To::Allow, 0);
    }
    a.op(BPF_JMP_JEQ_K, To::Enosys, To::Next, libc::SYS_clone3 as u32);
    // clone: flags are the first argument on x86_64 and aarch64.
    a.op(BPF_JMP_JEQ_K, To::Next, To::Skip(2), libc::SYS_clone as u32);
    a.op(BPF_LD_W_ABS, To::Next, To::Next, OFF_ARG0);
    a.op(BPF_JMP_JSET_K, To::Eperm, To::Allow, CLONE_NS_FLAGS);
    // socket(): never AF_UNIX: connecting to a pathname or abstract
    // socket outside the sandbox (D-Bus, an ssh agent, a container
    // engine, X11) would escape it, and Landlock does not gate
    // connect. When seccomp is the network method, no family at
    // all. socketpair() stays available for local IPC.
    a.op(BPF_JMP_JEQ_K, To::Next, To::Allow, libc::SYS_socket as u32);
    if network_deny {
        a.op(
            BPF_RET_K,
            To::Next,
            To::Next,
            libc::SECCOMP_RET_ERRNO | libc::EACCES as u32,
        );
    } else {
        a.op(BPF_LD_W_ABS, To::Next, To::Next, OFF_ARG0);
        a.op(BPF_JMP_JEQ_K, To::Eacces, To::Allow, libc::AF_UNIX as u32);
    }
    a.finish()
}

/// Install [`filter`] on the calling process (and, with TSYNC, on all its
/// threads), after setting `no_new_privs`. Used by the agent runtime on
/// itself; `anchor-exec` installs the same program in each command.
pub fn install_self(network_deny: bool) -> Result<(), String> {
    const SECCOMP_SET_MODE_FILTER: libc::c_uint = 1;
    const SECCOMP_FILTER_FLAG_TSYNC: libc::c_uint = 1;
    // A process may still signal itself (abort() does).
    let prog = filter(network_deny, &protected_pids(false))?;
    let fprog = libc::sock_fprog {
        len: prog.len() as libc::c_ushort,
        filter: prog.as_ptr() as *mut libc::sock_filter,
    };
    // SAFETY: plain prctl/syscall with valid arguments; `prog` outlives the
    // call (the kernel copies the program).
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(format!("no_new_privs: {}", std::io::Error::last_os_error()));
        }
        if libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_TSYNC,
            &fprog as *const libc::sock_fprog,
        ) != 0
        {
            return Err(format!("seccomp: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The filter alone (without Landlock scopes) refuses signals to a
    /// protected process and to every process, and keeps a process's own
    /// signals working. Checked in a forked child that installs it.
    #[test]
    fn protected_processes_cannot_be_signalled() {
        // SAFETY: no preconditions.
        let parent = unsafe { libc::getpid() };
        let prog = filter(false, &[parent]).unwrap();
        // SAFETY: the child makes async-signal-safe calls on pre-built
        // data only, then _exits with a bitmask of the outcomes.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                let fprog = libc::sock_fprog {
                    len: prog.len() as libc::c_ushort,
                    filter: prog.as_ptr() as *mut libc::sock_filter,
                };
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                    || libc::prctl(
                        libc::PR_SET_SECCOMP,
                        libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                        &fprog as *const libc::sock_fprog as libc::c_ulong,
                        0,
                        0,
                    ) != 0
                {
                    libc::_exit(100);
                }
                let eperm = |r: libc::c_long| r == -1 && *libc::__errno_location() == libc::EPERM;
                let mut bits = 0;
                if eperm(libc::kill(parent, 0) as libc::c_long) {
                    bits |= 1;
                }
                if eperm(libc::kill(-parent, 0) as libc::c_long) {
                    bits |= 2;
                }
                if eperm(libc::kill(-1, 0) as libc::c_long) {
                    bits |= 4;
                }
                if eperm(libc::syscall(libc::SYS_tgkill, parent, parent, 0)) {
                    bits |= 8;
                }
                if eperm(libc::syscall(libc::SYS_pidfd_open, parent, 0)) {
                    bits |= 16;
                }
                if libc::kill(libc::getpid(), 0) == 0 {
                    bits |= 32;
                }
                libc::_exit(bits);
            }
        }
        let mut status = 0;
        // SAFETY: waiting on our own child.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 63, "outcome bits");
    }
}
