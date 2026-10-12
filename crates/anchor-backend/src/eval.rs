//! The seeded-bug harness (spec §8.5, §16.3): measures how often the
//! verification gate lets a known-buggy change through.
//!
//! A corpus (see `examples/eval/README.md`) holds small tasks. Each has a
//! starting workspace, a correct change (`solution/`), seeded buggy changes
//! (`bugs/<name>/`, each with a category), the executor's claims, and
//! hidden acceptance tests that decide the ground truth: a variant is
//! *correct* iff its hidden tests pass. The pipeline never sees them.
//!
//! Every variant (each solution and each bug) is checked by the very
//! [`VerifyGate`] merges use, in review-only mode: the start is the
//! pre-merge workspace, the variant the candidate. A rejected bug is a
//! true positive; an approved bug a false negative.
//!
//! The corpus is trusted input (like a test suite): its hidden tests run
//! without the sandbox.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anchor_protocol::verify::{Claim, ClaimVerdict, ReviewInput, Verdict};
use anchor_snapshot::{Id, Root, SnapshotOptions, Store};
use anchor_verify::{Reviewer, Stage};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::mediator::SharedStore;
use crate::queue::GateContext;
use crate::verify::{ReviewUsage, VerifyFile, VerifyGate, VerifySpec};

/// Overlay files that are instructions, not workspace content.
const DELETE_FILE: &str = "ANCHOR_DELETE";
const CLAIMS_FILE: &str = "ANCHOR_CLAIMS.json";
/// Where hidden tests are put next to a variant for the ground truth.
const HIDDEN_DIR: &str = "_anchor_hidden";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusFile {
    verify: VerifyFile,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    instruction: String,
    #[serde(rename = "bug", default)]
    bugs: Vec<Bug>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bug {
    pub name: String,
    pub category: String,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub name: String,
    pub dir: PathBuf,
    pub instruction: String,
    pub claims: Vec<Claim>,
    pub bugs: Vec<Bug>,
}

#[derive(Debug, Clone)]
pub struct Corpus {
    pub dir: PathBuf,
    pub spec: VerifySpec,
    pub tasks: Vec<Task>,
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Corpus {
    /// Load `dir/corpus.toml` and every task directory beside it.
    pub fn load(dir: &Path) -> Result<Corpus, String> {
        let read =
            |p: &Path| std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()));
        let cf: CorpusFile = toml::from_str(&read(&dir.join("corpus.toml"))?)
            .map_err(|e| format!("corpus.toml: {e}"))?;
        let spec = VerifySpec::load(cf.verify, dir)?;
        let mut tasks = Vec::new();
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .filter_map(Result::ok)
            .filter(|e| e.path().join("task.toml").is_file())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if !valid_name(&name) {
                return Err(format!("task directory {name:?}: use [A-Za-z0-9_-]"));
            }
            let tdir = e.path();
            let tf: TaskFile = toml::from_str(&read(&tdir.join("task.toml"))?)
                .map_err(|e| format!("{name}/task.toml: {e}"))?;
            let claims: Vec<Claim> = serde_json::from_str(&read(&tdir.join("claims.json"))?)
                .map_err(|e| format!("{name}/claims.json: {e}"))?;
            anchor_protocol::verify::validate_claims(&claims)
                .map_err(|e| format!("{name}/claims.json: {e}"))?;
            for b in &tf.bugs {
                if !valid_name(&b.name) || !tdir.join("bugs").join(&b.name).is_dir() {
                    return Err(format!("{name}: bug {:?} has no bugs/ directory", b.name));
                }
            }
            for d in ["start", "solution", "hidden"] {
                if !tdir.join(d).is_dir() {
                    return Err(format!("{name}: missing {d}/"));
                }
            }
            tasks.push(Task {
                name,
                dir: tdir,
                instruction: tf.instruction,
                claims,
                bugs: tf.bugs,
            });
        }
        if tasks.is_empty() {
            return Err(format!("{}: no tasks", dir.display()));
        }
        Ok(Corpus {
            dir: dir.to_path_buf(),
            spec,
            tasks,
        })
    }

    /// Keep only the named tasks.
    pub fn retain(&mut self, names: &[String]) -> Result<(), String> {
        for n in names {
            if !self.tasks.iter().any(|t| &t.name == n) {
                return Err(format!("no task {n:?}"));
            }
        }
        self.tasks.retain(|t| names.contains(&t.name));
        Ok(())
    }
}

fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    let err = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    std::fs::create_dir_all(dst).map_err(|e| err(dst, e))?;
    for e in std::fs::read_dir(src).map_err(|e| err(src, e))? {
        let e = e.map_err(|e| err(src, e))?;
        let name = e.file_name();
        if name == DELETE_FILE || name == CLAIMS_FILE || name == "__pycache__" {
            continue;
        }
        let (from, to) = (e.path(), dst.join(&name));
        let ft = e.file_type().map_err(|e| err(&from, e))?;
        if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else if ft.is_file() {
            std::fs::copy(&from, &to).map_err(|e| err(&from, e))?;
        }
    }
    Ok(())
}

/// Write `task`'s start plus its solution (plus `bug`) into `out`; return
/// the claims that go with it.
pub fn build_variant(task: &Task, bug: Option<&str>, out: &Path) -> Result<Vec<Claim>, String> {
    copy_tree(&task.dir.join("start"), out)?;
    let mut layers = vec![task.dir.join("solution")];
    if let Some(b) = bug {
        layers.push(task.dir.join("bugs").join(b));
    }
    let mut claims = task.claims.clone();
    for l in &layers {
        copy_tree(l, out)?;
        if let Ok(list) = std::fs::read_to_string(l.join(DELETE_FILE)) {
            for rel in list.lines().map(str::trim).filter(|r| !r.is_empty()) {
                if rel
                    .split('/')
                    .any(|c| c.is_empty() || c == "." || c == "..")
                {
                    return Err(format!("{}: bad path {rel:?}", l.display()));
                }
                std::fs::remove_file(out.join(rel))
                    .map_err(|e| format!("{}: deleting {rel}: {e}", l.display()))?;
            }
        }
        if let Ok(text) = std::fs::read_to_string(l.join(CLAIMS_FILE)) {
            claims = serde_json::from_str(&text)
                .map_err(|e| format!("{}: {CLAIMS_FILE}: {e}", l.display()))?;
        }
    }
    Ok(claims)
}

/// Run unittest in `dir` (`args` after `-m unittest`): passed iff it exits
/// 0 and prints its own success summary for at least one test.
fn unittest_passes(python: &str, dir: &Path, args: &[&str]) -> Result<bool, String> {
    let path = std::env::var("PATH").unwrap_or_default();
    let prog = anchor_exec::resolve_program(python, &path).map_err(|e| e.to_string())?;
    let out = std::process::Command::new(prog)
        // -I ignores PYTHON* variables: -B keeps bytecode out of the tree.
        .args(["-I", "-B", "-m", "unittest"])
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(anchor_exec::os_env())
        .env("PATH", path)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .map_err(|e| format!("running {python}: {e}"))?;
    let (_, summary) = anchor_verify::parse::parse_unittest(&String::from_utf8_lossy(&out.stderr));
    Ok(out.status.success() && summary.ok && summary.ran > 0)
}

/// The ground truth: do `task`'s hidden tests pass on the variant in `dir`?
pub fn hidden_tests_pass(python: &str, task: &Task, dir: &Path) -> Result<bool, String> {
    let h = dir.join(HIDDEN_DIR);
    copy_tree(&task.dir.join("hidden"), &h)?;
    std::fs::write(h.join("__init__.py"), "").map_err(|e| e.to_string())?;
    let ok = unittest_passes(python, dir, &["discover", "-s", HIDDEN_DIR, "-t", "."]);
    let _ = std::fs::remove_dir_all(&h);
    ok
}

fn python_of(spec: &VerifySpec) -> String {
    spec.test[0].clone()
}

/// A fresh, empty, private working directory for one harness run.
fn work_dir(label: &str) -> Result<PathBuf, String> {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let d = std::env::temp_dir().join(format!("anchor-eval-{label}-{}-{ms}", std::process::id()));
    crate::create_private_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d)
}

