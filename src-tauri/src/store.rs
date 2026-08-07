//! All reads and writes go through here, so the Tauri commands and the MCP
//! server can never drift apart on rules like "who may resolve a thread".

use crate::auth;
use crate::error::{Error, Result};
use crate::models::*;
use crate::{db, export, git};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use tokio::sync::broadcast;

pub struct Store {
    conn: Mutex<Connection>,
    pub events: broadcast::Sender<EventNotice>,
    /// Who is holding a connection to the listener right now. In memory only,
    /// registered and released by the MCP handlers themselves.
    pub presence: std::sync::Arc<crate::presence::Presence>,
    /// Credentials for processes Rivendell started itself, keyed by digest.
    ///
    /// Only `sha256(key)` is ever persisted, so the app genuinely cannot read
    /// an agent's own key back out to hand to a child. It mints one of these
    /// instead: same identity, in memory only, gone when the run ends and
    /// certainly gone when the app quits. That is exactly the lifetime a
    /// spawned process should have.
    live_tokens: Mutex<std::collections::HashMap<String, LiveToken>>,
}

/// An `agents.id` is a bare rowid: not AUTOINCREMENT, and `delete_agent` really
/// deletes. Delete the newest agent and create another and the new one inherits
/// the id — so a token that remembered only the id would come back to life as
/// somebody else, in a different project, possibly with a different role and a
/// different jail root. Pinning `created_at` too makes the identity one that a
/// recreate cannot accidentally forge.
struct LiveToken {
    agent_id: i64,
    created_at: String,
    /// Last use, not mint time — see `LIVE_TOKEN_IDLE`.
    touched: std::time::Instant,
}

/// How long an unused live token stays valid.
///
/// A sliding window rather than a fixed lifetime, because the two things it has
/// to serve pull opposite ways: a watcher Rivendell starts may legitimately sit
/// for days, while a token leaked by a bug should stop working on its own. Idle
/// time separates them — the watcher keeps touching it, the leak does not.
const LIVE_TOKEN_IDLE: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// How to start an agent's CLI, read off its launch profile.
pub struct LaunchPlan {
    pub cmd: String,
    pub args: Vec<String>,
    pub mcp_install_mode: String,
}

/// Identity resolved from a bearer token, plus everything a tool call needs.
#[derive(Debug, Clone)]
pub struct AgentCtx {
    pub id: i64,
    pub name: String,
    pub role: String,
    pub project_id: i64,
    pub project_name: String,
    pub folder_path: String,
    /// True when Rivendell started this process itself. Such a run was started
    /// to do a named job and should finish it and exit — advice that is the
    /// exact opposite of what a resident session wants to hear.
    pub supervised: bool,
}

impl AgentCtx {
    /// The person sitting in front of the app, as opposed to a program.
    ///
    /// The only distinction left between participants, and the only one that
    /// was ever about something real. `role` still holds CODER or ASSISTANT for
    /// agents made before the council — nothing reads it, and the column stays
    /// because rebuilding this table is how the project lost data once.
    pub fn is_human(&self) -> bool {
        self.role == "HUMAN"
    }
    pub fn root(&self) -> Result<PathBuf> {
        crate::fsjail::canonical_root(&self.folder_path)
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

impl Store {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let conn = db::open(path)?;
        let (events, _) = broadcast::channel(1024);
        Ok(Self {
            conn: Mutex::new(conn),
            events,
            presence: crate::presence::Presence::new(),
            live_tokens: Mutex::new(std::collections::HashMap::new()),
        })
    }

    // ------------------------------------------------------- live tokens ---

    /// A credential for one spawned run. The caller must hold the returned
    /// handle and `drop_live_token` it when the process ends.
    ///
    /// The `rvdlive_` prefix is deliberately not `rvd_`, so `key_id_of` refuses
    /// it and one of these can never be mistaken for a real key — including by
    /// the database lookup, which would otherwise find nothing and say so in a
    /// confusing way.
    pub fn mint_live_token(&self, agent_id: i64) -> Result<(String, String)> {
        let created_at: String = {
            let conn = self.lock();
            conn.query_row(
                "SELECT created_at FROM agents WHERE id=?1",
                params![agent_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("agent {agent_id}")))?
        };
        let token = format!("rvdlive_{}", &auth::generate().full[4..]);
        let handle = auth::hash(&token);
        self.live_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                handle.clone(),
                LiveToken { agent_id, created_at, touched: std::time::Instant::now() },
            );
        Ok((token, handle))
    }

    pub fn drop_live_token(&self, handle: &str) {
        self.live_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(handle);
    }

