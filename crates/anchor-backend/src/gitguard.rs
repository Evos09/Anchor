//! Git internals are configuration that git *executes* (hooks, `core.*`
//! commands, filter and diff drivers). A sandboxed command may write the
//! whole workspace, `.git` included, and the user's own git would later run
//! what it planted, outside any sandbox (review finding T-1).
//!
//! The mediator therefore records these files before each command and, once
//! the command and every process it started are gone, puts back any that
//! changed, removes any that appeared, and reports it. Only the executable
//! surface is guarded; objects, refs and the index stay writable, so
//! commands that commit or check out keep working.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Largest guarded file kept for restoring (hooks and configs are small).
const MAX_FILE: u64 = 1 << 20;
/// Most guarded entries examined (a command could create millions).
const MAX_ENTRIES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    File {
        bytes: Vec<u8>,
        mode: u32,
    },
    Dir,
    /// Present but not restorable as a file (directory, symlink, too large).
    Other,
}

/// The guarded files of one workspace at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitState {
    entries: BTreeMap<PathBuf, Entry>,
}

#[cfg(unix)]
fn mode_of(m: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o777
}
#[cfg(not(unix))]
fn mode_of(_m: &std::fs::Metadata) -> u32 {
    0o644
}

/// Whether a path relative to a git directory is executable surface.
fn guarded(rel: &Path) -> bool {
    let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    // Strip any number of `modules/<name>/` and one `worktrees/<name>/`.
    let mut i = 0;
    while parts.len() > i + 2 && (parts[i] == "modules" || parts[i] == "worktrees") {
        i += 2;
    }
    matches!(
        &parts[i..],
        ["config"]
            | ["config.worktree"]
            | ["commondir"]
            | ["gitdir"]
            | ["info", "attributes"]
            | ["hooks", ..]
    )
}

impl GitState {
    /// Record the guarded files under `<root>/.git` (also when `.git` is a
    /// `gitdir:` file, which is itself guarded).
    pub fn capture(root: &Path) -> GitState {
        let mut st = GitState::default();
        let dot = root.join(".git");
        let Ok(meta) = std::fs::symlink_metadata(&dot) else {
            return st;
        };
        if !meta.is_dir() {
            st.entries
                .insert(PathBuf::from(".git"), read_entry(&dot, &meta));
            return st;
        }
        let mut stack = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(dot.join(&rel)) else {
                continue;
            };
            for e in rd.flatten() {
                if st.entries.len() >= MAX_ENTRIES {
                    return st;
                }
                let r = rel.join(e.file_name());
                let Ok(m) = std::fs::symlink_metadata(dot.join(&r)) else {
                    continue;
                };
                let is_guarded = guarded(&r);
                if m.is_dir() && !m.file_type().is_symlink() {
                    // Descend only where guarded files can live.
                    let first = r.iter().next().and_then(|c| c.to_str()).unwrap_or("");
                    if is_guarded || matches!(first, "hooks" | "info" | "modules" | "worktrees") {
                        stack.push(r.clone());
                    }
                    if is_guarded {
                        st.entries.insert(Path::new(".git").join(&r), Entry::Dir);
                    }
                } else if is_guarded {
                    st.entries
                        .insert(Path::new(".git").join(&r), read_entry(&dot.join(&r), &m));
                }
            }
        }
        st
    }

    /// Restore `root` to `self` where `now` differs: changed files are
    /// rewritten, new entries removed. Never follows a symlink. Returns the
    /// workspace-relative paths that were put back.
    pub fn restore(&self, root: &Path, now: &GitState) -> Vec<String> {
        let mut fixed = Vec::new();
        for (rel, entry) in &now.entries {
            if self.entries.get(rel) != Some(entry) {
                if !self.entries.contains_key(rel) {
                    remove_no_follow(&root.join(rel));
                }
                fixed.push(slash(rel));
            }
        }
        for (rel, entry) in &self.entries {
            if now.entries.get(rel) == Some(entry) {
                continue;
            }
            if let Entry::File { bytes, mode } = entry
                && write_no_follow(root, rel, bytes, *mode)
                && !fixed.contains(&slash(rel))
            {
                fixed.push(slash(rel));
            }
        }
        fixed.sort();
        fixed.dedup();
        fixed
    }
}

