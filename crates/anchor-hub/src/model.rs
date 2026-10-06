//! The shared-state data model (spec §6.3) and its enumerations.

use serde::{Deserialize, Serialize};

pub type AgentId = i64;

/// Who is calling the Hub. Identity always comes from the transport (the
/// backend's pipe to a runtime, a bearer token, the local CLI), never from
/// request parameters (ADR-0023).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum Caller {
    /// The human user, through the local CLI or a confirmation prompt.
    User,
    /// A registered agent.
    Agent(AgentId),
    /// The Hub or backend itself (lease expiry, merge queue feedback).
    System,
}

macro_rules! str_enum {
    ($(#[$m:meta])* $name:ident { $($v:ident = $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($v),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$v),+];

            pub fn as_str(self) -> &'static str {
                match self { $($name::$v => $s),+ }
            }

            pub fn parse(s: &str) -> Option<$name> {
                match s { $($s => Some($name::$v),)+ _ => None }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

str_enum!(ClaimType { Read = "read", Edit = "edit", Review = "review" });
str_enum!(ClaimStatus { Active = "active", Released = "released", Expired = "expired", Revoked = "revoked" });
str_enum!(MessageType { Task = "task", Question = "question", Decision = "decision", Review = "review", Status = "status" });
str_enum!(
    /// Delivery state of a message: held messages await the user's
    /// approval and are invisible to their recipient.
    MessageState { Held = "held", Delivered = "delivered", Rejected = "rejected" }
);
str_enum!(DecisionStatus { Proposed = "proposed", Accepted = "accepted", Rejected = "rejected" });
str_enum!(TaskStatus { Pending = "pending", InProgress = "in_progress", Review = "review", Done = "done", Blocked = "blocked" });
str_enum!(ArtifactKind { Diff = "diff", TestOutput = "test_output", Screenshot = "screenshot", Log = "log", CoverageReport = "coverage_report" });
str_enum!(AgentStatus { Active = "active", Lost = "lost", Retired = "retired" });
str_enum!(MergeKind { Merge = "merge", Revert = "revert" });
str_enum!(MergeStatus { Queued = "queued", Applying = "applying", Merged = "merged", Rejected = "rejected" });

/// What a user grants an agent at registration. Requested capabilities are
/// recorded separately and never grant anything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    /// Claim paths the agent may claim: a claim must lie within one of
    /// these (`"."` is the whole workspace, `"src/"` a subtree, `"a.txt"`
    /// one file).
    #[serde(default)]
    pub claim_within: Vec<String>,
    #[serde(default)]
    pub claim_types: Vec<ClaimType>,
    #[serde(default)]
    pub message_types: Vec<MessageType>,
    #[serde(default)]
    pub may_create_tasks: bool,
    #[serde(default)]
    pub may_assign_tasks: bool,
    #[serde(default)]
    pub may_propose_decisions: bool,
    #[serde(default)]
    pub may_register_artifacts: bool,
    /// Every message from this agent waits for the user's approval.
    #[serde(default)]
    pub messages_need_approval: bool,
}

impl Permissions {
    /// Everything except holding messages: the usual grant for an agent the
    /// user starts in a team run.
    pub fn collaborator(claim_within: &[&str]) -> Permissions {
        Permissions {
            claim_within: claim_within.iter().map(|s| s.to_string()).collect(),
            claim_types: ClaimType::ALL.to_vec(),
            message_types: MessageType::ALL.to_vec(),
            may_create_tasks: true,
            may_assign_tasks: true,
            may_propose_decisions: true,
            may_register_artifacts: true,
            messages_need_approval: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub project_id: i64,
    pub display_name: String,
    pub model_id: String,
    pub role: String,
    /// Model family as declared by the user's provider configuration
    /// (ADR-0021).
    pub family: String,
    pub capabilities: serde_json::Value,
    pub permissions: Permissions,
    pub status: AgentStatus,
    pub last_seen_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewAgent {
    pub display_name: String,
    pub model_id: String,
    pub role: String,
    pub family: String,
    #[serde(default)]
    pub capabilities: serde_json::Value,
    pub permissions: Permissions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub id: i64,
    pub project_id: i64,
    pub agent_id: AgentId,
    /// Normalised claim path (see [`crate::ClaimPath`]).
    pub path: String,
    pub claim_type: ClaimType,
    /// A read claim that requires isolation from concurrent edits.
    pub isolated: bool,
    pub lease_expires_at: u64,
    pub status: ClaimStatus,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum Recipient {
    Agent(AgentId),
    Room(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub project_id: i64,
    /// `None` for the user or the Hub itself.
    pub from_agent_id: Option<AgentId>,
    pub to_agent_id: Option<AgentId>,
    pub room_id: Option<String>,
    pub message_type: MessageType,
    pub content: serde_json::Value,
    pub requires_approval: bool,
    pub approved_by: Option<String>,
    pub state: MessageState,
    pub created_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageQuery {
    /// Only messages with a larger id.
    #[serde(default)]
    pub since_id: Option<i64>,
    #[serde(default)]
    pub room: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub id: i64,
    pub project_id: i64,
    pub agent_id: Option<AgentId>,
    pub summary: String,
    pub rationale: String,
    pub evidence_ref: Option<i64>,
    pub status: DecisionStatus,
    pub decided_reason: Option<String>,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub project_id: i64,
    pub assigned_to_agent_id: Option<AgentId>,
    pub description: String,
    pub status: TaskStatus,
    pub depends_on: Vec<i64>,
    pub created_by_agent_id: Option<AgentId>,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: i64,
    pub project_id: i64,
    pub kind: ArtifactKind,
    pub path: String,
    pub sha256: String,
    pub created_by_agent_id: Option<AgentId>,
    pub evidence: serde_json::Value,
    pub created_at: u64,
}

/// One merge-queue entry (spec §7.2). Snapshot ids are hex strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeEntry {
    pub seq: i64,
    pub agent_id: Option<AgentId>,
    pub kind: MergeKind,
    pub base: String,
    pub submitted: String,
    pub pre: Option<String>,
    pub merged: Option<String>,
    pub status: MergeStatus,
    pub feedback: serde_json::Value,
    /// For a revert: the merge it undoes.
    pub reverts: Option<i64>,
    pub created_at: u64,
}
