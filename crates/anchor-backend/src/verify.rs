//! The verification gate (spec §8, ADR-0024): the merge queue's
//! [`MergeGate`] that only lets a change through when its build passes,
//! its tests pass, nothing regressed, and the reviewer supports every claim
//! the executor made about it.
//!
//! The Tester is deterministic: the backend runs the build, test and
//! coverage commands the *user* configured in `team.toml` (`[verify]`),
//! never commands an agent chose. They run on the merge candidate in a
//! scratch directory, through the same sandbox as agent commands (no
//! network, writes confined to the scratch directory, resource limits).
//!
//! For each candidate:
//!
//! 1. the changed files and their line diffs (pre-merge main → candidate);
//! 2. hard rules: protected files (default `tests/**`) may not be modified
//!    or removed; no new top-level module may shadow a standard-library
//!    module;
//! 3. build, tests and coverage on the candidate; every test that passed
//!    before the merge must still pass (so deleting or skipping a test, or
//!    emptying the suite, is caught);
//! 4. claims are resolved to evidence references, claims without evidence
//!    are rejected automatically (§8.2(b)), and the reviewer (the rule
//!    baseline or a model of another family) judges the rest;
//! 5. every command output, the review input and the decision are stored
//!    as Hub evidence.
//!
//! On Windows agent commands have no sandbox, so the gate rejects every
//! merge (`unsupported_platform`): it fails closed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anchor_checkpoint::CheckpointStore;
use anchor_hub::{ArtifactKind, Caller, Hub};
use anchor_protocol::roles::{RolesView, Route, normalize_family, shared_family};
use anchor_protocol::verify::{
    ChangedFile, ClaimVerdict, CommandRecord, FileCoverage, FileStatus, MAX_REVIEW_BYTES,
    ReviewInput, TestOutcome,
};
use anchor_protocol::{Notice, NoticeKind};
use anchor_snapshot::textdiff::{self, TextDiff};
use anchor_snapshot::{ChangeKind, Id, Store};
use anchor_verify::{Decision, Finding, Reviewer, RuleReviewer, Stage, decide};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::mediator::{Mediator, SharedStore, lock_store};
use crate::queue::{GateContext, MergeGate};
use crate::session::{self, SessionConfig};

/// Placeholder in the coverage command for the coverage output directory.
pub const COVERDIR: &str = "{coverdir}";
/// The tail of each command's output kept inline in the review input.
const OUTPUT_TAIL_BYTES: usize = 8 << 10;

