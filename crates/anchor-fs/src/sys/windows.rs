//! Windows back end.
//!
//! STATUS: type-checked in this repository's CI, *not yet executed against
//! the attack suite on Windows hardware* (see docs/threat-model.md).
//!
//! * Resolution opens the target (following reparse points) and reads the
//!   real path back with `GetFinalPathNameByHandleW`, refusing anything
//!   outside the root.
//! * Every later operation opens with `FILE_FLAG_OPEN_REPARSE_POINT`, refuses
//!   a handle whose attributes include `FILE_ATTRIBUTE_REPARSE_POINT`, and
//!   verifies the handle's final path equals the canonical path (which
//!   catches a junction swapped into an intermediate component).
//! * Replacement uses `ReplaceFileW` with a backup name, then verifies the
//!   displaced file is the approved one, reversing the replacement if not.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
    GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW, DeleteFileW,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, GetFinalPathNameByHandleW,
    MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_EXISTING, REPLACEFILE_WRITE_THROUGH, ReplaceFileW,
    VOLUME_NAME_DOS,
};

use crate::{DirEntryInfo, Expect, FileId, FsError, Kind, MAX_READ_BYTES, Meta};

#[derive(Debug)]
pub(crate) struct Root {
    _handle: OwnedHandle,
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn last_err() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn map_open_err(e: std::io::Error) -> FsError {
    match e.raw_os_error().map(|c| c as u32) {
        Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PATH_NOT_FOUND) => FsError::NotFound,
        _ => FsError::Io(e.to_string()),
    }
}

fn open_handle(
    path: &Path,
    access: u32,
    extra_flags: u32,
    disposition: u32,
) -> Result<OwnedHandle, FsError> {
    let w = wide(path);
    // SAFETY: `w` is a NUL-terminated UTF-16 string that outlives the call;
    // all other arguments are plain values or null.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            disposition,
            FILE_FLAG_BACKUP_SEMANTICS | extra_flags,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(map_open_err(last_err()));
    }
    // SAFETY: `h` is a valid handle we exclusively own.
    Ok(unsafe { OwnedHandle::from_raw_handle(h as _) })
}

fn final_path(h: &OwnedHandle) -> Result<PathBuf, FsError> {
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: `buf` is a writable buffer of the stated length.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                h.as_raw_handle() as _,
                buf.as_mut_ptr(),
                buf.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if n == 0 {
            return Err(FsError::Io(last_err().to_string()));
        }
        if n < buf.len() {
            buf.truncate(n);
            break;
        }
        buf.resize(n + 1, 0);
    }
    let s = OsString::from_wide(&buf);
    let s = s.to_string_lossy();
    let stripped = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.into_owned()
    };
    Ok(PathBuf::from(stripped))
}

fn info(h: &OwnedHandle) -> Result<(Meta, u32), FsError> {
    // SAFETY: zeroed is a valid bit pattern for this plain-data struct.
    let mut i: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `i` is a valid out-pointer.
    if unsafe { GetFileInformationByHandle(h.as_raw_handle() as _, &mut i) } == 0 {
        return Err(FsError::Io(last_err().to_string()));
    }
    let attrs = i.dwFileAttributes;
    let kind = if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Kind::Other
    } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        Kind::Dir
    } else {
        Kind::File
    };
    Ok((
        Meta {
            id: FileId {
                dev: i.dwVolumeSerialNumber as u64,
                ino: ((i.nFileIndexHigh as u64) << 32) | i.nFileIndexLow as u64,
            },
            kind,
            nlink: i.nNumberOfLinks as u64,
            len: ((i.nFileSizeHigh as u64) << 32) | i.nFileSizeLow as u64,
            ctime_ns: ((((i.ftLastWriteTime.dwHighDateTime as u64) << 32)
                | i.ftLastWriteTime.dwLowDateTime as u64) as i128)
                * 100,
            mode: 0,
        },
        attrs,
    ))
}

pub(crate) fn names_equivalent(a: &OsStr, b: &OsStr) -> bool {
    // NTFS is case-insensitive by default. Unicode case folding is
    // approximated with `to_lowercase`; exact NTFS upcase-table semantics are
    // tracked in ADR-0013.
    match (a.to_str(), b.to_str()) {
        (Some(x), Some(y)) => x.to_lowercase() == y.to_lowercase(),
        _ => a == b,
    }
}

fn starts_with_equivalent(p: &Path, root: &Path) -> bool {
    let mut pi = p.components();
    for rc in root.components() {
        match pi.next() {
            Some(c) if names_equivalent(c.as_os_str(), rc.as_os_str()) => {}
            _ => return false,
        }
    }
    true
}

fn paths_equivalent(a: &Path, b: &Path) -> bool {
    starts_with_equivalent(a, b) && a.components().count() == b.components().count()
}

