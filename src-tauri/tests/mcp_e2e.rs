//! End-to-end exercise of the MCP surface over real HTTP: auth, tool dispatch,
//! verdict enforcement, the reply cap, room isolation and the export on resolve.

use rivendell_lib::mcp::server::{serve, McpState};
use rivendell_lib::store::Store;
use serde_json::{json, Value};
use std::sync::Arc;

struct Harness {
    url: String,
    dir: std::path::PathBuf,
    store: Arc<Store>,
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rivendell-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/auth.rs"),
        "fn refresh() {\n    // no lock here\n    token = fetch();\n}\n",
    )
    .unwrap();
    std::fs::canonicalize(dir).unwrap()
}

async fn boot(name: &str) -> Harness {
    let dir = scratch(name);
    let store = Arc::new(Store::open(&dir.join(".db/rivendell.db")).unwrap());
    let state = Arc::new(McpState { store: store.clone() });
    let running = serve(state, 0).await.unwrap();
    Harness {
        url: running.url,
        dir,
        store,
    }
}

/// One JSON-RPC round trip. Returns the status code and parsed body.
fn rpc(url: &str, key: Option<&str>, method: &str, params: Value) -> (u16, Value) {
    let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string();
    let mut req = ureq::post(url).set("Content-Type", "application/json");
    if let Some(k) = key {
        req = req.set("Authorization", &format!("Bearer {k}"));
    }
    match req.send_string(&body) {
        Ok(r) => {
            let code = r.status();
            let text = r.into_string().unwrap_or_default();
            (code, serde_json::from_str(&text).unwrap_or(Value::Null))
        }
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            (code, serde_json::from_str(&text).unwrap_or(Value::Null))
        }
        Err(e) => panic!("transport error: {e}"),
    }
}

/// Ages a thread so the grace window has demonstrably passed, instead of
/// sleeping through it.
fn backdate(dir: &std::path::Path, column: &str, table: &str, id: i64, seconds: i64) {
    let conn = rusqlite::Connection::open(dir.join(".db/rivendell.db")).unwrap();
    conn.execute(
        &format!("UPDATE {table} SET {column}=?1 WHERE {} =?2",
                 if table == "thread_claims" { "agent_id" } else { "id" }),
        rusqlite::params![iso(chrono_now() - seconds), id],
    )
    .unwrap();
}

