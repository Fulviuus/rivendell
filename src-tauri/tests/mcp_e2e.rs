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
    assert!(
        !asst_tools.contains(&"create_thread"),
        "assistants must not be offered create_thread"
    );

    // An assistant calling a coder-only tool is refused even if it guesses the name.
    let (is_err, text) = call(
        &h.url,
        &assistant_key,
        "create_thread",
        json!({"title": "sneaky", "body": "x", "tag": "FYI"}),
    );
    assert!(is_err, "assistant must not be able to open a thread: {text}");

    // --- create a thread with pinned context ------------------------------
    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({
            "title": "Token refresh races",
            "body": "Two concurrent 401s both trigger a refresh.",
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

    // --- verdict is enforced ----------------------------------------------
    let (is_err, text) = call(
        &h.url,
        &assistant_key,
        "reply",
        json!({"thread_id": thread_id, "body": "Looks fine to me."}),
    );
    assert!(is_err, "a verdict-requiring tag must reject a bare reply");
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
                    body: "…".into(),
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

/// Both roles drive the same loop over the same event log; only their
/// permissions differ. A coder opening a thread must be visible to a waiting
/// assistant, and that assistant's reply visible back to the coder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_roles_share_one_loop() {
    let h = boot("oneloop").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (_, coder_key) = mk_agent(&h, project.id, room, "main", "CODER");
    let (_, asst_key) = mk_agent(&h, project.id, room, "helper", "ASSISTANT");

    // Both roles are offered the same waiting primitive.
    for key in [&coder_key, &asst_key] {
        let (_, body) = rpc(&h.url, Some(key), "tools/list", json!({}));
        let names: Vec<&str> = body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"wait_for_updates"));
        // Nothing launches anything any more.
        assert!(!names.contains(&"dispatch"), "dispatch should be gone: {names:?}");
    }

    // The assistant parks on the cursor it has now.
    let (_, before) = call(&h.url, &asst_key, "wait_for_updates", json!({"timeout_s": 1}));
    let cursor = serde_json::from_str::<Value>(&before).unwrap()["next_cursor"]
        .as_i64()
        .unwrap();

    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({"title": "Look at this", "body": "…", "tag": "HELP_REQUEST"}),
    );
    assert!(!is_err, "{text}");
    let thread_id: i64 = text
        .split_whitespace()
        .find_map(|w| w.trim_end_matches('.').parse().ok())
        .unwrap();

    // …and sees it without anything having been spawned.
    let (_, seen) = call(
        &h.url,
        &asst_key,
        "wait_for_updates",
        json!({"cursor": cursor, "timeout_s": 5}),
    );
    let seen: Value = serde_json::from_str(&seen).unwrap();
    assert!(
        seen["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "thread.created"),
        "a waiting assistant must see the new thread: {seen}"
    );

    // The coder waits the same way and sees the answer come back.
    let coder_cursor = seen["next_cursor"].as_i64().unwrap() - 1;
    let (is_err, text) = call(
        &h.url,
        &asst_key,
        "reply",
        json!({"thread_id": thread_id, "body": "Here you go.", "verdict": "ANSWERED"}),
    );
    assert!(!is_err, "{text}");

    let (_, got) = call(
        &h.url,
        &coder_key,
        "wait_for_updates",
        json!({"cursor": coder_cursor, "timeout_s": 5}),
    );
    let got: Value = serde_json::from_str(&got).unwrap();
    assert!(
        got["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "message.created"),
        "the coder must see the reply: {got}"
    );

    // The reply does not hand over immediately any more: it opens the window in
    // which any other agent may claim. Only once that closes with nothing
    // outstanding does the thread come back.
    assert_eq!(
        h.store.thread_detail(thread_id).unwrap().summary.status,
        "AWAITING_REPLIES"
    );
    age_gather(&h.dir, thread_id, 3600);
    h.store.sweep_stalled_threads().unwrap();
    assert_eq!(
        h.store.thread_detail(thread_id).unwrap().summary.status,
        "NEEDS_CODER"
    );

    let _ = std::fs::remove_dir_all(&h.dir);
}

/// The four filter buckets must partition every status: anything that falls
/// through all of them is a thread you can never find again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_filter_buckets_cover_everything() {
    let h = boot("buckets").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (coder_id, _) = mk_agent(&h, project.id, room, "main", "CODER");
    let coder = h.store.agent_ctx(coder_id).unwrap();

    let open = |title: &str| {
        h.store
            .create_thread(
                &coder,
                rivendell_lib::models::NewThread {
                    room_id: room,
                    title: title.into(),
                    body: "…".into(),
                    tag: "FYI".into(),
                    mentions: vec![],
                    context: vec![],
                    include_diff: false,
                },
            )
            .unwrap()
    };

    // One thread in each of the six internal statuses.
    let t_open = open("open");
    let t_awaiting = open("awaiting");
    let t_needs = open("needs");
    let t_resolved = open("resolved");
    let t_wontfix = open("wontfix");
    let t_blocked = open("blocked");

    h.store.set_thread_status(&coder, t_awaiting, "AWAITING_REPLIES").unwrap();
    h.store.set_thread_status(&coder, t_needs, "NEEDS_CODER").unwrap();
    h.store.resolve_thread(&coder, t_resolved, "done", "RESOLVED").unwrap();
    h.store.resolve_thread(&coder, t_wontfix, "no", "WONTFIX").unwrap();
    h.store.resolve_thread(&coder, t_blocked, "waiting on upstream", "BLOCKED").unwrap();

    let ids = |bucket: &str| -> Vec<i64> {
        let mut v: Vec<i64> = h
            .store
            .list_threads(Some(room), Some(bucket), None, None, None, 50)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        v.sort();
        v
    };

    let mut live = vec![t_open, t_awaiting, t_needs];
    live.sort();
    assert_eq!(ids("open"), live, "Open is live work only");

    let mut done = vec![t_resolved, t_wontfix];
    done.sort();
    assert_eq!(ids("resolved"), done, "Resolved covers WONTFIX too");

    assert_eq!(ids("blocked"), vec![t_blocked]);

    let mut everything = vec![t_open, t_awaiting, t_needs, t_resolved, t_wontfix, t_blocked];
    everything.sort();
    assert_eq!(ids("all"), everything);

    // Together the three narrow buckets account for every thread — nothing is
    // reachable only through "All".
    let mut union = [ids("open"), ids("resolved"), ids("blocked")].concat();
    union.sort();
    assert_eq!(union, everything, "the buckets must partition, not merely overlap");

    // The room badge counts exactly what the Open filter shows.
    let rooms = h.store.list_rooms().unwrap();
    let badge = rooms.iter().find(|r| r.id == room).unwrap().open_threads;
    assert_eq!(
        badge,
        live.len() as i64,
        "a badge that disagrees with the list is the confusion we removed"
    );

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
        json!({"title": "Which one", "body": "…", "tag": "ADVERSARIAL_REVIEW"}),
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
                body: "…".into(),
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

/// Nothing times out before a single agent has spoken. A question with no
/// takers is not a failure, and a thread that quietly gave up on itself would
/// be worse than one that waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_thread_waits_indefinitely_for_its_first_answer() {
    let h = boot("waitforever").await;
    let (room, coder_key, _a, _b) = seed_room(&h).await;
    // Windows so short that anything time-based would fire immediately.
    h.store
        .update_room(room, json!({"claim_window_secs": 0, "response_timeout_secs": 0}))
        .unwrap();

    let id = open_thread(&h, &coder_key, "Anyone?");
    for _ in 0..3 {
        assert_eq!(h.store.sweep_stalled_threads().unwrap(), 0);
    }
    assert_eq!(
        h.store.thread_detail(id).unwrap().summary.status,
        "AWAITING_REPLIES",
        "with nobody having answered, the thread must keep waiting"
    );

    let _ = std::fs::remove_dir_all(&h.dir);
}

