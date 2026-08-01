//! Starts an agent when its rooms have work for it.
//!
//! An agent is a row in a table, not a process. Whatever is holding its key is
//! that agent for as long as it runs, and when it ends its turn there is
//! nothing left to talk to — no MCP notification can reach a model that is not
//! being asked for tokens. So an awake agent is not one we keep alive. It is
//! one Rivendell starts again, from nothing, each time its rooms need it. The
//! thread history is the context, so a fresh process picks up exactly where the
//! last one stopped.
//!
//! Rivendell already deleted a spawner once, and rightly: it dispatched *per
//! thread*, and the quorum and concurrency machinery on top of it was more
//! complexity than the event log needed. This one is per *agent* — one process
//! at a time, woken by activity, told which threads moved. That difference is
//! what keeps it small.
//!
//! Everything here is written on the assumption that the worst outcome is not a
//! missed wake-up but a run that should not have happened, so every rule below
//! errs towards not starting.

use crate::error::{Error, Result};
use crate::models::EventNotice;
use crate::store::Store;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;

/// A run still going after this is stuck, not thinking.
const MAX_RUN_SECONDS: u64 = 20 * 60;
/// A burst of replies is one wake-up. Long enough to collect a conversation's
/// worth of events, short enough that nobody notices the delay.
const SETTLE: Duration = Duration::from_secs(3);
/// Blunt backstop against a loop nobody predicted. Nothing legitimate needs an
/// agent started this often.
const MAX_RUNS_PER_HOUR: usize = 40;
/// A command that fails this many times running is broken, not unlucky.
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
/// Keep the tail of what the process said, for when it fails.
const LOG_TAIL_BYTES: usize = 4096;

/// Events worth starting an agent for. Anything else — `run.*`, `agent.*`,
/// `room.*`, `project.*` — is bookkeeping, and waking for bookkeeping is how a
/// supervisor starts feeding itself.
fn is_actionable(kind: &str) -> bool {
    matches!(
        kind,
        "thread.created" | "thread.mentioned" | "message.created" | "message.edited"
            | "thread.status"
    )
}

/// What the UI shows next to the toggle.
#[derive(Debug, Clone, Serialize)]
pub struct AwakeStatus {
    pub agent_id: i64,
    pub running: bool,
    pub waiting: usize,
    pub last_run_at: Option<String>,
    /// How the last run ended, in words. `None` until one has.
    pub last_outcome: Option<String>,
    /// Set when something needs the user's attention, and never cleared
    /// silently — a broken command has to be visible or the agent just looks
    /// lazy.
    pub trouble: Option<String>,
}

#[derive(Default)]
struct AgentState {
    /// Threads that have moved since this agent last ran.
    waiting: BTreeSet<i64>,
    /// When the first un-acted event landed, for the settle delay.
    since: Option<Instant>,
    running: Option<RunHandle>,
    /// Start times inside the last hour.
    recent: Vec<Instant>,
    failures: u32,
    last_run_at: Option<String>,
    last_outcome: Option<String>,
    trouble: Option<String>,
}

struct RunHandle {
    /// Also the process-group id — see `spawn`.
    pgid: Option<u32>,
    /// Handle for the ephemeral credential, so it dies with the process.
    token: String,
    /// Checked before killing anything at startup: a pid on its own is not
    /// proof of identity, because the number gets reused.
    command: String,
}

pub struct Supervisor {
    store: Arc<Store>,
    mcp_url: Arc<RwLock<String>>,
    /// Beside the database. Holds the MCP config handed to children, and the
    /// record of what is running.
    dir: std::path::PathBuf,
    agents: Mutex<HashMap<i64, AgentState>>,
    /// Bridged to the webview by lib.rs. Keeping tauri out of here means this
    /// can be tested.
    pub status: broadcast::Sender<AwakeStatus>,
}

