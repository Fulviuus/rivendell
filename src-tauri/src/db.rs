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

CREATE TABLE IF NOT EXISTS agents (
  id            INTEGER PRIMARY KEY,
  room_id       INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
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
  UNIQUE(room_id, name)
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
    migrate(&conn)?;
    seed(&conn)?;
    Ok(conn)
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

    /// A database written by an earlier build has an `agents` table without
    /// `color`. `CREATE TABLE IF NOT EXISTS` will not add it, so `migrate` has
    /// to — otherwise every agent query fails on an upgrade.
    #[test]
    fn adds_missing_columns_to_an_existing_database() {
        let path = std::env::temp_dir().join(format!("rivendell-migrate-{}.db", uuid::Uuid::new_v4()));
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE agents (
                   id INTEGER PRIMARY KEY, room_id INTEGER NOT NULL, name TEXT NOT NULL,
                   role TEXT NOT NULL, profile_id INTEGER, key_id TEXT, key_hash TEXT,
                   key_preview TEXT, auto_dispatch INTEGER NOT NULL DEFAULT 1,
                   system_note TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL,
                   revoked_at TEXT);",
            )
            .unwrap();
            let has: bool = conn
                .prepare("SELECT 1 FROM pragma_table_info('agents') WHERE name='color'")
                .unwrap()
                .exists([])
                .unwrap();
            assert!(!has, "precondition: the old table has no colour column");
        }

        let conn = open(&path).unwrap();
        let has: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('agents') WHERE name='color'")
            .unwrap()
            .exists([])
            .unwrap();
        assert!(has, "migrate should have added agents.color");

        // And the added column must be usable, not just present.
        conn.execute(
            "INSERT INTO agents(room_id,name,role,created_at) VALUES(1,'x','CODER','now')",
            [],
        )
        .unwrap();
        let color: String = conn
            .query_row("SELECT color FROM agents WHERE name='x'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(color, "");

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
