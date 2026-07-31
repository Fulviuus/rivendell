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
    /// Tokens minted for a single spawned run, keyed by digest. In memory only:
    /// they die with the process, which is exactly the lifetime we want.
    run_tokens: Mutex<std::collections::HashMap<String, RunToken>>,
}

#[derive(Debug, Clone)]
struct RunToken {
    agent_id: i64,
    run_id: i64,
}

/// Identity resolved from a bearer token, plus everything a tool call needs.
#[derive(Debug, Clone)]
pub struct AgentCtx {
    pub id: i64,
    pub name: String,
    pub role: String,
    pub room_id: i64,
    pub room_name: String,
    pub project_id: i64,
    pub project_name: String,
    pub folder_path: String,
    pub paused: bool,
}

impl AgentCtx {
    pub fn is_coder(&self) -> bool {
        self.role == "CODER" || self.role == "HUMAN"
    }
    /// The human sitting in front of the app is never rate-limited or paused out.
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
            run_tokens: Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Credential for one spawned run. Revoked the moment the process exits.
    pub fn mint_run_token(&self, agent_id: i64, run_id: i64) -> String {
        let generated = auth::generate();
        let token = format!("rvdrun_{}", &generated.full[4..]);
        let mut map = self
            .run_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.insert(auth::hash(&token), RunToken { agent_id, run_id });
        token
    }

    pub fn revoke_run_token(&self, run_id: i64) {
        let mut map = self
            .run_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.retain(|_, t| t.run_id != run_id);
    }

    fn agent_id_for_run_token(&self, token: &str) -> Option<i64> {
        let map = self
            .run_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.get(&auth::hash(token)).map(|t| t.agent_id)
    }

