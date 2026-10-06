//! Linux and macOS back end (descriptor-relative, via `rustix`).
//!
//! * Linux: `openat2(2)` with `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS` for
//!   resolution, and additionally `RESOLVE_NO_SYMLINKS` for every operation
//!   after resolution. Canonical paths are read back via `/proc/self/fd`.
//! * macOS: `openat(2)` then `fcntl(F_GETPATH)` on the descriptor, refusing
//!   anything whose real path is outside the root; operations after
//!   resolution use `O_NOFOLLOW_ANY`.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{self as rfs, AtFlags, FileType, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

use crate::{DirEntryInfo, Expect, FileId, FsError, Kind, MAX_READ_BYTES, Meta};

#[derive(Debug)]
pub(crate) struct Root {
    fd: OwnedFd,
}

fn io(e: Errno) -> FsError {
    FsError::Io(std::io::Error::from(e).to_string())
}

fn meta_of(st: &rfs::Stat) -> Meta {
    let kind = match FileType::from_raw_mode(st.st_mode as _) {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Dir,
        _ => Kind::Other,
    };
    Meta {
        id: FileId {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
        },
        kind,
        nlink: st.st_nlink as u64,
        len: st.st_size as u64,
        mode: (st.st_mode as u32) & 0o7777,
    }
}

fn rel_arg(rel: &Path) -> &Path {
    if rel.as_os_str().is_empty() {
        Path::new(".")
    } else {
        rel
    }
}

#[cfg(target_os = "linux")]
fn fd_path(fd: BorrowedFd<'_>) -> Result<PathBuf, FsError> {
    use std::os::fd::AsRawFd;
    let p = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
        .map_err(|e| FsError::Io(format!("cannot read descriptor path (is /proc mounted?): {e}")))?;
    if !p.is_absolute() || p.as_os_str().as_encoded_bytes().ends_with(b" (deleted)") {
        return Err(FsError::Changed);
    }
    Ok(p)
}

