//! Launches assistant processes for a thread.
//!
//! A spawned run authenticates with an *ephemeral* token minted for that run
//! and revoked when the process exits — the agent's long-lived API key never
//! leaves the database in plaintext. The long-lived key is for sessions you
//! attach yourself, such as the coder.

use crate::error::{Error, Result};
use crate::store::Store;
use serde_json::json;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::RwLock;

pub const MAX_RUN_SECONDS: u64 = 20 * 60;

pub struct Spawner {
    store: Arc<Store>,
    mcp_url: RwLock<String>,
}

struct LaunchPlan {
    cmd: String,
    args: Vec<String>,
    mcp_install_mode: String,
    profile_key: String,
}

impl Spawner {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            mcp_url: RwLock::new(String::new()),
        }
    }

    pub async fn set_url(&self, url: String) {
        *self.mcp_url.write().await = url;
    }

    pub async fn url(&self) -> String {
        self.mcp_url.read().await.clone()
    }

    /// Spawns every eligible assistant for `thread_id`. Returns how many started.
    pub async fn dispatch(&self, thread_id: i64, only: Option<Vec<i64>>) -> Result<usize> {
        let detail = self.store.thread_detail(thread_id)?;
        if crate::models::is_terminal(&detail.summary.status) {
            return Ok(0);
        }
        let room_id = detail.summary.room_id;

        let room = self
            .store
            .list_rooms()?
            .into_iter()
            .find(|r| r.id == room_id)
            .ok_or_else(|| Error::NotFound(format!("room {room_id}")))?;
        if room.paused {
            tracing::info!("room #{} is paused; not dispatching", room.name);
            return Ok(0);
        }

        let mut targets = self.store.dispatch_targets(thread_id)?;
        if let Some(only) = only {
            targets.retain(|id| only.contains(id));
        }

        let mut started = 0usize;
        for agent_id in targets {
            let active = self.store.active_run_count(room_id)?;
            if active >= room.max_concurrent_runs {
                tracing::info!(
                    "room #{} at its concurrency limit ({active}); {agent_id} will not start",
                    room.name
                );
                break;
            }
            // Never stack two runs of the same agent on the same thread.
            if detail
                .runs
                .iter()
                .any(|r| r.agent_id == agent_id && r.status == "RUNNING")
            {
                continue;
            }
            match self.spawn_one(thread_id, agent_id).await {
                Ok(true) => started += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!("spawn failed for agent {agent_id}: {e}"),
            }
        }
        Ok(started)
    }

    /// `Ok(false)` means the agent is deliberately not spawnable (external).
    async fn spawn_one(&self, thread_id: i64, agent_id: i64) -> Result<bool> {
        let ctx = self.store.agent_ctx(agent_id)?;
        let plan = self.plan_for(agent_id)?;
        if plan.profile_key == "external" || plan.cmd.trim().is_empty() {
            return Ok(false);
        }

        let detail = self.store.thread_detail(thread_id)?;
        let tag = self
            .store
            .list_tags()?
            .into_iter()
            .find(|t| t.key == detail.summary.tag);
        let prompt = build_brief(&ctx.name, &ctx.room_name, &ctx.project_name, &detail, tag.as_ref());

        let url = self.url().await;
        if url.is_empty() {
            return Err(Error::Invalid("the MCP server is not listening yet".into()));
        }

        let run_id = self.store.start_run(thread_id, agent_id, &plan.cmd)?;
        let token = self.store.mint_run_token(agent_id, run_id);

        // Config file lives beside the DB, not in the project, so it never
        // shows up in the user's git status.
        let cfg_path = std::env::temp_dir().join(format!("rivendell-mcp-{run_id}.json"));
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
            std::fs::write(&cfg_path, serde_json::to_vec_pretty(&cfg)?)?;
        }

        let subs = [
            ("{prompt}", prompt.as_str()),
            ("{mcp_config}", cfg_path.to_str().unwrap_or("")),
            ("{cwd}", ctx.folder_path.as_str()),
            ("{api_key}", token.as_str()),
            ("{mcp_url}", url.as_str()),
            ("{thread_id}", &thread_id.to_string()),
            ("{agent_name}", ctx.name.as_str()),
        ];
        let args: Vec<String> = plan
            .args
            .iter()
            .map(|a| {
                let mut s = a.clone();
                for (k, v) in &subs {
                    s = s.replace(k, v);
                }
                s
            })
            .collect();

        let cmdline = format!("{} {}", plan.cmd, shell_preview(&args));
        self.store
            .append_run_log(run_id, &format!("$ {cmdline}\n\n"))?;

        let mut child = match tokio::process::Command::new(&plan.cmd)
            .args(&args)
            .current_dir(&ctx.folder_path)
            .env("RIVENDELL_URL", &url)
            .env("RIVENDELL_KEY", &token)
            .env("RIVENDELL_THREAD_ID", thread_id.to_string())
            .env("RIVENDELL_AGENT", &ctx.name)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let hint = if e.kind() == std::io::ErrorKind::NotFound {
                    format!(
                        "`{}` is not on PATH. Fix the command in this agent's profile, or set the \
                         agent to External and drive it yourself.",
                        plan.cmd
                    )
                } else {
                    e.to_string()
                };
                self.store.append_run_log(run_id, &format!("\n{hint}\n"))?;
                self.store.finish_run(run_id, "FAILED", None)?;
                self.store.revoke_run_token(run_id);
                let _ = std::fs::remove_file(&cfg_path);
                return Err(Error::Invalid(hint));
            }
        };

        self.store.set_run_pid(run_id, child.id().map(|p| p as i64))?;

        let store = self.store.clone();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        tokio::spawn(async move {
            if let Some(out) = stdout {
                let store = store.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(out).lines();
                    while let Ok(Some(l)) = lines.next_line().await {
                        let _ = store.append_run_log(run_id, &format!("{l}\n"));
                    }
                });
            }
            if let Some(err) = stderr {
                let store = store.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(err).lines();
                    while let Ok(Some(l)) = lines.next_line().await {
                        let _ = store.append_run_log(run_id, &format!("{l}\n"));
                    }
                });
            }

            let outcome = tokio::select! {
                status = child.wait() => match status {
                    Ok(s) => ("EXITED", s.code()),
                    Err(e) => {
                        let _ = store.append_run_log(run_id, &format!("\nwait failed: {e}\n"));
                        ("FAILED", None)
                    }
                },
                _ = tokio::time::sleep(std::time::Duration::from_secs(MAX_RUN_SECONDS)) => {
                    let _ = child.kill().await;
                    let _ = store.append_run_log(
                        run_id,
                        &format!("\nkilled after {MAX_RUN_SECONDS}s without exiting\n"),
                    );
                    ("KILLED", None)
                }
            };

            let status = if outcome.0 == "EXITED" && outcome.1.unwrap_or(0) != 0 {
                "FAILED"
            } else {
                outcome.0
            };
            let _ = store.finish_run(run_id, status, outcome.1);
            store.revoke_run_token(run_id);
            let _ = std::fs::remove_file(&cfg_path);
        });

        Ok(true)
    }

    fn plan_for(&self, agent_id: i64) -> Result<LaunchPlan> {
        let profiles = self.store.list_profiles()?;
        let agents = self.store.list_agents(None)?;
        let agent = agents
            .into_iter()
            .find(|a| a.id == agent_id)
            .ok_or_else(|| Error::NotFound(format!("agent {agent_id}")))?;
        let Some(pid) = agent.profile_id else {
            return Err(Error::Invalid(format!(
                "{} has no launch profile; set one or mark it External",
                agent.name
            )));
        };
        let p = profiles
            .into_iter()
            .find(|p| p.id == pid)
            .ok_or_else(|| Error::NotFound("launch profile".into()))?;

        let args: Vec<String> = serde_json::from_str(&p.launch_args).map_err(|e| {
            Error::Invalid(format!("profile `{}` has bad launch_args: {e}", p.key))
        })?;
        Ok(LaunchPlan {
            cmd: p.launch_cmd,
            args,
            mcp_install_mode: p.mcp_install_mode,
            profile_key: p.key,
        })
    }
}