/// `[verify]` in `team.toml`, as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyFile {
    #[serde(default)]
    pub build: Option<Vec<String>>,
    pub test: Vec<String>,
    #[serde(default)]
    pub coverage: Option<Vec<String>>,
    #[serde(default = "default_protected")]
    pub protected: Vec<String>,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: usize,
    #[serde(default)]
    pub reviewer: ReviewerKind,
    /// Roles file of the model reviewer (relative to `team.toml`).
    #[serde(default)]
    pub reviewer_roles: Option<PathBuf>,
    /// Scripted reviewer model (tests, demos).
    #[serde(default)]
    pub reviewer_mock_script: Option<PathBuf>,
    /// Wall-clock and CPU limit of each command, in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_protected() -> Vec<String> {
    vec!["tests/**".into()]
}
fn default_max_attempts() -> usize {
    3
}
fn default_timeout() -> u64 {
    300
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerKind {
    /// The deterministic baseline (no model).
    #[default]
    Rule,
    /// A model reviewer of a different family than every executor.
    Model,
}

/// The validated verification configuration.
#[derive(Debug, Clone)]
pub struct VerifySpec {
    pub build: Option<Vec<String>>,
    pub test: Vec<String>,
    pub coverage: Option<Vec<String>>,
    pub protected: Vec<String>,
    pub max_attempts: usize,
    pub reviewer: ReviewerKind,
    pub reviewer_roles: Option<String>,
    pub reviewer_mock_script: Option<String>,
    pub timeout: Duration,
}

fn check_argv(name: &str, argv: &[String]) -> Result<(), String> {
    if argv.is_empty() || argv[0].is_empty() {
        return Err(format!("[verify] {name}: give a program and its arguments"));
    }
    if argv.iter().any(|a| a.contains('\0')) {
        return Err(format!("[verify] {name}: NUL in an argument"));
    }
    Ok(())
}

impl VerifySpec {
    /// Validate `[verify]`; paths in it are relative to `dir`.
    pub fn load(file: VerifyFile, dir: &Path) -> Result<VerifySpec, String> {
        let read = |p: &Path| {
            let p = dir.join(p);
            std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))
        };
        let spec = VerifySpec {
            reviewer_roles: file.reviewer_roles.as_deref().map(read).transpose()?,
            reviewer_mock_script: file.reviewer_mock_script.as_deref().map(read).transpose()?,
            build: file.build,
            test: file.test,
            coverage: file.coverage,
            protected: file.protected,
            max_attempts: file.max_attempts,
            reviewer: file.reviewer,
            timeout: Duration::from_secs(file.timeout_secs),
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(b) = &self.build {
            check_argv("build", b)?;
        }
        check_argv("test", &self.test)?;
        if let Some(c) = &self.coverage {
            check_argv("coverage", c)?;
            if !c.iter().any(|a| a.contains(COVERDIR)) {
                return Err(format!(
                    "[verify] coverage: pass the output directory as {COVERDIR}"
                ));
            }
        }
        if !(1..=10).contains(&self.max_attempts) {
            return Err("[verify] max_attempts: 1 to 10".into());
        }
        if !(1..=3600).contains(&self.timeout.as_secs()) {
            return Err("[verify] timeout_secs: 1 to 3600".into());
        }
        protected_set(&self.protected)?;
        match (self.reviewer, &self.reviewer_roles) {
            (ReviewerKind::Model, None) => {
                Err("[verify] reviewer = \"model\" needs reviewer_roles".into())
            }
            (ReviewerKind::Model, Some(r)) => {
                let view: RolesView =
                    toml::from_str(r).map_err(|e| format!("[verify] reviewer_roles: {e}"))?;
                view.identity(Route::Review)
                    .map_err(|e| format!("[verify] reviewer_roles: {e}"))?;
                Ok(())
            }
            (ReviewerKind::Rule, Some(_)) => {
                Err("[verify] reviewer_roles is only for reviewer = \"model\"".into())
            }
            (ReviewerKind::Rule, None) => Ok(()),
        }
    }

    /// The families the model reviewer can run on (empty for the rule
    /// reviewer).
    pub fn reviewer_families(&self) -> Result<Vec<String>, String> {
        match (&self.reviewer, &self.reviewer_roles) {
            (ReviewerKind::Model, Some(r)) => {
                let view: RolesView = toml::from_str(r).map_err(|e| e.to_string())?;
                Ok(view.families(Route::Review))
            }
            _ => Ok(Vec::new()),
        }
    }

    /// §8.2(a): refuse a reviewer that shares a family (primary or
    /// fallback) with any executor.
    pub fn check_families(&self, executor_families: &[String]) -> Result<(), String> {
        let reviewer = self.reviewer_families()?;
        if let Some(f) = shared_family(&reviewer, executor_families) {
            return Err(format!(
                "the reviewer and an executor can both run on model family {f:?}: \
                 the reviewer must be of a different family (spec §8.2(a))"
            ));
        }
        Ok(())
    }
}

fn protected_set(globs: &[String]) -> Result<globset::GlobSet, String> {
    let mut b = globset::GlobSetBuilder::new();
    for g in globs {
        b.add(
            globset::GlobBuilder::new(g)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("[verify] protected {g:?}: {e}"))?,
        );
    }
    b.build().map_err(|e| e.to_string())
}

/// Where scratch copies of candidates are tested. Commands of a backend
/// running as root run as an unprivileged uid, which must be able to reach
/// the directory: it then lives under the system temporary directory
/// (mode 0711, owned by root) instead of the private state directory.
pub fn scratch_base(state_base: &Path, ws_root: &Path) -> PathBuf {
    let id = anchor_policy::audit::sha256_hex(ws_root.to_string_lossy().as_bytes());
    if anchor_exec::running_as_root() {
        std::env::temp_dir().join(format!("anchor-verify-{}", &id[..16]))
    } else {
        state_base.join("verify").join(&id[..16])
    }
}

/// Create `dir` (and its parents) for the scratch area.
fn create_scratch_dir(dir: &Path, root: bool) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", dir.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        let mode = if root { 0o711 } else { 0o700 };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(mode)
            .create(dir)
            .map_err(err)?;
        // Never trust a directory someone else made (a shared /tmp).
        let md = std::fs::symlink_metadata(dir).map_err(err)?;
        if !md.is_dir() || Some(md.uid()) != anchor_exec::effective_uid() {
            return Err(format!(
                "{}: not a directory owned by this user",
                dir.display()
            ));
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).map_err(err)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        std::fs::create_dir_all(dir).map_err(err)
    }
}

/// Write snapshot `snap` into the empty directory `dir`. The scratch area
/// belongs to the backend, so no policy approval is involved (unlike
/// materializing into a workspace).
fn export(store: &Store, snap: &Id, dir: &Path) -> Result<(), String> {
    let root = store.snapshot(snap).map_err(|e| e.to_string())?.root;
    export_tree(store, &root, dir)
}