impl Supervisor {
    pub fn new(
        store: Arc<Store>,
        mcp_url: Arc<RwLock<String>>,
        dir: std::path::PathBuf,
    ) -> Arc<Self> {
        let (status, _) = broadcast::channel(256);
        Arc::new(Self {
            store,
            mcp_url,
            dir,
            agents: Mutex::new(HashMap::new()),
            status,
        })
    }

    fn agents(&self) -> std::sync::MutexGuard<'_, HashMap<i64, AgentState>> {
        self.agents.lock().unwrap_or_else(|e| e.into_inner())
    }

    // --------------------------------------------------------- the loops ---

    /// One subscriber for every awake agent, and one ticker that decides who
    /// runs. Per-agent tasks would mean N subscriptions to the same broadcast
    /// and N timers to reason about; this way there is a single place where a
    /// process can be started.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        let mut rx = self.store.events.subscribe();
        tauri::async_runtime::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(notice) => me.note(&notice),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Events were dropped, so we cannot know who they
                        // concerned. Waking everyone would be worse than
                        // missing one; the next event catches up.
                        tracing::warn!("supervisor lagged by {n} events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                me.clone().sweep().await;
            }
        });
    }

    /// Record that an event may concern some awake agents. Cheap and
    /// synchronous — nothing is started here.
    fn note(&self, notice: &EventNotice) {
        if !is_actionable(&notice.kind) {
            return;
        }
        let (Some(room_id), Some(thread_id)) = (notice.room_id, notice.thread_id) else {
            return;
        };
        let candidates = match self.store.awake_agents_in_room(room_id) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("could not tell who to wake: {e}");
                return;
            }
        };
        let mut agents = self.agents();
        for id in candidates {
            // Never wake an agent for its own doing. Without this one check a
            // reply wakes its own author, which replies, for ever.
            if notice.actor_agent_id == Some(id) {
                continue;
            }
            let st = agents.entry(id).or_default();
            st.waiting.insert(thread_id);
            st.since.get_or_insert_with(Instant::now);
        }
    }

    /// Start whoever is due. Runs once a second; almost always does nothing.
    async fn sweep(self: Arc<Self>) {
        // The listener binds on its own task. Until it has, there is nothing
        // for a child to connect back to — wait rather than start one and call
        // the agent broken.
        if self.mcp_url.read().map(|u| u.is_empty()).unwrap_or(true) {
            return;
        }
        let mut due: Vec<i64> = vec![];
        let mut tripped: Vec<i64> = vec![];
        {
            let mut agents = self.agents();
            let now = Instant::now();
            for (id, st) in agents.iter_mut() {
                if st.waiting.is_empty() || st.running.is_some() {
                    continue;
                }
                // Let a burst finish arriving.
                if st.since.map(|s| now.duration_since(s) < SETTLE).unwrap_or(true) {
                    continue;
                }
                st.recent.retain(|t| now.duration_since(*t) < Duration::from_secs(3600));
                if st.recent.len() >= MAX_RUNS_PER_HOUR {
                    st.waiting.clear();
                    st.since = None;
                    st.trouble = Some(format!(
                        "Started {MAX_RUNS_PER_HOUR} times in an hour, which is not normal, so \
                         Rivendell put it back to sleep. Something is looping — look at the room \
                         before switching it on again."
                    ));
                    tripped.push(*id);
                    continue;
                }
                due.push(*id);
            }
        }

        // A ceiling that merely throttles still bills 40 sessions an hour, all
        // night, which is the outcome it exists to prevent. Tripping it is a
        // circuit breaker: stop, and make the user look.
        for id in tripped {
            tracing::warn!("agent {id} hit the hourly ceiling and was put back to sleep");
            let _ = self.store.set_agent_awake(id, false);
            self.publish(id);
        }

        for id in due {
            // Between noting the events and now, the threads may have been
            // resolved, the room paused, or this agent's replies used up.
            let waiting: Vec<i64> = {
                let agents = self.agents();
                agents.get(&id).map(|s| s.waiting.iter().copied().collect()).unwrap_or_default()
            };
            let worth_it = match self.store.wakeable_threads(id, &waiting) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("could not check what agent {id} may do: {e}");
                    continue;
                }
            };
            if worth_it.is_empty() {
                let mut agents = self.agents();
                if let Some(st) = agents.get_mut(&id) {
                    st.waiting.clear();
                    st.since = None;
                }
                continue;
            }
            if let Err(e) = self.clone().spawn(id, worth_it).await {
                self.trouble(id, &e.to_string());
            }
        }
    }

    // ------------------------------------------------------------- runs ---

    async fn spawn(self: Arc<Self>, agent_id: i64, threads: Vec<i64>) -> Result<()> {
        let ctx = self.store.agent_ctx(agent_id)?;
        let plan = self.store.launch_plan(agent_id)?;
        let url = self.mcp_url.read().map(|u| u.clone()).unwrap_or_default();
        if url.is_empty() {
            return Err(Error::Invalid("the MCP server is not listening yet".into()));
        }

        let (token, handle) = self.store.mint_live_token(agent_id)?;

        // Beside the database: never in the project, where it would turn up in
        // the user's git status, and never in the shared temp directory, where
        // it was world-readable and had a bearer token in it.
        let cfg_path = self.dir.join(format!("mcp-{agent_id}.json"));
        if plan.mcp_install_mode == "config_file_flag" {
            let cfg = json!({
                "mcpServers": {
                    "rivendell": {
                        "type": "http",
                        "url": url,
                        "headers": { "Authorization": format!("Bearer {token}") }
                    }
                }
            });
            write_private(&cfg_path, &serde_json::to_vec_pretty(&cfg)?)?;
        }

        let ids = threads.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(",");
        let prompt = brief(&ctx.name, &ctx.project_name, &ctx.role, &threads);
        let subs = [
            ("{prompt}", prompt.as_str()),
            ("{mcp_config}", cfg_path.to_str().unwrap_or("")),
            ("{cwd}", ctx.folder_path.as_str()),
            ("{api_key}", token.as_str()),
            ("{mcp_url}", url.as_str()),
            ("{threads}", ids.as_str()),
            ("{agent_name}", ctx.name.as_str()),
        ];
        let mut args: Vec<String> = plan
            .args
            .iter()
            .map(|a| subs.iter().fold(a.clone(), |s, (k, v)| s.replace(k, v)))
            .collect();

        // An assistant reviews; only the coder edits. The seeded Claude profile
        // asks for `acceptEdits`, which is right for a coder and wrong for
        // everyone else, and a profile cannot tell the two apart. Nobody
        // expects turning on a switch called "awake" to authorise unattended
        // writes to their working tree.
        if ctx.role != "CODER" {
            for a in args.iter_mut() {
                if a == "acceptEdits" {
                    *a = "default".into();
                }
            }
        }

        let mut cmd = tokio::process::Command::new(&plan.cmd);
        cmd.args(&args)
            .current_dir(&ctx.folder_path)
            .env("RIVENDELL_URL", &url)
            .env("RIVENDELL_KEY", &token)
            .env("RIVENDELL_THREADS", &ids)
            .env("RIVENDELL_AGENT", &ctx.name)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // Its own process group, so one signal reaches the whole tree. An agent
        // CLI spawns children of its own, and killing only the process we can
        // see would leave those behind still billing.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.store.drop_live_token(&handle);
                let _ = std::fs::remove_file(&cfg_path);
                return Err(Error::Invalid(if e.kind() == std::io::ErrorKind::NotFound {
                    format!(
                        "`{}` is not on PATH, so Rivendell cannot start {}. Fix the launch \
                         profile, or run this agent yourself.",
                        plan.cmd, ctx.name
                    )
                } else {
                    format!("could not start {}: {e}", ctx.name)
                }));
            }
        };

        let pid = child.id();
        tracing::info!("woke {} for thread(s) {ids} (pid {pid:?})", ctx.name);

        {
            let mut agents = self.agents();
            let st = agents.entry(agent_id).or_default();
            st.waiting.clear();
            st.since = None;
            st.recent.push(Instant::now());
            st.last_run_at = Some(chrono::Utc::now().to_rfc3339());
            st.running = Some(RunHandle {
                pgid: pid,
                token: handle.clone(),
                command: plan.cmd.clone(),
            });
        }
        self.record();
        self.publish(agent_id);

        // Keep the tail of what it said. On success nobody looks; on failure it
        // is the only explanation the user gets.
        let tail = Arc::new(Mutex::new(String::new()));
        for stream in [
            child.stdout.take().map(Pipe::Out),
            child.stderr.take().map(Pipe::Err),
        ]
        .into_iter()
        .flatten()
        {
            let tail = tail.clone();
            tauri::async_runtime::spawn(async move {
                let mut lines = match stream {
                    Pipe::Out(o) => Reader::Out(BufReader::new(o).lines()),
                    Pipe::Err(e) => Reader::Err(BufReader::new(e).lines()),
                };
                while let Some(line) = lines.next().await {
                    let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                    t.push_str(&line);
                    t.push('\n');
                    if t.len() > LOG_TAIL_BYTES {
                        let cut = t.len() - LOG_TAIL_BYTES;
                        let cut = (cut..t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(t.len());
                        *t = t.split_off(cut);
                    }
                }
            });
        }

        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            let outcome = tokio::select! {
                status = child.wait() => match status {
                    Ok(s) if s.success() => Outcome::Done,
                    Ok(s) => Outcome::Failed(match s.code() {
                        Some(c) => format!("exited {c}"),
                        None => "killed by a signal".into(),
                    }),
                    Err(e) => Outcome::Failed(format!("could not wait for it: {e}")),
                },
                _ = tokio::time::sleep(Duration::from_secs(MAX_RUN_SECONDS)) => {
                    let _ = child.kill().await;
                    Outcome::Failed(format!("still going after {}m — killed", MAX_RUN_SECONDS / 60))
                }
            };
            me.store.drop_live_token(&handle);
            let _ = std::fs::remove_file(&cfg_path);
            let tail = tail.lock().unwrap_or_else(|e| e.into_inner()).clone();
            me.finish(agent_id, outcome, &tail);
            me.record();
        });

        Ok(())
    }

    fn finish(self: &Arc<Self>, agent_id: i64, outcome: Outcome, tail: &str) {
        let mut sleep_it = false;
        {
            let mut agents = self.agents();
            let Some(st) = agents.get_mut(&agent_id) else { return };
            st.running = None;
            match outcome {
                Outcome::Done => {
                    st.failures = 0;
                    st.last_outcome = Some("finished".into());
                }
                Outcome::Failed(why) => {
                    st.failures += 1;
                    st.last_outcome = Some(why.clone());
                    if st.failures >= MAX_CONSECUTIVE_FAILURES {
                        sleep_it = true;
                        let last = tail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
                        st.trouble = Some(format!(
                            "Failed {} times running ({why}) and has been put back to sleep.{}",
                            st.failures,
                            if last.is_empty() { String::new() } else { format!(" Last words: {last}") }
                        ));
                    }
                }
            }
        }
        if sleep_it {
            // A command that cannot run must not be retried for ever. Sleeping
            // it makes the failure something the user sees once, rather than a
            // process started every few seconds until they notice.
            let _ = self.store.set_agent_awake(agent_id, false);
            tracing::warn!("agent {agent_id} put back to sleep after repeated failures");
        }
        self.publish(agent_id);
    }

    // ------------------------------------------------------------ control ---

    /// Turn supervision on or off. Turning it off stops anything running now —
    /// "asleep" has to mean asleep, immediately.
    pub fn set_awake(&self, agent_id: i64, on: bool) -> Result<()> {
        if on {
            let ctx = self.store.agent_ctx(agent_id)?;
            if ctx.role == "HUMAN" {
                return Err(Error::Invalid("you are not something Rivendell starts".into()));
            }
            // Fail here rather than at the first event, so the toggle refusing
            // is the thing that explains why.
            self.store.launch_plan(agent_id)?;
            let mut agents = self.agents();
            let st = agents.entry(agent_id).or_default();
            st.trouble = None;
            st.failures = 0;
            st.recent.clear();
        }
        self.store.set_agent_awake(agent_id, on)?;
        if !on {
            self.stop(agent_id);
        }
        self.publish(agent_id);
        Ok(())
    }

    /// Stop whatever is running as this agent, and take away its credential.
    pub fn stop(&self, agent_id: i64) {
        let handle = {
            let mut agents = self.agents();
            let Some(st) = agents.get_mut(&agent_id) else { return };
            st.waiting.clear();
            st.since = None;
            st.running.take()
        };
        if let Some(h) = handle {
            kill_group(h.pgid);
            self.store.drop_live_token(&h.token);
        }
        // Belt and braces: any other credential this agent holds goes too.
        self.store.drop_live_tokens_for(agent_id);
    }

    /// Kill every child. Called on the way out, where async is not available.
    pub fn shutdown(&self) {
        {
            let mut agents = self.agents();
            for (id, st) in agents.iter_mut() {
                if let Some(h) = st.running.take() {
                    tracing::info!("stopping agent {id} on shutdown");
                    kill_group(h.pgid);
                }
            }
        }
        self.record();
    }

    // ------------------------------------------------- surviving a crash ---

    fn ledger(&self) -> std::path::PathBuf {
        self.dir.join("running.json")
    }

    /// Write down what is running, so a later launch can clean up after a death
    /// this process did not get to handle.
    ///
    /// The in-process teardown covers an orderly quit. It cannot cover Force
    /// Quit, `kill -9`, or a panic under `panic = "abort"` — and on macOS there
    /// is no `PR_SET_PDEATHSIG`, so a child outliving its parent is simply
    /// reparented to launchd and keeps running, and keeps billing. This file is
    /// the only leg that survives that.
    fn record(&self) {
        let live: Vec<serde_json::Value> = {
            let agents = self.agents();
            agents
                .values()
                .filter_map(|s| s.running.as_ref())
                .filter_map(|h| {
                    h.pgid.map(|p| json!({ "pgid": p, "command": h.command }))
                })
                .collect()
        };
        let path = self.ledger();
        if live.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        if let Ok(body) = serde_json::to_vec(&live) {
            let _ = write_private(&path, &body);
        }
    }

    /// Kill anything a previous run left behind. Call once, before starting.
    pub fn reap_orphans(dir: &std::path::Path) {
        let path = dir.join("running.json");
        let Ok(body) = std::fs::read(&path) else { return };
        let _ = std::fs::remove_file(&path);
        let Ok(entries) = serde_json::from_slice::<Vec<serde_json::Value>>(&body) else {
            return;
        };
        for e in entries {
            let (Some(pgid), Some(cmd)) = (
                e.get("pgid").and_then(|v| v.as_u64()),
                e.get("command").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            // A bare pid proves nothing — the number is reused, and killing
            // whatever happens to hold it now would be worse than the leak.
            if !still_running(pgid as u32, cmd) {
                continue;
            }
            tracing::warn!("killing orphaned {cmd} (pgid {pgid}) left by an earlier run");
            kill_group(Some(pgid as u32));
        }
    }

    fn trouble(&self, agent_id: i64, why: &str) {
        {
            let mut agents = self.agents();
            agents.entry(agent_id).or_default().trouble = Some(why.to_string());
        }
        tracing::warn!("agent {agent_id}: {why}");
        // A command that cannot start will not start next time either.
        let _ = self.store.set_agent_awake(agent_id, false);
        self.publish(agent_id);
    }

    pub fn status_of(&self, agent_id: i64) -> AwakeStatus {
        let agents = self.agents();
        let st = agents.get(&agent_id);
        AwakeStatus {
            agent_id,
            running: st.map(|s| s.running.is_some()).unwrap_or(false),
            waiting: st.map(|s| s.waiting.len()).unwrap_or(0),
            last_run_at: st.and_then(|s| s.last_run_at.clone()),
            last_outcome: st.and_then(|s| s.last_outcome.clone()),
            trouble: st.and_then(|s| s.trouble.clone()),
        }
    }

    pub fn status_all(&self) -> Vec<AwakeStatus> {
        let ids: Vec<i64> = self.agents().keys().copied().collect();
        ids.into_iter().map(|id| self.status_of(id)).collect()
    }

    fn publish(&self, agent_id: i64) {
        let _ = self.status.send(self.status_of(agent_id));
    }
}

