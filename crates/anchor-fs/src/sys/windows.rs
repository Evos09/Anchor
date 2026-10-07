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
//! * Every mutation first *pins* the directory chain from the root to the
//!   parent: each directory is opened without `FILE_SHARE_DELETE`, so none
//!   can be renamed, deleted or replaced by a junction until the operation
//!   ends; the path-based calls below then resolve through exactly the
//!   verified directories (review finding P-3). The root stays pinned for
//!   the workspace's lifetime.
//! * Removal goes through the verified handle (`FileDispositionInfo`), and
//!   directories are enumerated through their verified handle
//!   (`FileIdBothDirectoryInfo`), never by path.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES,
    ERROR_PATH_NOT_FOUND, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE, DeleteFileW,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO,
    FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo, FileIdBothDirectoryInfo,
    FileIdBothDirectoryRestartInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    GetFinalPathNameByHandleW, MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_EXISTING,
    REPLACEFILE_WRITE_THROUGH, ReplaceFileW, SYNCHRONIZE, SetFileInformationByHandle,
    VOLUME_NAME_DOS,
};

use crate::scan::{ScanDir, ScanEntry, ScanNode, ScanOptions, child, same_scanned_version};
use crate::{DirEntryInfo, Expect, FileId, FsError, Kind, MAX_READ_BYTES, MAX_SCAN_DEPTH, Meta};

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
    open_shared(
        path,
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        extra_flags,
        disposition,
    )
}

fn open_shared(
    path: &Path,
    access: u32,
    share: u32,
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
            share,
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

fn info(h: &impl AsRawHandle) -> Result<(Meta, u32), FsError> {
    // SAFETY: zeroed is a valid bit pattern for this plain-data struct.
    let mut i: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `i` is a valid out-pointer.
    if unsafe { GetFileInformationByHandle(h.as_raw_handle() as _, &mut i) } == 0 {
        return Err(FsError::Io(last_err().to_string()));
    }
    let attrs = i.dwFileAttributes;
    let kind = if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Kind::Symlink
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
            ctime_ns: filetime_ns(
                ((i.ftLastWriteTime.dwHighDateTime as u64) << 32)
                    | i.ftLastWriteTime.dwLowDateTime as u64,
            ),
            mtime_ns: filetime_ns(
                ((i.ftLastWriteTime.dwHighDateTime as u64) << 32)
                    | i.ftLastWriteTime.dwLowDateTime as u64,
            ),
            mode: 0,
        },
        attrs,
    ))
}

/// FILETIME (100 ns ticks since 1601) to ns since 1970. Windows has no inode change time; the
/// last-write time stands in for both ctime and mtime.
/// Converted to the Unix epoch, so times compare with `SystemTime`.
fn filetime_ns(t: u64) -> i128 {
    const UNIX_EPOCH_IN_FILETIME: i128 = 116_444_736_000_000_000;
    (t as i128 - UNIX_EPOCH_IN_FILETIME) * 100
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
    // Pinned for the workspace's lifetime: the root cannot be renamed or
    // replaced by a junction while it is open.
    let h = open_shared(
        path,
        FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        0,
        OPEN_EXISTING,
    )?;
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

/// Open the directory at `full` without `FILE_SHARE_DELETE` (it cannot be
/// renamed or removed while the handle lives), refusing a reparse point
/// and verifying the handle is exactly `full`.
fn pin_dir(full: &Path, access: u32) -> Result<(OwnedHandle, Meta), FsError> {
    let h = open_shared(
        full,
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        FILE_FLAG_OPEN_REPARSE_POINT,
        OPEN_EXISTING,
    )?;
    let (meta, attrs) = info(&h)?;
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(FsError::SymlinkInStrictPath);
    }
    if meta.kind != Kind::Dir {
        return Err(FsError::NotADirectory);
    }
    if !paths_equivalent(&final_path(&h)?, full) {
        return Err(FsError::Changed);
    }
    Ok((h, meta))
}

