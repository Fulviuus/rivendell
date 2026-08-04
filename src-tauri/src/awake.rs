//! Keeps a watcher running for every agent that should be awake.
//!
//! An agent is a row in a table, not a process. Whatever is holding its key is
//! that agent for as long as it runs, and when it ends its turn there is
//! nothing left to talk to — no MCP notification can reach a model that is not
//! being asked for tokens. So an awake agent is not one we keep alive. It is
//! one that gets started again, from nothing, each time its rooms need it. The
//! thread history is the context, so a fresh process picks up exactly where the
//! last one stopped.
//!
//! The deciding — which events matter, whose they were, when to start something
//! — is deliberately *not* here. It lives in `runner/`, outside the app, and
//! this module only keeps one of those running per awake agent and cleans up
//! after it. Rivendell already deleted an in-process spawner once, on the
//! grounds that a process supervisor keyed on thread state was more complexity
//! than the event log needed, and that verdict still holds. What is left here
//! is process lifetime and credentials: the two things only the app can do.
//!
//! Everything below is written on the assumption that the worst outcome is not
//! a missed wake-up but a process nobody meant to be running.

use crate::error::{Error, Result};
use crate::store::Store;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;

/// How long a watcher holds each poll open. Long, because it costs nothing and
/// returns the instant anything happens.
const POLL_SECONDS: u64 = 900;
/// A single agent run still going after this is stuck, not thinking.
const RUN_LIMIT_SECONDS: u64 = 20 * 60;
/// Passed through to the watcher. Nothing legitimate starts an agent this often.
const CEILING_PER_HOUR: usize = 40;
/// A watcher that dies faster than this did not do any work.
const TOO_SOON: Duration = Duration::from_secs(20);
/// That many pointless restarts running and it is broken, not unlucky.
const MAX_QUICK_EXITS: u32 = 3;
/// The watcher's own exit code for "something is looping, I stopped".
const EXIT_CEILING: i32 = 3;
/// Keep the tail of what it said, for when it fails.
const LOG_TAIL_BYTES: usize = 4096;

/// What the UI shows next to the toggle.
#[derive(Debug, Clone, Serialize)]
pub struct AwakeStatus {
    pub agent_id: i64,
    /// A watcher is up and holding the poll.
    pub watching: bool,
    /// The agent itself is running right now.
    pub running: bool,
    pub threads: Vec<i64>,
    pub last_run_at: Option<String>,
    /// Set when something needs the user's attention, and never cleared
    /// silently — a broken command has to be visible or the agent just looks
    /// lazy.
    pub trouble: Option<String>,
}

#[derive(Default)]
struct AgentState {
    watcher: Option<Watcher>,
    running: bool,
    threads: Vec<i64>,
    last_run_at: Option<String>,
    /// Consecutive restarts that died before doing anything.
    quick_exits: u32,
    trouble: Option<String>,
}

struct Watcher {
    /// Also the process-group id — see `spawn`. The watcher leads the group,
    /// and the agent it starts inherits it, so one signal reaches both.
    pgid: Option<u32>,
    /// Handle for the ephemeral credential, so it dies with the process.
    token: String,
    /// Checked before killing anything at startup: a pid on its own is not
    /// proof of identity, because the number gets reused.
    command: String,
    /// So a run that generation-mismatches on exit cannot clobber a newer one.
    generation: u64,
}