/// Ages the moment the first answer landed, so the claim window has closed.
fn age_gather(dir: &std::path::Path, thread_id: i64, seconds: i64) {
    backdate(dir, "gather_started_at", "threads", thread_id, seconds);
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Minimal RFC3339 in UTC, matching what the store writes.
fn iso(unix: i64) -> String {
    let days = unix / 86400;
    let rem = unix % 86400;
    let (mut y, mut d) = (1970, days);
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if d < len {
            break;
        }
        d -= len;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let months = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0;
    while d >= months[m] {
        d -= months[m];
        m += 1;
    }
    format!(
        "{y:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
        m + 1,
        d + 1,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn call(url: &str, key: &str, tool: &str, args: Value) -> (bool, String) {
    let (_, body) = rpc(url, Some(key), "tools/call", json!({"name": tool, "arguments": args}));
    let result = &body["result"];
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let text = result["content"][0]["text"].as_str().unwrap_or("").to_string();
    (is_error, text)
}

/// Creates an agent in the project and puts it in the room. Agents belong to a
/// project now, and membership is what lets them see a room at all.
fn mk_agent(h: &Harness, project: i64, room: i64, name: &str, role: &str) -> (i64, String) {
    let (id, key) = h.store.create_agent(project, name, role, None, "", "").unwrap();
    h.store.join_room(room, id).unwrap();
    (id, key)
}

fn mk_agent_with(
    h: &Harness,
    project: i64,
    room: i64,
    name: &str,
    role: &str,
    profile: Option<i64>,
) -> (i64, String) {
    let (id, key) = h.store.create_agent(project, name, role, profile, "", "").unwrap();
    h.store.join_room(room, id).unwrap();
    (id, key)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_thread_lifecycle() {
    let h = boot("lifecycle").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();

    let profiles = h.store.list_profiles().unwrap();
    let external = profiles.iter().find(|p| p.key == "external").unwrap().id;

    let (_, coder_key) = mk_agent_with(&h, project.id, room, "main", "CODER", Some(external));
    let (assistant_id, assistant_key) = mk_agent_with(&h, project.id, room, "skeptic", "ASSISTANT", Some(external));

    // --- auth -----------------------------------------------------------
    let (code, _) = rpc(&h.url, None, "initialize", json!({}));
    assert_eq!(code, 401, "a request with no bearer token must be rejected");

    let (code, _) = rpc(&h.url, Some("rvd_bogus_key"), "initialize", json!({}));
    assert_eq!(code, 401, "an unknown key must be rejected");

    // --- initialize ------------------------------------------------------
    let (code, body) = rpc(
        &h.url,
        Some(&coder_key),
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}}),
    );
    assert_eq!(code, 200);
    assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(body["result"]["serverInfo"]["name"], "rivendell");

    // --- tool visibility is role-scoped ----------------------------------
    let (_, body) = rpc(&h.url, Some(&coder_key), "tools/list", json!({}));
    let coder_tools: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(coder_tools.contains(&"create_thread"));
    assert!(coder_tools.contains(&"resolve_thread"));

    let (_, body) = rpc(&h.url, Some(&assistant_key), "tools/list", json!({}));
    let asst_tools: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(asst_tools.contains(&"reply"));
    // Every agent is the same kind of thing now: whoever has something worth
    // asking can open a thread, and whoever opened one can close it.
    assert!(
        asst_tools.contains(&"create_thread"),
        "the council offers the same tools to everyone"
    );

    // Any agent may convene the council. What it may not do is answer a thread
    // that did not ask it.
    let (is_err, text) = call(
        &h.url,
        &assistant_key,
        "create_thread",
        json!({"title": "asking in turn", "body": "who knows about this?", "tag": "FYI"}),
    );
    assert!(!is_err, "every agent may open a thread now: {text}");
    let mine: i64 = text
        .split_whitespace()
        .find_map(|w| w.trim_end_matches('.').parse().ok())
        .expect("thread id");

    // It asked nobody, so nobody else may speak in it — but its own author can.
    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "reply",
        json!({"thread_id": mine, "body": "butting in"}),
    );
    assert!(is_err, "an agent this thread did not ask must be refused: {text}");
    assert!(text.contains("did not ask you"), "and told why: {text}");

    // --- create a thread with pinned context ------------------------------
    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({
            "title": "Token refresh races",
            "body": "@everyone Two concurrent 401s both trigger a refresh.",
            "tag": "ADVERSARIAL_REVIEW",
            "context": [{"kind": "file", "path": "src/auth.rs", "start_line": 1, "end_line": 4}]
        }),
    );
    assert!(!is_err, "create_thread failed: {text}");
    let thread_id: i64 = text
        .split_whitespace()
        .find_map(|w| w.trim_end_matches('.').parse().ok())
        .expect("thread id in response");

    // --- assistant reads it, pinned context included ----------------------
    let (is_err, text) = call(&h.url, &assistant_key, "get_thread", json!({"thread_id": thread_id}));
    assert!(!is_err, "{text}");
    assert!(text.contains("Token refresh races"));
    assert!(text.contains("no lock here"), "pinned file excerpt missing");
    assert!(text.contains("CLEARED"), "tag instruction should be surfaced");

    // --- a verdict is offered, never demanded -----------------------------
    // Most of what gets said in a discussion is not a conclusion, so a reply
    // without one is ordinary. A wrong one is still refused.
    let (is_err, text) = call(
        &h.url,
        &assistant_key,
        "reply",
        json!({"thread_id": thread_id, "body": "Reading it now — one thing looks odd."}),
    );
    assert!(!is_err, "a reply that is not a conclusion is fine: {text}");

    let (is_err, text) = call(
        &h.url,
        &assistant_key,
        "reply",
        json!({"thread_id": thread_id, "body": "x", "verdict": "MAYBE"}),
    );
    assert!(is_err, "a verdict that is not one of the tag's must still be refused");
    assert!(text.contains("CONFIRMED"), "the error should list valid verdicts: {text}");

    let (is_err, text) = call(
        &h.url,
        &assistant_key,
        "reply",
        json!({
            "thread_id": thread_id,
            "body": "Both callers pass the stale-token check before either writes.",
            "verdict": "CONFIRMED",
            "severity": "HIGH",
            "refs": [{"path": "src/auth.rs", "line": 3, "note": "unguarded write"}]
        }),
    );
    assert!(!is_err, "{text}");

    // An invalid verdict for this tag is refused.
    let (is_err, _) = call(
        &h.url,
        &assistant_key,
        "reply",
        json!({"thread_id": thread_id, "body": "x", "verdict": "APPROVED"}),
    );
    assert!(is_err, "a verdict from another tag's set must be rejected");

    // --- read-only file tools are jailed -----------------------------------
    let (is_err, _) = call(&h.url, &assistant_key, "read_file", json!({"path": "src/auth.rs"}));
    assert!(!is_err, "reading inside the project must work");

    for escape in ["../../../etc/passwd", "/etc/passwd", ".git/config"] {
        let (is_err, text) = call(&h.url, &assistant_key, "read_file", json!({"path": escape}));
        assert!(is_err, "`{escape}` must be refused, got: {text}");
    }

    // --- the reply cap stops a runaway loop ---------------------------------
    h.store
        .update_room(room, json!({"max_replies_per_agent": 3}))
        .unwrap();
    let mut hit_cap = false;
    for i in 0..6 {
        let (is_err, text) = call(
            &h.url,
            &assistant_key,
            "reply",
            json!({"thread_id": thread_id, "body": format!("follow-up {i}"), "verdict": "UNCERTAIN"}),
        );
        if is_err && text.contains("cap") {
            hit_cap = true;
            break;
        }
    }
    assert!(hit_cap, "the per-agent reply cap must eventually refuse");

    // --- pausing the room stops agents but not the human ---------------------
    h.store.update_room(room, json!({"paused": true})).unwrap();
    let (is_err, text) = call(
        &h.url,
        &assistant_key,
        "reply",
        json!({"thread_id": thread_id, "body": "still here", "verdict": "UNCERTAIN"}),
    );
    assert!(is_err && text.contains("paused"), "paused room must refuse agents: {text}");
    h.store.update_room(room, json!({"paused": false})).unwrap();

    // --- resolve writes the decision record ----------------------------------
    let (is_err, text) = call(
        &h.url,
        &assistant_key,
        "resolve_thread",
        json!({"thread_id": thread_id, "summary": "nope"}),
    );
    assert!(is_err, "an assistant must not be able to resolve: {text}");

    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "resolve_thread",
        json!({
            "thread_id": thread_id,
            "summary": "Confirmed. Added a single-flight guard around refresh."
        }),
    );
    assert!(!is_err, "{text}");

    let record = h.dir.join(".rivendell/threads");
    let files: Vec<_> = std::fs::read_dir(&record)
        .expect("export directory should exist")
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(files.len(), 1, "exactly one decision record should be written");
    let contents = std::fs::read_to_string(files[0].path()).unwrap();
    assert!(contents.contains("single-flight guard"));
    assert!(contents.contains("CONFIRMED"), "verdicts belong in the record");
    assert!(contents.contains("status: RESOLVED"));

    // A resolved thread is closed for further agent replies.
    let (is_err, _) = call(
        &h.url,
        &assistant_key,
        "reply",
        json!({"thread_id": thread_id, "body": "one more", "verdict": "UNCERTAIN"}),
    );
    assert!(is_err, "a resolved thread must not accept new agent replies");

    let _ = assistant_id;
    let _ = std::fs::remove_dir_all(&h.dir);
}

