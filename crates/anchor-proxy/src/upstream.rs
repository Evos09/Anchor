//! Forwarding to a provider (Update #1 §4.2 steps 3–6). The proxy always
//! asks the provider to stream, with usage in the final chunk
//! (`stream_options.include_usage`), counts output as it arrives, and
//! cuts the stream if output passes the reservation by the margin. The
//! client gets one assembled chat completion.
//!
//! Content passes through memory only: nothing here writes anywhere.

use std::collections::BTreeMap;
use std::io::BufRead;
use std::time::Duration;

use serde_json::{Value, json};

use anchor_inference::estimate::tokens_for_bytes;

/// What a provider call came back with.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamResult {
    pub status: u16,
    /// Rate-limit headers, for the pool to learn limits from.
    pub headers: Vec<(String, String)>,
    pub retry_after_secs: Option<u64>,
    /// The error body says a quota (not a rate) is exhausted.
    pub quota: bool,
    /// The assembled completion (status 200 only).
    pub completion: Option<Assembled>,
    /// A short reason for a failed call (never the request's content).
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Assembled {
    pub content: String,
    /// (id, name, arguments) by tool-call index.
    pub tool_calls: BTreeMap<u64, (String, String, String)>,
    pub finish_reason: Option<String>,
    /// (prompt, completion) tokens, if the provider reported them.
    pub usage: Option<(u64, u64)>,
    /// Output tokens the proxy counted itself (bytes / 4).
    pub counted_output: u64,
    /// The stream was cut for passing the reservation.
    pub truncated: bool,
}

impl Assembled {
    /// The OpenAI chat completion the client gets.
    pub fn to_completion(&self, model: &str) -> Value {
        let mut message = json!({ "role": "assistant", "content": self.content });
        if !self.tool_calls.is_empty() {
            message["tool_calls"] = self
                .tool_calls
                .values()
                .map(|(id, name, args)| {
                    json!({ "id": id, "type": "function",
                            "function": { "name": name, "arguments": args } })
                })
                .collect();
        }
        let (p, c) = self.usage.unwrap_or((0, self.counted_output));
        json!({
            "object": "chat.completion",
            "model": model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": if self.truncated { "length" } else {
                    self.finish_reason.as_deref().unwrap_or("stop")
                },
            }],
            "usage": { "prompt_tokens": p, "completion_tokens": c, "total_tokens": p + c },
        })
    }
}

/// Read an SSE chat-completions stream, stopping once the counted output
/// passes `cut_at` tokens.
pub fn assemble(reader: impl BufRead, cut_at: u64) -> Assembled {
    let mut a = Assembled::default();
    let mut out_bytes = 0usize;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            a.usage = Some((n("prompt_tokens"), n("completion_tokens")));
        }
        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
            continue;
        };
        if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
            a.finish_reason = Some(f.to_string());
        }
        let delta = &choice["delta"];
        if let Some(t) = delta.get("content").and_then(Value::as_str) {
            a.content.push_str(t);
            out_bytes += t.len();
        }
        for tc in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
            let e = a.tool_calls.entry(idx).or_default();
            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                e.0 = id.to_string();
            }
            if let Some(n) = tc.pointer("/function/name").and_then(Value::as_str) {
                e.1.push_str(n);
                out_bytes += n.len();
            }
            if let Some(args) = tc.pointer("/function/arguments").and_then(Value::as_str) {
                e.2.push_str(args);
                out_bytes += args.len();
            }
        }
        a.counted_output = tokens_for_bytes(out_bytes);
        if a.counted_output > cut_at {
            a.truncated = true;
            break;
        }
    }
    a.counted_output = tokens_for_bytes(out_bytes);
    a
}

/// A provider endpoint (a trait so the proxy can be tested end to end
/// without the network).
pub trait Upstream: Send + Sync {
    /// POST `body` to `{base_url}/chat/completions` with `key`, streaming.
    fn chat(&self, base_url: &str, key: &str, body: &Value, cut_at: u64) -> UpstreamResult;
}

/// The real thing, over HTTPS.
#[derive(Debug)]
pub struct HttpUpstream {
    agent: ureq::Agent,
}

impl HttpUpstream {
    pub fn new(timeout: Duration) -> HttpUpstream {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build()
            .into();
        HttpUpstream { agent }
    }
}

const LIMIT_HEADERS: [&str; 4] = [
    "x-ratelimit-limit-requests",
    "x-ratelimit-limit-tokens",
    "x-ratelimit-remaining-requests",
    "x-ratelimit-remaining-tokens",
];

impl Upstream for HttpUpstream {
    fn chat(&self, base_url: &str, key: &str, body: &Value, cut_at: u64) -> UpstreamResult {
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let resp = match self
            .agent
            .post(&url)
            .header("Authorization", format!("Bearer {key}"))
            .header("Accept", "text/event-stream")
            .send_json(body)
        {
            Ok(r) => r,
            Err(e) => {
                return UpstreamResult {
                    status: 502,
                    headers: Vec::new(),
                    retry_after_secs: None,
                    quota: false,
                    completion: None,
                    error: Some(format!("could not reach the provider: {e}")),
                };
            }
        };
        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = LIMIT_HEADERS
            .iter()
            .filter_map(|h| {
                resp.headers()
                    .get(*h)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| (h.to_string(), v.to_string()))
            })
            .collect();
        let retry_after_secs = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let mut body = resp.into_body();
        if status != 200 {
            // Read a bounded error body only to classify it.
            let text = body
                .with_config()
                .limit(64 * 1024)
                .read_to_string()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let quota =
                text.contains("quota") || text.contains("per day") || text.contains("daily");
            return UpstreamResult {
                status,
                headers,
                retry_after_secs,
                quota,
                completion: None,
                error: Some(format!("the provider answered {status}")),
            };
        }
        let reader = std::io::BufReader::new(body.into_reader());
        UpstreamResult {
            status,
            headers,
            retry_after_secs,
            quota: false,
            completion: Some(assemble(reader, cut_at)),
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(chunks: &[Value]) -> String {
        let mut s = String::new();
        for c in chunks {
            s.push_str(&format!("data: {c}\n\n"));
        }
        s.push_str("data: [DONE]\n\n");
        s
    }

    #[test]
    fn a_stream_is_assembled_with_the_providers_usage() {
        let text = sse(&[
            json!({"choices":[{"delta":{"role":"assistant","content":"Hel"}}]}),
            json!({"choices":[{"delta":{"content":"lo"}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":9}}),
        ]);
        let a = assemble(text.as_bytes(), 1000);
        assert_eq!(a.content, "Hello");
        assert_eq!(
            a.tool_calls[&0],
            ("c1".into(), "read_file".into(), "{\"path\":\"a\"}".into())
        );
        assert_eq!(a.usage, Some((120, 9)));
        assert!(!a.truncated);
        let c = a.to_completion("m");
        assert_eq!(c["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(c["usage"]["total_tokens"], 129);
        assert_eq!(
            c["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
    }

    #[test]
    fn the_stream_is_cut_past_the_reservation() {
        let chunk = json!({"choices":[{"delta":{"content":"x".repeat(400)}}]});
        let text = sse(&vec![chunk; 50]); // 20,000 bytes ≈ 5,000 tokens
        let a = assemble(text.as_bytes(), 1200);
        assert!(a.truncated);
        assert!(
            a.counted_output > 1200 && a.counted_output <= 1300,
            "{}",
            a.counted_output
        );
        assert_eq!(
            a.to_completion("m")["choices"][0]["finish_reason"],
            "length"
        );
    }
}