fn export_tree(store: &Store, tree: &Id, dir: &Path) -> Result<(), String> {
    use anchor_snapshot::object::Node;
    let t = store.tree(tree).map_err(|e| e.to_string())?;
    for e in &t.entries {
        let Some(name) = name_path(&e.name) else {
            continue;
        };
        let p = dir.join(name);
        let err = |x: std::io::Error| format!("{}: {x}", p.display());
        match &e.node {
            Node::Dir { id, .. } => {
                std::fs::create_dir(&p).map_err(err)?;
                export_tree(store, id, &p)?;
            }
            Node::File { id, mode } => {
                let f = store.file(id).map_err(|e| e.to_string())?;
                let bytes = store.file_content(&f).map_err(|e| e.to_string())?;
                std::fs::write(&p, bytes).map_err(err)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let m = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
                    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(m))
                        .map_err(err)?;
                }
                #[cfg(not(unix))]
                let _ = mode;
            }
            Node::Symlink { target } => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStrExt;
                    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), &p)
                        .map_err(err)?;
                }
                #[cfg(not(unix))]
                let _ = target;
            }
            Node::Untracked(_) => {}
        }
    }
    Ok(())
}

fn name_path(name: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(name)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(name).ok().map(PathBuf::from)
    }
}

/// Hand a scratch tree to the uid commands run as (root backends only).
/// Never follows symlinks.
#[cfg(unix)]
fn chown_tree(dir: &Path, uid: u32) -> std::io::Result<()> {
    std::os::unix::fs::lchown(dir, Some(uid), Some(uid))?;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if e.file_type()?.is_dir() {
            chown_tree(&p, uid)?;
        } else {
            std::os::unix::fs::lchown(&p, Some(uid), Some(uid))?;
        }
    }
    Ok(())
}

fn tail(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut i = s.len() - n;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    s[i..].to_string()
}

/// One command's result: its record, its full output, and its two streams
/// (unittest reports on stderr).
struct Ran {
    record: CommandRecord,
    log: Vec<u8>,
    stdout: String,
    stderr: String,
}

/// The reviewer the gate uses.
pub type BoxedReviewer = Box<dyn Reviewer + Send>;

/// The verification gate. See the module documentation.
pub struct VerifyGate {
    spec: VerifySpec,
    protected: globset::GlobSet,
    scratch: PathBuf,
    hub: Option<Arc<Hub>>,
    reviewer: BoxedReviewer,
    path_var: String,
    read_only: Vec<PathBuf>,
    /// Tests that passed, by the tree they ran on (the pre-merge baseline).
    baseline: HashMap<Id, BTreeSet<String>>,
    stdlib: Option<BTreeSet<String>>,
    runs: u64,
    /// The last decision (the seeded-bug harness reads it).
    pub last: Option<GateRun>,
}

/// What one check decided, and on what.
#[derive(Debug, Clone)]
pub struct GateRun {
    pub input: ReviewInput,
    pub decision: Decision,
    /// Evidence group (`<group>/<file>` names) of this check.
    pub group: String,
}

impl std::fmt::Debug for VerifyGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifyGate")
            .field("reviewer", &self.reviewer.name())
            .field("scratch", &self.scratch)
            .finish()
    }
}

impl VerifyGate {
    /// `hub`: where evidence goes (`None` keeps it only in the decision).
    pub fn new(
        spec: VerifySpec,
        scratch: PathBuf,
        hub: Option<Arc<Hub>>,
        reviewer: BoxedReviewer,
        path_var: String,
        read_only: Vec<PathBuf>,
    ) -> Result<VerifyGate, String> {
        spec.validate()?;
        Ok(VerifyGate {
            protected: protected_set(&spec.protected)?,
            spec,
            scratch,
            hub,
            reviewer,
            path_var,
            read_only,
            baseline: HashMap::new(),
            stdlib: None,
            runs: 0,
            last: None,
        })
    }

    pub fn spec(&self) -> &VerifySpec {
        &self.spec
    }

    pub fn reviewer_name(&self) -> &str {
        self.reviewer.name()
    }

