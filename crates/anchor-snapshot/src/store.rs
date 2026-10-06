//! The on-disk store (ADR-0006).
//!
//! ```text
//! <dir>/
//!   lock                      exclusive while a Store is open
//!   objects/<aa>/<id hex>     content-addressed, immutable
//!   refs/HEAD, refs/turns/N   snapshot ids, replaced atomically
//!   index                     change-detection cache (rebuildable)
//!   tmp/                      staging; wiped on open
//!   restore.journal           present only while a restore is in flight
//! ```
//!
//! Publication order is what makes a kill harmless: new objects are
//! written to `tmp/`, made durable, then renamed into `objects/`; only then
//! is a ref written (temp + fsync + rename + directory fsync). A snapshot
//! no ref names does not exist, and an object without a referrer is
//! garbage that `gc` removes.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::object::{
    DecodeError, FileObj, Id, ObjType, SnapshotObj, TreeObj, object_id, open_object,
};
use crate::{Error, Result};

/// Largest stored object accepted on read (a tree of ~1M entries).
const MAX_OBJECT_BYTES: u64 = 256 << 20;
/// Above this many new objects, one `syncfs` replaces per-file fsyncs
/// (Linux only).
const SYNCFS_THRESHOLD: usize = 64;

#[derive(Debug, Clone, Default)]
pub(crate) struct Fault {
    /// Abort the process after this many objects are published.
    pub snapshot_after: Option<u64>,
    /// Abort the process after this many restore operations.
    pub restore_after: Option<u64>,
}

impl Fault {
    /// `ANCHOR_TEST_FAULT=snapshot:N|restore:N`, honoured only in debug
    /// builds (used by the kill-mid-snapshot / kill-mid-restore tests).
    fn from_env() -> Fault {
        let mut f = Fault::default();
        if cfg!(debug_assertions) {
            if let Ok(v) = std::env::var("ANCHOR_TEST_FAULT") {
                if let Some((k, n)) = v.split_once(':') {
                    let n = n.parse().ok();
                    match k {
                        "snapshot" => f.snapshot_after = n,
                        "restore" => f.restore_after = n,
                        _ => {}
                    }
                }
            }
        }
        f
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub version: u32,
    /// Snapshot of the workspace taken immediately before the restore.
    pub pre: String,
    /// Snapshot being restored.
    pub target: String,
    pub started_unix_ms: u64,
}

#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    _lock: File,
    fanouts: Mutex<HashSet<u8>>,
    pub(crate) fault: Fault,
    pub(crate) restore_ops: AtomicU64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn unix_ms() -> u64 {
    now_ms()
}

fn fsync_dir(p: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(p)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        // Directory handles cannot be fsynced through std on Windows; NTFS
        // journals the rename metadata.
        let _ = p;
        Ok(())
    }
}

/// Write `bytes` to `path` atomically and durably.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent"))?;
    static N: AtomicU64 = AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path)?;
    fsync_dir(dir)
}

fn lock_file(path: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    #[cfg(unix)]
    {
        use rustix::fs::{FlockOperation, flock};
        flock(&f, FlockOperation::NonBlockingLockExclusive).map_err(|_| Error::Locked)?;
    }
    #[cfg(windows)]
    {
        // Re-open without sharing: a second opener fails while we hold it.
        use std::os::windows::fs::OpenOptionsExt;
        drop(f);
        return OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(path)
            .map_err(|_| Error::Locked);
    }
    #[allow(unreachable_code)]
    Ok(f)
}

/// A valid ref name: one or two `/`-separated segments of
/// `[A-Za-z0-9_.-]`, none starting with `.`.
pub fn valid_ref(name: &str) -> bool {
    let segs: Vec<&str> = name.split('/').collect();
    !name.is_empty()
        && segs.len() <= 2
        && segs.iter().all(|s| {
            !s.is_empty()
                && !s.starts_with('.')
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        })
}

