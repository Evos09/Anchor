//! §5.1 / §11 attack suite for path resolution and descriptor-based access.

use std::fs;
use std::path::PathBuf;

use anchor_policy::{
    Approved, AuditLog, Decision, Engine, Operation, PermissionRequest, Policy, Tool,
};

use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    outside: PathBuf,
    engine: Engine,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    // Same form Anchor displays (no Windows verbatim prefix).
    let base = normalize_verbatim(&fs::canonicalize(dir.path()).unwrap());
    let ws = base.join("ws");
    let outside = base.join("outside");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret"), b"TOP SECRET").unwrap();
    let audit = AuditLog::open(&base.join("audit.jsonl")).unwrap();
    let policy = Policy::parse("version = 1\n[default]\naction = \"allow\"\n", &base).unwrap();
    let engine = Engine::from_policy(policy, &base, audit);
    Fixture {
        _dir: dir,
        ws,
        outside,
        engine,
    }
}

fn approve(f: &Fixture, w: &Workspace, r: &Resolved, op: Operation) -> Approved {
    let req = PermissionRequest {
        tool: Tool::WriteFile,
        operation: op,
        canonical_target: Some(r.canonical.clone()),
        workspace_root: w.root().to_path_buf(),
        target_exists: r.exists(),
        is_append_only: false,
        blast_radius_count: 1,
        network: false,
    };
    match f.engine.decide(&req) {
        Decision::Allow(a) => a,
        other => panic!("expected allow, got {other:?}"),
    }
}

fn write_new(
    f: &Fixture,
    w: &Workspace,
    path: &str,
    content: &[u8],
) -> Result<WriteReceipt, FsError> {
    let r = w.resolve(path)?;
    let a = approve(f, w, &r, Operation::Create);
    w.write(&r, content, Expect::Absent, a)
}

fn replace(
    f: &Fixture,
    w: &Workspace,
    path: &str,
    content: &[u8],
) -> Result<WriteReceipt, FsError> {
    let r = w.resolve(path)?;
    let cur = w.read(&r)?;
    let a = approve(f, w, &r, Operation::Write);
    w.write(
        &r,
        content,
        Expect::Existing {
            id: cur.meta.id,
            sha256: cur.sha256,
        },
        a,
    )
}

fn assert_outside_untouched(f: &Fixture) {
    assert_eq!(fs::read(f.outside.join("secret")).unwrap(), b"TOP SECRET");
    let names: Vec<_> = fs::read_dir(&f.outside)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        names,
        vec![std::ffi::OsString::from("secret")],
        "outside dir gained files"
    );
}

// ---------------------------------------------------------------------------
// Basic behaviour
// ---------------------------------------------------------------------------

#[test]
fn create_read_replace_roundtrip() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    write_new(&f, &w, "a.txt", b"one").unwrap();
    let r = w.resolve("a.txt").unwrap();
    assert_eq!(r.canonical, f.ws.join("a.txt"));
    assert_eq!(w.read(&r).unwrap().bytes, b"one");
    replace(&f, &w, "a.txt", b"two").unwrap();
    let r = w.resolve("a.txt").unwrap();
    assert_eq!(w.read(&r).unwrap().bytes, b"two");
    // No temp files left behind.
    let names: Vec<_> = fs::read_dir(&f.ws)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("a.txt")]);
}

#[test]
fn creates_missing_directories() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    let r = w.resolve("new/deep/f.txt").unwrap();
    assert_eq!(r.canonical, f.ws.join("new/deep/f.txt"));
    assert_eq!(r.missing_dirs.len(), 2);
    let a = approve(&f, &w, &r, Operation::Create);
    w.write(&r, b"x", Expect::Absent, a).unwrap();
    assert_eq!(fs::read(f.ws.join("new/deep/f.txt")).unwrap(), b"x");
}