/// Pin every directory from the root to `rel` (inclusive). The returned
/// handles keep the chain fixed until they are dropped.
fn pin_chain(root_path: &Path, rel: &Path) -> Result<Vec<OwnedHandle>, FsError> {
    let mut cur = root_path.to_path_buf();
    let mut pins = vec![pin_dir(&cur, FILE_READ_ATTRIBUTES)?.0];
    for c in rel.components() {
        cur.push(c);
        pins.push(pin_dir(&cur, FILE_READ_ATTRIBUTES)?.0);
    }
    Ok(pins)
}

/// One directory entry read through a directory handle.
struct RawEntry {
    name: OsString,
    attrs: u32,
    len: u64,
    last_write: u64,
}

/// Enumerate the directory behind `h` (opened with `FILE_LIST_DIRECTORY`)
/// by handle, never by path.
fn enumerate(h: &OwnedHandle) -> Result<Vec<RawEntry>, FsError> {
    let mut buf = vec![0u64; 8 * 1024];
    let mut class = FileIdBothDirectoryRestartInfo;
    let mut out = Vec::new();
    loop {
        // SAFETY: `buf` is a writable, 8-aligned buffer of the stated size.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                h.as_raw_handle() as _,
                class,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
            )
        };
        class = FileIdBothDirectoryInfo;
        if ok == 0 {
            let e = last_err();
            if e.raw_os_error().map(|c| c as u32) == Some(ERROR_NO_MORE_FILES) {
                break;
            }
            return Err(FsError::Io(e.to_string()));
        }
        let base = buf.as_ptr().cast::<u8>();
        let mut off = 0usize;
        loop {
            // SAFETY: the system filled the buffer with a chain of 8-aligned
            // records; `off` follows `NextEntryOffset` within it.
            let (next, entry) = unsafe {
                let p = base.add(off).cast::<FILE_ID_BOTH_DIR_INFO>();
                let n = (*p).FileNameLength as usize / 2;
                let name_ptr = std::ptr::addr_of!((*p).FileName).cast::<u16>();
                let name = OsString::from_wide(std::slice::from_raw_parts(name_ptr, n));
                (
                    (*p).NextEntryOffset as usize,
                    RawEntry {
                        name,
                        attrs: (*p).FileAttributes,
                        len: (*p).EndOfFile.max(0) as u64,
                        last_write: (*p).LastWriteTime as u64,
                    },
                )
            };
            if entry.name != "." && entry.name != ".." {
                out.push(entry);
            }
            if next == 0 {
                break;
            }
            off += next;
        }
    }
    Ok(out)
}

fn kind_of_attrs(attrs: u32) -> Kind {
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Kind::Symlink
    } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        Kind::Dir
    } else {
        Kind::File
    }
}

