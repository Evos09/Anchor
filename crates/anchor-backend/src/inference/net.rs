//! The app's few network calls, all on the user's behalf and all made
//! from the settings page or just before a task:
//!
//! * probing Ollama on the loopback port (Path C);
//! * one verify call with a newly added key, to that key's own provider
//!   (Path B): `GET {base_url}/models`, which also lists the models the
//!   key can use;
//! * registering with, and reading the meters of, the free tier (Path A).
//!
//! No call carries project content, and errors never carry a key.

use std::time::Duration;

use serde_json::{Value, json};

/// The default Ollama port (`OLLAMA_HOST` overrides it for Ollama itself;
/// Anchor's setting is separate).
pub const OLLAMA_PORT: u16 = 11434;

fn agent(timeout: Duration, loopback: bool) -> ureq::Agent {
    let mut b = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false);
    if loopback {
        // A proxy can never be the way to 127.0.0.1.
        b = b.proxy(None);
    }
    b.build().into()
}

/// An installed local model, as Ollama lists it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LocalModel {
    pub name: String,
    pub size: u64,
    /// Ollama's own family label (a hint only: it does not decide the
    /// reviewer-family rule, the registry or the user does).
    pub ollama_family: String,
    pub parameter_size: String,
    pub quantization: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Ollama {
    pub port: u16,
    pub version: String,
    pub models: Vec<LocalModel>,
}

/// Is Ollama answering on `port`, and with which models?
pub fn detect_ollama(port: u16, timeout: Duration) -> Result<Ollama, String> {
    let a = agent(timeout, true);
    let base = format!("http://127.0.0.1:{port}");
    let get = |path: &str| -> Result<Value, String> {
        let r = a
            .get(format!("{base}{path}"))
            .call()
            .map_err(|_| format!("nothing is answering on port {port}"))?;
        if r.status().as_u16() != 200 {
            return Err(format!("port {port} answered {}", r.status().as_u16()));
        }
        r.into_body()
            .with_config()
            .limit(4 * 1024 * 1024)
            .read_json()
            .map_err(|_| format!("port {port} is not Ollama"))
    };
    let version = get("/api/version")?
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("port {port} is not Ollama"))?
        .to_string();
    let tags = get("/api/tags")?;
    let s = |v: &Value, p: &str| {
        v.pointer(p)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let models = tags
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let name = m.get("name").and_then(Value::as_str)?.to_string();
            Some(LocalModel {
                name,
                size: m.get("size").and_then(Value::as_u64).unwrap_or(0),
                ollama_family: s(m, "/details/family"),
                parameter_size: s(m, "/details/parameter_size"),
                quantization: s(m, "/details/quantization_level"),
            })
        })
        .collect();
    Ok(Ollama {
        port,
        version,
        models,
    })
}

/// Check a key with its provider and list the models it can use.
pub fn verify_key(base_url: &str, key: &str, timeout: Duration) -> Result<Vec<String>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let r = agent(timeout, false)
        .get(&url)
        .header("Authorization", format!("Bearer {key}"))
        .call()
        .map_err(|_| "could not reach the provider; check your connection".to_string())?;
    match r.status().as_u16() {
        200 => {}
        401 | 403 => return Err("the provider did not accept this key".to_string()),
        429 => {
            return Err(
                "the provider is rate-limiting this key right now; try again in a minute"
                    .to_string(),
            );
        }
        s => return Err(format!("the provider answered {s}; the key was not saved")),
    }
    let v: Value = r
        .into_body()
        .with_config()
        .limit(8 * 1024 * 1024)
        .read_json()
        .map_err(|_| "the provider's model list was not readable".to_string())?;
    let mut ids: Vec<String> = v
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
        // Some providers prefix ids ("models/…"); the chat API takes them as listed.
        .map(str::to_string)
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// The free tier's answer to a call that failed.
#[derive(Debug, Clone, PartialEq)]
pub struct FreeError {
    pub status: u16,
    pub kind: String,
    pub message: String,
}

fn free_call(
    url: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
    timeout: Duration,
) -> Result<Value, FreeError> {
    let a = agent(timeout, false);
    let full = format!("{}{path}", url.trim_end_matches('/'));
    let unreachable = || FreeError {
        status: 0,
        kind: "unreachable".into(),
        message: "the free tier could not be reached".into(),
    };
    let r = if method == "POST" {
        let mut req = a.post(&full);
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        req.send_json(body.unwrap_or(json!({})))
    } else {
        let mut req = a.get(&full);
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        req.call()
    }
    .map_err(|_| unreachable())?;
    let status = r.status().as_u16();
    let v: Value = r
        .into_body()
        .with_config()
        .limit(1024 * 1024)
        .read_json()
        .map_err(|_| unreachable())?;
    if status == 200 {
        Ok(v)
    } else {
        Err(FreeError {
            status,
            kind: v
                .pointer("/error/type")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .into(),
            message: v
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("the free tier refused the request")
                .into(),
        })
    }
}