    /// Every live token for one agent, for when it is revoked or deleted and
    /// whatever is running as it must stop being able to speak.
    pub fn drop_live_tokens_for(&self, agent_id: i64) {
        self.live_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, t| t.agent_id != agent_id);
    }

    /// `(agent_id, created_at)` for a presented token, or `None` if it is not
    /// one of ours or has aged out.
    fn live_identity(&self, token: &str) -> Option<(i64, String)> {
        let mut map = self.live_tokens.lock().unwrap_or_else(|e| e.into_inner());
        let handle = auth::hash(token);
        let t = map.get_mut(&handle)?;
        if t.touched.elapsed() > LIVE_TOKEN_IDLE {
            map.remove(&handle);
            return None;
        }
        t.touched = std::time::Instant::now();
        Some((t.agent_id, t.created_at.clone()))
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A poisoned lock means a previous caller panicked mid-query; the
        // connection itself is still usable and losing the whole app over it
        // would be worse than continuing.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ------------------------------------------------------------ events ---

    fn append_event(
        conn: &Connection,
        room_id: Option<i64>,
        thread_id: Option<i64>,
        kind: &str,
        actor: Option<i64>,
        payload: serde_json::Value,
    ) -> Result<EventNotice> {
        conn.execute(
            "INSERT INTO events(room_id,thread_id,kind,actor_agent_id,payload,created_at)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![room_id, thread_id, kind, actor, payload.to_string(), now()],
        )?;
        Ok(EventNotice {
            seq: conn.last_insert_rowid(),
            room_id,
            thread_id,
            kind: kind.to_string(),
            actor_agent_id: actor,
        })
    }

    fn publish(&self, notice: EventNotice) {
        // The count is how many long polls are about to be woken. A zero here
        // while an agent is supposedly waiting is the whole answer to "why did
        // nothing happen", and it is invisible without saying it.
        match self.events.send(notice.clone()) {
            Ok(n) => tracing::info!("event {} {} -> {n} listener(s)", notice.seq, notice.kind),
            Err(_) => tracing::info!("event {} {} -> nobody listening", notice.seq, notice.kind),
        }
    }

    pub fn latest_seq(&self) -> Result<i64> {
        let conn = self.lock();
        Ok(conn
            .query_row("SELECT COALESCE(MAX(seq),0) FROM events", [], |r| r.get(0))
            .unwrap_or(0))
    }

    pub fn events_since(&self, cursor: i64, room_id: Option<i64>, limit: i64) -> Result<Vec<EventRow>> {
        let conn = self.lock();
        let mut sql = String::from(
            "SELECT seq,room_id,thread_id,kind,actor_agent_id,payload,created_at
             FROM events WHERE seq > ?1",
        );
        let mut ps: Vec<rusqlite::types::Value> = vec![cursor.into()];
        if let Some(r) = room_id {
            sql.push_str(" AND room_id = ?2");
            ps.push(r.into());
        }
        sql.push_str(" ORDER BY seq ASC LIMIT ");
        sql.push_str(&limit.clamp(1, 500).to_string());

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(ps), |r| {
                Ok(EventRow {
                    seq: r.get(0)?,
                    room_id: r.get(1)?,
                    thread_id: r.get(2)?,
                    kind: r.get(3)?,
                    actor_agent_id: r.get(4)?,
                    payload: serde_json::from_str(&r.get::<_, String>(5)?)
                        .unwrap_or(serde_json::Value::Null),
                    created_at: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---------------------------------------------------------- projects ---

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id,name,folder_path,git_remote,color,created_at FROM projects ORDER BY name",
        )?;
        let out = stmt
            .query_map([], |r| {
                Ok(Project {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    folder_path: r.get(2)?,
                    git_remote: r.get(3)?,
                    color: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    pub fn create_project(&self, name: &str, folder: &str) -> Result<Project> {
        let root = crate::fsjail::canonical_root(folder)?;
        if !root.is_dir() {
            return Err(Error::Invalid(format!("{folder} is not a directory")));
        }
        let folder = root.to_string_lossy().to_string();
        let name = if name.trim().is_empty() {
            root.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".into())
        } else {
            name.trim().to_string()
        };
        let remote = git::remote(&root);

        let conn = self.lock();
        conn.execute(
            "INSERT INTO projects(name,folder_path,git_remote,created_at) VALUES(?1,?2,?3,?4)",
            params![name, folder, remote, now()],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _) if f.extended_code == 2067 => {
                Error::Invalid("that folder already has a project".into())
            }
            other => other.into(),
        })?;
        let id = conn.last_insert_rowid();
        let notice = Self::append_event(&conn, None, None, "project.created", None, serde_json::json!({"id": id}))?;
        drop(conn);
        self.publish(notice);

        Ok(Project {
            id,
            name,
            folder_path: folder,
            git_remote: remote_of(&root),
            color: String::new(),
            created_at: now(),
        })
    }

    /// Rename, re-point at a different folder, or recolour.
    ///
    /// Moving the folder does not rewrite history: context pinned on existing
    /// threads was snapshotted and stays as it was. It changes where agents
    /// read from next.
    pub fn update_project(&self, id: i64, patch: serde_json::Value) -> Result<()> {
        if let Some(folder) = patch.get("folder_path").and_then(|v| v.as_str()) {
            let root = crate::fsjail::canonical_root(folder)?;
            if !root.is_dir() {
                return Err(Error::Invalid(format!("{folder} is not a directory")));
            }
            let conn = self.lock();
            conn.execute(
                "UPDATE projects SET folder_path=?1, git_remote=?2 WHERE id=?3",
                params![root.to_string_lossy(), git::remote(&root), id],
            )
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(f, _) if f.extended_code == 2067 => {
                    Error::Invalid("another project already uses that folder".into())
                }
                other => Error::from(other),
            })?;
        }
        {
            let conn = self.lock();
            for key in ["name", "color"] {
                let Some(v) = patch.get(key).and_then(|v| v.as_str()) else {
                    continue;
                };
                let v = v.trim();
                if key == "name" && v.is_empty() {
                    return Err(Error::Invalid("a project needs a name".into()));
                }
                conn.execute(
                    &format!("UPDATE projects SET {key}=?1 WHERE id=?2"),
                    params![v, id],
                )?;
            }
        }
        let conn = self.lock();
        let notice = Self::append_event(&conn, None, None, "project.updated", None, patch)?;
        drop(conn);
        self.publish(notice);
        Ok(())
    }

    pub fn project_stats(&self, id: i64) -> Result<ProjectStats> {
        let conn = self.lock();
        let one = |sql: &str| -> Result<i64> {
            Ok(conn.query_row(sql, params![id], |r| r.get(0))?)
        };
        Ok(ProjectStats {
            rooms: one("SELECT COUNT(*) FROM rooms WHERE project_id=?1")?,
            threads: one(
                "SELECT COUNT(*) FROM threads t JOIN rooms r ON r.id=t.room_id
                 WHERE r.project_id=?1",
            )?,
            messages: one(
                "SELECT COUNT(*) FROM messages m JOIN threads t ON t.id=m.thread_id
                 JOIN rooms r ON r.id=t.room_id WHERE r.project_id=?1",
            )?,
            agents: one("SELECT COUNT(*) FROM agents WHERE project_id=?1")?,
            // These live in the repo and survive the delete — worth saying so.
            exported_records: one(
                "SELECT COUNT(*) FROM threads t JOIN rooms r ON r.id=t.room_id
                 WHERE r.project_id=?1 AND t.export_path IS NOT NULL",
            )?,
        })
    }

    pub fn delete_project(&self, id: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM projects WHERE id=?1", params![id])?;
        let notice = Self::append_event(&conn, None, None, "project.deleted", None, serde_json::json!({"id": id}))?;
        drop(conn);
        self.publish(notice);
        Ok(())
    }

    // ------------------------------------------------------------- rooms ---

    pub fn list_rooms(&self) -> Result<Vec<Room>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT r.id,r.project_id,p.name,p.folder_path,r.name,r.purpose,r.paused,
                    r.max_replies_per_agent,r.max_thread_messages,r.response_timeout_secs,
                    r.cost_cap_usd,r.claim_window_secs,r.created_at,
                    (SELECT COUNT(*) FROM threads t
                      WHERE t.room_id=r.id AND t.status IN ('OPEN','AWAITING_REPLIES','NEEDS_CODER'))
             FROM rooms r JOIN projects p ON p.id=r.project_id
             ORDER BY p.name, r.name",
        )?;
        let out = stmt
            .query_map([], |r| {
                Ok(Room {
                    id: r.get(0)?,
                    project_id: r.get(1)?,
                    project_name: r.get(2)?,
                    folder_path: r.get(3)?,
                    name: r.get(4)?,
                    purpose: r.get(5)?,
                    paused: r.get::<_, i64>(6)? != 0,
                    max_replies_per_agent: r.get(7)?,
                    max_thread_messages: r.get(8)?,
                    response_timeout_secs: r.get(9)?,
                    cost_cap_usd: r.get(10)?,
                    claim_window_secs: r.get(11)?,
                    created_at: r.get(12)?,
                    open_threads: r.get(13)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    pub fn create_room(&self, project_id: i64, name: &str, purpose: &str) -> Result<i64> {
        let name = normalize_room_name(name)?;
        let conn = self.lock();
        conn.execute(
            "INSERT INTO rooms(project_id,name,purpose,created_at) VALUES(?1,?2,?3,?4)",
            params![project_id, name, purpose.trim(), now()],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _) if f.extended_code == 2067 => {
                Error::Invalid(format!("this project already has a room called {name}"))
            }
            other => other.into(),
        })?;
        let id = conn.last_insert_rowid();
        let notice = Self::append_event(&conn, Some(id), None, "room.created", None, serde_json::json!({"id": id}))?;
        drop(conn);
        self.publish(notice);
        Ok(id)
    }

    pub fn update_room(&self, id: i64, patch: serde_json::Value) -> Result<()> {
        let conn = self.lock();
        for (col, key) in [
            ("purpose", "purpose"),
            ("paused", "paused"),
            ("max_replies_per_agent", "max_replies_per_agent"),
            ("response_timeout_secs", "response_timeout_secs"),
            ("max_thread_messages", "max_thread_messages"),
            ("cost_cap_usd", "cost_cap_usd"),
            ("claim_window_secs", "claim_window_secs"),
        ] {
            let Some(v) = patch.get(key) else { continue };
            let sql = format!("UPDATE rooms SET {col}=?1 WHERE id=?2");
            match v {
                serde_json::Value::Bool(b) => conn.execute(&sql, params![*b as i64, id])?,
                serde_json::Value::Number(n) if n.is_f64() => {
                    conn.execute(&sql, params![n.as_f64().unwrap_or(0.0), id])?
                }
                serde_json::Value::Number(n) => {
                    conn.execute(&sql, params![n.as_i64().unwrap_or(0), id])?
                }
                serde_json::Value::String(s) => conn.execute(&sql, params![s, id])?,
                _ => 0,
            };
        }
        let notice = Self::append_event(&conn, Some(id), None, "room.updated", None, patch)?;
        drop(conn);
        self.publish(notice);
        Ok(())
    }

    pub fn delete_room(&self, id: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM rooms WHERE id=?1", params![id])?;
        let notice = Self::append_event(&conn, Some(id), None, "room.deleted", None, serde_json::json!({"id": id}))?;
        drop(conn);
        self.publish(notice);
        Ok(())
    }

    fn room_folder(conn: &Connection, room_id: i64) -> Result<String> {
        conn.query_row(
            "SELECT p.folder_path FROM rooms r JOIN projects p ON p.id=r.project_id WHERE r.id=?1",
            params![room_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("room {room_id}")))
    }

    // ---------------------------------------------------------- profiles ---

    pub fn list_profiles(&self) -> Result<Vec<AgentProfile>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id,key,label,icon,launch_cmd,launch_args,mcp_install_mode,notes,builtin
             FROM agent_profiles ORDER BY builtin DESC, label",
        )?;
        let out = stmt
            .query_map([], |r| {
                Ok(AgentProfile {
                    id: r.get(0)?,
                    key: r.get(1)?,
                    label: r.get(2)?,
                    icon: r.get(3)?,
                    launch_cmd: r.get(4)?,
                    launch_args: r.get(5)?,
                    mcp_install_mode: r.get(6)?,
                    notes: r.get(7)?,
                    builtin: r.get::<_, i64>(8)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    pub fn upsert_profile(&self, p: serde_json::Value) -> Result<i64> {
        let key = p
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Invalid("profile needs a key".into()))?;
        let args = p.get("launch_args").cloned().unwrap_or(serde_json::json!([]));
        if serde_json::from_value::<Vec<String>>(args.clone()).is_err() {
            return Err(Error::Invalid("launch_args must be an array of strings".into()));
        }
        let conn = self.lock();
        conn.execute(
            "INSERT INTO agent_profiles(key,label,icon,launch_cmd,launch_args,mcp_install_mode,notes,builtin)
             VALUES(?1,?2,?3,?4,?5,?6,?7,0)
             ON CONFLICT(key) DO UPDATE SET
               label=excluded.label, icon=excluded.icon, launch_cmd=excluded.launch_cmd,
               launch_args=excluded.launch_args, mcp_install_mode=excluded.mcp_install_mode,
               notes=excluded.notes",
            params![
                key,
                p.get("label").and_then(|v| v.as_str()).unwrap_or(key),
                p.get("icon").and_then(|v| v.as_str()).unwrap_or("robot"),
                p.get("launch_cmd").and_then(|v| v.as_str()).unwrap_or(""),
                args.to_string(),
                p.get("mcp_install_mode").and_then(|v| v.as_str()).unwrap_or("env"),
                p.get("notes").and_then(|v| v.as_str()).unwrap_or(""),
            ],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM agent_profiles WHERE key=?1",
            params![key],
            |r| r.get(0),
        )?)
    }

    // -------------------------------------------------------------- tags ---

    pub fn list_tags(&self) -> Result<Vec<Tag>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT key,label,color,instruction,requires_verdict,verdict_options,default_quorum,builtin
             FROM tags ORDER BY sort, label",
        )?;
        let out = stmt
            .query_map([], |r| {
                Ok(Tag {
                    key: r.get(0)?,
                    label: r.get(1)?,
                    color: r.get(2)?,
                    instruction: r.get(3)?,
                    requires_verdict: r.get::<_, i64>(4)? != 0,
                    verdict_options: serde_json::from_str(&r.get::<_, String>(5)?)
                        .unwrap_or_default(),
                    expects_replies: r.get::<_, i64>(6)? != 0,
                    builtin: r.get::<_, i64>(7)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    fn tag(conn: &Connection, key: &str) -> Result<Tag> {
        conn.query_row(
            "SELECT key,label,color,instruction,requires_verdict,verdict_options,default_quorum,builtin
             FROM tags WHERE key=?1",
            params![key],
            |r| {
                Ok(Tag {
                    key: r.get(0)?,
                    label: r.get(1)?,
                    color: r.get(2)?,
                    instruction: r.get(3)?,
                    requires_verdict: r.get::<_, i64>(4)? != 0,
                    verdict_options: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
                    expects_replies: r.get::<_, i64>(6)? != 0,
                    builtin: r.get::<_, i64>(7)? != 0,
                })
            },
        )
        .optional()?
        .ok_or_else(|| Error::Invalid(format!("unknown tag `{key}`")))
    }

    // ------------------------------------------------------------ agents ---

    /// Every agent, or only those in one room. `room_id` filters by
    /// membership, `project_id` by ownership.
    pub fn list_agents(&self, room_id: Option<i64>) -> Result<Vec<Agent>> {
        let conn = self.lock();
        let mut sql = String::from(
            "SELECT a.id,a.project_id,a.name,a.role,a.profile_id,p.key,p.label,
                    COALESCE(p.icon, CASE a.role WHEN 'HUMAN' THEN 'user' ELSE 'robot' END),
                    a.color,a.key_preview,a.system_note,a.created_at,a.revoked_at,a.awake
             FROM agents a LEFT JOIN agent_profiles p ON p.id=a.profile_id",
        );
        let mut ps: Vec<rusqlite::types::Value> = vec![];
        if let Some(r) = room_id {
            sql.push_str(
                " WHERE EXISTS(SELECT 1 FROM room_members m
                                WHERE m.agent_id=a.id AND m.room_id=?1)",
            );
            ps.push(r.into());
        }
        // You first, then everyone else alphabetically. There is no rank left
        // to sort by, and inventing one would be the whole problem again.
        sql.push_str(" ORDER BY CASE a.role WHEN 'HUMAN' THEN 0 ELSE 1 END, a.name");

        let mut stmt = conn.prepare(&sql)?;
        let out = stmt
            .query_map(params_from_iter(ps), |r| {
                Ok(Agent {
                    id: r.get(0)?,
                    project_id: r.get(1)?,
                    name: r.get(2)?,
                    role: r.get(3)?,
                    profile_id: r.get(4)?,
                    profile_key: r.get(5)?,
                    profile_label: r.get(6)?,
                    icon: r.get(7)?,
                    color: r.get(8)?,
                    key_preview: r.get(9)?,
                    system_note: r.get(10)?,
                    created_at: r.get(11)?,
                    revoked_at: r.get(12)?,
                    awake: r.get::<_, i64>(13)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    /// The presence registry joined with who each agent actually is: name and
    /// face, the project it is listening to, and the rooms it hears. Rows are
    /// looked up fresh rather than snapshotted at connect time, so a rename
    /// shows up without waiting for the agent to reconnect.
    pub fn connected_agents(&self) -> Result<Vec<crate::presence::ConnectedAgent>> {
        let present = self.presence.snapshot();
        if present.is_empty() {
            return Ok(Vec::new());
        }
        let agents = self.list_agents(None)?;
        let projects = self.list_projects()?;
        let rooms = self.list_rooms()?;

        let mut out = Vec::new();
        for p in present {
            // Deleted while its socket was still open — the registry will
            // notice when the connection dies; until then there is nobody to
            // show.
            let Some(agent) = agents.iter().find(|a| a.id == p.agent_id) else {
                continue;
            };
            let Some(project) = projects.iter().find(|pr| pr.id == agent.project_id) else {
                continue;
            };
            let joined = self.rooms_for(agent.id)?;
            let room_names = rooms
                .iter()
                .filter(|r| joined.contains(&r.id))
                .map(|r| r.name.clone())
                .collect();
            out.push(crate::presence::ConnectedAgent {
                agent_id: agent.id,
                name: agent.name.clone(),
                icon: agent.icon.clone(),
                color: agent.color.clone(),
                profile_label: agent.profile_label.clone(),
                project_id: project.id,
                project_name: project.name.clone(),
                project_color: project.color.clone(),
                folder_path: project.folder_path.clone(),
                rooms: room_names,
                connections: p.connections,
                last_seen: p.last_seen,
            });
        }
        // Holding a connection outranks having held one; names keep the list
        // from reshuffling every time a poll breathes.
        out.sort_by(|a, b| {
            (a.connections.is_empty())
                .cmp(&b.connections.is_empty())
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(out)
    }

    /// Returns the one and only plaintext view of the new key.
    pub fn create_agent(
        &self,
        project_id: i64,
        name: &str,
        role: &str,
        profile_id: Option<i64>,
        system_note: &str,
        color: &str,
    ) -> Result<(i64, String)> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Invalid("an agent needs a name".into()));
        }
        if !["CODER", "ASSISTANT", "HUMAN"].contains(&role) {
            return Err(Error::Invalid(format!("unknown role `{role}`")));
        }
        let key = auth::generate();

        let conn = self.lock();
        conn.execute(
            "INSERT INTO agents(project_id,name,role,profile_id,key_id,key_hash,key_preview,
                                system_note,color,created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                project_id,
                name,
                role,
                profile_id,
                key.key_id,
                key.hash,
                key.preview,
                system_note.trim(),
                color.trim(),
                now()
            ],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _) if f.extended_code == 2067 => {
                Error::Invalid(format!("this project already has an agent called {name}"))
            }
            other => other.into(),
        })?;
        let id = conn.last_insert_rowid();
        let notice = Self::append_event(
            &conn,
            None,
            None,
            "agent.created",
            None,
            serde_json::json!({"id": id, "name": name, "role": role}),
        )?;
        drop(conn);
        self.publish(notice);
        Ok((id, key.full))
    }

    pub fn rotate_key(&self, agent_id: i64) -> Result<String> {
        let key = auth::generate();
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE agents SET key_id=?1,key_hash=?2,key_preview=?3,revoked_at=NULL WHERE id=?4",
            params![key.key_id, key.hash, key.preview, agent_id],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("agent {agent_id}")));
        }
        drop(conn);
        // Rotating is how you cut off a key you no longer trust. Anything
        // already running on the old one must stop being that agent.
        self.drop_live_tokens_for(agent_id);
        Ok(key.full)
    }

    pub fn set_agent_revoked(&self, agent_id: i64, revoked: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE agents SET revoked_at=?1 WHERE id=?2",
            params![if revoked { Some(now()) } else { None }, agent_id],
        )?;
        drop(conn);
        if revoked {
            self.drop_live_tokens_for(agent_id);
        }
        Ok(())
    }

    /// Edits the mutable parts of an agent. The key and room are not among
    /// them — rotate or recreate for those.
    pub fn update_agent(&self, agent_id: i64, patch: serde_json::Value) -> Result<()> {
        {
            let conn = self.lock();
            for (col, key) in [
                ("name", "name"),
                ("system_note", "system_note"),
                ("color", "color"),
                ("profile_id", "profile_id"),
            ] {
                let Some(v) = patch.get(key) else { continue };
                let sql = format!("UPDATE agents SET {col}=?1 WHERE id=?2");
                let n = match v {
                    serde_json::Value::Bool(b) => conn.execute(&sql, params![*b as i64, agent_id]),
                    serde_json::Value::Number(n) => {
                        conn.execute(&sql, params![n.as_i64().unwrap_or(0), agent_id])
                    }
                    serde_json::Value::String(s) => {
                        let s = s.trim();
                        if col == "name" && s.is_empty() {
                            return Err(Error::Invalid("an agent needs a name".into()));
                        }
                        conn.execute(&sql, params![s, agent_id])
                    }
                    serde_json::Value::Null if col == "profile_id" => {
                        conn.execute(&sql, params![None::<i64>, agent_id])
                    }
                    _ => Ok(0),
                };
                n.map_err(|e| match e {
                    rusqlite::Error::SqliteFailure(f, _) if f.extended_code == 2067 => {
                        Error::Invalid("this project already has an agent with that name".into())
                    }
                    other => Error::from(other),
                })?;
            }
        }
        let conn = self.lock();
        let notice = Self::append_event(
            &conn,
            None,
            None,
            "agent.updated",
            Some(agent_id),
            patch,
        )?;
        drop(conn);
        self.publish(notice);
        Ok(())
    }

    pub fn delete_agent(&self, agent_id: i64) -> Result<()> {
        {
            let conn = self.lock();
            conn.execute("DELETE FROM agents WHERE id=?1", params![agent_id])?;
        }
        // Before anything can reuse this rowid.
        self.drop_live_tokens_for(agent_id);
        Ok(())
    }

    /// Bearer-token lookup. Returns `None` for unknown, malformed or revoked keys.
    pub fn authenticate(&self, token: &str) -> Result<Option<AgentCtx>> {
        // A token the app minted for a process it started. Revocation still
        // applies: revoking an agent has to cut off whatever is already running
        // as it, not just refuse the next connection.
        if let Some((agent_id, created_at)) = self.live_identity(token) {
            let conn = self.lock();
            return Ok(conn
                .query_row(
                    "SELECT a.id,a.name,a.role,p.id,p.name,p.folder_path
                     FROM agents a JOIN projects p ON p.id=a.project_id
                     WHERE a.id=?1 AND a.created_at=?2 AND a.revoked_at IS NULL",
                    params![agent_id, created_at],
                    |r| {
                        Ok(AgentCtx {
                            id: r.get(0)?,
                            name: r.get(1)?,
                            role: r.get(2)?,
                            project_id: r.get(3)?,
                            project_name: r.get(4)?,
                            folder_path: r.get(5)?,
                            supervised: true,
                        })
                    },
                )
                .optional()?);
        }

        let Some(key_id) = auth::key_id_of(token) else {
            return Ok(None);
        };
        let conn = self.lock();
        let row: Option<(i64, String, String, i64, String, String, String)> = conn
            .query_row(
                "SELECT a.id,a.name,a.role,p.id,p.name,p.folder_path,COALESCE(a.key_hash,'')
                 FROM agents a
                 JOIN projects p ON p.id=a.project_id
                 WHERE a.key_id=?1 AND a.revoked_at IS NULL",
                params![key_id],
                |r| {
                    Ok((
                        r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?,
                    ))
                },
            )
            .optional()?;

        let Some((id, name, role, project_id, project_name, folder_path, hash)) = row else {
            return Ok(None);
        };
        if !auth::verify(token, &hash) {
            return Ok(None);
        }
        Ok(Some(AgentCtx {
            id,
            name,
            role,
            project_id,
            project_name,
            folder_path,
            supervised: false,
        }))
    }

    /// Still allowed to speak? Checked inside the long poll, which can outlive
    /// a revocation by up to an hour otherwise.
    pub fn agent_is_live(&self, agent_id: i64) -> Result<bool> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT 1 FROM agents WHERE id=?1 AND revoked_at IS NULL")?;
        Ok(stmt.exists(params![agent_id])?)
    }

    /// Rooms this agent has joined.
    pub fn rooms_for(&self, agent_id: i64) -> Result<Vec<i64>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT room_id FROM room_members WHERE agent_id=?1 ORDER BY room_id")?;
        let out = stmt
            .query_map(params![agent_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(out)
    }

    /// Membership is what room isolation now rests on: an agent may only touch
    /// a room it has actually joined, even within its own project.
    fn require_member(conn: &Connection, agent_id: i64, room_id: i64) -> Result<()> {
        let member: bool = conn
            .prepare("SELECT 1 FROM room_members WHERE room_id=?1 AND agent_id=?2")?
            .exists(params![room_id, agent_id])?;
        if member {
            Ok(())
        } else {
            Err(Error::Forbidden(
                "you are not in that room".into(),
            ))
        }
    }

    fn room_paused(conn: &Connection, room_id: i64) -> Result<bool> {
        Ok(conn.query_row("SELECT paused FROM rooms WHERE id=?1", params![room_id], |r| {
            r.get::<_, i64>(0)
        })? != 0)
    }

    fn room_name(conn: &Connection, room_id: i64) -> Result<String> {
        Ok(conn.query_row("SELECT name FROM rooms WHERE id=?1", params![room_id], |r| r.get(0))?)
    }

    pub fn join_room(&self, room_id: i64, agent_id: i64) -> Result<()> {
        let conn = self.lock();
        let same: bool = conn
            .prepare(
                "SELECT 1 FROM agents a JOIN rooms r ON r.project_id=a.project_id
                 WHERE a.id=?1 AND r.id=?2",
            )?
            .exists(params![agent_id, room_id])?;
        if !same {
            return Err(Error::Forbidden(
                "an agent can only join rooms in its own project".into(),
            ));
        }
        conn.execute(
            "INSERT OR IGNORE INTO room_members(room_id,agent_id,joined_at) VALUES(?1,?2,?3)",
            params![room_id, agent_id, now()],
        )?;
        let notice = Self::append_event(
            &conn,
            Some(room_id),
            None,
            "agent.joined",
            Some(agent_id),
            serde_json::json!({}),
        )?;
        drop(conn);
        self.publish(notice);
        Ok(())
    }

    pub fn leave_room(&self, room_id: i64, agent_id: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM room_members WHERE room_id=?1 AND agent_id=?2",
            params![room_id, agent_id],
        )?;
        let notice = Self::append_event(
            &conn,
            Some(room_id),
            None,
            "agent.left",
            Some(agent_id),
            serde_json::json!({}),
        )?;
        drop(conn);
        self.publish(notice);
        Ok(())
    }

    // ------------------------------------------------------------- awake ---

    /// Turn Rivendell's own supervision of this agent on or off.
    pub fn set_agent_awake(&self, agent_id: i64, awake: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE agents SET awake=?1 WHERE id=?2",
            params![awake as i64, agent_id],
        )?;
        Ok(())
    }

    pub fn awake_agent_ids(&self) -> Result<Vec<i64>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT id FROM agents WHERE awake=1 AND revoked_at IS NULL")?;
        let out = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    /// Awake agents in one room — the candidates an event in that room could wake.
    pub fn awake_agents_in_room(&self, room_id: i64) -> Result<Vec<i64>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT a.id FROM agents a
             JOIN room_members m ON m.agent_id=a.id
             WHERE m.room_id=?1 AND a.awake=1 AND a.revoked_at IS NULL AND a.role<>'HUMAN'",
        )?;
        let out = stmt
            .query_map(params![room_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    /// Of `threads`, the ones this agent could still usefully act on.
    ///
    /// This is what keeps two awake agents from talking each other's budget
    /// away. The reply caps already refuse the message, but without this the
    /// agent still gets started, reads the room, discovers it may not speak and
    /// exits — a full billable run to accomplish nothing. Checking first costs
    /// one query.
    pub fn wakeable_threads(&self, agent_id: i64, threads: &[i64]) -> Result<Vec<i64>> {
        if threads.is_empty() {
            return Ok(vec![]);
        }
        let conn = self.lock();
        let holes = vec!["?"; threads.len()].join(",");
        let sql = format!(
            "SELECT t.id
             FROM threads t
             JOIN rooms r ON r.id=t.room_id
             JOIN room_members m ON m.room_id=t.room_id AND m.agent_id=?1
             WHERE t.id IN ({holes})
               AND t.status NOT IN ('RESOLVED','WONTFIX')
               AND r.paused=0
               AND (EXISTS(SELECT 1 FROM thread_mentions x
                            WHERE x.thread_id=t.id AND x.agent_id=?1)
                    OR t.author_agent_id=?1)

               AND (SELECT COUNT(*) FROM messages WHERE thread_id=t.id AND agent_id=?1)
                     < r.max_replies_per_agent
               AND (SELECT COUNT(*) FROM messages WHERE thread_id=t.id)
                     < r.max_thread_messages
             ORDER BY t.id"
        );
        let mut ps: Vec<rusqlite::types::Value> = vec![agent_id.into()];
        ps.extend(threads.iter().map(|t| rusqlite::types::Value::from(*t)));
        let mut stmt = conn.prepare(&sql)?;
        let out = stmt
            .query_map(params_from_iter(ps), |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    /// Open threads already waiting on this agent, for a watcher that has just
    /// come up.
    ///
    /// Work does not stop existing because nobody was listening when it
    /// arrived. A watcher that only reacted to events after its own start would
    /// ignore a thread opened while the app was closed — for ever, or until
    /// somebody happened to post again.
    ///
    /// "Waiting on this agent" means the newest thing said is not its own: a
    /// thread it has already answered is the coder's turn, not another prompt
    /// to answer twice.
    pub fn wakeable_open_threads(&self, agent_id: i64) -> Result<Vec<i64>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT t.id
             FROM threads t
             JOIN rooms r ON r.id=t.room_id
             JOIN room_members m ON m.room_id=t.room_id AND m.agent_id=?1
             WHERE t.status NOT IN ('RESOLVED','WONTFIX')
               AND r.paused=0
               AND (EXISTS(SELECT 1 FROM thread_mentions x
                            WHERE x.thread_id=t.id AND x.agent_id=?1)
                    OR t.author_agent_id=?1)
               AND COALESCE(
                     (SELECT agent_id FROM messages WHERE thread_id=t.id ORDER BY id DESC LIMIT 1),
                     t.author_agent_id
                   ) <> ?1
               AND (SELECT COUNT(*) FROM messages WHERE thread_id=t.id AND agent_id=?1)
                     < r.max_replies_per_agent
               AND (SELECT COUNT(*) FROM messages WHERE thread_id=t.id)
                     < r.max_thread_messages
             ORDER BY t.id",
        )?;
        let out = stmt
            .query_map(params![agent_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    /// How to start an agent, or why it cannot be started.
    pub fn launch_plan(&self, agent_id: i64) -> Result<LaunchPlan> {
        let conn = self.lock();
        let row: Option<(String, String, String, String)> = conn
            .query_row(
                "SELECT a.name, COALESCE(p.key,''), COALESCE(p.launch_cmd,''),
                        COALESCE(p.launch_args,'[]')
                 FROM agents a LEFT JOIN agent_profiles p ON p.id=a.profile_id
                 WHERE a.id=?1",
                params![agent_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((name, key, cmd, args_json)) = row else {
            return Err(Error::NotFound(format!("agent {agent_id}")));
        };
        if cmd.trim().is_empty() || key == "external" {
            return Err(Error::Invalid(format!(
                "Rivendell does not know how to start {name}. Give it a kind that carries a \
                 launch command, or run it yourself."
            )));
        }
        let args: Vec<String> = serde_json::from_str(&args_json)
            .map_err(|e| Error::Invalid(format!("profile `{key}` has bad launch_args: {e}")))?;
        let mode: String = conn.query_row(
            "SELECT COALESCE(p.mcp_install_mode,'none') FROM agents a
             LEFT JOIN agent_profiles p ON p.id=a.profile_id WHERE a.id=?1",
            params![agent_id],
            |r| r.get(0),
        )?;
        Ok(LaunchPlan { cmd, args, mcp_install_mode: mode })
    }

    pub fn agent_ctx(&self, agent_id: i64) -> Result<AgentCtx> {
        let conn = self.lock();
        conn.query_row(
            "SELECT a.id,a.name,a.role,p.id,p.name,p.folder_path
             FROM agents a JOIN projects p ON p.id=a.project_id
             WHERE a.id=?1",
            params![agent_id],
            |r| {
                Ok(AgentCtx {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    role: r.get(2)?,
                    project_id: r.get(3)?,
                    project_name: r.get(4)?,
                    folder_path: r.get(5)?,
                    supervised: false,
                })
            },
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("agent {agent_id}")))
    }


    /// Pull `@name` out of a message and add those agents to the thread.
    ///
    /// This is how an agent calls in another: an assistant that needs a second
    /// opinion writes `@auditor` and the thread now addresses them too. The new
    /// participant is announced on the event log, and the gather window reopens
    /// so they get the same chance to answer that everyone else had — otherwise
    /// being called in late would mean being ignored on arrival.
    fn apply_body_mentions(
        conn: &Connection,
        thread_id: i64,
        room_id: i64,
        body: &str,
        by: i64,
    ) -> Result<Vec<String>> {
        let names = parse_mentions(body);
        if names.is_empty() {
            return Ok(vec![]);
        }

        // Only agents who are actually in this room can be summoned into it.
        let mut stmt = conn.prepare(
            "SELECT a.id, a.name FROM agents a
             JOIN room_members m ON m.agent_id=a.id
             WHERE m.room_id=?1 AND a.revoked_at IS NULL",
        )?;
        let roster: Vec<(i64, String)> = stmt
            .query_map(params![room_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);

        // `@everyone` calls the whole room in, which is the difference between
        // asking a question of someone and asking it of the council.
        let everyone = names.iter().any(|n| n.eq_ignore_ascii_case("everyone"));
        let wanted_names: Vec<String> = if everyone {
            roster.iter().map(|(_, n)| n.clone()).collect()
        } else {
            names
        };

        let mut added = Vec::new();
        for wanted in wanted_names {
            let Some((id, name)) = roster
                .iter()
                .find(|(_, n)| n.eq_ignore_ascii_case(&wanted))
            else {
                continue; // an @word that is not an agent here is just prose
            };
            if *id == by {
                continue;
            }
            let inserted = conn.execute(
                "INSERT OR IGNORE INTO thread_mentions(thread_id,agent_id) VALUES(?1,?2)",
                params![thread_id, id],
            )?;
            if inserted > 0 {
                added.push(name.clone());
            }
        }

        if !added.is_empty() {
            conn.execute(
                "UPDATE threads SET updated_at=?1 WHERE id=?2",
                params![now(), thread_id],
            )?;
        }
        Ok(added)
    }

    /// An assistant announcing that it has picked a thread up. Re-claiming
    /// refreshes the heartbeat, so a slow job keeps its slot.
    pub fn claim_thread(&self, actor: &AgentCtx, thread_id: i64, note: &str) -> Result<()> {
        let conn = self.lock();
        let (room_id, status): (i64, String) = conn
            .query_row(
                "SELECT room_id, status FROM threads WHERE id=?1",
                params![thread_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("thread {thread_id}")))?;
        Self::require_member(&conn, actor.id, room_id)?;
        if is_terminal(&status) {
            return Err(Error::Forbidden(format!("thread {thread_id} is {status}")));
        }

        conn.execute(
            "INSERT INTO thread_claims(thread_id,agent_id,note,claimed_at)
             VALUES(?1,?2,?3,?4)
             ON CONFLICT(thread_id,agent_id) DO UPDATE SET
               note=excluded.note, claimed_at=excluded.claimed_at",
            params![thread_id, actor.id, note.trim(), now()],
        )?;
        let notice = Self::append_event(
            &conn,
            Some(room_id),
            Some(thread_id),
            "thread.claimed",
            Some(actor.id),
            serde_json::json!({"note": note.trim(), "supervised": actor.supervised}),
        )?;
        drop(conn);
        self.publish(notice);
        Ok(())
    }

    // ----------------------------------------------------------- threads ---

    pub fn create_thread(&self, author: &AgentCtx, input: NewThread) -> Result<i64> {
        {
            let conn = self.lock();
            Self::require_member(&conn, author.id, input.room_id)?;
            if !author.is_human() && Self::room_paused(&conn, input.room_id)? {
                return Err(Error::Forbidden(format!(
                    "room #{} is paused",
                    Self::room_name(&conn, input.room_id)?
                )));
            }
        }
        if input.title.trim().is_empty() {
            return Err(Error::Invalid("a thread needs a title".into()));
        }
        let root = author.root()?;
        let created = now();

        let mut guard = self.lock();
        let tag = Self::tag(&guard, &input.tag)?;

        let git_ref = git::head(&root);
        let git_dirty = git::is_dirty(&root);

        let tx = guard.transaction()?;
        let status = "OPEN";
        tx.execute(
            "INSERT INTO threads(room_id,author_agent_id,title,body,tag,status,git_ref,git_dirty,
                                 created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)",
            params![
                input.room_id,
                author.id,
                input.title.trim(),
                input.body,
                tag.key,
                status,
                git_ref,
                git_dirty as i64,
                created
            ],
        )?;
        let thread_id = tx.last_insert_rowid();

        // Snapshot every attachment now — the point is that the review stays
        // reproducible even as the working tree moves on.
        for c in &input.context {
            let (path, content) = match c.kind.as_str() {
                "file" => {
                    let p = c
                        .path
                        .clone()
                        .ok_or_else(|| Error::Invalid("file context needs a path".into()))?;
                    match crate::fsjail::read_slice(&root, &p, c.start_line, c.end_line) {
                        Ok(s) => (Some(s.path), s.content),
                        Err(e) => (Some(p), format!("<could not read: {e}>")),
                    }
                }
                "diff" => (
                    c.path.clone(),
                    c.content.clone().unwrap_or_else(|| {
                        git::diff(&root, None, c.path.as_deref()).unwrap_or_default()
                    }),
                ),
                _ => (c.path.clone(), c.content.clone().unwrap_or_default()),
            };
            tx.execute(
                "INSERT INTO thread_context(thread_id,kind,path,start_line,end_line,content)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![thread_id, c.kind, path, c.start_line, c.end_line, content],
            )?;
        }

        if input.include_diff && !input.context.iter().any(|c| c.kind == "diff") {
            if let Ok(d) = git::diff(&root, None, None) {
                if !d.trim().is_empty() {
                    tx.execute(
                        "INSERT INTO thread_context(thread_id,kind,path,content)
                         VALUES(?1,'diff',NULL,?2)",
                        params![thread_id, d],
                    )?;
                }
            }
        }

        for agent_id in &input.mentions {
            tx.execute(
                "INSERT OR IGNORE INTO thread_mentions(thread_id,agent_id) VALUES(?1,?2)",
                params![thread_id, agent_id],
            )?;
        }

        index_thread(&tx, thread_id, input.room_id, &input.title, &input.body)?;

        // @names in the opening post address those agents too.
        let room_id = input.room_id;
        let _ = Self::apply_body_mentions(&tx, thread_id, room_id, &input.body, author.id)?;

        let notice = Self::append_event(
            &tx,
            Some(input.room_id),
            Some(thread_id),
            "thread.created",
            Some(author.id),
            serde_json::json!({"title": input.title, "tag": tag.key}),
        )?;
        tx.commit()?;
        drop(guard);
        self.publish(notice);
        Ok(thread_id)
    }

    pub fn update_thread_body(&self, actor: &AgentCtx, thread_id: i64, body: &str) -> Result<()> {
        let conn = self.lock();
        let author: i64 = conn
            .query_row("SELECT author_agent_id FROM threads WHERE id=?1", params![thread_id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("thread {thread_id}")))?;
        if author != actor.id && !actor.is_human() {
            return Err(Error::Forbidden("only the author may edit the topic".into()));
        }
        let room_id: i64 = conn.query_row(
            "SELECT room_id FROM threads WHERE id=?1",
            params![thread_id],
            |r| r.get(0),
        )?;
        Self::require_member(&conn, actor.id, room_id)?;
        conn.execute(
            "UPDATE threads SET body=?1, updated_at=?2 WHERE id=?3",
            params![body, now(), thread_id],
        )?;
        let notice = Self::append_event(
            &conn,
            Some(room_id),
            Some(thread_id),
            "thread.updated",
            Some(actor.id),
            serde_json::json!({}),
        )?;
        drop(conn);
        self.publish(notice);
        Ok(())
    }

    /// Only the agents a thread actually asked may speak in it.
    ///
    /// This is what stops a council of five turning every question into five
    /// answers. Being asked means one of three things: the thread named you,
    /// somebody named you in a message since, or you opened it yourself. A
    /// person is never refused — they are the one convening.
    ///
    /// Anyone can be brought in at any time by naming them, which is the
    /// intended way past this and worth saying in the refusal.
    fn require_asked(conn: &Connection, actor: &AgentCtx, thread_id: i64) -> Result<()> {
        if actor.is_human() {
            return Ok(());
        }
        let asked: bool = conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM thread_mentions WHERE thread_id=?1 AND agent_id=?2
                 UNION ALL
                 SELECT 1 FROM threads WHERE id=?1 AND author_agent_id=?2
             )",
            params![thread_id, actor.id],
            |r| r.get(0),
        )?;
        if asked {
            return Ok(());
        }
        Err(Error::Forbidden(format!(
            "thread {thread_id} did not ask you, so it is not yours to answer. If you have \
             something it needs, ask someone in it to bring you in by name — or leave it to \
             the agents who were asked."
        )))
    }

    /// Whoever called the council together decides when it is finished — or the
    /// person, who can always overrule.
    fn require_author_or_human(
        conn: &Connection,
        actor: &AgentCtx,
        thread_id: i64,
    ) -> Result<()> {
        if actor.is_human() {
            return Ok(());
        }
        let author: i64 = conn.query_row(
            "SELECT author_agent_id FROM threads WHERE id=?1",
            params![thread_id],
            |r| r.get(0),
        )?;
        if author == actor.id {
            return Ok(());
        }
        let name: String = conn
            .query_row(
                "SELECT name FROM agents WHERE id=?1",
                params![author],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| "whoever opened it".into());
        Err(Error::Forbidden(format!(
            "only {name} opened this thread, so only {name} can close it. Say what you think \
             in a reply instead."
        )))
    }

    pub fn set_thread_status(&self, actor: &AgentCtx, thread_id: i64, status: &str) -> Result<()> {
        if !STATUSES.contains(&status) {
            return Err(Error::Invalid(format!("unknown status `{status}`")));
        }
        if status == "RESOLVED" {
            return Err(Error::Invalid(
                "use resolve_thread — resolving writes the decision record, and needs a summary"
                    .into(),
            ));
        }
        let conn = self.lock();
        let room_id: i64 = conn.query_row(
            "SELECT room_id FROM threads WHERE id=?1",
            params![thread_id],
            |r| r.get(0),
        )?;
        Self::require_member(&conn, actor.id, room_id)?;
        Self::require_author_or_human(&conn, actor, thread_id)?;
        conn.execute(
            "UPDATE threads SET status=?1, updated_at=?2 WHERE id=?3",
            params![status, now(), thread_id],
        )?;
        let notice = Self::append_event(
            &conn,
            Some(room_id),
            Some(thread_id),
            "thread.status",
            Some(actor.id),
            serde_json::json!({"status": status}),
        )?;
        drop(conn);
        self.publish(notice);
        Ok(())
    }

    /// Marks a thread done and writes the decision record into the repo.
    pub fn resolve_thread(
        &self,
        actor: &AgentCtx,
        thread_id: i64,
        summary: &str,
        status: &str,
    ) -> Result<Option<String>> {
        {
            let conn = self.lock();
            Self::require_author_or_human(&conn, actor, thread_id)?;
        }
        if !["RESOLVED", "WONTFIX", "BLOCKED"].contains(&status) {
            return Err(Error::Invalid(format!("cannot resolve to `{status}`")));
        }
        if summary.trim().is_empty() {
            return Err(Error::Invalid(
                "a resolution needs a summary — it becomes the decision record".into(),
            ));
        }
        {
            let conn = self.lock();
            let n = conn.execute(
                "UPDATE threads SET status=?1, resolution_summary=?2, resolved_at=?3, updated_at=?3
                 WHERE id=?4",
                params![status, summary.trim(), now(), thread_id],
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("thread {thread_id}")));
            }
        }

        let detail = self.thread_detail(thread_id)?;
        let export_path = if status == "RESOLVED" {
            let conn = self.lock();
            let folder = Self::room_folder(&conn, detail.summary.room_id)?;
            drop(conn);
            match export::write_thread(&folder, &detail) {
                Ok(p) => {
                    let conn = self.lock();
                    conn.execute(
                        "UPDATE threads SET export_path=?1 WHERE id=?2",
                        params![p, thread_id],
                    )?;
                    Some(p)
                }
                Err(e) => {
                    tracing::warn!("thread export failed: {e}");
                    None
                }
            }
        } else {
            None
        };

        let conn = self.lock();
        let notice = Self::append_event(
            &conn,
            Some(detail.summary.room_id),
            Some(thread_id),
            "thread.resolved",
            Some(actor.id),
            serde_json::json!({"status": status, "export_path": export_path}),
        )?;
        drop(conn);
        self.publish(notice);
        Ok(export_path)
    }

    pub fn list_threads(
        &self,
        room_id: Option<i64>,
        status: Option<&str>,
        tag: Option<&str>,
        mentions_agent: Option<i64>,
        sort: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ThreadSummary>> {
        let conn = self.lock();
        let mut sql = String::from(THREAD_SUMMARY_SQL);
        sql.push_str(" WHERE 1=1");
        let mut ps: Vec<rusqlite::types::Value> = vec![];

        if let Some(r) = room_id {
            ps.push(r.into());
            sql.push_str(&format!(" AND t.room_id=?{}", ps.len()));
        }
        match status {
            Some("open") => {
                sql.push_str(&format!(" AND t.status IN {OPEN_STATUS_SQL}"));
            }
            Some("resolved") => {
                sql.push_str(&format!(" AND t.status IN {DONE_STATUS_SQL}"));
            }
            Some("blocked") => sql.push_str(" AND t.status = 'BLOCKED'"),
            Some(s) if !s.is_empty() && s != "all" => {
                ps.push(s.to_string().into());
                sql.push_str(&format!(" AND t.status=?{}", ps.len()));
            }
            _ => {}
        }
        if let Some(tg) = tag {
            if !tg.is_empty() && tg != "all" {
                ps.push(tg.to_string().into());
                sql.push_str(&format!(" AND t.tag=?{}", ps.len()));
            }
        }
        if let Some(a) = mentions_agent {
            ps.push(a.into());
            let i = ps.len();
            sql.push_str(&format!(
                " AND (EXISTS(SELECT 1 FROM thread_mentions m WHERE m.thread_id=t.id AND m.agent_id=?{i})
                       OR NOT EXISTS(SELECT 1 FROM thread_mentions m WHERE m.thread_id=t.id))"
            ));
        }
        // Whitelisted, never interpolated from the caller's string.
        sql.push_str(match sort.unwrap_or("last_reply") {
            "created" => " ORDER BY t.created_at DESC",
            // Busiest first; recency breaks ties so it is not arbitrary.
            "activity" => " ORDER BY reply_count DESC, t.updated_at DESC",
            // A thread with no replies yet falls back to when it was opened.
            _ => " ORDER BY COALESCE(last_reply_at, t.created_at) DESC",
        });
        sql.push_str(" LIMIT ");
        sql.push_str(&limit.clamp(1, 500).to_string());

        let mut stmt = conn.prepare(&sql)?;
        let out = stmt
            .query_map(params_from_iter(ps), row_to_summary)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    pub fn thread_detail(&self, thread_id: i64) -> Result<ThreadDetail> {
        let conn = self.lock();
        let sql = format!("{THREAD_SUMMARY_SQL} WHERE t.id=?1");
        let summary = conn
            .query_row(&sql, params![thread_id], row_to_summary)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("thread {thread_id}")))?;

        let (body, git_dirty, resolution_summary, export_path): (String, i64, Option<String>, Option<String>) =
            conn.query_row(
                "SELECT body,git_dirty,resolution_summary,export_path FROM threads WHERE id=?1",
                params![thread_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;

        let mut stmt = conn.prepare(
            "SELECT id,kind,path,start_line,end_line,content FROM thread_context
             WHERE thread_id=?1 ORDER BY id",
        )?;
        let context = stmt
            .query_map(params![thread_id], |r| {
                Ok(ThreadContextItem {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    path: r.get(2)?,
                    start_line: r.get(3)?,
                    end_line: r.get(4)?,
                    content: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = conn.prepare(
            "SELECT c.agent_id, a.name, a.color,
                    COALESCE(p.icon, 'robot'), c.note, c.claimed_at
             FROM thread_claims c
             JOIN agents a ON a.id=c.agent_id
             LEFT JOIN agent_profiles p ON p.id=a.profile_id
             WHERE c.thread_id=?1 ORDER BY c.claimed_at",
        )?;
        let claims = stmt
            .query_map(params![thread_id], |r| {
                Ok(ThreadClaim {
                    agent_id: r.get(0)?,
                    agent_name: r.get(1)?,
                    color: r.get(2)?,
                    icon: r.get(3)?,
                    note: r.get(4)?,
                    claimed_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt =
            conn.prepare("SELECT agent_id FROM thread_mentions WHERE thread_id=?1")?;
        let mentions = stmt
            .query_map(params![thread_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;

        let mut stmt = conn.prepare(
            "SELECT m.id,m.thread_id,m.agent_id,a.name,a.role,
                    COALESCE(p.icon, CASE a.role WHEN 'HUMAN' THEN 'user' ELSE 'robot' END),
                    a.color,
                    m.body,m.verdict,m.severity,m.refs,m.tokens_in,m.tokens_out,m.cost_usd,m.created_at,m.edited_at
             FROM messages m
             JOIN agents a ON a.id=m.agent_id
             LEFT JOIN agent_profiles p ON p.id=a.profile_id
             WHERE m.thread_id=?1 ORDER BY m.id",
        )?;
        let messages = stmt
            .query_map(params![thread_id], |r| {
                Ok(Message {
                    id: r.get(0)?,
                    thread_id: r.get(1)?,
                    agent_id: r.get(2)?,
                    agent_name: r.get(3)?,
                    agent_role: r.get(4)?,
                    icon: r.get(5)?,
                    color: r.get(6)?,
                    body: r.get(7)?,
                    verdict: r.get(8)?,
                    severity: r.get(9)?,
                    refs: serde_json::from_str(&r.get::<_, String>(10)?)
                        .unwrap_or(serde_json::json!([])),
                    tokens_in: r.get(11)?,
                    tokens_out: r.get(12)?,
                    cost_usd: r.get(13)?,
                    created_at: r.get(14)?,
                    edited_at: r.get(15)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(ThreadDetail {
            summary,
            body,
            git_dirty: git_dirty != 0,
            resolution_summary,
            export_path,
            context,
            mentions,
            claims,
            messages,
        })
    }

    // ---------------------------------------------------------- messages ---

    pub fn reply(&self, actor: &AgentCtx, input: NewReply) -> Result<i64> {
        if input.body.trim().is_empty() {
            return Err(Error::Invalid("a reply needs a body".into()));
        }

        let mut guard = self.lock();
        let (room_id, tag_key, status): (i64, String, String) = guard
            .query_row(
                "SELECT room_id,tag,status FROM threads WHERE id=?1",
                params![input.thread_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("thread {}", input.thread_id)))?;

        Self::require_member(&guard, actor.id, room_id)?;
        Self::require_asked(&guard, actor, input.thread_id)?;

        // --- rails -------------------------------------------------------
        if !actor.is_human() {
            if Self::room_paused(&guard, room_id)? {
                return Err(Error::Forbidden(format!(
                    "room #{} is paused; nothing will be accepted until it is resumed",
                    Self::room_name(&guard, room_id)?
                )));
            }
            if is_terminal(&status) {
                return Err(Error::Forbidden(format!(
                    "thread {} is {status}",
                    input.thread_id
                )));
            }
            let (max_per_agent, max_total, cap): (i64, i64, f64) = guard.query_row(
                "SELECT max_replies_per_agent,max_thread_messages,cost_cap_usd FROM rooms WHERE id=?1",
                params![room_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;

            let mine: i64 = guard.query_row(
                "SELECT COUNT(*) FROM messages WHERE thread_id=?1 AND agent_id=?2",
                params![input.thread_id, actor.id],
                |r| r.get(0),
            )?;
            if mine >= max_per_agent {
                return Err(Error::Limit(format!(
                    "you have already posted {mine} replies on this thread (cap {max_per_agent}). \
                     Say what you still need and stop."
                )));
            }
            let total: i64 = guard.query_row(
                "SELECT COUNT(*) FROM messages WHERE thread_id=?1",
                params![input.thread_id],
                |r| r.get(0),
            )?;
            if total >= max_total {
                return Err(Error::Limit(format!(
                    "thread {} has hit its {max_total} message cap",
                    input.thread_id
                )));
            }
            let spent: f64 = guard.query_row(
                "SELECT COALESCE(SUM(m.cost_usd),0) FROM messages m
                 JOIN threads t ON t.id=m.thread_id WHERE t.room_id=?1",
                params![room_id],
                |r| r.get(0),
            )?;
            if cap > 0.0 && spent >= cap {
                return Err(Error::Limit(format!(
                    "room #{} has spent ${spent:.2} of its ${cap:.2} cap",
                    Self::room_name(&guard, room_id)?
                )));
            }
        }

        let tag = Self::tag(&guard, &tag_key)?;
        let verdict = Self::validate_verdict(&tag, input.verdict.as_deref())?;
        let severity = Self::validate_severity(input.severity.as_deref())?;

        let refs = input.refs.unwrap_or(serde_json::json!([]));
        let created = now();

        let tx = guard.transaction()?;
        tx.execute(
            "INSERT INTO messages(thread_id,agent_id,body,verdict,severity,refs,
                                  tokens_in,tokens_out,cost_usd,created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                input.thread_id,
                actor.id,
                input.body,
                verdict,
                severity,
                refs.to_string(),
                input.tokens_in,
                input.tokens_out,
                input.cost_usd,
                created
            ],
        )?;
        let message_id = tx.last_insert_rowid();

        // Only the clock moves. A discussion is finished when the person who
        // called it says so, not when something decides everyone has had a
        // turn — so a reply changes nothing but the ordering.
        tx.execute(
            "UPDATE threads SET updated_at=?1 WHERE id=?2",
            params![created, input.thread_id],
        )?;

        index_message(&tx, message_id, room_id, &input.body)?;

        // Anyone named in the body joins the thread and is told about it.
        let called = Self::apply_body_mentions(&tx, input.thread_id, room_id, &input.body, actor.id)?;
        let call_notice = if called.is_empty() {
            None
        } else {
            Some(Self::append_event(
                &tx,
                Some(room_id),
                Some(input.thread_id),
                "thread.mentioned",
                Some(actor.id),
                serde_json::json!({ "called": called }),
            )?)
        };

        let notice = Self::append_event(
            &tx,
            Some(room_id),
            Some(input.thread_id),
            "message.created",
            Some(actor.id),
            serde_json::json!({
                "message_id": message_id,
                "verdict": verdict,
                "severity": severity,
                "status": "OPEN",
                // Which of the two wrote this: a run Rivendell started, or a
                // session someone is sitting in front of. They share an
                // identity by design, so without recording it there is no way
                // afterwards to tell them apart — and when both are live,
                // every attribution becomes an argument about timestamps.
                "supervised": actor.supervised,
            }),
        )?;
        tx.commit()?;
        drop(guard);
        self.publish(notice);
        if let Some(n) = call_notice {
            self.publish(n);
        }
        Ok(message_id)
    }

    pub fn log_file_access(
        &self,
        agent_id: i64,
        thread_id: Option<i64>,
        path: &str,
        allowed: bool,
        reason: &str,
    ) {
        let conn = self.lock();
        let _ = conn.execute(
            "INSERT INTO file_access_log(agent_id,thread_id,path,allowed,reason,created_at)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![agent_id, thread_id, path, allowed as i64, reason, now()],
        );
    }


    fn validate_verdict(tag: &Tag, raw: Option<&str>) -> Result<Option<String>> {
        match raw.map(str::trim) {
            Some(v) if !v.is_empty() => {
                let v = v.to_ascii_uppercase();
                // REFUTED was renamed to CLEARED. Still accepted, so a
                // connected agent working from the old wording keeps working.
                let v = if v == "REFUTED" { "CLEARED".to_string() } else { v };
                if !tag.verdict_options.is_empty() && !tag.verdict_options.contains(&v) {
                    return Err(Error::Invalid(format!(
                        "verdict for a {} thread must be one of {}",
                        tag.key,
                        tag.verdict_options.join(", ")
                    )));
                }
                Ok(Some(v))
            }
            _ => Ok(None),
        }
    }

    fn validate_severity(raw: Option<&str>) -> Result<Option<String>> {
        match raw.map(str::trim) {
            Some(s) if !s.is_empty() => {
                let s = s.to_ascii_uppercase();
                if !SEVERITIES.contains(&s.as_str()) {
                    return Err(Error::Invalid(format!(
                        "severity must be one of {}",
                        SEVERITIES.join(", ")
                    )));
                }
                Ok(Some(s))
            }
            _ => Ok(None),
        }
    }

    /// Revise a message you wrote.
    ///
    /// Only ever your own: letting one participant rewrite another's words
    /// would make the verdicts in an exported decision record unattributable,
    /// which is the one thing that record exists to guarantee.
    ///
    /// The edit is announced on the event log, so an assistant whose reply was
    /// based on the old text can notice and revise its own answer.
    pub fn edit_message(&self, actor: &AgentCtx, input: NewReply, message_id: i64) -> Result<()> {
        if input.body.trim().is_empty() {
            return Err(Error::Invalid("a message needs a body".into()));
        }

        let mut guard = self.lock();
        let (author, thread_id, prev_verdict, prev_severity, prev_body): (
            i64, i64, Option<String>, Option<String>, String,
        ) = guard
            .query_row(
                "SELECT agent_id, thread_id, verdict, severity, body FROM messages WHERE id=?1",
                params![message_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("message {message_id}")))?;

        if author != actor.id {
            return Err(Error::Forbidden(
                "you can only edit your own messages".into(),
            ));
        }

        let (room_id, status, tag_key): (i64, String, String) = guard.query_row(
            "SELECT room_id, status, tag FROM threads WHERE id=?1",
            params![thread_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Self::require_member(&guard, actor.id, room_id)?;
        // Agents are held to the room's state; the human is the override.
        if !actor.is_human() {
            if Self::room_paused(&guard, room_id)? {
                return Err(Error::Forbidden(format!(
                    "room #{} is paused",
                    Self::room_name(&guard, room_id)?
                )));
            }
            if is_terminal(&status) {
                return Err(Error::Forbidden(format!("thread {thread_id} is {status}")));
            }
        }

        let tag = Self::tag(&guard, &tag_key)?;
        let verdict = Self::validate_verdict(&tag, input.verdict.as_deref())?;
        let severity = Self::validate_severity(input.severity.as_deref())?;
        let refs = input.refs.clone().unwrap_or(serde_json::json!([]));
        let edited = now();

        let tx = guard.transaction()?;
        tx.execute(
            "UPDATE messages
             SET body=?1, verdict=?2, severity=?3, refs=?4, edited_at=?5
             WHERE id=?6",
            params![input.body, verdict, severity, refs.to_string(), edited, message_id],
        )?;
        tx.execute("UPDATE threads SET updated_at=?1 WHERE id=?2", params![edited, thread_id])?;
        // Keep search honest — otherwise the old wording stays findable.
        let _ = tx.execute(
            "UPDATE search_index SET body=?1 WHERE kind='message' AND ref_id=?2",
            params![input.body, message_id],
        );


        // Anyone named in the body joins the thread and is told about it.
        let called = Self::apply_body_mentions(&tx, thread_id, room_id, &input.body, actor.id)?;
        let call_notice = if called.is_empty() {
            None
        } else {
            Some(Self::append_event(
                &tx,
                Some(room_id),
                Some(thread_id),
                "thread.mentioned",
                Some(actor.id),
                serde_json::json!({ "called": called }),
            )?)
        };

        // The previous verdict is worth keeping: it is the load-bearing part of
        // a reply, and the event log is where the trail lives. The old body is
        // not retained — only whether it changed.
        let notice = Self::append_event(
            &tx,
            Some(room_id),
            Some(thread_id),
            "message.edited",
            Some(actor.id),
            serde_json::json!({
                "message_id": message_id,
                "body_changed": prev_body != input.body,
                "previous_verdict": prev_verdict,
                "previous_severity": prev_severity,
                "verdict": verdict,
                "severity": severity,
                "role": actor.role,
            }),
        )?;
        tx.commit()?;
        drop(guard);
        self.publish(notice);
        if let Some(n) = call_notice {
            self.publish(n);
        }

        // A resolved thread already wrote its record; leaving it stale would
        // make the file disagree with the app.
        if is_terminal(&status) {
            if let Ok(detail) = self.thread_detail(thread_id) {
                let conn = self.lock();
                if let Ok(folder) = Self::room_folder(&conn, room_id) {
                    drop(conn);
                    if let Err(e) = export::write_thread(&folder, &detail) {
                        tracing::warn!("re-export after edit failed: {e}");
                    }
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ search ---

    pub fn search(&self, room_id: Option<i64>, query: &str, limit: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.lock();
        let limit = limit.clamp(1, 100);
        let (sql, ps) = if db::fts_available(&conn) {
            let mut sql = String::from(
                "SELECT kind, ref_id, room_id, title, snippet(search_index, 4, '«', '»', '…', 16)
                 FROM search_index WHERE search_index MATCH ?1",
            );
            if room_id.is_some() {
                sql.push_str(" AND room_id = ?2");
            }
            sql.push_str(&format!(" ORDER BY rank LIMIT {limit}"));
            let mut ps: Vec<rusqlite::types::Value> = vec![fts_query(query).into()];
            if let Some(r) = room_id {
                ps.push(r.into());
            }
            (sql, ps)
        } else {
            let mut sql = String::from(
                "SELECT kind, ref_id, room_id, title, substr(body,1,240) FROM search_index
                 WHERE (title LIKE ?1 OR body LIKE ?1)",
            );
            if room_id.is_some() {
                sql.push_str(" AND room_id = ?2");
            }
            sql.push_str(&format!(" ORDER BY ref_id DESC LIMIT {limit}"));
            let mut ps: Vec<rusqlite::types::Value> = vec![format!("%{query}%").into()];
            if let Some(r) = room_id {
                ps.push(r.into());
            }
            (sql, ps)
        };

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(ps), search_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn search_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "kind": r.get::<_, String>(0)?,
        "ref_id": r.get::<_, i64>(1)?,
        "room_id": r.get::<_, Option<i64>>(2)?,
        "title": r.get::<_, String>(3)?,
        "excerpt": r.get::<_, String>(4)?,
    }))
}

/// Quotes each term so user input can never be read as FTS5 syntax.
fn fts_query(raw: &str) -> String {
    raw.split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .filter(|t| t.len() > 2)
        .collect::<Vec<_>>()
        .join(" ")
}

fn index_thread(conn: &Connection, id: i64, room: i64, title: &str, body: &str) -> Result<()> {
    let _ = conn.execute(
        "INSERT INTO search_index(kind,ref_id,room_id,title,body) VALUES('thread',?1,?2,?3,?4)",
        params![id, room, title, body],
    );
    Ok(())
}

fn index_message(conn: &Connection, id: i64, room: i64, body: &str) -> Result<()> {
    let _ = conn.execute(
        "INSERT INTO search_index(kind,ref_id,room_id,title,body) VALUES('message',?1,?2,'',?3)",
        params![id, room, body],
    );
    Ok(())
}

/// `@name` where name is the usual identifier shape. Anything else is prose.
/// Deliberately strict: an email address or a decorator should not summon an
/// agent.
fn parse_mentions(body: &str) -> Vec<String> {
    let bytes: Vec<char> = body.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '@' && (i == 0 || !bytes[i - 1].is_alphanumeric()) {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_alphanumeric() || bytes[j] == '-' || bytes[j] == '_') {
                j += 1;
            }
            if j > i + 1 {
                let name: String = bytes[i + 1..j].iter().collect();
                if !out.iter().any(|e| e.eq_ignore_ascii_case(&name)) {
                    out.push(name);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

fn remote_of(root: &std::path::Path) -> Option<String> {
    git::remote(root)
}

fn normalize_room_name(name: &str) -> Result<String> {
    let n = name.trim().trim_start_matches('#').to_lowercase();
    let n: String = n
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let n = n.trim_matches('-').to_string();
    if n.is_empty() {
        return Err(Error::Invalid("a room needs a name".into()));
    }
    Ok(n)
}

const THREAD_SUMMARY_SQL: &str = "
SELECT t.id, t.room_id, r.name, t.title, t.tag, t.status, t.author_agent_id, a.name,
       COALESCE(p.icon, CASE a.role WHEN 'HUMAN' THEN 'user' ELSE 'robot' END),
       (SELECT COUNT(*) FROM messages m WHERE m.thread_id=t.id) AS reply_count,
       (SELECT COUNT(DISTINCT m.agent_id) FROM messages m
          JOIN agents ag ON ag.id=m.agent_id
         WHERE m.thread_id=t.id AND m.agent_id <> t.author_agent_id) AS responder_count,
       -- Claimed, still live, still unanswered. Recomputed per row rather than
       -- stored, so it cannot go stale between the timer and a read.
       (SELECT COUNT(*) FROM thread_claims c
          JOIN agents ca ON ca.id=c.agent_id
         WHERE c.thread_id=t.id AND ca.revoked_at IS NULL
           AND substr(c.claimed_at, 1, 19)
                 > strftime('%Y-%m-%dT%H:%M:%S', 'now',
                            '-' || r.response_timeout_secs || ' seconds')
           AND NOT EXISTS(SELECT 1 FROM messages m
                           WHERE m.thread_id=t.id AND m.agent_id=c.agent_id)) AS in_progress,
       (SELECT COALESCE(SUM(m.cost_usd),0) FROM messages m WHERE m.thread_id=t.id) AS cost_usd,
       t.git_ref, t.created_at, t.updated_at, t.resolved_at, a.color,
       (SELECT MAX(m.created_at) FROM messages m WHERE m.thread_id=t.id) AS last_reply_at
FROM threads t
JOIN rooms r ON r.id=t.room_id
JOIN agents a ON a.id=t.author_agent_id
LEFT JOIN agent_profiles p ON p.id=a.profile_id";

fn row_to_summary(r: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadSummary> {
    Ok(ThreadSummary {
        id: r.get(0)?,
        room_id: r.get(1)?,
        room_name: r.get(2)?,
        title: r.get(3)?,
        tag: r.get(4)?,
        status: r.get(5)?,
        author_agent_id: r.get(6)?,
        author_name: r.get(7)?,
        author_icon: r.get(8)?,
        reply_count: r.get(9)?,
        responder_count: r.get(10)?,
        in_progress: r.get(11)?,
        cost_usd: r.get(12)?,
        git_ref: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
        resolved_at: r.get(16)?,
        author_color: r.get(17)?,
        last_reply_at: r.get(18)?,
    })
}
