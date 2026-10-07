//! Windows command sandbox (ADR-0026): every confined command runs in an
//! AppContainer inside a Job Object.
//!
//! * **AppContainer** (one profile per workspace, `anchor.<hash>`): a
//!   lowbox token with no capabilities when the network is denied, so no
//!   internet, private network or loopback; `internetClient` only when it
//!   is allowed. Files are reachable only where the container's SID holds
//!   an ACE: "Modify" (no ACL or owner changes) on [`FsConfinement::read_write`], read/execute on
//!   [`FsConfinement::read_only`] (granted here, inheritable), plus what
//!   Windows grants ALL APPLICATION PACKAGES (system directories). The
//!   user's profile, credentials and other processes are out of reach.
//! * **Job Object**, attached at creation (`PROC_THREAD_ATTRIBUTE_JOB_LIST`,
//!   no window before assignment): active-process, job-memory and job CPU
//!   time limits, kill-on-close, no breakaway, UI restrictions (clipboard,
//!   desktops, global atoms, system parameters). The whole tree is killed
//!   with the job; accounting gives [`Rusage`].
//! * `CreateProcessW` with an explicit `HANDLE_LIST`: only the stdio pipes
//!   are inherited. The command line is quoted for `CommandLineToArgvW`;
//!   only `.exe`/`.com` programs run, judged on the real path (batch
//!   files would let cmd.exe re-parse their arguments).

use std::ffi::OsStr;
use std::io::Error as IoError;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::time::Instant;

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GENERIC_READ, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, HLOCAL,
    INVALID_HANDLE_VALUE, LocalFree, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, SE_FILE_OBJECT, SET_ACCESS,
    SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_WELL_KNOWN_GROUP,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, EqualSid, FreeSid, GetAce, GetAclInformation, NO_INHERITANCE,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_UI_RESTRICTIONS,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute,
    WaitForSingleObject,
};

use super::*;

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const INHERITED_ACE: u8 = 0x10;
const OBJECT_INHERIT_ACE: u8 = 0x1;
const CONTAINER_INHERIT_ACE: u8 = 0x2;
const INHERIT_ONLY_ACE: u8 = 0x8;
/// The "Modify" right set: read, write, execute and delete, without
/// WRITE_DAC, WRITE_OWNER or ACCESS_SYSTEM_SECURITY.
const MODIFY: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;
const SE_GROUP_ENABLED: u32 = 4;
/// The `internetClient` capability: outbound internet, no loopback or
/// private networks.
const INTERNET_CLIENT_SID: &str = "S-1-15-3-1";

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// `\\?\C:\x` as `C:\x` (and `\\?\UNC\s\x` as `\\s\x`): `canonicalize`
/// returns verbatim paths, which `CreateProcessW` and many programs (as
/// their own `argv[0]` or working directory) do not accept.
pub(crate) fn plain(p: &Path) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => std::path::PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

fn unavailable(what: &str) -> ExecError {
    ExecError::SandboxUnavailable(format!("{what}: {}", IoError::last_os_error()))
}

/// A SID and how it must be freed.
struct Sid {
    ptr: PSID,
    local: bool,
}

impl Drop for Sid {
    fn drop(&mut self) {
        // SAFETY: the pointer came from the matching allocator.
        unsafe {
            if self.local {
                LocalFree(self.ptr as HLOCAL);
            } else {
                FreeSid(self.ptr);
            }
        }
    }
}

/// The AppContainer profile name for a confinement: stable per workspace
/// (its first read/write root).
pub(crate) fn profile_name(fs: &FsConfinement, cwd: &Path) -> String {
    let key = fs
        .read_write
        .first()
        .map(|p| p.as_path())
        .unwrap_or(cwd)
        .to_string_lossy()
        .to_lowercase();
    // FNV-1a: no dependency, and only uniqueness matters.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("anchor.{h:016x}")
}