/// The three orderings must genuinely differ: an old thread that is still busy
/// should outrank a newer quiet one under "activity", and lose to it under
/// "created".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn threads_sort_three_ways() {
    let h = boot("sorting").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (coder_id, _) = mk_agent(&h, project.id, room, "main", "CODER");
    let (asst_id, _) = mk_agent(&h, project.id, room, "helper", "ASSISTANT");
    let coder = h.store.agent_ctx(coder_id).unwrap();
    let asst = h.store.agent_ctx(asst_id).unwrap();

    let mut open = |title: &str| {
        h.store
            .create_thread(
                &coder,
                rivendell_lib::models::NewThread {
                    room_id: room,
                    title: title.into(),
                    body: "@everyone …".into(),
                    tag: "FYI".into(),
                    mentions: vec![],
                    context: vec![],
                    include_diff: false,
                },
            )
            .unwrap()
    };

    let old_busy = open("old but busy");
    let newest_quiet = open("newest and quiet");

    // Only the older thread gets replies.
    for i in 0..3 {
        h.store
            .reply(
                &asst,
                rivendell_lib::models::NewReply {
                    thread_id: old_busy,
                    body: format!("reply {i}"),
                    verdict: None,
                    severity: None,
                    refs: None,
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_usd: 0.0,
                },
            )
            .unwrap();
    }

    let titles = |sort: &str| -> Vec<String> {
        h.store
            .list_threads(Some(room), Some("all"), None, None, Some(sort), 50)
            .unwrap()
            .into_iter()
            .map(|t| t.title)
            .collect()
    };

    // Newest thread first, regardless of who is talking.
    assert_eq!(titles("created")[0], "newest and quiet");
    // Busiest first.
    assert_eq!(titles("activity")[0], "old but busy");
    // Freshest conversation first — the replies landed after the quiet thread
    // was opened, so the busy one leads here too.
    assert_eq!(titles("last_reply")[0], "old but busy");

    // A thread with no replies still sorts by when it was opened rather than
    // dropping to the bottom on a NULL.
    let all = h
        .store
        .list_threads(Some(room), Some("all"), None, None, Some("last_reply"), 50)
        .unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|t| t.last_reply_at.is_none()));

    let _ = std::fs::remove_dir_all(&h.dir);
}



