//! Line diffs between snapshots (Phase 5 evidence, ADR-0024): Myers'
//! O(ND) algorithm producing unified hunks. Line numbers are 1-based and
//! count `\n`-terminated lines, the numbering Python's `trace` coverage
//! uses, so changed lines can be matched against coverage.
//!
//! Built here rather than taken from a crate (spec §0): it is small, and
//! its output feeds a merge decision.

use std::fmt::Write as _;
use std::path::{Component, Path};

use crate::object::Node;
use crate::{Error, Id, Result, Store};

/// Files larger than this (either side) are not diffed line by line.
pub const MAX_DIFF_BYTES: usize = 4 << 20;
/// Edit distances beyond this are reported as a whole-file replacement.
const MAX_EDIT_DISTANCE: usize = 20_000;
/// Frontier cells kept for backtracking (8 bytes each) before the diff
/// falls back to a whole-file replacement: bounds memory at 64 MiB
/// whatever the file sizes (review finding P-4).
const MAX_TRACE_CELLS: usize = 8 << 20;
/// Context lines around each change.
pub const CONTEXT: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Removed,
    Added,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    /// The line without its `\n` (a `\r` before it is kept).
    pub text: String,
    /// The last line of a file that does not end with `\n`.
    pub no_eol: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub lines: Vec<Line>,
}