fn container_sid(name: &str) -> Result<Sid, ExecError> {
    let n = wide(OsStr::new(name));
    let desc = wide(OsStr::new("Anchor agent commands"));
    let mut sid: PSID = std::ptr::null_mut();
    // SAFETY: valid wide strings and out-pointer; no capabilities.
    let hr = unsafe {
        CreateAppContainerProfile(
            n.as_ptr(),
            n.as_ptr(),
            desc.as_ptr(),
            std::ptr::null(),
            0,
            &mut sid,
        )
    };
    let exists = (0x8007_0000u32 | ERROR_ALREADY_EXISTS) as i32;
    if hr == exists {
        // SAFETY: as above.
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(n.as_ptr(), &mut sid) };
        if hr < 0 {
            return Err(ExecError::SandboxUnavailable(format!(
                "AppContainer profile {name}: HRESULT {hr:#x}"
            )));
        }
    } else if hr < 0 {
        return Err(ExecError::SandboxUnavailable(format!(
            "AppContainer profile {name}: HRESULT {hr:#x}"
        )));
    }
    Ok(Sid {
        ptr: sid,
        local: false,
    })
}

fn string_sid(s: &str) -> Result<Sid, ExecError> {
    let w = wide(OsStr::new(s));
    let mut sid: PSID = std::ptr::null_mut();
    // SAFETY: valid wide string and out-pointer.
    if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut sid) } == 0 {
        return Err(unavailable("capability SID"));
    }
    Ok(Sid {
        ptr: sid,
        local: true,
    })
}

/// Whether `dacl` already holds an explicit allow ACE for `sid` with at
/// least `access`.
///
/// # Safety
/// `dacl` must be null or a valid ACL, `sid` a valid SID.
unsafe fn has_ace(dacl: *const ACL, sid: PSID, access: u32, dir: bool) -> bool {
    if dacl.is_null() {
        return true; // a null DACL grants everyone everything
    }
    let mut info = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    // SAFETY: caller's contract; `info` is the right size.
    unsafe {
        if GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        ) == 0
        {
            return false;
        }
        for i in 0..info.AceCount {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            if GetAce(dacl, i, &mut ace) == 0 {
                continue;
            }
            let hdr = &*(ace as *const ACE_HEADER);
            if hdr.AceType != ACCESS_ALLOWED_ACE_TYPE || hdr.AceFlags & INHERITED_ACE != 0 {
                continue;
            }
            let a = &*(ace as *const ACCESS_ALLOWED_ACE);
            let ace_sid = (&a.SidStart as *const u32) as PSID;
            // Exactly the mask, inherited by everything below a directory
            // and effective here (round 2 W-10): a broader ACE (from an
            // earlier version, or edited) is replaced, not kept.
            let inherit = u8::from(dir) * (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE);
            let flags =
                hdr.AceFlags & (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE | INHERIT_ONLY_ACE);
            if EqualSid(ace_sid, sid) != 0 && a.Mask == access && flags == inherit {
                return true;
            }
        }
    }
    false
}

/// Give `sid` `access` on `path` (inherited by everything below a
/// directory), unless it already has it.
fn grant(path: &Path, sid: PSID, access: u32) -> Result<(), String> {
    let w = wide(path.as_os_str());
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: valid path and out-pointers; `sd` owns `dacl` and is freed
    // with LocalFree below.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != 0 {
        return Err(format!("{}: reading the ACL: error {rc}", path.display()));
    }
    let result = (|| {
        // SAFETY: `dacl` came from GetNamedSecurityInfoW; `sid` is valid.
        if unsafe { has_ace(dacl, sid, access, path.is_dir()) } {
            return Ok(());
        }
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: access,
            // Replaces any other explicit ACE for the container's SID.
            grfAccessMode: SET_ACCESS,
            grfInheritance: if path.is_dir() {
                SUB_CONTAINERS_AND_OBJECTS_INHERIT
            } else {
                NO_INHERITANCE
            },
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
                ptstrName: sid as *mut u16,
            },
        };
        let mut new: *mut ACL = std::ptr::null_mut();
        // SAFETY: one valid entry; `new` is freed with LocalFree.
        let rc = unsafe { SetEntriesInAclW(1, &ea, dacl, &mut new) };
        if rc != 0 {
            return Err(format!("{}: building the ACL: error {rc}", path.display()));
        }
        // SAFETY: valid path and ACL.
        let rc = unsafe {
            SetNamedSecurityInfoW(
                w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                new,
                std::ptr::null(),
            )
        };
        // SAFETY: allocated by SetEntriesInAclW.
        unsafe { LocalFree(new as HLOCAL) };
        if rc != 0 {
            return Err(format!("{}: writing the ACL: error {rc}", path.display()));
        }
        Ok(())
    })();
    // SAFETY: allocated by GetNamedSecurityInfoW.
    unsafe { LocalFree(sd as HLOCAL) };
    result
}

