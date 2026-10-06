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

    /// `self` names `other` or a path inside it.
    pub fn covers(&self, other: &ClaimPath) -> bool {
        if self.segs.len() > other.segs.len() {
            return false;
        }
        let prefix = self.segs[..] == other.segs[..self.segs.len()];
        if !prefix {
            return false;
        }
        self.segs.len() == other.segs.len() && (self.subtree || !other.subtree) || self.subtree
    }

    /// The two claims name at least one common path.
    pub fn overlaps(&self, other: &ClaimPath) -> bool {
        let n = self.segs.len().min(other.segs.len());
        if self.segs[..n] != other.segs[..n] {
            return false;
        }
        match self.segs.len().cmp(&other.segs.len()) {
            std::cmp::Ordering::Equal => true,
            std::cmp::Ordering::Less => self.subtree,
            std::cmp::Ordering::Greater => other.subtree,
        }
    }
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
    }
}