#[cfg(target_os = "macos")]
fn fd_path(fd: BorrowedFd<'_>) -> Result<PathBuf, FsError> {
    use std::os::unix::ffi::OsStringExt;
    let c = rfs::getpath(fd).map_err(io)?;
    Ok(PathBuf::from(OsString::from_vec(c.into_bytes())))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn fd_path(_fd: BorrowedFd<'_>) -> Result<PathBuf, FsError> {
    Err(FsError::Io("unsupported platform: cannot read a descriptor's path".into()))
}

/// Component-wise comparison of two paths using [`names_equivalent`].
fn paths_equivalent(a: &Path, b: &Path) -> bool {
    let mut ai = a.components();
    let mut bi = b.components();
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if names_equivalent(x.as_os_str(), y.as_os_str()) => {}
            _ => return false,
        }
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

#[cfg(target_os = "macos")]
pub(crate) fn names_equivalent(a: &OsStr, b: &OsStr) -> bool {
    use unicode_normalization::UnicodeNormalization;
    if a == b {
        return true;
    }
    match (a.to_str(), b.to_str()) {
        (Some(x), Some(y)) => {
            let nx: String = x.nfc().collect::<String>().to_lowercase();
            let ny: String = y.nfc().collect::<String>().to_lowercase();
            nx == ny
        }
        _ => false,
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn names_equivalent(a: &OsStr, b: &OsStr) -> bool {
    a == b
}

pub(crate) fn open_root(path: &Path) -> Result<(Root, PathBuf), FsError> {
    let fd = rfs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| match e {
        Errno::NOENT => FsError::NotFound,
        Errno::NOTDIR => FsError::NotADirectory,
        e => io(e),
    })?;
    let canonical = fd_path(fd.as_fd())?;
    Ok((Root { fd }, canonical))
}

/// Resolve following symlinks, but never outside the root.
#[cfg(target_os = "linux")]
fn open_follow(root: &Root, root_path: &Path, rel: &Path) -> Result<OwnedFd, FsError> {
    use rustix::fs::ResolveFlags;
    let _ = root_path;
    let mut tries = 0;
    loop {
        let r = rfs::openat2(
            &root.fd,
            rel_arg(rel),
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS,
        );
        match r {
            Ok(fd) => return Ok(fd),
            // The kernel asks us to retry if a concurrent rename raced the
            // lookup. Bounded.
            Err(Errno::AGAIN) if tries < 16 => tries += 1,
            Err(Errno::NOENT) => return Err(FsError::NotFound),
            Err(Errno::XDEV) => return Err(FsError::EscapesWorkspace),
            Err(Errno::NOTDIR) => return Err(FsError::NotADirectory),
            Err(Errno::LOOP) => {
                return Err(FsError::Io("too many levels of symbolic links".into()));
            }
            Err(Errno::NOSYS) => {
                return Err(FsError::Io(
                    "openat2(2) unavailable (Linux >= 5.6 required); refusing".into(),
                ));
            }
            Err(e) => return Err(io(e)),
        }
    }
}

#[cfg(target_os = "macos")]
fn open_follow(root: &Root, root_path: &Path, rel: &Path) -> Result<OwnedFd, FsError> {
    let fd = rfs::openat(
        &root.fd,
        rel_arg(rel),
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| match e {
        Errno::NOENT => FsError::NotFound,
        Errno::NOTDIR => FsError::NotADirectory,
        e => io(e),
    })?;
    // macOS has no RESOLVE_BENEATH: verify the descriptor's real path.
    let real = fd_path(fd.as_fd())?;
    if !starts_with_equivalent(&real, root_path) {
        return Err(FsError::EscapesWorkspace);
    }
    Ok(fd)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn open_follow(_root: &Root, _root_path: &Path, _rel: &Path) -> Result<OwnedFd, FsError> {
    Err(FsError::Io("unsupported platform".into()))
}

/// Open a canonical (symlink-free) relative path with symlink following
/// disabled for every component, then verify the descriptor's real path.
fn open_strict(
    root: &Root,
    root_path: &Path,
    rel: &Path,
    flags: OFlags,
) -> Result<OwnedFd, FsError> {
    let fd = open_strict_raw(root, rel, flags)?;
    let real = fd_path(fd.as_fd())?;
    let expected = if rel.as_os_str().is_empty() {
        root_path.to_path_buf()
    } else {
        root_path.join(rel)
    };
    if !paths_equivalent(&real, &expected) {
        return Err(FsError::Changed);
    }
    Ok(fd)
}

fn strict_err(e: Errno) -> FsError {
    match e {
        Errno::NOENT => FsError::NotFound,
        Errno::LOOP => FsError::SymlinkInStrictPath,
        Errno::XDEV => FsError::EscapesWorkspace,
        Errno::NOTDIR => FsError::NotADirectory,
        e => io(e),
    }
}

#[cfg(target_os = "linux")]
fn open_strict_raw(root: &Root, rel: &Path, flags: OFlags) -> Result<OwnedFd, FsError> {
    use rustix::fs::ResolveFlags;
    let mut tries = 0;
    loop {
        match rfs::openat2(
            &root.fd,
            rel_arg(rel),
            // No O_NOFOLLOW: with O_PATH it would return a handle on a
            // trailing symlink instead of failing. RESOLVE_NO_SYMLINKS
            // already rejects symlinks in every component.
            flags | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        ) {
            Ok(fd) => return Ok(fd),
            Err(Errno::AGAIN) if tries < 16 => tries += 1,
            Err(e) => return Err(strict_err(e)),
        }
    }
}

#[cfg(target_os = "macos")]
fn open_strict_raw(root: &Root, rel: &Path, flags: OFlags) -> Result<OwnedFd, FsError> {
    rfs::openat(
        &root.fd,
        rel_arg(rel),
        flags | OFlags::CLOEXEC | OFlags::NOFOLLOW_ANY,
        Mode::empty(),
    )
    .map_err(strict_err)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn open_strict_raw(_root: &Root, _rel: &Path, _flags: OFlags) -> Result<OwnedFd, FsError> {
    Err(FsError::Io("unsupported platform".into()))
}

#[cfg(target_os = "linux")]
fn path_only() -> OFlags {
    OFlags::PATH
}
#[cfg(not(target_os = "linux"))]
fn path_only() -> OFlags {
    OFlags::RDONLY | OFlags::NONBLOCK
}

pub(crate) fn resolve_follow(
    root: &Root,
    root_path: &Path,
    rel: &Path,
) -> Result<(PathBuf, Meta), FsError> {
    let fd = open_follow(root, root_path, rel)?;
    let st = rfs::fstat(&fd).map_err(io)?;
    let canonical = fd_path(fd.as_fd())?;
    if !starts_with_equivalent(&canonical, root_path) {
        return Err(FsError::EscapesWorkspace);
    }
    Ok((canonical, meta_of(&st)))
}

pub(crate) fn is_symlink(
    root: &Root,
    _root_path: &Path,
    parent_rel: &Path,
    name: &OsStr,
) -> Result<bool, FsError> {
    let dir = open_strict_raw(root, parent_rel, path_only() | OFlags::DIRECTORY)?;
    match rfs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => Ok(FileType::from_raw_mode(st.st_mode as _) == FileType::Symlink),
        Err(Errno::NOENT) => Ok(false),
        Err(e) => Err(io(e)),
    }
}

fn read_fd_bounded(fd: OwnedFd) -> Result<Vec<u8>, FsError> {
    let mut f = std::fs::File::from(fd);
    let mut buf = Vec::new();
    (&mut f).take(MAX_READ_BYTES + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_READ_BYTES {
        return Err(FsError::TooLarge);
    }
    Ok(buf)
}

pub(crate) fn read_strict(
    root: &Root,
    root_path: &Path,
    rel: &Path,
) -> Result<(Meta, Vec<u8>), FsError> {
    let fd = open_strict(root, root_path, rel, OFlags::RDONLY | OFlags::NONBLOCK)?;
    let meta = meta_of(&rfs::fstat(&fd).map_err(io)?);
    if meta.kind != Kind::File {
        return Err(FsError::NotAFile);
    }
    if meta.len > MAX_READ_BYTES {
        return Err(FsError::TooLarge);
    }
    Ok((meta, read_fd_bounded(fd)?))
}

pub(crate) fn list_strict(
    root: &Root,
    root_path: &Path,
    rel: &Path,
) -> Result<(FileId, Vec<DirEntryInfo>), FsError> {
    let fd = open_strict(root, root_path, rel, OFlags::RDONLY | OFlags::DIRECTORY)?;
    let id = meta_of(&rfs::fstat(&fd).map_err(io)?).id;
    let mut dir = rfs::Dir::read_from(&fd).map_err(io)?;
    let mut out = Vec::new();
    while let Some(ent) = dir.read() {
        let ent = ent.map_err(io)?;
        let name = ent.file_name().to_string_lossy().into_owned();
        if name == "." || name == ".." {
            continue;
        }
        let kind = match ent.file_type() {
            FileType::RegularFile => Kind::File,
            FileType::Directory => Kind::Dir,
            _ => Kind::Other,
        };
        out.push(DirEntryInfo { name, kind });
    }
    Ok((id, out))
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_name() -> OsString {
    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    OsString::from(format!(".anchor-tmp-{}-{n}-{nanos}", std::process::id()))
}

fn exchange_unsupported(e: Errno) -> bool {
    e == Errno::INVAL || e == Errno::NOSYS || e == Errno::NOTSUP || e == Errno::OPNOTSUPP
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_atomic(
    root: &Root,
    root_path: &Path,
    base_rel: &Path,
    missing_dirs: &[OsString],
    name: &OsStr,
    content: &[u8],
    expect: &Expect,
) -> Result<(), FsError> {
    let mut dir = open_strict(root, root_path, base_rel, OFlags::RDONLY | OFlags::DIRECTORY)?;
    for m in missing_dirs {
        rfs::mkdirat(&dir, m.as_os_str(), Mode::from_bits_truncate(0o755)).map_err(|e| match e {
            Errno::EXIST => FsError::Changed,
            e => io(e),
        })?;
        dir = rfs::openat(
            &dir,
            m.as_os_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(strict_err)?;
    }

    // Verify the current target against the precondition.
    let mode = match expect {
        Expect::Absent => match rfs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Err(Errno::NOENT) => 0o644,
            Ok(_) => return Err(FsError::Changed),
            Err(e) => return Err(io(e)),
        },
        Expect::Existing { id, sha256 } => {
            let f = rfs::openat(
                &dir,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(strict_err)?;
            let meta = meta_of(&rfs::fstat(&f).map_err(io)?);
            if meta.kind != Kind::File {
                return Err(FsError::NotAFile);
            }
            if meta.id != *id {
                return Err(FsError::Changed);
            }
            if meta.nlink > 1 {
                return Err(FsError::HardlinkRefused(meta.nlink));
            }
            if crate::sha256(&read_fd_bounded(f)?) != *sha256 {
                return Err(FsError::ContentMismatch);
            }
            meta.mode
        }
    };

    let tmp = tmp_name();
    let tmp_fd = rfs::openat(
        &dir,
        tmp.as_os_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(mode as _),
    )
    .map_err(io)?;
    let cleanup = |dir: &OwnedFd| {
        let _ = rfs::unlinkat(dir, tmp.as_os_str(), AtFlags::empty());
    };
    let mut f = std::fs::File::from(tmp_fd);
    if let Err(e) = f.write_all(content).and_then(|_| f.sync_all()) {
        cleanup(&dir);
        return Err(e.into());
    }
    drop(f);

    let result = match expect {
        Expect::Absent => rfs::renameat_with(&dir, tmp.as_os_str(), &dir, name, RenameFlags::NOREPLACE)
            .map_err(|e| match e {
                Errno::EXIST => FsError::Changed,
                e if exchange_unsupported(e) => FsError::AtomicReplaceUnsupported,
                e => io(e),
            }),
        Expect::Existing { id, .. } => {
            match rfs::renameat_with(&dir, tmp.as_os_str(), &dir, name, RenameFlags::EXCHANGE) {
                Err(e) if exchange_unsupported(e) => Err(FsError::AtomicReplaceUnsupported),
                Err(e) => Err(strict_err(e)),
                Ok(()) => {
                    // `tmp` now names the file we displaced. It must be the
                    // approved one; otherwise put it back.
                    let ok = match rfs::statat(&dir, tmp.as_os_str(), AtFlags::SYMLINK_NOFOLLOW) {
                        Ok(st) => {
                            let m = meta_of(&st);
                            m.kind == Kind::File && m.id == *id
                        }
                        Err(_) => false,
                    };
                    if ok {
                        Ok(())
                    } else {
                        let _ = rfs::renameat_with(
                            &dir,
                            tmp.as_os_str(),
                            &dir,
                            name,
                            RenameFlags::EXCHANGE,
                        );
                        Err(FsError::Changed)
                    }
                }
            }
        }
    };
    match result {
        Ok(()) => {
            if matches!(expect, Expect::Existing { .. }) {
                // Remove the displaced original (its pre-image was captured
                // by the backend before approval was acted on).
                let _ = rfs::unlinkat(&dir, tmp.as_os_str(), AtFlags::empty());
            }
            rfs::fsync(&dir).map_err(io)?;
            Ok(())
        }
        Err(e) => {
            cleanup(&dir);
            Err(e)
        }
    }
}