/// Append `arg` quoted so `CommandLineToArgvW` (and the MSVC runtime)
/// yields it back unchanged.
pub(crate) fn quote_arg(arg: &str, out: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        if c == '"' {
            out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n('\\', backslashes));
        }
        out.push(c);
        backslashes = 0;
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
}

pub(crate) fn command_line(program: &Path, args: &[String]) -> String {
    let mut s = String::new();
    quote_arg(&program.to_string_lossy(), &mut s);
    for a in args {
        s.push(' ');
        quote_arg(a, &mut s);
    }
    s
}

/// The environment block: exactly `env` (sorted, as Windows expects),
/// plus `SystemRoot`, without which much of Windows fails to start.
fn env_block(env: &[(String, String)]) -> Vec<u16> {
    let mut vars: Vec<(String, String)> = env.to_vec();
    if !vars
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("SystemRoot"))
        && let Ok(v) = std::env::var("SystemRoot")
    {
        vars.push(("SystemRoot".into(), v));
    }
    vars.sort_by_key(|(k, _)| k.to_uppercase());
    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(OsStr::new(&format!("{k}={v}")).encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

fn owned(h: HANDLE, what: &str) -> Result<OwnedHandle, ExecError> {
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(unavailable(what));
    }
    // SAFETY: a fresh handle we own.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

/// A pipe whose write end the child inherits; the read end stays here.
fn pipe(sa: &SECURITY_ATTRIBUTES) -> Result<(OwnedHandle, OwnedHandle), ExecError> {
    let (mut r, mut w): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: valid out-pointers and attributes.
    if unsafe { CreatePipe(&mut r, &mut w, sa, 0) } == 0 {
        return Err(unavailable("pipe"));
    }
    let (r, w) = (owned(r, "pipe")?, owned(w, "pipe")?);
    // SAFETY: a handle we own.
    if unsafe { SetHandleInformation(r.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(unavailable("pipe"));
    }
    Ok((r, w))
}

fn job(limits: &Limits) -> Result<OwnedHandle, ExecError> {
    // SAFETY: anonymous job with default security.
    let job = owned(
        unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) },
        "job object",
    )?;
    // SAFETY: zeroed is a valid initial value for this plain struct.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    let b = &mut info.BasicLimitInformation;
    b.LimitFlags = JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_JOB_TIME
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    b.ActiveProcessLimit = u32::try_from(limits.max_processes.max(1)).unwrap_or(u32::MAX);
    // 100 ns units.
    b.PerJobUserTimeLimit = i64::try_from(limits.cpu_seconds.max(1))
        .unwrap_or(i64::MAX / 10_000_000)
        .saturating_mul(10_000_000);
    info.JobMemoryLimit = usize::try_from(limits.address_space_bytes).unwrap_or(usize::MAX);
    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
            | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
            | JOB_OBJECT_UILIMIT_EXITWINDOWS
            | JOB_OBJECT_UILIMIT_GLOBALATOMS
            | JOB_OBJECT_UILIMIT_HANDLES
            | JOB_OBJECT_UILIMIT_READCLIPBOARD
            | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
            | JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    };
    // SAFETY: valid job handle and correctly sized structs.
    unsafe {
        if SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
            || SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicUIRestrictions,
                (&ui as *const JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            ) == 0
        {
            return Err(unavailable("job limits"));
        }
    }
    Ok(job)
}

/// A process attribute list, freed on drop.
struct Attributes {
    buf: Vec<u64>,
}