/// The revision loop: someone edits, the room is told, and the agent whose
/// answer is now stale revises it rather than posting a correction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edit_is_announced_and_can_be_answered() {
    let h = boot("edits").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (_, coder_key) = mk_agent(&h, project.id, room, "main", "CODER");
    let (_, asst_key) = mk_agent(&h, project.id, room, "helper", "ASSISTANT");

    let (_, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({"title": "Which one", "body": "@everyone …", "tag": "ADVERSARIAL_REVIEW"}),
    );
    let thread_id: i64 = text
        .split_whitespace()
        .find_map(|w| w.trim_end_matches('.').parse().ok())
        .unwrap();

    let (is_err, text) = call(
        &h.url,
        &asst_key,
        "reply",
        json!({"thread_id": thread_id, "body": "Breaks on empty input.", "verdict": "CONFIRMED"}),
    );
    assert!(!is_err, "{text}");
    let msg_id = h.store.thread_detail(thread_id).unwrap().messages[0].id;
    assert!(
        h.store.thread_detail(thread_id).unwrap().messages[0]
            .edited_at
            .is_none(),
        "not edited yet"
    );

    // The coder parks on a cursor, then the assistant revises its own reply.
    let (_, before) = call(&h.url, &coder_key, "wait_for_updates", json!({"timeout_s": 1}));
    let cursor = serde_json::from_str::<Value>(&before).unwrap()["next_cursor"]
        .as_i64()
        .unwrap();

    let (is_err, text) = call(
        &h.url,
        &asst_key,
        "edit_reply",
        json!({
            "message_id": msg_id,
            "body": "Re-checked against the edited topic: the empty case is guarded.",
            "verdict": "REFUTED"
        }),
    );
    assert!(!is_err, "{text}");

    let d = h.store.thread_detail(thread_id).unwrap();
    assert!(d.messages[0].edited_at.is_some(), "edit must be marked");
    assert_eq!(
        d.messages[0].verdict.as_deref(),
        Some("CLEARED"),
        "an edit normalises the old name too"
    );
    assert_eq!(d.messages.len(), 1, "editing must not add a message");

    // The coder is told, and the old verdict is on the event so the change is
    // not silent.
    let (_, got) = call(
        &h.url,
        &coder_key,
        "wait_for_updates",
        json!({"cursor": cursor, "timeout_s": 5}),
    );
    let got: Value = serde_json::from_str(&got).unwrap();
    let edit = got["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "message.edited")
        .expect("an edit must be announced");
    assert_eq!(edit["payload"]["previous_verdict"], "CONFIRMED");
    assert_eq!(edit["payload"]["verdict"], "CLEARED");

    // You may only rewrite your own words.
    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "edit_reply",
        json!({"message_id": msg_id, "body": "actually it is fine", "verdict": "REFUTED"}),
    );
    assert!(is_err, "one participant must not rewrite another's: {text}");

    // A tag's verdict rules still apply to an edit.
    let (is_err, text) = call(
        &h.url,
        &asst_key,
        "edit_reply",
        json!({"message_id": msg_id, "body": "hm", "verdict": "APPROVED"}),
    );
    assert!(is_err, "an edit must not smuggle in a verdict the tag forbids: {text}");

    let _ = std::fs::remove_dir_all(&h.dir);
}

/// Project edits and the delete cascade. The counts shown before the
/// confirmation have to be right — they are what the decision is made on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_settings_and_deletion() {
    let h = boot("project").await;
    let project = h
        .store
        .create_project("Demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (coder_id, _) = mk_agent(&h, project.id, room, "main", "CODER");
    let (_, asst_key) = mk_agent(&h, project.id, room, "helper", "ASSISTANT");
    let coder = h.store.agent_ctx(coder_id).unwrap();

    let t1 = h
        .store
        .create_thread(
            &coder,
            rivendell_lib::models::NewThread {
                room_id: room,
                title: "One".into(),
                body: "@everyone …".into(),
                tag: "HELP_REQUEST".into(),
                mentions: vec![],
                context: vec![],
                include_diff: false,
            },
        )
        .unwrap();
    call(
        &h.url,
        &asst_key,
        "reply",
        json!({"thread_id": t1, "body": "here", "verdict": "ANSWERED"}),
    );

    // --- rename and recolour ---------------------------------------------
    h.store
        .update_project(project.id, json!({"name": "Renamed", "color": "teal"}))
        .unwrap();
    let p = h.store.list_projects().unwrap().into_iter().next().unwrap();
    assert_eq!(p.name, "Renamed");
    assert_eq!(p.color, "teal");

    // An empty name would leave the sidebar with a blank row.
    assert!(h.store.update_project(project.id, json!({"name": "  "})).is_err());

    // --- move the working folder -----------------------------------------
    let moved = h.dir.parent().unwrap().join("rivendell-e2e-project-moved");
    std::fs::create_dir_all(&moved).unwrap();
    let moved = std::fs::canonicalize(&moved).unwrap();
    h.store
        .update_project(project.id, json!({"folder_path": moved.to_str().unwrap()}))
        .unwrap();
    assert_eq!(
        h.store.agent_ctx(coder_id).unwrap().folder_path,
        moved.to_string_lossy(),
        "agents read from the new folder"
    );
    assert!(
        h.store
            .update_project(project.id, json!({"folder_path": "/definitely/not/here"}))
            .is_err(),
        "a folder that does not exist must be refused, not stored"
    );

    // --- what deletion would destroy -------------------------------------
    let stats = h.store.project_stats(project.id).unwrap();
    assert_eq!(stats.rooms, 1);
    assert_eq!(stats.threads, 1);
    assert_eq!(stats.messages, 1);
    assert_eq!(stats.agents, 2);

    h.store.delete_project(project.id).unwrap();
    assert!(h.store.list_projects().unwrap().is_empty());
    assert!(h.store.list_rooms().unwrap().is_empty(), "rooms cascade");
    assert!(
        h.store.thread_detail(t1).is_err(),
        "threads cascade with the project"
    );
    assert!(
        h.store.list_agents(None).unwrap().is_empty(),
        "agents cascade, so their keys stop working"
    );

    let _ = std::fs::remove_dir_all(&moved);
    let _ = std::fs::remove_dir_all(&h.dir);
}






/// Two agents and a coder, which most of these tests want.
async fn seed_room(h: &Harness) -> (i64, String, String, String) {
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (_, coder) = mk_agent(&h, project.id, room, "main", "CODER");
    let (_, first) = mk_agent(&h, project.id, room, "first", "ASSISTANT");
    let (_, second) = mk_agent(&h, project.id, room, "second", "ASSISTANT");
    (room, coder, first, second)
}