    fn run(&self, name: &str, argv: &[String], cwd: &Path, rw: &[PathBuf]) -> Ran {
        let mut record = CommandRecord {
            name: name.into(),
            argv: argv.to_vec(),
            ok: false,
            exit: None,
            timed_out: false,
            elapsed_ms: 0,
            user_cpu_ms: None,
            max_rss_bytes: None,
            output_tail: String::new(),
            artifact: String::new(),
        };
        let program = match anchor_exec::resolve_program(&argv[0], &self.path_var) {
            Ok(p) => p,
            Err(e) => {
                record.output_tail = format!("cannot run {:?}: {e}", argv[0]);
                let log = record.output_tail.clone().into_bytes();
                return Ran {
                    record,
                    log,
                    stdout: String::new(),
                    stderr: String::new(),
                };
            }
        };
        let spec = anchor_exec::CommandSpec {
            program,
            args: argv[1..].to_vec(),
            cwd: cwd.to_path_buf(),
            env: vec![
                ("PATH".into(), self.path_var.clone()),
                ("HOME".into(), cwd.display().to_string()),
                ("LANG".into(), "C.UTF-8".into()),
                ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
                ("PYTHONHASHSEED".into(), "0".into()),
            ],
            limits: anchor_exec::Limits {
                cpu_seconds: self.spec.timeout.as_secs(),
                wall_timeout: self.spec.timeout,
                ..anchor_exec::Limits::default()
            },
            network: anchor_exec::Network::Deny,
            fs: Some(anchor_exec::FsConfinement {
                read_write: rw.to_vec(),
                read_only: self.read_only.clone(),
            }),
        };
        match anchor_exec::run(&spec) {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
                record.exit = match o.exit {
                    anchor_exec::ExitKind::Exited(c) => Some(c),
                    _ => None,
                };
                record.timed_out = o.exit == anchor_exec::ExitKind::TimedOut;
                record.ok = record.exit == Some(0);
                record.elapsed_ms = o.elapsed.as_millis() as u64;
                record.user_cpu_ms = o.rusage.as_ref().map(|r| r.user_cpu.as_millis() as u64);
                record.max_rss_bytes = o.rusage.as_ref().map(|r| r.max_rss_bytes);
                let both = format!("{stdout}{stderr}");
                record.output_tail = tail(&both, OUTPUT_TAIL_BYTES);
                let log = format!(
                    "$ {argv:?}\nexit: {:?}\nisolation: network {:?}, fs_confined {}, \
                     dropped_root {}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}{}",
                    o.exit,
                    o.isolation.network,
                    o.isolation.fs_confined,
                    o.isolation.dropped_root,
                    if o.truncated {
                        "\n[output truncated]"
                    } else {
                        ""
                    }
                )
                .into_bytes();
                Ran {
                    record,
                    log,
                    stdout,
                    stderr,
                }
            }
            Err(e) => {
                // No sandbox, no run: the command fails, so the merge does.
                record.output_tail = format!("refused to run: {e}");
                let log = record.output_tail.clone().into_bytes();
                Ran {
                    record,
                    log,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            }
        }
    }

    /// Store one evidence file; returns its reference.
    fn persist(
        &self,
        group: &str,
        file: &str,
        kind: ArtifactKind,
        bytes: &[u8],
        meta: Value,
    ) -> String {
        let name = format!("{group}/{file}");
        match &self.hub {
            Some(hub) => match hub.register_evidence(Caller::System, kind, &name, bytes, meta) {
                Ok(a) => a.path,
                Err(e) => format!("unrecorded:{name} ({e})"),
            },
            None => format!("local:{name}"),
        }
    }

    /// Standard-library module names of the configured Python, when the
    /// test command runs Python (empty otherwise).
    fn stdlib_names(&mut self, cwd: &Path, rw: &[PathBuf]) -> BTreeSet<String> {
        if let Some(s) = &self.stdlib {
            return s.clone();
        }
        let prog = &self.spec.test[0];
        let base = Path::new(prog)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let mut names = BTreeSet::new();
        if base.starts_with("python") {
            let argv = vec![
                prog.clone(),
                "-I".into(),
                "-c".into(),
                "import sys\nfor n in sorted(getattr(sys, 'stdlib_module_names', ())): print(n)"
                    .into(),
            ];
            let r = self.run("stdlib", &argv, cwd, rw);
            if r.record.ok {
                names.extend(
                    r.stdout
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .map(String::from),
                );
            }
            names.extend(["sitecustomize".into(), "usercustomize".into()]);
        }
        self.stdlib = Some(names.clone());
        names
    }
}

