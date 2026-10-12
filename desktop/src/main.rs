//! Anchor's desktop app (ADR-0028): one window over the loopback app that
//! the bundled `anchor` backend serves (`anchor app`).
//!
//! The shell holds no authority of its own. It has no Tauri commands, no
//! plugins and no capabilities, so the page cannot reach any IPC; the page
//! talks only to the backend's token-gated, same-origin HTTP surface, the
//! same one the browser and the tests use. Navigation is pinned to that
//! one origin, new windows are refused, and the backend is stopped when
//! the window closes.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::webview::NewWindowResponse;
use tauri::{RunEvent, Url, WebviewUrl, WebviewWindowBuilder};

/// How long the backend may take to print its URL.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// The backend's URL: `http://127.0.0.1:<port>/#t=<64 hex>`. Anything else
/// is refused, so the window can only ever open the loopback app.
fn parse_url(s: &str) -> Option<(Url, u16)> {
    let url = Url::parse(s.trim()).ok()?;
    let token = url.fragment()?.strip_prefix("t=")?;
    let ok = url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.path() == "/"
        && url.query().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && token.len() == 64
        && token
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    let port = url.port()?;
    ok.then_some((url, port))
}

/// Navigation stays on the backend's origin.
fn allowed(url: &Url, port: u16) -> bool {
    url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.port() == Some(port)
}

/// The bundled backend: next to this executable (Tauri's `externalBin`),
/// or `ANCHOR_BACKEND` when developing.
fn backend_path() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("ANCHOR_BACKEND") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate the app: {e}"))?;
    let dir = exe.parent().ok_or("cannot locate the app's folder")?;
    let name = if cfg!(windows) {
        "anchor.exe"
    } else {
        "anchor"
    };
    let p = dir.join(name);
    if p.is_file() {
        Ok(p)
    } else {
        Err(format!("the Anchor backend is missing: {}", p.display()))
    }
}