fn open_thread(h: &Harness, coder_key: &str, title: &str) -> i64 {
    let (is_err, text) = call(
        &h.url,
        coder_key,
        "create_thread",
        json!({"title": title, "body": "@everyone …", "tag": "HELP_REQUEST"}),
    );
    assert!(!is_err, "{text}");
    text.split_whitespace()
        .find_map(|w| w.trim_end_matches('.').parse().ok())
        .unwrap()
}

/// One agent, one key, several rooms — the thing room-scoped agents made
/// impossible without creating a duplicate for each room.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_agent_can_be_in_several_rooms() {
    let h = boot("membership").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let general = h.store.create_room(project.id, "general", "").unwrap();
    let security = h.store.create_room(project.id, "security", "").unwrap();

    let (_, coder_key) = mk_agent(&h, project.id, general, "main", "CODER");
    let (helper_id, helper_key) = mk_agent(&h, project.id, general, "helper", "ASSISTANT");

    // Same agent, same key, now also in #security.
    h.store.join_room(security, helper_id).unwrap();
    h.store.join_room(security, h.store.list_agents(None).unwrap()
        .iter().find(|a| a.name == "main").unwrap().id).unwrap();

    let (_, who) = call(&h.url, &helper_key, "whoami", json!({}));
    let who: Value = serde_json::from_str(&who).unwrap();
    let rooms: Vec<&str> = who["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(rooms.len(), 2, "one key, two rooms: {rooms:?}");

    // A thread in each; the agent sees both without a second identity.
    for room in ["general", "security"] {
        let (is_err, text) = call(
            &h.url,
            &coder_key,
            "create_thread",
            json!({"room": room, "title": format!("in {room}"), "body": "@everyone …", "tag": "HELP_REQUEST"}),
        );
        assert!(!is_err, "{text}");
    }
    let (_, listed) = call(&h.url, &helper_key, "list_threads", json!({"status": "all"}));
    assert!(listed.contains("in general") && listed.contains("in security"));

    // Narrowing to one room works, and only for rooms it is in.
    let (_, one) = call(&h.url, &helper_key, "list_threads", json!({"room": "security", "status": "all"}));
    assert!(one.contains("in security") && !one.contains("in general"));

    // Leaving takes the access away again.
    h.store.leave_room(security, helper_id).unwrap();
    let (is_err, text) = call(
        &h.url,
        &helper_key,
        "list_threads",
        json!({"room": "security", "status": "all"}),
    );
    assert!(is_err, "a room it left must be refused: {text}");

    // Names are unique per project now, not per room.
    assert!(
        h.store
            .create_agent(project.id, "helper", "ASSISTANT", None, "", "")
            .is_err(),
        "two agents in a project cannot share a name"
    );

    let _ = std::fs::remove_dir_all(&h.dir);
}


/// The old verdict name still works, so an agent mid-reply is not broken by a
/// rename it never saw.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_renamed_verdict_still_accepts_the_old_word() {
    let h = boot("verdict").await;
    let (_room, coder_key, first, _second) = seed_room(&h).await;

    let (_, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({"title": "Break this", "body": "@everyone …", "tag": "ADVERSARIAL_REVIEW"}),
    );
    let id: i64 = text
        .split_whitespace()
        .find_map(|w| w.trim_end_matches('.').parse().ok())
        .unwrap();

    let (is_err, text) = call(
        &h.url,
        &first,
        "reply",
        json!({"thread_id": id, "body": "could not break it", "verdict": "REFUTED"}),
    );
    assert!(!is_err, "the old word must still be accepted: {text}");
    assert_eq!(
        h.store.thread_detail(id).unwrap().messages[0].verdict.as_deref(),
        Some("CLEARED"),
        "and is stored under the new name"
    );

    let _ = std::fs::remove_dir_all(&h.dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rooms_are_isolated() {
    let h = boot("isolation").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room_a = h.store.create_room(project.id, "alpha", "").unwrap();
    let room_b = h.store.create_room(project.id, "beta", "").unwrap();

    let (_, coder_a) = mk_agent(&h, project.id, room_a, "a-coder", "CODER");
    let (_, coder_b) = mk_agent(&h, project.id, room_b, "b-coder", "CODER");

    let (is_err, text) = call(
        &h.url,
        &coder_a,
        "create_thread",
        json!({"room": "alpha", "title": "secret alpha work", "body": "@everyone …", "tag": "FYI"}),
    );
    assert!(!is_err, "{text}");
    let thread_id: i64 = text
        .split_whitespace()
        .find_map(|w| w.trim_end_matches('.').parse().ok())
        .unwrap();

    // Same project, but b-coder never joined #alpha.
    let (is_err, text) = call(&h.url, &coder_b, "get_thread", json!({"thread_id": thread_id}));
    assert!(is_err, "a non-member must not read that room's thread: {text}");

    let (_, text) = call(&h.url, &coder_b, "list_threads", json!({"status": "all"}));
    assert!(
        !text.contains("secret alpha work"),
        "room B's listing leaked room A: {text}"
    );

    let _ = std::fs::remove_dir_all(&h.dir);
}

/// A message says which kind of session wrote it.
///
/// A watcher-started run and a session you are sitting in front of hold the
/// same identity by design. Without recording which was which, two live at
/// once makes every attribution an argument about timestamps — and I lost one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reply_records_whether_rivendell_started_it() {
    let h = boot("attribution").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (_coder, coder_key) = mk_agent(&h, project.id, room, "dev", "CODER");
    let (scout, own_key) = mk_agent(&h, project.id, room, "scout", "ASSISTANT");

    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({"title": "who answered", "body": "@everyone ?", "tag": "HELP_REQUEST"}),
    );
    assert!(!is_err, "{text}");
    let tid: i64 = text
        .split_whitespace()
        .find_map(|w| w.trim_end_matches('.').parse().ok())
        .expect("thread id in response");

    // A session the user runs themselves.
    let (is_err, text) = call(
        &h.url,
        &own_key,
        "reply",
        json!({"thread_id": tid, "body": "from my own terminal", "verdict": "ANSWERED"}),
    );
    assert!(!is_err, "{text}");

    // A run Rivendell started, with the credential it mints for one.
    let (token, handle) = h.store.mint_live_token(scout).unwrap();
    let (is_err, text) = call(
        &h.url,
        &token,
        "reply",
        json!({"thread_id": tid, "body": "started by the watcher", "verdict": "ANSWERED"}),
    );
    assert!(!is_err, "{text}");

    let marks: Vec<bool> = h
        .store
        .events_since(0, None, 500)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "message.created" && e.actor_agent_id == Some(scout))
        .map(|e| e.payload["supervised"].as_bool().unwrap_or(false))
        .collect();
    assert_eq!(
        marks,
        vec![false, true],
        "the log must say which session wrote each reply"
    );

    h.store.drop_live_token(&handle);
    let _ = std::fs::remove_dir_all(&h.dir);
}