/// The first answer starts the clock. Agents that stay silent through the
/// window are simply left out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_answer_opens_a_window_for_the_others() {
    let h = boot("window").await;
    let (room, coder_key, first, _silent) = seed_room(&h).await;
    h.store.update_room(room, json!({"claim_window_secs": 600})).unwrap();

    let id = open_thread(&h, &coder_key, "Two of you");
    let (is_err, text) = call(
        &h.url,
        &first,
        "reply",
        json!({"thread_id": id, "body": "mine", "verdict": "ANSWERED"}),
    );
    assert!(!is_err, "{text}");

    // Inside the window the other agent may still put its hand up.
    assert_eq!(
        h.store.thread_detail(id).unwrap().summary.status,
        "AWAITING_REPLIES"
    );
    assert_eq!(h.store.sweep_stalled_threads().unwrap(), 0);

    // Once it closes and nobody claimed, the coder gets it.
    age_gather(&h.dir, id, 3600);
    assert_eq!(h.store.sweep_stalled_threads().unwrap(), 1);
    assert_eq!(
        h.store.thread_detail(id).unwrap().summary.status,
        "NEEDS_CODER"
    );

    let _ = std::fs::remove_dir_all(&h.dir);
}

/// Claiming inside the window buys you the time to answer; going quiet loses
/// it, so one stalled agent cannot hold a thread open for ever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_claim_holds_the_thread_and_a_stale_one_does_not() {
    let h = boot("claims2").await;
    let (room, coder_key, first, second) = seed_room(&h).await;
    h.store
        .update_room(room, json!({"claim_window_secs": 600, "response_timeout_secs": 600}))
        .unwrap();

    let id = open_thread(&h, &coder_key, "Both of you");
    call(&h.url, &first, "reply", json!({"thread_id": id, "body": "one", "verdict": "ANSWERED"}));
    call(&h.url, &second, "claim_thread", json!({"thread_id": id, "note": "digging"}));

    // Window closed, but the claimant is still live.
    age_gather(&h.dir, id, 3600);
    assert_eq!(
        h.store.sweep_stalled_threads().unwrap(),
        0,
        "a live claim must keep the thread open"
    );
    assert_eq!(
        h.store.thread_detail(id).unwrap().summary.in_progress,
        1,
        "and be reported as in progress"
    );

    // Its answer releases the thread.
    let (is_err, text) = call(
        &h.url,
        &second,
        "reply",
        json!({"thread_id": id, "body": "two", "verdict": "ANSWERED"}),
    );
    assert!(!is_err, "{text}");
    let d = h.store.thread_detail(id).unwrap();
    assert_eq!(d.summary.status, "NEEDS_CODER");
    assert_eq!(d.summary.in_progress, 0);

    let _ = std::fs::remove_dir_all(&h.dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_that_goes_quiet_is_discarded() {
    let h = boot("stale").await;
    let (room, coder_key, first, second) = seed_room(&h).await;
    h.store
        .update_room(room, json!({"claim_window_secs": 600, "response_timeout_secs": 300}))
        .unwrap();

    let id = open_thread(&h, &coder_key, "One will vanish");
    call(&h.url, &first, "reply", json!({"thread_id": id, "body": "one", "verdict": "ANSWERED"}));
    call(&h.url, &second, "claim_thread", json!({"thread_id": id}));

    age_gather(&h.dir, id, 3600);
    assert_eq!(h.store.sweep_stalled_threads().unwrap(), 0, "still live");

    // The claimant stops heartbeating.
    let agents = h.store.list_agents(Some(room)).unwrap();
    let second_id = agents.iter().find(|a| a.name == "second").unwrap().id;
    backdate(&h.dir, "claimed_at", "thread_claims", second_id, 3600);

    assert_eq!(h.store.sweep_stalled_threads().unwrap(), 1);
    let d = h.store.thread_detail(id).unwrap();
    assert_eq!(d.summary.status, "NEEDS_CODER");
    assert_eq!(d.summary.in_progress, 0, "the stalled claim is no longer counted");
    assert_eq!(d.summary.responder_count, 1, "and only the real answer counts");

    let _ = std::fs::remove_dir_all(&h.dir);
}

/// An agent calling in another with @name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_at_mention_calls_another_agent_in() {
    let h = boot("atmention").await;
    let (room, coder_key, first, second) = seed_room(&h).await;
    h.store.update_room(room, json!({"claim_window_secs": 0})).unwrap();

    let id = open_thread(&h, &coder_key, "Needs a specialist");
    assert!(
        h.store.thread_detail(id).unwrap().mentions.is_empty(),
        "opens addressed to the whole room"
    );

    // `second` parks on a cursor so we can see it being told.
    let (_, before) = call(&h.url, &second, "wait_for_updates", json!({"timeout_s": 1}));
    let cursor = serde_json::from_str::<Value>(&before).unwrap()["next_cursor"]
        .as_i64()
        .unwrap();

    let (is_err, text) = call(
        &h.url,
        &first,
        "reply",
        json!({
            "thread_id": id,
            "body": "Crypto is not my area — @second can you take the signature check?",
            "verdict": "NEEDS_INFO"
        }),
    );
    assert!(!is_err, "{text}");

    let agents = h.store.list_agents(Some(room)).unwrap();
    let second_id = agents.iter().find(|a| a.name == "second").unwrap().id;
    assert_eq!(
        h.store.thread_detail(id).unwrap().mentions,
        vec![second_id],
        "the named agent is now addressed by the thread"
    );

    let (_, got) = call(
        &h.url,
        &second,
        "wait_for_updates",
        json!({"cursor": cursor, "timeout_s": 5}),
    );
    let got: Value = serde_json::from_str(&got).unwrap();
    let called = got["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "thread.mentioned")
        .expect("the called agent must be notified");
    assert_eq!(called["payload"]["called"][0], "second");

    // Being called in reopens the window, so arriving late is not the same as
    // being ignored.
    assert_eq!(
        h.store.thread_detail(id).unwrap().summary.status,
        "AWAITING_REPLIES"
    );

    // An @word that is not an agent is just prose.
    call(
        &h.url,
        &second,
        "reply",
        json!({"thread_id": id, "body": "see @nobody and user@example.com", "verdict": "ANSWERED"}),
    );
    assert_eq!(
        h.store.thread_detail(id).unwrap().mentions.len(),
        1,
        "unknown @words and email addresses must not summon anyone"
    );

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
        json!({"title": title, "body": "…", "tag": "HELP_REQUEST"}),
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
            json!({"room": room, "title": format!("in {room}"), "body": "…", "tag": "HELP_REQUEST"}),
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

/// Closing and resolving are different acts. Only a decision writes a record,
/// and resolving cannot be reached by the route that skips the summary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_is_not_resolving() {
    let h = boot("closing").await;
    let (_room, coder_key, first, _second) = seed_room(&h).await;

    let closed = open_thread(&h, &coder_key, "Never mind");
    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "set_thread_status",
        json!({"thread_id": closed, "status": "WONTFIX"}),
    );
    assert!(!is_err, "{text}");
    let d = h.store.thread_detail(closed).unwrap();
    assert_eq!(d.summary.status, "WONTFIX");
    assert!(
        d.export_path.is_none(),
        "closing writes no decision record — there was no decision"
    );

    // Reopening puts it back in front of the assistants.
    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "set_thread_status",
        json!({"thread_id": closed, "status": "AWAITING_REPLIES"}),
    );
    assert!(!is_err, "{text}");
    let (is_err, text) = call(
        &h.url,
        &first,
        "reply",
        json!({"thread_id": closed, "body": "back on it", "verdict": "ANSWERED"}),
    );
    assert!(!is_err, "a reopened thread must accept replies again: {text}");

    // Resolving cannot be reached without a summary.
    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "set_thread_status",
        json!({"thread_id": closed, "status": "RESOLVED"}),
    );
    assert!(is_err, "status must not be a back door around the record: {text}");
    assert!(text.contains("resolve_thread"), "and should say what to use: {text}");

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
        json!({"title": "Break this", "body": "…", "tag": "ADVERSARIAL_REVIEW"}),
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
        json!({"room": "alpha", "title": "secret alpha work", "body": "…", "tag": "FYI"}),
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
        json!({"title": "who answered", "body": "?", "tag": "HELP_REQUEST"}),
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
        json!({"title": "not yours", "body": "x", "tag": "FYI"}),
    );
    assert!(!is_err);

    // In scout's room: must reach it.
    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({"title": "yours", "body": "x", "tag": "HELP_REQUEST"}),
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
        json!({"title": "Opened while nobody was watching", "body": "well?", "tag": "HELP_REQUEST"}),
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
            "body": "Third attempt fires immediately.",
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
