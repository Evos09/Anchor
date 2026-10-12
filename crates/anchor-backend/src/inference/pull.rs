//! Downloading a local model from the app (0.12.0, ADR-0040): Ollama's
//! own `POST /api/pull` on the loopback port, streamed as NDJSON progress
//! lines. Only models the registry lists for Ollama can be pulled, so the
//! app never fetches an arbitrary name and every model it offers has had
//! its license recorded.

use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anchor_inference::Registry;
use serde_json::{Value, json};

/// How long connecting to Ollama may take (the pull itself has no limit:
/// a model is gigabytes).
const CONNECT: Duration = Duration::from_secs(5);
/// Longest progress line read.
const MAX_LINE: usize = 64 * 1024;

/// One progress line, as Ollama reports it.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct PullProgress {
    pub status: String,
    pub total: u64,
    pub completed: u64,
}

/// Is `model` a registry model Ollama can serve? The error says why not.
pub fn allowed(registry: &Registry, model: &str) -> Result<(), String> {
    if registry
        .models
        .iter()
        .any(|m| m.provider == "ollama" && m.provider_model_id == model)
    {
        Ok(())
    } else {
        Err(format!(
            "{model:?} is not one of the local models Anchor lists; only those can be downloaded here"
        ))
    }
}

/// Pull `model` through Ollama on `port`, reporting each progress line.
/// Returns when Ollama says `success`, on its error, or soon after
/// `cancel` is set (the connection is dropped, which stops the pull).
pub fn pull(
    port: u16,
    model: &str,
    progress: &dyn Fn(&PullProgress),
    cancel: &AtomicBool,
) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT))
        .http_status_as_error(false)
        // A proxy can never be the way to 127.0.0.1.
        .proxy(None)
        .build()
        .into();
    let r = agent
        .post(format!("http://127.0.0.1:{port}/api/pull"))
        .send_json(json!({ "model": model, "stream": true }))
        .map_err(|_| format!("Ollama is not answering on port {port}"))?;
    let status = r.status().as_u16();
    let mut lines = BufReader::new(r.into_body().into_reader());
    if status != 200 {
        let mut first = String::new();
        let _ = lines.read_line(&mut first);
        let why = serde_json::from_str::<Value>(&first)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| format!("Ollama answered {status}"));
        return Err(why);
    }
    let mut last = PullProgress::default();
    let mut buf = Vec::new();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        buf.clear();
        let n = std::io::Read::take(&mut lines, MAX_LINE as u64)
            .read_until(b'\n', &mut buf)
            .map_err(|e| format!("the download stopped: {e}"))?;
        if n == 0 {
            return if last.status == "success" {
                Ok(())
            } else {
                Err("Ollama ended the download early".into())
            };
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value =
            serde_json::from_str(line).map_err(|_| "Ollama sent something unreadable")?;
        if let Some(e) = v.get("error").and_then(Value::as_str) {
            return Err(e.to_string());
        }
        let status = v
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // Ollama reports each layer's bytes; a line without them keeps
        // the last numbers so the bar does not jump back to zero.
        let (total, completed) = match v.get("total").and_then(Value::as_u64) {
            Some(t) => (t, v.get("completed").and_then(Value::as_u64).unwrap_or(0)),
            None => (last.total, last.completed),
        };
        last = PullProgress {
            status,
            total,
            completed,
        };
        progress(&last);
        if last.status == "success" {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    /// A fake Ollama: answers one `/api/pull` with `lines` (NDJSON),
    /// pausing `gap` between them.
    pub(crate) fn fake_pull(status: u16, lines: Vec<String>, gap: Duration) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                // Read the whole request (headers and body) first.
                let mut req = Vec::new();
                let mut b = [0u8; 4096];
                loop {
                    let n = s.read(&mut b).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&b[..n]);
                    let text = String::from_utf8_lossy(&req);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|h| {
                                let (k, v) = h.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if req.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n"
                );
                for line in &lines {
                    if writeln!(s, "{line}").is_err() {
                        break;
                    }
                    let _ = s.flush();
                    std::thread::sleep(gap);
                }
                let _ = s.shutdown(std::net::Shutdown::Write);
            }
        });
        port
    }

    #[test]
    fn progress_is_reported_until_success() {
        let port = fake_pull(
            200,
            vec![
                r#"{"status":"pulling manifest"}"#.into(),
                r#"{"status":"pulling abc","digest":"abc","total":100,"completed":40}"#.into(),
                r#"{"status":"verifying sha256 digest"}"#.into(),
                r#"{"status":"success"}"#.into(),
            ],
            Duration::ZERO,
        );
        let seen = Mutex::new(Vec::new());
        pull(
            port,
            "qwen2.5-coder:7b",
            &|p| seen.lock().unwrap().push(p.clone()),
            &AtomicBool::new(false),
        )
        .unwrap();
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.len(), 4);
        assert_eq!((seen[1].total, seen[1].completed), (100, 40));
        assert_eq!((seen[2].total, seen[2].completed), (100, 40), "kept");
        assert_eq!(seen[3].status, "success");
    }

    #[test]
    fn an_error_line_or_status_is_the_error() {
        let port = fake_pull(
            200,
            vec![
                r#"{"status":"pulling manifest"}"#.into(),
                r#"{"error":"pull model manifest: file does not exist"}"#.into(),
            ],
            Duration::ZERO,
        );
        let e = pull(port, "m", &|_| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(e.contains("file does not exist"), "{e}");
        let port = fake_pull(500, vec![r#"{"error":"disk full"}"#.into()], Duration::ZERO);
        assert_eq!(
            pull(port, "m", &|_| {}, &AtomicBool::new(false)).unwrap_err(),
            "disk full"
        );
        let port = fake_pull(200, vec![r#"{"status":"pulling"}"#.into()], Duration::ZERO);
        assert!(
            pull(port, "m", &|_| {}, &AtomicBool::new(false))
                .unwrap_err()
                .contains("early")
        );
    }

    #[test]
    fn a_pull_can_be_cancelled() {
        let lines = (0..200)
            .map(|i| format!(r#"{{"status":"pulling","total":200,"completed":{i}}}"#))
            .collect();
        let port = fake_pull(200, lines, Duration::from_millis(20));
        let cancel = AtomicBool::new(false);
        let e = pull(
            port,
            "m",
            &|p| {
                if p.completed >= 3 {
                    cancel.store(true, Ordering::SeqCst);
                }
            },
            &cancel,
        )
        .unwrap_err();
        assert_eq!(e, "cancelled");
    }

    #[test]
    fn only_registry_models_for_ollama_are_allowed() {
        let r = crate::inference::bundled_registry().unwrap();
        assert!(allowed(&r, "qwen2.5-coder:7b").is_ok());
        assert!(allowed(&r, "llama3.1:8b").is_ok());
        assert!(allowed(&r, "evil/model:latest").is_err());
        assert!(allowed(&r, "").is_err());
    }
}