/// The notification stream delivers, and delivers only what this agent may see.
///
/// This proves the server half only. Whether a client that receives one does
/// anything with it — least of all wake an idle model — is the client's
/// business and cannot be asserted here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_stream_pushes_room_activity() {
    let h = boot("sse").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let other = h.store.create_room(project.id, "elsewhere", "").unwrap();
    let (_coder, coder_key) = mk_agent(&h, project.id, room, "dev", "CODER");
    let (_scout, scout_key) = mk_agent(&h, project.id, room, "scout", "ASSISTANT");
    // A coder in the room scout is NOT in, to prove the stream is scoped.
    let (elsewhere, elsewhere_key) = h
        .store
        .create_agent(project.id, "stranger", "CODER", None, "", "")
        .unwrap();
    h.store.join_room(other, elsewhere).unwrap();

    let url = h.url.clone();
    let key = scout_key.clone();
    let reader = std::thread::spawn(move || {
        let agent = ureq::AgentBuilder::new()
            .timeout_read(std::time::Duration::from_secs(20))
            .build();
        let res = agent
            .get(&url)
            .set("Authorization", &format!("Bearer {key}"))
            .set("Accept", "text/event-stream")
            .call()
            .expect("the stream should open");
        assert_eq!(
            res.header("content-type").unwrap_or(""),
            "text/event-stream",
            "not an SSE stream"
        );
        let mut buf = String::new();
        let mut r = std::io::BufReader::new(res.into_reader());
        // Enough lines to carry one notification; the read timeout ends it.
        for _ in 0..12 {
            let mut line = String::new();
            use std::io::BufRead;
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            buf.push_str(&line);
            if buf.contains("notifications/resources/updated") {
                break;
            }
        }
        buf
    });

    tokio::time::sleep(std::time::Duration::from_millis(600)).await;

    // In another room: must not reach scout.
    let (is_err, _) = call(
        &h.url,
        &elsewhere_key,
        "create_thread",
        json!({"title": "not yours", "body": "@everyone x", "tag": "FYI"}),
    );
    assert!(!is_err);

    // In scout's room: must reach it.
    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({"title": "yours", "body": "@everyone x", "tag": "HELP_REQUEST"}),
    );
    assert!(!is_err, "{text}");

    let got = reader.join().expect("reader panicked");
    assert!(
        got.contains("notifications/resources/updated"),
        "no resource notification arrived: {got:?}"
    );
    assert!(
        got.contains("rivendell://thread/2"),
        "should name scout's thread, not the other room's: {got:?}"
    );
    assert!(
        !got.contains("rivendell://thread/1"),
        "leaked a thread from a room this agent is not in: {got:?}"
    );

    let _ = std::fs::remove_dir_all(&h.dir);
}

/// The socket: one connection held open, and Rivendell speaks when there is
/// something to say. No cursor, no repeated request, no timeout to tune.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_socket_is_told_without_asking() {
    let watcher = std::path::Path::new("../runner/target/release/rivendell-run");
    if !watcher.is_file() {
        eprintln!("skipped: build it with `cargo build --release --manifest-path runner/Cargo.toml`");
        return;
    }

    let h = boot("socket").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (_coder, coder_key) = mk_agent(&h, project.id, room, "dev", "CODER");
    let (scout, _k) = mk_agent(&h, project.id, room, "scout", "ASSISTANT");

    let log = h.dir.join("ran.txt");
    let script = h.dir.join("fake-agent");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf 'threads=%s\\n' \"$RIVENDELL_THREADS\" >> {}\n",
            log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let (token, handle) = h.store.mint_live_token(scout).unwrap();
    let mut child = std::process::Command::new(watcher)
        .args(["--key", &token, "--url", &h.url, "--ws", "--once", "--"])
        .arg(&script)
        .spawn()
        .unwrap();

    // Let the socket be established before there is anything to hear, so what
    // arrives arrives because it was pushed.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({"title": "over the wire", "body": "@everyone well?", "tag": "HELP_REQUEST"}),
    );
    assert!(!is_err, "{text}");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("nothing came down the socket");
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let ran = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(ran.contains("threads="), "the agent never ran — log was {ran:?}");

    h.store.drop_live_token(&handle);
    let _ = std::fs::remove_dir_all(&h.dir);
}

