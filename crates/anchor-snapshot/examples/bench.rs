//! Snapshot benchmark (spec §14 Phase 3: "a 100k-file monorepo snapshots
//! incrementally in < 2 s, measured and published").
//!
//! Generates a monorepo-shaped tree, then measures (median of `--runs`):
//! the initial snapshot, a no-change incremental snapshot, incremental
//! snapshots after 100 and after 1% of files changed, a restore that
//! undoes 100 changed files, and a full store verification.
//!
//! ```sh
//! cargo run --release -p anchor-snapshot --example bench -- \
//!     [--files 100000] [--runs 5] [--dir PATH] [--summary FILE] \
//!     [--max-incremental-ms 2000]
//! ```
//!
//! `--max-incremental-ms` makes the run fail if the no-change or 100-changed
//! median exceeds the bound (used as the CI gate on Linux).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anchor_fs::Workspace;
use anchor_policy::{AuditLog, Engine, Operation, PermissionRequest, Policy, Tool, UserAction};
use anchor_snapshot::{SnapshotOptions, Store, restore, take_snapshot, verify};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Standard normal (Box–Muller).
    fn normal(&mut self) -> f64 {
        let (u1, u2) = (self.unit().max(1e-12), self.unit());
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n + 8);
        while v.len() < n {
            v.extend_from_slice(&self.next().to_le_bytes());
        }
        v.truncate(n);
        v
    }
}

struct Args {
    files: usize,
    runs: usize,
    dir: Option<PathBuf>,
    summary: Option<PathBuf>,
    max_incremental_ms: Option<u128>,
}

fn args() -> Args {
    let mut a = Args {
        files: 100_000,
        runs: 5,
        dir: None,
        summary: None,
        max_incremental_ms: None,
    };
    let v: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < v.len() {
        let val = v.get(i + 1).cloned().unwrap_or_default();
        match v[i].as_str() {
            "--files" => a.files = val.parse().expect("--files"),
            "--runs" => a.runs = val.parse().expect("--runs"),
            "--dir" => a.dir = Some(PathBuf::from(val)),
            "--summary" => a.summary = Some(PathBuf::from(val)),
            "--max-incremental-ms" => a.max_incremental_ms = Some(val.parse().expect("ms")),
            other => panic!("unknown argument {other}"),
        }
        i += 2;
    }
    a
}

/// Paths of the generated files, relative to the workspace.
fn generate(ws: &Path, n: usize, rng: &mut Rng) -> (Vec<PathBuf>, u64) {
    // ~2,000 directories: 40 packages × 50 modules, plus a .git pack.
    let (packages, modules) = (40, 50);
    let mut paths = Vec::with_capacity(n);
    let mut total = 0u64;
    for i in 0..n {
        let p = PathBuf::from(format!(
            "packages/pkg{:02}/src/mod{:02}/file{:06}.rs",
            i % packages,
            (i / packages) % modules,
            i
        ));
        // Log-normal sizes, median 3 KiB; 0.1% of files are 1 MiB.
        let size = if i % 1000 == 999 {
            1 << 20
        } else {
            ((3072.0 * (0.9 * rng.normal()).exp()) as usize).clamp(16, 256 << 10)
        };
        let full = ws.join(&p);
        if i < packages * modules {
            fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        }
        fs::write(&full, rng.bytes(size)).expect("write");
        total += size as u64;
        paths.push(p);
    }
    let pack = ws.join(".git/objects/pack");
    fs::create_dir_all(&pack).expect("mkdir");
    fs::write(pack.join("pack-0001.pack"), rng.bytes(32 << 20)).expect("pack");
    total += 32 << 20;
    (paths, total)
}

fn touch_files(ws: &Path, paths: &[PathBuf], k: usize, rng: &mut Rng) {
    for _ in 0..k {
        let p = &paths[(rng.next() % paths.len() as u64) as usize];
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(ws.join(p))
            .expect("open");
        f.write_all(b"// edit\n").expect("append");
    }
}