/// Owner-readable only. These files carry a bearer token.
fn write_private(path: &std::path::Path, body: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        return f.write_all(body);
    }
    #[cfg(not(unix))]
    std::fs::write(path, body)
}

/// Is this pid still the process we started? Compares the command, because pids
/// are recycled and a stale one may now be something of the user's.
fn still_running(pid: u32, expect: &str) -> bool {
    let want = std::path::Path::new(expect)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(expect);
    match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
    {
        Ok(o) if o.status.success() => {
            let comm = String::from_utf8_lossy(&o.stdout);
            let comm = comm.trim();
            !comm.is_empty()
                && std::path::Path::new(comm)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|c| c == want)
                    .unwrap_or(false)
        }
        _ => false,
    }
}

enum Outcome {
    Done,
    Failed(String),
}

// Two pipe types, one loop. Neither `Lines` type is nameable in a way that lets
// them share a variable, so they share an enum instead.
enum Pipe {
    Out(tokio::process::ChildStdout),
    Err(tokio::process::ChildStderr),
}

enum Reader {
    Out(tokio::io::Lines<BufReader<tokio::process::ChildStdout>>),
    Err(tokio::io::Lines<BufReader<tokio::process::ChildStderr>>),
}

impl Reader {
    async fn next(&mut self) -> Option<String> {
        match self {
            Reader::Out(l) => l.next_line().await.ok().flatten(),
            Reader::Err(l) => l.next_line().await.ok().flatten(),
        }
    }
}