/// Work already waiting is volunteered the moment a socket connects, with no
/// event to trigger it — the case a listener that only looked forward misses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_socket_hears_what_was_already_waiting() {
    let watcher = std::path::Path::new("../runner/target/release/rivendell-run");
    if !watcher.is_file() {
        return;
    }
    let h = boot("socket-catchup").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (_coder, coder_key) = mk_agent(&h, project.id, room, "dev", "CODER");
    let (scout, _k) = mk_agent(&h, project.id, room, "scout", "ASSISTANT");

    // Opened before anything is listening. This is the whole point.
    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({"title": "nobody was connected", "body": "@everyone ?", "tag": "HELP_REQUEST"}),
    );
    assert!(!is_err, "{text}");

    let (token, handle) = h.store.mint_live_token(scout).unwrap();
    let out = std::process::Command::new(watcher)
        .args(["--key", &token, "--url", &h.url, "--ws", "--once"])
        .output()
        .expect("should exit on its own");
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("Threads needing you"),
        "should volunteer the waiting thread: {said:?} / {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    h.store.drop_live_token(&handle);
    let _ = std::fs::remove_dir_all(&h.dir);
}

/// An agent is told how to wait without asking, with a command it can actually
/// run. A path it has to guess is a path it will get wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whoami_says_how_to_be_told() {
    let h = boot("staying").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (_id, key) = mk_agent(&h, project.id, room, "scout", "ASSISTANT");

    let (is_err, text) = call(&h.url, &key, "whoami", json!({}));
    assert!(!is_err, "{text}");
    let me: Value = serde_json::from_str(&text).unwrap();
    let how = &me["staying_in_touch"];
    assert!(!how.is_null(), "whoami should say how to wait: {text}");

    // Whatever it claims, it claims about a binary that answered for itself —
    // the flags are read from `--capabilities`, not assumed to match this build.
    if how["available"].as_bool().unwrap_or(false) {
        let cmd = how["command"].as_str().unwrap_or("");
        assert!(cmd.contains("--ws"), "should hold a socket: {cmd:?}");
        assert!(cmd.contains("--once"), "should exit so the host notices: {cmd:?}");
        assert!(
            cmd.starts_with('/'),
            "must be a path the agent can run, not one it has to find: {cmd:?}"
        );
        assert!(how["how"].as_str().unwrap_or("").contains("background"));
    } else {
        // A checkout that has not built it, or one whose copy is older than the
        // app, says which and says what to do instead — rather than naming a
        // command that would be rejected.
        assert!(how["instead"].as_str().unwrap_or("").contains("wait_for_updates"));
        let why = how["why"].as_str().unwrap_or("");
        assert!(!why.is_empty(), "an agent should be told why, not just no");
    }

    let _ = std::fs::remove_dir_all(&h.dir);
}

/// A thread opened before the watcher existed still gets answered.
///
/// This is the failure that looks exactly like a broken feature from outside:
/// the agent is awake, the thread is open and waiting, and nothing ever
/// happens — because the watcher only ever looked forward from its own start.
/// Work does not stop existing because nobody was listening when it arrived.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_already_waiting_is_picked_up_on_startup() {
    let watcher = std::path::Path::new("../runner/target/release/rivendell-run");
    if !watcher.is_file() {
        eprintln!("skipped: build it with `cargo build --release --manifest-path runner/Cargo.toml`");
        return;
    }

    let h = boot("catchup").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (_coder, coder_key) = mk_agent(&h, project.id, room, "dev", "CODER");
    let (scout, _k) = mk_agent(&h, project.id, room, "scout", "ASSISTANT");

    // Opened first, with nothing watching. This is the whole point.
    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({"title": "Opened while nobody was watching", "body": "@everyone well?", "tag": "HELP_REQUEST"}),
    );
    assert!(!is_err, "could not open the thread: {text}");

    let log = h.dir.join("ran.txt");
    let script = h.dir.join("fake-agent");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf 'threads=%s\\n' \"$RIVENDELL_THREADS\" >> {}\n",
            log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let (token, handle) = h.store.mint_live_token(scout).unwrap();
    let mut child = std::process::Command::new(watcher)
        .args(["--key", &token, "--url", &h.url, "--wait", "20", "--once", "--"])
        .arg(&script)
        .spawn()
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the watcher ignored a thread that was already waiting for it");
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let ran = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(ran.contains("threads="), "the agent never ran — log was {ran:?}");

    h.store.drop_live_token(&handle);
    let _ = std::fs::remove_dir_all(&h.dir);
}

