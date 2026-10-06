//! Embed the dashboard's built frontend (`ui/dist`, produced by
//! `npm --prefix ui run build`) into the binary. Without it, a one-page
//! notice is embedded instead, so the backend builds without Node.

use std::path::{Path, PathBuf};

fn mime(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, e)| e) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let p = format!("{rel}/{name}");
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(&e.path(), &p, out);
        } else if ft.is_file() && !name.ends_with(".map") {
            out.push((p, e.path()));
        }
    }
}

const FALLBACK: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Anchor dashboard</title></head><body style=\"font-family: system-ui, sans-serif; max-width: 40rem; margin: 3rem auto; padding: 0 1rem\">\
<h1>Anchor dashboard</h1><p>This <code>anchor</code> binary was built without the dashboard's frontend.</p>\
<p>Build it with <code>npm --prefix ui ci &amp;&amp; npm --prefix ui run build</code>, then rebuild <code>anchor</code>.</p>\
<p>The JSON API under <code>/api/</code> works regardless.</p></body></html>";

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let dist = manifest.join("../../ui/dist");
    // The frontend build writes this stamp last, so a rebuilt frontend
    // re-runs this script.
    println!("cargo:rerun-if-changed={}", dist.join(".stamp").display());
    let mut files = Vec::new();
    walk(&dist, "", &mut files);
    let has_index = files.iter().any(|(p, _)| p == "/index.html");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let mut code = String::from("pub static ASSETS: &[(&str, &str, &[u8])] = &[\n");
    if has_index {
        for (p, path) in &files {
            if p == "/.stamp" {
                continue;
            }
            println!("cargo:rerun-if-changed={}", path.display());
            code.push_str(&format!(
                "    ({p:?}, {:?}, include_bytes!({:?})),\n",
                mime(p),
                path.display().to_string()
            ));
        }
    } else {
        let f = out.join("fallback.html");
        std::fs::write(&f, FALLBACK).unwrap_or_default();
        code.push_str(&format!(
            "    (\"/index.html\", \"text/html; charset=utf-8\", include_bytes!({:?})),\n",
            f.display().to_string()
        ));
    }
    code.push_str("];\n");
    let target = out.join("ui_assets.rs");
    if std::fs::read_to_string(&target).ok().as_deref() != Some(code.as_str()) {
        std::fs::write(&target, code).unwrap_or_default();
    }
}
