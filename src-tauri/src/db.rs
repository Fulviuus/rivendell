//! SQLite schema, migrations and seed data.
//!
//! The `events` table is the spine of the whole app: every state change appends
//! a row, the UI subscribes to it, and agents long-poll it by cursor.

use rusqlite::Connection;

pub const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS projects (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL,
  folder_path TEXT NOT NULL UNIQUE,
  git_remote  TEXT,
  color       TEXT NOT NULL DEFAULT '',
  created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS rooms (
  id                    INTEGER PRIMARY KEY,
  project_id            INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name                  TEXT NOT NULL,
  purpose               TEXT NOT NULL DEFAULT '',
  paused                INTEGER NOT NULL DEFAULT 0,
  max_replies_per_agent INTEGER NOT NULL DEFAULT 6,
  -- After the first agent answers, how long the others get to say they are
  -- working on it. Anyone who stays silent through this window is ignored.
  claim_window_secs     INTEGER NOT NULL DEFAULT 120,
  -- How long an "in progress" claim is honoured without a reply. Re-claiming
  -- refreshes it; going quiet past this drops the claim.
  response_timeout_secs INTEGER NOT NULL DEFAULT 300,
  max_thread_messages   INTEGER NOT NULL DEFAULT 60,
  max_concurrent_runs   INTEGER NOT NULL DEFAULT 3,
  cost_cap_usd          REAL    NOT NULL DEFAULT 5.0,
  created_at            TEXT NOT NULL,
  UNIQUE(project_id, name)
);

-- What flavour of agent this is. This is what the "which assistant is it"
-- dropdown selects: a label and an icon. Agents are started by you and connect
-- on their own, so nothing here launches anything.
CREATE TABLE IF NOT EXISTS agent_profiles (
  id               INTEGER PRIMARY KEY,
  key              TEXT NOT NULL UNIQUE,
  label            TEXT NOT NULL,
  icon             TEXT NOT NULL,
  launch_cmd       TEXT NOT NULL,
  launch_args      TEXT NOT NULL,          -- JSON array, may contain {placeholders}
  mcp_install_mode TEXT NOT NULL,          -- config_file_flag | env | none
  notes            TEXT NOT NULL DEFAULT '',
  builtin          INTEGER NOT NULL DEFAULT 0
);

-- An agent is defined once per project and joins rooms, the way a person
-- belongs to a workspace and is in some of its channels. Its key identifies
-- the agent; room_members decides what it can see.
CREATE TABLE IF NOT EXISTS agents (
  id            INTEGER PRIMARY KEY,
  project_id    INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name          TEXT NOT NULL,
  role          TEXT NOT NULL CHECK(role IN ('CODER','ASSISTANT','HUMAN')),
  profile_id    INTEGER REFERENCES agent_profiles(id) ON DELETE SET NULL,
  key_id        TEXT UNIQUE,
  key_hash      TEXT,
  key_preview   TEXT,
  auto_dispatch INTEGER NOT NULL DEFAULT 1,
  system_note   TEXT NOT NULL DEFAULT '',
  -- Empty means "pick one from the name"; a value here is an explicit choice.
  color         TEXT NOT NULL DEFAULT '',
  created_at    TEXT NOT NULL,
  revoked_at    TEXT,
  UNIQUE(project_id, name)
);

CREATE TABLE IF NOT EXISTS room_members (
  room_id   INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
  agent_id  INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  joined_at TEXT NOT NULL,
  PRIMARY KEY (room_id, agent_id)
);

-- Tags route work: they decide who gets pulled in, what they are told, and
-- what shape their reply must take.
CREATE TABLE IF NOT EXISTS tags (
  id              INTEGER PRIMARY KEY,
  key             TEXT NOT NULL UNIQUE,
  label           TEXT NOT NULL,
  color           TEXT NOT NULL,
  instruction     TEXT NOT NULL,
  requires_verdict INTEGER NOT NULL DEFAULT 0,
  verdict_options TEXT NOT NULL DEFAULT '[]',
  -- Legacy name. Now purely a flag: 0 means the tag expects no replies (FYI),
  -- anything else means it does.
  default_quorum  INTEGER NOT NULL DEFAULT 1,
  sort            INTEGER NOT NULL DEFAULT 0,
  builtin         INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS threads (
  id                 INTEGER PRIMARY KEY,
  room_id            INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
  author_agent_id    INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  title              TEXT NOT NULL,
  body               TEXT NOT NULL,
  tag                TEXT NOT NULL,
  status             TEXT NOT NULL,
  git_ref            TEXT,
  git_dirty          INTEGER NOT NULL DEFAULT 0,
  -- When the first agent answered. Null means the thread is still waiting,
  -- and it waits indefinitely — nothing times out before anyone has spoken.
  gather_started_at  TEXT,
  resolution_summary TEXT,
  export_path        TEXT,
  created_at         TEXT NOT NULL,
  updated_at         TEXT NOT NULL,
  resolved_at        TEXT
);
CREATE INDEX IF NOT EXISTS idx_threads_room ON threads(room_id, status);

-- Context is snapshotted at post time so a review stays reproducible even
-- after the coder keeps typing.
CREATE TABLE IF NOT EXISTS thread_context (
  id         INTEGER PRIMARY KEY,
  thread_id  INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL,          -- file | diff | note
  path       TEXT,
  start_line INTEGER,
  end_line   INTEGER,
  content    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ctx_thread ON thread_context(thread_id);

-- "I am working on this." Doubles as a heartbeat: a long-running assistant
-- re-claims to keep the thread waiting for it.
CREATE TABLE IF NOT EXISTS thread_claims (
  thread_id  INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
  agent_id   INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  note       TEXT NOT NULL DEFAULT '',
  claimed_at TEXT NOT NULL,
  PRIMARY KEY (thread_id, agent_id)
);

CREATE TABLE IF NOT EXISTS thread_mentions (
  thread_id INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
  agent_id  INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  PRIMARY KEY (thread_id, agent_id)
);

CREATE TABLE IF NOT EXISTS messages (
  id         INTEGER PRIMARY KEY,
  thread_id  INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
  agent_id   INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  body       TEXT NOT NULL,
  verdict    TEXT,
  severity   TEXT,
  refs       TEXT NOT NULL DEFAULT '[]',
  tokens_in  INTEGER NOT NULL DEFAULT 0,
  tokens_out INTEGER NOT NULL DEFAULT 0,
  cost_usd   REAL    NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  -- Set the first time the author revises the message. Null means untouched.
  edited_at  TEXT
);
CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(thread_id, id);

CREATE TABLE IF NOT EXISTS events (
  seq            INTEGER PRIMARY KEY AUTOINCREMENT,
  room_id        INTEGER,
  thread_id      INTEGER,
  kind           TEXT NOT NULL,
  actor_agent_id INTEGER,
  payload        TEXT NOT NULL DEFAULT '{}',
  created_at     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_room ON events(room_id, seq);

CREATE TABLE IF NOT EXISTS agent_runs (
  id         INTEGER PRIMARY KEY,
  thread_id  INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
  agent_id   INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  status     TEXT NOT NULL,          -- RUNNING | EXITED | FAILED | KILLED
  pid        INTEGER,
  exit_code  INTEGER,
  command    TEXT NOT NULL,
  log        TEXT NOT NULL DEFAULT '',
  started_at TEXT NOT NULL,
  ended_at   TEXT
);
CREATE INDEX IF NOT EXISTS idx_runs_thread ON agent_runs(thread_id);

-- Every jailed read is recorded, so you can see what each assistant looked at.
CREATE TABLE IF NOT EXISTS file_access_log (
  id         INTEGER PRIMARY KEY,
  agent_id   INTEGER NOT NULL,
  thread_id  INTEGER,
  path       TEXT NOT NULL,
  allowed    INTEGER NOT NULL,
  reason     TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL
);
"#;

const FTS: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS search_index USING fts5(
  kind, ref_id UNINDEXED, room_id UNINDEXED, title, body
);
"#;

/// Copies the database aside before anything that rewrites a table.
///
/// A migration that goes wrong can be unrecoverable, and the loss is silent —
/// you notice when content is missing, long after the run that did it. A file
/// copy costs nothing at this size and turns that into an inconvenience.
fn back_up_before_migrating(path: &std::path::Path, conn: &Connection) -> rusqlite::Result<()> {
    let needed = {
        let mut stmt =
            conn.prepare("SELECT 1 FROM pragma_table_info('agents') WHERE name='room_id'")?;
        stmt.exists([])?
    };
    if !needed || !path.exists() {
        return Ok(());
    }
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let backup = path.with_extension(format!("pre-migration-{stamp}.db"));
    // A plain file copy would miss anything still in the WAL.
    let mut target = Connection::open(&backup)?;
    rusqlite::backup::Backup::new(conn, &mut target)?
        .run_to_completion(64, std::time::Duration::from_millis(0), None)?;
    tracing::info!("database backed up to {}", backup.display());
    Ok(())
}

pub fn open(path: &std::path::Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(SCHEMA)?;
    // FTS5 ships with the bundled SQLite in every build we care about, but we
    // degrade to LIKE-based search rather than refusing to start if it doesn't.
    let has_fts = conn.execute_batch(FTS).is_ok();
    conn.execute(
        "INSERT INTO meta(key,value) VALUES('fts', ?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [if has_fts { "1" } else { "0" }],
    )?;
    back_up_before_migrating(path, &conn)?;
    migrate_agents_to_projects(&conn)?;
    migrate(&conn)?;
    seed(&conn)?;
    ensure_human_in_every_room(&conn)?;
    Ok(conn)
}

/// Moves agents from belonging to a room to belonging to a project, with
/// membership held separately.
///
/// SQLite cannot drop a column or change a UNIQUE constraint in place, so this
/// rebuilds the table. Names only had to be unique per room before and now must
/// be unique per project, so collisions have to be resolved rather than
/// crashing the migration:
///
///   * duplicate HUMANs are the same person in different rooms, so they merge
///     and their messages are re-pointed at the survivor
///   * anything else keeps the first and suffixes the rest with their room, so
///     no agent silently loses its identity or its key
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn migrate_agents_to_projects(conn: &Connection) -> rusqlite::Result<()> {
    let already = {
        let mut stmt =
            conn.prepare("SELECT 1 FROM pragma_table_info('agents') WHERE name='project_id'")?;
        stmt.exists([])?
    };
    let has_rooms = {
        let mut stmt =
            conn.prepare("SELECT 1 FROM pragma_table_info('agents') WHERE name='room_id'")?;
        stmt.exists([])?
    };
    if already || !has_rooms {
        return Ok(());
    }
    tracing::info!("migrating agents from rooms to projects");

    // Must be outside the transaction — SQLite ignores this pragma inside one.
    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;

    // Everything from here is one transaction. Without it, a failure partway
    // leaves the database half-converted: rows already merged away and a
    // stranded agents_new that makes every later attempt fail on CREATE.
    let conn2 = conn;
    let tx = conn.unchecked_transaction()?;
    let conn = &tx;

    // A previous attempt may have died after CREATE. Its table is empty and
    // worthless, and its presence is what blocks the retry.
    conn.execute_batch("DROP TABLE IF EXISTS agents_new;")?;

    // (id, project_id, room_id, name, role) in creation order, so "first wins"
    // means the oldest keeps its name.
    let rows: Vec<(i64, i64, i64, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT a.id, r.project_id, a.room_id, a.name, a.role
             FROM agents a JOIN rooms r ON r.id=a.room_id ORDER BY a.id",
        )?;
        // Bind before the block ends: the iterator borrows `stmt`.
        let collected = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        collected
    };

    let mut taken: std::collections::HashMap<(i64, String), i64> = std::collections::HashMap::new();
    let mut merges: Vec<(i64, i64, i64)> = Vec::new(); // (loser, winner, loser's room)
    let mut renames: Vec<(i64, String)> = Vec::new();

    for (id, project_id, room_id, name, role) in &rows {
        match taken.get(&(*project_id, name.to_lowercase())) {
            None => {
                taken.insert((*project_id, name.to_lowercase()), *id);
            }
            Some(winner) if role == "HUMAN" => merges.push((*id, *winner, *room_id)),
            Some(_) => {
                let room: String = conn.query_row(
                    "SELECT name FROM rooms WHERE id=?1",
                    [room_id],
                    |r| r.get(0),
                )?;
                let mut candidate = format!("{name}-{room}");
                let mut n = 2;
                while taken.contains_key(&(*project_id, candidate.to_lowercase())) {
                    candidate = format!("{name}-{room}-{n}");
                    n += 1;
                }
                taken.insert((*project_id, candidate.to_lowercase()), *id);
                renames.push((*id, candidate));
            }
        }
    }

    for (id, name) in &renames {
        conn.execute("UPDATE agents SET name=?1 WHERE id=?2", rusqlite::params![name, id])?;
        tracing::info!("renamed duplicate agent {id} to {name}");
    }
    for (loser, winner, _) in &merges {
        for table in ["messages", "thread_claims", "thread_mentions", "file_access_log"] {
            let _ = conn.execute(
                &format!("UPDATE OR IGNORE {table} SET agent_id=?1 WHERE agent_id=?2"),
                rusqlite::params![winner, loser],
            );
        }
        let _ = conn.execute(
            "UPDATE threads SET author_agent_id=?1 WHERE author_agent_id=?2",
            rusqlite::params![winner, loser],
        );
        // Drop the loser before the rebuild, or the copy hits the new
        // per-project name constraint that this merge exists to satisfy.
        conn.execute("DELETE FROM agents WHERE id=?1", [loser])?;
    }

    conn.execute_batch(
        "CREATE TABLE agents_new (
           id            INTEGER PRIMARY KEY,
           project_id    INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
           name          TEXT NOT NULL,
           role          TEXT NOT NULL CHECK(role IN ('CODER','ASSISTANT','HUMAN')),
           profile_id    INTEGER REFERENCES agent_profiles(id) ON DELETE SET NULL,
           key_id        TEXT UNIQUE,
           key_hash      TEXT,
           key_preview   TEXT,
           system_note   TEXT NOT NULL DEFAULT '',
           color         TEXT NOT NULL DEFAULT '',
           created_at    TEXT NOT NULL,
           revoked_at    TEXT,
           UNIQUE(project_id, name)
         );

         INSERT INTO agents_new
           SELECT a.id, r.project_id, a.name, a.role, a.profile_id, a.key_id, a.key_hash,
                  a.key_preview, a.system_note, a.color, a.created_at, a.revoked_at
           FROM agents a JOIN rooms r ON r.id=a.room_id
           WHERE a.id IN (SELECT MIN(id) FROM agents GROUP BY id);

         CREATE TABLE IF NOT EXISTS room_members (
           room_id   INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
           agent_id  INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
           joined_at TEXT NOT NULL,
           PRIMARY KEY (room_id, agent_id)
         );

         INSERT OR IGNORE INTO room_members(room_id, agent_id, joined_at)
           SELECT a.room_id, a.id, a.created_at FROM agents a;",
    )?;

    // The survivor inherits the rooms the merged rows were in.
    for (_, winner, room) in &merges {
        conn.execute(
            "INSERT OR IGNORE INTO room_members(room_id, agent_id, joined_at) VALUES(?1,?2,?3)",
            rusqlite::params![room, winner, now_iso()],
        )?;
    }

    conn.execute_batch(
        "DROP TABLE agents;
         ALTER TABLE agents_new RENAME TO agents;",
    )?;
    tx.commit()?;
    // Safe to restore now that the rebuild has committed.
    conn2.execute_batch("PRAGMA foreign_keys = ON;")?;
    tracing::info!(
        "agents migrated: {} renamed, {} merged",
        renames.len(),
        merges.len()
    );
    Ok(())
}

/// You are a participant in every room, not a spectator, so the project's HUMAN
/// belongs to all of them. Rooms can end up without one — a room created before
/// this was an invariant, or one whose human was merged away by the move to
/// project-level agents — and a room with no human is one you cannot post in.
fn ensure_human_in_every_room(conn: &Connection) -> rusqlite::Result<()> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO room_members(room_id, agent_id, joined_at)
         SELECT r.id, a.id, ?1
         FROM rooms r
         JOIN agents a ON a.project_id = r.project_id AND a.role = 'HUMAN'
         WHERE NOT EXISTS(
           SELECT 1 FROM room_members m
           JOIN agents ma ON ma.id = m.agent_id
           WHERE m.room_id = r.id AND ma.role = 'HUMAN')",
        [now_iso()],
    )?;
    if n > 0 {
        tracing::info!("joined you to {n} room(s) that had no human");
    }
    Ok(())
}

