//! The merge queue (spec §7.2, ADR-0007): the only way an isolated agent's
//! work reaches the main workspace. Strictly serialized; every step is a
//! Hub `merge` row and an audit entry.
//!
//! For each submission:
//!
//! 1. **Claim check** (the hard guarantee behind claims): every path that
//!    changed between the agent's base and its submission must lie within
//!    the agent's granted `claim_within`, must have been claimed by the
//!    agent (edit claim, any time during its work), and must not be under
//!    another agent's active edit claim now.
//! 2. **Merge**: `merged = merge3(base, pre, submitted)`, where `pre` is a
//!    fresh snapshot of the main workspace. Path conflicts reject the
//!    submission with structured feedback, sent to the agent as a
//!    `review` message.
//! 3. **Gate**: a [`MergeGate`] may veto the merged tree (the verification
//!    pipeline, ADR-0024). It runs *without* the store lock, so a long test
//!    run never stalls other agents; if the main workspace moves meanwhile,
//!    the merge is recomputed and the new candidate is checked again (an
//!    identical tree is not re-checked).
//! 4. **Apply**: with the user's approval (or `--auto-merge`), the main
//!    workspace is moved to `merged` by an atomic, journaled restore that
//!    refuses if the workspace changed since `pre`; then the merge is
//!    recomputed against the new state.
//!
//! Refs: `merges/<seq>-pre` (main before), `merges/<seq>-cand` (computed,
//! not yet applied), `merges/<seq>` (applied). A revert (§7.3) is the
//! three-way merge `merge3(post, main, pre)`: it undoes exactly that
//! merge's changes and keeps everything since.

use std::path::Path;
use std::sync::Arc;

use anchor_fs::Resolved;
use anchor_hub::ArtifactKind;
use anchor_hub::{
    AgentId, Caller, ClaimPath, ClaimType, Hub, MergeEntry, MergeKind, MergeStatus, MessageType,
    Recipient,
};
use anchor_policy::{Operation, PermissionRequest, Tool, UserAction, UserResponse};
use anchor_protocol::verify::Claim;
use anchor_snapshot::{Change, Id, Store};
use serde_json::{Value, json};

use crate::interact::PromptView;
use crate::mediator::{Mediator, SharedStore, lock_store};

/// Times a merge is recomputed when the main workspace keeps changing
/// under it.
const MAX_ATTEMPTS: usize = 3;

/// What a gate is asked to check: one merge candidate and where it came
/// from.
#[derive(Debug, Clone)]
pub struct GateContext<'a> {
    pub seq: i64,
    /// The recomputation round (1-based; > 1 after the main workspace
    /// moved under the merge).
    pub attempt: usize,
    pub agent: AgentId,
    /// The agent's base and its submission.
    pub base: Id,
    pub submitted: Id,
    /// The main workspace before the merge, and the merged candidate.
    pub pre: Id,
    pub candidate: Id,
    /// The agent's changes (`base` → `submitted`).
    pub changes: &'a [Change],
    /// What the agent claims its change does.
    pub claims: &'a [Claim],
}

/// A check on the merged tree before it is applied. `Ok` carries a short
/// summary shown with the approval prompt and recorded with the merge;
/// `Err` carries structured feedback for the agent.
///
/// The gate is called without the store lock held; it takes the lock only
/// for as long as it reads the store.
pub trait MergeGate: Send {
    fn check(&mut self, ctx: &GateContext<'_>, store: &SharedStore) -> Result<Value, Value>;

    /// Whether this gate verifies anything. Merges through a gate that does
    /// not are audited as `merge_unverified`.
    fn verifies(&self) -> bool {
        true
    }
}

/// No verification (an explicit `--unverified` choice, or tests).
#[derive(Debug, Default)]
pub struct NoGate;

impl MergeGate for NoGate {
    fn check(&mut self, _: &GateContext<'_>, _: &SharedStore) -> Result<Value, Value> {
        Ok(Value::Null)
    }
    fn verifies(&self) -> bool {
        false
    }
}

/// Why a merge was rejected. Only [`RejectKind::Gate`] and
/// [`RejectKind::Conflict`] are worth another attempt by the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectKind {
    /// Paths changed without the right claims.
    Claims,
    /// Path conflicts with the main workspace.
    Conflict,
    /// The gate (verification) refused the candidate.
    Gate,
    /// The main workspace kept changing during the merge.
    Drift,
    /// The user declined.
    Declined,
    /// Applying the merge failed.
    Apply,
}

