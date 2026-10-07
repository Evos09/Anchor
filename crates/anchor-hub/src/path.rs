//! Claim paths (spec §6.4): workspace-relative, normalised, `/`-separated.
//!
//! * `"."` (or `"./"`) is the whole workspace, stored as `"/"`;
//! * `"src/"` (trailing slash) is the subtree `src` and everything in it;
//! * `"src/main.rs"` is one path.
//!
//! Absolute paths, `..`, empty segments other than a trailing slash,
//! backslashes and control characters are refused, so a claim can never
//! name anything outside the workspace or alias another spelling.

use std::fmt;

const MAX_LEN: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClaimPath {
    segs: Vec<String>,
    subtree: bool,
}

impl ClaimPath {
    pub fn parse(s: &str) -> Result<ClaimPath, String> {
        if s.is_empty() || s.len() > MAX_LEN {
            return Err("claim path must be 1-1024 bytes".into());
        }
        if s.chars().any(|c| c.is_control() || c == '\\') {
            return Err("claim path contains a backslash or control character".into());
        }
        if s == "." || s == "./" {
            return Ok(ClaimPath {
                segs: Vec::new(),
                subtree: true,
            });
        }
        if s.starts_with('/') || s.contains(':') {
            return Err("claim path must be workspace-relative".into());
        }
        let subtree = s.ends_with('/');
        let body = s.strip_suffix('/').unwrap_or(s);
        let mut segs = Vec::new();
        for seg in body.split('/') {
            match seg {
                "" => return Err("claim path has an empty segment".into()),
                "." | ".." => return Err("claim path must not contain . or ..".into()),
                _ if windows_alias(seg) => {
                    return Err(format!(
                        "claim path segment {seg:?} can alias another name on Windows"
                    ));
                }
                _ => segs.push(seg.to_string()),
            }
        }
        Ok(ClaimPath { segs, subtree })
    }

    /// A file path (as the mediator sees a write target).
    pub fn file(rel: &str) -> Result<ClaimPath, String> {
        let p = ClaimPath::parse(rel)?;
        if p.subtree {
            return Err("not a file path".into());
        }
        Ok(p)
    }

    pub fn is_subtree(&self) -> bool {
        self.subtree
    }

    /// `self` names `other` or a path inside it. Where the file system
    /// ignores case, so does this.
    pub fn covers(&self, other: &ClaimPath) -> bool {
        if self.segs.len() > other.segs.len() {
            return false;
        }
        let prefix = same_segs(
            &self.segs,
            &other.segs[..self.segs.len()],
            CASE_INSENSITIVE_FS,
        );
        if !prefix {
            return false;
        }
        self.segs.len() == other.segs.len() && (self.subtree || !other.subtree) || self.subtree
    }

    /// The two claims name at least one common path. Case is ignored on
    /// every platform: a false conflict only serialises two agents, a
    /// missed one lets them edit the same file (review finding H-4).
    pub fn overlaps(&self, other: &ClaimPath) -> bool {
        let n = self.segs.len().min(other.segs.len());
        if !same_segs(&self.segs[..n], &other.segs[..n], true) {
            return false;
        }
        match self.segs.len().cmp(&other.segs.len()) {
            std::cmp::Ordering::Equal => true,
            std::cmp::Ordering::Less => self.subtree,
            std::cmp::Ordering::Greater => other.subtree,
        }
    }
}

/// Whether the platform's usual file systems (APFS, NTFS) treat names
/// that differ only in case as the same name.
const CASE_INSENSITIVE_FS: bool = cfg!(any(windows, target_os = "macos"));

fn same_segs(a: &[String], b: &[String], fold: bool) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x == y
                || (fold
                    && x.chars()
                        .flat_map(char::to_lowercase)
                        .eq(y.chars().flat_map(char::to_lowercase)))
        })
}

/// A name Windows resolves to a different spelling: a trailing dot or
/// space (stripped), an 8.3 short name (`PROGRA~1`), or a reserved device
/// name. Refused on every platform, so a claim means the same everywhere.
fn windows_alias(seg: &str) -> bool {
    if seg.ends_with('.') || seg.ends_with(' ') {
        return true;
    }
    let stem = seg.split('.').next().unwrap_or(seg);
    if let Some((head, tail)) = stem.rsplit_once('~')
        && !head.is_empty()
        && !tail.is_empty()
        && tail.bytes().all(|b| b.is_ascii_digit())
    {
        return true;
    }
    let upper = stem.trim_end().to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit())
}

impl fmt::Display for ClaimPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.segs.is_empty() {
            return f.write_str("/");
        }
        f.write_str(&self.segs.join("/"))?;
        if self.subtree {
            f.write_str("/")?;
        }
        Ok(())
    }
}

/// Parse a stored (normalised) claim path.
pub(crate) fn stored(s: &str) -> ClaimPath {
    if s == "/" {
        return ClaimPath {
            segs: Vec::new(),
            subtree: true,
        };
    }
    ClaimPath::parse(s).unwrap_or(ClaimPath {
        segs: vec![s.to_string()],
        subtree: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ClaimPath {
        ClaimPath::parse(s).unwrap()
    }

    #[test]
    fn hostile_claim_paths_are_refused() {
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "./a",
            "a/./b",
            "C:\\x",
            "c:x",
            "a\\b",
            "a\u{0}b",
            "a\nb",
            "/",
            "src.",
            "src /a",
            "PROGRA~1/x",
            "docs/README~2.md",
            "nul",
            "a/con.txt",
            "COM1",
        ] {
            assert!(ClaimPath::parse(bad).is_err(), "{bad:?}");
        }
        assert!(ClaimPath::parse(&"a/".repeat(600)).is_err());
    }

    #[test]
    fn normalised_forms() {
        assert_eq!(p(".").to_string(), "/");
        assert_eq!(p("./").to_string(), "/");
        assert_eq!(p("src/").to_string(), "src/");
        assert_eq!(p("src/a.rs").to_string(), "src/a.rs");
        assert_eq!(stored("/"), p("."));
        assert_eq!(stored("src/"), p("src/"));
    }

    #[test]
    fn overlap_and_cover_matrix() {
        let cases = [
            ("a.txt", "a.txt", true),
            ("a.txt", "b.txt", false),
            ("src/", "src/a.rs", true),
            ("src/a.rs", "src/", true),
            ("src", "src/a.rs", false),
            ("src/", "srcx/a.rs", false),
            (".", "anything/at/all", true),
            ("src/", "src", true),
            ("a/b/", "a/c/", false),
        ];
        for (x, y, want) in cases {
            assert_eq!(p(x).overlaps(&p(y)), want, "{x} ~ {y}");
            assert_eq!(p(y).overlaps(&p(x)), want, "{y} ~ {x}");
        }
        assert!(p("src/").covers(&p("src/a/b.rs")));
        assert!(p(".").covers(&p("src/")));
        assert!(!p("src/a.rs").covers(&p("src/")));
        assert!(p("src/a.rs").covers(&p("src/a.rs")));
        assert!(!p("src/").covers(&p("lib/x")));
        // Case never hides a conflict, and covers follows the platform.
        assert!(p("SRC/").overlaps(&p("src/a.rs")));
        assert!(p("Readme.md").overlaps(&p("README.MD")));
        assert_eq!(
            p("SRC/").covers(&p("src/a.rs")),
            cfg!(any(windows, target_os = "macos"))
        );
        for ok in ["a~b", "~tmp", "x~", ".config", "console.log", "com10"] {
            assert!(ClaimPath::parse(ok).is_ok(), "{ok}");
        }
    }
}