pub(crate) fn open_root(path: &Path) -> Result<(Root, PathBuf), FsError> {
    let h = open_handle(path, FILE_READ_ATTRIBUTES, 0, OPEN_EXISTING)?;
    let (meta, _) = info(&h)?;
    if meta.kind != Kind::Dir {
        return Err(FsError::NotADirectory);
    }
    let p = final_path(&h)?;
    Ok((Root { _handle: h }, p))
}

pub(crate) fn resolve_follow(
    _root: &Root,
    root_path: &Path,
    rel: &Path,
) -> Result<(PathBuf, Meta), FsError> {
    let full = root_path.join(rel);
    let h = open_handle(&full, FILE_READ_ATTRIBUTES, 0, OPEN_EXISTING)?;
    let canonical = final_path(&h)?;
    if !starts_with_equivalent(&canonical, root_path) {
        return Err(FsError::EscapesWorkspace);
    }
    let (meta, _) = info(&h)?;
    Ok((canonical, meta))
}

/// Open without following a final reparse point and verify the handle is
/// exactly the expected canonical path.
fn open_strict(root_path: &Path, rel: &Path, access: u32) -> Result<(OwnedHandle, Meta), FsError> {
    let full = root_path.join(rel);
    let h = open_handle(&full, access, FILE_FLAG_OPEN_REPARSE_POINT, OPEN_EXISTING)?;
    let (meta, attrs) = info(&h)?;
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(FsError::SymlinkInStrictPath);
    }
    if !paths_equivalent(&final_path(&h)?, &full) {
        return Err(FsError::Changed);
    }
    Ok((h, meta))
}

pub(crate) fn is_symlink(
    _root: &Root,
    root_path: &Path,
    parent_rel: &Path,
    name: &OsStr,
) -> Result<bool, FsError> {
    let full = root_path.join(parent_rel).join(name);
    match open_handle(
        &full,
        FILE_READ_ATTRIBUTES,
        FILE_FLAG_OPEN_REPARSE_POINT,
        OPEN_EXISTING,
    ) {
        Ok(h) => Ok(info(&h)?.1 & FILE_ATTRIBUTE_REPARSE_POINT != 0),
        Err(FsError::NotFound) => Ok(false),
        Err(e) => Err(e),
    }
}

