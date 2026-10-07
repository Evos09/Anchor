//! Verifier plugins (spec §14 Phase 8, ADR-0009): reviewers that run as
//! separate programs, in the command sandbox, speaking verifier protocol
//! version 1 (`anchor_protocol::verifier`). The contract a plugin author
//! needs is `docs/verifier/`.
//!
//! A plugin ships a manifest, `verifier.toml`:
//!
//! ```toml
//! name = "py-hygiene"
//! command = ["python3", "verifier.py"]
//! windows_command = ["python", "verifier.py"]   # optional
//! timeout_secs = 60                             # optional, 1..=600
//! memory_mb = 512                               # optional, 64..=8192
//! ```
//!
//! The manifest's directory is the plugin's directory: its working
//! directory and the only thing it can read besides the system and `PATH`
//! directories. It gets no network, no writable directory, and no part of
//! the workspace. Each review is one fresh run, so nothing carries over
//! from one candidate to the next.
//!
//! Anything other than a well-formed answer (a crash, a timeout, a bad
//! response, a protocol the plugin does not speak) makes every claim
//! `unverifiable` with `verifier_failed`: a plugin fails closed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anchor_protocol::verifier::{self as wire, Info, Request};
use anchor_protocol::verify::{ClaimVerdict, ReviewInput};
use anchor_verify::Reviewer;
use serde::Deserialize;

/// The manifest's file name, by convention.
pub const MANIFEST: &str = "verifier.toml";
/// At most this many plugins per gate.
pub const MAX_VERIFIERS: usize = 8;

/// `verifier.toml`, as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFile {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub windows_command: Option<Vec<String>>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_memory")]
    pub memory_mb: u64,
}

fn default_timeout() -> u64 {
    60
}
fn default_memory() -> u64 {
    512
}

/// A validated plugin: what to run and where.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    /// The plugin's directory (canonical).
    pub dir: PathBuf,
    /// Absolute program path.
    pub program: PathBuf,
    pub args: Vec<String>,
    pub timeout: Duration,
    pub memory_bytes: u64,
}

impl Plugin {
    /// Load the manifest at `path` (a `verifier.toml`, or a directory
    /// holding one). The program is a bare name looked up in `path_var`,
    /// or a relative path inside the plugin's directory.
    pub fn load(path: &Path, path_var: &str) -> Result<Plugin, String> {
        let manifest = if path.is_dir() {
            path.join(MANIFEST)
        } else {
            path.to_path_buf()
        };
        let text = std::fs::read_to_string(&manifest)
            .map_err(|e| format!("{}: {e}", manifest.display()))?;
        let file: ManifestFile =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", manifest.display()))?;
        let dir = manifest
            .parent()
            .map(|d| {
                if d.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    d
                }
            })
            .ok_or("manifest has no directory")?;
        let dir = std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Plugin::from_manifest(file, dir, path_var)
            .map_err(|e| format!("{}: {e}", manifest.display()))
    }

    fn from_manifest(file: ManifestFile, dir: PathBuf, path_var: &str) -> Result<Plugin, String> {
        let name_ok = !file.name.is_empty()
            && file.name.len() <= 64
            && file.name.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
            });
        if !name_ok {
            return Err("name must be 1..=64 bytes of [a-z0-9_.-]".into());
        }
        let command = match (cfg!(windows), file.windows_command) {
            (true, Some(c)) => c,
            _ => file.command,
        };
        let (first, args) = command
            .split_first()
            .ok_or("command: give a program and its arguments")?;
        if first.is_empty() || command.iter().any(|a| a.contains('\0')) {
            return Err("command: empty program or NUL in an argument".into());
        }
        let program = if first.contains(['/', '\\']) {
            let p = std::fs::canonicalize(dir.join(first))
                .map_err(|e| format!("command: {first}: {e}"))?;
            if !p.starts_with(&dir) || !p.is_file() {
                return Err(format!(
                    "command: {first} must be a file inside the plugin's directory"
                ));
            }
            p
        } else {
            anchor_exec::resolve_program(first, path_var)
                .map_err(|e| format!("command: {first}: {e}"))?
        };
        if !(1..=600).contains(&file.timeout_secs) {
            return Err("timeout_secs: 1 to 600".into());
        }
        if !(64..=8192).contains(&file.memory_mb) {
            return Err("memory_mb: 64 to 8192".into());
        }
        Ok(Plugin {
            name: file.name,
            dir,
            program,
            args: args.to_vec(),
            timeout: Duration::from_secs(file.timeout_secs),
            memory_bytes: file.memory_mb << 20,
        })
    }

    /// Refuse a plugin whose directory is the workspace or holds it: the
    /// plugin could then read the workspace (ADR-0009).
    pub fn check_outside(&self, workspace: &Path) -> Result<(), String> {
        let ws = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        if ws.starts_with(&self.dir) {
            return Err(format!(
                "verifier {}: its directory {} holds the workspace; \
                 put the plugin in a directory of its own",
                self.name,
                self.dir.display()
            ));
        }
        Ok(())
    }
}

