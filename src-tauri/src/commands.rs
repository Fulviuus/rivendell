//! Tauri commands. These are the UI's only way in, and they go through the
//! same `Store` the MCP server uses so the rules cannot diverge.

use crate::error::{Error, Result};
use crate::models::*;
use crate::spawner::Spawner;
use crate::store::Store;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use tauri::State;

pub struct AppState {
    pub store: Arc<Store>,
    pub spawner: Arc<Spawner>,
}

#[derive(Serialize)]
pub struct NewAgentKey {
    pub agent_id: i64,
    pub api_key: String,
    pub mcp_json: String,
    pub claude_cli: String,
    pub shim_json: String,
}

#[derive(Serialize)]
pub struct ServerInfo {
    pub url: String,
    pub listening: bool,
}

// ---------------------------------------------------------------- server ---

#[tauri::command]
pub async fn server_info(state: State<'_, AppState>) -> Result<ServerInfo> {
    let url = state.spawner.url().await;
    Ok(ServerInfo {
        listening: !url.is_empty(),
        url,
    })
}

// -------------------------------------------------------------- projects ---

#[tauri::command]
pub fn list_projects(state: State<'_, AppState>) -> Result<Vec<Project>> {
    state.store.list_projects()
}

#[tauri::command]
pub fn create_project(state: State<'_, AppState>, name: String, folder: String) -> Result<Project> {
    state.store.create_project(&name, &folder)
}

#[tauri::command]
pub fn delete_project(state: State<'_, AppState>, id: i64) -> Result<()> {
    state.store.delete_project(id)
}

// ----------------------------------------------------------------- rooms ---

#[tauri::command]
pub fn list_rooms(state: State<'_, AppState>) -> Result<Vec<Room>> {
    state.store.list_rooms()
}

#[tauri::command]
pub fn create_room(
    state: State<'_, AppState>,
    project_id: i64,
    name: String,
    purpose: String,
) -> Result<i64> {
    let room_id = state.store.create_room(project_id, &name, &purpose)?;
    // Every room gets an identity for the person sitting in front of the app,
    // so you are a participant rather than a spectator.
    state
        .store
        .create_agent(room_id, "you", "HUMAN", None, "The human in the room.", false, "slate")?;
    Ok(room_id)
}

#[tauri::command]
pub fn update_room(state: State<'_, AppState>, id: i64, patch: Value) -> Result<()> {
    state.store.update_room(id, patch)
}

#[tauri::command]
pub fn delete_room(state: State<'_, AppState>, id: i64) -> Result<()> {
    state.store.delete_room(id)
}

// -------------------------------------------------------------- profiles ---

#[tauri::command]
pub fn list_profiles(state: State<'_, AppState>) -> Result<Vec<AgentProfile>> {
    state.store.list_profiles()
}

#[tauri::command]
pub fn upsert_profile(state: State<'_, AppState>, profile: Value) -> Result<i64> {
    state.store.upsert_profile(profile)
}

// ---------------------------------------------------------------- agents ---

#[tauri::command]
pub fn list_agents(state: State<'_, AppState>, room_id: Option<i64>) -> Result<Vec<Agent>> {
    state.store.list_agents(room_id)
}

#[tauri::command]
pub async fn create_agent(
    state: State<'_, AppState>,
    room_id: i64,
    name: String,
    role: String,
    profile_id: Option<i64>,
    system_note: String,
    auto_dispatch: bool,
    color: Option<String>,
) -> Result<NewAgentKey> {
    let (agent_id, api_key) = state.store.create_agent(
        room_id,
        &name,
        &role,
        profile_id,
        &system_note,
        auto_dispatch,
        color.as_deref().unwrap_or(""),
    )?;
    let url = state.spawner.url().await;
    Ok(connection_bundle(agent_id, api_key, &url))
}

#[tauri::command]
pub async fn rotate_agent_key(state: State<'_, AppState>, agent_id: i64) -> Result<NewAgentKey> {
    let api_key = state.store.rotate_key(agent_id)?;
    let url = state.spawner.url().await;
    Ok(connection_bundle(agent_id, api_key, &url))
}