impl Attributes {
    fn new(count: u32) -> Result<Self, ExecError> {
        let mut size = 0usize;
        // SAFETY: a size query (expected to fail with the size set).
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size) };
        let mut buf = vec![0u64; size.div_ceil(8)];
        // SAFETY: `buf` holds at least `size` bytes, 8-aligned.
        if unsafe {
            InitializeProcThreadAttributeList(buf.as_mut_ptr().cast(), count, 0, &mut size)
        } == 0
        {
            return Err(unavailable("attribute list"));
        }
        Ok(Attributes { buf })
    }

    fn list(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buf.as_mut_ptr().cast()
    }

    /// # Safety
    /// `value` must stay valid until the process is created.
    unsafe fn set<T>(&mut self, attr: u32, value: *const T, size: usize) -> Result<(), ExecError> {
        // SAFETY: caller's contract; the list was initialised.
        if unsafe {
            UpdateProcThreadAttribute(
                self.list(),
                0,
                attr as usize,
                value.cast(),
                size,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(unavailable("process attribute"));
        }
        Ok(())
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: initialised in `new`.
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
}

/// Run a confined command (network denied and/or filesystem confined).
pub(crate) fn run_confined(spec: &CommandSpec) -> Result<Output, ExecError> {
    let Some(fs) = &spec.fs else {
        return Err(ExecError::SandboxUnavailable(
            "on Windows, network denial requires filesystem confinement (AppContainer)".into(),
        ));
    };
    // The extension of the file that will actually run: Windows drops
    // trailing dots and spaces, so `x.bat.` is a batch file (round 2 W-3).
    // Only executables run confined.
    let real = std::fs::canonicalize(&spec.program).map_err(|e| ExecError::Io(e.to_string()))?;
    let ext = real
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    if !matches!(ext.as_deref(), Some("exe" | "com")) {
        return Err(ExecError::SandboxUnavailable(format!(
            "only .exe and .com programs run confined, not {}",
            real.display()
        )));
    }

    let sid = container_sid(&profile_name(fs, &spec.cwd))?;
    let home = std::env::home_dir();
    let too_broad = |p: &Path| p.parent().is_none() || home.as_deref() == Some(p);
    for p in &fs.read_write {
        if too_broad(p) {
            return Err(ExecError::SandboxUnavailable(format!(
                "refusing to grant a drive root or the user profile: {}",
                p.display()
            )));
        }
        // "Modify": no WRITE_DAC, WRITE_OWNER or ACCESS_SYSTEM_SECURITY,
        // so a command cannot rewrite the workspace's ACLs, lock the user
        // out or remove its own grant (round 2 W-1).
        grant(p, sid.ptr, MODIFY).map_err(ExecError::SandboxUnavailable)?;
    }
    for p in fs.read_only.iter().filter(|p| p.exists() && !too_broad(p)) {
        // Best effort: system directories are readable by every
        // AppContainer already, and their ACLs belong to the system.
        let _ = grant(p, sid.ptr, FILE_GENERIC_READ | FILE_GENERIC_EXECUTE);
    }

    let internet;
    let mut caps: Vec<SID_AND_ATTRIBUTES> = Vec::new();
    if spec.network == Network::Allow {
        internet = string_sid(INTERNET_CLIENT_SID)?;
        caps.push(SID_AND_ATTRIBUTES {
            Sid: internet.ptr,
            Attributes: SE_GROUP_ENABLED,
        });
    }
    let sec = SECURITY_CAPABILITIES {
        AppContainerSid: sid.ptr,
        Capabilities: if caps.is_empty() {
            std::ptr::null_mut()
        } else {
            caps.as_mut_ptr()
        },
        CapabilityCount: caps.len() as u32,
        Reserved: 0,
    };

    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (out_r, out_w) = pipe(&sa)?;
    let (err_r, err_w) = pipe(&sa)?;
    let nul = wide(OsStr::new("NUL"));
    // SAFETY: valid name and attributes.
    let stdin = owned(
        unsafe {
            CreateFileW(
                nul.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &sa,
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        },
        "NUL",
    )?;
    let job = job(&spec.limits)?;

    let inherit: [HANDLE; 3] = [
        stdin.as_raw_handle(),
        out_w.as_raw_handle(),
        err_w.as_raw_handle(),
    ];
    let jobs: [HANDLE; 1] = [job.as_raw_handle()];
    let mut attrs = Attributes::new(3)?;
    // SAFETY: the values live until CreateProcessW returns.
    unsafe {
        attrs.set(
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            &sec,
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
        )?;
        attrs.set(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            inherit.as_ptr(),
            std::mem::size_of_val(&inherit),
        )?;
        attrs.set(
            PROC_THREAD_ATTRIBUTE_JOB_LIST,
            jobs.as_ptr(),
            std::mem::size_of_val(&jobs),
        )?;
    }
    let mut si = STARTUPINFOEXW::default();
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = inherit[0];
    si.StartupInfo.hStdOutput = inherit[1];
    si.StartupInfo.hStdError = inherit[2];
    si.lpAttributeList = attrs.list();

    let program = plain(&real);
    let app = wide(program.as_os_str());
    let mut cmdline = wide(OsStr::new(&command_line(&program, &spec.args)));
    let env = env_block(&spec.env);
    let cwd = wide(plain(&spec.cwd).as_os_str());
    let mut pi = PROCESS_INFORMATION {
        hProcess: std::ptr::null_mut(),
        hThread: std::ptr::null_mut(),
        dwProcessId: 0,
        dwThreadId: 0,
    };
    let start = Instant::now();
    // SAFETY: every pointer is valid for the call; the attribute values
    // are alive; handles in the list are inheritable.
    let ok = unsafe {
        CreateProcessW(
            app.as_ptr(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            env.as_ptr().cast(),
            cwd.as_ptr(),
            &si.StartupInfo,
            &mut pi,
        )
    };
    if ok == 0 {
        // SAFETY: no preconditions.
        let code = unsafe { GetLastError() };
        return Err(ExecError::Spawn(format!(
            "CreateProcessW in AppContainer: {}",
            IoError::from_raw_os_error(code as i32)
        )));
    }
    let process = owned(pi.hProcess, "process")?;
    drop(owned(pi.hThread, "thread")?);
    drop(attrs);
    // The child holds its own copies; ours would keep the pipes open.
    drop((stdin, out_w, err_w));

    let cap = spec.limits.max_output_bytes;
    let out_reader = spawn_reader(Some(std::fs::File::from(out_r)), cap);
    let err_reader = spawn_reader(Some(std::fs::File::from(err_r)), cap);

    let kill = || {
        // SAFETY: a job handle we own.
        unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
    };
    let exit = loop {
        // SAFETY: a process handle we own.
        match unsafe { WaitForSingleObject(process.as_raw_handle(), 5) } {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                // SAFETY: as above.
                if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
                    let err = IoError::last_os_error();
                    kill();
                    return Err(ExecError::Io(format!("exit code: {err}")));
                }
                break ExitKind::Exited(code as i32);
            }
            WAIT_TIMEOUT if start.elapsed() >= spec.limits.wall_timeout => {
                kill();
                // SAFETY: as above.
                unsafe { WaitForSingleObject(process.as_raw_handle(), 5_000) };
                break ExitKind::TimedOut;
            }
            WAIT_TIMEOUT => {}
            _ => {
                // Read before TerminateJobObject overwrites it (W-12).
                let err = IoError::last_os_error();
                kill();
                return Err(ExecError::Io(err.to_string()));
            }
        }
    };
    // Background processes may outlive the leader and hold the pipes.
    kill();

    let mut acct: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: zeroed is valid for this plain struct.
    let mut ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: valid job handle and correctly sized out-structs.
    let rusage = unsafe {
        let a = QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            (&mut acct as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        );
        let e = QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&mut ext as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        );
        (a != 0 && e != 0).then(|| Rusage {
            user_cpu: Duration::from_nanos(acct.TotalUserTime.max(0) as u64 * 100),
            sys_cpu: Duration::from_nanos(acct.TotalKernelTime.max(0) as u64 * 100),
            // Peak committed memory of one process: Windows' nearest
            // counterpart to a peak resident size.
            max_rss_bytes: ext.PeakProcessMemoryUsed as u64,
        })
    };
    // The job refuses allocations past its limit; a failure with the peak
    // at the limit is reported as such.
    let limit = spec.limits.address_space_bytes;
    let exit = match exit {
        ExitKind::Exited(c)
            if c != 0
                && (ext.PeakJobMemoryUsed as u64).saturating_mul(100)
                    >= limit.saturating_mul(95) =>
        {
            ExitKind::MemoryExceeded
        }
        e => e,
    };

    let deadline = Instant::now() + Duration::from_secs(2);
    let (stdout, t1) = out_reader.take(deadline);
    let (stderr, t2) = err_reader.take(deadline);
    Ok(Output {
        exit,
        stdout,
        stderr,
        truncated: t1 || t2,
        isolation: Isolation {
            network: (spec.network == Network::Deny).then_some(NetworkMethod::AppContainer),
            fs_confined: true,
            ..Isolation::default()
        },
        elapsed: start.elapsed(),
        rusage,
    })
}
