//! Validation of non-path tool parameters as hostile input (spec §5.2(e)).
//!
//! * Glob parameters must be workspace-relative: no absolute paths, no `..`
//!   components, no `~`, no NUL, bounded length.
//! * Regex parameters are compiled with the `regex` crate, which guarantees
//!   linear-time matching (no backtracking, so no catastrophic backtracking),
//!   with explicit compiled-size limits.
//! * There are no format-string parameters in the tool surface; command
//!   arguments are passed as an argv array and never joined into a shell
//!   string (see `anchor-exec`).

use globset::{GlobBuilder, GlobMatcher};

pub const MAX_PATTERN_BYTES: usize = 1024;
/// Upper bound on a compiled regex program.
pub const REGEX_SIZE_LIMIT: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParamError {
    #[error("pattern is empty")]
    Empty,
    #[error("pattern exceeds {MAX_PATTERN_BYTES} bytes")]
    TooLong,
    #[error("pattern contains NUL")]
    Nul,
    #[error("glob must be workspace-relative (absolute path or drive prefix found)")]
    AbsoluteGlob,
    #[error("glob contains a `..` component")]
    Traversal,
    #[error("glob contains `~`")]
    HomeExpansion,
    #[error("invalid glob: {0}")]
    BadGlob(String),
    #[error("invalid or too complex regex: {0}")]
    BadRegex(String),
}

fn basic(p: &str) -> Result<(), ParamError> {
    if p.is_empty() {
        return Err(ParamError::Empty);
    }
    if p.len() > MAX_PATTERN_BYTES {
        return Err(ParamError::TooLong);
    }
    if p.contains('\0') {
        return Err(ParamError::Nul);
    }
    Ok(())
}

/// Validate and compile a workspace-relative glob.
pub fn validate_glob(p: &str) -> Result<GlobMatcher, ParamError> {
    basic(p)?;
    if p.starts_with('/') || p.starts_with('\\') || p.as_bytes().get(1) == Some(&b':') {
        return Err(ParamError::AbsoluteGlob);
    }
    if p.contains('~') {
        return Err(ParamError::HomeExpansion);
    }
    if p.split(['/', '\\']).any(|c| c == "..") {
        return Err(ParamError::Traversal);
    }
    GlobBuilder::new(p)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| ParamError::BadGlob(e.to_string()))
}

/// Validate and compile a search regex.
pub fn validate_regex(p: &str) -> Result<regex::Regex, ParamError> {
    basic(p)?;
    regex::RegexBuilder::new(p)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .nest_limit(64)
        .build()
        .map_err(|e| ParamError::BadRegex(e.to_string()))
}
