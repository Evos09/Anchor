//! Taking a snapshot of a workspace (spec §5.5(a)–(c)).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anchor_fs::{FsError, Kind, Meta, ScanDir, ScanNode, ScanOptions, Workspace};
use sha2::{Digest, Sha256};

use crate::chunker::{AVG_CHUNK, StreamChunker};
use crate::index::{Fingerprint, Index, IndexEntry, RACY_NS};
use crate::object::{
    FileObj, Id, MAX_LINK_TARGET, Node, ObjType, SnapshotObj, TreeEntry, TreeObj, Untracked,
    valid_name,
};
use crate::store::{Batch, Store, unix_ms};
use crate::{Error, Result};

/// Files at most this large are stored as one chunk without running the
/// chunker.
pub const SINGLE_CHUNK_MAX: u64 = AVG_CHUNK as u64;

#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    /// Refuse (fail closed) above this many workspace entries.
    pub max_entries: usize,
    /// Files larger than this are recorded as untracked.
    pub max_file_bytes: u64,
    /// Workspace-relative paths (and everything below them) to leave out.
    pub exclude: Vec<PathBuf>,
    /// Hashing threads (0 = available parallelism, at most 8).
    pub threads: usize,
    /// Files modified within this window before the scan are re-hashed
    /// next time (see [`crate::index`]).
    pub racy_window_ns: i128,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        SnapshotOptions {
            max_entries: 1_000_000,
            max_file_bytes: 256 << 20,
            exclude: Vec::new(),
            threads: 0,
            racy_window_ns: RACY_NS,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotReport {
    pub id: Option<Id>,
    pub root: Option<Id>,
    pub files: u64,
    pub bytes: u64,
    pub dirs: u64,
    pub symlinks: u64,
    /// Files whose content was read and hashed (the rest came from the
    /// index unchanged).
    pub hashed_files: u64,
    pub hashed_bytes: u64,
    pub new_objects: u64,
    pub new_bytes: u64,
    /// Entries recorded without content, with the reason.
    pub untracked: Vec<(String, Untracked)>,
    /// Entries whose names cannot be recorded on this platform (left out,
    /// and therefore never touched by a restore).
    pub skipped_names: Vec<String>,
    pub scan_ms: u64,
    pub hash_ms: u64,
    pub publish_ms: u64,
    pub total_ms: u64,
}

impl SnapshotReport {
    pub fn id(&self) -> Id {
        self.id.unwrap_or(Id([0; 32]))
    }
}

/// The stored bytes for a name (Unix: raw bytes; Windows: UTF-8, or `None`
/// for names that are not valid Unicode).
pub(crate) fn name_bytes(n: &OsStr) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(n.as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        n.to_str().map(|s| s.as_bytes().to_vec())
    }
}

/// The platform name for stored name bytes.
pub(crate) fn name_os(b: &[u8]) -> Option<std::ffi::OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(OsStr::from_bytes(b).to_os_string())
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(b).ok().map(std::ffi::OsString::from)
    }
}

fn join_key(parent: &[u8], name: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(parent.len() + 1 + name.len());
    if !parent.is_empty() {
        k.extend_from_slice(parent);
        k.push(b'/');
    }
    k.extend_from_slice(name);
    k
}

fn mode_of(m: &Meta) -> u32 {
    m.mode & 0o777
}

fn now_ns() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

enum Outcome {
    File { id: Id, size: u64, fp: Fingerprint },
    Symlink(Vec<u8>),
    Untracked(Untracked),
    Gone,
}

struct Job {
    key: Vec<u8>,
    rel: PathBuf,
    meta: Meta,
}