#[test]
fn compare_and_swap_preconditions() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    write_new(&f, &w, "a.txt", b"one").unwrap();
    // Absent expected, but the file exists.
    let r = w.resolve("a.txt").unwrap();
    let a = approve(&f, &w, &r, Operation::Create);
    assert_eq!(w.write(&r, b"x", Expect::Absent, a), Err(FsError::Changed));
    // Wrong content hash (file modified since it was read).
    let cur = w.read(&r).unwrap();
    fs::write(f.ws.join("a.txt"), b"concurrently modified").unwrap();
    let r2 = w.resolve("a.txt").unwrap();
    let a = approve(&f, &w, &r2, Operation::Write);
    let res = w.write(
        &r2,
        b"x",
        Expect::Existing {
            id: r2.meta.unwrap().id,
            sha256: cur.sha256,
        },
        a,
    );
    assert_eq!(res, Err(FsError::ContentMismatch));
    assert_eq!(
        fs::read(f.ws.join("a.txt")).unwrap(),
        b"concurrently modified"
    );
}

#[test]
fn approval_must_match_target_and_operation() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    let ra = w.resolve("a.txt").unwrap();
    let rb = w.resolve("b.txt").unwrap();
    let a = approve(&f, &w, &ra, Operation::Create);
    assert_eq!(
        w.write(&rb, b"x", Expect::Absent, a),
        Err(FsError::ApprovalMismatch)
    );
    let read_only = approve(&f, &w, &rb, Operation::Read);
    assert_eq!(
        w.write(&rb, b"x", Expect::Absent, read_only),
        Err(FsError::ApprovalMismatch)
    );
    assert!(!f.ws.join("b.txt").exists());
}

#[test]
fn append_verified_preserves_prefix() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    write_new(&f, &w, "DEVLOG.md", b"2026-01-01 start\n").unwrap();
    let r = w.resolve("DEVLOG.md").unwrap();
    let cur = w.read(&r).unwrap();
    let a = approve(&f, &w, &r, Operation::Append);
    w.append_verified(&r, b"2026-01-02 more\n", cur.sha256, a)
        .unwrap();
    assert_eq!(
        fs::read(f.ws.join("DEVLOG.md")).unwrap(),
        b"2026-01-01 start\n2026-01-02 more\n"
    );
    // Stale hash is refused.
    let r = w.resolve("DEVLOG.md").unwrap();
    let a = approve(&f, &w, &r, Operation::Append);
    assert_eq!(
        w.append_verified(&r, b"x", cur.sha256, a),
        Err(FsError::ContentMismatch)
    );
}

#[test]
fn remove_requires_delete_approval_and_same_file() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    write_new(&f, &w, "gone.txt", b"x").unwrap();
    let r = w.resolve("gone.txt").unwrap();
    let wrong_op = approve(&f, &w, &r, Operation::Write);
    assert_eq!(w.remove(&r, wrong_op), Err(FsError::ApprovalMismatch));
    // Replaced by a different file after resolution: refused.
    fs::remove_file(f.ws.join("gone.txt")).unwrap();
    fs::write(f.ws.join("gone.txt"), b"other").unwrap();
    let a = approve(&f, &w, &r, Operation::Delete);
    assert_eq!(w.remove(&r, a), Err(FsError::Changed));
    let r = w.resolve("gone.txt").unwrap();
    let a = approve(&f, &w, &r, Operation::Delete);
    w.remove(&r, a).unwrap();
    assert!(!f.ws.join("gone.txt").exists());
}

#[test]
fn verbatim_absolute_params_are_accepted() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    fs::write(f.ws.join("v.txt"), b"x").unwrap();
    // `canonicalize` yields the verbatim form on Windows.
    let verbatim = fs::canonicalize(f.ws.join("v.txt")).unwrap();
    let r = w.resolve(verbatim.to_str().unwrap()).unwrap();
    assert_eq!(r.canonical, f.ws.join("v.txt"));
    assert_eq!(normalize_verbatim(&r.canonical), r.canonical);
}