pub struct Supervisor {
    store: Arc<Store>,
    mcp_url: Arc<RwLock<String>>,
    /// Beside the database. Holds the MCP config handed to agents, and the
    /// record of what is running.
    dir: std::path::PathBuf,
    agents: Mutex<HashMap<i64, AgentState>>,
    generation: std::sync::atomic::AtomicU64,
    /// Restart requests. A watcher that dies asks for a replacement through
    /// here rather than starting one itself: two async functions calling each
    /// other makes the future type recursive, and it puts every start in one
    /// place, which is where you want it when the thing being started costs
    /// money.
    wake: tokio::sync::mpsc::UnboundedSender<i64>,
    inbox: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<i64>>>,
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
        let (wake, inbox) = tokio::sync::mpsc::unbounded_channel();
        Arc::new(Self {
            store,
            mcp_url,
            dir,
            agents: Mutex::new(HashMap::new()),
            generation: std::sync::atomic::AtomicU64::new(0),
            wake,
            inbox: Mutex::new(Some(inbox)),
            status,
        })
    }

    fn agents(&self) -> std::sync::MutexGuard<'_, HashMap<i64, AgentState>> {
        self.agents.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Bring up a watcher for everything already marked awake, once the MCP
    /// server is listening. Nothing can start before then — there would be
    /// nothing for it to connect back to.
    pub fn start(self: &Arc<Self>) {
        let Some(mut inbox) = self.inbox.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return; // already started
        };
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            // Nothing can start before the listener binds — there would be
            // nothing for it to connect back to.
            loop {
                let ready = me.mcp_url.read().map(|u| !u.is_empty()).unwrap_or(false);
                if ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            match me.store.awake_agent_ids() {
                Ok(ids) => {
                    for id in ids {
                        let _ = me.wake.send(id);
                    }
                }
                Err(e) => tracing::warn!("could not read who should be awake: {e}"),
            }
            while let Some(id) = inbox.recv().await {
                if let Err(e) = me.clone().spawn(id).await {
                    me.trouble(id, &e.to_string());
                }
            }
        });
    }

    // ----------------------------------------------------------- watchers ---

    /// Start one watcher for `agent_id`. It holds the long poll and starts the
    /// agent itself when its rooms have work.
    async fn spawn(self: Arc<Self>, agent_id: i64) -> Result<()> {
        if self.agents().get(&agent_id).and_then(|s| s.watcher.as_ref()).is_some() {
            return Ok(()); // already watching
        }
        let ctx = self.store.agent_ctx(agent_id)?;
        let plan = self.store.launch_plan(agent_id)?;
        let watcher = watcher_binary()?;
        let url = self.mcp_url.read().map(|u| u.clone()).unwrap_or_default();
        if url.is_empty() {
            return Err(Error::Invalid("the MCP server is not listening yet".into()));
        }

        let (token, handle) = self.store.mint_live_token(agent_id)?;

        // Beside the database: never in the project, where it would turn up in
        // the user's git status, and never in the shared temp directory, where
        // it would be world-readable with a bearer token in it.
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

        // The watcher fills {prompt} and {threads} in at the moment it starts
        // the agent — it is the one that knows which threads moved. Everything
        // else is knowable now.
        let mut args: Vec<String> = plan
            .args
            .iter()
            .map(|a| {
                a.replace("{mcp_config}", cfg_path.to_str().unwrap_or(""))
                    .replace("{cwd}", &ctx.folder_path)
                    .replace("{api_key}", &token)
                    .replace("{mcp_url}", &url)
                    .replace("{agent_name}", &ctx.name)
            })
            .collect();

        // Nobody expects turning on a switch called "awake" to authorise
        // unattended writes to their working tree. The seeded Claude profile
        // asks for `acceptEdits`; a person can hand that out deliberately, but
        // it is not something an agent should inherit by being started.
        if !ctx.is_human() {
            for a in args.iter_mut() {
                if a == "acceptEdits" {
                    *a = "default".into();
                }
            }
        }

        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;

        let mut cmd = tokio::process::Command::new(&watcher);
        cmd.arg("--url")
            .arg(&url)
            .arg("--wait")
            .arg(POLL_SECONDS.to_string())
            .arg("--limit")
            .arg(RUN_LIMIT_SECONDS.to_string())
            .arg("--ceiling")
            .arg(CEILING_PER_HOUR.to_string())
            .arg("--report")
            .arg("--")
            .arg(&plan.cmd)
            .args(&args)
            .current_dir(&ctx.folder_path)
            .env("PATH", login_path())
            // Never on the command line, which is world-readable via `ps`.
            .env("RIVENDELL_KEY", &token)
            .env("RIVENDELL_URL", &url)
            .env("RIVENDELL_AGENT", &ctx.name)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // Its own process group, so one signal reaches the whole tree. The
        // agent CLI is started by the watcher and spawns children of its own;
        // killing only the process we can see would leave those behind, still
        // billing.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.store.drop_live_token(&handle);
                let _ = std::fs::remove_file(&cfg_path);
                return Err(Error::Invalid(if e.kind() == std::io::ErrorKind::NotFound {
                    format!(
                        "the watcher is missing from this build ({}). Run \
                         `cargo build --release --manifest-path runner/Cargo.toml`.",
                        watcher.display()
                    )
                } else {
                    format!("could not start a watcher for {}: {e}", ctx.name)
                }));
            }
        };

        let pid = child.id();
        tracing::info!("watching for {} (pid {pid:?})", ctx.name);
        {
            let mut agents = self.agents();
            let st = agents.entry(agent_id).or_default();
            st.trouble = None;
            st.watcher = Some(Watcher {
                pgid: pid,
                token: handle.clone(),
                command: watcher.to_string_lossy().into_owned(),
                generation,
            });
        }
        self.record();
        self.publish(agent_id);

        // Its stdout is a state feed; its stderr is what to show when it fails.
        if let Some(out) = child.stdout.take() {
            let me = self.clone();
            tauri::async_runtime::spawn(async move {
                let mut lines = BufReader::new(out).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    me.observe(agent_id, &l);
                }
            });
        }
        let tail = Arc::new(Mutex::new(String::new()));
        if let Some(err) = child.stderr.take() {
            let tail = tail.clone();
            let name = ctx.name.clone();
            tauri::async_runtime::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    tracing::info!("{name}: {l}");
                    let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                    t.push_str(&l);
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
        let started = Instant::now();
        tauri::async_runtime::spawn(async move {
            let code = match child.wait().await {
                Ok(s) => s.code(),
                Err(e) => {
                    tracing::warn!("lost track of the watcher for agent {agent_id}: {e}");
                    None
                }
            };
            me.store.drop_live_token(&handle);
            let tail = tail.lock().unwrap_or_else(|e| e.into_inner()).clone();
            me.watcher_ended(agent_id, generation, code, started.elapsed(), &tail)
                .await;
        });

        Ok(())
    }

    /// One line of the watcher's state feed.
    fn observe(&self, agent_id: i64, line: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            tracing::info!("agent {agent_id} watcher said: {line}");
            return;
        };
        tracing::info!("agent {agent_id}: {}", v["state"].as_str().unwrap_or("?"));
        let mut agents = self.agents();
        let Some(st) = agents.get_mut(&agent_id) else { return };
        match v["state"].as_str() {
            Some("running") => {
                st.running = true;
                st.threads = v["threads"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|t| t.as_i64()).collect())
                    .unwrap_or_default();
                st.last_run_at = Some(chrono::Utc::now().to_rfc3339());
            }
            Some("waiting") => {
                st.running = false;
                st.threads.clear();
            }
            Some("ceiling") => {
                st.running = false;
                st.trouble = Some(format!(
                    "Started {CEILING_PER_HOUR} times in an hour, which is not normal, so \
                     Rivendell stopped it. Something is looping — look at the room before \
                     switching it back on."
                ));
            }
            _ => {}
        }
        drop(agents);
        self.publish(agent_id);
    }

    /// A watcher exited. Decide whether that was us stopping it, a loop it
    /// caught itself, or a crash worth restarting.
    async fn watcher_ended(
        self: &Arc<Self>,
        agent_id: i64,
        generation: u64,
        code: Option<i32>,
        lived: Duration,
        tail: &str,
    ) {
        {
            let mut agents = self.agents();
            let Some(st) = agents.get_mut(&agent_id) else { return };
            // A newer watcher has taken over; this is an old corpse.
            match &st.watcher {
                Some(w) if w.generation != generation => return,
                None => return, // we stopped it on purpose
                _ => {}
            }
            st.watcher = None;
            st.running = false;
            st.threads.clear();
        }
        self.record();

        // It caught its own runaway and said so. Not something to restart.
        if code == Some(EXIT_CEILING) {
            let _ = self.store.set_agent_awake(agent_id, false);
            self.publish(agent_id);
            return;
        }

        // Still meant to be awake? Then this was a crash, and the usual cause
        // is the app's own MCP server going away — worth getting back up.
        let still_awake = self
            .store
            .awake_agent_ids()
            .map(|ids| ids.contains(&agent_id))
            .unwrap_or(false);
        if !still_awake {
            self.publish(agent_id);
            return;
        }

        let give_up = {
            let mut agents = self.agents();
            let st = agents.entry(agent_id).or_default();
            if lived < TOO_SOON {
                st.quick_exits += 1;
            } else {
                st.quick_exits = 0;
            }
            st.quick_exits >= MAX_QUICK_EXITS
        };

        if give_up {
            // Restarting something that dies immediately, for ever, is how a
            // supervisor turns one broken command into a busy loop.
            let last = tail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
            let why = format!(
                "Its watcher stopped {MAX_QUICK_EXITS} times without getting going, so \
                 Rivendell gave up.{}",
                if last.is_empty() { String::new() } else { format!(" Last words: {last}") }
            );
            self.trouble(agent_id, &why);
            return;
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = self.wake.send(agent_id);
    }

    // ------------------------------------------------------------ control ---

    /// Turn supervision on or off. Turning it off stops the watcher and
    /// anything it started — "asleep" has to mean asleep, immediately.
    pub async fn set_awake(self: &Arc<Self>, agent_id: i64, on: bool) -> Result<()> {
        if on {
            let ctx = self.store.agent_ctx(agent_id)?;
            if ctx.is_human() {
                return Err(Error::Invalid("you are not something Rivendell starts".into()));
            }
            // Fail here rather than at the first event, so the toggle refusing
            // is the thing that explains why.
            self.store.launch_plan(agent_id)?;
            watcher_binary()?;
            {
                let mut agents = self.agents();
                let st = agents.entry(agent_id).or_default();
                st.trouble = None;
                st.quick_exits = 0;
            }
            self.store.set_agent_awake(agent_id, true)?;
            self.clone().spawn(agent_id).await?;
        } else {
            self.store.set_agent_awake(agent_id, false)?;
            self.stop(agent_id);
        }
        self.publish(agent_id);
        Ok(())
    }

    /// Stop the watcher for this agent, and take away its credential.
    pub fn stop(&self, agent_id: i64) {
        let w = {
            let mut agents = self.agents();
            let Some(st) = agents.get_mut(&agent_id) else { return };
            st.running = false;
            st.threads.clear();
            st.watcher.take()
        };
        if let Some(w) = w {
            kill_group(w.pgid);
            self.store.drop_live_token(&w.token);
        }
        // Belt and braces: any other credential this agent holds goes too.
        self.store.drop_live_tokens_for(agent_id);
        self.record();
    }

    /// Kill every watcher. Called on the way out, where async is not available.
    pub fn shutdown(&self) {
        {
            let mut agents = self.agents();
            for (id, st) in agents.iter_mut() {
                if let Some(w) = st.watcher.take() {
                    tracing::info!("stopping the watcher for agent {id}");
                    kill_group(w.pgid);
                }
            }
        }
        self.record();
    }

    fn trouble(&self, agent_id: i64, why: &str) {
        {
            let mut agents = self.agents();
            agents.entry(agent_id).or_default().trouble = Some(why.to_string());
        }
        tracing::warn!("agent {agent_id}: {why}");
        let _ = self.store.set_agent_awake(agent_id, false);
        self.publish(agent_id);
    }

    pub fn status_of(&self, agent_id: i64) -> AwakeStatus {
        let agents = self.agents();
        let st = agents.get(&agent_id);
        AwakeStatus {
            agent_id,
            watching: st.map(|s| s.watcher.is_some()).unwrap_or(false),
            running: st.map(|s| s.running).unwrap_or(false),
            threads: st.map(|s| s.threads.clone()).unwrap_or_default(),
            last_run_at: st.and_then(|s| s.last_run_at.clone()),
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

    // ------------------------------------------------- surviving a crash ---

    fn ledger(&self) -> std::path::PathBuf {
        self.dir.join("running.json")
    }

    /// Write down what is running, so a later launch can clean up after a death
    /// this process did not get to handle.
    ///
    /// The teardown on quit covers an orderly exit. It cannot cover Force Quit,
    /// `kill -9`, or a panic under `panic = "abort"` — and on macOS there is no
    /// `PR_SET_PDEATHSIG`, so a child outliving its parent is simply reparented
    /// and keeps running, and keeps billing. This file is the only leg that
    /// survives that.
    fn record(&self) {
        let live: Vec<serde_json::Value> = {
            let agents = self.agents();
            agents
                .values()
                .filter_map(|s| s.watcher.as_ref())
                .filter_map(|w| w.pgid.map(|p| json!({ "pgid": p, "command": w.command })))
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
}

/// Where the watcher lives.
///
/// Bundled, tauri puts it beside the app's own binary. In a dev run it is
/// wherever cargo left it. Both are checked, and saying which were tried is the
/// difference between a fixable message and a mystery.
pub fn watcher_binary() -> Result<std::path::PathBuf> {
    let mut tried = vec![];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("rivendell-run");
            if p.is_file() {
                return Ok(p);
            }
            tried.push(p);
        }
    }
    for rel in [
        "runner/target/release/rivendell-run",
        "../runner/target/release/rivendell-run",
        "runner/target/debug/rivendell-run",
    ] {
        let p = std::path::PathBuf::from(rel);
        if p.is_file() {
            return Ok(std::fs::canonicalize(&p).unwrap_or(p));
        }
        tried.push(p);
    }
    Err(Error::NotFound(format!(
        "the watcher `rivendell-run` is not in this build. Build it with \
         `cargo build --release --manifest-path runner/Cargo.toml`. Looked in: {}",
        tried
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// The `PATH` the user's shell would have.
///
/// An app launched from Finder or `open` inherits almost nothing —
/// `/usr/bin:/bin:/usr/sbin:/sbin` and no more. Every agent CLI worth running
/// lives somewhere else: a version manager, `~/.local/bin`, Homebrew. Without
/// this, a launch profile that works perfectly in a terminal fails the moment
/// Rivendell is the one starting it, and the error — "not on PATH" — reads like
/// the agent is not installed.
///
/// Asked once, from the login shell, because that is the only thing that knows
/// what the user's own setup does.
fn login_path() -> String {
    static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let inherited = std::env::var("PATH").unwrap_or_default();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let asked = std::process::Command::new(&shell)
            .args(["-lc", "printf %s \"$PATH\""])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|p| !p.is_empty());

        // Union, inherited first: the shell's answer is the useful part, but
        // dropping what we already had would be a regression in the dev case,
        // where the app is started from a terminal that already had it right.
        let mut seen = std::collections::BTreeSet::new();
        let mut out: Vec<String> = vec![];
        for part in inherited
            .split(':')
            .chain(asked.iter().flat_map(|p| p.split(':')))
            .chain(["/opt/homebrew/bin", "/usr/local/bin"])
        {
            if !part.is_empty() && seen.insert(part.to_string()) {
                out.push(part.to_string());
            }
        }
        out.join(":")
    })
    .clone()
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

/// Signal the whole process group. The watcher is a group leader (see
/// `process_group(0)` in `spawn`) and the agent it starts inherits the group,
/// so this reaches both.
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The failure this prevents is invisible in a terminal and certain from
    /// Finder: an app launched by the OS gets a PATH with nothing useful in it,
    /// and every launch profile names a bare command.
    #[test]
    fn spawned_agents_get_a_usable_path() {
        let p = login_path();
        assert!(p.contains("/usr/bin"), "lost the basics: {p}");
        // Somewhere a CLI actually gets installed, beyond the OS defaults.
        assert!(
            ["/opt/homebrew/bin", "/usr/local/bin", ".local/bin", ".nvm", ".bun", ".cargo"]
                .iter()
                .any(|d| p.contains(d)),
            "nothing but system directories, so a bare command would not resolve: {p}"
        );
        // Union, not concatenation.
        let parts: Vec<&str> = p.split(':').collect();
        let mut uniq = parts.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(parts.len(), uniq.len(), "duplicate entries in {p}");
    }

    #[test]
    fn a_missing_watcher_says_where_it_looked() {
        // The failure a user is most likely to hit is a build without the
        // watcher in it, and "not found" alone would send them nowhere.
        if let Err(e) = watcher_binary() {
            let m = e.to_string();
            assert!(m.contains("cargo build"), "no way forward in: {m}");
            assert!(m.contains("Looked in:"), "no paths in: {m}");
        }
    }
}
