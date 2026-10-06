//! Provider adapters. The internal abstraction is the OpenAI-compatible chat
//! completions API (spec §3.7); provider-specific behaviour stays here.

use std::time::Duration;

use anchor_protocol::{ChatMessage, ChatRole, ToolCall, ToolRequest, Validate};
use serde::Deserialize;

use crate::config::{Params, ProviderConfig, ProviderKind};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InvalidCall {
    pub id: String,
    pub name: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub invalid_calls: Vec<InvalidCall>,
    /// `None` if the provider did not report usage.
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProviderError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("bad response: {0}")]
    BadResponse(String),
    #[error("missing API key: environment variable {0} is not set")]
    MissingKey(String),
    #[error("mock script error: {0}")]
    Script(String),
}

pub trait Provider: std::fmt::Debug {
    fn complete(
        &mut self,
        model_id: &str,
        messages: &[ChatMessage],
        tools: &serde_json::Value,
        params: &Params,
    ) -> Result<ChatResponse, ProviderError>;
}

/// Convert a model-proposed (name, arguments) pair into a validated request.
/// Model output is hostile: anything that does not decode into the closed
/// tool set, or fails validation, is rejected here.
pub fn parse_tool_call(id: &str, name: &str, arguments: &serde_json::Value) -> Result<ToolCall, InvalidCall> {
    let invalid = |error: String| InvalidCall {
        id: id.to_string(),
        name: name.to_string(),
        error,
    };
    let request: ToolRequest = serde_json::from_value(serde_json::json!({"tool": name, "args": arguments}))
        .map_err(|e| invalid(e.to_string()))?;
    let call = ToolCall {
        call_id: id.to_string(),
        request,
    };
    call.validate().map_err(invalid)?;
    Ok(call)
}

// ---------------------------------------------------------------------------
// OpenAI-compatible HTTP adapter
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct OpenAiCompatible {
    base_url: String,
    api_key: Option<String>,
    agent: ureq::Agent,
}

impl OpenAiCompatible {
    pub fn new(cfg: &ProviderConfig) -> Result<Self, ProviderError> {
        let api_key = match &cfg.api_key_env {
            None => None,
            Some(var) if var.is_empty() => None,
            Some(var) => Some(std::env::var(var).map_err(|_| ProviderError::MissingKey(var.clone()))?),
        };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(cfg.timeout_secs)))
            .build()
            .into();
        Ok(OpenAiCompatible {
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key,
            agent,
        })
    }
}

/// Encode our message history in OpenAI chat format.
pub fn to_openai_messages(messages: &[ChatMessage]) -> serde_json::Value {
    let out: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            let role = match m.role {
                ChatRole::System => "system",
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
                ChatRole::Tool => "tool",
            };
            let mut v = serde_json::json!({"role": role, "content": m.content});
            if !m.tool_calls.is_empty() {
                let calls: Vec<_> = m
                    .tool_calls
                    .iter()
                    .map(|c| {
                        let enc = serde_json::to_value(&c.request).unwrap_or_default();
                        serde_json::json!({
                            "id": c.call_id,
                            "type": "function",
                            "function": {
                                "name": enc["tool"],
                                "arguments": enc["args"].to_string(),
                            }
                        })
                    })
                    .collect();
                v["tool_calls"] = serde_json::Value::Array(calls);
            }
            if let Some(id) = &m.tool_call_id {
                v["tool_call_id"] = serde_json::Value::String(id.clone());
            }
            v
        })
        .collect();
    serde_json::Value::Array(out)
}

#[derive(Debug, Deserialize)]
struct OaResponse {
    choices: Vec<OaChoice>,
    #[serde(default)]
    usage: Option<OaUsage>,
}
#[derive(Debug, Deserialize)]
struct OaChoice {
    message: OaMessage,
}
#[derive(Debug, Deserialize)]
struct OaMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OaToolCall>,
}
#[derive(Debug, Deserialize)]
struct OaToolCall {
    id: String,
    function: OaFunction,
}
#[derive(Debug, Deserialize)]
struct OaFunction {
    name: String,
    arguments: String,
}
#[derive(Debug, Deserialize)]
struct OaUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

/// Parse an OpenAI-compatible response body.
pub fn parse_openai_response(body: &serde_json::Value) -> Result<ChatResponse, ProviderError> {
    let r: OaResponse =
        serde_json::from_value(body.clone()).map_err(|e| ProviderError::BadResponse(e.to_string()))?;
    let choice = r
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ProviderError::BadResponse("no choices".into()))?;
    let mut tool_calls = Vec::new();
    let mut invalid_calls = Vec::new();
    for c in choice.message.tool_calls {
        let args: serde_json::Value = match serde_json::from_str(&c.function.arguments) {
            Ok(a) => a,
            Err(e) => {
                invalid_calls.push(InvalidCall {
                    id: c.id,
                    name: c.function.name,
                    error: format!("arguments are not valid JSON: {e}"),
                });
                continue;
            }
        };
        match parse_tool_call(&c.id, &c.function.name, &args) {
            Ok(tc) => tool_calls.push(tc),
            Err(ic) => invalid_calls.push(ic),
        }
    }
    Ok(ChatResponse {
        content: choice.message.content.unwrap_or_default(),
        tool_calls,
        invalid_calls,
        usage: r.usage.map(|u| TokenUsage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
        }),
    })
}