/// A private directory for the URL hand-over (the URL carries the token).
fn private_dir() -> Result<PathBuf, String> {
    let mut seed = [0u8; 8];
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    seed.copy_from_slice(&(nanos as u64 ^ u64::from(std::process::id())).to_le_bytes());
    let name = format!(
        "anchor-desktop-{}-{}",
        std::process::id(),
        seed.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    let dir = std::env::temp_dir().join(name);
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    // A fresh directory, never an existing one.
    b.create(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

struct Backend {
    child: Child,
    url: Url,
    port: u16,
    dir: PathBuf,
    started: Duration,
}

impl Backend {
    fn start() -> Result<Backend, String> {
        let t0 = Instant::now();
        let bin = backend_path()?;
        let dir = private_dir()?;
        let url_file = dir.join("url");
        let log = std::fs::File::create(dir.join("backend.log"))
            .map_err(|e| format!("backend log: {e}"))?;
        let mut cmd = Command::new(&bin);
        // Its stdin is a pipe only this process holds: if the shell dies
        // without stopping it, the pipe closes and the backend exits.
        // The backend compares this with the latest release (Settings →
        // General → Check for updates).
        cmd.env("ANCHOR_APP_VERSION", env!("CARGO_PKG_VERSION"));
        cmd.arg("app")
            .arg("--exit-with-stdin")
            .arg("--ui-url-file")
            .arg(&url_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window for the backend.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", bin.display()))?;
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Ok(s) = std::fs::read_to_string(&url_file)
                && let Some((url, port)) = parse_url(&s)
            {
                return Ok(Backend {
                    child,
                    url,
                    port,
                    dir,
                    started: t0.elapsed(),
                });
            }
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!(
                    "the backend stopped ({status}):\n{}",
                    tail(&dir.join("backend.log"))
                ));
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                return Err(format!(
                    "the backend did not start within {} s:\n{}",
                    START_TIMEOUT.as_secs(),
                    tail(&dir.join("backend.log"))
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn tail(p: &Path) -> String {
    let s = std::fs::read_to_string(p).unwrap_or_default();
    let start = s.len().saturating_sub(2000);
    let start = (start..s.len())
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(0);
    s[start..].to_string()
}

/// `--smoke`: start the backend, ask it for its overview with the token,
/// stop it, and report. CI runs this on every platform after bundling.
fn smoke() -> Result<String, String> {
    let mut b = Backend::start()?;
    let token = b
        .url
        .fragment()
        .and_then(|f| f.strip_prefix("t="))
        .unwrap_or("");
    let r = (|| {
        let mut s = TcpStream::connect(("127.0.0.1", b.port)).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        write!(
            s,
            "GET /api/overview HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {token}\r\n\r\n",
            b.port
        )
        .map_err(|e| e.to_string())?;
        let mut out = String::new();
        s.read_to_string(&mut out).map_err(|e| e.to_string())?;
        if out.starts_with("HTTP/1.1 200") && out.contains("\"mode\":\"app\"") {
            Ok(format!(
                "smoke: ok: the backend served the app on port {} after {} ms",
                b.port,
                b.started.as_millis()
            ))
        } else {
            Err(format!(
                "unexpected answer: {}",
                out.lines().next().unwrap_or("")
            ))
        }
    })();
    b.stop();
    r
}

/// Shown in the window when the backend cannot start (no URL to open).
fn error_page(msg: &str) -> String {
    let esc = msg
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<!doctype html><meta charset=utf-8><title>Anchor</title>\
         <body><h1>Anchor could not start</h1>\
         <p>Its backend did not come up. Details:</p><pre>{esc}</pre>\
         <p>Reinstalling the app usually fixes a missing backend. Please report anything else at \
         github.com/evos09/anchor/issues.</p></body>"
    )
}

fn main() {
    if std::env::args().any(|a| a == "--smoke") {
        let r = smoke();
        let line = match &r {
            Ok(m) => m.clone(),
            Err(e) => format!("smoke: FAILED: {e}"),
        };
        // A Windows GUI build has no console to print to.
        if let Some(f) = std::env::var_os("ANCHOR_SMOKE_OUT") {
            let _ = std::fs::write(f, format!("{line}\n"));
        }
        println!("{line}");
        std::process::exit(if r.is_ok() { 0 } else { 1 });
    }
    let backend = Backend::start();
    let (url, port, err) = match &backend {
        Ok(b) => (Some(b.url.clone()), b.port, None),
        Err(e) => (None, 0, Some(e.clone())),
    };
    let backend = Arc::new(Mutex::new(backend.ok()));
    let app = tauri::Builder::default()
        .setup(move |app| {
            let w = match &url {
                Some(u) => WebviewWindowBuilder::new(app, "main", WebviewUrl::External(u.clone()))
                    .on_navigation(move |u| allowed(u, port))
                    .on_new_window(|_, _| NewWindowResponse::Deny),
                None => {
                    let page = error_page(err.as_deref().unwrap_or("unknown error"));
                    WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                        .initialization_script(format!(
                            "document.addEventListener('DOMContentLoaded',()=>{{document.open();document.write({});document.close();}});",
                            serde_json_string(&page)
                        ))
                        .on_navigation(|u| u.scheme() == "tauri" || u.host_str() == Some("tauri.localhost"))
                        .on_new_window(|_, _| NewWindowResponse::Deny)
                }
            };
            w.title("Anchor").inner_size(1200.0, 820.0).min_inner_size(640.0, 480.0).build()?;
            Ok(())
        })
        .build(tauri::generate_context!());
    let app = match app {
        Ok(a) => a,
        Err(e) => {
            eprintln!("anchor-desktop: {e}");
            if let Some(b) = backend.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
                b.stop();
            }
            std::process::exit(1);
        }
    };
    app.run(move |_, event| {
        if let RunEvent::Exit = event
            && let Some(b) = backend.lock().unwrap_or_else(|p| p.into_inner()).as_mut()
        {
            b.stop();
        }
    });
}

/// `s` as a JavaScript string literal.
fn serde_json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn only_the_loopback_app_url_is_accepted() {
        let (u, port) = parse_url(&format!("http://127.0.0.1:4321/#t={T}\n")).unwrap();
        assert_eq!(port, 4321);
        assert_eq!(u.fragment(), Some(format!("t={T}").as_str()));
        for bad in [
            format!("https://127.0.0.1:4321/#t={T}"),
            format!("http://localhost:4321/#t={T}"),
            format!("http://evil.example:4321/#t={T}"),
            format!("http://127.0.0.1:4321/x#t={T}"),
            format!("http://127.0.0.1:4321/?a=1#t={T}"),
            format!("http://u:p@127.0.0.1:4321/#t={T}"),
            format!("http://127.0.0.1/#t={T}"),
            format!("http://127.0.0.1:4321/#t={}", &T[..63]),
            format!("http://127.0.0.1:4321/#t={}", T.to_uppercase()),
            "http://127.0.0.1:4321/".into(),
            "file:///etc/passwd".into(),
        ] {
            assert!(parse_url(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn navigation_stays_on_the_backend_origin() {
        let ok = |s: &str| allowed(&Url::parse(s).unwrap(), 4321);
        assert!(ok("http://127.0.0.1:4321/"));
        assert!(ok("http://127.0.0.1:4321/assets/x.js"));
        assert!(!ok("http://127.0.0.1:4322/"));
        assert!(!ok("http://localhost:4321/"));
        assert!(!ok("https://127.0.0.1:4321/"));
        assert!(!ok("https://example.com/"));
        assert!(!ok("file:///etc/passwd"));
    }

    /// The page gets no IPC: no capabilities, no global Tauri object.
    /// (Tauri also refuses every command from a remote origin that no
    /// capability names: its `remote_context_denied` test.)
    #[test]
    fn the_shell_grants_the_page_nothing() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let conf: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("tauri.conf.json")).unwrap())
                .unwrap();
        assert_eq!(conf["app"]["withGlobalTauri"], false);
        assert_eq!(
            conf["app"]["security"]["capabilities"],
            serde_json::json!([])
        );
        assert_eq!(conf["app"]["windows"], serde_json::json!([]));
        assert!(conf["plugins"].as_object().is_none_or(|p| p.is_empty()));
        assert!(!root.join("capabilities").exists());
    }

    #[test]
    fn error_text_cannot_break_out_of_the_page() {
        let p = error_page("<script>alert(1)</script> & more");
        assert!(!p.contains("<script>"));
        let js = serde_json_string("</script>\u{2028}\"\\");
        assert!(!js.contains("</"));
        assert!(js.contains("\\u2028"));
    }
}