/// The changed files between `pre` and `cand`, with their line diffs.
fn changed_files(
    store: &Store,
    pre: &Id,
    cand: &Id,
    protected: &globset::GlobSet,
) -> Result<Vec<ChangedFile>, String> {
    let mut out = Vec::new();
    for c in anchor_snapshot::diff_files(store, pre, cand).map_err(|e| e.to_string())? {
        let path = crate::queue::slash_path(&c.path);
        let mut f = ChangedFile {
            is_test: anchor_verify::is_test_path(&path) || protected.is_match(&path),
            status: match c.kind {
                ChangeKind::Added => FileStatus::Added,
                ChangeKind::Removed => FileStatus::Removed,
                ChangeKind::Modified | ChangeKind::ModeChanged => FileStatus::Modified,
            },
            path,
            added_lines: Vec::new(),
            removed_lines: 0,
            added_text: Vec::new(),
            diff: String::new(),
        };
        match textdiff::file_diff(store, pre, cand, &c.path).map_err(|e| e.to_string())? {
            TextDiff::Text(hunks) => {
                for h in &hunks {
                    for (n, l) in h.added_lines().into_iter().zip(
                        h.lines
                            .iter()
                            .filter(|l| l.kind == textdiff::LineKind::Added),
                    ) {
                        f.added_lines.push(n as u32);
                        f.added_text.push(l.text.clone());
                    }
                    f.removed_lines += h
                        .lines
                        .iter()
                        .filter(|l| l.kind == textdiff::LineKind::Removed)
                        .count() as u32;
                }
                f.diff = textdiff::render(&f.path, &hunks);
            }
            TextDiff::Binary | TextDiff::TooLarge => f.status = FileStatus::Opaque,
        }
        out.push(f);
    }
    Ok(out)
}

/// Cut the review input down to its size limit, largest diffs first; a
/// reviewer cannot support claims over an input marked truncated.
fn fit(input: &mut ReviewInput) {
    while serde_json::to_vec(input).map_or(usize::MAX, |v| v.len()) > MAX_REVIEW_BYTES {
        input.truncated = true;
        let biggest = input
            .changes
            .iter_mut()
            .filter(|c| !c.diff.is_empty() || !c.added_text.is_empty())
            .max_by_key(|c| c.diff.len() + c.added_text.iter().map(String::len).sum::<usize>());
        match biggest {
            Some(c) => {
                c.diff = "[diff omitted: the review input was too large]\n".into();
                c.added_text.clear();
            }
            None => {
                for c in &mut input.commands {
                    c.output_tail.clear();
                }
                break;
            }
        }
    }
}

fn unique_suffix(runs: u64) -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{ms}-{runs}")
}

impl VerifyGate {
    /// Check one candidate (the merge queue's gate, and the seeded-bug
    /// harness, call this).
    pub fn verify(
        &mut self,
        ctx: &GateContext<'_>,
        store: &SharedStore,
    ) -> Result<GateRun, String> {
        self.runs += 1;
        let group = format!(
            "verify-{:010}-{}-{}",
            ctx.seq,
            ctx.attempt,
            unique_suffix(self.runs)
        );
        if cfg!(windows) {
            // Agent commands have no sandbox on Windows: fail closed.
            let input = ReviewInput {
                claims: ctx.claims.to_vec(),
                changes: Vec::new(),
                commands: Vec::new(),
                tests: Vec::new(),
                coverage: None,
                claim_refs: BTreeMap::new(),
                truncated: false,
            };
            let decision = Decision {
                approved: false,
                reviewer: self.reviewer.name().to_string(),
                findings: vec![Finding::new(
                    Stage::Rules,
                    "unsupported_platform",
                    json!({"detail": "verification commands cannot be sandboxed on Windows"}),
                )],
                verdicts: Vec::new(),
            };
            return Ok(GateRun {
                input,
                decision,
                group,
            });
        }
        let root = anchor_exec::running_as_root();
        create_scratch_dir(&self.scratch, root)?;
        let run_dir = self.scratch.join(&group);
        create_scratch_dir(&run_dir, root)?;
        let result = self.verify_in(ctx, store, &group, &run_dir, root);
        let _ = std::fs::remove_dir_all(&run_dir);
        result
    }

