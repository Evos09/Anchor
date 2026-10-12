//! Builder + checker (ADR-0039): the app's two-AI loop. The builder (the
//! executor route) changes the project; a reviewer of another model
//! family reads the change and answers claim by claim; while it does not
//! accept the change, its notes go back to the builder, for a few rounds.
//!
//! The reviewer is a review session (ADR-0009): no workspace tools, and
//! the family rule is checked twice, on the roles before it starts and on
//! the providers it actually ran on. Every change the builder makes still
//! goes through the user's approvals as in a one-AI task.

use std::path::PathBuf;
use std::sync::Arc;

use anchor_checkpoint::CheckpointStore;
use anchor_protocol::roles::Route;
use anchor_protocol::verify::{
    ChangedFile, Claim, ClaimVerdict, MAX_CLAIM_TEXT, ReviewInput, Verdict,
};
use serde_json::json;

use crate::inference::keys::SecretEnv;
use crate::mediator::Mediator;
use crate::session::{self, Cancel, SessionConfig, SessionReport};
use anchor_verify::Reviewer;

use crate::verify::ModelReviewer;

/// Most review rounds a task may ask for.
pub const MAX_ROUNDS: u32 = 5;
/// Most diff text quoted back to the builder.
const MAX_FEEDBACK_DIFF: usize = 8000;
/// The snapshot ref holding the task's starting point.
const NS: &str = "pair";
const LEAF: &str = "start";

/// How the checker runs for one task.
#[derive(Debug, Clone)]
pub struct PairSpec {
    /// Reviews per task; 0 runs the builder alone.
    pub rounds: u32,
    pub state_base: PathBuf,
    pub paths: crate::StatePaths,
    /// Secrets for the review sessions (the same plan as the builder's).
    pub env: SecretEnv,
    /// Scripted reviewer models, one per review (the last repeats).
    pub review_mock_scripts: Vec<String>,
    /// Scripted builder models for rounds 2, 3, … (the last repeats).
    pub repair_mock_scripts: Vec<String>,
}

fn pick(scripts: &[String], i: usize) -> Option<String> {
    scripts.get(i.min(scripts.len().saturating_sub(1))).cloned()
}