fn median(mut v: Vec<u128>) -> u128 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn hardware() -> Vec<(String, String)> {
    let mut h = vec![
        ("os".into(), std::env::consts::OS.to_string()),
        ("arch".into(), std::env::consts::ARCH.to_string()),
        (
            "logical cpus".into(),
            std::thread::available_parallelism()
                .map(|n| n.get().to_string())
                .unwrap_or_default(),
        ),
    ];
    if let Ok(c) = fs::read_to_string("/proc/cpuinfo") {
        if let Some(l) = c.lines().find(|l| l.starts_with("model name")) {
            h.push(("cpu".into(), l.split(':').nth(1).unwrap_or("").trim().into()));
        }
    }
    if let Ok(out) = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
    {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            h.push(("cpu".into(), s));
        }
    }
    if let Ok(p) = std::env::var("PROCESSOR_IDENTIFIER") {
        h.push(("cpu".into(), p));
    }
    if let Ok(k) = fs::read_to_string("/proc/sys/kernel/osrelease") {
        h.push(("kernel".into(), k.trim().into()));
    }
    h
}

/// Filesystem type of `p` (Linux: longest matching mount in /proc/mounts).
fn fs_type(p: &Path) -> String {
    let Ok(m) = fs::read_to_string("/proc/mounts") else {
        return "unknown".into();
    };
    let mut best = ("", "unknown");
    for l in m.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() > 2 && p.starts_with(f[1]) && f[1].len() >= best.0.len() {
            best = (f[1], f[2]);
        }
    }
    best.1.to_string()
}