impl Hunk {
    /// 1-based line numbers, on the new side, of the lines this hunk adds.
    pub fn added_lines(&self) -> Vec<usize> {
        let mut n = self.new_start;
        let mut out = Vec::new();
        for l in &self.lines {
            match l.kind {
                LineKind::Context => n += 1,
                LineKind::Added => {
                    out.push(n);
                    n += 1;
                }
                LineKind::Removed => {}
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextDiff {
    /// Line hunks; empty when the contents are equal.
    Text(Vec<Hunk>),
    /// One side has a NUL in its first 8 KiB.
    Binary,
    /// One side exceeds [`MAX_DIFF_BYTES`].
    TooLarge,
}

/// Split into lines. Each line carries whether it lacks a final `\n`
/// (only ever the last one), so a changed final newline is a changed line.
fn lines(b: &[u8]) -> Vec<(&[u8], bool)> {
    if b.is_empty() {
        return Vec::new();
    }
    let mut v: Vec<(&[u8], bool)> = b.split(|&c| c == b'\n').map(|l| (l, false)).collect();
    if b.last() == Some(&b'\n') {
        v.pop(); // the empty piece after the final newline
    } else if let Some(last) = v.last_mut() {
        last.1 = true;
    }
    v
}

fn is_binary(b: &[u8]) -> bool {
    b[..b.len().min(8192)].contains(&0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal,
    Delete,
    Insert,
}

/// Myers' greedy shortest edit script, as a sequence of operations.
fn edit_script(a: &[(&[u8], bool)], b: &[(&[u8], bool)]) -> Vec<Op> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    let off = max as isize + 1;
    let mut v = vec![0isize; 2 * max + 3];
    // trace[d] holds the frontier before round d, diagonals -d..=d only
    // (the rest is never read), so memory grows with D², not D·(N+M).
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut cells = 0usize;
    let mut found = None;
    'outer: for d in 0..=(max as isize) {
        cells += 2 * d as usize + 1;
        if d as usize > MAX_EDIT_DISTANCE || cells > MAX_TRACE_CELLS {
            break;
        }
        trace.push(v[(off - d) as usize..=(off + d) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let i = (k + off) as usize;
            let mut x = if k == -d || (k != d && v[i - 1] < v[i + 1]) {
                v[i + 1]
            } else {
                v[i - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[i] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let Some(dmax) = found else {
        // Too different: delete everything, insert everything.
        let mut ops = vec![Op::Delete; a.len()];
        ops.extend(std::iter::repeat_n(Op::Insert, b.len()));
        return ops;
    };
    // Backtrack through the saved frontiers.
    let mut ops = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (1..=dmax).rev() {
        let t = &trace[d as usize];
        let v = |k: isize| t[(k + d) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && v(k - 1) < v(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = v(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Equal);
            x -= 1;
            y -= 1;
        }
        if x == prev_x {
            ops.push(Op::Insert);
            y -= 1;
        } else {
            ops.push(Op::Delete);
            x -= 1;
        }
    }
    while x > 0 && y > 0 {
        ops.push(Op::Equal);
        x -= 1;
        y -= 1;
    }
    ops.reverse();
    ops
}

/// Diff two file contents into unified hunks with `context` lines.
pub fn diff_text(old: &[u8], new: &[u8], context: usize) -> TextDiff {
    if old.len() > MAX_DIFF_BYTES || new.len() > MAX_DIFF_BYTES {
        return TextDiff::TooLarge;
    }
    if is_binary(old) || is_binary(new) {
        return TextDiff::Binary;
    }
    let a = lines(old);
    let b = lines(new);
    let ops = edit_script(&a, &b);
    let text = |l: &[u8]| String::from_utf8_lossy(l).into_owned();

    // Walk the script with line positions, then group into hunks.
    struct Row {
        op: Op,
        ai: usize,
        bi: usize,
    }
    let mut rows = Vec::with_capacity(ops.len());
    let (mut ai, mut bi) = (0usize, 0usize);
    for op in ops {
        rows.push(Row { op, ai, bi });
        match op {
            Op::Equal => {
                ai += 1;
                bi += 1;
            }
            Op::Delete => ai += 1,
            Op::Insert => bi += 1,
        }
    }
    let changed: Vec<usize> = (0..rows.len())
        .filter(|&r| rows[r].op != Op::Equal)
        .collect();
    let mut hunks = Vec::new();
    let mut g = 0;
    while g < changed.len() {
        let start = changed[g].saturating_sub(context);
        let mut end = changed[g];
        let mut h = g;
        while h + 1 < changed.len() && changed[h + 1] <= end + 2 * context + 1 {
            h += 1;
            end = changed[h];
        }
        let end = (end + context).min(rows.len() - 1);
        let first = &rows[start];
        let mut hunk = Hunk {
            old_start: first.ai + 1,
            old_lines: 0,
            new_start: first.bi + 1,
            new_lines: 0,
            lines: Vec::new(),
        };
        for r in &rows[start..=end] {
            let (kind, t, no_eol) = match r.op {
                Op::Equal => (LineKind::Context, text(a[r.ai].0), a[r.ai].1),
                Op::Delete => (LineKind::Removed, text(a[r.ai].0), a[r.ai].1),
                Op::Insert => (LineKind::Added, text(b[r.bi].0), b[r.bi].1),
            };
            match kind {
                LineKind::Context => {
                    hunk.old_lines += 1;
                    hunk.new_lines += 1;
                }
                LineKind::Removed => hunk.old_lines += 1,
                LineKind::Added => hunk.new_lines += 1,
            }
            hunk.lines.push(Line {
                kind,
                text: t,
                no_eol,
            });
        }
        // Unified-diff convention: an empty side starts at the line before.
        if hunk.old_lines == 0 {
            hunk.old_start -= 1;
        }
        if hunk.new_lines == 0 {
            hunk.new_start -= 1;
        }
        hunks.push(hunk);
        g = h + 1;
    }
    TextDiff::Text(hunks)
}

/// Render hunks as a unified diff for `path`.
pub fn render(path: &str, hunks: &[Hunk]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "--- a/{path}\n+++ b/{path}");
    for h in hunks {
        let _ = writeln!(
            s,
            "@@ -{},{} +{},{} @@",
            h.old_start, h.old_lines, h.new_start, h.new_lines
        );
        for l in &h.lines {
            let c = match l.kind {
                LineKind::Context => ' ',
                LineKind::Removed => '-',
                LineKind::Added => '+',
            };
            let _ = writeln!(s, "{c}{}", l.text);
            if l.no_eol {
                s.push_str("\\ No newline at end of file\n");
            }
        }
    }
    s
}

/// The content of the regular file at `path` in snapshot `snap`; `None`
/// when there is no regular file there.
pub fn file_at(store: &Store, snap: &Id, path: &Path) -> Result<Option<Vec<u8>>> {
    let mut tree = store.tree(&store.snapshot(snap)?.root)?;
    let comps: Vec<&std::ffi::OsStr> = path
        .components()
        .map(|c| match c {
            Component::Normal(n) => Ok(n),
            _ => Err(Error::Corrupt(format!("not a relative path: {path:?}"))),
        })
        .collect::<Result<_>>()?;
    for (i, name) in comps.iter().enumerate() {
        let Some(name) = name.to_str() else {
            return Ok(None);
        };
        match tree.get(name.as_bytes()) {
            Some(Node::Dir { id, .. }) if i + 1 < comps.len() => tree = store.tree(id)?,
            Some(Node::File { id, .. }) if i + 1 == comps.len() => {
                return Ok(Some(store.file_content(&store.file(id)?)?));
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// Line diff of `path` between snapshots `a` and `b`; a side without a
/// regular file there counts as empty.
pub fn file_diff(store: &Store, a: &Id, b: &Id, path: &Path) -> Result<TextDiff> {
    let old = file_at(store, a, path)?.unwrap_or_default();
    let new = file_at(store, b, path)?.unwrap_or_default();
    Ok(diff_text(&old, &new, CONTEXT))
}