    fn verify_in(
        &mut self,
        ctx: &GateContext<'_>,
        store: &SharedStore,
        group: &str,
        run_dir: &Path,
        root: bool,
    ) -> Result<GateRun, String> {
        let cand_ws = run_dir.join("ws");
        let pre_ws = run_dir.join("pre");
        let cover = run_dir.join("cover");
        for d in [&cand_ws, &pre_ws, &cover] {
            std::fs::create_dir(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        // Read everything needed from the store, then let go of it.
        let (changes, pre_tree) = {
            let st = lock_store(store);
            let pre_tree = st.snapshot(&ctx.pre).map_err(|e| e.to_string())?.root;
            let changes = changed_files(&st, &ctx.pre, &ctx.candidate, &self.protected)?;
            export(&st, &ctx.candidate, &cand_ws)?;
            if !self.baseline.contains_key(&pre_tree) {
                export(&st, &ctx.pre, &pre_ws)?;
            }
            (changes, pre_tree)
        };
        #[cfg(unix)]
        if root {
            for d in [&cand_ws, &pre_ws, &cover] {
                chown_tree(d, anchor_exec::NOBODY_UID)
                    .map_err(|e| format!("{}: {e}", d.display()))?;
            }
        }
        let _ = root;

        let mut prior = Vec::new();
        // Hard rule: existing protected files (the tests) stay as they are.
        let tampered: Vec<&str> = changes
            .iter()
            .filter(|c| self.protected.is_match(&c.path) && c.status != FileStatus::Added)
            .map(|c| c.path.as_str())
            .collect();
        if !tampered.is_empty() {
            prior.push(Finding::new(
                Stage::Rules,
                "tests_tampered",
                json!({ "paths": tampered }),
            ));
        }
        // Hard rule: no new top-level module named like a stdlib module.
        let stdlib = self.stdlib_names(&cand_ws, std::slice::from_ref(&cand_ws));
        let shadowing: Vec<&str> = changes
            .iter()
            .filter(|c| c.status != FileStatus::Removed)
            .filter(|c| {
                let top = match c.path.split_once('/') {
                    None => c.path.strip_suffix(".py"),
                    Some((dir, "__init__.py")) => Some(dir),
                    _ => None,
                };
                top.is_some_and(|m| stdlib.contains(m))
            })
            .map(|c| c.path.as_str())
            .collect();
        if !shadowing.is_empty() {
            prior.push(Finding::new(
                Stage::Rules,
                "shadows_stdlib",
                json!({ "paths": shadowing }),
            ));
        }

        // The pre-merge baseline: which tests passed before.
        if !self.baseline.contains_key(&pre_tree) {
            let r = self.run(
                "baseline",
                &self.spec.test.clone(),
                &pre_ws,
                std::slice::from_ref(&pre_ws),
            );
            let (records, _) = anchor_verify::parse::parse_unittest(&r.stderr);
            let passed = records
                .into_iter()
                .filter(|t| t.outcome == TestOutcome::Passed)
                .map(|t| t.id)
                .collect();
            self.baseline.insert(pre_tree, passed);
        }

        let mut commands = Vec::new();
        let mut build_ok = true;
        if let Some(b) = self.spec.build.clone() {
            let mut r = self.run("build", &b, &cand_ws, std::slice::from_ref(&cand_ws));
            r.record.artifact = self.persist(
                group,
                "build.log",
                ArtifactKind::Log,
                &r.log,
                json!({"type": "build", "merge": ctx.seq}),
            );
            build_ok = r.record.ok;
            commands.push(r.record);
        }
        let mut tests = Vec::new();
        let mut coverage = None;
        if build_ok {
            let t = self.spec.test.clone();
            let mut r = self.run("test", &t, &cand_ws, std::slice::from_ref(&cand_ws));
            r.record.artifact = self.persist(
                group,
                "test.log",
                ArtifactKind::TestOutput,
                &r.log,
                json!({"type": "test", "merge": ctx.seq}),
            );
            let (records, summary) = anchor_verify::parse::parse_unittest(&r.stderr);
            if r.record.ok && (!summary.ok || summary.ran == 0) {
                // A zero exit without the runner's own success summary: the
                // process did not end the way a passing test run ends.
                prior.push(Finding::new(
                    Stage::Tests,
                    "tests_inconclusive",
                    json!({ "ran": summary.ran, "summary_ok": summary.ok }),
                ));
            }
            tests = records;
            commands.push(r.record);

            if let Some(c) = self.spec.coverage.clone() {
                let dir = cover.display().to_string();
                let argv: Vec<String> = c.iter().map(|a| a.replace(COVERDIR, &dir)).collect();
                let mut r = self.run(
                    "coverage",
                    &argv,
                    &cand_ws,
                    &[cand_ws.clone(), cover.clone()],
                );
                r.record.artifact = self.persist(
                    group,
                    "coverage.log",
                    ArtifactKind::CoverageReport,
                    &r.log,
                    json!({"type": "coverage", "merge": ctx.seq}),
                );
                let mut files = Vec::new();
                for ch in changes.iter().filter(|c| {
                    !c.is_test && c.status != FileStatus::Removed && c.path.ends_with(".py")
                }) {
                    let Some(name) = anchor_verify::parse::cover_name(&ch.path) else {
                        continue;
                    };
                    let fc = match std::fs::read_to_string(cover.join(&name)) {
                        Ok(text) => FileCoverage {
                            path: ch.path.clone(),
                            instrumented: true,
                            uncovered_lines: anchor_verify::parse::parse_cover(&text),
                        },
                        Err(_) => FileCoverage {
                            path: ch.path.clone(),
                            instrumented: false,
                            uncovered_lines: Vec::new(),
                        },
                    };
                    files.push(fc);
                }
                coverage = Some(files);
                commands.push(r.record);
            }

            // No regressions: every test that passed before still passes.
            let now: BTreeSet<&str> = tests
                .iter()
                .filter(|t| t.outcome == TestOutcome::Passed)
                .map(|t| t.id.as_str())
                .collect();
            let lost: Vec<&String> = self.baseline[&pre_tree]
                .iter()
                .filter(|t| !now.contains(t.as_str()))
                .collect();
            if !lost.is_empty() {
                prior.push(Finding::new(
                    Stage::Tests,
                    "test_regressed",
                    json!({ "tests": lost }),
                ));
            }
        }

        let (claim_refs, auto) =
            anchor_verify::resolve(ctx.claims, &changes, &tests, coverage.as_deref());
        let mut input = ReviewInput {
            claims: ctx.claims.to_vec(),
            changes,
            commands,
            tests,
            coverage,
            claim_refs,
            truncated: false,
        };
        fit(&mut input);
        let decision = decide(&input, prior, auto, self.reviewer.as_mut());
        let bytes = serde_json::to_vec_pretty(&input).unwrap_or_default();
        self.persist(
            group,
            "review-input.json",
            ArtifactKind::Log,
            &bytes,
            json!({"type": "review_input", "merge": ctx.seq}),
        );
        let bytes = serde_json::to_vec_pretty(&decision).unwrap_or_default();
        self.persist(
            group,
            "decision.json",
            ArtifactKind::Log,
            &bytes,
            json!({"type": "decision", "merge": ctx.seq, "approved": decision.approved}),
        );
        Ok(GateRun {
            input,
            decision,
            group: group.to_string(),
        })
    }
}

impl MergeGate for VerifyGate {
    fn check(&mut self, ctx: &GateContext<'_>, store: &SharedStore) -> Result<Value, Value> {
        match self.verify(ctx, store) {
            // Verification could not run: fail closed.
            Err(e) => Err(json!({
                "approved": false,
                "findings": [{"stage": "rules", "code": "verification_error", "detail": {"error": e}}],
            })),
            Ok(run) => {
                let out = if run.decision.approved {
                    Ok(json!({
                        "approved": true,
                        "reviewer": run.decision.reviewer,
                        "claims": run.input.claims.len(),
                        "tests": run.input.tests.len(),
                        "evidence": run.group,
                    }))
                } else {
                    let mut fb = run.decision.feedback(&run.input);
                    fb["evidence"] = json!(run.group);
                    Err(fb)
                };
                self.last = Some(run);
                out
            }
        }
    }
}

/// Model usage of review sessions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReviewUsage {
    pub sessions: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
}

/// A model reviewer: a runtime session on the review route, with no
/// workspace tools (ADR-0009), whose verdicts the gate uses.
pub struct ModelReviewer {
    mediator: Mediator,
    checkpoints: CheckpointStore,
    cfg: SessionConfig,
    view: RolesView,
    executor_families: Vec<String>,
    sessions: u64,
    pub usage: Arc<Mutex<ReviewUsage>>,
}

impl std::fmt::Debug for ModelReviewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelReviewer")
            .field("executor_families", &self.executor_families)
            .finish()
    }
}

