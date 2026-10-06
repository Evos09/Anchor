//! The Hub's agent-facing tools: one definition and one implementation,
//! shared by the MCP endpoint and by the backend's own runtimes (protocol
//! v2 `ToolRequest::Hub`). Arguments are parsed into closed structs
//! (`deny_unknown_fields`), so a forged identity field such as
//! `from_agent_id` is an error, not something to ignore.

use anchor_fs::Workspace;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::hub::{Hub, VerifiedFile};
use crate::model::*;
use crate::{HubError, Result as HubResult};

#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool {0}")]
    UnknownTool(String),
    #[error("invalid arguments: {0}")]
    BadArgs(String),
    #[error(transparent)]
    Hub(#[from] HubError),
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

fn enum_of(values: &[&str]) -> Value {
    json!({ "type": "string", "enum": values })
}

/// Every tool, in a fixed order (stable `tools/list`, stable prompts).
pub fn definitions() -> Vec<ToolDef> {
    let claim_types: Vec<&str> = ClaimType::ALL.iter().map(|c| c.as_str()).collect();
    let msg_types: Vec<&str> = MessageType::ALL.iter().map(|c| c.as_str()).collect();
    let task_states: Vec<&str> = TaskStatus::ALL.iter().map(|c| c.as_str()).collect();
    let art_kinds: Vec<&str> = ArtifactKind::ALL.iter().map(|c| c.as_str()).collect();
    let id = json!({ "type": "integer", "minimum": 1 });
    vec![
        ToolDef {
            name: "hub_list_agents",
            description: "List the agents registered on this project (id, name, role, model family, status).",
            input_schema: obj(json!({}), &[]),
        },
        ToolDef {
            name: "hub_request_claim",
            description: "Claim a workspace path before working on it. 'src/' claims a subtree, '.' the whole workspace. Fails with a conflict naming the holder if another agent's claim overlaps.",
            input_schema: obj(
                json!({
                    "path": { "type": "string", "maxLength": 1024 },
                    "claim_type": enum_of(&claim_types),
                    "isolated": { "type": "boolean" },
                    "ttl_ms": { "type": "integer", "minimum": 1000 },
                }),
                &["path", "claim_type"],
            ),
        },
        ToolDef {
            name: "hub_renew_claim",
            description: "Extend the lease of one of your active claims.",
            input_schema: obj(
                json!({ "claim_id": id, "ttl_ms": { "type": "integer", "minimum": 1000 } }),
                &["claim_id"],
            ),
        },
        ToolDef {
            name: "hub_release_claim",
            description: "Release one of your claims when you are done with the path.",
            input_schema: obj(json!({ "claim_id": id }), &["claim_id"]),
        },
        ToolDef {
            name: "hub_list_claims",
            description: "List claims on this project (active ones by default).",
            input_schema: obj(json!({ "active_only": { "type": "boolean" } }), &[]),
        },
        ToolDef {
            name: "hub_send_message",
            description: "Send a message to another agent (to_agent) or a project room (room). Messages may be held until the user approves them.",
            input_schema: obj(
                json!({
                    "to_agent": id,
                    "room": { "type": "string", "pattern": "^[A-Za-z0-9_-]{1,40}$" },
                    "message_type": enum_of(&msg_types),
                    "content": {},
                }),
                &["message_type", "content"],
            ),
        },
        ToolDef {
            name: "hub_query_messages",
            description: "Read messages addressed to you, sent by you, or posted in rooms. Use since_id to fetch only newer ones.",
            input_schema: obj(
                json!({
                    "since_id": { "type": "integer", "minimum": 0 },
                    "room": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500 },
                }),
                &[],
            ),
        },
        ToolDef {
            name: "hub_create_task",
            description: "Create a task, optionally depending on other tasks and assigned to an agent.",
            input_schema: obj(
                json!({
                    "description": { "type": "string", "maxLength": 8000 },
                    "depends_on": { "type": "array", "items": id, "maxItems": 64 },
                    "assign_to": id,
                }),
                &["description"],
            ),
        },
        ToolDef {
            name: "hub_assign_task",
            description: "Assign a task to an agent.",
            input_schema: obj(
                json!({ "task_id": id, "agent_id": id }),
                &["task_id", "agent_id"],
            ),
        },
        ToolDef {
            name: "hub_update_task_status",
            description: "Move a task assigned to you to a new status. Starting it requires its dependencies to be done.",
            input_schema: obj(
                json!({ "task_id": id, "status": enum_of(&task_states) }),
                &["task_id", "status"],
            ),
        },
        ToolDef {
            name: "hub_list_tasks",
            description: "List the project's tasks.",
            input_schema: obj(json!({}), &[]),
        },
        ToolDef {
            name: "hub_propose_decision",
            description: "Propose a project decision for the user to accept or reject, optionally citing an artifact as evidence.",
            input_schema: obj(
                json!({
                    "summary": { "type": "string", "maxLength": 1000 },
                    "rationale": { "type": "string", "maxLength": 8000 },
                    "evidence_ref": id,
                }),
                &["summary", "rationale"],
            ),
        },
        ToolDef {
            name: "hub_list_decisions",
            description: "List proposed, accepted and rejected decisions.",
            input_schema: obj(json!({}), &[]),
        },
        ToolDef {
            name: "hub_register_artifact",
            description: "Register a workspace file as evidence (the Hub hashes the file itself).",
            input_schema: obj(
                json!({
                    "kind": enum_of(&art_kinds),
                    "path": { "type": "string", "maxLength": 1024 },
                    "evidence": { "type": "object" },
                }),
                &["kind", "path"],
            ),
        },
    ]
}