#[cfg(windows)]
#[test]
fn normalize_verbatim_forms() {
    use std::path::Path;
    assert_eq!(
        normalize_verbatim(Path::new(r"\\?\C:\a\b")),
        PathBuf::from(r"C:\a\b")
    );
    assert_eq!(
        normalize_verbatim(Path::new(r"\\?\UNC\srv\share\x")),
        PathBuf::from(r"\\srv\share\x")
    );
    assert_eq!(
        normalize_verbatim(Path::new(r"C:\a")),
        PathBuf::from(r"C:\a")
    );
    assert_eq!(
        normalize_verbatim(Path::new(r"\\?\Volume{x}\a")),
        PathBuf::from(r"\\?\Volume{x}\a")
    );
}

#[test]
fn pure_append_check() {
    assert!(is_pure_append(b"abc", b"abcdef"));
    assert!(is_pure_append(b"abc", b"abc"));
    assert!(!is_pure_append(b"abc", b"abXdef"));
    assert!(!is_pure_append(b"abc", b"ab"));
}

#[test]
fn mutations_are_counted() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    let before = mutations_performed();
    write_new(&f, &w, "c.txt", b"x").unwrap();
    assert!(mutations_performed() > before);
}

#[test]
fn list_dir_is_sorted_and_typed() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    fs::create_dir(f.ws.join("d")).unwrap();
    fs::write(f.ws.join("b"), b"").unwrap();
    let r = w.resolve(".").unwrap();
    let ents = w.list_dir(&r).unwrap();
    assert_eq!(
        ents,
        vec![
            DirEntryInfo {
                name: "b".into(),
                kind: Kind::File
            },
            DirEntryInfo {
                name: "d".into(),
                kind: Kind::Dir
            },
        ]
    );
}

// ---------------------------------------------------------------------------
// `..` traversal and absolute path injection (Threat 3)
// ---------------------------------------------------------------------------

#[test]
fn dotdot_traversal_is_normalised_and_confined() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    assert_eq!(w.confine("a/../b").unwrap(), PathBuf::from("b"));
    assert_eq!(w.confine("./a/./b/..").unwrap(), PathBuf::from("a"));
    assert_eq!(w.confine("a/../../x"), Err(FsError::TraversalEscapesRoot));
    assert_eq!(
        w.resolve("../../../../etc/passwd"),
        Err(FsError::TraversalEscapesRoot)
    );
    assert_eq!(
        w.resolve("../outside/secret"),
        Err(FsError::TraversalEscapesRoot)
    );
    assert_outside_untouched(&f);
}

#[test]
fn absolute_path_injection_is_confined_or_refused() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    assert_eq!(
        w.resolve("/etc/passwd"),
        Err(FsError::AbsoluteOutsideWorkspace)
    );
    let outside_secret = f.outside.join("secret");
    assert_eq!(
        w.resolve(outside_secret.to_str().unwrap()),
        Err(FsError::AbsoluteOutsideWorkspace)
    );
    // An absolute path inside the workspace is accepted and confined.
    fs::write(f.ws.join("in.txt"), b"x").unwrap();
    let inside = f.ws.join("in.txt");
    let r = w.resolve(inside.to_str().unwrap()).unwrap();
    assert_eq!(r.canonical, inside);
    // Absolute path that climbs out after the root prefix.
    let sneaky = format!("{}/../outside/secret", f.ws.display());
    assert_eq!(w.resolve(&sneaky), Err(FsError::TraversalEscapesRoot));
}

#[test]
fn empty_and_nul_params_refused() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    assert_eq!(w.resolve(""), Err(FsError::InvalidParam));
    assert_eq!(w.resolve("a\0b"), Err(FsError::InvalidParam));
}