pub(crate) fn read_strict(
    _root: &Root,
    root_path: &Path,
    rel: &Path,
) -> Result<(Meta, Vec<u8>), FsError> {
    let (h, meta) = open_strict(root_path, rel, GENERIC_READ)?;
    if meta.kind != Kind::File {
        return Err(FsError::NotAFile);
    }
    if meta.len > MAX_READ_BYTES {
        return Err(FsError::TooLarge);
    }
    let mut f = std::fs::File::from(h);
    let mut buf = Vec::new();
    (&mut f).take(MAX_READ_BYTES + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_READ_BYTES {
        return Err(FsError::TooLarge);
    }
    Ok((meta, buf))
}

pub(crate) fn list_strict(
    _root: &Root,
    root_path: &Path,
    rel: &Path,
) -> Result<(FileId, Vec<DirEntryInfo>), FsError> {
    let (_h, meta) = open_strict(root_path, rel, FILE_READ_ATTRIBUTES)?;
    if meta.kind != Kind::Dir {
        return Err(FsError::NotADirectory);
    }
    // Holding `_h` open (without FILE_SHARE_DELETE semantics for renames of
    // the directory itself being blocked is not guaranteed); re-verify after.
    let mut out = Vec::new();
    for ent in std::fs::read_dir(root_path.join(rel))? {
        let ent = ent?;
        let ft = ent.file_type()?;
        let kind = if ft.is_symlink() {
            Kind::Other
        } else if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else {
            Kind::Other
        };
        out.push(DirEntryInfo {
            name: ent.file_name().to_string_lossy().into_owned(),
            kind,
        });
    }
    let (_h2, meta2) = open_strict(root_path, rel, FILE_READ_ATTRIBUTES)?;
    if meta2.id != meta.id {
        return Err(FsError::Changed);
    }
    Ok((meta.id, out))
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_name(tag: &str) -> OsString {
    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    OsString::from(format!(".anchor-{tag}-{}-{n}", std::process::id()))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_atomic(
    _root: &Root,
    root_path: &Path,
    base_rel: &Path,
    missing_dirs: &[OsString],
    name: &OsStr,
    content: &[u8],
    expect: &Expect,
) -> Result<(), FsError> {
    let (_dir, dmeta) = open_strict(root_path, base_rel, FILE_READ_ATTRIBUTES)?;
    if dmeta.kind != Kind::Dir {
        return Err(FsError::NotADirectory);
    }
    let mut parent_rel = base_rel.to_path_buf();
    for m in missing_dirs {
        parent_rel.push(m);
        let w = wide(&root_path.join(&parent_rel));
        // SAFETY: NUL-terminated wide string; null security attributes.
        if unsafe { CreateDirectoryW(w.as_ptr(), std::ptr::null()) } == 0 {
            let e = last_err();
            return Err(match e.raw_os_error().map(|c| c as u32) {
                Some(ERROR_ALREADY_EXISTS) => FsError::Changed,
                _ => FsError::Io(e.to_string()),
            });
        }
        open_strict(root_path, &parent_rel, FILE_READ_ATTRIBUTES)?;
    }
    let target_rel = parent_rel.join(name);
    let target = root_path.join(&target_rel);

    match expect {
        Expect::Absent => {
            if !matches!(
                open_handle(
                    &target,
                    FILE_READ_ATTRIBUTES,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                    OPEN_EXISTING
                ),
                Err(FsError::NotFound)
            ) {
                return Err(FsError::Changed);
            }
        }
        Expect::Existing { id, sha256 } => {
            let (h, meta) = open_strict(root_path, &target_rel, GENERIC_READ)?;
            if meta.kind != Kind::File {
                return Err(FsError::NotAFile);
            }
            if meta.id != *id {
                return Err(FsError::Changed);
            }
            if meta.nlink > 1 {
                return Err(FsError::HardlinkRefused(meta.nlink));
            }
            let mut f = std::fs::File::from(h);
            let mut buf = Vec::new();
            (&mut f).take(MAX_READ_BYTES + 1).read_to_end(&mut buf)?;
            if crate::sha256(&buf) != *sha256 {
                return Err(FsError::ContentMismatch);
            }
        }
    }

    let tmp = root_path.join(&parent_rel).join(tmp_name("tmp"));
    {
        let h = open_handle(
            &tmp,
            GENERIC_WRITE,
            FILE_FLAG_OPEN_REPARSE_POINT,
            CREATE_NEW,
        )?;
        let mut f = std::fs::File::from(h);
        if let Err(e) = f.write_all(content).and_then(|_| f.sync_all()) {
            let w = wide(&tmp);
            // SAFETY: NUL-terminated wide string.
            unsafe { DeleteFileW(w.as_ptr()) };
            return Err(e.into());
        }
    }
    // Re-verify the parent directory has not been swapped for a junction.
    open_strict(root_path, &parent_rel, FILE_READ_ATTRIBUTES)?;

    let wt = wide(&tmp);
    let wtarget = wide(&target);
    let delete_tmp = || {
        // SAFETY: NUL-terminated wide string.
        unsafe { DeleteFileW(wt.as_ptr()) };
    };
    match expect {
        Expect::Absent => {
            // SAFETY: NUL-terminated wide strings.
            if unsafe { MoveFileExW(wt.as_ptr(), wtarget.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
                let e = last_err();
                delete_tmp();
                return Err(match e.raw_os_error().map(|c| c as u32) {
                    Some(ERROR_ALREADY_EXISTS) | Some(ERROR_FILE_EXISTS) => FsError::Changed,
                    _ => FsError::Io(e.to_string()),
                });
            }
        }
        Expect::Existing { id, .. } => {
            let backup = root_path.join(&parent_rel).join(tmp_name("old"));
            let wb = wide(&backup);
            // SAFETY: NUL-terminated wide strings; reserved pointers null.
            let ok = unsafe {
                ReplaceFileW(
                    wtarget.as_ptr(),
                    wt.as_ptr(),
                    wb.as_ptr(),
                    REPLACEFILE_WRITE_THROUGH,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            if ok == 0 {
                let e = last_err();
                delete_tmp();
                return Err(FsError::Io(e.to_string()));
            }
            let displaced_ok = open_handle(
                &backup,
                FILE_READ_ATTRIBUTES,
                FILE_FLAG_OPEN_REPARSE_POINT,
                OPEN_EXISTING,
            )
            .and_then(|h| info(&h))
            .map(|(m, _)| m.kind == Kind::File && m.id == *id)
            .unwrap_or(false);
            if !displaced_ok {
                // SAFETY: as above.
                unsafe {
                    ReplaceFileW(
                        wtarget.as_ptr(),
                        wb.as_ptr(),
                        std::ptr::null(),
                        REPLACEFILE_WRITE_THROUGH,
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                };
                return Err(FsError::Changed);
            }
            // SAFETY: NUL-terminated wide string.
            unsafe { DeleteFileW(wb.as_ptr()) };
        }
    }
    Ok(())
}

pub(crate) fn remove_strict(
    _root: &Root,
    root_path: &Path,
    parent_rel: &Path,
    name: &OsStr,
    expected: &Meta,
) -> Result<(), FsError> {
    let rel = parent_rel.join(name);
    let (h, meta) = open_strict(root_path, &rel, FILE_READ_ATTRIBUTES)?;
    if meta.kind != Kind::File {
        return Err(FsError::NotAFile);
    }
    if !meta.same_version(expected) {
        return Err(FsError::Changed);
    }
    drop(h);
    let w = wide(&root_path.join(&rel));
    // SAFETY: NUL-terminated wide string.
    if unsafe { DeleteFileW(w.as_ptr()) } == 0 {
        return Err(FsError::Io(last_err().to_string()));
    }
    Ok(())
}
