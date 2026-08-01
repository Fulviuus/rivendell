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
        })
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
                    a.color,a.key_preview,a.system_note,a.created_at,a.revoked_at
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
        sql.push_str(" ORDER BY CASE a.role WHEN 'HUMAN' THEN 0 WHEN 'CODER' THEN 1 ELSE 2 END, a.name");

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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
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
        let conn = self.lock();
        conn.execute("DELETE FROM agents WHERE id=?1", params![agent_id])?;
        Ok(())
    }

    /// Bearer-token lookup. Returns `None` for unknown, malformed or revoked keys.
    pub fn authenticate(&self, token: &str) -> Result<Option<AgentCtx>> {
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
        }))
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

        let mut added = Vec::new();
        for wanted in names {
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
            // Reopening the window is the point: someone was just invited.
            conn.execute(
                "UPDATE threads
                 SET gather_started_at = CASE WHEN gather_started_at IS NULL
                                              THEN NULL ELSE ?1 END,
                     status = CASE WHEN status IN ('RESOLVED','WONTFIX') THEN status
                                   ELSE 'AWAITING_REPLIES' END
                 WHERE id=?2",
                params![now(), thread_id],
            )?;
        }
        Ok(added)
    }

    /// Whether a thread is ready to go back to the coder.
    ///
    /// The rule, in order:
    ///
    ///   1. Nobody has answered yet -> wait. Indefinitely. A question with no
    ///      takers is not a failure, and nothing should time out before a
    ///      single agent has spoken.
    ///   2. The first agent reply opens a window for the others to say they
    ///      are working on it. While that window is open, keep waiting.
    ///   3. Once it closes, the participants are whoever spoke or claimed.
    ///      Anyone still silent is ignored.
    ///   4. Wait for the outstanding claims to turn into replies — but a claim
    ///      that goes quiet past the room's timeout is dropped, so one agent
    ///      that died mid-job cannot hold the thread for ever.
    fn is_ready_for_coder(conn: &Connection, thread_id: i64) -> Result<bool> {
        let (room_id, gather_started_at): (i64, Option<String>) = conn.query_row(
            "SELECT room_id, gather_started_at FROM threads WHERE id=?1",
            params![thread_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        // (1) nobody has answered
        let Some(started) = gather_started_at else {
            return Ok(false);
        };

        let (claim_window, claim_timeout): (i64, i64) = conn.query_row(
            "SELECT claim_window_secs, response_timeout_secs FROM rooms WHERE id=?1",
            params![room_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        // (2) the window for others to put their hand up
        let now = chrono::Utc::now();
        let window_closes =
            (now - chrono::Duration::seconds(claim_window.max(0))).to_rfc3339();
        if started.as_str() > window_closes.as_str() {
            return Ok(false);
        }

        // (3)/(4) claims that are still live and still unanswered
        let stale_before = (now - chrono::Duration::seconds(claim_timeout.max(0))).to_rfc3339();
        let outstanding: i64 = conn.query_row(
            "SELECT COUNT(*) FROM thread_claims c
             JOIN agents a ON a.id=c.agent_id
             WHERE c.thread_id=?1
               AND a.revoked_at IS NULL
               AND c.claimed_at > ?2
               AND NOT EXISTS(SELECT 1 FROM messages m
                               WHERE m.thread_id=?1 AND m.agent_id=c.agent_id)",
            params![thread_id, stale_before],
            |r| r.get(0),
        )?;
        Ok(outstanding == 0)
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
            serde_json::json!({"note": note.trim()}),
        )?;
        drop(conn);
        self.publish(notice);
        Ok(())
    }

    /// Threads whose gather window has closed with nothing outstanding are handed back
    /// to the coder. Runs on a timer so this happens without anyone asking.
    pub fn sweep_stalled_threads(&self) -> Result<usize> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id FROM threads WHERE status='AWAITING_REPLIES'",
        )?;
        let ids: Vec<i64> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);

        let mut notices = Vec::new();
        for id in ids {
            if !Self::is_ready_for_coder(&conn, id)? {
                continue;
            }
            let room: Option<i64> = conn
                .query_row("SELECT room_id FROM threads WHERE id=?1", params![id], |r| r.get(0))
                .optional()?;
            conn.execute(
                "UPDATE threads SET status='NEEDS_CODER', updated_at=?1 WHERE id=?2",
                params![now(), id],
            )?;
            notices.push(Self::append_event(
                &conn,
                room,
                Some(id),
                "thread.status",
                None,
                serde_json::json!({
                    "status": "NEEDS_CODER",
                    "reason": "everyone still working on it has answered",
                }),
            )?);
        }
        drop(conn);
        let n = notices.len();
        for notice in notices {
            self.publish(notice);
        }
        Ok(n)
    }

    // ----------------------------------------------------------- threads ---

    pub fn create_thread(&self, author: &AgentCtx, input: NewThread) -> Result<i64> {
        if !author.is_coder() {
            return Err(Error::Forbidden(
                "only a CODER may open a thread; assistants reply".into(),
            ));
        }
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
        // FYI threads expect nothing; everything else waits, for as long as it
        // takes, until an agent answers.
        let status = if tag.expects_replies { "AWAITING_REPLIES" } else { "OPEN" };
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

    pub fn set_thread_status(&self, actor: &AgentCtx, thread_id: i64, status: &str) -> Result<()> {
        if !STATUSES.contains(&status) {
            return Err(Error::Invalid(format!("unknown status `{status}`")));
        }
        if !actor.is_coder() {
            return Err(Error::Forbidden("only a CODER may change thread status".into()));
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
        let verdict = Self::validate_verdict(&tag, input.verdict.as_deref(), &actor.role)?;
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

        // The first *agent* answer starts the clock for everyone else. A human
        // posting does not: the question is still unanswered.
        if actor.role == "ASSISTANT" {
            tx.execute(
                "UPDATE threads SET gather_started_at=?1
                 WHERE id=?2 AND gather_started_at IS NULL",
                params![created, input.thread_id],
            )?;
        }

        let next_status = if Self::is_ready_for_coder(&tx, input.thread_id)? {
            "NEEDS_CODER"
        } else {
            "AWAITING_REPLIES"
        };
        tx.execute(
            "UPDATE threads SET status=?1, updated_at=?2 WHERE id=?3 AND status NOT IN ('RESOLVED','WONTFIX')",
            params![next_status, created, input.thread_id],
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
                "status": next_status,
                "role": actor.role,
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


    fn validate_verdict(tag: &Tag, raw: Option<&str>, role: &str) -> Result<Option<String>> {
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
            _ => {
                if tag.requires_verdict && role == "ASSISTANT" {
                    return Err(Error::Invalid(format!(
                        "a {} reply must carry a verdict: one of {}",
                        tag.key,
                        tag.verdict_options.join(", ")
                    )));
                }
                Ok(None)
            }
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
        let verdict = Self::validate_verdict(&tag, input.verdict.as_deref(), &actor.role)?;
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
         WHERE m.thread_id=t.id AND ag.role='ASSISTANT') AS responder_count,
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
