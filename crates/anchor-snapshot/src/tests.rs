//! §5.5 test suite for the snapshot engine.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anchor_fs::{Resolved, Workspace};
use anchor_policy::{
    Approved, AuditLog, Engine, Operation, PermissionRequest, Policy, Tool, UserAction,
};

use super::*;
use crate::chunker::{MAX_CHUNK, MIN_CHUNK, StreamChunker, chunks};
use crate::object::{FileObj, Node, ObjType, SnapshotObj, TreeEntry, TreeObj, object_id};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Env {
    _dir: tempfile::TempDir,
    base: PathBuf,
    ws: PathBuf,
    store_dir: PathBuf,
    engine: Engine,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let base = anchor_fs::normalize_verbatim(&fs::canonicalize(dir.path()).unwrap());
    let ws = base.join("ws");
    let store_dir = base.join("store");
    fs::create_dir_all(&ws).unwrap();
    let audit = AuditLog::open(&base.join("audit.jsonl")).unwrap();
    let policy = Policy::parse("version = 1\n[default]\naction = \"allow\"\n", &base).unwrap();
    let engine = Engine::from_policy(policy, &base, audit);
    Env {
        _dir: dir,
        base,
        ws,
        store_dir,
        engine,
    }
}

fn opts() -> SnapshotOptions {
    SnapshotOptions {
        racy_window_ns: 0,
        ..Default::default()
    }
}

/// Approve every restore operation as a user-initiated rollback (the
/// capability floor still applies).
fn approver<'e>(
    engine: &'e Engine,
    root: &'e Path,
) -> impl FnMut(&Resolved, Operation) -> std::result::Result<Approved, String> + 'e {
    move |r: &Resolved, op: Operation| {
        let req = PermissionRequest {
            tool: Tool::UserAction,
            operation: op,
            canonical_target: Some(r.canonical.clone()),
            workspace_root: root.to_path_buf(),
            target_exists: r.exists(),
            is_append_only: false,
            blast_radius_count: 1,
            network: false,
        };
        engine
            .user_initiated(UserAction::Rollback, &req)
            .map_err(|d| d.rule)
    }
}

/// Deterministic pseudo-random bytes.
fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ent {
    File(Vec<u8>, u32),
    Dir(u32),
    Link(PathBuf),
}

/// The exact observable state of a directory tree (never following
/// symlinks).
fn state(root: &Path) -> BTreeMap<String, Ent> {
    fn mode(m: &fs::Metadata) -> u32 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            m.permissions().mode() & 0o777
        }
        #[cfg(not(unix))]
        {
            let _ = m;
            0
        }
    }
    fn go(root: &Path, rel: &Path, out: &mut BTreeMap<String, Ent>) {
        for e in fs::read_dir(root.join(rel)).unwrap() {
            let e = e.unwrap();
            let r = rel.join(e.file_name());
            let p = root.join(&r);
            let m = fs::symlink_metadata(&p).unwrap();
            let key = r.to_string_lossy().replace('\\', "/");
            if m.file_type().is_symlink() {
                out.insert(key, Ent::Link(fs::read_link(&p).unwrap()));
            } else if m.is_dir() {
                out.insert(key, Ent::Dir(mode(&m)));
                go(root, &r, out);
            } else {
                out.insert(key, Ent::File(fs::read(&p).unwrap(), mode(&m)));
            }
        }
    }
    let mut out = BTreeMap::new();
    go(root, Path::new(""), &mut out);
    out
}

fn tick() {
    // Past the filesystem timestamp granularity, so edits change ctime.
    std::thread::sleep(std::time::Duration::from_millis(30));
}

#[cfg(unix)]
fn chmod(p: &Path, m: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
}