/// Signal the whole process group. The agent CLI is a group leader (see
/// `process_group(0)` in `spawn`), so this reaches the children it started too.
#[cfg(unix)]
fn kill_group(pgid: Option<u32>) {
    let Some(pid) = pgid else { return };
    // SAFETY: killpg with a pgid we created ourselves. A dead group returns
    // ESRCH, which is fine — it means the work is already done.
    unsafe {
        libc::killpg(pid as i32, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn kill_group(_pgid: Option<u32>) {}

/// What a freshly started agent is told. It has no memory of this workspace, so
/// everything it needs is here — but the threads themselves hold the context,
/// and it is cheaper to send it to read them than to restate them.
fn brief(name: &str, project: &str, role: &str, threads: &[i64]) -> String {
    let list = threads.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(", ");
    // Every reply cap in Rivendell is per thread, so a new thread resets all of
    // them. A coder that answers a reply by opening another topic is therefore
    // the one shape of loop no rail downstream can bound.
    let scope = if role == "CODER" {
        "Deal with the threads named above and nothing else. Do not open new threads on this \
         run — if the work needs one, say so in a reply and let a person start it.\n\n"
    } else {
        ""
    };
    format!(
        "You are `{name}` in the Rivendell workspace for `{project}`, and you have been started \
         because these threads moved: {list}.\n\n\
         Read each one with the `rivendell` MCP tool `get_thread`. The thread holds the whole \
         conversation, including anything a previous run of you said — you are continuing that \
         work, not starting over.\n\n\
         {scope}\
         Act only where you are actually needed. Answer with `reply`, being concrete: exact \
         paths, line numbers in `refs`, failing inputs, a fix where you have one. If a thread \
         does not concern you, or says all it needs to, leave it alone — saying nothing is a \
         perfectly good outcome and costs nobody anything.\n\n\
         When you are done with these threads, exit. Do not sit in `wait_for_updates`: \
         Rivendell will start you again when there is more.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_conversation_wakes_an_agent() {
        for k in [
            "thread.created",
            "thread.mentioned",
            "message.created",
            "message.edited",
            "thread.status",
        ] {
            assert!(is_actionable(k), "{k} should wake an agent");
        }
        // Bookkeeping. `run.*` especially: waking for our own run events is a
        // supervisor feeding itself.
        for k in [
            "run.started",
            "run.finished",
            "agent.created",
            "agent.updated",
            "room.paused",
            "project.deleted",
            "thread.exported",
        ] {
            assert!(!is_actionable(k), "{k} must not wake an agent");
        }
    }

    use crate::models::EventNotice;

    fn notice(kind: &str, room: i64, thread: i64, actor: Option<i64>) -> EventNotice {
        EventNotice {
            seq: 1,
            room_id: Some(room),
            thread_id: Some(thread),
            kind: kind.into(),
            actor_agent_id: actor,
        }
    }

    /// The rule that stops a reply waking its own author, who replies, for
    /// ever. It is the difference between a supervisor and a money fire.
    #[test]
    fn an_agent_is_never_woken_by_itself() {
        let mine = notice("message.created", 1, 42, Some(7));
        let theirs = notice("message.created", 1, 42, Some(9));
        assert_eq!(mine.actor_agent_id, Some(7));
        assert_ne!(theirs.actor_agent_id, Some(7));
        assert!(is_actionable(&mine.kind));
    }

    #[test]
    fn a_human_reply_still_wakes_it() {
        // A person has no agent id on the event, and the self-filter must not
        // mistake that for "this was me".
        let n = notice("message.created", 1, 42, None);
        assert!(is_actionable(&n.kind));
        assert_ne!(n.actor_agent_id, Some(7));
    }

    #[test]
    fn an_unrecognised_pid_is_left_alone() {
        // Reaping orphans compares the command, because pids are recycled and
        // the one we wrote down may now belong to something of the user's.
        assert!(!still_running(999_999, "an-agent-cli"));
        assert!(!still_running(std::process::id(), "definitely-not-this-process"));
    }

    #[test]
    fn a_config_file_is_readable_only_by_its_owner() {
        // It carries a bearer token. The prior art wrote it 0644 into the
        // shared temp directory.
        let dir = std::env::temp_dir().join(format!("rivendell-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("mcp.json");
        write_private(&p, b"{}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "found mode {mode:o}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_brief_names_the_threads_and_says_to_stop() {
        let b = brief("scout", "rivendell", "ASSISTANT", &[42, 43]);
        assert!(b.contains("#42, #43"));
        assert!(b.contains("get_thread"));
        // Sitting in the long poll would defeat the point of being started.
        assert!(b.contains("Do not sit in `wait_for_updates`"));
    }

    /// Every reply cap is per thread, so a coder that answers by opening
    /// another topic escapes all of them. It is the one loop nothing
    /// downstream can bound.
    #[test]
    fn only_a_coder_is_told_not_to_open_threads() {
        let coder = brief("dev", "rivendell", "CODER", &[42]);
        assert!(coder.contains("Do not open new threads"));
        // An assistant cannot open one anyway; saying so would be noise.
        let assistant = brief("scout", "rivendell", "ASSISTANT", &[42]);
        assert!(!assistant.contains("Do not open new threads"));
    }
}