    pub fn set_run_pid(&self, run_id: i64, pid: Option<i64>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE agent_runs SET pid=?1 WHERE id=?2",
            params![pid, run_id],
        )?;
        Ok(())
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
        })
    }

    fn publish(&self, notice: EventNotice) {
        // Errors only mean "nobody is listening", which is normal at startup.
        let _ = self.events.send(notice);
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
            "SELECT id,name,folder_path,git_remote,created_at FROM projects ORDER BY name",
        )?;
        let out = stmt
            .query_map([], |r| {
                Ok(Project {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    folder_path: r.get(2)?,
                    git_remote: r.get(3)?,
                    created_at: r.get(4)?,
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
            created_at: now(),
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
                    r.max_replies_per_agent,r.max_thread_messages,r.max_concurrent_runs,
                    r.cost_cap_usd,r.created_at,
                    (SELECT COUNT(*) FROM threads t
                      WHERE t.room_id=r.id AND t.status NOT IN ('RESOLVED','WONTFIX'))
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
                    max_concurrent_runs: r.get(9)?,
                    cost_cap_usd: r.get(10)?,
                    created_at: r.get(11)?,
                    open_threads: r.get(12)?,
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
            ("max_thread_messages", "max_thread_messages"),
            ("max_concurrent_runs", "max_concurrent_runs"),
            ("cost_cap_usd", "cost_cap_usd"),
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
                    default_quorum: r.get(6)?,
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
                    default_quorum: r.get(6)?,
                    builtin: r.get::<_, i64>(7)? != 0,
                })
            },
        )
        .optional()?
        .ok_or_else(|| Error::Invalid(format!("unknown tag `{key}`")))
    }

    // ------------------------------------------------------------ agents ---

    pub fn list_agents(&self, room_id: Option<i64>) -> Result<Vec<Agent>> {
        let conn = self.lock();
        let mut sql = String::from(
            "SELECT a.id,a.room_id,a.name,a.role,a.profile_id,p.key,p.label,
                    COALESCE(p.icon, CASE a.role WHEN 'HUMAN' THEN 'user' ELSE 'robot' END),
                    a.color,a.key_preview,a.auto_dispatch,a.system_note,a.created_at,a.revoked_at
             FROM agents a LEFT JOIN agent_profiles p ON p.id=a.profile_id",
        );
        let mut ps: Vec<rusqlite::types::Value> = vec![];
        if let Some(r) = room_id {
            sql.push_str(" WHERE a.room_id=?1");
            ps.push(r.into());
        }
        sql.push_str(" ORDER BY CASE a.role WHEN 'HUMAN' THEN 0 WHEN 'CODER' THEN 1 ELSE 2 END, a.name");

        let mut stmt = conn.prepare(&sql)?;
        let out = stmt
            .query_map(params_from_iter(ps), |r| {
                Ok(Agent {
                    id: r.get(0)?,
                    room_id: r.get(1)?,
                    name: r.get(2)?,
                    role: r.get(3)?,
                    profile_id: r.get(4)?,
                    profile_key: r.get(5)?,
                    profile_label: r.get(6)?,
                    icon: r.get(7)?,
                    color: r.get(8)?,
                    key_preview: r.get(9)?,
                    auto_dispatch: r.get::<_, i64>(10)? != 0,
                    system_note: r.get(11)?,
                    created_at: r.get(12)?,
                    revoked_at: r.get(13)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    /// Returns the one and only plaintext view of the new key.
    pub fn create_agent(
        &self,
        room_id: i64,
        name: &str,
        role: &str,
        profile_id: Option<i64>,
        system_note: &str,
        auto_dispatch: bool,
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
            "INSERT INTO agents(room_id,name,role,profile_id,key_id,key_hash,key_preview,
                                auto_dispatch,system_note,color,created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                room_id,
                name,
                role,
                profile_id,
                key.key_id,
                key.hash,
                key.preview,
                auto_dispatch as i64,
                system_note.trim(),
                color.trim(),
                now()
            ],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _) if f.extended_code == 2067 => {
                Error::Invalid(format!("this room already has an agent called {name}"))
            }
            other => other.into(),
        })?;
        let id = conn.last_insert_rowid();
        let notice = Self::append_event(
            &conn,
            Some(room_id),
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
        Ok(key.full)
    }

    pub fn set_agent_revoked(&self, agent_id: i64, revoked: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE agents SET revoked_at=?1 WHERE id=?2",
            params![if revoked { Some(now()) } else { None }, agent_id],
        )?;
        Ok(())
    }

    /// Edits the mutable parts of an agent. The key and room are not among
    /// them — rotate or recreate for those.
    pub fn update_agent(&self, agent_id: i64, patch: serde_json::Value) -> Result<()> {
        let room_id = self.agent_ctx(agent_id)?.room_id;
        {
            let conn = self.lock();
            for (col, key) in [
                ("name", "name"),
                ("system_note", "system_note"),
                ("color", "color"),
                ("auto_dispatch", "auto_dispatch"),
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
                        Error::Invalid("this room already has an agent with that name".into())
                    }
                    other => Error::from(other),
                })?;
            }
        }
        let conn = self.lock();
        let notice = Self::append_event(
            &conn,
            Some(room_id),
            None,
            "agent.updated",
            Some(agent_id),
            patch,
        )?;
        drop(conn);
        self.publish(notice);
        Ok(())
    }

    pub fn set_agent_auto_dispatch(&self, agent_id: i64, on: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE agents SET auto_dispatch=?1 WHERE id=?2",
            params![on as i64, agent_id],
        )?;
        Ok(())
    }

    pub fn delete_agent(&self, agent_id: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM agents WHERE id=?1", params![agent_id])?;
        Ok(())
    }

    /// Bearer-token lookup. Returns `None` for unknown, malformed or revoked keys.
    ///
    /// Accepts two shapes: an agent's long-lived `rvd_…` key, and the ephemeral
    /// `rvdrun_…` token held by a live spawned process.
    pub fn authenticate(&self, token: &str) -> Result<Option<AgentCtx>> {
        if token.starts_with("rvdrun_") {
            return match self.agent_id_for_run_token(token) {
                Some(agent_id) => self.agent_ctx(agent_id).map(Some),
                None => Ok(None),
            };
        }
        let Some(key_id) = auth::key_id_of(token) else {
            return Ok(None);
        };
        let conn = self.lock();
        let row: Option<(i64, String, String, i64, String, i64, String, String, i64, String)> = conn
            .query_row(
                "SELECT a.id,a.name,a.role,a.room_id,r.name,p.id,p.name,p.folder_path,r.paused,
                        COALESCE(a.key_hash,'')
                 FROM agents a
                 JOIN rooms r ON r.id=a.room_id
                 JOIN projects p ON p.id=r.project_id
                 WHERE a.key_id=?1 AND a.revoked_at IS NULL",
                params![key_id],
                |r| {
                    Ok((
                        r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?,
                        r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?,
                    ))
                },
            )
            .optional()?;

        let Some((id, name, role, room_id, room_name, project_id, project_name, folder_path, paused, hash)) = row
        else {
            return Ok(None);
        };
        if !auth::verify(token, &hash) {
            return Ok(None);
        }
        Ok(Some(AgentCtx {
            id,
            name,
            role,
            room_id,
            room_name,
            project_id,
            project_name,
            folder_path,
            paused: paused != 0,
        }))
    }

    pub fn agent_ctx(&self, agent_id: i64) -> Result<AgentCtx> {
        let conn = self.lock();
        conn.query_row(
            "SELECT a.id,a.name,a.role,a.room_id,r.name,p.id,p.name,p.folder_path,r.paused
             FROM agents a JOIN rooms r ON r.id=a.room_id JOIN projects p ON p.id=r.project_id
             WHERE a.id=?1",
            params![agent_id],
            |r| {
                Ok(AgentCtx {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    role: r.get(2)?,
                    room_id: r.get(3)?,
                    room_name: r.get(4)?,
                    project_id: r.get(5)?,
                    project_name: r.get(6)?,
                    folder_path: r.get(7)?,
                    paused: r.get::<_, i64>(8)? != 0,
                })
            },
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("agent {agent_id}")))
    }

    // ----------------------------------------------------------- threads ---

    pub fn create_thread(&self, author: &AgentCtx, input: NewThread) -> Result<i64> {
        if !author.is_coder() {
            return Err(Error::Forbidden(
                "only a CODER may open a thread; assistants reply".into(),
            ));
        }
        if author.paused && !author.is_human() {
            return Err(Error::Forbidden(format!(
                "room #{} is paused",
                author.room_name
            )));
        }
        if input.title.trim().is_empty() {
            return Err(Error::Invalid("a thread needs a title".into()));
        }
        if input.room_id != author.room_id {
            return Err(Error::Forbidden("that room is not yours".into()));
        }

        let root = author.root()?;
        let created = now();

        let mut guard = self.lock();
        let tag = Self::tag(&guard, &input.tag)?;
        let quorum = input.quorum.unwrap_or(tag.default_quorum).max(0);

        let git_ref = git::head(&root);
        let git_dirty = git::is_dirty(&root);

        let tx = guard.transaction()?;
        let status = if quorum > 0 { "AWAITING_REPLIES" } else { "OPEN" };
        tx.execute(
            "INSERT INTO threads(room_id,author_agent_id,title,body,tag,status,git_ref,git_dirty,
                                 quorum,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10)",
            params![
                input.room_id,
                author.id,
                input.title.trim(),
                input.body,
                tag.key,
                status,
                git_ref,
                git_dirty as i64,
                quorum,
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
        conn.execute(
            "UPDATE threads SET body=?1, updated_at=?2 WHERE id=?3",
            params![body, now(), thread_id],
        )?;
        let notice = Self::append_event(
            &conn,
            Some(actor.room_id),
            Some(thread_id),
            "thread.updated",
            Some(actor.id),
            serde_json::json!({}),
        )?;
        drop(conn);
        self.publish(notice);
        Ok(())
    }

    pub fn set_thread_status(&self, actor: &AgentCtx, thread_id: i64, status: &str) -> Result<()> {
        if !STATUSES.contains(&status) {
            return Err(Error::Invalid(format!("unknown status `{status}`")));
        }
        if !actor.is_coder() {
            return Err(Error::Forbidden("only a CODER may change thread status".into()));
        }
        let conn = self.lock();
        conn.execute(
            "UPDATE threads SET status=?1, updated_at=?2 WHERE id=?3",
            params![status, now(), thread_id],
        )?;
        let notice = Self::append_event(
            &conn,
            Some(actor.room_id),
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
        if !actor.is_coder() {
            return Err(Error::Forbidden(
                "only a CODER may resolve a thread".into(),
            ));
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
        let export_path = if status == "RESOLVED" || status == "WONTFIX" {
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
            Some(actor.room_id),
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
            Some("open") => sql.push_str(" AND t.status NOT IN ('RESOLVED','WONTFIX')"),
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
        sql.push_str(" ORDER BY t.updated_at DESC LIMIT ");
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

        let mut stmt =
            conn.prepare("SELECT agent_id FROM thread_mentions WHERE thread_id=?1")?;
        let mentions = stmt
            .query_map(params![thread_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;

        let mut stmt = conn.prepare(
            "SELECT m.id,m.thread_id,m.agent_id,a.name,a.role,
                    COALESCE(p.icon, CASE a.role WHEN 'HUMAN' THEN 'user' ELSE 'robot' END),
                    a.color,
                    m.body,m.verdict,m.severity,m.refs,m.tokens_in,m.tokens_out,m.cost_usd,m.created_at
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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = conn.prepare(
            "SELECT r.id,r.thread_id,r.agent_id,a.name,r.status,r.pid,r.exit_code,r.command,
                    r.log,r.started_at,r.ended_at
             FROM agent_runs r JOIN agents a ON a.id=r.agent_id
             WHERE r.thread_id=?1 ORDER BY r.id",
        )?;
        let runs = stmt
            .query_map(params![thread_id], |r| {
                Ok(AgentRun {
                    id: r.get(0)?,
                    thread_id: r.get(1)?,
                    agent_id: r.get(2)?,
                    agent_name: r.get(3)?,
                    status: r.get(4)?,
                    pid: r.get(5)?,
                    exit_code: r.get(6)?,
                    command: r.get(7)?,
                    log: r.get(8)?,
                    started_at: r.get(9)?,
                    ended_at: r.get(10)?,
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
            messages,
            runs,
        })
    }

    // ---------------------------------------------------------- messages ---

    pub fn reply(&self, actor: &AgentCtx, input: NewReply) -> Result<i64> {
        if input.body.trim().is_empty() {
            return Err(Error::Invalid("a reply needs a body".into()));
        }

        let mut guard = self.lock();
        let (room_id, tag_key, status, quorum): (i64, String, String, i64) = guard
            .query_row(
                "SELECT room_id,tag,status,quorum FROM threads WHERE id=?1",
                params![input.thread_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("thread {}", input.thread_id)))?;

        if room_id != actor.room_id {
            return Err(Error::Forbidden("that thread is in another room".into()));
        }

        // --- rails -------------------------------------------------------
        if !actor.is_human() {
            if actor.paused {
                return Err(Error::Forbidden(format!(
                    "room #{} is paused; nothing will be accepted until it is resumed",
                    actor.room_name
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
                    actor.room_name
                )));
            }
        }

        // --- verdict validation ------------------------------------------
        let tag = Self::tag(&guard, &tag_key)?;
        let verdict = match input.verdict.as_deref().map(str::trim) {
            Some(v) if !v.is_empty() => {
                let v = v.to_ascii_uppercase();
                if !tag.verdict_options.is_empty() && !tag.verdict_options.contains(&v) {
                    return Err(Error::Invalid(format!(
                        "verdict for a {} thread must be one of {}",
                        tag.key,
                        tag.verdict_options.join(", ")
                    )));
                }
                Some(v)
            }
            _ => {
                if tag.requires_verdict && actor.role == "ASSISTANT" {
                    return Err(Error::Invalid(format!(
                        "a {} reply must carry a verdict: one of {}",
                        tag.key,
                        tag.verdict_options.join(", ")
                    )));
                }
                None
            }
        };
        let severity = match input.severity.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => {
                let s = s.to_ascii_uppercase();
                if !SEVERITIES.contains(&s.as_str()) {
                    return Err(Error::Invalid(format!(
                        "severity must be one of {}",
                        SEVERITIES.join(", ")
                    )));
                }
                Some(s)
            }
            _ => None,
        };

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

        // Status follows who spoke: an assistant moves it toward the coder once
        // quorum is met; the coder speaking hands the ball back to the room.
        let next_status = if actor.role == "ASSISTANT" {
            let responders: i64 = tx.query_row(
                "SELECT COUNT(DISTINCT m.agent_id) FROM messages m
                 JOIN agents a ON a.id=m.agent_id
                 WHERE m.thread_id=?1 AND a.role='ASSISTANT'",
                params![input.thread_id],
                |r| r.get(0),
            )?;
            if responders >= quorum.max(1) {
                "NEEDS_CODER"
            } else {
                "AWAITING_REPLIES"
            }
        } else if quorum > 0 {
            "AWAITING_REPLIES"
        } else {
            "OPEN"
        };
        tx.execute(
            "UPDATE threads SET status=?1, updated_at=?2 WHERE id=?3 AND status NOT IN ('RESOLVED','WONTFIX')",
            params![next_status, created, input.thread_id],
        )?;

        index_message(&tx, message_id, room_id, &input.body)?;

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
                "status": next_status,
                "role": actor.role,
            }),
        )?;
        tx.commit()?;
        drop(guard);
        self.publish(notice);
        Ok(message_id)
    }

    // -------------------------------------------------------------- runs ---

    pub fn start_run(&self, thread_id: i64, agent_id: i64, command: &str) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO agent_runs(thread_id,agent_id,status,command,started_at)
             VALUES(?1,?2,'RUNNING',?3,?4)",
            params![thread_id, agent_id, command, now()],
        )?;
        let id = conn.last_insert_rowid();
        let room: Option<i64> = conn
            .query_row("SELECT room_id FROM threads WHERE id=?1", params![thread_id], |r| r.get(0))
            .optional()?;
        let notice = Self::append_event(
            &conn,
            room,
            Some(thread_id),
            "run.started",
            Some(agent_id),
            serde_json::json!({"run_id": id}),
        )?;
        drop(conn);
        self.publish(notice);
        Ok(id)
    }

    pub fn append_run_log(&self, run_id: i64, chunk: &str) -> Result<()> {
        let conn = self.lock();
        // Keep the tail only; a chatty agent should not be able to grow the DB
        // without bound.
        conn.execute(
            "UPDATE agent_runs
             SET log = substr(log || ?1, max(1, length(log || ?1) - 60000))
             WHERE id=?2",
            params![chunk, run_id],
        )?;
        Ok(())
    }

    pub fn finish_run(&self, run_id: i64, status: &str, exit_code: Option<i32>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE agent_runs SET status=?1, exit_code=?2, ended_at=?3 WHERE id=?4",
            params![status, exit_code, now(), run_id],
        )?;
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT thread_id,agent_id FROM agent_runs WHERE id=?1",
                params![run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((thread_id, agent_id)) = row {
            let room: Option<i64> = conn
                .query_row("SELECT room_id FROM threads WHERE id=?1", params![thread_id], |r| r.get(0))
                .optional()?;
            let notice = Self::append_event(
                &conn,
                room,
                Some(thread_id),
                "run.finished",
                Some(agent_id),
                serde_json::json!({"run_id": run_id, "status": status, "exit_code": exit_code}),
            )?;
            drop(conn);
            self.publish(notice);
        }
        Ok(())
    }

    pub fn active_run_count(&self, room_id: i64) -> Result<i64> {
        let conn = self.lock();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM agent_runs r JOIN threads t ON t.id=r.thread_id
             WHERE t.room_id=?1 AND r.status='RUNNING'",
            params![room_id],
            |r| r.get(0),
        )?)
    }

    /// Anything still marked RUNNING at startup belongs to a previous process.
    pub fn reap_orphan_runs(&self) -> Result<usize> {
        let conn = self.lock();
        Ok(conn.execute(
            "UPDATE agent_runs SET status='KILLED', ended_at=?1 WHERE status='RUNNING'",
            params![now()],
        )?)
    }

    /// Assistants eligible for auto-dispatch on a thread: explicit mentions if
    /// any, otherwise every auto-dispatch assistant in the room.
    pub fn dispatch_targets(&self, thread_id: i64) -> Result<Vec<i64>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT a.id FROM agents a
             JOIN threads t ON t.room_id=a.room_id
             WHERE t.id=?1 AND a.role='ASSISTANT' AND a.revoked_at IS NULL AND a.auto_dispatch=1
               AND (
                 EXISTS(SELECT 1 FROM thread_mentions m WHERE m.thread_id=t.id AND m.agent_id=a.id)
                 OR NOT EXISTS(SELECT 1 FROM thread_mentions m WHERE m.thread_id=t.id)
               )
             ORDER BY a.name",
        )?;
        let out = stmt
            .query_map(params![thread_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(out)
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
       t.quorum,
       (SELECT COUNT(*) FROM messages m WHERE m.thread_id=t.id),
       (SELECT COUNT(DISTINCT m.agent_id) FROM messages m
          JOIN agents ag ON ag.id=m.agent_id
         WHERE m.thread_id=t.id AND ag.role='ASSISTANT'),
       (SELECT COALESCE(SUM(m.cost_usd),0) FROM messages m WHERE m.thread_id=t.id),
       t.git_ref, t.created_at, t.updated_at, t.resolved_at, a.color
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
        quorum: r.get(9)?,
        reply_count: r.get(10)?,
        responder_count: r.get(11)?,
        cost_usd: r.get(12)?,
        git_ref: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
        resolved_at: r.get(16)?,
        author_color: r.get(17)?,
    })
}