/// Additive column migrations for databases created by an earlier build.
/// `CREATE TABLE IF NOT EXISTS` silently skips existing tables, so new columns
/// have to be added explicitly or they only appear on a fresh install.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    for (table, column, decl) in [
        ("agents", "color", "TEXT NOT NULL DEFAULT ''"),
        ("rooms", "response_timeout_secs", "INTEGER NOT NULL DEFAULT 300"),
        ("messages", "edited_at", "TEXT"),
        ("projects", "color", "TEXT NOT NULL DEFAULT ''"),
        ("rooms", "claim_window_secs", "INTEGER NOT NULL DEFAULT 120"),
        ("threads", "gather_started_at", "TEXT"),
    ] {
        let exists: bool = conn
            .prepare(&format!(
                "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"
            ))?
            .exists([column])?;
        if !exists {
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"), [])?;
            tracing::info!("migrated: added {table}.{column}");
        }
    }
    Ok(())
}

pub fn fts_available(conn: &Connection) -> bool {
    conn.query_row("SELECT value FROM meta WHERE key='fts'", [], |r| {
        r.get::<_, String>(0)
    })
    .map(|v| v == "1")
    .unwrap_or(false)
}

fn seed(conn: &Connection) -> rusqlite::Result<()> {
    seed_profiles(conn)?;
    seed_tags(conn)?;
    Ok(())
}

