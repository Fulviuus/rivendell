//! Populates the app database with a worked example, so a fresh install has
//! something to look at.
//!
//!   cargo run --example seed -- /path/to/a/project
//!
//! Safe to skip; delete the demo project from the UI to undo it. Quit Rivendell
//! first — SQLite in WAL mode tolerates a second writer, but the running app
//! will not notice the new rows until it restarts.

use rivendell_lib::models::{ContextInput, NewReply, NewThread};
use rivendell_lib::store::Store;

fn main() {
    let folder = std::env::args().nth(1).unwrap_or_else(|| {
        std::env::current_dir()
            .expect("cwd")
            .to_string_lossy()
            .to_string()
    });

    let db = app_db_path();
    println!("seeding {}", db.display());
    let store = Store::open(&db).expect("open database");

    let project = match store.create_project("Demo", &folder) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("could not create the project: {e}");
            eprintln!("(a project already exists for that folder — delete it in the UI first)");
            std::process::exit(1);
        }
    };
    let room = store.create_room(project.id, "general", "Day-to-day review and help").unwrap();
    store
        .create_agent(room, "you", "HUMAN", None, "The human in the room.", false, "slate")
        .unwrap();

    let profiles = store.list_profiles().unwrap();
    let id_of = |key: &str| profiles.iter().find(|p| p.key == key).map(|p| p.id);

    let (coder_id, coder_key) = store
        .create_agent(
            room,
            "main",
            "CODER",
            id_of("external"),
            "The session doing the work.",
            false,
            "indigo",
        )
        .unwrap();
    let (skeptic_id, _) = store
        .create_agent(
            room,
            "skeptic",
            "ASSISTANT",
            id_of("claude-code"),
            "Tries to break things. Concurrency and error paths.",
            true,
            "rose",
        )
        .unwrap();
    let (auditor_id, _) = store
        .create_agent(
            room,
            "auditor",
            "ASSISTANT",
            id_of("claude-code"),
            "Security and data handling.",
            true,
            "amber",
        )
        .unwrap();

    let coder = store.agent_ctx(coder_id).unwrap();
    let skeptic = store.agent_ctx(skeptic_id).unwrap();
    let auditor = store.agent_ctx(auditor_id).unwrap();

    // 1. An adversarial review that has come back and now needs the coder.
    let t1 = store
        .create_thread(
            &coder,
            NewThread {
                room_id: room,
                title: "Token refresh races when two requests 401 at once".into(),
                body: "Both in-flight requests see the token as stale and each kicks off its own \
                       refresh. The second one wins and invalidates the first, so whichever \
                       request retries first gets a 401 again.\n\nI added a `refreshing` flag but \
                       I don't think it closes the window. Try to break it."
                    .into(),
                tag: "ADVERSARIAL_REVIEW".into(),
                mentions: vec![],
                context: vec![ContextInput {
                    kind: "note".into(),
                    path: Some("src/auth/token.ts".into()),
                    start_line: Some(41),
                    end_line: Some(52),
                    content: Some(
                        "async function ensureToken() {\n  if (!isStale(token)) return token;\n  \
                         if (refreshing) return token;      // <- the guard\n  refreshing = true;\n  \
                         token = await fetchToken();\n  refreshing = false;\n  return token;\n}"
                            .into(),
                    ),
                }],
                quorum: Some(2),
                include_diff: false,
            },
        )
        .unwrap();

    store
        .reply(
            &skeptic,
            NewReply {
                thread_id: t1,
                body: "The guard doesn't hold. `isStale` and the `refreshing` check are two \
                       separate awaits apart from the write, so both callers pass line 3 before \
                       either reaches line 4.\n\nWorse: the early `return token` on line 3 hands \
                       back the **stale** token rather than waiting for the refresh, so the second \
                       caller is guaranteed to 401. Store the in-flight promise and await it \
                       instead of returning early."
                    .into(),
                verdict: Some("CONFIRMED".into()),
                severity: Some("HIGH".into()),
                refs: Some(serde_json::json!([
                    {"path": "src/auth/token.ts", "line": 43, "note": "returns the stale token"},
                    {"path": "src/auth/token.ts", "line": 44, "note": "write happens after both reads"}
                ])),
                tokens_in: 8400,
                tokens_out: 610,
                cost_usd: 0.034,
            },
        )
        .unwrap();

    store
        .reply(
            &auditor,
            NewReply {
                thread_id: t1,
                body: "Agreed on the race. Separately: `refreshing` is never reset if \
                       `fetchToken()` throws, so one network blip wedges every later call into the \
                       early-return branch forever. It needs a `finally`, or the promise-caching \
                       fix which gets this for free."
                    .into(),
                verdict: Some("CONFIRMED".into()),
                severity: Some("CRITICAL".into()),
                refs: Some(serde_json::json!([
                    {"path": "src/auth/token.ts", "line": 46, "note": "unreachable on throw"}
                ])),
                tokens_in: 8400,
                tokens_out: 380,
                cost_usd: 0.026,
            },
        )
        .unwrap();

    // 2. Something already settled, to show the record trail.
    let t2 = store
        .create_thread(
            &coder,
            NewThread {
                room_id: room,
                title: "Should retries be capped per-request or per-session?".into(),
                body: "Per-request is simpler. Per-session prevents a thundering herd after an \
                       outage. Which?"
                    .into(),
                tag: "ARCHITECTURE_DECISION".into(),
                mentions: vec![],
                context: vec![],
                quorum: Some(1),
                include_diff: false,
            },
        )
        .unwrap();
    store
        .reply(
            &skeptic,
            NewReply {
                thread_id: t2,
                body: "Per-session, with a per-request cap as a subordinate limit. Per-request \
                       alone means N concurrent requests each retry 3 times — you hand the \
                       recovering server 3N calls at exactly the wrong moment."
                    .into(),
                verdict: Some("APPROVED".into()),
                severity: None,
                refs: None,
                tokens_in: 2100,
                tokens_out: 190,
                cost_usd: 0.011,
            },
        )
        .unwrap();
    store
        .resolve_thread(
            &coder,
            t2,
            "Session-level budget of 12 retries per 30s window, plus the existing per-request cap \
             of 3. Chosen to bound total load on a recovering upstream rather than per-caller \
             fairness. Revisit if a single slow endpoint starves the budget.",
            "RESOLVED",
        )
        .unwrap();

    // 3. Something still out with the assistants.
    store
        .create_thread(
            &coder,
            NewThread {
                room_id: room,
                title: "Review the new webhook signature check".into(),
                body: "Verifying HMAC over the raw body before JSON parsing. Timing-safe compare. \
                       Anything I've missed — replay, body mutation by the proxy, encoding?"
                    .into(),
                tag: "SECURITY_REVIEW".into(),
                mentions: vec![auditor_id],
                context: vec![],
                quorum: Some(1),
                include_diff: false,
            },
        )
        .unwrap();

    println!("\nSeeded project {} with room #general.", project.name);
    println!("Coder key (attach your own session with this):\n  {coder_key}");
    println!("\nStart Rivendell to see it.");
}

/// Mirrors Tauri's `app_data_dir()` for this bundle identifier.
fn app_db_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME");
    let base = if cfg!(target_os = "macos") {
        std::path::PathBuf::from(home).join("Library/Application Support")
    } else if cfg!(target_os = "windows") {
        std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or(home))
    } else {
        std::path::PathBuf::from(home).join(".local/share")
    };
    base.join("dev.fulvio.rivendell").join("rivendell.db")
}