/// Read, chunk and store one file's content.
fn store_content(ws: &Workspace, batch: &Batch<'_>, rel: &Path, meta: &Meta) -> Result<(Id, u64)> {
    let mut f = ws.open_scanned(rel, meta)?;
    let mut whole = Sha256::new();
    let mut chunks = Vec::new();
    let mut size = 0u64;
    if meta.len <= SINGLE_CHUNK_MAX {
        let mut buf = Vec::with_capacity(meta.len as usize);
        (&mut f).take(SINGLE_CHUNK_MAX + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > SINGLE_CHUNK_MAX {
            return Err(FsError::Changed.into());
        }
        if !buf.is_empty() {
            whole.update(&buf);
            let id = batch.put(ObjType::Chunk, &buf)?;
            chunks.push((id, buf.len() as u32));
            size = buf.len() as u64;
        }
        f.finish()?;
    } else {
        let mut sc = StreamChunker::new(f);
        while let Some(c) = sc.next_chunk()? {
            whole.update(c);
            let id = batch.put(ObjType::Chunk, c)?;
            chunks.push((id, c.len() as u32));
            size += c.len() as u64;
        }
        sc.into_inner().finish()?;
    }
    let obj = FileObj {
        size,
        sha256: whole.finalize().into(),
        chunks,
    };
    Ok((batch.put(ObjType::File, &obj.encode())?, size))
}

fn hash_job(ws: &Workspace, batch: &Batch<'_>, job: &Job, max_file_bytes: u64) -> Result<Outcome> {
    let mut meta = job.meta;
    for attempt in 0..2 {
        if meta.nlink > 1 {
            return Ok(Outcome::Untracked(Untracked::Hardlinked));
        }
        if meta.len > max_file_bytes {
            return Ok(Outcome::Untracked(Untracked::TooLarge));
        }
        match store_content(ws, batch, &job.rel, &meta) {
            Ok((id, size)) => {
                return Ok(Outcome::File {
                    id,
                    size,
                    fp: Fingerprint::of(&meta),
                });
            }
            Err(Error::Fs(FsError::HardlinkRefused(_))) => {
                return Ok(Outcome::Untracked(Untracked::Hardlinked));
            }
            Err(Error::Fs(
                FsError::Changed
                | FsError::NotFound
                | FsError::NotAFile
                | FsError::SymlinkInStrictPath,
            )) if attempt == 0 => {
                // Changed between scan and read: look again, once.
                match ws.resolve_exact(&job.rel) {
                    Err(FsError::NotFound) | Ok(anchor_fs::Resolved { meta: None, .. }) => {
                        return Ok(Outcome::Gone);
                    }
                    Ok(anchor_fs::Resolved { meta: Some(m), .. }) => match m.kind {
                        Kind::File => meta = m,
                        Kind::Symlink => {
                            let t = ws.read_link_scanned(&job.rel, &m)?;
                            return Ok(link_outcome(&t));
                        }
                        Kind::Other => return Ok(Outcome::Untracked(Untracked::Special)),
                        Kind::Dir => return Err(FsError::Changed.into()),
                    },
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(FsError::Changed.into())
}

fn link_outcome(target: &OsStr) -> Outcome {
    match name_bytes(target) {
        Some(t) if !t.is_empty() && t.len() <= MAX_LINK_TARGET && !t.contains(&0) => {
            Outcome::Symlink(t)
        }
        _ => Outcome::Untracked(Untracked::Unrecordable),
    }
}

/// Snapshot the workspace and publish its objects. The snapshot exists
/// once a ref names it ([`take_snapshot`] does both).
pub fn snapshot(
    store: &Store,
    ws: &Workspace,
    opts: &SnapshotOptions,
    label: &str,
) -> Result<SnapshotReport> {
    let t0 = Instant::now();
    let scan_start = now_ns();
    let excludes = opts.exclude.clone();
    let skip = move |p: &Path, _k: Kind| excludes.iter().any(|e| p.starts_with(e));
    let scanned = ws.scan(&ScanOptions {
        max_entries: opts.max_entries,
        skip: &skip,
    })?;
    let scan_ms = t0.elapsed().as_millis() as u64;

    let t1 = Instant::now();
    let index = Index::load(&store.index_path());
    let mut report = SnapshotReport {
        scan_ms,
        ..Default::default()
    };
    let mut jobs = Vec::new();
    let mut results: HashMap<Vec<u8>, Outcome> = HashMap::new();
    collect(
        &scanned,
        b"",
        Path::new(""),
        &index,
        opts,
        &mut jobs,
        &mut results,
    );

    let batch = store.batch();
    let threads = if opts.threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(8)
    } else {
        opts.threads
    };
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<(usize, Result<Outcome>)>> = Mutex::new(Vec::with_capacity(jobs.len()));
    std::thread::scope(|s| {
        for _ in 0..threads.min(jobs.len().max(1)) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    let r = hash_job(ws, &batch, job, opts.max_file_bytes);
                    out.lock().unwrap_or_else(|p| p.into_inner()).push((i, r));
                }
            });
        }
    });
    let mut new_index = Index::default();
    for (i, r) in out.into_inner().unwrap_or_else(|p| p.into_inner()) {
        let o = r?;
        let job = &jobs[i];
        if let Outcome::File { fp, size, id } = &o {
            report.hashed_files += 1;
            report.hashed_bytes += size;
            let t = fp.mtime_ns.max(fp.ctime_ns);
            new_index.entries.insert(
                job.key.clone(),
                IndexEntry {
                    fp: *fp,
                    file: *id,
                    racy: t >= scan_start - opts.racy_window_ns,
                },
            );
        }
        results.insert(job.key.clone(), o);
    }
    // Unchanged files carry their index entries forward.
    for (k, o) in &results {
        if let Outcome::File { fp, id, .. } = o {
            new_index.entries.entry(k.clone()).or_insert(IndexEntry {
                fp: *fp,
                file: *id,
                racy: false,
            });
        }
    }

    let root = build_tree(
        ws,
        &batch,
        &scanned,
        b"",
        Path::new(""),
        &mut results,
        &mut report,
    )?;
    let mut label = label.to_string();
    while label.len() > 256 {
        label.pop();
    }
    let snap = SnapshotObj {
        root,
        created_unix_ms: unix_ms(),
        files: report.files,
        bytes: report.bytes,
        label,
    };
    let id = batch.put(ObjType::Snapshot, &snap.encode())?;
    report.hash_ms = t1.elapsed().as_millis() as u64;

    let t2 = Instant::now();
    let (n, bytes) = batch.publish()?;
    report.new_objects = n as u64;
    report.new_bytes = bytes;
    report.publish_ms = t2.elapsed().as_millis() as u64;
    // The index may only name published objects.
    let _ = new_index.save(&store.index_path());
    report.id = Some(id);
    report.root = Some(root);
    report.total_ms = t0.elapsed().as_millis() as u64;
    Ok(report)
}

