//! Native Windows/Desktop GUI shell for Anchor (`anchor-desktop.exe`).
//!
//! Runs with `windows_subsystem = "windows"` so no console/terminal window
//! appears. Starts the embedded Anchor backend & workbench engine in-process
//! and opens a dedicated native chromeless desktop application window.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anchor_backend::interact::Scripted;
use anchor_backend::{
    StatePaths, default_exec_read_only, default_state_base, open_workspace, team, ui,
};
use anchor_policy::AuditLog;
use anchor_protocol::ResumeChoice;

fn resolve_workspace() -> Result<PathBuf, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--workspace" {
            if let Some(val) = args.get(i + 1) {
                let p = PathBuf::from(val);
                if p.exists() {
                    return Ok(p);
                }
            }
            i += 2;
            continue;
        }
        if !args[i].starts_with('-') {
            let p = PathBuf::from(&args[i]);
            if p.exists() {
                return Ok(p);
            }
        }
        i += 1;
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let local_ws = exe_dir.join("workspace");
    if local_ws.is_dir() {
        return Ok(local_ws);
    }
    let example = Path::new("examples/ui/workspace");
    if example.is_dir() {
        return Ok(example.to_path_buf());
    }
    std::fs::create_dir_all(&local_ws).map_err(|e| e.to_string())?;
    let _ = std::fs::write(
        local_ws.join("ANCHOR.md"),
        "---\ntrigger: always_on\n---\n# Anchor Desktop Harness Rules\n- Maintain Swiss-Industrial Precision.\n- Enforce read-before-write and capability-floor sandboxing.\n",
    );
    let _ = std::fs::write(
        local_ws.join("calc.py"),
        "def clamp(x: int, lo: int, hi: int) -> int:\n    \"\"\"Clamp x into the inclusive range [lo, hi].\"\"\"\n    if lo > hi:\n        raise ValueError(\"lo must not exceed hi\")\n    return max(lo, min(hi, x))\n",
    );
    Ok(local_ws)
}

fn launch_native_window(url: &str) {
    #[cfg(target_os = "windows")]
    {
        let candidates = [
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ];
        let profile_dir = std::env::temp_dir().join("anchor-desktop-gui-profile");
        let _ = std::fs::create_dir_all(&profile_dir);
        for bin in candidates {
            if Path::new(bin).exists()
                && std::process::Command::new(bin)
                    .arg(format!("--app={url}"))
                    .arg(format!("--user-data-dir={}", profile_dir.display()))
                    .arg("--window-size=1560,940")
                    .arg("--no-first-run")
                    .arg("--no-default-browser-check")
                    .spawn()
                    .is_ok()
            {
                return;
            }
        }
        let _ = std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = url;
    }
}

fn run_desktop() -> Result<(), String> {
    let ws = resolve_workspace()?;
    let deny = Box::new(Scripted::new(false, ResumeChoice::FreshSession));
    let (src, mediator) = match open_workspace(&ws, None, deny, default_exec_read_only()) {
        Ok(opened) => {
            let audit = opened.mediator.engine().audit_arc();
            let root = opened.mediator.workspace().root().to_path_buf();
            let hub = opened
                .paths
                .dir
                .join("hub.sqlite")
                .exists()
                .then(|| team::open_hub(&opened.paths, &root, audit.clone()))
                .transpose()?;
            (
                ui::Sources {
                    workspace: root,
                    audit,
                    hub,
                    store: Some(opened.mediator.shared_store()),
                    store_note: None,
                    broker: None,
                    mode: "inspect",
                },
                Some(opened.mediator),
            )
        }
        Err(_) => {
            let w = anchor_fs::Workspace::open(&ws).map_err(|e| e.to_string())?;
            let paths =
                StatePaths::create(w.root(), &default_state_base()).map_err(|e| e.to_string())?;
            let audit = Arc::new(AuditLog::open(&paths.audit).map_err(|e| e.to_string())?);
            let hub = paths
                .dir
                .join("hub.sqlite")
                .exists()
                .then(|| team::open_hub(&paths, w.root(), audit.clone()))
                .transpose()?;
            (
                ui::Sources {
                    workspace: w.root().to_path_buf(),
                    audit,
                    hub,
                    store: None,
                    store_note: Some("workspace snapshot store is held by another process".into()),
                    broker: None,
                    mode: "inspect",
                },
                None,
            )
        }
    };

    let preferred_port = if std::net::TcpListener::bind(("127.0.0.1", 3210)).is_ok() {
        3210
    } else {
        0
    };
    let server = ui::start(src, preferred_port).map_err(|e| e.to_string())?;
    if let Some(m) = mediator {
        server.attach_workspace(m);
    }
    let url = server.url();
    launch_native_window(&url);
    server.wait();
    Ok(())
}

fn main() -> ExitCode {
    match run_desktop() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
