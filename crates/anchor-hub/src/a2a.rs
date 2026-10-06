//! A2A endpoint, protocol version **1.0** (ADR-0004), JSON-RPC binding
//! (`POST /a2a`) plus the public Agent Card at
//! `/.well-known/agent-card.json`.
//!
//! The Hub is the A2A server every agent talks to (A2S2A): `SendMessage`
//! puts an A2A message on the Hub's bus, addressed by `contextId`
//! (`agent:<id>` or `room:<name>`); the sender is always the agent the
//! bearer token belongs to, whatever the message's metadata says.
//! `GetTask` and `ListTasks` expose the Hub's tasks. Streaming, push
//! notifications, cancellation and the extended card are not offered
//! (`UnsupportedOperationError`), and the card says so.

use serde_json::{Value, json};

use crate::http::{Request, Response};
use crate::model::*;
use crate::{Hub, HubError};

pub const PROTOCOL_VERSION: &str = "1.0";

const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL: i64 = -32603;
const TASK_NOT_FOUND: i64 = -32001;
const UNSUPPORTED_OPERATION: i64 = -32004;
const VERSION_NOT_SUPPORTED: i64 = -32009;

fn error(id: &Value, code: i64, message: &str) -> Response {
    Response::json(
        200,
        &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    )
}

fn ok(id: &Value, result: Value) -> Response {
    Response::json(
        200,
        &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    )
}

fn hub_error(id: &Value, e: &HubError) -> Response {
    let code = match e {
        HubError::NotFound(_) => TASK_NOT_FOUND,
        HubError::Invalid(_) => INVALID_PARAMS,
        HubError::Db(_) | HubError::Audit(_) | HubError::Fs(_) => INTERNAL,
        // Permission refusals and conflicts are request-specific failures.
        _ => INVALID_REQUEST,
    };
    error(id, code, &e.to_string())
}

/// The Hub's Agent Card (A2A §8).
pub fn agent_card(port: u16) -> Value {
    json!({
        "name": "Anchor Coordination Hub",
        "description": "Shared project state for the agents working on one workspace: file claims, messages, tasks and decisions. Every call is authenticated, checked against the caller's permissions and audited.",
        "supportedInterfaces": [{
            "url": format!("http://127.0.0.1:{port}/a2a"),
            "protocolBinding": "JSONRPC",
            "protocolVersion": PROTOCOL_VERSION,
        }],
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": {
            "streaming": false,
            "pushNotifications": false,
            "extendedAgentCard": false,
        },
        "securitySchemes": {
            "bearer": { "httpAuthSecurityScheme": { "scheme": "Bearer" } }
        },
        "securityRequirements": [{ "schemes": { "bearer": { "list": [] } } }],
        "defaultInputModes": ["text/plain", "application/json"],
        "defaultOutputModes": ["application/json"],
        "skills": [{
            "id": "coordination",
            "name": "Agent coordination",
            "description": "Relay messages between agents (contextId agent:<id> or room:<name>) and report project tasks.",
            "tags": ["coordination", "messaging", "tasks"],
        }],
    })
}

fn task_state(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Pending => "TASK_STATE_SUBMITTED",
        TaskStatus::InProgress => "TASK_STATE_WORKING",
        TaskStatus::Review => "TASK_STATE_INPUT_REQUIRED",
        TaskStatus::Done => "TASK_STATE_COMPLETED",
        TaskStatus::Blocked => "TASK_STATE_REJECTED",
    }
}

fn task_json(t: &Task) -> Value {
    json!({
        "id": t.id.to_string(),
        "contextId": "project",
        "status": { "state": task_state(t.status) },
        "metadata": {
            "description": t.description,
            "assignedTo": t.assigned_to_agent_id,
            "dependsOn": t.depends_on,
        },
    })
}