/// The whole chain, with nothing faked but the agent itself: the app mints a
/// credential, the real watcher authenticates with it, holds the long poll,
/// and starts the agent when somebody else moves a thread.
///
/// Every piece here is unit-tested on its own. This is the one test that proves
/// they are wired to each other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_watcher_starts_an_agent_when_a_thread_moves() {
    let watcher = std::path::Path::new("../runner/target/release/rivendell-run");
    if !watcher.is_file() {
        eprintln!("skipped: build it with `cargo build --release --manifest-path runner/Cargo.toml`");
        return;
    }

    let h = boot("watcher").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (_coder, coder_key) = mk_agent(&h, project.id, room, "dev", "CODER");
    let (scout, _k) = mk_agent(&h, project.id, room, "scout", "ASSISTANT");

    // Stands in for the agent CLI. It records that it ran and with what.
    let log = h.dir.join("ran.txt");
    let script = h.dir.join("fake-agent");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf 'threads=%s key=%.8s\\n' \"$RIVENDELL_THREADS\" \"$RIVENDELL_KEY\" >> {}\n",
            log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Exactly what the supervisor hands it: an ephemeral credential, never the
    // agent's own key — which the app could not read back if it wanted to.
    let (token, handle) = h.store.mint_live_token(scout).unwrap();
    let mut child = std::process::Command::new(watcher)
        .args(["--key", &token, "--url", &h.url, "--wait", "20", "--once", "--"])
        .arg(&script)
        .spawn()
        .unwrap();

    // Let it authenticate and prime its cursor before there is anything to see,
    // so what it reacts to is genuinely new.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({
            "title": "The retry loop never backs off",
            "body": "@everyone Third attempt fires immediately.",
            "tag": "HELP_REQUEST"
        }),
    );
    assert!(!is_err, "could not open the thread: {text}");

    // Generous: it has to notice, start a process, and wait for it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the watcher never started the agent");
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let ran = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        ran.contains("threads="),
        "the agent did not run — log was {ran:?}"
    );
    assert!(
        ran.contains("key=rvdlive"),
        "it should hold the ephemeral credential, not a stored key: {ran:?}"
    );

    h.store.drop_live_token(&handle);
    let _ = std::fs::remove_dir_all(&h.dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_supervised_run_authenticates_and_can_be_cut_off() {
    let h = boot("live-token").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (agent_id, _key) = mk_agent(&h, project.id, room, "scout", "ASSISTANT");

    // What the supervisor hands a process it starts. An agent's own key is
    // unrecoverable — only its digest was ever stored — so this is the only way
    // the app can authenticate something it launched itself.
    let (token, handle) = h.store.mint_live_token(agent_id).unwrap();
    let (code, body) = rpc(
        &h.url,
        Some(&token),
        "tools/call",
        json!({"name":"whoami","arguments":{}}),
    );
    assert_eq!(code, 200, "a live token must authenticate");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("scout"), "it is the same identity: {text}");

    // Revoking has to reach what is already running as that agent, not just
    // refuse the next connection.
    h.store.set_agent_revoked(agent_id, true).unwrap();
    let (code, _) = rpc(&h.url, Some(&token), "initialize", json!({}));
    assert_eq!(code, 401, "revoking must reach a run already in flight");

    h.store.drop_live_token(&handle);
    let _ = std::fs::remove_dir_all(&h.dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_token_cannot_come_back_as_a_different_agent() {
    let h = boot("rowid-reuse").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (doomed, _k) = mk_agent(&h, project.id, room, "leaving", "ASSISTANT");
    let (token, _handle) = h.store.mint_live_token(doomed).unwrap();

    // `agents.id` is a bare rowid, not AUTOINCREMENT, and deletion is real, so
    // the next agent created can inherit the number. A token that remembered
    // only the id would wake up as somebody else — different role, different
    // project, different jail root.
    h.store.delete_agent(doomed).unwrap();
    let (reborn, _k) = mk_agent(&h, project.id, room, "arriving", "CODER");
    assert_eq!(reborn, doomed, "pointless unless the id really was reused");

    let (code, _) = rpc(&h.url, Some(&token), "initialize", json!({}));
    assert_eq!(
        code, 401,
        "a token for a deleted agent must not resolve to its replacement"
    );

    let _ = std::fs::remove_dir_all(&h.dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_keys_stop_working() {
    let h = boot("revoke").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (agent_id, key) = mk_agent(&h, project.id, room, "main", "CODER");

    let (code, _) = rpc(&h.url, Some(&key), "initialize", json!({}));
    assert_eq!(code, 200);

    h.store.set_agent_revoked(agent_id, true).unwrap();
    let (code, _) = rpc(&h.url, Some(&key), "initialize", json!({}));
    assert_eq!(code, 401, "a revoked key must stop working immediately");

    // Rotating is not "show me the key again" — it mints a new one and the old
    // one dies with it, which is why the UI asks before doing it.
    h.store.set_agent_revoked(agent_id, false).unwrap();
    let fresh = h.store.rotate_key(agent_id).unwrap();
    assert_ne!(fresh, key, "rotation must produce a different key");
    let (code, _) = rpc(&h.url, Some(&fresh), "initialize", json!({}));
    assert_eq!(code, 200);
    let (code, _) = rpc(&h.url, Some(&key), "initialize", json!({}));
    assert_eq!(code, 401, "the superseded key must no longer authenticate");

    let _ = std::fs::remove_dir_all(&h.dir);
}
