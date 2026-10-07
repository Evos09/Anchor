//! `anchor eval run`: the seeded-bug harness (spec §8.5, §16.3).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anchor_backend::eval::{self, Calibration, Corpus};
use anchor_backend::interact::Scripted;
use anchor_backend::verify::{BoxedReviewer, ModelReviewer, VerifyGate};
use anchor_backend::{default_exec_read_only, open_workspace_in, session};
use anchor_protocol::ResumeChoice;
use anchor_protocol::roles::{RolesView, Route};

use crate::Args;

fn read(p: &str) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))
}

pub fn cmd_eval(a: &Args) -> Result<ExitCode, String> {
    if a.pos.get(1).map(String::as_str) != Some("run") {
        return Err("usage: anchor eval run --corpus DIR [options]".into());
    }
    let mut corpus = Corpus::load(Path::new(a.need("--corpus")?))?;
    if let Some(only) = a.get("--only") {
        let names: Vec<String> = only.split(',').map(|s| s.trim().to_string()).collect();
        corpus.retain(&names)?;
    }
    let max_fn = a
        .get("--max-fn-rate")
        .map(|x| x.parse::<f64>().map_err(|e| format!("--max-fn-rate: {e}")))
        .transpose()?;

    eprintln!("anchor: checking the corpus ({} tasks)", corpus.tasks.len());
    let problems = eval::self_check(&corpus)?;
    if !problems.is_empty() {
        return Err(format!(
            "invalid corpus: its ground truth does not hold:\n  {}",
            problems.join("\n  ")
        ));
    }

    let state = std::env::temp_dir().join(format!("anchor-eval-state-{}", std::process::id()));
    let oracle = Arc::new(AtomicBool::new(false));
    let mut usage = None;
    let kind = a.get("--reviewer").unwrap_or("rule");
    let reviewer: BoxedReviewer = match kind {
        "rule" => anchor_backend::verify::rule_reviewer(),
        "accept-all" => Box::new(Calibration::AcceptAll),
        "oracle" => Box::new(Calibration::Oracle(oracle.clone())),
        "model" => {
            let roles = read(a.need("--reviewer-roles")?)?;
            let executor_families = match a.get("--executor-roles") {
                Some(p) => toml::from_str::<RolesView>(&read(p)?)
                    .map_err(|e| format!("{p}: {e}"))?
                    .families(Route::Agentic),
                None => Vec::new(),
            };
            // Review sessions get no tools: an empty workspace serves.
            let ws = state.join("reviewer-ws");
            std::fs::create_dir_all(&ws).map_err(|e| e.to_string())?;
            let opened = open_workspace_in(
                &ws,
                &state,
                None,
                Box::new(Scripted::new(false, ResumeChoice::FreshSession)),
                vec![],
            )
            .map_err(|e| e.to_string())?;
            let checkpoints = anchor_checkpoint::CheckpointStore::open(&opened.paths.checkpoints)
                .map_err(|e| e.to_string())?;
            let m = ModelReviewer::new(
                opened.mediator,
                checkpoints,
                a.get("--agent")
                    .map(PathBuf::from)
                    .unwrap_or_else(session::default_agent_bin),
                roles,
                a.get("--mock-reviewer").map(read).transpose()?,
                executor_families,
            )?;
            usage = Some(m.usage.clone());
            Box::new(m)
        }
        other => {
            return Err(format!(
                "--reviewer {other:?}: use rule, model, accept-all or oracle"
            ));
        }
    };
    // `--verifier MANIFEST` (repeatable): measure a verifier plugin on the
    // corpus, combined with the reviewer above (Phase 8).
    let manifests: Vec<PathBuf> = a.all("--verifier").into_iter().map(PathBuf::from).collect();
    let path_var = std::env::var("PATH").unwrap_or_default();
    let reviewer = anchor_backend::verifier::with_plugins(
        reviewer,
        &manifests,
        &corpus.dir,
        &path_var,
        default_exec_read_only(),
    )?;
    let mut gate = VerifyGate::new(
        corpus.spec.clone(),
        anchor_backend::verify::scratch_base(&state, &corpus.dir),
        None,
        reviewer,
        std::env::var("PATH").unwrap_or_default(),
        default_exec_read_only(),
    )?;
    let oracle_ref = (kind == "oracle").then_some(&*oracle);
    let report = eval::run(&corpus, &mut gate, oracle_ref, usage);
    let _ = std::fs::remove_dir_all(&state);
    let report = report?;

    let md = report.to_markdown();
    print!("{md}");
    if let Some(p) = a.get("--json") {
        let j = serde_json::to_string_pretty(&eval::report_json(&report)).unwrap_or_default();
        std::fs::write(p, j + "\n").map_err(|e| format!("{p}: {e}"))?;
    }
    if let Some(p) = a.get("--md") {
        std::fs::write(p, md).map_err(|e| format!("{p}: {e}"))?;
    }
    if let (Some(max), Some(fnr)) = (max_fn, report.gate.fn_rate())
        && fnr > max
    {
        eprintln!(
            "anchor: false-negative rate {:.1}% exceeds --max-fn-rate {:.1}%: \
             STOP and redesign the reviewer (spec §8.5)",
            fnr * 100.0,
            max * 100.0
        );
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// `anchor verifier check MANIFEST`: run the conformance checks against a
/// verifier plugin, in the same sandbox the merge gate uses.
pub fn cmd_verifier(a: &Args) -> Result<ExitCode, String> {
    let (Some("check"), Some(manifest)) = (
        a.pos.get(1).map(String::as_str),
        a.pos.get(2).map(String::as_str),
    ) else {
        return Err("usage: anchor verifier check MANIFEST".into());
    };
    use anchor_backend::verifier::{Plugin, Runner, conformance};
    let path_var = std::env::var("PATH").unwrap_or_default();
    let plugin = Plugin::load(Path::new(manifest), &path_var)?;
    let runner = Runner::new(path_var, default_exec_read_only());
    println!(
        "verifier {} ({})",
        plugin.name,
        anchor_exec::native_path(&plugin.program).display()
    );
    let checks = conformance(&plugin, &runner);
    for c in &checks {
        println!(
            "  {} {:<36} {}",
            if c.ok { "ok  " } else { "FAIL" },
            c.name,
            c.detail
        );
    }
    let failed = checks.iter().filter(|c| !c.ok).count();
    if failed == 0 {
        println!("conforms to verifier protocol 1 ({} checks)", checks.len());
        Ok(ExitCode::SUCCESS)
    } else {
        println!("{failed} of {} checks failed", checks.len());
        Ok(ExitCode::FAILURE)
    }
}