impl ModelReviewer {
    /// `mediator`: any mediator (review sessions are refused every tool).
    pub fn new(
        mediator: Mediator,
        checkpoints: CheckpointStore,
        agent_bin: PathBuf,
        roles_toml: String,
        mock_script: Option<String>,
        executor_families: Vec<String>,
    ) -> Result<ModelReviewer, String> {
        let view: RolesView = toml::from_str(&roles_toml).map_err(|e| e.to_string())?;
        if let Some(f) = shared_family(&view.families(Route::Review), &executor_families) {
            return Err(format!(
                "the reviewer can run on model family {f:?}, as an executor can (spec §8.2(a))"
            ));
        }
        Ok(ModelReviewer {
            mediator,
            checkpoints,
            cfg: SessionConfig {
                agent_bin,
                session_id: String::new(),
                instruction: "Review the change.".into(),
                roles_toml,
                checkpoint_threshold: anchor_checkpoint::DEFAULT_THRESHOLD,
                resume_from: None,
                mock_script,
                hub: None,
                review: None,
            },
            view,
            executor_families,
            sessions: 0,
            usage: Arc::new(Mutex::new(ReviewUsage::default())),
        })
    }
}

/// Providers a session's notices say it ran on.
fn notice_providers(notices: &[Notice]) -> Vec<String> {
    let mut out = Vec::new();
    for n in notices {
        match n.kind {
            NoticeKind::RoutingDecision => {
                if let Some(p) = n
                    .detail
                    .split_whitespace()
                    .find_map(|w| w.strip_prefix("provider="))
                {
                    out.push(p.to_string());
                }
            }
            NoticeKind::ProviderFallback => {
                if let Some((_, p)) = n.detail.rsplit_once("falling back to ") {
                    out.push(p.trim().trim_matches('"').to_string());
                }
            }
            _ => {}
        }
    }
    out
}

