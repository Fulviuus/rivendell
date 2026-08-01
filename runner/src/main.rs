//! Wakes an agent when a room needs it.
//!
//!   rivendell-run --key rvd_… -- claude -p "{prompt}"
//!
//! An agent that has to remember to poll will eventually stop. Nothing in MCP
//! can wake an idle model — server-initiated messages reach the host, not the
//! model's context — so the waiting has to happen somewhere that is not a
//! model. That is this: no LLM, no tokens, nothing to forget.
//!
//! It blocks on `wait_for_updates`, and when something lands that concerns this
//! agent it runs the command once, with the thread ids already in the prompt.
//! While the room is quiet it costs nothing at all.

use std::process::Command;
use std::time::{Duration, Instant};

const DEFAULT_URL: &str = "http://127.0.0.1:8787/mcp";

/// Distinct so a parent can tell "this looped" from "this crashed" and stop
/// restarting it.
const EXIT_CEILING: i32 = 3;

/// One JSON line per state change, for a parent watching stdout. Silent unless
/// asked, so running this by hand stays readable.
fn report(cfg: &Config, mut v: serde_json::Value) {
    if !cfg.report {
        return;
    }
    if let Some(o) = v.as_object_mut() {
        o.insert("t".into(), serde_json::json!(now_secs()));
    }
    println!("{v}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Config {
    url: String,
    key: String,
    /// Seconds to hold each long poll open.
    wait: u64,
    /// Pause after a run, so a burst of replies is one wake-up not five.
    settle: u64,
    /// A run still going after this is stuck, not thinking.
    limit: u64,
    /// Nothing legitimate needs an agent started this often. 0 disables it.
    ceiling: usize,
    /// Emit one JSON line per state change, for a parent watching stdout.
    report: bool,
    once: bool,
    cmd: Vec<String>,
}

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}\n");
            usage();
            std::process::exit(2);
        }
    };

    let agent = ureq::AgentBuilder::new()
        // Longer than the poll, or the client hangs up before the server answers.
        .timeout_read(Duration::from_secs(cfg.wait + 60))
        .timeout_connect(Duration::from_secs(10))
        .build();

    // Fail loudly and immediately on a bad key rather than looping in silence.
    let me = match call(&agent, &cfg, "whoami", serde_json::json!({})) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("rivendell-run: could not reach Rivendell as this agent.\n  {e}");
            eprintln!("  Check the app is running and the key is current.");
            std::process::exit(1);
        }
    };
    let name = me["name"].as_str().unwrap_or("agent").to_string();
    let my_id = me["agent_id"].as_i64().unwrap_or(-1);
    let rooms: Vec<&str> = me["rooms"]
        .as_array()
        .map(|a| a.iter().filter_map(|r| r["name"].as_str()).collect())
        .unwrap_or_default();
    eprintln!(
        "rivendell-run: watching for {name} in {}",
        if rooms.is_empty() {
            "no rooms — it has not joined any".to_string()
        } else {
            rooms.iter().map(|r| format!("#{r}")).collect::<Vec<_>>().join(", ")
        }
    );

    // Start from now: a fresh runner should react to what happens next, not
    // replay a backlog it was never running for.
    let mut cursor = match call(
        &agent,
        &cfg,
        "wait_for_updates",
        serde_json::json!({"timeout_s": 1, "watcher": true}),
    )
    {
        Ok(v) => v["next_cursor"].as_i64().unwrap_or(0),
        Err(e) => {
            eprintln!("rivendell-run: could not read the cursor ({e}) — starting from the top");
            0
        }
    };

    report(&cfg, serde_json::json!({ "state": "waiting", "agent": name }));
    let mut recent: Vec<Instant> = Vec::new();

    loop {
        let res = call(
            &agent,
            &cfg,
            "wait_for_updates",
            serde_json::json!({ "cursor": cursor, "timeout_s": cfg.wait, "watcher": true }),
        );
        let v = match res {
            Ok(v) => v,
            Err(e) => {
                // The app restarting is normal; keep the watch alive.
                eprintln!("rivendell-run: {e} — retrying in 5s");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        cursor = v["next_cursor"].as_i64().unwrap_or(cursor);

        let threads = threads_needing_me(&v, my_id);
        if threads.is_empty() {
            if cfg.once {
                return;
            }
            continue;
        }

        let list = threads
            .iter()
            .map(|t| format!("#{t}"))
            .collect::<Vec<_>>()
            .join(", ");
        // Nothing legitimate starts an agent this often. Stopping is the
        // point: a rate limit would still bill every hour of the night, and
        // whatever is looping needs a person to look at it.
        recent.retain(|t: &Instant| t.elapsed() < Duration::from_secs(3600));
        if cfg.ceiling > 0 && recent.len() >= cfg.ceiling {
            report(&cfg, serde_json::json!({
                "state": "ceiling",
                "detail": format!("started {} times in an hour", cfg.ceiling),
            }));
            eprintln!(
                "rivendell-run: started {} times in an hour, which is not normal. Stopping.",
                cfg.ceiling
            );
            std::process::exit(EXIT_CEILING);
        }
        recent.push(Instant::now());

        eprintln!("rivendell-run: {name} — activity on {list}");
        report(&cfg, serde_json::json!({ "state": "running", "threads": threads }));
        run(&cfg, &threads, &list);
        report(&cfg, serde_json::json!({ "state": "waiting" }));

        if cfg.once {
            return;
        }
        std::thread::sleep(Duration::from_secs(cfg.settle));
    }
}

/// Which threads this agent should look at.
///
/// Its own actions are filtered out, which is what stops a reply from waking
/// the agent that just wrote it and looping for ever.
fn threads_needing_me(update: &serde_json::Value, my_id: i64) -> Vec<i64> {
    // Rivendell answers this itself when it can: `needs_you` is the subset
    // somebody else moved *and* this agent may still act on — it knows about
    // resolved threads, paused rooms and spent reply budgets, none of which are
    // visible from out here. Falling back to the local filter keeps this
    // working against an older Rivendell.
    if let Some(ids) = update["needs_you"].as_array() {
        return ids.iter().filter_map(|v| v.as_i64()).collect();
    }
    let mut out: Vec<i64> = Vec::new();
    let Some(events) = update["events"].as_array() else {
        return out;
    };
    for e in events {
        let kind = e["kind"].as_str().unwrap_or("");
        let actionable = matches!(
            kind,
            "thread.created" | "thread.mentioned" | "message.created" | "message.edited"
                | "thread.status"
        );
        if !actionable {
            continue;
        }
        if e["actor_agent_id"].as_i64() == Some(my_id) {
            continue;
        }
        if let Some(id) = e["thread_id"].as_i64() {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

fn run(cfg: &Config, threads: &[i64], list: &str) {
    let prompt = format!(
        "Rivendell has activity on {list}. Read each with get_thread and act on the ones \
         that need you, then stop. If a thread does not concern you, leave it."
    );
    let ids = threads
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let args: Vec<String> = cfg.cmd[1..]
        .iter()
        .map(|a| {
            a.replace("{prompt}", &prompt)
                .replace("{threads}", &ids)
                .replace("{list}", list)
        })
        .collect();

    let mut child = match Command::new(&cfg.cmd[0])
        .args(&args)
        .env("RIVENDELL_URL", &cfg.url)
        .env("RIVENDELL_KEY", &cfg.key)
        .env("RIVENDELL_THREADS", &ids)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rivendell-run: could not run `{}`: {e}", cfg.cmd[0]);
            if e.kind() == std::io::ErrorKind::NotFound {
                eprintln!("  It is not on PATH.");
                std::process::exit(1);
            }
            return;
        }
    };

    // Without a wall clock, one wedged run stops the watch for good — the agent
    // goes quiet and nothing says why. Waiting forever is the failure this
    // whole program exists to prevent.
    let deadline = Instant::now() + Duration::from_secs(cfg.limit);
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                if !s.success() {
                    eprintln!("rivendell-run: agent exited with {s}");
                }
                return;
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("rivendell-run: lost track of the agent: {e}");
                return;
            }
        }
        if Instant::now() >= deadline {
            eprintln!(
                "rivendell-run: still going after {}m — stopping it",
                cfg.limit / 60
            );
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn call(
    agent: &ureq::Agent,
    cfg: &Config,
    tool: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": args }
    })
    .to_string();

    let text = agent
        .post(&cfg.url)
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {}", cfg.key))
        .send_string(&body)
        .map_err(|e| match e {
            ureq::Error::Status(401, _) => "unauthorised — the key is unknown or revoked".into(),
            ureq::Error::Status(c, _) => format!("HTTP {c}"),
            other => other.to_string(),
        })?
        .into_string()
        .map_err(|e| e.to_string())?;

    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if let Some(err) = v["error"]["message"].as_str() {
        return Err(err.to_string());
    }
    let inner = v["result"]["content"][0]["text"].as_str().unwrap_or("{}");
    if v["result"]["isError"].as_bool().unwrap_or(false) {
        return Err(inner.to_string());
    }
    // Tools answer in text; the ones we call answer with JSON in it.
    serde_json::from_str(inner).map_err(|_| inner.to_string())
}