fn args<T: DeserializeOwned>(v: Value) -> Result<T, ToolError> {
    let v = if v.is_null() { json!({}) } else { v };
    serde_json::from_value(v).map_err(|e| ToolError::BadArgs(e.to_string()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimArgs {
    path: String,
    claim_type: ClaimType,
    #[serde(default)]
    isolated: bool,
    #[serde(default)]
    ttl_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimIdArgs {
    claim_id: i64,
    #[serde(default)]
    ttl_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListClaimsArgs {
    #[serde(default = "yes")]
    active_only: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    #[serde(default)]
    to_agent: Option<AgentId>,
    #[serde(default)]
    room: Option<String>,
    message_type: MessageType,
    content: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTaskArgs {
    description: String,
    #[serde(default)]
    depends_on: Vec<i64>,
    #[serde(default)]
    assign_to: Option<AgentId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssignArgs {
    task_id: i64,
    agent_id: AgentId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    task_id: i64,
    status: TaskStatus,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionArgs {
    summary: String,
    rationale: String,
    #[serde(default)]
    evidence_ref: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactArgs {
    kind: ArtifactKind,
    path: String,
    #[serde(default)]
    evidence: Value,
}

fn to_value<T: serde::Serialize>(r: HubResult<T>) -> Result<Value, ToolError> {
    Ok(serde_json::to_value(r?).unwrap_or(Value::Null))
}

/// Run tool `name` for `caller`. `ws` is the workspace artifacts are read
/// from (the main workspace for external agents, the agent's isolated
/// workspace for the backend's runtimes).
pub fn call(
    hub: &Hub,
    caller: Caller,
    ws: Option<&Workspace>,
    name: &str,
    raw: Value,
) -> Result<Value, ToolError> {
    let r = call_inner(hub, caller, ws, name, raw);
    // Refusals inside the Hub are audited there; refusals of malformed or
    // forged arguments happen before it and are audited here.
    if let Err(e @ (ToolError::BadArgs(_) | ToolError::UnknownTool(_))) = &r {
        hub.audit_event(
            "hub_denied",
            json!({ "caller": caller, "method": name, "error": e.to_string(), "code": "invalid" }),
        );
    }
    r
}

fn call_inner(
    hub: &Hub,
    caller: Caller,
    ws: Option<&Workspace>,
    name: &str,
    raw: Value,
) -> Result<Value, ToolError> {
    match name {
        "hub_list_agents" => {
            let Empty {} = args(raw)?;
            let agents = hub.agents()?;
            Ok(Value::Array(
                agents
                    .into_iter()
                    .map(|a| {
                        json!({
                            "id": a.id, "name": a.display_name, "role": a.role,
                            "model_id": a.model_id, "family": a.family, "status": a.status,
                        })
                    })
                    .collect(),
            ))
        }
        "hub_request_claim" => {
            let a: ClaimArgs = args(raw)?;
            to_value(hub.request_claim(caller, &a.path, a.claim_type, a.isolated, a.ttl_ms))
        }
        "hub_renew_claim" => {
            let a: ClaimIdArgs = args(raw)?;
            to_value(hub.renew_claim(caller, a.claim_id, a.ttl_ms))
        }
        "hub_release_claim" => {
            let a: ClaimIdArgs = args(raw)?;
            hub.release_claim(caller, a.claim_id)?;
            Ok(json!({ "released": a.claim_id }))
        }
        "hub_list_claims" => {
            let a: ListClaimsArgs = args(raw)?;
            to_value(hub.list_claims(caller, a.active_only))
        }
        "hub_send_message" => {
            let a: SendArgs = args(raw)?;
            let to = match (a.to_agent, a.room) {
                (Some(id), None) => Recipient::Agent(id),
                (None, Some(r)) => Recipient::Room(r),
                _ => {
                    return Err(ToolError::BadArgs(
                        "exactly one of to_agent and room is required".into(),
                    ));
                }
            };
            to_value(hub.send_message(caller, to, a.message_type, a.content))
        }
        "hub_query_messages" => {
            let q: MessageQuery = args(raw)?;
            to_value(hub.query_messages(caller, q))
        }
        "hub_create_task" => {
            let a: CreateTaskArgs = args(raw)?;
            to_value(hub.create_task(caller, &a.description, &a.depends_on, a.assign_to))
        }
        "hub_assign_task" => {
            let a: AssignArgs = args(raw)?;
            to_value(hub.assign_task(caller, a.task_id, a.agent_id))
        }
        "hub_update_task_status" => {
            let a: StatusArgs = args(raw)?;
            to_value(hub.update_task_status(caller, a.task_id, a.status))
        }
        "hub_list_tasks" => {
            let Empty {} = args(raw)?;
            to_value(hub.tasks(caller))
        }
        "hub_propose_decision" => {
            let a: DecisionArgs = args(raw)?;
            to_value(hub.propose_decision(caller, &a.summary, &a.rationale, a.evidence_ref))
        }
        "hub_list_decisions" => {
            let Empty {} = args(raw)?;
            to_value(hub.decisions(caller))
        }
        "hub_register_artifact" => {
            let a: ArtifactArgs = args(raw)?;
            let ws = ws.ok_or_else(|| ToolError::BadArgs("no workspace to read from".into()))?;
            let file = VerifiedFile::read(ws, &a.path)?;
            let evidence = if a.evidence.is_null() {
                json!({})
            } else {
                a.evidence
            };
            to_value(hub.register_artifact(caller, a.kind, &file, evidence))
        }
        other => Err(ToolError::UnknownTool(other.to_string())),
    }
}