/// How a plugin run went.
#[derive(Debug)]
pub struct RunOutput {
    pub stdout: Vec<u8>,
    pub elapsed: Duration,
}

/// Runs plugins in the sandbox.
#[derive(Debug, Clone)]
pub struct Runner {
    path_var: String,
    read_only: Vec<PathBuf>,
}

impl Runner {
    /// `read_only` is what a plugin may read besides its own directory
    /// (the system and `PATH` directories, as for commands).
    pub fn new(path_var: String, read_only: Vec<PathBuf>) -> Runner {
        Runner {
            path_var,
            read_only,
        }
    }

    /// One run: `request` on stdin, stdout back. Fails on a sandbox
    /// error, a timeout, a kill, or a non-zero exit.
    pub fn run(&self, plugin: &Plugin, request: Vec<u8>) -> Result<RunOutput, String> {
        let mut read_only = vec![plugin.dir.clone()];
        read_only.extend(self.read_only.iter().cloned());
        let spec = anchor_exec::CommandSpec {
            program: plugin.program.clone(),
            args: plugin.args.clone(),
            cwd: plugin.dir.clone(),
            env: vec![
                ("PATH".into(), self.path_var.clone()),
                ("LANG".into(), "C.UTF-8".into()),
                ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
                ("PYTHONNOUSERSITE".into(), "1".into()),
                (
                    "ANCHOR_VERIFIER_PROTOCOL".into(),
                    wire::VERIFIER_PROTOCOL.to_string(),
                ),
            ],
            limits: anchor_exec::Limits {
                cpu_seconds: plugin.timeout.as_secs(),
                wall_timeout: plugin.timeout,
                address_space_bytes: plugin.memory_bytes,
                max_processes: 32,
                max_file_bytes: 1 << 20,
                max_output_bytes: wire::MAX_RESPONSE_BYTES + 1,
            },
            network: anchor_exec::Network::Deny,
            fs: Some(anchor_exec::FsConfinement {
                read_write: Vec::new(),
                read_only,
            }),
            stdin: request,
        };
        let result = anchor_exec::run(&spec);
        // On Windows each run's AppContainer profile (and the folder Windows
        // lets it write) is deleted, so no state reaches the next review.
        if let Some(fs) = &spec.fs {
            let _ = anchor_exec::release(fs, &spec.cwd);
        }
        let o = result.map_err(|e| format!("could not run {}: {e}", plugin.name))?;
        let stderr = String::from_utf8_lossy(&o.stderr);
        let tail: String = stderr
            .chars()
            .rev()
            .take(400)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        match o.exit {
            anchor_exec::ExitKind::Exited(0) => Ok(RunOutput {
                stdout: o.stdout,
                elapsed: o.elapsed,
            }),
            anchor_exec::ExitKind::TimedOut => {
                Err(format!("timed out after {}s", plugin.timeout.as_secs()))
            }
            other => Err(format!("{other:?}; stderr: {}", tail.trim())),
        }
    }

    /// `describe`: who the plugin is, and that it speaks version 1.
    pub fn describe(&self, plugin: &Plugin, id: u64) -> Result<Info, String> {
        let out = self.run(plugin, Request::describe(id).encode())?;
        wire::check_info(id, &out.stdout)
    }

    /// `verify`: one verdict per claim, or why not.
    pub fn verify(
        &self,
        plugin: &Plugin,
        id: u64,
        input: &ReviewInput,
    ) -> Result<Vec<ClaimVerdict>, String> {
        let out = self.run(plugin, Request::verify(id, input.clone()).encode())?;
        wire::check_verdicts(id, input, &out.stdout)
    }
}