/// Register this install (no signup: a random device id, ADR-0037).
pub fn free_register(url: &str, device: &str, timeout: Duration) -> Result<String, FreeError> {
    let v = free_call(
        url,
        "POST",
        "/v1/anchor/register",
        None,
        Some(json!({ "device": device })),
        timeout,
    )?;
    v.get("token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(FreeError {
            status: 200,
            kind: "bad_response".into(),
            message: "the free tier did not issue a token".into(),
        })
}

/// The three meters and the pool's state.
pub fn free_usage(url: &str, token: &str, timeout: Duration) -> Result<Value, FreeError> {
    free_call(url, "GET", "/v1/anchor/usage", Some(token), None, timeout)
}

/// The models the free tier serves now.
pub fn free_models(url: &str, token: &str, timeout: Duration) -> Result<Value, FreeError> {
    free_call(url, "GET", "/v1/models", Some(token), None, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// A one-request-per-connection HTTP server: `answer(request line,
    /// authorization header)` gives (status, JSON body).
    fn serve(answer: impl Fn(&str, &str) -> (u16, String) + Send + 'static) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut first = String::new();
                r.read_line(&mut first).unwrap();
                let (mut auth, mut len) = (String::new(), 0usize);
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap_or(("", ""));
                    match k.to_ascii_lowercase().as_str() {
                        "authorization" => auth = v.trim().to_string(),
                        "content-length" => len = v.trim().parse().unwrap(),
                        _ => {}
                    }
                }
                let mut body = vec![0u8; len];
                std::io::Read::read_exact(&mut r, &mut body).unwrap();
                let (status, out) = answer(first.trim(), &auth);
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                    out.len()
                );
            }
        });
        port
    }

    #[test]
    fn ollama_is_detected_with_its_models() {
        let port = serve(|line, _| {
            match line {
            l if l.starts_with("GET /api/version") => (200, r#"{"version":"0.12.3"}"#.into()),
            l if l.starts_with("GET /api/tags") => (
                200,
                r#"{"models":[{"name":"qwen2.5-coder:7b","model":"qwen2.5-coder:7b","size":4683087332,
                   "details":{"family":"qwen2","parameter_size":"7.6B","quantization_level":"Q4_K_M"}}]}"#
                    .into(),
            ),
            _ => (404, "{}".into()),
        }
        });
        let o = detect_ollama(port, Duration::from_secs(5)).unwrap();
        assert_eq!(o.version, "0.12.3");
        assert_eq!(o.models.len(), 1);
        assert_eq!(o.models[0].name, "qwen2.5-coder:7b");
        assert_eq!(o.models[0].ollama_family, "qwen2");
        assert_eq!(o.models[0].parameter_size, "7.6B");
    }

    #[test]
    fn a_closed_port_or_another_server_is_not_ollama() {
        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(
            detect_ollama(closed, Duration::from_secs(2))
                .unwrap_err()
                .contains("nothing is answering")
        );
        let other = serve(|_, _| (200, r#"{"hello":"world"}"#.into()));
        assert!(
            detect_ollama(other, Duration::from_secs(5))
                .unwrap_err()
                .contains("not Ollama")
        );
    }

    #[test]
    fn a_key_is_verified_with_its_own_provider_and_never_echoed() {
        let port = serve(|line, auth| {
            assert!(line.starts_with("GET /v1/models"), "{line}");
            if auth == "Bearer sk-good" {
                (
                    200,
                    r#"{"data":[{"id":"model-b"},{"id":"model-a"}]}"#.into(),
                )
            } else {
                (401, r#"{"error":{"message":"bad key sk-bad"}}"#.into())
            }
        });
        let base = format!("http://127.0.0.1:{port}/v1");
        assert_eq!(
            verify_key(&base, "sk-good", Duration::from_secs(5)).unwrap(),
            ["model-a", "model-b"]
        );
        let e = verify_key(&base, "sk-bad", Duration::from_secs(5)).unwrap_err();
        assert_eq!(e, "the provider did not accept this key");
    }

    #[test]
    fn the_free_tier_client_registers_and_reads_meters() {
        let port = serve(|line, auth| match line {
            l if l.starts_with("POST /v1/anchor/register") => {
                (200, r#"{"token":"t-1","user_id":"u"}"#.into())
            }
            l if l.starts_with("GET /v1/anchor/usage") && auth == "Bearer t-1" => {
                (200, r#"{"now_ms":1,"caps":{"blocked_by":null}}"#.into())
            }
            _ => (
                401,
                r#"{"error":{"type":"anchor_auth","message":"unknown token"}}"#.into(),
            ),
        });
        let url = format!("http://127.0.0.1:{port}");
        let t = free_register(&url, "d", Duration::from_secs(5)).unwrap();
        assert_eq!(t, "t-1");
        assert_eq!(
            free_usage(&url, &t, Duration::from_secs(5)).unwrap()["now_ms"],
            1
        );
        let e = free_usage(&url, "nope", Duration::from_secs(5)).unwrap_err();
        assert_eq!((e.status, e.kind.as_str()), (401, "anchor_auth"));
    }
}
