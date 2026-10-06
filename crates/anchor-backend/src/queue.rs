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
//! 3. **Gate**: a [`MergeGate`] may veto the merged tree (Phase 5 plugs
//!    verification in here).
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
use anchor_hub::{
    AgentId, Caller, ClaimPath, ClaimType, Hub, MergeEntry, MergeKind, MergeStatus, MessageType,
    Recipient,
};
use anchor_policy::{Operation, PermissionRequest, Tool, UserAction, UserResponse};
use anchor_snapshot::{Change, Id, Store};
use serde_json::{Value, json};

use crate::interact::PromptView;
use crate::mediator::{Mediator, lock_store};

/// Times a merge is recomputed when the main workspace keeps changing
/// under it.
const MAX_ATTEMPTS: usize = 3;

/// A check on the merged tree before it is applied. `Err` carries
/// structured feedback for the agent.
pub trait MergeGate: Send {
    fn check(&mut self, store: &Store, merged: &Id, changes: &[Change]) -> Result<(), Value>;
}

/// Phase 4 ships no verification gate.
#[derive(Debug, Default)]
pub struct NoGate;

impl MergeGate for NoGate {
    fn check(&mut self, _: &Store, _: &Id, _: &[Change]) -> Result<(), Value> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Merged { seq: i64, merged: Id },
    Rejected { seq: i64, feedback: Value },
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

    /// Queue an agent's submission.
    pub fn submit(
        &mut self,
        agent: AgentId,
        base: &Id,
        submitted: &Id,
    ) -> Result<MergeEntry, String> {
        self.hub
            .merge_enqueue(
                Caller::System,
                Some(agent),
                MergeKind::Merge,
                &base.hex(),
                &submitted.hex(),
                None,
            )
            .map_err(|e| e.to_string())
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

    fn reject(&mut self, entry: &MergeEntry, feedback: Value) -> Result<Outcome, String> {
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
        Ok(Outcome::Rejected {
            seq: entry.seq,
            feedback,
        })
    }

    /// Paths of `changes` the agent was not entitled to change.
    fn claim_violations(&self, agent: AgentId, changes: &[Change]) -> Result<Vec<Value>, String> {
        let me = self.hub.agent(agent).map_err(|e| e.to_string())?;
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
            .filter(|c| c.claim_type == ClaimType::Edit)
            .filter_map(|c| ClaimPath::parse(&c.path).ok())
            .collect();
        let mut bad = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for c in changes {
            let p = slash_path(&c.path);
            if !seen.insert(p.clone()) {
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
            return self.reject(&entry, json!({ "reason": "claims", "paths": violations }));
        }
        if changes.is_empty() {
            self.set_status(seq, MergeStatus::Merged, None, json!({ "empty": true }))?;
            self.audit("merge_applied", json!({ "seq": seq, "empty": true }));
            return Ok(Outcome::Merged { seq, merged: base });
        }

        for attempt in 1..=MAX_ATTEMPTS {
            let store = lock_store(&store_arc);
            let ws = anchor_fs::Workspace::open(self.main.workspace().root())
                .map_err(|e| e.to_string())?;
            let opts = self.main.snapshot_options().clone();
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
            store
                .set_ref(&format!("merges/{seq:010}-cand"), &m.snapshot)
                .map_err(|e| e.to_string())?;
            if !m.conflicts.is_empty() {
                drop(store);
                let paths: Vec<String> = m.conflicts.iter().map(|p| slash_path(p)).collect();
                return self.reject(
                    &entry,
                    json!({
                        "reason": "conflict",
                        "paths": paths,
                        "base": base.hex(),
                        "main": pre.hex(),
                    }),
                );
            }
            if let Err(fb) = self.gate.check(&store, &m.snapshot, &changes) {
                drop(store);
                return self.reject(&entry, json!({ "reason": "gate", "detail": fb }));
            }
            let summary: Vec<String> = anchor_snapshot::diff(&store, &pre, &m.snapshot)
                .map_err(|e| e.to_string())?
                .iter()
                .map(|c| c.to_string())
                .collect();
            drop(store);
            self.set_status(
                seq,
                MergeStatus::Applying,
                Some((&pre, &m.snapshot)),
                json!({}),
            )?;
            if !self.auto_merge {
                let view = PromptView {
                    tool: Tool::UserAction,
                    operation: Operation::Write,
                    canonical_target: Some(ws.root().to_path_buf()),
                    rule: "merge_queue".into(),
                    detail: Some(format!(
                        "  Merge {seq} from agent {agent} changes:\n{}",
                        summary
                            .iter()
                            .map(|l| format!("    {l}\n"))
                            .collect::<String>()
                    )),
                };
                if self.main.interact().confirm(&view) != UserResponse::Approve {
                    return self.reject(&entry, json!({ "reason": "declined by the user" }));
                }
            }
            let store = lock_store(&store_arc);
            let engine = self.main.engine();
            let root = ws.root().to_path_buf();
            let mut auth = |r: &Resolved, op: Operation| {
                engine
                    .user_initiated(UserAction::Merge, &user_request(&root, r, op))
                    .map_err(|d| d.rule)
            };
            match anchor_snapshot::restore_expect(&store, &ws, &opts, &pre, &m.snapshot, &mut auth)
            {
                Ok(r) => {
                    let post = r.post.unwrap_or(m.snapshot);
                    store
                        .set_ref(&format!("merges/{seq:010}"), &post)
                        .map_err(|e| e.to_string())?;
                    let _ = store.delete_ref(&format!("merges/{seq:010}-cand"));
                    drop(store);
                    self.set_status(
                        seq,
                        MergeStatus::Merged,
                        Some((&pre, &post)),
                        json!({ "changes": summary }),
                    )?;
                    self.audit(
                        "merge_applied",
                        json!({
                            "seq": seq,
                            "agent_id": agent,
                            "pre": pre.hex(),
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
                    // touched; recompute against its new state.
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
                        json!({ "reason": "apply failed", "detail": e.to_string() }),
                    );
                }
            }
        }
        self.reject(
            &entry,
            json!({ "reason": "the main workspace kept changing during the merge" }),
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