impl RejectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RejectKind::Claims => "claims",
            RejectKind::Conflict => "conflict",
            RejectKind::Gate => "gate",
            RejectKind::Drift => "drift",
            RejectKind::Declined => "declined",
            RejectKind::Apply => "apply",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Merged {
        seq: i64,
        merged: Id,
    },
    Rejected {
        seq: i64,
        reason: RejectKind,
        feedback: Value,
        /// The main workspace the merge was computed against, and the
        /// computed candidate (`merges/<seq>-cand`), when it got that far.
        pre: Option<Id>,
        candidate: Option<Id>,
    },
}

impl Outcome {
    pub fn seq(&self) -> i64 {
        match self {
            Outcome::Merged { seq, .. } | Outcome::Rejected { seq, .. } => *seq,
        }
    }
    pub fn is_merged(&self) -> bool {
        matches!(self, Outcome::Merged { .. })
    }
    pub fn reject_kind(&self) -> Option<RejectKind> {
        match self {
            Outcome::Rejected { reason, .. } => Some(*reason),
            Outcome::Merged { .. } => None,
        }
    }
}

/// Evidence name of a merge's claims.
fn claims_evidence(seq: i64) -> String {
    format!("merge-{seq:010}/claims.json")
}

pub struct MergeQueue<'m> {
    hub: Arc<Hub>,
    main: &'m mut Mediator,
    gate: Box<dyn MergeGate>,
    auto_merge: bool,
}

impl std::fmt::Debug for MergeQueue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeQueue")
            .field("auto_merge", &self.auto_merge)
            .finish()
    }
}