fn parse_args() -> Result<Config, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let split = argv.iter().position(|a| a == "--");
    let (flags, cmd) = match split {
        Some(i) => (&argv[..i], argv[i + 1..].to_vec()),
        None => (&argv[..], Vec::new()),
    };
    if cmd.is_empty() {
        return Err("no command given — put it after `--`".into());
    }

    let mut cfg = Config {
        url: std::env::var("RIVENDELL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string()),
        key: std::env::var("RIVENDELL_KEY").unwrap_or_default(),
        wait: 900,
        settle: 2,
        limit: 20 * 60,
        ceiling: 40,
        report: false,
        once: false,
        cmd,
    };

    let mut i = 0;
    while i < flags.len() {
        let need = |i: usize| -> Result<String, String> {
            flags
                .get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", flags[i]))
        };
        match flags[i].as_str() {
            "--key" => {
                cfg.key = need(i)?;
                i += 2;
            }
            "--url" => {
                cfg.url = need(i)?;
                i += 2;
            }
            "--wait" => {
                cfg.wait = need(i)?.parse().map_err(|_| "--wait wants seconds")?;
                i += 2;
            }
            "--settle" => {
                cfg.settle = need(i)?.parse().map_err(|_| "--settle wants seconds")?;
                i += 2;
            }
            "--limit" => {
                cfg.limit = need(i)?.parse().map_err(|_| "--limit wants seconds")?;
                i += 2;
            }
            "--ceiling" => {
                cfg.ceiling = need(i)?.parse().map_err(|_| "--ceiling wants a count")?;
                i += 2;
            }
            "--report" => {
                cfg.report = true;
                i += 1;
            }
            "--once" => {
                cfg.once = true;
                i += 1;
            }
            "-h" | "--help" => {
                usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown option {other}")),
        }
    }

    if cfg.key.is_empty() {
        return Err("no key — pass --key or set RIVENDELL_KEY".into());
    }
    Ok(cfg)
}

fn usage() {
    eprintln!(
        "\
Wakes an agent when a Rivendell room needs it.

  rivendell-run --key rvd_… -- claude -p \"{{prompt}}\"

  --key KEY       the agent's API key (or RIVENDELL_KEY)
  --url URL       default {DEFAULT_URL} (or RIVENDELL_URL)
  --wait SECS     how long each poll blocks; default 900
  --settle SECS   pause after a run, so a burst is one wake-up; default 2
  --limit SECS    kill a run that has not finished; default 1200
  --ceiling N     stop after N starts in an hour; default 40, 0 to disable
  --report        emit one JSON line per state change on stdout
  --once          handle one wake-up and exit — useful for trying it out

The command runs once per wake-up. In its arguments, {{prompt}} becomes an
instruction naming the threads that changed, {{threads}} the bare ids and
{{list}} them formatted. RIVENDELL_URL, RIVENDELL_KEY and RIVENDELL_THREADS are
in its environment either way."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update(events: serde_json::Value) -> serde_json::Value {
        json!({ "next_cursor": 5, "events": events })
    }

    #[test]
    fn wakes_on_a_reply_from_someone_else() {
        let v = update(json!([
            { "kind": "message.created", "thread_id": 42, "actor_agent_id": 9 }
        ]));
        assert_eq!(threads_needing_me(&v, 7), vec![42]);
    }

    /// The one that would burn tokens for ever: an agent's own reply is an
    /// event in its own rooms, so without this it wakes itself, replies, and
    /// wakes itself again.
    #[test]
    fn ignores_its_own_actions() {
        let v = update(json!([
            { "kind": "message.created", "thread_id": 42, "actor_agent_id": 7 }
        ]));
        assert!(threads_needing_me(&v, 7).is_empty());
    }

    #[test]
    fn ignores_bookkeeping_events() {
        let v = update(json!([
            { "kind": "run.started", "thread_id": 42, "actor_agent_id": 9 },
            { "kind": "agent.updated", "thread_id": 42, "actor_agent_id": 9 },
            { "kind": "room.paused", "thread_id": 42, "actor_agent_id": 9 },
        ]));
        assert!(threads_needing_me(&v, 7).is_empty());
    }

    #[test]
    fn a_human_reply_wakes_it() {
        // A human has no agent id, so the self-filter must not swallow it.
        let v = update(json!([
            { "kind": "message.created", "thread_id": 42, "actor_agent_id": null }
        ]));
        assert_eq!(threads_needing_me(&v, 7), vec![42]);
    }

    /// A burst on one thread is one wake-up, not four.
    #[test]
    fn collapses_a_burst_and_keeps_order() {
        let v = update(json!([
            { "kind": "message.created", "thread_id": 42, "actor_agent_id": 9 },
            { "kind": "thread.created", "thread_id": 43, "actor_agent_id": 9 },
            { "kind": "message.edited", "thread_id": 42, "actor_agent_id": 9 },
            { "kind": "thread.mentioned", "thread_id": 44, "actor_agent_id": 9 },
        ]));
        assert_eq!(threads_needing_me(&v, 7), vec![42, 43, 44]);
    }

    /// Rivendell knows things this program cannot see — resolved threads,
    /// paused rooms, a spent reply budget. When it answers, its answer wins.
    #[test]
    fn rivendells_own_answer_is_preferred() {
        let v = json!({
            "next_cursor": 5,
            "needs_you": [43],
            "events": [
                { "kind": "message.created", "thread_id": 42, "actor_agent_id": 9 },
                { "kind": "message.created", "thread_id": 43, "actor_agent_id": 9 },
            ]
        });
        // Locally both look like work; Rivendell says only 43 is.
        assert_eq!(threads_needing_me(&v, 7), vec![43]);
    }

    /// An older Rivendell does not send it, and the watch has to keep working.
    #[test]
    fn without_it_the_local_filter_still_runs() {
        let v = json!({
            "next_cursor": 5,
            "events": [
                { "kind": "message.created", "thread_id": 42, "actor_agent_id": 7 },
                { "kind": "message.created", "thread_id": 43, "actor_agent_id": 9 },
            ]
        });
        assert_eq!(threads_needing_me(&v, 7), vec![43]);
    }

    /// An empty list is an answer — "nothing here needs you" — not an absence.
    #[test]
    fn an_empty_answer_means_do_nothing() {
        let v = json!({
            "next_cursor": 5,
            "needs_you": [],
            "events": [
                { "kind": "message.created", "thread_id": 42, "actor_agent_id": 9 },
            ]
        });
        assert!(threads_needing_me(&v, 7).is_empty());
    }

    #[test]
    fn quiet_poll_means_no_wake_up() {
        assert!(threads_needing_me(&update(json!([])), 7).is_empty());
        assert!(threads_needing_me(&json!({ "next_cursor": 5 }), 7).is_empty());
    }
}