impl Provider for OpenAiCompatible {
    fn complete(
        &mut self,
        model_id: &str,
        messages: &[ChatMessage],
        tools: &serde_json::Value,
        params: &Params,
    ) -> Result<ChatResponse, ProviderError> {
        let mut body = serde_json::json!({
            "model": model_id,
            "messages": to_openai_messages(messages),
            "tools": tools,
        });
        if let Some(t) = params.temperature {
            body["temperature"] = serde_json::json!(t);
        }
        if let Some(m) = params.max_tokens {
            body["max_tokens"] = serde_json::json!(m);
        }
        let mut req = self.agent.post(format!("{}/chat/completions", self.base_url));
        if let Some(k) = &self.api_key {
            req = req.header("Authorization", format!("Bearer {k}"));
        }
        let resp = req
            .send_json(&body)
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let v: serde_json::Value = resp
            .into_body()
            .read_json()
            .map_err(|e| ProviderError::BadResponse(e.to_string()))?;
        parse_openai_response(&v)
    }
}

// ---------------------------------------------------------------------------
// Mock provider
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct MockScript {
    steps: Vec<MockStep>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct MockStep {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<MockCall>,
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    /// Simulate a provider failure for this step.
    #[serde(default)]
    fail: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct MockCall {
    tool: String,
    args: serde_json::Value,
}

#[derive(Debug)]
pub struct MockProvider {
    steps: std::vec::IntoIter<MockStep>,
    call_counter: u64,
}

impl MockProvider {
    pub fn from_script(script: &str) -> Result<Self, ProviderError> {
        let s: MockScript = serde_json::from_str(script).map_err(|e| ProviderError::Script(e.to_string()))?;
        Ok(MockProvider {
            steps: s.steps.into_iter(),
            call_counter: 0,
        })
    }
}

impl Provider for MockProvider {
    fn complete(
        &mut self,
        _model_id: &str,
        _messages: &[ChatMessage],
        _tools: &serde_json::Value,
        _params: &Params,
    ) -> Result<ChatResponse, ProviderError> {
        let Some(step) = self.steps.next() else {
            return Ok(ChatResponse {
                content: "Script finished.".into(),
                tool_calls: vec![],
                invalid_calls: vec![],
                usage: None,
            });
        };
        if step.fail {
            return Err(ProviderError::Transport("mock provider failure".into()));
        }
        let mut tool_calls = Vec::new();
        let mut invalid_calls = Vec::new();
        for c in step.tool_calls {
            self.call_counter += 1;
            let id = format!("mock-{}", self.call_counter);
            match parse_tool_call(&id, &c.tool, &c.args) {
                Ok(tc) => tool_calls.push(tc),
                Err(ic) => invalid_calls.push(ic),
            }
        }
        let usage = match (step.prompt_tokens, step.completion_tokens) {
            (None, None) => None,
            (p, c) => Some(TokenUsage {
                prompt_tokens: p.unwrap_or(0),
                completion_tokens: c.unwrap_or(0),
            }),
        };
        Ok(ChatResponse {
            content: step.content,
            tool_calls,
            invalid_calls,
            usage,
        })
    }
}

/// Build a provider from config. Mock providers require a script.
pub fn build(cfg: &ProviderConfig, mock_script: Option<&str>) -> Result<Box<dyn Provider>, ProviderError> {
    match cfg.kind {
        ProviderKind::OpenaiCompatible => Ok(Box::new(OpenAiCompatible::new(cfg)?)),
        ProviderKind::Mock => {
            let script = mock_script.ok_or_else(|| ProviderError::Script("no mock script supplied".into()))?;
            Ok(Box::new(MockProvider::from_script(script)?))
        }
    }
}

// ---------------------------------------------------------------------------
// Local inference server detection (spec §10)
// ---------------------------------------------------------------------------

/// Common local OpenAI-compatible endpoints. Which server runs on which port
/// is a convention, not a guarantee; detection only checks the protocol.
pub const LOCAL_CANDIDATE_PORTS: &[u16] = &[11434, 1234, 8080, 8000];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalServer {
    pub base_url: String,
    pub models: Vec<String>,
}

/// Probe localhost for OpenAI-compatible `/v1/models` endpoints.
pub fn detect_local_servers(timeout: Duration) -> Vec<LocalServer> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .proxy(None)
        .build()
        .into();
    let mut found = Vec::new();
    for port in LOCAL_CANDIDATE_PORTS {
        let base = format!("http://127.0.0.1:{port}/v1");
        let Ok(resp) = agent.get(format!("{base}/models")).call() else {
            continue;
        };
        let Ok(v) = resp.into_body().read_json::<serde_json::Value>() else {
            continue;
        };
        let models = v["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        found.push(LocalServer { base_url: base, models });
    }
    found
}