// ---------------------------------------------------------------------------
// Symlinks (Threats 1 and 2)
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod symlinks {
    use std::os::unix::fs::symlink;
    use std::path::Path;

    use super::*;

    /// Threat 1: a symlink inside the workspace points outside; the write is
    /// refused and nothing outside changes.
    #[test]
    fn symlink_escape_refused() {
        let f = fixture();
        symlink(&f.outside, f.ws.join("link")).unwrap();
        symlink(f.outside.join("secret"), f.ws.join("file_link")).unwrap();
        let w = Workspace::open(&f.ws).unwrap();
        assert_eq!(w.resolve("link"), Err(FsError::EscapesWorkspace));
        assert_eq!(w.resolve("link/secret"), Err(FsError::EscapesWorkspace));
        assert_eq!(w.resolve("link/new_file"), Err(FsError::EscapesWorkspace));
        assert_eq!(w.resolve("file_link"), Err(FsError::EscapesWorkspace));
        assert!(write_new(&f, &w, "link/new_file", b"pwned").is_err());
        assert!(replace(&f, &w, "file_link", b"pwned").is_err());
        assert_outside_untouched(&f);
    }

    /// Relative symlinks that escape via `..` are refused too.
    #[test]
    fn relative_symlink_escape_refused() {
        let f = fixture();
        fs::create_dir(f.ws.join("sub")).unwrap();
        symlink("../../outside", f.ws.join("sub/up")).unwrap();
        let w = Workspace::open(&f.ws).unwrap();
        assert_eq!(w.resolve("sub/up/secret"), Err(FsError::EscapesWorkspace));
    }

    /// The canonical path shown to the user is the real target.
    #[test]
    fn inside_symlink_resolves_to_real_target() {
        let f = fixture();
        fs::create_dir(f.ws.join("real")).unwrap();
        fs::write(f.ws.join("real/settings.json"), b"{}").unwrap();
        symlink("real", f.ws.join("config")).unwrap();
        let w = Workspace::open(&f.ws).unwrap();
        let r = w.resolve("./config/settings.json").unwrap();
        assert_eq!(r.canonical, f.ws.join("real/settings.json"));
        assert_eq!(r.rel, PathBuf::from("real/settings.json"));
        // Not-yet-existing file under a symlinked dir.
        let r = w.resolve("config/new.json").unwrap();
        assert_eq!(r.canonical, f.ws.join("real/new.json"));
    }

    /// Magic links (/proc/self/...) cannot be used to escape.
    #[cfg(target_os = "linux")]
    #[test]
    fn magic_link_refused() {
        let f = fixture();
        symlink("/proc/self/root/etc/passwd", f.ws.join("pw")).unwrap();
        symlink("/proc/self/cwd", f.ws.join("cwd")).unwrap();
        let w = Workspace::open(&f.ws).unwrap();
        assert_eq!(w.resolve("pw"), Err(FsError::EscapesWorkspace));
        assert_eq!(w.resolve("cwd"), Err(FsError::EscapesWorkspace));
        assert_eq!(
            w.resolve("/proc/self/root/etc/passwd"),
            Err(FsError::AbsoluteOutsideWorkspace)
        );
    }

    #[test]
    fn dangling_symlink_target_refused() {
        let f = fixture();
        symlink(f.outside.join("does_not_exist"), f.ws.join("dangling")).unwrap();
        symlink("nowhere_inside", f.ws.join("dangling_in")).unwrap();
        let w = Workspace::open(&f.ws).unwrap();
        assert!(w.resolve("dangling").is_err());
        assert_eq!(w.resolve("dangling_in"), Err(FsError::DanglingSymlink));
        assert!(!f.outside.join("does_not_exist").exists());
    }

    #[test]
    fn symlink_loop_does_not_hang() {
        let f = fixture();
        symlink("b", f.ws.join("a")).unwrap();
        symlink("a", f.ws.join("b")).unwrap();
        let w = Workspace::open(&f.ws).unwrap();
        assert!(w.resolve("a").is_err());
        assert!(w.resolve("a/x").is_err());
    }

    /// Threat 2 (deterministic): the parent directory is swapped for a
    /// symlink to outside *between* resolution/approval and the write. The
    /// write fails; nothing lands outside.
    #[test]
    fn parent_swapped_after_resolution_is_refused() {
        let f = fixture();
        fs::create_dir(f.ws.join("sub")).unwrap();
        let w = Workspace::open(&f.ws).unwrap();
        let r = w.resolve("sub/new.txt").unwrap();
        assert_eq!(r.canonical, f.ws.join("sub/new.txt"));
        let a = approve(&f, &w, &r, Operation::Create);
        // Attacker swaps.
        fs::rename(f.ws.join("sub"), f.ws.join("sub_orig")).unwrap();
        symlink(&f.outside, f.ws.join("sub")).unwrap();
        let res = w.write(&r, b"pwned", Expect::Absent, a);
        assert!(
            matches!(
                res,
                Err(FsError::SymlinkInStrictPath) | Err(FsError::EscapesWorkspace)
            ),
            "{res:?}"
        );
        assert_outside_untouched(&f);
    }

    /// Threat 2 (deterministic): the target file itself is replaced by a
    /// symlink to a file outside after approval. The write fails.
    #[test]
    fn target_swapped_for_symlink_after_resolution_is_refused() {
        let f = fixture();
        fs::write(f.ws.join("t.txt"), b"orig").unwrap();
        let w = Workspace::open(&f.ws).unwrap();
        let r = w.resolve("t.txt").unwrap();
        let cur = w.read(&r).unwrap();
        let a = approve(&f, &w, &r, Operation::Write);
        fs::remove_file(f.ws.join("t.txt")).unwrap();
        symlink(f.outside.join("secret"), f.ws.join("t.txt")).unwrap();
        let res = w.write(
            &r,
            b"pwned",
            Expect::Existing {
                id: cur.meta.id,
                sha256: cur.sha256,
            },
            a,
        );
        assert!(res.is_err(), "{res:?}");
        assert_outside_untouched(&f);
    }

    /// Threat 2 (racy): another thread flips a symlink between an inside and
    /// an outside directory as fast as it can while we resolve and write
    /// through it. Every write must either fail or land inside the
    /// workspace at the path that was resolved; nothing ever lands outside.
    #[test]
    fn symlink_swap_race_never_escapes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let f = fixture();
        fs::create_dir(f.ws.join("inside")).unwrap();
        symlink("inside", f.ws.join("flip")).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let ws = f.ws.clone();
        let outside = f.outside.clone();
        let s2 = stop.clone();
        let flipper = std::thread::spawn(move || {
            let mut n = 0u64;
            while !s2.load(Ordering::Relaxed) {
                let tmp = ws.join(format!(".flip{n}"));
                let target: &Path = if n.is_multiple_of(2) {
                    &outside
                } else {
                    Path::new("inside")
                };
                let _ = symlink(target, &tmp);
                let _ = fs::rename(&tmp, ws.join("flip"));
                n += 1;
            }
            n
        });
        let w = Workspace::open(&f.ws).unwrap();
        let mut ok = 0;
        let mut refused = 0;
        for i in 0..2000 {
            let name = format!("flip/f{i}.txt");
            match w.resolve(&name) {
                Ok(r) => {
                    assert!(r.canonical.starts_with(f.ws.join("inside")), "{r:?}");
                    let a = approve(&f, &w, &r, Operation::Create);
                    match w.write(&r, b"data", Expect::Absent, a) {
                        Ok(rcpt) => {
                            assert!(rcpt.canonical.starts_with(f.ws.join("inside")));
                            ok += 1;
                        }
                        Err(_) => refused += 1,
                    }
                }
                Err(_) => refused += 1,
            }
        }
        stop.store(true, Ordering::Relaxed);
        let flips = flipper.join().unwrap();
        assert!(flips > 100, "flipper barely ran ({flips})");
        assert!(ok + refused == 2000);
        assert_outside_untouched(&f);
        eprintln!("swap race: {ok} writes landed inside, {refused} refused, {flips} flips");
    }
}

