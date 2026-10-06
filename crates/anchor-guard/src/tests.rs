use std::path::Path;

use super::*;

fn sha(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

fn guard() -> (Guard, tempfile::TempDir) {
    let d = tempfile::tempdir().unwrap();
    let g = Guard::load(&d.path().join(".anchor/workspace.toml")).unwrap();
    (g, d)
}

const CANON: &str = "/ws/src/main.rs";

#[test]
fn heuristic_file_names() {
    for n in ["DEVLOG", "DEVLOG.md", "CHANGELOG.md", "changelog", "History.rst", "JOURNAL.txt", "docs/CHANGES"] {
        assert_eq!(
            Guard::heuristic(Path::new(n), None),
            Some(HeuristicReason::FileName),
            "{n}"
        );
    }
    for n in ["README.md", "changelog_parser.py", "src/history_view.rs"] {
        assert_eq!(Guard::heuristic(Path::new(n), None), None, "{n}");
    }
}

#[test]
fn heuristic_dated_entries() {
    let log = b"# Notes\n\n## 2026-01-01\nstarted\n\n## 2026-01-02\nmore\n\n## 2026-01-05\nthings\n";
    assert_eq!(
        Guard::heuristic(Path::new("notes.md"), Some(log)),
        Some(HeuristicReason::DatedEntries)
    );
    let plain_log = b"2026-01-01 a\n2026-01-02 b\n2026-01-03 c\n";
    assert_eq!(
        Guard::heuristic(Path::new("ops.log"), Some(plain_log)),
        Some(HeuristicReason::DatedEntries)
    );
}

/// False-positive guards: a README with a few dated release headings among
/// many others, and a CSV keyed by date, are not flagged.
#[test]
fn heuristic_avoids_common_false_positives() {
    let readme = b"# Project\n## Install\n## Usage\n## API\n## FAQ\n## 2026-01-01 release\n## 2025-06-01 release\n## 2025-01-01 release\n## License\n## Contributing\n";
    assert_eq!(Guard::heuristic(Path::new("README.md"), Some(readme)), None);
    let csv = b"2026-01-01,1\n2026-01-02,2\n2026-01-03,3\n";
    assert_eq!(Guard::heuristic(Path::new("prices.csv"), Some(csv)), None);
    let code = b"// 2026-01-01\n// 2026-01-02\n// 2026-01-03\n";
    assert_eq!(Guard::heuristic(Path::new("main.rs"), Some(code)), None);
}

#[test]
fn marker_and_explicit_config() {
    let d = tempfile::tempdir().unwrap();
    let cfg = WorkspaceConfig {
        append_only: vec!["logs/*.md".into()],
        not_append_only: vec![],
    };
    let g = Guard::with_config(&d.path().join("w.toml"), cfg).unwrap();
    assert_eq!(g.classify(Path::new("logs/a.md"), None), AppendOnlyStatus::Explicit);
    assert_eq!(g.classify(Path::new("logs/sub/a.md"), None), AppendOnlyStatus::No);
    assert_eq!(
        g.classify(Path::new("x.txt"), Some(b"<!-- anchor:append-only -->\nhello")),
        AppendOnlyStatus::Marker
    );
    assert_eq!(g.classify(Path::new("x.txt"), Some(b"hello")), AppendOnlyStatus::No);
}

/// §5.3 test: a Write to an append-only file is refused.
#[test]
fn write_to_append_only_refused() {
    let (mut g, _d) = guard();
    let st = g.classify(Path::new("DEVLOG.md"), Some(b"x"));
    assert!(st.is_append_only());
    // Even if fully read.
    g.record_full_read(Path::new("/ws/DEVLOG.md"), sha(b"x"));
    assert_eq!(
        g.check_write(Path::new("/ws/DEVLOG.md"), st, Some(sha(b"x")), true),
        Err(GuardError::AppendOnlyWriteBlocked)
    );
}

/// §5.3 test: an Edit-append to an append-only file succeeds; an edit that
/// modifies an existing byte is refused.
#[test]
fn append_only_edits() {
    let (g, _d) = guard();
    let st = g.classify(Path::new("CHANGELOG.md"), None);
    let orig = b"## 1.0\n- first\n";
    let mut appended = orig.to_vec();
    appended.extend_from_slice(b"## 1.1\n- second\n");
    assert!(g.check_edit(Path::new("/ws/CHANGELOG.md"), st, orig, &appended, false).is_ok());
    let modified = b"## 1.0\n- FIRST\n## 1.1\n";
    assert_eq!(
        g.check_edit(Path::new("/ws/CHANGELOG.md"), st, orig, modified, true),
        Err(GuardError::AppendOnlyModification)
    );
    let truncated = b"## 1.0\n";
    assert_eq!(
        g.check_edit(Path::new("/ws/CHANGELOG.md"), st, orig, truncated, false),
        Err(GuardError::AppendOnlyModification)
    );
}

/// §5.3 test: a Write to a non-append-only file that was not read is refused.
#[test]
fn write_without_read_refused() {
    let (g, _d) = guard();
    assert_eq!(
        g.check_write(Path::new(CANON), AppendOnlyStatus::No, Some(sha(b"old")), false),
        Err(GuardError::NotReadInSession)
    );
}

/// §5.3 test: a Write to a file read in-session succeeds.
#[test]
fn write_after_full_read_succeeds() {
    let (mut g, _d) = guard();
    g.record_full_read(Path::new(CANON), sha(b"old"));
    assert_eq!(
        g.check_write(Path::new(CANON), AppendOnlyStatus::No, Some(sha(b"old")), false),
        Ok(WriteBasis::FullReadInSession)
    );
}

#[test]
fn stale_read_refused_but_diff_confirmation_allows() {
    let (mut g, _d) = guard();
    g.record_full_read(Path::new(CANON), sha(b"old"));
    assert_eq!(
        g.check_write(Path::new(CANON), AppendOnlyStatus::No, Some(sha(b"changed")), false),
        Err(GuardError::StaleRead)
    );
    assert_eq!(
        g.check_write(Path::new(CANON), AppendOnlyStatus::No, Some(sha(b"changed")), true),
        Ok(WriteBasis::UserConfirmedDiff)
    );
}

#[test]
fn new_files_need_no_read() {
    let (g, _d) = guard();
    assert_eq!(
        g.check_write(Path::new(CANON), AppendOnlyStatus::No, None, false),
        Ok(WriteBasis::NewFile)
    );
}

#[test]
fn edits_require_read_too() {
    let (mut g, _d) = guard();
    assert_eq!(
        g.check_edit(Path::new(CANON), AppendOnlyStatus::No, b"a", b"b", false),
        Err(GuardError::NotReadInSession)
    );
    g.record_full_read(Path::new(CANON), sha(b"a"));
    assert!(g.check_edit(Path::new(CANON), AppendOnlyStatus::No, b"a", b"b", false).is_ok());
}

/// §5.3 test: the "not append-only" override persists across sessions and is
/// surfaced as an override, not silently as "no".
#[test]
fn override_persists_across_sessions() {
    let d = tempfile::tempdir().unwrap();
    let cfg_path = d.path().join(".anchor/workspace.toml");
    {
        let mut g = Guard::load(&cfg_path).unwrap();
        assert_eq!(
            g.classify(Path::new("docs/HISTORY.md"), None),
            AppendOnlyStatus::Heuristic(HeuristicReason::FileName)
        );
        g.mark_not_append_only(Path::new("docs/HISTORY.md")).unwrap();
    }
    let g = Guard::load(&cfg_path).unwrap();
    let st = g.classify(Path::new("docs/HISTORY.md"), None);
    assert_eq!(st, AppendOnlyStatus::Overridden(HeuristicReason::FileName));
    assert!(!st.is_append_only());
    // Explicit marking still beats an override.
    assert_eq!(
        g.classify(Path::new("docs/HISTORY.md"), Some(b"anchor:append-only")),
        AppendOnlyStatus::Marker
    );
}

#[test]
fn rejects_unknown_config_keys() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("w.toml");
    std::fs::write(&p, "append_only = []\nallow_everything = true\n").unwrap();
    assert!(Guard::load(&p).is_err());
}
