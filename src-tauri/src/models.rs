//! Wire types shared by the Tauri commands and the MCP server.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub folder_path: String,
    pub git_remote: Option<String>,
    pub color: String,
    pub created_at: String,
}

/// What deleting a project would destroy. Shown before the confirmation, so
/// the number is a fact rather than a guess.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectStats {
    pub rooms: i64,
    pub threads: i64,
    pub messages: i64,
    pub agents: i64,
    pub exported_records: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Room {
    pub id: i64,
    pub project_id: i64,
    pub project_name: String,
    pub folder_path: String,
    pub name: String,
    pub purpose: String,
    pub paused: bool,
    pub max_replies_per_agent: i64,
    pub max_thread_messages: i64,
    /// Seconds a thread waits on an assistant that has shown no sign of life.
    pub response_timeout_secs: i64,
    pub cost_cap_usd: f64,
    /// After the first agent answers, seconds the others get to claim.
    pub claim_window_secs: i64,
    pub open_threads: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    pub id: i64,
    pub key: String,
    pub label: String,
    pub icon: String,
    pub launch_cmd: String,
    pub launch_args: String,
    pub mcp_install_mode: String,
    pub notes: String,
    pub builtin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: i64,
    /// Agents belong to a project; `room_members` says which rooms they are in.
    pub project_id: i64,
    pub name: String,
    pub role: String,
    pub profile_id: Option<i64>,
    pub profile_key: Option<String>,
    pub profile_label: Option<String>,
    pub icon: String,
    pub color: String,
    pub key_preview: Option<String>,
    pub system_note: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub key: String,
    pub label: String,
    pub color: String,
    pub instruction: String,
    pub requires_verdict: bool,
    pub verdict_options: Vec<String>,
    /// Whether the tag expects any replies at all. FYI does not.
    pub expects_replies: bool,
    pub builtin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadContextItem {
    pub id: i64,
    pub kind: String,
    pub path: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub content: String,
}

/// An assistant saying "I have picked this up". Re-claiming is a heartbeat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadClaim {
    pub agent_id: i64,
    pub agent_name: String,
    pub color: String,
    pub icon: String,
    pub note: String,
    pub claimed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub thread_id: i64,
    pub agent_id: i64,
    pub agent_name: String,
    pub agent_role: String,
    pub icon: String,
    pub color: String,
    pub body: String,
    pub verdict: Option<String>,
    pub severity: Option<String>,
    pub refs: serde_json::Value,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost_usd: f64,
    pub created_at: String,
    /// Set once the author has revised it.
    pub edited_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: i64,
    pub room_id: i64,
    pub room_name: String,
    pub title: String,
    pub tag: String,
    pub status: String,
    pub author_agent_id: i64,
    pub author_name: String,
    pub author_icon: String,
    pub author_color: String,
    pub reply_count: i64,
    /// Distinct assistants that have answered.
    pub responder_count: i64,
    /// Assistants that said they are working on it and have not answered yet.
    pub in_progress: i64,
    pub cost_usd: f64,
    pub git_ref: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub resolved_at: Option<String>,
    /// When the newest reply landed; None until someone answers.
    pub last_reply_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadDetail {
    #[serde(flatten)]
    pub summary: ThreadSummary,
    pub body: String,
    pub git_dirty: bool,
    pub resolution_summary: Option<String>,
    pub export_path: Option<String>,
    pub context: Vec<ThreadContextItem>,
    pub mentions: Vec<i64>,
    pub claims: Vec<ThreadClaim>,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRow {
    pub seq: i64,
    pub room_id: Option<i64>,
    pub thread_id: Option<i64>,
    pub kind: String,
    pub actor_agent_id: Option<i64>,
    pub payload: serde_json::Value,
    pub created_at: String,
}

/// Broadcast to the webview and to long-polling agents.
#[derive(Debug, Clone, Serialize)]
pub struct EventNotice {
    pub seq: i64,
    pub room_id: Option<i64>,
    pub thread_id: Option<i64>,
    pub kind: String,
}

// ---------------------------------------------------------------- inputs ---

#[derive(Debug, Clone, Deserialize)]
pub struct ContextInput {
    pub kind: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub start_line: Option<i64>,
    #[serde(default)]
    pub end_line: Option<i64>,
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewThread {
    pub room_id: i64,
    pub title: String,
    pub body: String,
    pub tag: String,
    #[serde(default)]
    pub mentions: Vec<i64>,
    #[serde(default)]
    pub context: Vec<ContextInput>,
    #[serde(default)]
    pub include_diff: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewReply {
    pub thread_id: i64,
    pub body: String,
    #[serde(default)]
    pub verdict: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub refs: Option<serde_json::Value>,
    #[serde(default)]
    pub tokens_in: i64,
    #[serde(default)]
    pub tokens_out: i64,
    #[serde(default)]
    pub cost_usd: f64,
}

pub const SEVERITIES: &[&str] = &["CRITICAL", "HIGH", "MEDIUM", "LOW", "INFO"];

pub const STATUSES: &[&str] = &[
    "OPEN",
    "AWAITING_REPLIES",
    "NEEDS_CODER",
    "RESOLVED",
    "BLOCKED",
    "WONTFIX",
];

/// The three statuses that mean "still live work". Shared so the thread filter
/// and the room's unread badge cannot disagree about what Open means — a badge
/// showing 3 next to a list of 2 is exactly the kind of confusion worth
/// designing out.
pub const OPEN_STATUS_SQL: &str = "('OPEN','AWAITING_REPLIES','NEEDS_CODER')";
pub const DONE_STATUS_SQL: &str = "('RESOLVED','WONTFIX')";

pub fn is_terminal(status: &str) -> bool {
    matches!(status, "RESOLVED" | "WONTFIX")
}