/// Launch recipes. Placeholders substituted at spawn time:
///   {prompt} {mcp_config} {cwd} {api_key} {mcp_url} {thread_id} {agent_name}
fn seed_profiles(conn: &Connection) -> rusqlite::Result<()> {
    let profiles: &[(&str, &str, &str, &str, &str, &str, &str)] = &[
        (
            "claude-code",
            "Claude Code",
            "claude",
            "claude",
            r#"["-p","{prompt}","--mcp-config","{mcp_config}","--allowed-tools","mcp__rivendell","--permission-mode","acceptEdits","--output-format","json"]"#,
            "config_file_flag",
            "Anthropic's Claude Code.",
        ),
        (
            "codex",
            "Codex CLI",
            "openai",
            "codex",
            r#"["exec","--skip-git-repo-check","{prompt}"]"#,
            "env",
            "OpenAI's Codex CLI.",
        ),
        (
            "gemini-cli",
            "Gemini CLI",
            "gemini",
            "gemini",
            r#"["-p","{prompt}"]"#,
            "env",
            "Google's Gemini CLI.",
        ),
        (
            "cursor-agent",
            "Cursor Agent",
            "cursor",
            "cursor-agent",
            r#"["-p","{prompt}"]"#,
            "env",
            "Cursor's agent.",
        ),
        (
            "shell",
            "Custom command",
            "terminal",
            "sh",
            r#"["-c","{prompt}"]"#,
            "env",
            "Anything else — a script or a client of your own.",
        ),
        (
            "external",
            "External / manual",
            "user",
            "",
            r#"[]"#,
            "none",
            "Unspecified. Fine for any client that speaks MCP.",
        ),
    ];

    for (key, label, icon, cmd, args, mode, notes) in profiles {
        conn.execute(
            "INSERT INTO agent_profiles(key,label,icon,launch_cmd,launch_args,mcp_install_mode,notes,builtin)
             VALUES(?1,?2,?3,?4,?5,?6,?7,1)
             ON CONFLICT(key) DO UPDATE SET
               label=excluded.label, icon=excluded.icon, notes=excluded.notes
             WHERE agent_profiles.builtin=1",
            rusqlite::params![key, label, icon, cmd, args, mode, notes],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real upgrade path: agents used to belong to a room, and names only
    /// had to be unique within one. Moving them to the project has to resolve
    /// the collisions that creates rather than failing or losing rows.
    #[test]
    fn migrates_agents_from_rooms_to_projects() {
        let path =
            std::env::temp_dir().join(format!("rivendell-agentmig-{}.db", uuid::Uuid::new_v4()));
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                   folder_path TEXT NOT NULL UNIQUE, git_remote TEXT, created_at TEXT NOT NULL);
                 CREATE TABLE rooms (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
                   name TEXT NOT NULL, created_at TEXT NOT NULL);
                 CREATE TABLE agents (
                   id INTEGER PRIMARY KEY, room_id INTEGER NOT NULL, name TEXT NOT NULL,
                   role TEXT NOT NULL, profile_id INTEGER, key_id TEXT UNIQUE, key_hash TEXT,
                   key_preview TEXT, auto_dispatch INTEGER NOT NULL DEFAULT 1,
                   system_note TEXT NOT NULL DEFAULT '', color TEXT NOT NULL DEFAULT '',
                   created_at TEXT NOT NULL, revoked_at TEXT, UNIQUE(room_id, name));
                 CREATE TABLE messages (id INTEGER PRIMARY KEY, thread_id INTEGER NOT NULL,
                   agent_id INTEGER NOT NULL, body TEXT NOT NULL, created_at TEXT NOT NULL);

                 INSERT INTO projects VALUES (1,'demo','/tmp/demo-mig',NULL,'t');
                 INSERT INTO rooms VALUES (1,1,'general','t'), (2,1,'test','t');
                 -- the same person in two rooms
                 INSERT INTO agents (id,room_id,name,role,key_id,created_at)
                   VALUES (1,1,'you','HUMAN','k1','t'), (2,2,'you','HUMAN','k2','t');
                 -- and a genuinely different agent that happens to share a name
                 INSERT INTO agents (id,room_id,name,role,key_id,created_at)
                   VALUES (3,1,'skeptic','ASSISTANT','k3','t'),
                          (4,2,'skeptic','ASSISTANT','k4','t');
                 INSERT INTO messages VALUES (1,1,2,'from the second you','t');",
            )
            .unwrap();
        }

        let conn = open(&path).unwrap();

        // Duplicate HUMANs are one person: merged, and their words follow them.
        let humans: i64 = conn
            .query_row("SELECT COUNT(*) FROM agents WHERE role='HUMAN'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(humans, 1, "the two `you` rows are the same person");
        let author: i64 = conn
            .query_row("SELECT agent_id FROM messages WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(author, 1, "the merged agent inherits the messages");

        // Two real agents that merely shared a name both survive, with keys.
        let names: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM agents WHERE role='ASSISTANT' ORDER BY id")
                .unwrap();
            let v = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<String>>>()
                .unwrap();
            v
        };
        assert_eq!(
            names,
            vec!["skeptic".to_string(), "skeptic-test".to_string()],
            "the older keeps the name, the other is suffixed with its room"
        );
        let keys: i64 = conn
            .query_row("SELECT COUNT(*) FROM agents WHERE key_id IS NOT NULL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(keys, 3, "nobody loses their key");

        // Everyone ends up in the room they were in.
        let membership: Vec<(i64, i64)> = {
            let mut stmt = conn
                .prepare("SELECT agent_id, room_id FROM room_members ORDER BY agent_id, room_id")
                .unwrap();
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            v
        };
        assert_eq!(
            membership,
            vec![(1, 1), (1, 2), (3, 1), (4, 2)],
            "the merged human is in both rooms; the others stay where they were"
        );

        // And agents now hang off the project.
        let project: i64 = conn
            .query_row("SELECT project_id FROM agents WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(project, 1);

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    /// A migration that died partway leaves an empty `agents_new` behind, and
    /// every later attempt then failed on CREATE — the database could never
    /// finish upgrading without manual surgery. Retrying has to just work.
    #[test]
    fn recovers_from_a_half_finished_migration() {
        let path =
            std::env::temp_dir().join(format!("rivendell-halfmig-{}.db", uuid::Uuid::new_v4()));
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                   folder_path TEXT NOT NULL UNIQUE, git_remote TEXT, created_at TEXT NOT NULL);
                 CREATE TABLE rooms (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
                   name TEXT NOT NULL, created_at TEXT NOT NULL);
                 CREATE TABLE agents (
                   id INTEGER PRIMARY KEY, room_id INTEGER NOT NULL, name TEXT NOT NULL,
                   role TEXT NOT NULL, profile_id INTEGER, key_id TEXT UNIQUE, key_hash TEXT,
                   key_preview TEXT, system_note TEXT NOT NULL DEFAULT '',
                   color TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL, revoked_at TEXT,
                   UNIQUE(room_id, name));
                 INSERT INTO projects VALUES (1,'demo','/tmp/demo-half',NULL,'t');
                 INSERT INTO rooms VALUES (1,1,'general','t');
                 INSERT INTO agents (id,room_id,name,role,key_id,created_at)
                   VALUES (1,1,'you','HUMAN','k1','t'), (2,1,'main','CODER','k2','t');

                 -- the debris of a run that died after CREATE
                 CREATE TABLE agents_new (
                   id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, name TEXT NOT NULL,
                   role TEXT NOT NULL, profile_id INTEGER, key_id TEXT UNIQUE, key_hash TEXT,
                   key_preview TEXT, system_note TEXT NOT NULL DEFAULT '',
                   color TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL, revoked_at TEXT,
                   UNIQUE(project_id, name));",
            )
            .unwrap();
        }

        let conn = open(&path).expect("a half-finished migration must be recoverable");

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM agents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "both agents survive the retry");
        let has_project: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('agents') WHERE name='project_id'")
            .unwrap()
            .exists([])
            .unwrap();
        assert!(has_project, "and the table really was rebuilt");
        let leftover: bool = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE name='agents_new'")
            .unwrap()
            .exists([])
            .unwrap();
        assert!(!leftover, "no debris is left for the next run to trip on");

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    /// Rebuilding `agents` drops the table, and `threads.author_agent_id`
    /// references it ON DELETE CASCADE. If foreign keys are enforced at that
    /// moment, every thread in the database goes with it — and everything that
    /// cascades from threads after that.
    ///
    /// The earlier migration tests had no `threads` table, so there was no
    /// cascade path for them to expose. This one does.
    #[test]
    fn migrating_agents_does_not_take_the_threads_with_them() {
        let path =
            std::env::temp_dir().join(format!("rivendell-fkmig-{}.db", uuid::Uuid::new_v4()));
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                   folder_path TEXT NOT NULL UNIQUE, git_remote TEXT, created_at TEXT NOT NULL);
                 CREATE TABLE rooms (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL
                   REFERENCES projects(id) ON DELETE CASCADE,
                   name TEXT NOT NULL, created_at TEXT NOT NULL);
                 CREATE TABLE agents (
                   id INTEGER PRIMARY KEY, room_id INTEGER NOT NULL REFERENCES rooms(id)
                     ON DELETE CASCADE,
                   name TEXT NOT NULL, role TEXT NOT NULL, profile_id INTEGER, key_id TEXT UNIQUE,
                   key_hash TEXT, key_preview TEXT, system_note TEXT NOT NULL DEFAULT '',
                   color TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL, revoked_at TEXT,
                   UNIQUE(room_id, name));
                 CREATE TABLE threads (
                   id INTEGER PRIMARY KEY,
                   room_id INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
                   author_agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
                   title TEXT NOT NULL, body TEXT NOT NULL, tag TEXT NOT NULL,
                   status TEXT NOT NULL, git_dirty INTEGER NOT NULL DEFAULT 0,
                   created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
                 CREATE TABLE messages (
                   id INTEGER PRIMARY KEY,
                   thread_id INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
                   agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
                   body TEXT NOT NULL, created_at TEXT NOT NULL);

                 INSERT INTO projects VALUES (1,'demo','/tmp/demo-fk',NULL,'t');
                 INSERT INTO rooms VALUES (1,1,'general','t'), (2,1,'test','t');
                 INSERT INTO agents (id,room_id,name,role,key_id,created_at)
                   VALUES (1,1,'you','HUMAN','k1','t'), (2,2,'you','HUMAN','k2','t'),
                          (3,1,'main','CODER','k3','t');
                 INSERT INTO threads VALUES (1,1,3,'A real thread','…','FYI','OPEN',0,'t','t');
                 INSERT INTO messages VALUES (1,1,3,'a real reply','t');",
            )
            .unwrap();
        }

        let conn = open(&path).unwrap();

        let threads: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(threads, 1, "the migration must not destroy threads");
        let messages: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(messages, 1, "nor the messages that hang off them");

        // The thread's author must still resolve to a real agent.
        let author: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM threads t JOIN agents a ON a.id=t.author_agent_id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(author, 1, "and still point at its author");

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let path = std::env::temp_dir().join(format!("rivendell-migrate2-{}.db", uuid::Uuid::new_v4()));
        drop(open(&path).unwrap());
        drop(open(&path).unwrap());
        let conn = open(&path).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('agents') WHERE name='color'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
}