/// A workspace-relative path with `/` separators on every platform, as
/// audit entries write paths.
fn slash(rel: &Path) -> String {
    rel.iter()
        .map(|c| c.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn read_entry(p: &Path, m: &std::fs::Metadata) -> Entry {
    if !m.is_file() || m.len() > MAX_FILE {
        return Entry::Other;
    }
    match std::fs::read(p) {
        Ok(bytes) => Entry::File {
            bytes,
            mode: mode_of(m),
        },
        Err(_) => Entry::Other,
    }
}

/// Remove a file, symlink or directory tree without following links
/// (`remove_dir_all` does not follow symlinks).
fn remove_no_follow(p: &Path) {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {
            let _ = std::fs::remove_dir_all(p);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(p);
        }
        Err(_) => {}
    }
}

/// Write `bytes` at `root/rel`, replacing whatever is there, with every
/// ancestor below `root` checked to be a real directory (a symlinked
/// ancestor is removed and recreated, so nothing is written through it).
fn write_no_follow(root: &Path, rel: &Path, bytes: &[u8], mode: u32) -> bool {
    let mut dir = root.to_path_buf();
    let comps: Vec<_> = rel.iter().collect();
    for c in &comps[..comps.len().saturating_sub(1)] {
        dir.push(c);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            Ok(_) => {
                remove_no_follow(&dir);
                if std::fs::create_dir(&dir).is_err() {
                    return false;
                }
            }
            Err(_) => {
                if std::fs::create_dir(&dir).is_err() {
                    return false;
                }
            }
        }
    }
    let target = root.join(rel);
    remove_no_follow(&target);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(mode).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    let _ = mode;
    match o.open(&target) {
        Ok(mut f) => std::io::Write::write_all(&mut f, bytes).is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_and_config_changes_are_put_back() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::write(root.join(".git/config"), "[core]\n").unwrap();
        std::fs::write(root.join(".git/hooks/pre-commit.sample"), "#!/bin/sh\n").unwrap();
        let before = GitState::capture(root);

        // What a hostile command might do.
        std::fs::write(
            root.join(".git/config"),
            "[core]\nfsmonitor = touch /tmp/pwned\n",
        )
        .unwrap();
        std::fs::write(root.join(".git/hooks/post-checkout"), "#!/bin/sh\nevil\n").unwrap();
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::write(root.join(".git/info/attributes"), "* filter=evil\n").unwrap();
        std::fs::write(root.join(".git/objects/ok"), "objects stay writable").unwrap();
        let outside = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            std::fs::remove_file(root.join(".git/hooks/pre-commit.sample")).unwrap();
            std::os::unix::fs::symlink(
                outside.path().join("x"),
                root.join(".git/hooks/pre-commit.sample"),
            )
            .unwrap();
        }

        let now = GitState::capture(root);
        let fixed = before.restore(root, &now);
        assert!(fixed.contains(&".git/config".to_string()), "{fixed:?}");
        assert!(
            fixed.contains(&".git/hooks/post-checkout".to_string()),
            "{fixed:?}"
        );
        assert!(
            fixed.contains(&".git/info/attributes".to_string()),
            "{fixed:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".git/config")).unwrap(),
            "[core]\n"
        );
        assert!(!root.join(".git/hooks/post-checkout").exists());
        assert!(!root.join(".git/info/attributes").exists());
        assert!(root.join(".git/objects/ok").exists());
        // Restored as a regular file; nothing written through the link.
        let m = std::fs::symlink_metadata(root.join(".git/hooks/pre-commit.sample")).unwrap();
        assert!(m.is_file());
        assert!(!outside.path().join("x").exists());
        assert_eq!(GitState::capture(root), before);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_hooks_directory_is_replaced_not_followed() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::fs::write(root.join(".git/hooks/keep"), "k").unwrap();
        let before = GitState::capture(root);
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("post-merge"), "#!/bin/sh\nevil\n").unwrap();
        std::fs::remove_dir_all(root.join(".git/hooks")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join(".git/hooks")).unwrap();
        let now = GitState::capture(root);
        before.restore(root, &now);
        let m = std::fs::symlink_metadata(root.join(".git/hooks")).unwrap();
        assert!(m.is_dir() && !m.file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(root.join(".git/hooks/keep")).unwrap(),
            "k"
        );
        assert!(
            outside.path().join("post-merge").exists(),
            "outside untouched"
        );
        assert!(!outside.path().join("keep").exists());
    }
}