/// A plugin as a [`Reviewer`]: combined with the built-in reviewer
/// through [`anchor_verify::Combined`], so it can only add rejections.
#[derive(Debug)]
pub struct PluginVerifier {
    label: String,
    plugin: Plugin,
    runner: Runner,
    info: Option<Info>,
    next_id: u64,
}

impl PluginVerifier {
    pub fn new(plugin: Plugin, runner: Runner) -> PluginVerifier {
        PluginVerifier {
            label: format!("plugin:{}", plugin.name),
            plugin,
            runner,
            info: None,
            next_id: 1,
        }
    }

    fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

impl Reviewer for PluginVerifier {
    fn name(&self) -> &str {
        &self.label
    }

    fn review(&mut self, input: &ReviewInput) -> Result<Vec<ClaimVerdict>, String> {
        if self.info.is_none() {
            let id = self.id();
            match self.runner.describe(&self.plugin, id) {
                Ok(info) => self.info = Some(info),
                Err(e) => return Ok(wire::failed(input, &format!("describe: {e}"))),
            }
        }
        let version = self
            .info
            .as_ref()
            .map(|i| i.version.clone())
            .unwrap_or_default();
        let id = self.id();
        Ok(match self.runner.verify(&self.plugin, id, input) {
            Ok(vs) => vs
                .into_iter()
                .map(|mut v| {
                    v.note = format!("({version}) {}", v.note);
                    v
                })
                .collect(),
            Err(e) => wire::failed(input, &e),
        })
    }
}

/// `reviewer` combined with the plugins whose manifests are listed (none:
/// `reviewer` unchanged). Plugins are refused if their directory holds
/// `workspace`. They read only the system's directories plus `PATH`
/// (`read_only`), never what agent commands were additionally granted.
pub fn with_plugins(
    reviewer: Box<dyn Reviewer + Send>,
    manifests: &[PathBuf],
    workspace: &Path,
    path_var: &str,
    read_only: Vec<PathBuf>,
) -> Result<Box<dyn Reviewer + Send>, String> {
    if manifests.is_empty() {
        return Ok(reviewer);
    }
    let runner = Runner::new(path_var.to_string(), read_only);
    let mut all: Vec<Box<dyn Reviewer + Send>> = vec![reviewer];
    for m in manifests {
        let plugin = Plugin::load(m, path_var).map_err(|e| format!("[verify] verifiers: {e}"))?;
        plugin.check_outside(workspace)?;
        all.push(Box::new(PluginVerifier::new(plugin, runner.clone())));
    }
    Ok(Box::new(anchor_verify::Combined::new(all)))
}

/// The review inputs the conformance check sends: copies of the published
/// `docs/verifier/fixtures/` (the files a plugin author tests with), kept
/// in the crate so it builds on its own; a test keeps them identical.
pub const FIXTURES: &[(&str, &str)] = &[
    (
        "passing",
        include_str!("../assets/verifier-fixtures/passing.json"),
    ),
    (
        "failing-test",
        include_str!("../assets/verifier-fixtures/failing-test.json"),
    ),
    (
        "debug-leftover",
        include_str!("../assets/verifier-fixtures/debug-leftover.json"),
    ),
    (
        "truncated",
        include_str!("../assets/verifier-fixtures/truncated.json"),
    ),
];

/// One line of the conformance report.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

fn fixture(name: &str) -> Result<ReviewInput, String> {
    let (_, text) = FIXTURES
        .iter()
        .find(|(n, _)| *n == name)
        .ok_or_else(|| format!("no fixture {name}"))?;
    serde_json::from_str(text).map_err(|e| format!("fixture {name}: {e}"))
}

/// The verdicts exactly as the plugin wrote them (none filled in).
fn raw_verdicts(id: u64, out: &[u8]) -> Result<Vec<ClaimVerdict>, String> {
    match wire::parse_response(id, out)?.msg {
        wire::ResponseMsg::Verdicts(v) => Ok(v.verdicts),
        _ => Err("verify was not answered with verdicts".into()),
    }
}

/// Run the conformance checks of `anchor verifier check` against a
/// plugin. A conforming plugin passes every one; what it decides about
/// each claim is its own business.
pub fn conformance(plugin: &Plugin, runner: &Runner) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut push = |name: &str, r: Result<String, String>| {
        let (ok, detail) = match r {
            Ok(d) => (true, d),
            Err(e) => (false, e),
        };
        checks.push(Check {
            name: name.into(),
            ok,
            detail,
        });
    };
    push(
        "describe: info with protocol 1",
        runner
            .describe(plugin, 3)
            .map(|i| format!("{} {} speaks {:?}", i.name, i.version, i.protocol)),
    );
    let mut id = 100;
    for (name, _) in FIXTURES {
        id += 1;
        let r = fixture(name).and_then(|input| {
            let out = runner.run(plugin, Request::verify(id, input.clone()).encode())?;
            let raw = raw_verdicts(id, &out.stdout)?;
            let checked = wire::check_verdicts(id, &input, &out.stdout)?;
            if raw.len() != input.claims.len() {
                return Err(format!(
                    "{} verdicts for {} claims: answer every claim",
                    raw.len(),
                    input.claims.len()
                ));
            }
            if input.truncated
                && raw
                    .iter()
                    .any(|v| v.verdict == anchor_protocol::verify::Verdict::Supported)
            {
                return Err("supported over a truncated input".into());
            }
            Ok(format!(
                "{} in {} ms",
                checked
                    .iter()
                    .map(|v| format!("{}={:?}", v.claim_id, v.verdict).to_lowercase())
                    .collect::<Vec<_>>()
                    .join(" "),
                out.elapsed.as_millis()
            ))
        });
        push(&format!("verify {name}"), r);
    }
    // Forward compatibility: within version 1, requests may gain optional
    // fields, which a plugin must ignore.
    id += 1;
    let r = fixture("passing").and_then(|input| {
        let mut req =
            serde_json::to_value(Request::verify(id, input.clone())).map_err(|e| e.to_string())?;
        req["msg"]["payload"]["x_future_field"] = serde_json::json!({"a": [1, 2]});
        req["msg"]["payload"]["claims"][0]["x_future_field"] = serde_json::json!("ignored");
        req["x_future_envelope"] = serde_json::json!(true);
        let mut line = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
        line.push(b'\n');
        let out = runner.run(plugin, line)?;
        wire::check_verdicts(id, &input, &out.stdout).map(|_| "answered".to_string())
    });
    push("unknown request fields are ignored", r);
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(s: &str) -> ManifestFile {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn manifests_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        let d = std::fs::canonicalize(dir.path()).unwrap();
        let path = std::env::var("PATH").unwrap_or_default();
        #[cfg(unix)]
        let prog = "sh";
        #[cfg(windows)]
        let prog = "cmd";
        let ok = Plugin::from_manifest(
            manifest(&format!("name = \"x-1\"\ncommand = [\"{prog}\"]")),
            d.clone(),
            &path,
        )
        .unwrap();
        assert!(ok.program.is_absolute());
        assert_eq!(ok.timeout, Duration::from_secs(60));
        for bad in [
            format!("name = \"Bad\"\ncommand = [\"{prog}\"]"),
            "name = \"x\"\ncommand = []".to_string(),
            format!("name = \"x\"\ncommand = [\"{prog}\"]\ntimeout_secs = 0"),
            format!("name = \"x\"\ncommand = [\"{prog}\"]\nmemory_mb = 1"),
            "name = \"x\"\ncommand = [\"../outside\"]".to_string(),
            "name = \"x\"\ncommand = [\"no-such-program-xyz\"]".to_string(),
        ] {
            assert!(
                Plugin::from_manifest(manifest(&bad), d.clone(), &path).is_err(),
                "{bad}"
            );
        }
        assert!(
            toml::from_str::<ManifestFile>("name = \"x\"\ncommand = [\"a\"]\nmore = 1").is_err()
        );
        // A plugin directory that holds the workspace is refused.
        assert!(ok.check_outside(&d.join("ws")).is_err());
        assert!(ok.check_outside(&d).is_err());
        let other = tempfile::tempdir().unwrap();
        ok.check_outside(other.path()).unwrap();
    }

    /// The published fixtures are valid review inputs (they decode with
    /// unknown fields refused and pass validation).
    #[test]
    fn fixtures_are_valid_review_inputs() {
        for (name, _) in FIXTURES {
            let input = fixture(name).unwrap();
            input.validate().unwrap();
            assert!(!input.claims.is_empty(), "{name}");
        }
    }
}