fn main() {
    let a = args();
    let tmp;
    let base = match &a.dir {
        Some(d) => {
            fs::create_dir_all(d).expect("dir");
            d.clone()
        }
        None => {
            tmp = tempfile::tempdir().expect("tempdir");
            tmp.path().to_path_buf()
        }
    };
    let base = anchor_fs::normalize_verbatim(&fs::canonicalize(&base).expect("canonicalize"));
    let ws_dir = base.join("ws");
    let store_dir = base.join("store");
    let _ = fs::remove_dir_all(&ws_dir);
    let _ = fs::remove_dir_all(&store_dir);
    fs::create_dir_all(&ws_dir).expect("ws");
    let mut rng = Rng(0x5eed_1234_abcd_0001);

    eprintln!("generating {} files in {} ...", a.files, ws_dir.display());
    let t = Instant::now();
    let (paths, total) = generate(&ws_dir, a.files, &mut rng);
    eprintln!(
        "generated {:.1} MiB in {:.1} s",
        total as f64 / (1 << 20) as f64,
        t.elapsed().as_secs_f64()
    );

    let store = Store::open(&store_dir).expect("store");
    let ws = Workspace::open(&ws_dir).expect("workspace");
    let opts = SnapshotOptions {
        max_entries: 2_000_000,
        ..Default::default()
    };
    let audit = AuditLog::open(&base.join("audit.jsonl")).expect("audit");
    let policy = Policy::parse("version = 1\n[default]\naction = \"allow\"\n", &base).expect("p");
    let engine = Engine::from_policy(policy, &base, audit);
    let root = ws.root().to_path_buf();
    let mut auth = |r: &anchor_fs::Resolved, op: Operation| {
        engine
            .user_initiated(
                UserAction::Rollback,
                &PermissionRequest {
                    tool: Tool::UserAction,
                    operation: op,
                    canonical_target: Some(r.canonical.clone()),
                    workspace_root: root.clone(),
                    target_exists: r.exists(),
                    is_append_only: false,
                    blast_radius_count: 1,
                    network: false,
                },
            )
            .map_err(|d| d.rule)
    };

    let mut seq = 0u64;
    let mut snap = |label: &str| {
        seq += 1;
        let t = Instant::now();
        let r = take_snapshot(&store, &ws, &opts, &format!("turns/{seq:010}"), label)
            .expect("snapshot");
        (t.elapsed().as_millis(), r)
    };

    let (initial_ms, init) = snap("initial");
    eprintln!(
        "initial: {initial_ms} ms ({} files, {} new objects)",
        init.files, init.new_objects
    );
    // Let the racy window pass so the steady state is measured.
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let _ = snap("warm-up");

    let mut rows: Vec<(String, u128, String)> = Vec::new();
    rows.push((
        format!("initial snapshot ({} files)", init.files),
        initial_ms,
        format!(
            "{:.0} MiB stored, {} ms scan",
            init.new_bytes as f64 / (1 << 20) as f64,
            init.scan_ms
        ),
    ));

    let mut measure = |name: &str, change: usize, rng: &mut Rng| {
        let mut times = Vec::new();
        let mut last = None;
        for _ in 0..a.runs {
            if change > 0 {
                touch_files(&ws_dir, &paths, change, rng);
            }
            let (ms, r) = snap(name);
            times.push(ms);
            last = Some(r);
            // Steady state: edits are older than the racy window.
            std::thread::sleep(std::time::Duration::from_millis(2100));
        }
        let r = last.expect("ran");
        let m = median(times.clone());
        eprintln!("{name}: median {m} ms (runs {times:?})");
        (
            m,
            format!(
                "scan {} ms, re-hashed {} files, {} new objects",
                r.scan_ms, r.hashed_files, r.new_objects
            ),
        )
    };
    let (nochange, d0) = measure("no-change incremental", 0, &mut rng);
    rows.push(("incremental, no change".into(), nochange, d0));
    let (hundred, d1) = measure("100 files changed", 100, &mut rng);
    rows.push(("incremental, 100 files changed".into(), hundred, d1));
    let pct = (a.files / 100).max(1);
    let (one_pct, d2) = measure("1% changed", pct, &mut rng);
    rows.push((format!("incremental, {pct} files (1%) changed"), one_pct, d2));

    // Restore: undo 100 changed files.
    let mut rtimes = Vec::new();
    let mut rops = 0;
    for _ in 0..a.runs {
        let base_snap = snap("restore-base").1.id();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        touch_files(&ws_dir, &paths, 100, &mut rng);
        let t = Instant::now();
        let r = restore(&store, &ws, &opts, &base_snap, &mut auth).expect("restore");
        rtimes.push(t.elapsed().as_millis());
        rops = r.ops.len();
        std::thread::sleep(std::time::Duration::from_millis(2100));
    }
    let restore_ms = median(rtimes);
    rows.push((
        "restore undoing 100 changed files".into(),
        restore_ms,
        format!("{rops} file operations, incl. pre-restore snapshot and verifying rescan"),
    ));

    let t = Instant::now();
    let v = verify(&store).expect("verify");
    let verify_ms = t.elapsed().as_millis();
    assert!(v.ok(), "{:?}", v.errors);
    rows.push((
        "verify whole store".into(),
        verify_ms,
        format!("{} objects re-hashed", v.objects),
    ));

    let mut md = String::new();
    md.push_str("### Anchor snapshot benchmark\n\n");
    for (k, v) in hardware() {
        md.push_str(&format!("- {k}: {v}\n"));
    }
    md.push_str(&format!(
        "- filesystem: {}\n- workspace: {} files, {:.0} MiB, ~2,000 directories\n- runs: median of {}\n\n",
        fs_type(&base),
        a.files,
        total as f64 / (1 << 20) as f64,
        a.runs
    ));
    md.push_str("| measurement | ms | detail |\n|---|---:|---|\n");
    for (k, ms, d) in &rows {
        md.push_str(&format!("| {k} | {ms} | {d} |\n"));
    }
    println!("{md}");
    if let Some(p) = &a.summary {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .expect("summary");
        let _ = f.write_all(md.as_bytes());
    }
    if let Some(max) = a.max_incremental_ms {
        if nochange > max || hundred > max {
            eprintln!(
                "FAIL: incremental snapshot exceeded {max} ms (no change {nochange} ms, 100 changed {hundred} ms)"
            );
            std::process::exit(1);
        }
        eprintln!("PASS: incremental snapshots under {max} ms");
    }
}