// ---------------------------------------------------------------------------
// Hardlinks
// ---------------------------------------------------------------------------

/// A hardlink inside the workspace to a file outside is detected and refused
/// for both reading and writing.
#[cfg(unix)]
#[test]
fn hardlink_attack_refused() {
    let f = fixture();
    fs::hard_link(f.outside.join("secret"), f.ws.join("innocent.txt")).unwrap();
    let w = Workspace::open(&f.ws).unwrap();
    let r = w.resolve("innocent.txt").unwrap();
    assert_eq!(w.read(&r), Err(FsError::HardlinkRefused(2)));
    let a = approve(&f, &w, &r, Operation::Write);
    let res = w.write(
        &r,
        b"pwned",
        Expect::Existing {
            id: r.meta.unwrap().id,
            sha256: sha256(b"TOP SECRET"),
        },
        a,
    );
    assert_eq!(res, Err(FsError::HardlinkRefused(2)));
    assert_outside_untouched(&f);
}

// ---------------------------------------------------------------------------
// Case and Unicode normalisation collisions
// ---------------------------------------------------------------------------

/// On case-insensitive filesystems (macOS and Windows defaults) `File.txt`
/// and `file.txt` are the same file: identity (not spelling) decides.
#[cfg(any(target_os = "macos", windows))]
#[test]
fn case_insensitive_collision_is_same_file() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    write_new(&f, &w, "File.txt", b"x").unwrap();
    let a = w.resolve("File.txt").unwrap();
    let b = w.resolve("file.txt").unwrap();
    assert_eq!(a.meta.unwrap().id, b.meta.unwrap().id);
    // And the canonical path shown is the on-disk spelling.
    assert_eq!(a.canonical, b.canonical);
    // Creating `file.txt` as "new" is refused because it already exists.
    assert!(write_new(&f, &w, "file.txt", b"y").is_err());
}