fn send_message(hub: &Hub, caller: Caller, id: &Value, params: &Value) -> Response {
    let m = &params["message"];
    let (Some(message_id), Some(parts)) = (m["messageId"].as_str(), m["parts"].as_array()) else {
        return error(
            id,
            INVALID_PARAMS,
            "message.messageId and message.parts are required",
        );
    };
    if message_id.is_empty() || message_id.len() > 200 || parts.is_empty() || parts.len() > 64 {
        return error(id, INVALID_PARAMS, "invalid messageId or parts");
    }
    let mut out_parts = Vec::new();
    for p in parts {
        if let Some(t) = p["text"].as_str() {
            out_parts.push(json!({ "text": t }));
        } else if p.get("data").is_some() {
            out_parts.push(json!({ "data": p["data"] }));
        } else {
            return error(id, INVALID_PARAMS, "only text and data parts are accepted");
        }
    }
    let to = match m["contextId"].as_str().and_then(|c| c.split_once(':')) {
        Some(("agent", n)) => match n.parse::<i64>() {
            Ok(a) => Recipient::Agent(a),
            Err(_) => return error(id, INVALID_PARAMS, "contextId agent:<id> needs a number"),
        },
        Some(("room", r)) => Recipient::Room(r.to_string()),
        _ => {
            return error(
                id,
                INVALID_PARAMS,
                "contextId must be agent:<id> or room:<name>",
            );
        }
    };
    let message_type = m["metadata"]["anchorMessageType"]
        .as_str()
        .map(|s| MessageType::parse(s).ok_or(()))
        .unwrap_or(Ok(MessageType::Question));
    let Ok(message_type) = message_type else {
        return error(id, INVALID_PARAMS, "unknown anchorMessageType");
    };
    let content = json!({ "a2a": { "messageId": message_id, "parts": out_parts } });
    match hub.send_message(caller, to, message_type, content) {
        Ok(stored) => ok(
            id,
            json!({
                "message": {
                    "messageId": format!("hub-{}", stored.id),
                    "contextId": m["contextId"],
                    "role": "ROLE_AGENT",
                    "parts": [{ "text": match stored.state {
                        MessageState::Held => "held for the user's approval",
                        _ => "delivered",
                    }}],
                    "metadata": { "anchorMessageId": stored.id, "state": stored.state },
                }
            }),
        ),
        Err(e) => hub_error(id, &e),
    }
}

pub fn handle(hub: &Hub, caller: Caller, req: &Request) -> Response {
    let null = Value::Null;
    let msg: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(_) => return error(&null, -32700, "Invalid JSON payload"),
    };
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    if msg.get("jsonrpc") != Some(&json!("2.0")) || !msg["method"].is_string() {
        return error(&id, INVALID_REQUEST, "Request payload validation error");
    }
    // An empty header means 0.3 (A2A §3.6.2), which this server does not
    // implement.
    let version = req.header("a2a-version").unwrap_or("");
    if version != PROTOCOL_VERSION {
        return error(
            &id,
            VERSION_NOT_SUPPORTED,
            &format!("A2A version {version:?} is not supported; use {PROTOCOL_VERSION}"),
        );
    }
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    match msg["method"].as_str().unwrap_or_default() {
        "SendMessage" => send_message(hub, caller, &id, &params),
        "GetTask" => {
            let Some(task_id) = params["id"].as_str().and_then(|s| s.parse::<i64>().ok()) else {
                return error(&id, INVALID_PARAMS, "params.id must be a task id");
            };
            match hub.task(caller, task_id) {
                Ok(t) => ok(&id, task_json(&t)),
                Err(e) => hub_error(&id, &e),
            }
        }
        "ListTasks" => match hub.tasks(caller) {
            Ok(ts) => {
                let tasks: Vec<Value> = ts.iter().map(task_json).collect();
                let n = tasks.len();
                ok(
                    &id,
                    json!({ "tasks": tasks, "nextPageToken": "", "pageSize": n, "totalSize": n }),
                )
            }
            Err(e) => hub_error(&id, &e),
        },
        "SendStreamingMessage"
        | "CancelTask"
        | "SubscribeToTask"
        | "CreateTaskPushNotificationConfig"
        | "GetTaskPushNotificationConfig"
        | "ListTaskPushNotificationConfigs"
        | "DeleteTaskPushNotificationConfig"
        | "GetExtendedAgentCard" => error(
            &id,
            UNSUPPORTED_OPERATION,
            "This operation is not supported by the Anchor Hub",
        ),
        _ => error(&id, METHOD_NOT_FOUND, "Method not found"),
    }
}