#[tauri::command]
pub fn update_agent(state: State<'_, AppState>, agent_id: i64, patch: Value) -> Result<()> {
    state.store.update_agent(agent_id, patch)
}

#[tauri::command]
pub fn set_agent_revoked(state: State<'_, AppState>, agent_id: i64, revoked: bool) -> Result<()> {
    state.store.set_agent_revoked(agent_id, revoked)
}

#[tauri::command]
pub fn set_agent_auto_dispatch(
    state: State<'_, AppState>,
    agent_id: i64,
    enabled: bool,
) -> Result<()> {
    state.store.set_agent_auto_dispatch(agent_id, enabled)
}

#[tauri::command]
pub fn delete_agent(state: State<'_, AppState>, agent_id: i64) -> Result<()> {
    state.store.delete_agent(agent_id)
}

/// Everything needed to point a client at this agent. Assembled once, at the
/// only moment the plaintext key exists.
fn connection_bundle(agent_id: i64, api_key: String, url: &str) -> NewAgentKey {
    let mcp_json = serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": {
            "rivendell": {
                "type": "http",
                "url": url,
                "headers": { "Authorization": format!("Bearer {api_key}") }
            }
        }
    }))
    .unwrap_or_default();

    let shim_json = serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": {
            "rivendell": {
                "command": "rivendell-mcp",
                "env": { "RIVENDELL_URL": url, "RIVENDELL_KEY": api_key }
            }
        }
    }))
    .unwrap_or_default();

    NewAgentKey {
        agent_id,
        claude_cli: format!(
            "claude mcp add --transport http rivendell {url} --header \"Authorization: Bearer {api_key}\""
        ),
        api_key,
        mcp_json,
        shim_json,
    }
}

// ------------------------------------------------------------------ tags ---

#[tauri::command]
pub fn list_tags(state: State<'_, AppState>) -> Result<Vec<Tag>> {
    state.store.list_tags()
}

// --------------------------------------------------------------- threads ---

#[tauri::command]
pub fn list_threads(
    state: State<'_, AppState>,
    room_id: Option<i64>,
    status: Option<String>,
    tag: Option<String>,
    limit: Option<i64>,
) -> Result<Vec<ThreadSummary>> {
    state.store.list_threads(
        room_id,
        status.as_deref(),
        tag.as_deref(),
        None,
        limit.unwrap_or(200),
    )
}

#[tauri::command]
pub fn get_thread(state: State<'_, AppState>, thread_id: i64) -> Result<ThreadDetail> {
    state.store.thread_detail(thread_id)
}

/// The UI always acts as a concrete agent — by default the room's HUMAN.
fn actor(store: &Arc<Store>, room_id: i64, as_agent_id: Option<i64>) -> Result<crate::store::AgentCtx> {
    match as_agent_id {
        Some(id) => store.agent_ctx(id),
        None => {
            let agents = store.list_agents(Some(room_id))?;
            let human = agents
                .iter()
                .find(|a| a.role == "HUMAN")
                .or_else(|| agents.iter().find(|a| a.role == "CODER"))
                .ok_or_else(|| {
                    Error::Invalid("this room has no human or coder to post as".into())
                })?;
            store.agent_ctx(human.id)
        }
    }
}

#[tauri::command]
pub async fn create_thread(
    state: State<'_, AppState>,
    input: NewThread,
    as_agent_id: Option<i64>,
) -> Result<i64> {
    let ctx = actor(&state.store, input.room_id, as_agent_id)?;
    let thread_id = state.store.create_thread(&ctx, input)?;
    let spawner = state.spawner.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = spawner.dispatch(thread_id, None).await {
            tracing::warn!("dispatch failed: {e}");
        }
    });
    Ok(thread_id)
}

#[tauri::command]
pub fn reply(state: State<'_, AppState>, input: NewReply, as_agent_id: Option<i64>) -> Result<i64> {
    let detail = state.store.thread_detail(input.thread_id)?;
    let ctx = actor(&state.store, detail.summary.room_id, as_agent_id)?;
    state.store.reply(&ctx, input)
}