/// On macOS, NFC and NFD spellings of the same name are the same file.
#[cfg(target_os = "macos")]
#[test]
fn unicode_normalisation_collision_is_same_file() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    let nfc = "caf\u{e9}.txt";
    let nfd = "cafe\u{301}.txt";
    write_new(&f, &w, nfc, b"x").unwrap();
    let a = w.resolve(nfc).unwrap();
    let b = w.resolve(nfd).unwrap();
    assert_eq!(a.meta.unwrap().id, b.meta.unwrap().id);
}

/// On Linux (case-sensitive, no normalisation) the two spellings are
/// distinct files, and identity still comes from the filesystem, never from
/// string comparison.
#[cfg(target_os = "linux")]
#[test]
fn identity_is_filesystem_defined_on_linux() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    write_new(&f, &w, "File.txt", b"x").unwrap();
    write_new(&f, &w, "file.txt", b"y").unwrap();
    let a = w.resolve("File.txt").unwrap();
    let b = w.resolve("file.txt").unwrap();
    assert_ne!(a.meta.unwrap().id, b.meta.unwrap().id);
    let nfc = w.resolve("caf\u{e9}").unwrap();
    let nfd = w.resolve("cafe\u{301}").unwrap();
    assert_ne!(nfc.canonical, nfd.canonical);
}

// ---------------------------------------------------------------------------
// Snapshot support (§5.5): scan, exact resolution, restore primitives.
// ---------------------------------------------------------------------------

fn scan_all(w: &Workspace) -> ScanDir {
    w.scan(&ScanOptions {
        max_entries: 1_000_000,
        skip: &|_, _| false,
    })
    .unwrap()
}

fn names(d: &ScanDir) -> Vec<String> {
    d.entries
        .iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect()
}

fn find<'a>(d: &'a ScanDir, name: &str) -> &'a ScanNode {
    &d.entries
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("{name} not in scan"))
        .node
}

#[test]
fn scan_is_sorted_recursive_and_skippable() {
    let f = fixture();
    fs::create_dir_all(f.ws.join("b/inner")).unwrap();
    fs::write(f.ws.join("b/inner/x"), b"x").unwrap();
    fs::write(f.ws.join("a"), b"aa").unwrap();
    fs::create_dir(f.ws.join("skipme")).unwrap();
    fs::write(f.ws.join("skipme/y"), b"y").unwrap();
    let w = Workspace::open(&f.ws).unwrap();
    let d = w
        .scan(&ScanOptions {
            max_entries: 100,
            skip: &|p, _| p == Path::new("skipme"),
        })
        .unwrap();
    assert_eq!(names(&d), ["a", "b"]);
    match find(&d, "a") {
        ScanNode::File(m) => assert_eq!(m.len, 2),
        n => panic!("{n:?}"),
    }
    let ScanNode::Dir(b) = find(&d, "b") else {
        panic!()
    };
    let ScanNode::Dir(inner) = find(b, "inner") else {
        panic!()
    };
    assert_eq!(names(inner), ["x"]);
    // Entry limit fails closed.
    let e = w.scan(&ScanOptions {
        max_entries: 2,
        skip: &|_, _| false,
    });
    assert_eq!(e.unwrap_err(), FsError::TooManyEntries(2));
}