/// Delete the object behind `h` (opened with `DELETE`) on close.
fn delete_by_handle(h: OwnedHandle) -> Result<(), FsError> {
    let d = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: valid handle and a correctly sized struct.
    if unsafe {
        SetFileInformationByHandle(
            h.as_raw_handle() as _,
            FileDispositionInfo,
            (&d as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(FsError::Io(last_err().to_string()));
    }
    drop(h);
    Ok(())
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
    // Enumerated through the verified, pinned handle (ADR-0013).
    let _pins = pin_chain(root_path, rel.parent().unwrap_or(Path::new("")))?;
    let (h, meta) = pin_dir(
        &root_path.join(rel),
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
    )?;
    let out = enumerate(&h)?
        .into_iter()
        .map(|e| DirEntryInfo {
            name: e.name.to_string_lossy().into_owned(),
            kind: match kind_of_attrs(e.attrs) {
                Kind::Symlink => Kind::Other,
                k => k,
            },
        })
        .collect();
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
    _mode: Option<u32>,
) -> Result<(), FsError> {
    let mut pins = pin_chain(root_path, base_rel)?;
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
        pins.push(pin_dir(&root_path.join(&parent_rel), FILE_READ_ATTRIBUTES)?.0);
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
    // The parent chain is pinned: it cannot have been swapped.

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
    drop(pins);
    Ok(())
}

pub(crate) fn remove_strict(
    _root: &Root,
    root_path: &Path,
    parent_rel: &Path,
    name: &OsStr,
    expected: &Meta,
) -> Result<(), FsError> {
    let _pins = pin_chain(root_path, parent_rel)?;
    let rel = parent_rel.join(name);
    // The object is verified and deleted through one handle.
    let h = open_handle(
        &root_path.join(&rel),
        DELETE | FILE_READ_ATTRIBUTES,
        FILE_FLAG_OPEN_REPARSE_POINT,
        OPEN_EXISTING,
    )?;
    let (meta, _) = info(&h)?;
    if !matches!(meta.kind, Kind::File | Kind::Symlink | Kind::Other) {
        return Err(FsError::NotAFile);
    }
    if meta.kind != Kind::Symlink && !paths_equivalent(&final_path(&h)?, &root_path.join(&rel)) {
        return Err(FsError::Changed);
    }
    if meta.kind != expected.kind || !meta.same_version(expected) {
        return Err(FsError::Changed);
    }
    delete_by_handle(h)
}

// ---------------------------------------------------------------------------
// Snapshot support: scanning and exact (no-follow) operations.
// ---------------------------------------------------------------------------

fn meta_of_std(m: &std::fs::Metadata) -> Meta {
    use std::os::windows::fs::MetadataExt;
    let ft = m.file_type();
    let kind = if ft.is_symlink() || m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Kind::Symlink
    } else if ft.is_dir() {
        Kind::Dir
    } else if ft.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    let t = filetime_ns(m.last_write_time());
    Meta {
        // The stable std API exposes no file index; the scan compares
        // size and times instead, and `open_scanned` reads the real id.
        id: FileId { dev: 0, ino: 0 },
        kind,
        nlink: 1,
        len: m.file_size(),
        ctime_ns: t,
        mtime_ns: t,
        mode: 0,
    }
}

pub(crate) fn scan(
    _root: &Root,
    root_path: &Path,
    opts: &ScanOptions<'_>,
    count: &mut usize,
) -> Result<ScanDir, FsError> {
    let (_h, meta) = open_strict(root_path, Path::new(""), FILE_READ_ATTRIBUTES)?;
    scan_dir(root_path, Path::new(""), meta, 0, opts, count)
}

fn scan_dir(
    root_path: &Path,
    rel: &Path,
    meta: Meta,
    depth: usize,
    opts: &ScanOptions<'_>,
    count: &mut usize,
) -> Result<ScanDir, FsError> {
    if depth >= MAX_SCAN_DEPTH {
        return Err(FsError::TooDeep);
    }
    // Pinned while scanned, and enumerated through its verified handle: a
    // junction swapped in is neither followed nor possible.
    let (h, _) = pin_dir(
        &root_path.join(rel),
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
    )?;
    let mut items = enumerate(&h)?;
    items.sort_by(|a, b| a.name.as_encoded_bytes().cmp(b.name.as_encoded_bytes()));
    let mut entries = Vec::with_capacity(items.len());
    for e in items {
        *count += 1;
        if *count > opts.max_entries {
            return Err(FsError::TooManyEntries(opts.max_entries));
        }
        let name = e.name;
        let t = filetime_ns(e.last_write);
        let m = Meta {
            // As with the std scan before: the file index is read by
            // `open_scanned`; the scan compares size and times.
            id: FileId { dev: 0, ino: 0 },
            kind: kind_of_attrs(e.attrs),
            nlink: 1,
            len: e.len,
            ctime_ns: t,
            mtime_ns: t,
            mode: 0,
        };
        let child_rel = child(rel, &name);
        if (opts.skip)(&child_rel, m.kind) {
            continue;
        }
        let node = match m.kind {
            Kind::Dir if depth + 1 >= MAX_SCAN_DEPTH => ScanNode::Unscanned(m),
            // Unreadable: recorded as is (review finding P-5).
            Kind::Dir
                if std::fs::read_dir(root_path.join(&child_rel))
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied) =>
            {
                ScanNode::Unscanned(m)
            }
            Kind::Dir => ScanNode::Dir(scan_dir(root_path, &child_rel, m, depth + 1, opts, count)?),
            Kind::File => ScanNode::File(m),
            Kind::Symlink => ScanNode::Symlink(m),
            Kind::Other => ScanNode::Other(m),
        };
        entries.push(ScanEntry { name, node });
    }
    drop(h);
    Ok(ScanDir { meta, entries })
}