#[tauri::command]
pub fn update_thread(
    state: State<'_, AppState>,
    thread_id: i64,
    body: String,
    as_agent_id: Option<i64>,
) -> Result<()> {
    let detail = state.store.thread_detail(thread_id)?;
    let ctx = actor(&state.store, detail.summary.room_id, as_agent_id)?;
    state.store.update_thread_body(&ctx, thread_id, &body)
}

#[tauri::command]
pub fn resolve_thread(
    state: State<'_, AppState>,
    thread_id: i64,
    summary: String,
    status: Option<String>,
    as_agent_id: Option<i64>,
) -> Result<Option<String>> {
    let detail = state.store.thread_detail(thread_id)?;
    let ctx = actor(&state.store, detail.summary.room_id, as_agent_id)?;
    state.store.resolve_thread(
        &ctx,
        thread_id,
        &summary,
        status.as_deref().unwrap_or("RESOLVED"),
    )
}

#[tauri::command]
pub fn set_thread_status(
    state: State<'_, AppState>,
    thread_id: i64,
    status: String,
    as_agent_id: Option<i64>,
) -> Result<()> {
    let detail = state.store.thread_detail(thread_id)?;
    let ctx = actor(&state.store, detail.summary.room_id, as_agent_id)?;
    state.store.set_thread_status(&ctx, thread_id, &status)
}

#[tauri::command]
pub async fn dispatch_thread(
    state: State<'_, AppState>,
    thread_id: i64,
    agent_ids: Option<Vec<i64>>,
) -> Result<usize> {
    state.spawner.dispatch(thread_id, agent_ids).await
}

// ---------------------------------------------------------------- search ---

#[tauri::command]
pub fn search(
    state: State<'_, AppState>,
    room_id: Option<i64>,
    query: String,
    limit: Option<i64>,
) -> Result<Vec<Value>> {
    state.store.search(room_id, &query, limit.unwrap_or(40))
}

#[tauri::command]
pub fn events_since(
    state: State<'_, AppState>,
    cursor: i64,
    room_id: Option<i64>,
) -> Result<Vec<EventRow>> {
    state.store.events_since(cursor, room_id, 200)
}

#[tauri::command]
pub fn file_preview(
    state: State<'_, AppState>,
    room_id: i64,
    path: String,
    start_line: Option<i64>,
    end_line: Option<i64>,
) -> Result<Value> {
    let rooms = state.store.list_rooms()?;
    let room = rooms
        .iter()
        .find(|r| r.id == room_id)
        .ok_or_else(|| Error::NotFound(format!("room {room_id}")))?;
    let root = crate::fsjail::canonical_root(&room.folder_path)?;
    let s = crate::fsjail::read_slice(&root, &path, start_line, end_line)?;
    Ok(serde_json::json!({
        "path": s.path,
        "start_line": s.start_line,
        "end_line": s.end_line,
        "total_lines": s.total_lines,
        "content": s.content,
    }))
}

#[tauri::command]
pub fn list_project_files(
    state: State<'_, AppState>,
    room_id: i64,
    path: Option<String>,
) -> Result<Vec<String>> {
    let rooms = state.store.list_rooms()?;
    let room = rooms
        .iter()
        .find(|r| r.id == room_id)
        .ok_or_else(|| Error::NotFound(format!("room {room_id}")))?;
    let root = crate::fsjail::canonical_root(&room.folder_path)?;
    crate::fsjail::list_dir(&root, path.as_deref().unwrap_or(""), 3)
}

#[tauri::command]
pub fn git_status(state: State<'_, AppState>, room_id: i64) -> Result<Value> {
    let rooms = state.store.list_rooms()?;
    let room = rooms
        .iter()
        .find(|r| r.id == room_id)
        .ok_or_else(|| Error::NotFound(format!("room {room_id}")))?;
    let root = crate::fsjail::canonical_root(&room.folder_path)?;
    Ok(serde_json::json!({
        "is_repo": crate::git::is_repo(&root),
        "branch": crate::git::branch(&root),
        "head": crate::git::head(&root),
        "dirty": crate::git::is_dirty(&root),
    }))
}