/// `/`-separated form of a store path (the Hub's claim form).
pub fn slash_path(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn parse_id(s: &str) -> Result<Id, String> {
    Id::parse(s).ok_or_else(|| format!("bad snapshot id {s:?}"))
}

fn user_request(root: &Path, r: &Resolved, op: Operation) -> PermissionRequest {
    PermissionRequest {
        tool: Tool::UserAction,
        operation: op,
        canonical_target: Some(r.canonical.clone()),
        workspace_root: root.to_path_buf(),
        target_exists: r.exists(),
        is_append_only: false,
        blast_radius_count: 1,
        network: false,
    }
}

fn same_tree(store: &Store, a: &Id, b: &Id) -> Result<bool, String> {
    let ra = store.snapshot(a).map_err(|e| e.to_string())?.root;
    let rb = store.snapshot(b).map_err(|e| e.to_string())?.root;
    Ok(ra == rb)
}

impl<'m> MergeQueue<'m> {
    pub fn new(
        hub: Arc<Hub>,
        main: &'m mut Mediator,
        gate: Box<dyn MergeGate>,
        auto_merge: bool,
    ) -> Self {
        MergeQueue {
            hub,
            main,
            gate,
            auto_merge,
        }
    }

    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }

    pub fn main(&mut self) -> &mut Mediator {
        self.main
    }

    fn audit(&self, kind: &str, data: Value) {
        let _ = self.main.engine().log(kind, data);
    }

    /// Queue an agent's submission. Its claims are persisted as evidence
    /// right away, so a merge re-checked after a crash sees them too.
    pub fn submit(
        &mut self,
        agent: AgentId,
        base: &Id,
        submitted: &Id,
        claims: &[Claim],
    ) -> Result<MergeEntry, String> {
        anchor_protocol::verify::validate_claims(claims)?;
        let entry = self
            .hub
            .merge_enqueue(
                Caller::System,
                Some(agent),
                MergeKind::Merge,
                &base.hex(),
                &submitted.hex(),
                None,
            )
            .map_err(|e| e.to_string())?;
        let bytes = serde_json::to_vec(claims).map_err(|e| e.to_string())?;
        self.hub
            .register_evidence(
                Caller::System,
                ArtifactKind::Log,
                &claims_evidence(entry.seq),
                &bytes,
                json!({ "type": "claims", "merge": entry.seq, "agent_id": agent }),
            )
            .map_err(|e| e.to_string())?;
        Ok(entry)
    }

    /// The claims submitted with merge `seq` (none if it has no record).
    pub fn claims_of(&self, seq: i64) -> Result<Vec<Claim>, String> {
        let path = format!("evidence:{}", claims_evidence(seq));
        let found = self
            .hub
            .list_artifacts(Caller::System, Some(&path))
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|a| a.path == path);
        let Some(a) = found else {
            return Ok(Vec::new());
        };
        let bytes = self
            .hub
            .read_evidence(Caller::System, &a)
            .map_err(|e| e.to_string())?;
        serde_json::from_slice(&bytes).map_err(|e| format!("claims of merge {seq}: {e}"))
    }

    /// Settle merges a crash left `applying` (the main workspace itself was
    /// already recovered from its restore journal when it was opened):
    /// finalize those whose result is in place, re-queue the rest. Returns
    /// every queued merge, oldest first.
    pub fn recover(&mut self) -> Result<Vec<i64>, String> {
        let entries = self.hub.merges().map_err(|e| e.to_string())?;
        for e in entries.iter().filter(|e| e.status == MergeStatus::Applying) {
            let store_arc = self.main.shared_store();
            let store = lock_store(&store_arc);
            let now = anchor_snapshot::snapshot(
                &store,
                self.main.workspace(),
                self.main.snapshot_options(),
                "merge recovery",
            )
            .map_err(|e| e.to_string())?
            .id();
            let done = match e.merged.as_deref() {
                Some(m) => same_tree(&store, &now, &parse_id(m)?)?,
                None => false,
            };
            if done {
                let merged = parse_id(e.merged.as_deref().unwrap_or_default())?;
                store
                    .set_ref(&format!("merges/{:010}", e.seq), &merged)
                    .map_err(|e| e.to_string())?;
                let _ = store.delete_ref(&format!("merges/{:010}-cand", e.seq));
                drop(store);
                self.set_status(e.seq, MergeStatus::Merged, None, json!({"recovered": true}))?;
            } else {
                drop(store);
                self.set_status(e.seq, MergeStatus::Queued, None, json!({"recovered": true}))?;
            }
            self.audit(
                "merge_recovered",
                json!({ "seq": e.seq, "finalized": done }),
            );
        }
        Ok(self
            .hub
            .merges()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|e| e.status == MergeStatus::Queued)
            .map(|e| e.seq)
            .collect())
    }

    fn set_status(
        &self,
        seq: i64,
        status: MergeStatus,
        ids: Option<(&Id, &Id)>,
        feedback: Value,
    ) -> Result<MergeEntry, String> {
        let pre = ids.map(|(p, _)| p.hex());
        let merged = ids.map(|(_, m)| m.hex());
        self.hub
            .merge_update(
                Caller::System,
                seq,
                status,
                pre.as_deref(),
                merged.as_deref(),
                feedback,
            )
            .map_err(|e| e.to_string())
    }

    /// Reject `entry`. `detail` (an object) becomes the feedback, with
    /// `reason` set from `kind`.
    fn reject(
        &mut self,
        entry: &MergeEntry,
        kind: RejectKind,
        detail: Value,
        ids: Option<(Id, Id)>,
    ) -> Result<Outcome, String> {
        let mut feedback = match detail {
            Value::Object(m) => m,
            other => {
                let mut m = serde_json::Map::new();
                m.insert("detail".into(), other);
                m
            }
        };
        feedback.insert("reason".into(), json!(kind.as_str()));
        let feedback = Value::Object(feedback);
        self.set_status(entry.seq, MergeStatus::Rejected, None, feedback.clone())?;
        self.audit(
            "merge_rejected",
            json!({ "seq": entry.seq, "agent_id": entry.agent_id, "feedback": feedback }),
        );
        if let Some(agent) = entry.agent_id {
            // The agent learns why through the bus, like any other review.
            let _ = self.hub.send_message(
                Caller::System,
                Recipient::Agent(agent),
                MessageType::Review,
                json!({ "merge": entry.seq, "status": "rejected", "feedback": feedback }),
            );
        }
        let (pre, candidate) = ids.map_or((None, None), |(p, c)| (Some(p), Some(c)));
        Ok(Outcome::Rejected {
            seq: entry.seq,
            reason: kind,
            feedback,
            pre,
            candidate,
        })
    }

    /// Paths of `changes` the agent was not entitled to change.
    fn claim_violations(&self, agent: AgentId, changes: &[Change]) -> Result<Vec<Value>, String> {
        let me = self.hub.agent(agent).map_err(|e| e.to_string())?;
        // A retired agent's authority is withdrawn, its work included
        // (review finding H-2).
        if me.status == anchor_hub::AgentStatus::Retired {
            return Ok(changes
                .iter()
                .map(|c| json!({ "path": slash_path(&c.path), "reason": "the agent was retired" }))
                .collect());
        }
        let within: Vec<ClaimPath> = me
            .permissions
            .claim_within
            .iter()
            .filter_map(|w| ClaimPath::parse(w).ok())
            .collect();
        let held: Vec<ClaimPath> = self
            .hub
            .claim_history(agent)
            .map_err(|e| e.to_string())?
            .into_iter()
            // Released and expired claims still cover the work done under
            // them; a claim the user revoked does not.
            .filter(|c| {
                c.claim_type == ClaimType::Edit && c.status != anchor_hub::ClaimStatus::Revoked
            })
            .filter_map(|c| ClaimPath::parse(&c.path).ok())
            .collect();
        let mut bad = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for c in changes {
            let p = slash_path(&c.path);
            if !seen.insert(p.clone()) {
                continue;
            }
            // Git executes its internals (hooks, config): never merged,
            // whatever the claims (review finding T-1).
            if p.split('/').any(|seg| seg.eq_ignore_ascii_case(".git")) {
                bad.push(json!({ "path": p, "reason": "git internals are never merged" }));
                continue;
            }
            let Ok(target) = ClaimPath::file(&p) else {
                bad.push(json!({ "path": p, "reason": "not a claimable path" }));
                continue;
            };
            let reason = if !within.iter().any(|w| w.covers(&target)) {
                Some("outside the paths granted to this agent".to_string())
            } else if !held.iter().any(|h| h.covers(&target)) {
                Some("changed without an edit claim".to_string())
            } else {
                match self.hub.edit_blocker(agent, &p) {
                    Ok(Some(c)) => Some(format!("claimed by agent {} ({})", c.agent_id, c.path)),
                    Ok(None) => None,
                    Err(e) => Some(e.to_string()),
                }
            };
            if let Some(r) = reason {
                bad.push(json!({ "path": p, "reason": r }));
            }
        }
        Ok(bad)
    }

    /// Process one queued merge to `merged` or `rejected`.
    pub fn process(&mut self, seq: i64) -> Result<Outcome, String> {
        let entry = self
            .hub
            .merges()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|e| e.seq == seq)
            .ok_or_else(|| format!("no merge {seq}"))?;
        if entry.status != MergeStatus::Queued || entry.kind != MergeKind::Merge {
            return Err(format!("merge {seq} is not a queued merge"));
        }
        let agent = entry.agent_id.ok_or("merge without an agent")?;
        let base = parse_id(&entry.base)?;
        let submitted = parse_id(&entry.submitted)?;
        let store_arc = self.main.shared_store();

        let changes = {
            let store = lock_store(&store_arc);
            anchor_snapshot::diff_files(&store, &base, &submitted).map_err(|e| e.to_string())?
        };
        let violations = self.claim_violations(agent, &changes)?;
        if !violations.is_empty() {
            return self.reject(
                &entry,
                RejectKind::Claims,
                json!({ "paths": violations }),
                None,
            );
        }
        if changes.is_empty() {
            self.set_status(seq, MergeStatus::Merged, None, json!({ "empty": true }))?;
            self.audit("merge_applied", json!({ "seq": seq, "empty": true }));
            return Ok(Outcome::Merged { seq, merged: base });
        }
        let claims = self.claims_of(seq)?;
        let ws =
            anchor_fs::Workspace::open(self.main.workspace().root()).map_err(|e| e.to_string())?;
        let opts = self.main.snapshot_options().clone();
        // Gate verdicts by candidate tree: a recomputation that yields the
        // same tree needs no new check.
        let mut verified: Vec<(Id, Value)> = Vec::new();

        for attempt in 1..=MAX_ATTEMPTS {
            let store = lock_store(&store_arc);
            let pre = anchor_snapshot::take_snapshot(
                &store,
                &ws,
                &opts,
                &format!("merges/{seq:010}-pre"),
                &format!("before merge {seq}"),
            )
            .map_err(|e| e.to_string())?
            .id();
            let m = anchor_snapshot::merge3(
                &store,
                &base,
                &pre,
                &submitted,
                &format!("merge {seq} (agent {agent})"),
            )
            .map_err(|e| e.to_string())?;
            let cand = m.snapshot;
            store
                .set_ref(&format!("merges/{seq:010}-cand"), &cand)
                .map_err(|e| e.to_string())?;
            let tree = store.snapshot(&cand).map_err(|e| e.to_string())?.root;
            drop(store);
            if !m.conflicts.is_empty() {
                let paths: Vec<String> = m.conflicts.iter().map(|p| slash_path(p)).collect();
                return self.reject(
                    &entry,
                    RejectKind::Conflict,
                    json!({ "paths": paths, "base": base.hex(), "main": pre.hex() }),
                    Some((pre, cand)),
                );
            }

            let cached = verified
                .iter()
                .find(|(t, _)| *t == tree)
                .map(|(_, v)| v.clone());
            let verdict = match cached {
                Some(v) => v,
                None => {
                    let ctx = GateContext {
                        seq,
                        attempt,
                        agent,
                        base,
                        submitted,
                        pre,
                        candidate: cand,
                        changes: &changes,
                        claims: &claims,
                    };
                    match self.gate.check(&ctx, &store_arc) {
                        Ok(v) => {
                            self.audit(
                                "merge_verified",
                                json!({
                                    "seq": seq,
                                    "attempt": attempt,
                                    "candidate": cand.hex(),
                                    "verified": self.gate.verifies(),
                                    "summary": v,
                                }),
                            );
                            verified.push((tree, v.clone()));
                            v
                        }
                        Err(fb) => {
                            return self.reject(
                                &entry,
                                RejectKind::Gate,
                                json!({ "detail": fb, "candidate": cand.hex() }),
                                Some((pre, cand)),
                            );
                        }
                    }
                }
            };

            let summary: Vec<String> = {
                let store = lock_store(&store_arc);
                anchor_snapshot::diff(&store, &pre, &cand)
                    .map_err(|e| e.to_string())?
                    .iter()
                    .map(|c| c.to_string())
                    .collect()
            };
            if !self.auto_merge {
                let verdict_line = if self.gate.verifies() {
                    format!("  Verification: {verdict}\n")
                } else {
                    "  Verification: NONE (unverified merge)\n".to_string()
                };
                let view = PromptView {
                    agent: self.hub.agent(agent).ok().map(|a| a.display_name),
                    merge_seq: Some(seq),
                    tool: Tool::UserAction,
                    operation: Operation::Write,
                    canonical_target: Some(ws.root().to_path_buf()),
                    rule: "merge_queue".into(),
                    detail: Some(format!(
                        "  Merge {seq} from agent {agent} changes:\n{}{verdict_line}",
                        summary
                            .iter()
                            .map(|l| format!("    {l}\n"))
                            .collect::<String>()
                    )),
                };
                if self.main.interact().confirm(&view) != UserResponse::Approve {
                    return self.reject(
                        &entry,
                        RejectKind::Declined,
                        json!({ "detail": "declined by the user" }),
                        Some((pre, cand)),
                    );
                }
            }
            // Only now is the merge in flight: a crash from here is settled
            // by `recover`.
            self.set_status(seq, MergeStatus::Applying, Some((&pre, &cand)), json!({}))?;
            let store = lock_store(&store_arc);
            let engine = self.main.engine();
            let root = ws.root().to_path_buf();
            let mut auth = |r: &Resolved, op: Operation| {
                engine
                    .user_initiated(UserAction::Merge, &user_request(&root, r, op))
                    .map_err(|d| d.rule)
            };
            match anchor_snapshot::restore_expect(&store, &ws, &opts, &pre, &cand, &mut auth) {
                Ok(r) => {
                    let post = r.post.unwrap_or(cand);
                    store
                        .set_ref(&format!("merges/{seq:010}"), &post)
                        .map_err(|e| e.to_string())?;
                    let _ = store.delete_ref(&format!("merges/{seq:010}-cand"));
                    drop(store);
                    self.set_status(
                        seq,
                        MergeStatus::Merged,
                        Some((&pre, &post)),
                        json!({ "changes": summary, "verification": verdict }),
                    )?;
                    if !self.gate.verifies() {
                        self.audit("merge_unverified", json!({ "seq": seq, "agent_id": agent }));
                    }
                    self.audit(
                        "merge_applied",
                        json!({
                            "seq": seq,
                            "agent_id": agent,
                            "pre": pre.hex(),
                            "candidate": cand.hex(),
                            "merged": post.hex(),
                            "operations": r.ops.len(),
                            "attempt": attempt,
                        }),
                    );
                    return Ok(Outcome::Merged { seq, merged: post });
                }
                Err(anchor_snapshot::Error::ConcurrentChange(p)) => {
                    drop(store);
                    // The main workspace moved since `pre`: nothing was
                    // touched; recompute (and re-check) against its new
                    // state.
                    self.set_status(
                        seq,
                        MergeStatus::Queued,
                        None,
                        json!({ "retry": attempt, "changed": p }),
                    )?;
                    self.audit(
                        "merge_retry",
                        json!({ "seq": seq, "attempt": attempt, "changed": p }),
                    );
                }
                Err(e) => {
                    drop(store);
                    return self.reject(
                        &entry,
                        RejectKind::Apply,
                        json!({ "detail": e.to_string() }),
                        Some((pre, cand)),
                    );
                }
            }
        }
        self.reject(
            &entry,
            RejectKind::Drift,
            json!({ "detail": "the main workspace kept changing during the merge" }),
            None,
        )
    }

    /// Undo merge `seq` (spec §7.3), keeping every later change. Refuses
    /// (and changes nothing) if later changes touched the same paths.
    pub fn revert(&mut self, seq: i64) -> Result<MergeEntry, String> {
        let entry = self
            .hub
            .merges()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|e| e.seq == seq)
            .ok_or_else(|| format!("no merge {seq}"))?;
        if entry.kind != MergeKind::Merge || entry.status != MergeStatus::Merged {
            return Err(format!("merge {seq} is not an applied merge"));
        }
        let (Some(pre_k), Some(post_k)) = (entry.pre.as_deref(), entry.merged.as_deref()) else {
            return Err(format!("merge {seq} changed nothing"));
        };
        let (pre_k, post_k) = (parse_id(pre_k)?, parse_id(post_k)?);
        let store_arc = self.main.shared_store();
        let ws =
            anchor_fs::Workspace::open(self.main.workspace().root()).map_err(|e| e.to_string())?;
        let opts = self.main.snapshot_options().clone();
        let store = lock_store(&store_arc);
        let now = anchor_snapshot::take_snapshot(
            &store,
            &ws,
            &opts,
            &format!("merges/{seq:010}-revert-pre"),
            &format!("before reverting merge {seq}"),
        )
        .map_err(|e| e.to_string())?
        .id();
        let m = anchor_snapshot::merge3(
            &store,
            &post_k,
            &now,
            &pre_k,
            &format!("revert of merge {seq}"),
        )
        .map_err(|e| e.to_string())?;
        if !m.conflicts.is_empty() {
            let _ = store.delete_ref(&format!("merges/{seq:010}-revert-pre"));
            let paths: Vec<String> = m.conflicts.iter().map(|p| slash_path(p)).collect();
            return Err(format!(
                "later changes touch the same paths; nothing was reverted: {}",
                paths.join(", ")
            ));
        }
        store
            .set_ref(&format!("merges/{seq:010}-revert"), &m.snapshot)
            .map_err(|e| e.to_string())?;
        let row = self
            .hub
            .merge_enqueue(
                Caller::User,
                entry.agent_id,
                MergeKind::Revert,
                &post_k.hex(),
                &pre_k.hex(),
                Some(seq),
            )
            .map_err(|e| e.to_string())?;
        let engine = self.main.engine();
        let root = ws.root().to_path_buf();
        let mut auth = |r: &Resolved, op: Operation| {
            engine
                .user_initiated(UserAction::RevertMerge, &user_request(&root, r, op))
                .map_err(|d| d.rule)
        };
        let result =
            anchor_snapshot::restore_expect(&store, &ws, &opts, &now, &m.snapshot, &mut auth);
        drop(store);
        match result {
            Ok(r) => {
                let post = r.post.unwrap_or(m.snapshot);
                let row = self
                    .hub
                    .merge_update(
                        Caller::User,
                        row.seq,
                        MergeStatus::Merged,
                        Some(&now.hex()),
                        Some(&post.hex()),
                        json!({ "reverts": seq }),
                    )
                    .map_err(|e| e.to_string())?;
                self.audit(
                    "merge_reverted",
                    json!({ "seq": seq, "revert_seq": row.seq, "operations": r.ops.len() }),
                );
                Ok(row)
            }
            Err(e) => {
                let _ = self.hub.merge_update(
                    Caller::User,
                    row.seq,
                    MergeStatus::Rejected,
                    None,
                    None,
                    json!({ "error": e.to_string() }),
                );
                Err(e.to_string())
            }
        }
    }

    /// The latest applied merge of `agent` (for `queue revert --agent`).
    pub fn latest_merge_of(&self, agent: AgentId) -> Result<Option<i64>, String> {
        Ok(self
            .hub
            .merges()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|e| {
                e.agent_id == Some(agent)
                    && e.kind == MergeKind::Merge
                    && e.status == MergeStatus::Merged
                    && e.pre.is_some()
            })
            .map(|e| e.seq)
            .max())
    }
}