/// §8.2(a) once more, on what a review session actually ran on.
fn check_ran_on(
    view: &RolesView,
    executor_families: &[String],
    notices: &[Notice],
) -> Result<(), String> {
    let providers = notice_providers(notices);
    if providers.is_empty() {
        return Err("the review session reported no routing decision".into());
    }
    for p in providers {
        let family = view
            .providers
            .get(&p)
            .map(|pv| normalize_family(&pv.family))
            .ok_or_else(|| format!("the reviewer ran on unknown provider {p:?}"))?;
        if executor_families.contains(&family) {
            return Err(format!(
                "reviewer_family_clash: the reviewer ran on {p:?} (family {family:?}), \
                 an executor's family"
            ));
        }
    }
    Ok(())
}

impl Reviewer for ModelReviewer {
    fn name(&self) -> &str {
        "model"
    }

    fn review(&mut self, input: &ReviewInput) -> Result<Vec<ClaimVerdict>, String> {
        self.sessions += 1;
        let mut cfg = self.cfg.clone();
        cfg.review = Some(input.clone());
        cfg.session_id = format!("{}-review{}", session::new_session_id(), self.sessions);
        let rep = session::run(&cfg, &mut self.mediator, &self.checkpoints)
            .map_err(|e| format!("review session: {e}"))?;
        {
            let mut u = self.usage.lock().unwrap_or_else(|p| p.into_inner());
            u.sessions += 1;
            u.tokens_in += rep.tokens_in;
            u.tokens_out += rep.tokens_out;
            u.cost_usd += rep.cost_usd;
        }
        check_ran_on(&self.view, &self.executor_families, &rep.notices)?;
        if let Some(f) = rep.failed {
            return Err(format!("review session failed: {f}"));
        }
        rep.verdicts
            .ok_or_else(|| "the reviewer returned no verdicts".to_string())
    }
}

/// The rule baseline, boxed for the gate.
pub fn rule_reviewer() -> BoxedReviewer {
    Box::new(RuleReviewer)
}

/// The executor-facing text of a gate rejection (`feedback.detail`).
pub fn render_gate_feedback(feedback: &Value) -> String {
    let detail = &feedback["detail"];
    if detail.get("findings").is_some() {
        anchor_verify::render_feedback(detail)
    } else {
        format!("Your previous change was REJECTED by verification:\n{detail}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(kind: NoticeKind, detail: &str) -> Notice {
        Notice {
            kind,
            detail: detail.into(),
        }
    }

    #[test]
    fn verdicts_from_an_executor_family_are_refused() {
        let view: RolesView = toml::from_str(
            "[providers.a]\nfamily = \"Fam-A\"\n[providers.b]\nfamily = \"fam-b\"\n",
        )
        .unwrap();
        let exec = vec!["fam-a".to_string()];
        let routed = notice(
            NoticeKind::RoutingDecision,
            "role=reviewer provider=b model=m reason: review route",
        );
        check_ran_on(&view, &exec, std::slice::from_ref(&routed)).unwrap();
        // The session fell back into the executor's family mid-review.
        let fell = notice(
            NoticeKind::ProviderFallback,
            "provider \"b\" failed (overloaded); falling back to \"a\"",
        );
        let e = check_ran_on(&view, &exec, &[routed.clone(), fell]).unwrap_err();
        assert!(e.contains("reviewer_family_clash"), "{e}");
        // No routing notice, or an unknown provider: refused too.
        assert!(check_ran_on(&view, &exec, &[]).is_err());
        let odd = notice(NoticeKind::RoutingDecision, "role=reviewer provider=zz");
        assert!(check_ran_on(&view, &exec, &[odd]).is_err());
    }

    #[test]
    fn verify_config_is_validated() {
        let dir = std::path::Path::new(".");
        let file = |extra: &str| -> Result<VerifySpec, String> {
            let f: VerifyFile = toml::from_str(&format!(
                "test = [\"python3\", \"-m\", \"unittest\"]\n{extra}"
            ))
            .map_err(|e| e.to_string())?;
            VerifySpec::load(f, dir)
        };
        let s = file("").unwrap();
        assert_eq!(s.protected, ["tests/**"]);
        assert_eq!((s.max_attempts, s.reviewer), (3, ReviewerKind::Rule));
        assert!(file("coverage = [\"python3\", \"-m\", \"trace\"]").is_err());
        assert!(file("max_attempts = 0").is_err());
        assert!(file("reviewer = \"model\"").is_err());
        assert!(file("unknown = 1").is_err());
        assert!(file("build = []").is_err());
    }
}