impl Store {
    /// Open (creating if needed) and lock the store. Fails with
    /// [`Error::Locked`] if another process holds it.
    pub fn open(dir: &Path) -> Result<Store> {
        for d in ["objects", "refs/turns", "refs/restore", "tmp"] {
            fs::create_dir_all(dir.join(d))?;
        }
        let lock = lock_file(&dir.join("lock"))?;
        // Leftovers of an interrupted batch were never published.
        for e in fs::read_dir(dir.join("tmp"))? {
            let p = e?.path();
            let _ = fs::remove_file(&p).or_else(|_| fs::remove_dir_all(&p));
        }
        Ok(Store {
            dir: dir.to_path_buf(),
            _lock: lock,
            fanouts: Mutex::new(HashSet::new()),
            fault: Fault::from_env(),
            restore_ops: AtomicU64::new(0),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn index_path(&self) -> PathBuf {
        self.dir.join("index")
    }

    fn object_path(&self, id: &Id) -> PathBuf {
        let h = id.hex();
        self.dir.join("objects").join(&h[..2]).join(&h)
    }

    /// The fan-out directory for `id`, created if needed and verified to be
    /// a real directory (a symlink planted there could redirect writes).
    fn fanout(&self, id: &Id) -> Result<PathBuf> {
        let d = self.dir.join("objects").join(&id.hex()[..2]);
        let mut known = self.fanouts.lock().unwrap_or_else(|p| p.into_inner());
        if !known.contains(&id.0[0]) {
            match fs::symlink_metadata(&d) {
                Ok(m) if m.file_type().is_dir() => {}
                Ok(_) => {
                    return Err(Error::Corrupt(format!(
                        "{} is not a directory",
                        d.display()
                    )));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&d).or_else(|e| {
                        if e.kind() == std::io::ErrorKind::AlreadyExists {
                            Ok(())
                        } else {
                            Err(e)
                        }
                    })?;
                    if !fs::symlink_metadata(&d)?.file_type().is_dir() {
                        return Err(Error::Corrupt(format!(
                            "{} is not a directory",
                            d.display()
                        )));
                    }
                }
                Err(e) => return Err(e.into()),
            }
            known.insert(id.0[0]);
        }
        Ok(d)
    }

    pub fn has(&self, id: &Id) -> bool {
        fs::symlink_metadata(self.object_path(id)).is_ok_and(|m| m.file_type().is_file())
    }

    /// Read an object, verify its hash, and return its type and payload.
    pub fn read_raw(&self, id: &Id) -> Result<(ObjType, Vec<u8>)> {
        let p = self.object_path(id);
        let m = fs::symlink_metadata(&p).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::Missing(*id),
            _ => e.into(),
        })?;
        if !m.file_type().is_file() {
            return Err(Error::Corrupt(format!("object {id} is not a regular file")));
        }
        if m.len() > MAX_OBJECT_BYTES {
            return Err(Error::Corrupt(format!("object {id} is too large")));
        }
        let stored = fs::read(&p)?;
        let (ty, payload) = open_object(id, &stored)?;
        Ok((ty, payload.to_vec()))
    }

    fn read_typed(&self, id: &Id, want: ObjType) -> Result<Vec<u8>> {
        let (ty, payload) = self.read_raw(id)?;
        if ty != want {
            return Err(Error::Corrupt(format!(
                "object {id} is a {ty:?}, expected a {want:?}"
            )));
        }
        Ok(payload)
    }

    pub fn chunk(&self, id: &Id) -> Result<Vec<u8>> {
        self.read_typed(id, ObjType::Chunk)
    }

    pub fn file(&self, id: &Id) -> Result<FileObj> {
        Ok(FileObj::decode(&self.read_typed(id, ObjType::File)?)?)
    }

    pub fn tree(&self, id: &Id) -> Result<TreeObj> {
        Ok(TreeObj::decode(&self.read_typed(id, ObjType::Tree)?)?)
    }

    pub fn snapshot(&self, id: &Id) -> Result<SnapshotObj> {
        Ok(SnapshotObj::decode(
            &self.read_typed(id, ObjType::Snapshot)?,
        )?)
    }