/// `s` cut to at most `max` bytes, on a character boundary.
fn cut(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// What the reviewer judges: the builder's claims, or one claim standing
/// for the whole change, each tied to the diffs of its files.
pub fn review_input(rep: &SessionReport, changes: Vec<ChangedFile>) -> ReviewInput {
    let paths: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
    let mut claims = rep.claims.clone();
    if claims.is_empty() || anchor_protocol::verify::validate_claims(&claims).is_err() {
        let summary = rep
            .done
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("The change does what the task asks.");
        claims = vec![Claim {
            id: "task".into(),
            text: cut(summary, MAX_CLAIM_TEXT),
            files: paths.iter().take(64).cloned().collect(),
            tests: Vec::new(),
        }];
    }
    let claim_refs = claims
        .iter()
        .map(|c| {
            let own: Vec<String> = paths
                .iter()
                .filter(|p| c.files.is_empty() || c.files.contains(p))
                .map(|p| format!("diff:{p}"))
                .collect();
            (c.id.clone(), own)
        })
        .collect();
    let mut input = ReviewInput {
        claims,
        changes,
        commands: Vec::new(),
        tests: Vec::new(),
        coverage: None,
        claim_refs,
        truncated: false,
    };
    crate::verify::fit(&mut input);
    input
}

/// Whether every claim has a `supported` verdict.
pub fn approved(input: &ReviewInput, verdicts: &[ClaimVerdict]) -> bool {
    !input.claims.is_empty()
        && input.claims.iter().all(|c| {
            verdicts
                .iter()
                .any(|v| v.claim_id == c.id && v.verdict == Verdict::Supported)
        })
}

/// The builder's next instruction: the task, then the reviewer's
/// objections and the diffs they are about.
pub fn feedback(
    task: &str,
    reviewer_family: &str,
    input: &ReviewInput,
    verdicts: &[ClaimVerdict],
) -> String {
    let mut out = format!(
        "{task}\n\nThe reviewer ({reviewer_family}) checked your change and does not accept it yet:\n"
    );
    let mut cited: Vec<String> = Vec::new();
    for c in &input.claims {
        let v = verdicts.iter().find(|v| v.claim_id == c.id);
        match v {
            Some(v) if v.verdict == Verdict::Supported => continue,
            Some(v) => {
                let word = match v.verdict {
                    Verdict::Refuted => "refuted",
                    _ => "could not be checked",
                };
                out.push_str(&format!(
                    "- \"{}\": {word} ({})",
                    cut(&c.text, 300),
                    v.reason_code
                ));
                if !v.note.trim().is_empty() {
                    out.push_str(&format!(": {}", cut(v.note.trim(), 1000)));
                }
                out.push('\n');
                for r in &v.evidence_refs {
                    if let Some(p) = r.strip_prefix("diff:")
                        && !cited.iter().any(|c| c == p)
                    {
                        cited.push(p.to_string());
                    }
                }
            }
            None => out.push_str(&format!(
                "- \"{}\": the reviewer gave no verdict\n",
                cut(&c.text, 300)
            )),
        }
    }
    let mut diff = String::new();
    for f in &input.changes {
        if (cited.is_empty() || cited.contains(&f.path)) && !f.diff.is_empty() {
            diff.push_str(&f.diff);
        }
    }
    if !diff.is_empty() {
        out.push_str("\nYour change so far, as the reviewer saw it:\n");
        out.push_str(&cut(&diff, MAX_FEEDBACK_DIFF));
        if diff.len() > MAX_FEEDBACK_DIFF {
            out.push_str("\n[diff cut short]");
        }
        out.push('\n');
    }
    out.push_str("\nFix these, then finish again.");
    out
}

fn log(m: &Mediator, kind: &str, data: serde_json::Value) {
    let _ = m.log(kind, data);
}

/// Run a task as builder + checker. Session errors end the task (and are
/// logged); `Err` only when the very first session could not start.
pub fn run(
    cfg: &SessionConfig,
    mediator: &mut Mediator,
    checkpoints: &CheckpointStore,
    cancel: &Arc<Cancel>,
    spec: &PairSpec,
) -> Result<(), session::SessionError> {
    if spec.rounds == 0 {
        return session::run_cancellable(cfg, mediator, checkpoints, Some(cancel)).map(drop);
    }
    let start = match mediator.snapshot_ref(NS, LEAF, "before the task (builder + checker)") {
        Ok(id) => id,
        Err(e) => {
            log(
                mediator,
                "pair_error",
                json!({ "detail": format!("cannot snapshot the project: {e}") }),
            );
            return session::run_cancellable(cfg, mediator, checkpoints, Some(cancel)).map(drop);
        }
    };
    let out = rounds(cfg, mediator, checkpoints, cancel, spec, &start);
    mediator.drop_ref(NS, LEAF);
    out
}

fn rounds(
    cfg: &SessionConfig,
    mediator: &mut Mediator,
    checkpoints: &CheckpointStore,
    cancel: &Arc<Cancel>,
    spec: &PairSpec,
    start: &anchor_snapshot::Id,
) -> Result<(), session::SessionError> {
    let rounds = spec.rounds.min(MAX_ROUNDS);
    let identity = crate::team::roles_view(&cfg.roles_toml)
        .and_then(|v| v.identity(Route::Review))
        .map(|i| json!({ "model": i.model_id, "family": i.family }));
    let reviewer_label = identity.clone().unwrap_or(json!(null));
    let family = reviewer_label["family"]
        .as_str()
        .unwrap_or("another family")
        .to_string();
    let mut reviewer: Option<ModelReviewer> = None;
    let mut instruction = cfg.instruction.clone();
    for round in 1..=rounds {
        let mut c = cfg.clone();
        c.instruction = instruction.clone();
        if round > 1 {
            c.session_id = format!("{}-r{round}", cfg.session_id);
            if !spec.repair_mock_scripts.is_empty() {
                c.mock_script = pick(&spec.repair_mock_scripts, round as usize - 2);
            }
            log(
                mediator,
                "pair_reply",
                json!({ "round": round, "rounds": rounds }),
            );
        }
        let rep = match session::run_cancellable(&c, mediator, checkpoints, Some(cancel)) {
            Ok(r) => r,
            Err(e) if round == 1 => return Err(e),
            Err(e) => {
                log(
                    mediator,
                    "pair_error",
                    json!({ "round": round, "detail": format!("the builder could not run: {e}") }),
                );
                return Ok(());
            }
        };
        if rep.failed.is_some() || cancel.is_cancelled() {
            return Ok(());
        }
        let changes = match mediator.changes_since(start) {
            Ok(c) => c,
            Err(e) => {
                log(
                    mediator,
                    "pair_error",
                    json!({ "round": round, "detail": format!("cannot read the change: {e}") }),
                );
                return Ok(());
            }
        };
        if changes.is_empty() {
            log(
                mediator,
                "pair_skipped",
                json!({ "round": round, "reason": "the builder changed no files" }),
            );
            return Ok(());
        }
        let input = review_input(&rep, changes);
        if let Err(e) = input.validate() {
            log(
                mediator,
                "pair_error",
                json!({ "round": round, "detail": format!("the change cannot be reviewed: {e}") }),
            );
            return Ok(());
        }
        if reviewer.is_none() {
            match new_reviewer(cfg, mediator, cancel, spec, &identity) {
                Ok(r) => reviewer = Some(r),
                Err(e) => {
                    log(
                        mediator,
                        "pair_error",
                        json!({ "round": round, "detail": e }),
                    );
                    return Ok(());
                }
            }
        }
        let Some(r) = reviewer.as_mut() else {
            return Ok(());
        };
        if !spec.review_mock_scripts.is_empty() {
            r.set_mock_script(pick(&spec.review_mock_scripts, round as usize - 1));
        }
        let verdicts = match r.review(&input) {
            Ok(v) => v,
            Err(e) => {
                if !cancel.is_cancelled() {
                    log(
                        mediator,
                        "pair_error",
                        json!({ "round": round, "detail": format!("the reviewer could not finish: {e}") }),
                    );
                }
                return Ok(());
            }
        };
        let ok = approved(&input, &verdicts);
        let claims: Vec<_> = input
            .claims
            .iter()
            .map(|c| json!({ "id": c.id, "text": cut(&c.text, 500) }))
            .collect();
        log(
            mediator,
            "pair_review",
            json!({
                "round": round,
                "rounds": rounds,
                "reviewer": reviewer_label,
                "claims": claims,
                "verdicts": verdicts,
                "approved": ok,
                "final": ok || round == rounds,
            }),
        );
        if ok || cancel.is_cancelled() {
            return Ok(());
        }
        instruction = feedback(&cfg.instruction, &family, &input, &verdicts);
    }
    Ok(())
}

fn new_reviewer(
    cfg: &SessionConfig,
    mediator: &Mediator,
    cancel: &Arc<Cancel>,
    spec: &PairSpec,
    identity: &Result<serde_json::Value, String>,
) -> Result<ModelReviewer, String> {
    if let Err(e) = identity {
        return Err(format!("no reviewer is set up: {e}"));
    }
    let builders = crate::team::roles_view(&cfg.roles_toml)?.families(Route::Agentic);
    let (m, cp) = crate::verify::reviewer_mediator(mediator, &spec.state_base, &spec.paths)?;
    Ok(ModelReviewer::new(
        m,
        cp,
        cfg.agent_bin.clone(),
        cfg.roles_toml.clone(),
        pick(&spec.review_mock_scripts, 0),
        builders,
    )?
    .with_env(spec.env.clone())
    .with_cancel(cancel.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            status: anchor_protocol::verify::FileStatus::Modified,
            is_test: false,
            added_lines: vec![1],
            removed_lines: 0,
            added_text: vec!["x".into()],
            diff: format!("--- a/{path}\n+++ b/{path}\n@@ -0,0 +1 @@\n+x\n"),
        }
    }

    fn report(done: &str) -> SessionReport {
        SessionReport {
            done: Some(done.into()),
            ..Default::default()
        }
    }

    fn verdict(id: &str, v: Verdict, note: &str, refs: &[&str]) -> ClaimVerdict {
        ClaimVerdict {
            claim_id: id.into(),
            verdict: v,
            reason_code: "checked".into(),
            evidence_refs: refs.iter().map(|r| r.to_string()).collect(),
            note: note.into(),
        }
    }

    #[test]
    fn a_change_without_claims_gets_one_claim_over_every_file() {
        let input = review_input(
            &report("Added a greeting."),
            vec![change("a.py"), change("b.py")],
        );
        assert_eq!(input.claims.len(), 1);
        assert_eq!(input.claims[0].id, "task");
        assert_eq!(input.claims[0].text, "Added a greeting.");
        assert_eq!(input.claim_refs["task"], vec!["diff:a.py", "diff:b.py"]);
        input.validate().unwrap();
    }

    #[test]
    fn claims_are_tied_to_their_own_files() {
        let mut rep = report("done");
        rep.claims = vec![Claim {
            id: "c1".into(),
            text: "b changed".into(),
            files: vec!["b.py".into()],
            tests: vec![],
        }];
        let input = review_input(&rep, vec![change("a.py"), change("b.py")]);
        assert_eq!(input.claim_refs["c1"], vec!["diff:b.py"]);
    }

    #[test]
    fn only_every_claim_supported_is_approval() {
        let input = review_input(&report("x"), vec![change("a.py")]);
        assert!(approved(
            &input,
            &[verdict("task", Verdict::Supported, "", &[])]
        ));
        assert!(!approved(
            &input,
            &[verdict("task", Verdict::Refuted, "", &[])]
        ));
        assert!(!approved(
            &input,
            &[verdict("task", Verdict::Unverifiable, "", &[])]
        ));
        assert!(!approved(
            &input,
            &[verdict("other", Verdict::Supported, "", &[])]
        ));
        assert!(!approved(&input, &[]));
    }

    #[test]
    fn feedback_carries_the_task_the_notes_and_the_cited_diff() {
        let input = review_input(&report("Greets."), vec![change("a.py"), change("b.py")]);
        let v = [verdict(
            "task",
            Verdict::Refuted,
            "a.py prints the wrong name",
            &["diff:a.py"],
        )];
        let f = feedback("Make it greet Bob.", "llama", &input, &v);
        assert!(f.starts_with("Make it greet Bob."));
        assert!(f.contains("The reviewer (llama) checked your change"));
        assert!(f.contains("refuted (checked): a.py prints the wrong name"));
        assert!(f.contains("+++ b/a.py"));
        assert!(!f.contains("+++ b/b.py"), "only the cited file's diff");
        assert!(f.ends_with("Fix these, then finish again."));
    }
}
