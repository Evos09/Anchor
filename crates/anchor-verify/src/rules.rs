//! Hard rules over the changed files that guard the test run itself
//! (review findings V-1 and V-2). The test runner executes in the same
//! process as the code under test, so its verdict is only as trustworthy
//! as that code: these rules reject changes that could reach the runner,
//! the interpreter's startup, or its import system, before any verdict is
//! believed. They are deliberately coarse; a legitimate change that trips
//! one gets the finding as feedback and can be reworked, and the residual
//! risk (obfuscated tampering the patterns miss) is an accepted risk that
//! the review stage and the regression check also cover.

use anchor_protocol::verify::{ChangedFile, FileStatus};

/// Files the interpreter or a test runner loads on its own, before or
/// around the tests: `.pth` files, `sitecustomize`/`usercustomize`,
/// compiled bytecode (an unchecked-hash `.pyc` is used without consulting
/// its source), and runner configuration.
pub fn startup_hooks(changes: &[ChangedFile]) -> Vec<&str> {
    changes
        .iter()
        .filter(|c| c.status != FileStatus::Removed)
        .filter(|c| {
            let lower = c.path.to_ascii_lowercase();
            let mut parts: Vec<&str> = lower.split('/').collect();
            let file = parts.pop().unwrap_or("");
            parts
                .iter()
                .any(|d| matches!(*d, "__pycache__" | "sitecustomize" | "usercustomize"))
                || file.ends_with(".pth")
                || file.ends_with(".pyc")
                || file.ends_with(".pyo")
                || matches!(
                    file,
                    "sitecustomize.py" | "usercustomize.py" | "conftest.py" | "pytest.ini"
                )
        })
        .map(|c| c.path.as_str())
        .collect()
}

/// Code nobody can read: a Python file too large or not text enough to
/// diff, or a native extension module (imported in preference to a `.py`
/// of the same name).
pub fn unreviewable_code(changes: &[ChangedFile]) -> Vec<&str> {
    changes
        .iter()
        .filter(|c| c.status != FileStatus::Removed)
        .filter(|c| {
            let lower = c.path.to_ascii_lowercase();
            (c.status == FileStatus::Opaque && lower.ends_with(".py"))
                || [".so", ".pyd", ".dylib", ".dll"]
                    .iter()
                    .any(|e| lower.ends_with(e))
        })
        .map(|c| c.path.as_str())
        .collect()
}

/// Names whose use lets code under test reach the test runner, the
/// process's exit, its stderr (where the verdict is read), or import
/// machinery that hides what runs.
const MARKERS: &[&str] = &[
    "_exit",
    "sys.modules",
    "builtins",
    "__builtins__",
    "atexit",
    "settrace",
    "setprofile",
    "TestResult",
    "TextTestResult",
    "TextTestRunner",
    "TestProgram",
    "stderr",
    "__stderr__",
    "dup2",
    "__import__",
    "importlib",
    "exec(",
    "eval(",
    "compile(",
];

fn ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether `needle` occurs in `hay` as a whole name: not preceded by an
/// identifier character (nor, for a call such as `eval(`, by a dot, so
/// `re.compile(` is not `compile(`), and not followed by one.
fn has_name(hay: &str, needle: &str) -> bool {
    let call = needle.ends_with('(');
    hay.match_indices(needle).any(|(i, _)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + needle.len()..].chars().next();
        !before.is_some_and(|c| ident(c) || (call && c == '.'))
            && (call || !after.is_some_and(ident))
    })
}

/// Added Python lines that use a [`MARKERS`] name (in any file, tests
/// included: a test module runs inside the runner too), or that import
/// `unittest` outside test files. Each site is `path:line: name`.
pub fn runner_tampering(changes: &[ChangedFile]) -> Vec<String> {
    let mut sites = Vec::new();
    for c in changes
        .iter()
        .filter(|c| c.status != FileStatus::Removed && c.path.ends_with(".py"))
    {
        for (n, text) in c.added_lines.iter().zip(&c.added_text) {
            let code = text.split('#').next().unwrap_or("");
            let mut hit = MARKERS.iter().copied().find(|m| has_name(code, m));
            if hit.is_none() && !c.is_test && has_name(code, "unittest") {
                hit = Some("unittest");
            }
            if let Some(m) = hit {
                sites.push(format!("{}:{n}: {m}", c.path));
            }
        }
    }
    sites
}