#[test]
fn open_scanned_detects_changes() {
    let f = fixture();
    fs::write(f.ws.join("f"), b"one").unwrap();
    let w = Workspace::open(&f.ws).unwrap();
    let d = scan_all(&w);
    let m = *find(&d, "f").meta();
    let mut s = w.open_scanned(Path::new("f"), &m).unwrap();
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut s, &mut buf).unwrap();
    assert_eq!(buf, b"one");
    s.finish().unwrap();

    // Modified after the scan: refused at open.
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(f.ws.join("f"), b"two!").unwrap();
    assert_eq!(
        w.open_scanned(Path::new("f"), &m).unwrap_err(),
        FsError::Changed
    );
    // Modified while being read: refused at finish.
    let m2 = *find(&scan_all(&w), "f").meta();
    let s = w.open_scanned(Path::new("f"), &m2).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(f.ws.join("f"), b"three").unwrap();
    assert_eq!(s.finish().unwrap_err(), FsError::Changed);
}

#[test]
fn create_and_remove_dir_exactly() {
    let f = fixture();
    let w = Workspace::open(&f.ws).unwrap();
    let r = w.resolve_exact(Path::new("d")).unwrap();
    assert!(!r.exists());
    let a = approve(&f, &w, &r, Operation::Create);
    w.create_dir(&r, 0o750, a).unwrap();
    assert!(f.ws.join("d").is_dir());
    let r = w.resolve_exact(Path::new("d")).unwrap();
    assert_eq!(r.meta.unwrap().kind, Kind::Dir);
    #[cfg(unix)]
    assert_eq!(r.meta.unwrap().mode, 0o750);
    // Creating again is refused (the precondition is "absent").
    let a = approve(&f, &w, &r, Operation::Create);
    assert_eq!(w.create_dir(&r, 0o755, a).unwrap_err(), FsError::Changed);
    // Non-empty directories are not removed.
    fs::write(f.ws.join("d/x"), b"x").unwrap();
    let a = approve(&f, &w, &r, Operation::Delete);
    assert!(w.remove_dir(&r, a).is_err());
    fs::remove_file(f.ws.join("d/x")).unwrap();
    // Wrong operation in the approval.
    let a = approve(&f, &w, &r, Operation::Write);
    assert_eq!(w.remove_dir(&r, a).unwrap_err(), FsError::ApprovalMismatch);
    let a = approve(&f, &w, &r, Operation::Delete);
    w.remove_dir(&r, a).unwrap();
    assert!(!f.ws.join("d").exists());
}

#[cfg(unix)]
mod snapshot_unix {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    /// §5.5 "a symlink loop doesn't hang": self-loops, directory cycles and
    /// dangling links are recorded as symlinks and never followed.
    #[test]
    fn scan_never_follows_symlinks_or_loops() {
        let f = fixture();
        fs::create_dir_all(f.ws.join("a")).unwrap();
        symlink("self", f.ws.join("self")).unwrap();
        symlink("..", f.ws.join("a/up")).unwrap();
        symlink(f.ws.join("a"), f.ws.join("a/cycle")).unwrap();
        symlink("nowhere", f.ws.join("dangling")).unwrap();
        symlink(&f.outside, f.ws.join("out")).unwrap();
        let w = Workspace::open(&f.ws).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let root = f.ws.clone();
        std::thread::spawn(move || {
            let w = Workspace::open(&root).unwrap();
            let _ = tx.send(scan_all(&w));
        });
        let d = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("scan hung on a symlink loop");
        for n in ["self", "dangling", "out"] {
            assert!(matches!(find(&d, n), ScanNode::Symlink(_)), "{n}");
        }
        let ScanNode::Dir(a) = find(&d, "a") else {
            panic!()
        };
        assert!(matches!(find(a, "up"), ScanNode::Symlink(_)));
        assert!(matches!(find(a, "cycle"), ScanNode::Symlink(_)));
        let m = *find(&d, "out").meta();
        assert_eq!(
            w.read_link_scanned(Path::new("out"), &m).unwrap(),
            f.outside.as_os_str()
        );
    }