/// A mixed workspace: nested dirs, an empty dir, binary and large files,
/// and (on Unix) an executable and symlinks.
fn populate(ws: &Path) {
    fs::create_dir_all(ws.join("src/deep/er")).unwrap();
    fs::create_dir_all(ws.join("empty")).unwrap();
    fs::create_dir_all(ws.join(".git/objects")).unwrap();
    fs::write(ws.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(ws.join("src/deep/er/x.txt"), "deep\n").unwrap();
    fs::write(ws.join("bin.dat"), noise(1, 3000)).unwrap();
    fs::write(ws.join("big.bin"), noise(2, 3 << 20)).unwrap();
    fs::write(ws.join(".env"), "SECRET=1\n").unwrap();
    fs::write(ws.join(".gitignore"), ".env\n").unwrap();
    fs::write(ws.join(".git/objects/pack"), noise(3, 10_000)).unwrap();
    fs::write(ws.join("zero"), "").unwrap();
    #[cfg(unix)]
    {
        fs::write(ws.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        chmod(&ws.join("run.sh"), 0o755);
        std::os::unix::fs::symlink("src/main.rs", ws.join("link")).unwrap();
        std::os::unix::fs::symlink("nowhere", ws.join("dangling")).unwrap();
    }
}

/// Every kind of change a command or agent turn could make.
fn mutate(ws: &Path) {
    tick();
    fs::write(ws.join("src/main.rs"), "fn main() { evil() }\n").unwrap();
    fs::remove_file(ws.join("src/deep/er/x.txt")).unwrap();
    fs::remove_dir_all(ws.join("empty")).unwrap();
    fs::write(ws.join("empty"), "now a file").unwrap();
    fs::create_dir_all(ws.join("new/dir")).unwrap();
    fs::write(ws.join("new/dir/file"), "created").unwrap();
    fs::remove_file(ws.join(".env")).unwrap();
    fs::write(ws.join(".git/objects/pack"), "corrupted").unwrap();
    let mut big = fs::read(ws.join("big.bin")).unwrap();
    big[1_000_000] ^= 0xff;
    fs::write(ws.join("big.bin"), big).unwrap();
    fs::remove_dir_all(ws.join("src/deep")).unwrap();
    fs::write(ws.join("src/deep"), "file where a dir was").unwrap();
    #[cfg(unix)]
    {
        chmod(&ws.join("run.sh"), 0o644);
        fs::remove_file(ws.join("link")).unwrap();
        std::os::unix::fs::symlink("elsewhere", ws.join("link")).unwrap();
        fs::remove_file(ws.join("dangling")).unwrap();
        fs::create_dir(ws.join("dangling")).unwrap();
    }
}

// ---------------------------------------------------------------------------
// Chunker
// ---------------------------------------------------------------------------

#[test]
fn chunker_is_deterministic_and_bounded() {
    let data = noise(7, 4 << 20);
    let a = chunks(&data);
    let b = chunks(&data);
    assert_eq!(a, b);
    assert!(a.len() > 20, "{} chunks", a.len());
    for (i, c) in a.iter().enumerate() {
        assert!(c.len() <= MAX_CHUNK);
        if i + 1 < a.len() {
            assert!(c.len() >= MIN_CHUNK);
        }
    }
    assert_eq!(a.concat(), data);
    // Average lands near the 64 KiB target.
    let avg = data.len() / a.len();
    assert!((32 << 10..=128 << 10).contains(&avg), "avg {avg}");
}

#[test]
fn chunker_resists_boundary_shift() {
    let data = noise(9, 2 << 20);
    let mut shifted = vec![0x42];
    shifted.extend_from_slice(&data);
    let old: std::collections::HashSet<&[u8]> = chunks(&data).into_iter().collect();
    let changed = chunks(&shifted)
        .into_iter()
        .filter(|c| !old.contains(c))
        .count();
    assert!(
        changed <= 2,
        "{changed} chunks changed after a 1-byte insert"
    );
}

#[test]
fn stream_chunker_matches_in_memory() {
    let data = noise(11, 3 << 20);
    let mut sc = StreamChunker::new(std::io::Cursor::new(data.clone()));
    let mut got = Vec::new();
    while let Some(c) = sc.next_chunk().unwrap() {
        got.push(c.to_vec());
    }
    let want: Vec<Vec<u8>> = chunks(&data).into_iter().map(<[u8]>::to_vec).collect();
    assert_eq!(got, want);
}

// ---------------------------------------------------------------------------
// Object encoding
// ---------------------------------------------------------------------------

fn sample_tree() -> TreeObj {
    let id = object_id(ObjType::Chunk, b"x");
    TreeObj {
        entries: vec![
            TreeEntry {
                name: b"a".to_vec(),
                node: Node::File { id, mode: 0o644 },
            },
            TreeEntry {
                name: b"b".to_vec(),
                node: Node::Dir { id, mode: 0o755 },
            },
            TreeEntry {
                name: b"c".to_vec(),
                node: Node::Symlink {
                    target: b"../x".to_vec(),
                },
            },
            TreeEntry {
                name: b"d".to_vec(),
                node: Node::Untracked(Untracked::Hardlinked),
            },
        ],
    }
}

#[test]
fn objects_roundtrip_and_reject_noncanonical_input() {
    let t = sample_tree();
    let enc = t.encode();
    assert_eq!(TreeObj::decode(&enc).unwrap(), t);
    // Truncation and trailing bytes.
    assert!(TreeObj::decode(&enc[..enc.len() - 1]).is_err());
    let mut long = enc.clone();
    long.push(0);
    assert!(TreeObj::decode(&long).is_err());
    // Unsorted / duplicate entries.
    let mut rev = t.clone();
    rev.entries.reverse();
    assert!(TreeObj::decode(&rev.encode()).is_err());
    let mut dup = t.clone();
    dup.entries[1].name = b"a".to_vec();
    assert!(TreeObj::decode(&dup.encode()).is_err());
    // setuid bits are never accepted.
    let mut suid = t.clone();
    suid.entries[0].node = Node::File {
        id: object_id(ObjType::Chunk, b"x"),
        mode: 0o4755,
    };
    assert!(TreeObj::decode(&suid.encode()).is_err());

    let f = FileObj {
        size: 3,
        sha256: [1; 32],
        chunks: vec![(object_id(ObjType::Chunk, b"abc"), 3)],
    };
    assert_eq!(FileObj::decode(&f.encode()).unwrap(), f);
    let lie = FileObj { size: 4, ..f };
    assert!(
        FileObj::decode(&lie.encode()).is_err(),
        "declared size must equal the sum of chunk lengths"
    );

    let s = SnapshotObj {
        root: object_id(ObjType::Tree, b""),
        created_unix_ms: 5,
        files: 1,
        bytes: 2,
        label: "turn 3".into(),
    };
    assert_eq!(SnapshotObj::decode(&s.encode()).unwrap(), s);
}

/// §5.5 "a crafted file cannot escape the store": names that could leave
/// the directory are refused at decode time.
#[test]
fn crafted_tree_names_are_refused() {
    let id = object_id(ObjType::Chunk, b"x");
    for bad in [
        &b".."[..],
        b".",
        b"",
        b"a/b",
        b"/etc",
        b"a\0b",
        &[b'x'; 256][..],
    ] {
        let t = TreeObj {
            entries: vec![TreeEntry {
                name: bad.to_vec(),
                node: Node::File { id, mode: 0o644 },
            }],
        };
        assert!(
            TreeObj::decode(&t.encode()).is_err(),
            "{:?} accepted",
            String::from_utf8_lossy(bad)
        );
    }
    for bad in ["../x", "ABCDEF", "", &"A".repeat(64), &"0".repeat(63)] {
        assert!(Id::parse(bad).is_none(), "{bad}");
    }
    for bad in ["../x", "/abs", "a/b/c", ".hidden", "turns/../../x", ""] {
        assert!(!valid_ref(bad), "{bad}");
    }
}

// ---------------------------------------------------------------------------
// Snapshot and restore
// ---------------------------------------------------------------------------

fn snap(store: &Store, ws: &Workspace, name: &str) -> SnapshotReport {
    take_snapshot(store, ws, &opts(), name, name).unwrap()
}

/// §5.5 "revert restores exactly".
#[test]
fn revert_restores_exactly() {
    let e = env();
    populate(&e.ws);
    let before = state(&e.ws);
    let store = Store::open(&e.store_dir).unwrap();
    let ws = Workspace::open(&e.ws).unwrap();
    let s1 = snap(&store, &ws, "turns/1");
    assert!(s1.untracked.is_empty(), "{:?}", s1.untracked);
    mutate(&e.ws);
    assert_ne!(state(&e.ws), before);
    let mut auth = approver(&e.engine, ws.root());
    let r = restore(&store, &ws, &opts(), &s1.id(), &mut auth).unwrap();
    assert!(!r.ops.is_empty());
    assert_eq!(state(&e.ws), before, "workspace differs after restore");
    // Verified by rescan: same root tree id.
    let again = snapshot(&store, &ws, &opts(), "check").unwrap();
    assert_eq!(again.root, s1.root);
    assert!(store.journal().unwrap().is_none());
    // The pre-restore state is kept, so the restore itself can be undone.
    let pre = r.pre.unwrap();
    restore(&store, &ws, &opts(), &pre, &mut auth).unwrap();
    assert_ne!(state(&e.ws), before);
    restore(&store, &ws, &opts(), &s1.id(), &mut auth).unwrap();
    assert_eq!(state(&e.ws), before);
}

#[test]
fn unchanged_files_are_neither_reread_nor_restored() {
    let e = env();
    populate(&e.ws);
    let store = Store::open(&e.store_dir).unwrap();
    let ws = Workspace::open(&e.ws).unwrap();
    let a = snap(&store, &ws, "turns/1");
    assert!(a.hashed_files > 0);
    let b = snap(&store, &ws, "turns/2");
    assert_eq!(b.hashed_files, 0, "unchanged files were re-hashed");
    assert_eq!(b.root, a.root);
    // Only the new snapshot object itself is stored.
    assert_eq!(b.new_objects, 1);

    // A 1-byte edit in a 10 MiB file stores a few chunks, not the file.
    tick();
    let mut big = noise(5, 10 << 20);
    fs::write(e.ws.join("huge"), &big).unwrap();
    let c = snap(&store, &ws, "turns/3");
    tick();
    big[5 << 20] ^= 1;
    fs::write(e.ws.join("huge"), &big).unwrap();
    let d = snap(&store, &ws, "turns/4");
    assert_eq!(d.hashed_files, 1);
    // ≤ 3 chunks + file object + root tree + snapshot.
    assert!(d.new_objects <= 6, "{} new objects", d.new_objects);
    assert!(d.new_bytes < 1 << 20, "{} new bytes", d.new_bytes);
    assert!(c.new_bytes > 9 << 20);
}

#[cfg(unix)]
#[test]
fn index_catches_backdated_and_replaced_files() {
    let e = env();
    fs::write(e.ws.join("f"), "aaaa").unwrap();
    let store = Store::open(&e.store_dir).unwrap();
    let ws = Workspace::open(&e.ws).unwrap();
    let a = snap(&store, &ws, "turns/1");
    // Same size, mtime set back to the old value: ctime still moves.
    let old = fs::metadata(e.ws.join("f")).unwrap().modified().unwrap();
    tick();
    fs::write(e.ws.join("f"), "bbbb").unwrap();
    fs::File::options()
        .write(true)
        .open(e.ws.join("f"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    let b = snap(&store, &ws, "turns/2");
    assert_eq!(b.hashed_files, 1);
    assert_ne!(a.root, b.root);
    // Replaced by a different inode with identical times and size.
    tick();
    fs::write(e.ws.join("g"), "cccc").unwrap();
    fs::rename(e.ws.join("g"), e.ws.join("f")).unwrap();
    let c = snap(&store, &ws, "turns/3");
    assert_eq!(c.hashed_files, 1);
    // A corrupt index only costs a re-hash.
    fs::write(store.dir().join("index"), b"garbage").unwrap();
    let d = snap(&store, &ws, "turns/4");
    assert_eq!(d.hashed_files, 1);
    assert_eq!(d.root, c.root);
}

#[test]
fn racy_entries_are_rehashed() {
    let e = env();
    fs::write(e.ws.join("f"), "x").unwrap();
    let store = Store::open(&e.store_dir).unwrap();
    let ws = Workspace::open(&e.ws).unwrap();
    let o = SnapshotOptions::default(); // 2 s racy window
    take_snapshot(&store, &ws, &o, "turns/1", "").unwrap();
    // Just written, so racy: hashed again even though unchanged.
    let b = take_snapshot(&store, &ws, &o, "turns/2", "").unwrap();
    assert_eq!(b.hashed_files, 1);
}

#[cfg(unix)]
#[test]
fn hardlinks_and_specials_are_untracked_and_left_alone() {
    let e = env();
    let outside = e.base.join("outside");
    fs::write(&outside, "outside content").unwrap();
    fs::hard_link(&outside, e.ws.join("hl")).unwrap();
    fs::write(e.ws.join("normal"), "n").unwrap();
    let store = Store::open(&e.store_dir).unwrap();
    let ws = Workspace::open(&e.ws).unwrap();
    let s = snap(&store, &ws, "turns/1");
    assert_eq!(s.untracked, vec![("hl".to_string(), Untracked::Hardlinked)]);
    // Remove the hardlink and change the normal file; restore brings back
    // the normal file but never recreates the hardlinked one.
    tick();
    fs::remove_file(e.ws.join("hl")).unwrap();
    fs::write(e.ws.join("normal"), "changed").unwrap();
    let mut auth = approver(&e.engine, ws.root());
    let r = restore(&store, &ws, &opts(), &s.id(), &mut auth).unwrap();
    assert_eq!(fs::read_to_string(e.ws.join("normal")).unwrap(), "n");
    assert!(!e.ws.join("hl").exists());
    assert_eq!(r.skipped, vec!["hl".to_string()]);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside content");
}

/// §5.5 "a symlink loop doesn't hang": snapshot and restore complete.
#[cfg(unix)]
#[test]
fn symlink_loops_snapshot_and_restore() {
    let e = env();
    fs::create_dir(e.ws.join("a")).unwrap();
    std::os::unix::fs::symlink("loop", e.ws.join("loop")).unwrap();
    std::os::unix::fs::symlink("..", e.ws.join("a/up")).unwrap();
    std::os::unix::fs::symlink(&e.ws, e.ws.join("a/root")).unwrap();
    let before = state(&e.ws);
    let (tx, rx) = std::sync::mpsc::channel();
    let (wsp, sd) = (e.ws.clone(), e.store_dir.clone());
    std::thread::spawn(move || {
        let store = Store::open(&sd).unwrap();
        let ws = Workspace::open(&wsp).unwrap();
        let r = take_snapshot(&store, &ws, &opts(), "turns/1", "").unwrap();
        let _ = tx.send(r);
    });
    let s = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("snapshot hung on a symlink loop");
    assert_eq!(s.symlinks, 3);
    fs::remove_file(e.ws.join("loop")).unwrap();
    fs::remove_dir_all(e.ws.join("a")).unwrap();
    let store = Store::open(&e.store_dir).unwrap();
    let ws = Workspace::open(&e.ws).unwrap();
    let mut auth = approver(&e.engine, ws.root());
    restore(&store, &ws, &opts(), &s.id(), &mut auth).unwrap();
    assert_eq!(state(&e.ws), before);
}

/// A failed operation mid-restore returns the workspace to its pre-restore
/// state (§5.5(d)).
#[test]
fn failed_restore_reverts_to_pre_restore_state() {
    let e = env();
    populate(&e.ws);
    let store = Store::open(&e.store_dir).unwrap();
    let ws = Workspace::open(&e.ws).unwrap();
    let s = snap(&store, &ws, "turns/1");
    mutate(&e.ws);
    let pre_restore = state(&e.ws);
    let mut calls = 0;
    let mut real = approver(&e.engine, ws.root());
    let mut flaky = |r: &Resolved, op: Operation| {
        calls += 1;
        // Deny the 4th operation of the forward pass only.
        if calls == 4 {
            Err("injected failure".to_string())
        } else {
            real(r, op)
        }
    };
    let err = restore(&store, &ws, &opts(), &s.id(), &mut flaky).unwrap_err();
    assert!(
        matches!(err, Error::RestoreFailed { reverted: true, .. }),
        "{err}"
    );
    assert_eq!(state(&e.ws), pre_restore);
    assert!(store.journal().unwrap().is_none());
}

/// A file changed by someone else during a restore is never clobbered
/// silently: the restore stops and reverts, and the changed content is
/// kept in a snapshot.
#[test]
fn concurrent_change_is_detected_and_preserved() {
    let e = env();
    fs::write(e.ws.join("a"), "1").unwrap();
    fs::write(e.ws.join("b"), "1").unwrap();
    let store = Store::open(&e.store_dir).unwrap();
    let ws = Workspace::open(&e.ws).unwrap();
    let s = snap(&store, &ws, "turns/1");
    tick();
    fs::write(e.ws.join("a"), "2").unwrap();
    fs::write(e.ws.join("b"), "2").unwrap();
    let mut real = approver(&e.engine, ws.root());
    let bpath = e.ws.join("b");
    let mut meddle = |r: &Resolved, op: Operation| {
        if r.canonical.ends_with("a") {
            // Someone edits b while a is being restored.
            tick();
            fs::write(&bpath, "someone else's edit").unwrap();
        }
        real(r, op)
    };
    let err = restore(&store, &ws, &opts(), &s.id(), &mut meddle).unwrap_err();
    assert!(matches!(err, Error::RestoreFailed { .. }), "{err}");
    // The edit survives in the mid-failure snapshot.
    let mids: Vec<_> = store
        .list_refs("restore")
        .unwrap()
        .into_iter()
        .filter(|(n, _)| n.ends_with("-mid"))
        .collect();
    assert_eq!(mids.len(), 1);
    let root = store.snapshot(&mids[0].1).unwrap().root;
    let Some(Node::File { id, .. }) = store.tree(&root).unwrap().get(b"b").cloned() else {
        panic!("b missing from mid snapshot")
    };
    let content = store.file_content(&store.file(&id).unwrap()).unwrap();
    assert_eq!(content, b"someone else's edit");
}

#[test]
fn store_is_exclusive() {
    let e = env();
    let _a = Store::open(&e.store_dir).unwrap();
    assert!(matches!(Store::open(&e.store_dir), Err(Error::Locked)));
}

/// §5.5(e) / Threat 9: tampered objects are detected; crafted trees cannot
/// write outside the workspace; a symlink planted in the store is refused.
#[test]
fn poisoned_store_is_refused() {
    let e = env();
    fs::write(e.ws.join("f"), "original").unwrap();
    let store = Store::open(&e.store_dir).unwrap();
    let ws = Workspace::open(&e.ws).unwrap();
    let s = snap(&store, &ws, "turns/1");
    let mut auth = approver(&e.engine, ws.root());

    // 1. A crafted snapshot whose tree names `..`: written as raw bytes
    //    with a valid hash, so only the decoder stands in the way.
    let chunk = b"pwned".to_vec();
    let mut stored = vec![ObjType::Tree as u8];
    let file = FileObj {
        size: 5,
        sha256: anchor_fs::sha256(&chunk),
        chunks: vec![(object_id(ObjType::Chunk, &chunk), 5)],
    };
    stored.extend_from_slice(
        &TreeObj {
            entries: vec![TreeEntry {
                name: b"..".to_vec(),
                node: Node::File {
                    id: object_id(ObjType::File, &file.encode()),
                    mode: 0o644,
                },
            }],
        }
        .encode(),
    );
    let tree_id = Id(anchor_fs::sha256(&stored));
    let put_raw = |id: &Id, bytes: &[u8]| {
        let d = store.dir().join("objects").join(&id.hex()[..2]);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(id.hex()), bytes).unwrap();
    };
    put_raw(&tree_id, &stored);
    let b = store.batch();
    b.put(ObjType::Chunk, &chunk).unwrap();
    b.put(ObjType::File, &file.encode()).unwrap();
    let evil = b
        .put(
            ObjType::Snapshot,
            &SnapshotObj {
                root: tree_id,
                created_unix_ms: 0,
                files: 1,
                bytes: 5,
                label: String::new(),
            }
            .encode(),
        )
        .unwrap();
    b.publish().unwrap();
    assert!(restore(&store, &ws, &opts(), &evil, &mut auth).is_err());
    assert!(!e.base.join("pwned").exists());
    assert_eq!(fs::read_to_string(e.ws.join("f")).unwrap(), "original");

    // 2. Tampered content is caught by its hash.
    let root = store.snapshot(&s.id()).unwrap().root;
    let Some(Node::File { id: fid, .. }) = store.tree(&root).unwrap().get(b"f").cloned() else {
        panic!()
    };
    let (cid, _) = store.file(&fid).unwrap().chunks[0];
    let cpath = store
        .dir()
        .join("objects")
        .join(&cid.hex()[..2])
        .join(cid.hex());
    let mut bytes = fs::read(&cpath).unwrap();
    bytes[1] ^= 1;
    fs::write(&cpath, &bytes).unwrap();
    tick();
    fs::write(e.ws.join("f"), "changed").unwrap();
    assert!(restore(&store, &ws, &opts(), &s.id(), &mut auth).is_err());
    assert_eq!(fs::read_to_string(e.ws.join("f")).unwrap(), "changed");
    assert!(!verify(&store).unwrap().ok());

    // 3. A ref pointing at a non-snapshot object is refused.
    assert!(store.set_ref("turns/9", &fid).is_err());
    fs::write(store.dir().join("refs/turns/9"), format!("{}\n", fid.hex())).unwrap();
    let id = store.get_ref("turns/9").unwrap().unwrap();
    assert!(restore(&store, &ws, &opts(), &id, &mut auth).is_err());
    fs::write(store.dir().join("refs/turns/9"), "../../../etc/passwd\n").unwrap();
    assert!(store.get_ref("turns/9").is_err());
}

#[cfg(unix)]
#[test]
fn symlink_planted_in_store_is_refused() {
    let e = env();
    let elsewhere = e.base.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::write(e.ws.join("f"), "content").unwrap();
    let store = Store::open(&e.store_dir).unwrap();
    let ws = Workspace::open(&e.ws).unwrap();
    // Plant symlinks for every fan-out directory.
    for i in 0..=255u8 {
        let d = store.dir().join("objects").join(format!("{i:02x}"));
        std::os::unix::fs::symlink(&elsewhere, d).unwrap();
    }
    assert!(take_snapshot(&store, &ws, &opts(), "turns/1", "").is_err());
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

// ---------------------------------------------------------------------------
// Merge-queue substrate and maintenance
// ---------------------------------------------------------------------------

#[test]
fn diff_merge3_and_materialize() {
    let e = env();
    fs::create_dir(e.ws.join("d")).unwrap();
    fs::write(e.ws.join("d/shared"), "base").unwrap();
    fs::write(e.ws.join("ours_only"), "base").unwrap();
    fs::write(e.ws.join("theirs_only"), "base").unwrap();
    fs::write(e.ws.join("both"), "base").unwrap();
    let store = Store::open(&e.store_dir).unwrap();
    let ws = Workspace::open(&e.ws).unwrap();
    let base = snap(&store, &ws, "turns/1").id();
    tick();
    fs::write(e.ws.join("ours_only"), "ours").unwrap();
    fs::write(e.ws.join("both"), "ours").unwrap();
    fs::write(e.ws.join("d/new_ours"), "x").unwrap();
    let ours = snap(&store, &ws, "turns/2").id();
    let mut auth = approver(&e.engine, ws.root());
    restore(&store, &ws, &opts(), &base, &mut auth).unwrap();
    tick();
    fs::write(e.ws.join("theirs_only"), "theirs").unwrap();
    fs::write(e.ws.join("both"), "theirs").unwrap();
    fs::write(e.ws.join("d/new_theirs"), "y").unwrap();
    let theirs = snap(&store, &ws, "turns/3").id();

    let d = diff(&store, &base, &ours).unwrap();
    let paths: Vec<String> = d.iter().map(|c| c.to_string()).collect();
    assert_eq!(
        paths,
        ["modified: both", "added: d/new_ours", "modified: ours_only"]
    );

    let m = merge3(&store, &base, &ours, &theirs, "merge").unwrap();
    assert_eq!(m.conflicts, vec![PathBuf::from("both")]);
    store.set_ref("turns/4", &m.snapshot).unwrap();

    // Check the merge out into a fresh, empty workspace.
    let wt = e.base.join("worktree");
    fs::create_dir(&wt).unwrap();
    let wtws = Workspace::open(&wt).unwrap();
    let mut auth2 = approver(&e.engine, wtws.root());
    materialize(&store, &wtws, &opts(), &m.snapshot, &mut auth2).unwrap();
    let st = state(&wt);
    let file = |p: &str| match &st[p] {
        Ent::File(b, _) => String::from_utf8(b.clone()).unwrap(),
        x => panic!("{x:?}"),
    };
    assert_eq!(file("ours_only"), "ours");
    assert_eq!(file("theirs_only"), "theirs");
    assert_eq!(file("both"), "ours", "a conflict keeps ours");
    assert_eq!(file("d/new_ours"), "x");
    assert_eq!(file("d/new_theirs"), "y");
    assert!(matches!(
        materialize(&store, &wtws, &opts(), &m.snapshot, &mut auth2),
        Err(Error::NotEmpty)
    ));
}

#[test]
fn gc_keeps_reachable_and_verify_passes() {
    let e = env();
    let store = Store::open(&e.store_dir).unwrap();
    let ws = Workspace::open(&e.ws).unwrap();
    for i in 0..5 {
        tick();
        fs::write(e.ws.join("f"), format!("version {i}")).unwrap();
        snap(&store, &ws, &format!("turns/{i:010}"));
    }
    let before = store.all_objects().unwrap().len();
    let r = gc(&store, 2, 20).unwrap();
    assert_eq!(r.refs_pruned, 3);
    assert!(r.objects_removed > 0);
    assert!(store.all_objects().unwrap().len() < before);
    let v = verify(&store).unwrap();
    assert!(v.ok(), "{:?}", v.errors);
    // The kept snapshots still restore.
    let mut auth = approver(&e.engine, ws.root());
    let (_, id) = store.list_refs("turns").unwrap()[0].clone();
    restore(&store, &ws, &opts(), &id, &mut auth).unwrap();
    assert_eq!(fs::read_to_string(e.ws.join("f")).unwrap(), "version 3");
}

// ---------------------------------------------------------------------------
// Kill tests: a child process (this test binary) aborts at a fault point.
// ---------------------------------------------------------------------------

const CHILD_ENV: &str = "ANCHOR_SNAPSHOT_TEST_CHILD";

/// Entry point for the child process of the kill tests; a no-op otherwise.
#[test]
fn kill_child_entry() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    let v: serde_json::Value = serde_json::from_str(&spec).unwrap();
    let ws_path = PathBuf::from(v["ws"].as_str().unwrap());
    let store = Store::open(Path::new(v["store"].as_str().unwrap())).unwrap();
    let ws = Workspace::open(&ws_path).unwrap();
    match v["mode"].as_str().unwrap() {
        "snapshot" => {
            take_snapshot(&store, &ws, &opts(), "turns/2", "").unwrap();
        }
        "restore" => {
            let base = PathBuf::from(v["base"].as_str().unwrap());
            let audit = AuditLog::open(&base.join("child-audit.jsonl")).unwrap();
            let policy =
                Policy::parse("version = 1\n[default]\naction = \"allow\"\n", &base).unwrap();
            let engine = Engine::from_policy(policy, &base, audit);
            let mut auth = approver(&engine, ws.root());
            let target = store.get_ref("turns/1").unwrap().unwrap();
            restore(&store, &ws, &opts(), &target, &mut auth).unwrap();
        }
        m => panic!("{m}"),
    }
    panic!("the fault hook should have aborted the process");
}

fn run_child(e: &Env, mode: &str, fault: &str) {
    let spec = serde_json::json!({
        "mode": mode,
        "ws": e.ws,
        "store": e.store_dir,
        "base": e.base,
    });
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::kill_child_entry", "--test-threads=1"])
        .env(CHILD_ENV, spec.to_string())
        .env("ANCHOR_TEST_FAULT", fault)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "child was expected to die");
}

/// §5.5 "kill mid-snapshot leaves no partial snapshot".
#[test]
fn kill_mid_snapshot_leaves_no_partial_snapshot() {
    let e = env();
    for i in 0..300 {
        fs::write(e.ws.join(format!("f{i}")), format!("v1 {i}")).unwrap();
    }
    let s1 = {
        let store = Store::open(&e.store_dir).unwrap();
        let ws = Workspace::open(&e.ws).unwrap();
        snap(&store, &ws, "turns/1")
    };
    tick();
    for i in 0..300 {
        fs::write(e.ws.join(format!("f{i}")), format!("v2 {i}")).unwrap();
    }
    run_child(&e, "snapshot", "snapshot:50");

    let store = Store::open(&e.store_dir).unwrap();
    assert_eq!(
        fs::read_dir(store.dir().join("tmp")).unwrap().count(),
        0,
        "staging not wiped"
    );
    let refs: Vec<String> = store.all_refs().unwrap().into_iter().map(|r| r.0).collect();
    assert_eq!(refs, ["HEAD", "turns/1"], "no ref for the killed snapshot");
    assert_eq!(store.get_ref("HEAD").unwrap(), Some(s1.id()));
    let v = verify(&store).unwrap();
    assert!(v.ok(), "{:?}", v.errors);
    // The store keeps working: snapshot, then restore turn 1 exactly.
    let ws = Workspace::open(&e.ws).unwrap();
    snap(&store, &ws, "turns/2");
    let mut auth = approver(&e.engine, ws.root());
    restore(&store, &ws, &opts(), &s1.id(), &mut auth).unwrap();
    assert_eq!(fs::read_to_string(e.ws.join("f7")).unwrap(), "v1 7");
    assert!(gc(&store, 10, 10).is_ok());
}

/// §5.5 "kill mid-restore leaves the workspace pre-restore".
#[test]
fn kill_mid_restore_recovers_to_pre_restore_state() {
    let e = env();
    populate(&e.ws);
    {
        let store = Store::open(&e.store_dir).unwrap();
        let ws = Workspace::open(&e.ws).unwrap();
        snap(&store, &ws, "turns/1");
    }
    mutate(&e.ws);
    let pre_restore = state(&e.ws);
    run_child(&e, "restore", "restore:4");

    // The child died part-way: the journal is there and the workspace is
    // neither state.
    let store = Store::open(&e.store_dir).unwrap();
    assert!(store.journal().unwrap().is_some());
    assert_ne!(state(&e.ws), pre_restore);
    let ws = Workspace::open(&e.ws).unwrap();
    let mut auth = approver(&e.engine, ws.root());
    // A new restore is refused until recovery has run.
    let t1 = store.get_ref("turns/1").unwrap().unwrap();
    assert!(matches!(
        restore(&store, &ws, &opts(), &t1, &mut auth),
        Err(Error::RecoveryPending)
    ));
    let r = recover(&store, &ws, &opts(), &mut auth).unwrap();
    assert!(r.is_some());
    assert_eq!(state(&e.ws), pre_restore);
    assert!(store.journal().unwrap().is_none());
    assert!(recover(&store, &ws, &opts(), &mut auth).unwrap().is_none());
}