    /// Reassemble a file's content, verifying every chunk and the whole.
    pub fn file_content(&self, f: &FileObj) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(f.size as usize);
        for (id, len) in &f.chunks {
            let c = self.chunk(id)?;
            if c.len() != *len as usize {
                return Err(Error::Corrupt(format!("chunk {id} has the wrong length")));
            }
            out.extend_from_slice(&c);
        }
        if anchor_fs::sha256(&out) != f.sha256 {
            return Err(Error::Corrupt("file content fails its hash check".into()));
        }
        Ok(out)
    }

    pub fn batch(&self) -> Batch<'_> {
        Batch {
            verify_existing_trees: true,
            store: self,
            pending: Mutex::new(Vec::new()),
            seen: Mutex::new(HashSet::new()),
            new_bytes: AtomicU64::new(0),
            seq: AtomicU64::new(0),
        }
    }

    // -- refs ---------------------------------------------------------------

    fn ref_path(&self, name: &str) -> Result<PathBuf> {
        if !valid_ref(name) {
            return Err(Error::BadRef(name.to_string()));
        }
        Ok(self.dir.join("refs").join(name))
    }

    /// Point `name` at snapshot `id` (which must exist), atomically.
    pub fn set_ref(&self, name: &str, id: &Id) -> Result<()> {
        let p = self.ref_path(name)?;
        self.snapshot(id)?;
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&p, format!("{}\n", id.hex()).as_bytes())?;
        Ok(())
    }

    pub fn get_ref(&self, name: &str) -> Result<Option<Id>> {
        let p = self.ref_path(name)?;
        match fs::read_to_string(&p) {
            Ok(s) => Id::parse(s.trim_end_matches('\n'))
                .map(Some)
                .ok_or_else(|| Error::Corrupt(format!("ref {name} is malformed"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn delete_ref(&self, name: &str) -> Result<()> {
        match fs::remove_file(self.ref_path(name)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Refs under `prefix` (`""` for top-level, e.g. `"turns"`), sorted by
    /// name. Temporary and malformed names are skipped.
    pub fn list_refs(&self, prefix: &str) -> Result<Vec<(String, Id)>> {
        let dir = if prefix.is_empty() {
            self.dir.join("refs")
        } else {
            self.ref_path(prefix)?
        };
        let mut out = Vec::new();
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for e in rd {
            let e = e?;
            if !e.file_type()?.is_file() {
                continue;
            }
            let n = e.file_name().to_string_lossy().into_owned();
            let full = if prefix.is_empty() {
                n
            } else {
                format!("{prefix}/{n}")
            };
            if !valid_ref(&full) {
                continue;
            }
            if let Some(id) = self.get_ref(&full)? {
                out.push((full, id));
            }
        }
        out.sort();
        Ok(out)
    }

    /// Every ref, in every namespace.
    pub fn all_refs(&self) -> Result<Vec<(String, Id)>> {
        let mut v = self.list_refs("")?;
        let mut namespaces = Vec::new();
        for e in fs::read_dir(self.dir.join("refs"))? {
            let e = e?;
            let n = e.file_name().to_string_lossy().into_owned();
            if e.file_type()?.is_dir() && valid_ref(&n) {
                namespaces.push(n);
            }
        }
        namespaces.sort();
        for ns in namespaces {
            v.extend(self.list_refs(&ns)?);
        }
        Ok(v)
    }

    // -- restore journal ----------------------------------------------------

    fn journal_path(&self) -> PathBuf {
        self.dir.join("restore.journal")
    }

    pub fn journal(&self) -> Result<Option<Journal>> {
        match fs::read(self.journal_path()) {
            Ok(b) => {
                let j: Journal = serde_json::from_slice(&b)
                    .map_err(|e| Error::Corrupt(format!("restore journal: {e}")))?;
                if j.version != 1 || Id::parse(&j.pre).is_none() || Id::parse(&j.target).is_none() {
                    return Err(Error::Corrupt("restore journal is malformed".into()));
                }
                Ok(Some(j))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) fn write_journal(&self, pre: &Id, target: &Id) -> Result<()> {
        let j = Journal {
            version: 1,
            pre: pre.hex(),
            target: target.hex(),
            started_unix_ms: now_ms(),
        };
        let bytes = serde_json::to_vec_pretty(&j).map_err(|e| Error::Corrupt(e.to_string()))?;
        atomic_write(&self.journal_path(), &bytes)?;
        Ok(())
    }

    pub(crate) fn clear_journal(&self) -> Result<()> {
        match fs::remove_file(self.journal_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        fsync_dir(&self.dir)?;
        Ok(())
    }

    // -- objects listing (gc / verify) --------------------------------------

    pub(crate) fn all_objects(&self) -> Result<Vec<(Id, PathBuf)>> {
        let mut out = Vec::new();
        for fan in fs::read_dir(self.dir.join("objects"))? {
            let fan = fan?;
            if !fan.file_type()?.is_dir() {
                continue;
            }
            for e in fs::read_dir(fan.path())? {
                let e = e?;
                if let Some(id) = Id::parse(&e.file_name().to_string_lossy()) {
                    out.push((id, e.path()));
                }
            }
        }
        Ok(out)
    }
}

/// New objects staged for one publication.
#[derive(Debug)]
pub struct Batch<'a> {
    /// Verify an existing tree object before reusing it (content objects
    /// are always verified).
    pub verify_existing_trees: bool,
    store: &'a Store,
    pending: Mutex<Vec<(Id, PathBuf)>>,
    seen: Mutex<HashSet<Id>>,
    new_bytes: AtomicU64,
    seq: AtomicU64,
}

impl Batch<'_> {
    /// Stage an object unless the store or this batch already has it.
    /// Safe to call from several threads.
    pub fn put(&self, ty: ObjType, payload: &[u8]) -> Result<Id> {
        let id = object_id(ty, payload);
        {
            let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
            if !seen.insert(id) {
                return Ok(id);
            }
        }
        // An existing object is trusted only if it still verifies; a
        // damaged one is replaced by this fresh copy on publish.
        let trusted = |s: &Store| {
            (ty == ObjType::Tree && !self.verify_existing_trees)
                || s.read_raw(&id).is_ok_and(|(t, _)| t == ty)
        };
        if self.store.has(&id) && trusted(self.store) {
            return Ok(id);
        }
        let tmp = self.store.dir.join("tmp").join(format!(
            "{}-{}",
            id.hex(),
            self.seq.fetch_add(1, Ordering::SeqCst)
        ));
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(&[ty as u8])?;
        f.write_all(payload)?;
        drop(f);
        self.new_bytes
            .fetch_add(payload.len() as u64 + 1, Ordering::Relaxed);
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((id, tmp));
        Ok(id)
    }

    pub fn new_objects(&self) -> usize {
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn new_bytes(&self) -> u64 {
        self.new_bytes.load(Ordering::Relaxed)
    }

    /// Make every staged object durable, then move it into `objects/`.
    /// After this returns, the objects may be referenced by a ref.
    pub fn publish(self) -> Result<(usize, u64)> {
        let pending = self.pending.into_inner().unwrap_or_else(|p| p.into_inner());
        let n = pending.len();
        let bytes = self.new_bytes.load(Ordering::Relaxed);
        if n == 0 {
            return Ok((0, 0));
        }
        sync_files(&self.store.dir.join("tmp"), &pending)?;
        let mut dirs = HashSet::new();
        for (i, (id, tmp)) in pending.iter().enumerate() {
            let fan = self.store.fanout(id)?;
            fs::rename(tmp, fan.join(id.hex()))?;
            dirs.insert(fan);
            if self
                .store
                .fault
                .snapshot_after
                .is_some_and(|k| i as u64 + 1 >= k)
            {
                // Test hook: die with objects published but no ref.
                std::process::abort();
            }
        }
        for d in &dirs {
            fsync_dir(d)?;
        }
        fsync_dir(&self.store.dir.join("objects"))?;
        Ok((n, bytes))
    }
}

fn sync_files(tmp_dir: &Path, files: &[(Id, PathBuf)]) -> Result<()> {
    #[cfg(target_os = "linux")]
    if files.len() > SYNCFS_THRESHOLD {
        let d = File::open(tmp_dir)?;
        rustix::fs::syncfs(&d).map_err(std::io::Error::from)?;
        return Ok(());
    }
    let _ = (tmp_dir, SYNCFS_THRESHOLD);
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    let per = files.len().div_ceil(workers);
    std::thread::scope(|s| {
        let handles: Vec<_> = files
            .chunks(per.max(1))
            .map(|part| {
                s.spawn(move || -> std::io::Result<()> {
                    for (_, p) in part {
                        OpenOptions::new().write(true).open(p)?.sync_all()?;
                    }
                    Ok(())
                })
            })
            .collect();
        for h in handles {
            h.join()
                .map_err(|_| Error::Corrupt("fsync worker panicked".into()))??;
        }
        Ok(())
    })
}

impl From<DecodeError> for Error {
    fn from(e: DecodeError) -> Self {
        Error::Corrupt(e.0)
    }
}