fn seed_tags(conn: &Connection) -> rusqlite::Result<()> {
    // (key, label, color, instruction, requires_verdict, verdict_options, expects_replies, sort)
    let tags: &[(&str, &str, &str, &str, i64, &str, i64, i64)] = &[
        (
            "HELP_REQUEST",
            "Help request",
            "sky",
            "The coder is stuck and wants a concrete way forward. Read the attached context, diagnose the actual cause, and give a specific fix — code, not advice. If you cannot tell from the context provided, say exactly what you would need and reply with verdict NEEDS_INFO rather than guessing.",
            1,
            r#"["ANSWERED","NEEDS_INFO"]"#,
            1,
            10,
        ),
        (
            "ADVERSARIAL_REVIEW",
            "Adversarial review",
            "rose",
            "Your job is to REFUTE this change, not to praise it. Hunt for the input that breaks it: boundary values, empty and huge collections, concurrency, partial failure, error paths that swallow, assumptions that hold only on the happy path. For each issue give a concrete failing scenario — inputs and the resulting wrong behaviour. If after a genuine attempt you cannot break it, reply REFUTED with what you tried. Default to UNCERTAIN over inventing a plausible-sounding bug.",
            1,
            r#"["CONFIRMED","REFUTED","UNCERTAIN"]"#,
            2,
            20,
        ),
        (
            "DESIGN_REVIEW",
            "Design review",
            "violet",
            "Assess the design against what it is actually trying to do. Name the tradeoff being made, say whether it is the right one, and give the strongest alternative you would defend. Be specific about what would have to be true for your alternative to win.",
            1,
            r#"["APPROVED","CONCERNS","REJECTED"]"#,
            2,
            30,
        ),
        (
            "SECURITY_REVIEW",
            "Security review",
            "amber",
            "Look for exploitable problems, not lint. Injection, authz gaps, path traversal, secret handling, unsafe deserialization, TOCTOU, resource exhaustion. For each finding give the attack path concretely. Rate severity honestly — inflating severity is worse than missing a low.",
            1,
            r#"["CONFIRMED","REFUTED","UNCERTAIN"]"#,
            2,
            40,
        ),
        (
            "ARCHITECTURE_DECISION",
            "Architecture decision",
            "emerald",
            "This is a decision record in progress. Argue for one option, state the cost you are accepting, and name what would make you change your mind. Do not fence-sit.",
            1,
            r#"["APPROVED","CONCERNS","REJECTED"]"#,
            2,
            50,
        ),
        (
            "SPEC_CLARIFICATION",
            "Spec clarification",
            "cyan",
            "The requirement is ambiguous. Enumerate the readings, say which one you would build under and why, and flag anything that would be expensive to get wrong.",
            1,
            r#"["ANSWERED","NEEDS_INFO"]"#,
            1,
            60,
        ),
        (
            "PERF",
            "Performance",
            "orange",
            "Find where the time or memory actually goes. Prefer measurement over intuition; if you are reasoning without data, say so. Give the expected magnitude of any improvement you propose.",
            1,
            r#"["CONFIRMED","REFUTED","UNCERTAIN"]"#,
            1,
            70,
        ),
        (
            "FYI",
            "FYI",
            "slate",
            "Context only. No reply is required unless something looks wrong.",
            0,
            r#"[]"#,
            0,
            80,
        ),
    ];

    for (key, label, color, instruction, rv, opts, expects_replies, sort) in tags {
        conn.execute(
            "INSERT INTO tags(key,label,color,instruction,requires_verdict,verdict_options,default_quorum,sort,builtin)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1)
             ON CONFLICT(key) DO UPDATE SET
               label=excluded.label, color=excluded.color, sort=excluded.sort
             WHERE tags.builtin=1",
            rusqlite::params![key, label, color, instruction, rv, opts, expects_replies, sort],
        )?;
    }
    Ok(())
}
