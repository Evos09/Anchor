//! MCP server endpoint, protocol revision **2026-07-28** (ADR-0004),
//! Streamable HTTP transport with JSON responses (`POST /mcp`).
//!
//! The 2026-07-28 revision is stateless: there is no `initialize`; every
//! request carries its protocol version and client capabilities in
//! `_meta`. Implemented: `server/discover`, `tools/list`, `tools/call`.
//! Not implemented (see ADR-0004): SSE responses, `subscriptions/listen`,
//! multi round-trip requests, the tasks extension, resources and prompts,
//! and the deprecated sampling, roots and logging features.

use serde_json::{Value, json};

use crate::Hub;
use crate::http::{Request, Response};
use crate::model::Caller;
use crate::tools::{self, ToolError};

pub const PROTOCOL_VERSION: &str = "2026-07-28";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const HEADER_MISMATCH: i64 = -32020;
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

fn error(status: u16, id: &Value, code: i64, message: &str, data: Option<Value>) -> Response {
    let mut err = json!({ "code": code, "message": message });
    if let Some(d) = data {
        err["data"] = d;
    }
    let mut body = json!({ "jsonrpc": "2.0", "error": err });
    if !id.is_null() {
        body["id"] = id.clone();
    }
    Response::json(status, &body)
}

fn server_info() -> Value {
    json!({ "name": "anchor-hub", "version": env!("CARGO_PKG_VERSION") })
}

fn result(id: &Value, mut r: Value) -> Response {
    r["resultType"] = json!("complete");
    r["_meta"] = json!({ "io.modelcontextprotocol/serverInfo": server_info() });
    Response::json(200, &json!({ "jsonrpc": "2.0", "id": id, "result": r }))
}

pub fn handle(hub: &Hub, caller: Caller, req: &Request) -> Response {
    let null = Value::Null;
    let msg: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(_) => return error(400, &null, PARSE_ERROR, "Parse error", None),
    };
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    if msg.get("jsonrpc") != Some(&json!("2.0")) || !msg["method"].is_string() {
        return error(400, &id, INVALID_REQUEST, "Invalid Request", None);
    }
    if !(id.is_string() || id.is_i64() || id.is_u64()) {
        // A notification (no id): this revision defines none over HTTP.
        return error(
            400,
            &null,
            INVALID_REQUEST,
            "notifications are not accepted",
            None,
        );
    }
    let method = msg["method"].as_str().unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let meta = &params["_meta"];

    // Standard request headers must match the body (transport §Server
    // Validation), and the protocol version must be one we implement.
    if req.header("mcp-method") != Some(method) {
        return error(
            400,
            &id,
            HEADER_MISMATCH,
            "Header mismatch: Mcp-Method",
            None,
        );
    }
    let body_version = meta["io.modelcontextprotocol/protocolVersion"].as_str();
    let header_version = req.header("mcp-protocol-version");
    if body_version.is_none() || header_version != body_version {
        return error(
            400,
            &id,
            HEADER_MISMATCH,
            "Header mismatch: MCP-Protocol-Version",
            None,
        );
    }
    let requested = body_version.unwrap_or_default();
    if requested != PROTOCOL_VERSION {
        return error(
            400,
            &id,
            UNSUPPORTED_PROTOCOL_VERSION,
            "Unsupported protocol version",
            Some(json!({ "supported": [PROTOCOL_VERSION], "requested": requested })),
        );
    }
    if !meta["io.modelcontextprotocol/clientCapabilities"].is_object() {
        return error(
            400,
            &id,
            INVALID_PARAMS,
            "clientCapabilities is required in _meta",
            None,
        );
    }

    match method {
        "server/discover" => result(
            &id,
            json!({
                "supportedVersions": [PROTOCOL_VERSION],
                "capabilities": { "tools": {} },
                "instructions": "Anchor Coordination Hub. Claim paths before editing them, coordinate with other agents through hub messages, and record tasks and decisions here. Every call is audited.",
                "ttlMs": 0,
                "cacheScope": "private",
            }),
        ),
        "tools/list" => {
            let tools: Vec<Value> = tools::definitions()
                .into_iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                    })
                })
                .collect();
            result(
                &id,
                json!({ "tools": tools, "ttlMs": 0, "cacheScope": "private" }),
            )
        }
        "tools/call" => {
            let Some(name) = params["name"].as_str() else {
                return error(400, &id, INVALID_PARAMS, "params.name is required", None);
            };
            if req.header("mcp-name") != Some(name) {
                return error(400, &id, HEADER_MISMATCH, "Header mismatch: Mcp-Name", None);
            }
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let ws = anchor_fs::Workspace::open(hub.workspace()).ok();
            match tools::call(hub, caller, ws.as_ref(), name, args) {
                Ok(v) => result(
                    &id,
                    json!({
                        "content": [{ "type": "text", "text": v.to_string() }],
                        "structuredContent": v,
                        "isError": false,
                    }),
                ),
                Err(ToolError::UnknownTool(t)) => error(
                    400,
                    &id,
                    INVALID_PARAMS,
                    &format!("Unknown tool: {t}"),
                    None,
                ),
                // Tool execution errors go back to the model as results.
                Err(e) => {
                    let code = match &e {
                        ToolError::Hub(h) => h.code(),
                        _ => "invalid",
                    };
                    result(
                        &id,
                        json!({
                            "content": [{ "type": "text", "text": e.to_string() }],
                            "structuredContent": { "error": code, "message": e.to_string() },
                            "isError": true,
                        }),
                    )
                }
            }
        }
        _ => error(404, &id, METHOD_NOT_FOUND, "Method not found", None),
    }
}