/// The instruction handed to a freshly spawned assistant. It has no memory of
/// this workspace, so everything it needs to act is stated here.
fn build_brief(
    agent_name: &str,
    room: &str,
    project: &str,
    detail: &crate::models::ThreadDetail,
    tag: Option<&crate::models::Tag>,
) -> String {
    let s = &detail.summary;
    let mut b = String::new();

    b.push_str(&format!(
        "You are `{agent_name}`, an ASSISTANT in the Rivendell room #{room} for the project \
         `{project}`. You have been called into one thread and your entire job is to answer it.\n\n"
    ));
    b.push_str(&format!(
        "Thread {} — \"{}\"  [{}]\n\n",
        s.id, s.title, s.tag
    ));

    if let Some(t) = tag {
        b.push_str("What this tag expects of you:\n");
        b.push_str(&t.instruction);
        b.push_str("\n\n");
    }

    b.push_str("Do exactly this:\n");
    b.push_str(&format!(
        "1. Call the `rivendell` MCP tool `get_thread` with thread_id={} — it returns the topic \
         plus the diff and file excerpts pinned when the thread was opened. Review those, not \
         whatever the working tree looks like now.\n",
        s.id
    ));
    b.push_str(
        "2. Investigate as much as you need with `read_file`, `list_files` and `git_diff`. They \
         are read-only and jailed to this project.\n",
    );
    b.push_str(&format!(
        "3. Post ONE `reply` with thread_id={}{}. Be concrete: give failing inputs, exact paths \
         and line numbers in `refs`, and a fix where you can. Vague reassurance is worse than \
         silence.\n",
        s.id,
        match tag {
            Some(t) if !t.verdict_options.is_empty() =>
                format!(" and a verdict from: {}", t.verdict_options.join(", ")),
            _ => String::new(),
        }
    ));
    b.push_str(
        "\nDo not modify any file. Do not open new threads. Once your reply is posted you are \
         done — exit rather than looping.\n",
    );

    if !detail.messages.is_empty() {
        b.push_str(&format!(
            "\nNote: {} repl{} already posted. Read them first and add something new rather than \
             restating what is there.\n",
            detail.messages.len(),
            if detail.messages.len() == 1 { "y is" } else { "ies are" }
        ));
    }
    b
}

/// Human-readable command line for the run log. Not used to execute anything.
fn shell_preview(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            let a = if a.len() > 120 {
                let cut = a
                    .char_indices()
                    .map(|(i, _)| i)
                    .take_while(|i| *i <= 120)
                    .last()
                    .unwrap_or(0);
                format!("{}…", &a[..cut])
            } else {
                a.clone()
            };
            if a.contains(char::is_whitespace) {
                format!("{a:?}")
            } else {
                a
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
