//! The snapshot timeline (§12.2): every named snapshot, which agent's turn
//! (or which merge) produced it, and what changed since the snapshot
//! before it on the same root.

use std::collections::BTreeMap;
use std::path::Path;

use anchor_snapshot::{ChangeKind, Id, Store};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    pub path: String,
    pub kind: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapRow {
    pub r#ref: String,
    pub id: String,
    pub label: String,
    pub created_ms: u64,
    pub files: u64,
    pub bytes: u64,
    /// `main`, or the isolated root's name (`a<agent id>`).
    pub root: String,
    /// `turn`, `merge`, `merge-pre`, `merge-candidate`, `head`, `iso`,
    /// `other`.
    pub kind: &'static str,
    pub agent: Option<String>,
    pub session_id: Option<String>,
    pub turn: Option<u64>,
    pub merge_seq: Option<i64>,
    /// The snapshot this one is compared with.
    pub previous: Option<String>,
    pub changes: Vec<FileChange>,
    /// Only main-workspace snapshots can be restored from the dashboard.
    pub restorable: bool,
}

fn kind_str(k: ChangeKind) -> &'static str {
    match k {
        ChangeKind::Added => "added",
        ChangeKind::Removed => "removed",
        ChangeKind::Modified => "modified",
        ChangeKind::ModeChanged => "mode",
    }
}

fn path_str(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Classify a ref name: `(root, kind, merge seq)`.
fn classify(name: &str) -> (String, &'static str, Option<i64>) {
    let (root, leaf) = match name.strip_prefix("r-").and_then(|r| r.split_once('/')) {
        Some((root, leaf)) => (root.to_string(), leaf.replacen('-', "/", 1)),
        None => ("main".to_string(), name.to_string()),
    };
    let seq = |s: &str| s.trim_start_matches('0').parse::<i64>().ok().or(Some(0));
    let kind = if leaf == "HEAD" || leaf == "head" {
        ("head", None)
    } else if leaf.starts_with("turns/") {
        ("turn", None)
    } else if let Some(m) = leaf.strip_prefix("merges/") {
        if let Some(n) = m.strip_suffix("-pre") {
            ("merge-pre", seq(n))
        } else if let Some(n) = m.strip_suffix("-cand") {
            ("merge-candidate", seq(n))
        } else {
            ("merge", seq(m))
        }
    } else if leaf.starts_with("iso/") {
        ("iso", None)
    } else {
        ("other", None)
    };
    (root, kind.0, kind.1)
}

/// `session <id> turn <n>`.
fn parse_label(label: &str) -> Option<(String, u64)> {
    let rest = label.strip_prefix("session ")?;
    let (session, turn) = rest.rsplit_once(" turn ")?;
    Some((session.to_string(), turn.parse().ok()?))
}

/// The timeline, oldest first. `names`: agent id → display name;
/// `merge_agents`: merge seq → the agent whose work it integrates.
/// `diff_cache` keeps computed change lists (snapshots are immutable).
pub fn timeline(
    store: &Store,
    names: &BTreeMap<i64, String>,
    merge_agents: &BTreeMap<i64, String>,
    diff_cache: &mut BTreeMap<(Id, Id), Vec<FileChange>>,
) -> Result<Vec<SnapRow>, String> {
    let mut rows = Vec::new();
    for (name, id) in store.all_refs().map_err(|e| e.to_string())? {
        let Ok(snap) = store.snapshot(&id) else {
            continue;
        };
        let (root, kind, merge_seq) = classify(&name);
        let agent = root
            .strip_prefix('a')
            .and_then(|n| n.parse::<i64>().ok())
            .map(|n| {
                names
                    .get(&n)
                    .cloned()
                    .unwrap_or_else(|| format!("agent #{n}"))
            })
            .or_else(|| merge_seq.and_then(|m| merge_agents.get(&m).cloned()));
        let (session_id, turn) =
            parse_label(&snap.label).map_or((None, None), |(s, t)| (Some(s), Some(t)));
        rows.push((
            id,
            SnapRow {
                restorable: root == "main" && kind != "head",
                r#ref: name,
                id: id.hex(),
                label: snap.label,
                created_ms: snap.created_unix_ms,
                files: snap.files,
                bytes: snap.bytes,
                root,
                kind,
                agent,
                session_id,
                turn,
                merge_seq,
                previous: None,
                changes: Vec::new(),
            },
        ));
    }
    rows.sort_by(|a, b| (a.1.created_ms, &a.1.r#ref).cmp(&(b.1.created_ms, &b.1.r#ref)));
    // Compare each with the previous snapshot of the same root.
    let mut last: BTreeMap<String, (Id, String)> = BTreeMap::new();
    for (id, row) in rows.iter_mut() {
        if let Some((prev, prev_ref)) = last.get(&row.root) {
            row.previous = Some(prev_ref.clone());
            let key = (*prev, *id);
            if let std::collections::btree_map::Entry::Vacant(slot) = diff_cache.entry(key) {
                let changes = anchor_snapshot::diff(store, prev, id)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|c| FileChange {
                        path: path_str(&c.path),
                        kind: kind_str(c.kind),
                    })
                    .collect();
                slot.insert(changes);
            }
            row.changes = diff_cache[&key].clone();
        }
        last.insert(row.root.clone(), (*id, row.r#ref.clone()));
    }
    Ok(rows.into_iter().map(|(_, r)| r).collect())
}

/// A unified diff of `path` between two snapshots (text files only).
pub fn file_diff(store: &Store, a: &Id, b: &Id, path: &str) -> Result<String, String> {
    use anchor_snapshot::textdiff::{TextDiff, file_diff, render};
    let p = Path::new(path);
    match file_diff(store, a, b, p).map_err(|e| e.to_string())? {
        TextDiff::Text(h) => Ok(render(path, &h)),
        TextDiff::Binary => Ok(format!("{path}: binary file changed")),
        TextDiff::TooLarge => Ok(format!("{path}: too large to show")),
    }
}

/// Resolve a ref name or a full id.
pub fn resolve(store: &Store, r: &str) -> Result<Id, String> {
    if let Some(id) = Id::parse(r) {
        return Ok(id);
    }
    store
        .get_ref(r)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no snapshot {r:?}"))
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn refs_are_classified() {
        assert_eq!(classify("turns/0000000003"), ("main".into(), "turn", None));
        assert_eq!(
            classify("merges/0000000004-pre"),
            ("main".into(), "merge-pre", Some(4))
        );
        assert_eq!(
            classify("merges/0000000004"),
            ("main".into(), "merge", Some(4))
        );
        assert_eq!(
            classify("r-a2/turns-0000000001"),
            ("a2".into(), "turn", None)
        );
        assert_eq!(classify("HEAD"), ("main".into(), "head", None));
    }
}