/// Check the corpus itself before measuring anything with it: each start's
/// tests pass, each solution passes its visible and hidden tests, each bug
/// fails its hidden tests. Returns every problem found.
pub fn self_check(corpus: &Corpus) -> Result<Vec<String>, String> {
    let python = python_of(&corpus.spec);
    let work = work_dir("check")?;
    let mut problems = Vec::new();
    let mut n = 0;
    let mut fresh = || {
        n += 1;
        work.join(format!("v{n}"))
    };
    for t in &corpus.tasks {
        let d = fresh();
        copy_tree(&t.dir.join("start"), &d)?;
        if !unittest_passes(&python, &d, &["discover"])? {
            problems.push(format!("{}: the start's tests do not pass", t.name));
        }
        let d = fresh();
        build_variant(t, None, &d)?;
        if !unittest_passes(&python, &d, &["discover"])? {
            problems.push(format!("{}: the solution fails its visible tests", t.name));
        }
        if !hidden_tests_pass(&python, t, &d)? {
            problems.push(format!("{}: the solution fails the hidden tests", t.name));
        }
        for b in &t.bugs {
            let d = fresh();
            build_variant(t, Some(&b.name), &d)?;
            if hidden_tests_pass(&python, t, &d)? {
                problems.push(format!(
                    "{}/{}: the bug passes the hidden tests",
                    t.name, b.name
                ));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&work);
    Ok(problems)
}

/// Calibration reviewers, to check the harness itself: one that supports
/// every claim, and one that knows the ground truth.
#[derive(Debug)]
pub enum Calibration {
    AcceptAll,
    /// Supports the claims iff the flag (set per variant) says "correct".
    Oracle(Arc<AtomicBool>),
}

impl Reviewer for Calibration {
    fn name(&self) -> &str {
        match self {
            Calibration::AcceptAll => "accept-all",
            Calibration::Oracle(_) => "oracle",
        }
    }
    fn review(&mut self, input: &ReviewInput) -> Result<Vec<ClaimVerdict>, String> {
        let ok = match self {
            Calibration::AcceptAll => true,
            Calibration::Oracle(correct) => correct.load(Ordering::SeqCst),
        };
        Ok(input
            .claims
            .iter()
            .map(|c| ClaimVerdict {
                claim_id: c.id.clone(),
                verdict: if ok {
                    Verdict::Supported
                } else {
                    Verdict::Refuted
                },
                reason_code: if ok { "accepted" } else { "oracle_refuted" }.into(),
                evidence_refs: Vec::new(),
                note: String::new(),
            })
            .collect())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VariantResult {
    pub task: String,
    /// `None` for the task's correct solution.
    pub bug: Option<String>,
    pub category: Option<String>,
    /// Ground truth: the hidden tests fail.
    pub buggy: bool,
    /// The gate approved the change (it would merge).
    pub approved: bool,
    /// The deterministic stages passed it on to the reviewer.
    pub reached_review: bool,
    /// Finding codes of the gate's decision.
    pub findings: Vec<String>,
    pub wall_ms: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Matrix {
    /// Buggy and rejected.
    pub tp: u32,
    /// Buggy and approved (it would merge).
    pub fn_: u32,
    /// Correct and rejected.
    pub fp: u32,
    /// Correct and approved.
    pub tn: u32,
}

fn rate(num: u32, den: u32) -> Option<f64> {
    (den > 0).then(|| f64::from(num) / f64::from(den))
}

impl Matrix {
    pub fn add(&mut self, buggy: bool, approved: bool) {
        match (buggy, approved) {
            (true, false) => self.tp += 1,
            (true, true) => self.fn_ += 1,
            (false, false) => self.fp += 1,
            (false, true) => self.tn += 1,
        }
    }
    /// Share of buggy changes that would merge.
    pub fn fn_rate(&self) -> Option<f64> {
        rate(self.fn_, self.tp + self.fn_)
    }
    /// Share of buggy changes caught.
    pub fn tp_rate(&self) -> Option<f64> {
        rate(self.tp, self.tp + self.fn_)
    }
    /// Share of correct changes rejected.
    pub fn fp_rate(&self) -> Option<f64> {
        rate(self.fp, self.fp + self.tn)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub reviewer: String,
    pub tasks: usize,
    pub variants: Vec<VariantResult>,
    /// The whole gate: does a buggy change merge?
    pub gate: Matrix,
    /// Per bug category (buggy variants only: tp and fn).
    pub by_category: BTreeMap<String, Matrix>,
    /// The reviewer alone, on the variants the deterministic stages passed
    /// on to it.
    pub reviewer_stage: Matrix,
    /// Correct solutions approved, out of all tasks (§16.3 completion, in
    /// review-only mode: whether a correct change gets through).
    pub completion_rate: Option<f64>,
    pub wall_ms: u64,
    /// Model reviewer usage (`None` for reviewers without a model).
    pub usage: Option<ReviewUsage>,
}

fn pct(r: Option<f64>) -> String {
    r.map_or("n/a".into(), |r| format!("{:.1}%", r * 100.0))
}

impl Report {
    pub fn to_markdown(&self) -> String {
        let mut s = String::new();
        let g = &self.gate;
        s.push_str(&format!(
            "Reviewer: **{}**, {} tasks, {} variants ({} buggy), {:.1} s.\n\n",
            self.reviewer,
            self.tasks,
            self.variants.len(),
            g.tp + g.fn_,
            self.wall_ms as f64 / 1000.0
        ));
        s.push_str("| Gate (does it merge?) | Rejected | Approved |\n|---|---|---|\n");
        s.push_str(&format!("| Buggy | {} (TP) | {} (FN) |\n", g.tp, g.fn_));
        s.push_str(&format!("| Correct | {} (FP) | {} (TN) |\n\n", g.fp, g.tn));
        s.push_str(&format!(
            "False-negative rate **{}**; bugs caught {}; correct changes rejected {}; \
             completion (correct solutions approved) {}.\n\n",
            pct(g.fn_rate()),
            pct(g.tp_rate()),
            pct(g.fp_rate()),
            pct(self.completion_rate)
        ));
        s.push_str(
            "| Bug category | Bugs | Caught | Merged (FN) | FN rate |\n|---|---|---|---|---|\n",
        );
        for (c, m) in &self.by_category {
            s.push_str(&format!(
                "| {c} | {} | {} | {} | {} |\n",
                m.tp + m.fn_,
                m.tp,
                m.fn_,
                pct(m.fn_rate())
            ));
        }
        let r = &self.reviewer_stage;
        s.push_str(&format!(
            "\nReviewer stage alone (variants that passed build, tests and hard rules): \
             {} buggy, {} caught, FN rate {}; {} correct, {} rejected.\n",
            r.tp + r.fn_,
            r.tp,
            pct(r.fn_rate()),
            r.fp + r.tn,
            r.fp
        ));
        if let Some(u) = &self.usage {
            s.push_str(&format!(
                "\nModel usage: {} review sessions, {} tokens in, {} out, ${:.4} \
                 (${:.4} per task).\n",
                u.sessions,
                u.tokens_in,
                u.tokens_out,
                u.cost_usd,
                u.cost_usd / self.tasks.max(1) as f64
            ));
        } else {
            s.push_str("\nCost: n/a (no model).\n");
        }
        s.push_str("\n| Task | Variant | Category | Truth | Gate | Findings | ms |\n|---|---|---|---|---|---|---|\n");
        for v in &self.variants {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                v.task,
                v.bug.as_deref().unwrap_or("solution"),
                v.category.as_deref().unwrap_or("-"),
                if v.buggy { "buggy" } else { "correct" },
                if v.approved { "approved" } else { "rejected" },
                v.findings.join(", "),
                v.wall_ms
            ));
        }
        s
    }
}

/// Snapshot directory `dir` into `store` (its own root, so the
/// change-detection index of one variant never serves another).
fn snapshot_dir(store: &Store, dir: &Path, root: &str) -> Result<Id, String> {
    let ws = anchor_fs::Workspace::open(dir).map_err(|e| e.to_string())?;
    let opts = SnapshotOptions {
        root: Root::named(root).map_err(|e| e.to_string())?,
        ..SnapshotOptions::default()
    };
    anchor_snapshot::snapshot(store, &ws, &opts, root)
        .map(|r| r.id())
        .map_err(|e| e.to_string())
}

/// Measure `gate` on every variant of `corpus`. `oracle`, if given, is
/// told each variant's ground truth before it is checked (for
/// [`Calibration::Oracle`]); `usage` is a model reviewer's usage counter.
pub fn run(
    corpus: &Corpus,
    gate: &mut VerifyGate,
    oracle: Option<&AtomicBool>,
    usage: Option<Arc<Mutex<ReviewUsage>>>,
) -> Result<Report, String> {
    let python = python_of(&corpus.spec);
    let started = Instant::now();
    let work = work_dir("run")?;
    let store: SharedStore = Arc::new(Mutex::new(
        Store::open(&work.join("store")).map_err(|e| e.to_string())?,
    ));
    let mut variants = Vec::new();
    let mut n = 0u32;
    for t in &corpus.tasks {
        let start_dir = work.join(format!("{}-start", t.name));
        copy_tree(&t.dir.join("start"), &start_dir)?;
        let pre = {
            let st = crate::mediator::lock_store(&store);
            snapshot_dir(&st, &start_dir, &format!("s{n}"))?
        };
        let mut cases: Vec<Option<&Bug>> = vec![None];
        cases.extend(t.bugs.iter().map(Some));
        for bug in cases {
            n += 1;
            let dir = work.join(format!("v{n}"));
            let claims = build_variant(t, bug.map(|b| b.name.as_str()), &dir)?;
            let cand = {
                let st = crate::mediator::lock_store(&store);
                snapshot_dir(&st, &dir, &format!("v{n}"))?
            };
            let buggy = !hidden_tests_pass(&python, t, &dir)?;
            let changes = {
                let st = crate::mediator::lock_store(&store);
                anchor_snapshot::diff_files(&st, &pre, &cand).map_err(|e| e.to_string())?
            };
            if let Some(o) = oracle {
                o.store(!buggy, Ordering::SeqCst);
            }
            let ctx = GateContext {
                seq: i64::from(n),
                attempt: 1,
                agent: 0,
                base: pre,
                submitted: cand,
                pre,
                candidate: cand,
                changes: &changes,
                claims: &claims,
            };
            let t0 = Instant::now();
            let r = gate.verify(&ctx, &store)?;
            let wall_ms = t0.elapsed().as_millis() as u64;
            let d = &r.decision;
            variants.push(VariantResult {
                task: t.name.clone(),
                bug: bug.map(|b| b.name.clone()),
                category: bug.map(|b| b.category.clone()),
                buggy,
                approved: d.approved,
                reached_review: d.findings.iter().all(|f| f.stage == Stage::Review),
                findings: d.findings.iter().map(|f| f.code.clone()).collect(),
                wall_ms,
            });
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    let _ = std::fs::remove_dir_all(&work);

    let mut gate_m = Matrix::default();
    let mut by_category: BTreeMap<String, Matrix> = BTreeMap::new();
    let mut reviewer_stage = Matrix::default();
    for v in &variants {
        gate_m.add(v.buggy, v.approved);
        if let Some(c) = &v.category {
            by_category
                .entry(c.clone())
                .or_default()
                .add(v.buggy, v.approved);
        }
        if v.reached_review {
            reviewer_stage.add(v.buggy, v.approved);
        }
    }
    let solutions = variants.iter().filter(|v| v.bug.is_none());
    let completed = solutions.clone().filter(|v| v.approved).count() as u32;
    Ok(Report {
        reviewer: gate.reviewer_name().to_string(),
        tasks: corpus.tasks.len(),
        completion_rate: rate(completed, solutions.count() as u32),
        variants,
        gate: gate_m,
        by_category,
        reviewer_stage,
        wall_ms: started.elapsed().as_millis() as u64,
        usage: usage.map(|u| u.lock().unwrap_or_else(|p| p.into_inner()).clone()),
    })
}

/// The report as JSON (with the rates spelled out).
pub fn report_json(r: &Report) -> serde_json::Value {
    json!({
        "report": r,
        "rates": {
            "gate_fn_rate": r.gate.fn_rate(),
            "gate_tp_rate": r.gate.tp_rate(),
            "gate_fp_rate": r.gate.fp_rate(),
            "reviewer_stage_fn_rate": r.reviewer_stage.fn_rate(),
            "by_category_fn_rate": r.by_category.iter()
                .map(|(c, m)| (c.clone(), m.fn_rate()))
                .collect::<BTreeMap<_, _>>(),
        }
    })
}