/// Snapshot the workspace and point `ref_name` (and `HEAD`) at it.
pub fn take_snapshot(
    store: &Store,
    ws: &Workspace,
    opts: &SnapshotOptions,
    ref_name: &str,
    label: &str,
) -> Result<SnapshotReport> {
    let t = Instant::now();
    let mut r = snapshot(store, ws, opts, label)?;
    store.set_ref(ref_name, &r.id())?;
    store.set_ref("HEAD", &r.id())?;
    r.total_ms = r.total_ms.max(t.elapsed().as_millis() as u64);
    Ok(r)
}

fn collect(
    dir: &ScanDir,
    key: &[u8],
    rel: &Path,
    index: &Index,
    opts: &SnapshotOptions,
    jobs: &mut Vec<Job>,
    results: &mut HashMap<Vec<u8>, Outcome>,
) {
    for e in &dir.entries {
        let Some(nb) = name_bytes(&e.name).filter(|n| valid_name(n)) else {
            continue;
        };
        let k = join_key(key, &nb);
        let r = rel.join(&e.name);
        match &e.node {
            ScanNode::Dir(d) => collect(d, &k, &r, index, opts, jobs, results),
            ScanNode::File(m) => {
                let fp = Fingerprint::of(m);
                if m.nlink > 1 {
                    results.insert(k, Outcome::Untracked(Untracked::Hardlinked));
                } else if m.len > opts.max_file_bytes {
                    results.insert(k, Outcome::Untracked(Untracked::TooLarge));
                } else if let Some(id) = index.lookup(&k, &fp) {
                    results.insert(
                        k,
                        Outcome::File {
                            id,
                            size: m.len,
                            fp,
                        },
                    );
                } else {
                    jobs.push(Job {
                        key: k,
                        rel: r,
                        meta: *m,
                    });
                }
            }
            ScanNode::Symlink(_) | ScanNode::Other(_) => {}
        }
    }
}

fn build_tree(
    ws: &Workspace,
    batch: &Batch<'_>,
    dir: &ScanDir,
    key: &[u8],
    rel: &Path,
    results: &mut HashMap<Vec<u8>, Outcome>,
    report: &mut SnapshotReport,
) -> Result<Id> {
    let mut entries = Vec::with_capacity(dir.entries.len());
    for e in &dir.entries {
        let r = rel.join(&e.name);
        let Some(nb) = name_bytes(&e.name).filter(|n| valid_name(n)) else {
            report.skipped_names.push(r.to_string_lossy().into_owned());
            continue;
        };
        let k = join_key(key, &nb);
        let node = match &e.node {
            ScanNode::Dir(d) => {
                report.dirs += 1;
                Node::Dir {
                    id: build_tree(ws, batch, d, &k, &r, results, report)?,
                    mode: mode_of(&d.meta),
                }
            }
            ScanNode::File(m) => match results.remove(&k) {
                Some(Outcome::File { id, size, .. }) => {
                    report.files += 1;
                    report.bytes += size;
                    Node::File {
                        id,
                        mode: mode_of(m),
                    }
                }
                Some(Outcome::Symlink(t)) => {
                    report.symlinks += 1;
                    Node::Symlink { target: t }
                }
                Some(Outcome::Untracked(u)) => Node::Untracked(u),
                Some(Outcome::Gone) | None => continue,
            },
            ScanNode::Symlink(m) => match ws.read_link_scanned(&r, m) {
                Ok(t) => match link_outcome(&t) {
                    Outcome::Symlink(t) => {
                        report.symlinks += 1;
                        Node::Symlink { target: t }
                    }
                    _ => Node::Untracked(Untracked::Unrecordable),
                },
                Err(FsError::NotFound) => continue,
                Err(e) => return Err(e.into()),
            },
            ScanNode::Other(_) => Node::Untracked(Untracked::Special),
        };
        if let Node::Untracked(u) = node {
            report
                .untracked
                .push((String::from_utf8_lossy(&k).into_owned(), u));
        }
        entries.push(TreeEntry { name: nb, node });
    }
    // Scan order is by OS name bytes; stored order is by stored bytes.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries.dedup_by(|a, b| a.name == b.name);
    batch.put(ObjType::Tree, &TreeObj { entries }.encode())
}