pub(crate) fn open_read_strict(
    _root: &Root,
    root_path: &Path,
    rel: &Path,
) -> Result<std::fs::File, FsError> {
    let (h, _) = open_strict(root_path, rel, GENERIC_READ)?;
    Ok(std::fs::File::from(h))
}

pub(crate) fn file_meta(f: &std::fs::File) -> Result<Meta, FsError> {
    Ok(info(f)?.0)
}

pub(crate) fn read_link_strict(
    _root: &Root,
    root_path: &Path,
    parent_rel: &Path,
    name: &OsStr,
    expected: &Meta,
) -> Result<OsString, FsError> {
    let _pins = pin_chain(root_path, parent_rel)?;
    let full = root_path.join(parent_rel).join(name);
    let before = meta_of_std(&std::fs::symlink_metadata(&full)?);
    if before.kind != Kind::Symlink || !same_scanned_version(expected, &before) {
        return Err(FsError::Changed);
    }
    let target = std::fs::read_link(&full)?;
    Ok(target.into_os_string())
}

pub(crate) fn lstat_strict(
    _root: &Root,
    root_path: &Path,
    parent_rel: &Path,
    name: &OsStr,
) -> Result<Option<Meta>, FsError> {
    let _pins = pin_chain(root_path, parent_rel)?;
    match open_handle(
        &root_path.join(parent_rel).join(name),
        FILE_READ_ATTRIBUTES,
        FILE_FLAG_OPEN_REPARSE_POINT,
        OPEN_EXISTING,
    ) {
        Ok(h) => Ok(Some(info(&h)?.0)),
        Err(FsError::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) fn mkdir_strict(
    _root: &Root,
    root_path: &Path,
    parent_rel: &Path,
    name: &OsStr,
    _mode: u32,
) -> Result<(), FsError> {
    let _pins = pin_chain(root_path, parent_rel)?;
    let w = wide(&root_path.join(parent_rel).join(name));
    // SAFETY: NUL-terminated wide string; null security attributes.
    if unsafe { CreateDirectoryW(w.as_ptr(), std::ptr::null()) } == 0 {
        let e = last_err();
        return Err(match e.raw_os_error().map(|c| c as u32) {
            Some(ERROR_ALREADY_EXISTS) => FsError::Changed,
            _ => FsError::Io(e.to_string()),
        });
    }
    Ok(())
}

pub(crate) fn rmdir_strict(
    _root: &Root,
    root_path: &Path,
    parent_rel: &Path,
    name: &OsStr,
    expected: &Meta,
) -> Result<(), FsError> {
    let _pins = pin_chain(root_path, parent_rel)?;
    let rel = parent_rel.join(name);
    let (h, meta) = open_strict(root_path, &rel, DELETE | FILE_READ_ATTRIBUTES)?;
    if meta.kind != Kind::Dir {
        return Err(FsError::NotADirectory);
    }
    if meta.id != expected.id {
        return Err(FsError::Changed);
    }
    // Deleted through the verified handle (fails if not empty).
    delete_by_handle(h)
}

pub(crate) fn symlink_strict(
    _root: &Root,
    _root_path: &Path,
    _parent_rel: &Path,
    _name: &OsStr,
    _target: &OsStr,
) -> Result<(), FsError> {
    Err(FsError::Unsupported(
        "creating symlinks (needs Developer Mode or elevation on Windows)",
    ))
}

pub(crate) fn chmod_strict(
    _root: &Root,
    root_path: &Path,
    rel: &Path,
    expected: &Meta,
    _mode: u32,
) -> Result<(), FsError> {
    let (_h, now) = open_strict(root_path, rel, FILE_READ_ATTRIBUTES)?;
    if now.id != expected.id {
        return Err(FsError::Changed);
    }
    Ok(())
}