    #[test]
    fn open_scanned_refuses_hardlinks_and_symlinks() {
        let f = fixture();
        fs::hard_link(f.outside.join("secret"), f.ws.join("hl")).unwrap();
        symlink(f.outside.join("secret"), f.ws.join("sl")).unwrap();
        let w = Workspace::open(&f.ws).unwrap();
        let d = scan_all(&w);
        let hl = *find(&d, "hl").meta();
        assert_eq!(hl.nlink, 2);
        assert_eq!(
            w.open_scanned(Path::new("hl"), &hl).unwrap_err(),
            FsError::HardlinkRefused(2)
        );
        let sl = *find(&d, "sl").meta();
        assert_eq!(
            w.open_scanned(Path::new("sl"), &sl).unwrap_err(),
            FsError::SymlinkInStrictPath
        );
    }

    /// Restore acts on names: removing a symlink removes the link, never
    /// what it points to, and a symlinked parent is refused.
    #[test]
    fn exact_resolution_never_follows() {
        let f = fixture();
        fs::write(f.ws.join("target"), b"keep").unwrap();
        symlink("target", f.ws.join("link")).unwrap();
        symlink(&f.outside, f.ws.join("outdir")).unwrap();
        let w = Workspace::open(&f.ws).unwrap();
        let r = w.resolve_exact(Path::new("link")).unwrap();
        assert_eq!(r.meta.unwrap().kind, Kind::Symlink);
        assert_eq!(r.canonical, f.ws.join("link"));
        let a = approve(&f, &w, &r, Operation::Delete);
        w.remove(&r, a).unwrap();
        assert!(fs::symlink_metadata(f.ws.join("link")).is_err());
        assert_eq!(fs::read(f.ws.join("target")).unwrap(), b"keep");
        assert_eq!(
            w.resolve_exact(Path::new("outdir/secret")).unwrap_err(),
            FsError::SymlinkInStrictPath
        );
        assert_eq!(
            w.resolve_exact(Path::new("../outside/secret")).unwrap_err(),
            FsError::TraversalEscapesRoot
        );
        assert_outside_untouched(&f);
    }

    #[test]
    fn symlinks_and_modes_are_restorable() {
        let f = fixture();
        let w = Workspace::open(&f.ws).unwrap();
        let r = w.resolve_exact(Path::new("ln")).unwrap();
        let a = approve(&f, &w, &r, Operation::Create);
        w.create_symlink(&r, std::ffi::OsStr::new("../outside/secret"), a)
            .unwrap();
        assert_eq!(
            fs::read_link(f.ws.join("ln")).unwrap(),
            Path::new("../outside/secret")
        );
        // Not over an existing entry.
        let a = approve(&f, &w, &r, Operation::Create);
        assert_eq!(
            w.create_symlink(&r, std::ffi::OsStr::new("x"), a)
                .unwrap_err(),
            FsError::Changed
        );

        // Exact mode on create (not subject to the umask) and on replace.
        let r = w.resolve_exact(Path::new("exe")).unwrap();
        let a = approve(&f, &w, &r, Operation::Create);
        w.write_with_mode(&r, b"#!/bin/sh\n", Expect::Absent, Some(0o751), a)
            .unwrap();
        let mode = |p: &str| fs::metadata(f.ws.join(p)).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode("exe"), 0o751);
        let r = w.resolve_exact(Path::new("exe")).unwrap();
        let a = approve(&f, &w, &r, Operation::Write);
        w.set_mode(&r, 0o600, a).unwrap();
        assert_eq!(mode("exe"), 0o600);
        // set_mode never follows a symlink.
        let r = w.resolve_exact(Path::new("ln")).unwrap();
        let a = approve(&f, &w, &r, Operation::Write);
        assert!(w.set_mode(&r, 0o777, a).is_err());
        assert_outside_untouched(&f);
    }
}
