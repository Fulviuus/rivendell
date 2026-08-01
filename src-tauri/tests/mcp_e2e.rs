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
fn backdate_thread(dir: &std::path::Path, thread_id: i64, seconds: i64) {
    let when = (chrono_now() - seconds).to_string();
    let conn = rusqlite::Connection::open(dir.join(".db/rivendell.db")).unwrap();
    conn.execute(
        "UPDATE threads SET created_at=?1 WHERE id=?2",
        rusqlite::params![iso(when.parse().unwrap()), thread_id],
    )
    .unwrap();
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

    let (_, coder_key) = h
        .store
        .create_agent(room, "main", "CODER", Some(external), "", "")
        .unwrap();
    let (assistant_id, assistant_key) = h
        .store
        .create_agent(room, "skeptic", "ASSISTANT", Some(external), "", "")
        .unwrap();

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
    assert!(text.contains("REFUTE"), "tag instruction should be surfaced");

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

/// A tag whose default quorum exceeds the number of assistants in the room must
/// not strand the thread. Before this was clamped, a lone assistant replying to
/// an ADVERSARIAL_REVIEW (quorum 2) left the thread in AWAITING_REPLIES for
/// ever and it never told the coder it was their turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quorum_cannot_exceed_available_assistants() {
    let h = boot("quorum").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (_, coder_key) = h
        .store
        .create_agent(room, "main", "CODER", None, "", "")
        .unwrap();
    // Exactly one assistant, against a tag that asks for two.
    let (_, only_key) = h
        .store
        .create_agent(room, "solo", "ASSISTANT", None, "", "")
        .unwrap();

    let (is_err, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({"title": "Review this", "body": "…", "tag": "ADVERSARIAL_REVIEW"}),
    );
    assert!(!is_err, "{text}");
    let thread_id: i64 = text
        .split_whitespace()
        .find_map(|w| w.trim_end_matches('.').parse().ok())
        .unwrap();

    let stored = h.store.thread_detail(thread_id).unwrap();
    assert_eq!(
        stored.summary.quorum, 1,
        "quorum should be clamped to the one assistant that can answer"
    );

    let (is_err, text) = call(
        &h.url,
        &only_key,
        "reply",
        json!({"thread_id": thread_id, "body": "Found a race.", "verdict": "CONFIRMED"}),
    );
    assert!(!is_err, "{text}");

    let after = h.store.thread_detail(thread_id).unwrap();
    assert_eq!(
        after.summary.status, "NEEDS_CODER",
        "one reply from the only assistant must hand the thread back, not wait for a second"
    );

    let _ = std::fs::remove_dir_all(&h.dir);
}

/// The room's policy decides the default, and a per-thread argument overrides
/// it — both still clamped to who can actually answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn room_quorum_policy_is_configurable() {
    let h = boot("policy").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (_, coder_key) = h
        .store
        .create_agent(room, "main", "CODER", None, "", "")
        .unwrap();
    for n in ["a", "b", "c"] {
        h.store
            .create_agent(room, n, "ASSISTANT", None, "", "")
            .unwrap();
    }

    let open = |body: serde_json::Value| -> i64 {
        let (is_err, text) = call(&h.url, &coder_key, "create_thread", body);
        assert!(!is_err, "{text}");
        text.split_whitespace()
            .find_map(|w| w.trim_end_matches('.').parse().ok())
            .unwrap()
    };

    // Default policy is "all": three assistants, quorum three.
    let t1 = open(json!({"title": "one", "body": "…", "tag": "HELP_REQUEST"}));
    assert_eq!(h.store.thread_detail(t1).unwrap().summary.quorum, 3);

    // Switch the room to a fixed number.
    h.store
        .update_room(room, json!({"quorum_mode": "fixed", "quorum_fixed": 2}))
        .unwrap();
    let t2 = open(json!({"title": "two", "body": "…", "tag": "HELP_REQUEST"}));
    assert_eq!(h.store.thread_detail(t2).unwrap().summary.quorum, 2);

    // An explicit per-thread value beats the room policy.
    let t3 = open(json!({"title": "three", "body": "…", "tag": "HELP_REQUEST", "quorum": 1}));
    assert_eq!(h.store.thread_detail(t3).unwrap().summary.quorum, 1);

    // Asking for more than exist is clamped, never stranded.
    let t4 = open(json!({"title": "four", "body": "…", "tag": "HELP_REQUEST", "quorum": 99}));
    assert_eq!(h.store.thread_detail(t4).unwrap().summary.quorum, 3);

    // Mentioning one agent narrows the pool, and the quorum with it.
    let solo = h.store.list_agents(Some(room)).unwrap();
    let solo = solo.iter().find(|a| a.name == "a").unwrap().id;
    let t5 = open(json!({"title": "five", "body": "…", "tag": "HELP_REQUEST", "mentions": [solo]}));
    assert_eq!(h.store.thread_detail(t5).unwrap().summary.quorum, 1);

    // FYI still opts out of replies entirely.
    let t6 = open(json!({"title": "six", "body": "…", "tag": "FYI"}));
    assert_eq!(h.store.thread_detail(t6).unwrap().summary.quorum, 0);

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
    let (coder_id, _) = h
        .store
        .create_agent(room, "main", "CODER", None, "", "")
        .unwrap();
    let (asst_id, _) = h
        .store
        .create_agent(room, "helper", "ASSISTANT", None, "", "")
        .unwrap();
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
                    quorum: Some(0),
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
    let (_, coder_key) = h
        .store
        .create_agent(room, "main", "CODER", None, "", "")
        .unwrap();
    let (_, asst_key) = h
        .store
        .create_agent(room, "helper", "ASSISTANT", None, "", "")
        .unwrap();

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

    assert_eq!(
        h.store.thread_detail(thread_id).unwrap().summary.status,
        "NEEDS_CODER"
    );

    let _ = std::fs::remove_dir_all(&h.dir);
}

/// An assistant that never shows up must not hold a thread open for ever, and
/// one that claims must keep its slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silent_assistant_stops_being_waited_for() {
    let h = boot("timeout").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (_, coder_key) = h
        .store
        .create_agent(room, "main", "CODER", None, "", "")
        .unwrap();
    let (_, present_key) = h
        .store
        .create_agent(room, "present", "ASSISTANT", None, "", "")
        .unwrap();
    let (_, slow_key) = h
        .store
        .create_agent(room, "slow", "ASSISTANT", None, "", "")
        .unwrap();
    // Third assistant that will never connect at all.
    h.store
        .create_agent(room, "absent", "ASSISTANT", None, "", "")
        .unwrap();

    let (_, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({"title": "Three-way", "body": "…", "tag": "HELP_REQUEST"}),
    );
    let thread_id: i64 = text
        .split_whitespace()
        .find_map(|w| w.trim_end_matches('.').parse().ok())
        .unwrap();
    assert_eq!(
        h.store.thread_detail(thread_id).unwrap().summary.quorum,
        3,
        "default is every connected assistant"
    );

    // `slow` says it is on it; `absent` says nothing.
    let (is_err, text) = call(
        &h.url,
        &slow_key,
        "claim_thread",
        json!({"thread_id": thread_id, "note": "reproducing locally"}),
    );
    assert!(!is_err, "{text}");
    let d = h.store.thread_detail(thread_id).unwrap();
    assert_eq!(d.claims.len(), 1);
    assert_eq!(d.claims[0].note, "reproducing locally");

    // One reply, inside the grace window: still waiting on the other two.
    let (is_err, text) = call(
        &h.url,
        &present_key,
        "reply",
        json!({"thread_id": thread_id, "body": "one", "verdict": "ANSWERED"}),
    );
    assert!(!is_err, "{text}");
    assert_eq!(
        h.store.thread_detail(thread_id).unwrap().summary.status,
        "AWAITING_REPLIES",
        "inside the window an agent that has not spoken may still turn up"
    );

    // Age the thread past its window. `absent` never claimed and never
    // replied, so it stops counting; `slow` holds its slot because its claim
    // is recent.
    backdate_thread(&h.dir, thread_id, 600);
    let swept = h.store.sweep_stalled_threads().unwrap();
    assert_eq!(swept, 0, "still waiting on the assistant that claimed");
    assert_eq!(
        h.store.thread_detail(thread_id).unwrap().summary.status,
        "AWAITING_REPLIES"
    );

    // Once the claimant answers too, nobody is left to wait for.
    let (is_err, text) = call(
        &h.url,
        &slow_key,
        "reply",
        json!({"thread_id": thread_id, "body": "two", "verdict": "ANSWERED"}),
    );
    assert!(!is_err, "{text}");
    assert_eq!(
        h.store.thread_detail(thread_id).unwrap().summary.status,
        "NEEDS_CODER",
        "the absent assistant must not keep the thread open"
    );

    let _ = std::fs::remove_dir_all(&h.dir);
}

/// A thread nobody picks up at all still comes back to the coder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_thread_nobody_touches_comes_back() {
    let h = boot("nobody").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room = h.store.create_room(project.id, "general", "").unwrap();
    let (_, coder_key) = h
        .store
        .create_agent(room, "main", "CODER", None, "", "")
        .unwrap();
    h.store
        .create_agent(room, "ghost", "ASSISTANT", None, "", "")
        .unwrap();

    let (_, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({"title": "Anyone?", "body": "…", "tag": "HELP_REQUEST"}),
    );
    let thread_id: i64 = text
        .split_whitespace()
        .find_map(|w| w.trim_end_matches('.').parse().ok())
        .unwrap();

    // Within the window the sweep leaves it alone.
    assert_eq!(h.store.sweep_stalled_threads().unwrap(), 0);
    assert_eq!(
        h.store.thread_detail(thread_id).unwrap().summary.status,
        "AWAITING_REPLIES"
    );

    backdate_thread(&h.dir, thread_id, 600);
    assert_eq!(h.store.sweep_stalled_threads().unwrap(), 1);
    let d = h.store.thread_detail(thread_id).unwrap();
    assert_eq!(d.summary.status, "NEEDS_CODER");
    assert_eq!(d.summary.reply_count, 0, "handed back with nothing, not silently resolved");

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
    let (coder_id, _) = h
        .store
        .create_agent(room, "main", "CODER", None, "", "")
        .unwrap();
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
                    quorum: Some(0),
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
    let (_, coder_key) = h
        .store
        .create_agent(room, "main", "CODER", None, "", "")
        .unwrap();
    let (_, asst_key) = h
        .store
        .create_agent(room, "helper", "ASSISTANT", None, "", "")
        .unwrap();

    let (_, text) = call(
        &h.url,
        &coder_key,
        "create_thread",
        json!({"title": "Which one", "body": "…", "tag": "ADVERSARIAL_REVIEW", "quorum": 1}),
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
    assert_eq!(d.messages[0].verdict.as_deref(), Some("REFUTED"));
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
    assert_eq!(edit["payload"]["verdict"], "REFUTED");

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
    let (coder_id, _) = h
        .store
        .create_agent(room, "main", "CODER", None, "", "")
        .unwrap();
    let (_, asst_key) = h
        .store
        .create_agent(room, "helper", "ASSISTANT", None, "", "")
        .unwrap();
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
                quorum: Some(1),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rooms_are_isolated() {
    let h = boot("isolation").await;
    let project = h
        .store
        .create_project("demo", h.dir.to_str().unwrap())
        .unwrap();
    let room_a = h.store.create_room(project.id, "alpha", "").unwrap();
    let room_b = h.store.create_room(project.id, "beta", "").unwrap();

    let (_, coder_a) = h
        .store
        .create_agent(room_a, "a-coder", "CODER", None, "", "")
        .unwrap();
    let (_, coder_b) = h
        .store
        .create_agent(room_b, "b-coder", "CODER", None, "", "")
        .unwrap();

    let (is_err, text) = call(
        &h.url,
        &coder_a,
        "create_thread",
        json!({"title": "secret alpha work", "body": "…", "tag": "FYI"}),
    );
    assert!(!is_err, "{text}");
    let thread_id: i64 = text
        .split_whitespace()
        .find_map(|w| w.trim_end_matches('.').parse().ok())
        .unwrap();

    let (is_err, text) = call(&h.url, &coder_b, "get_thread", json!({"thread_id": thread_id}));
    assert!(is_err, "room B must not read room A's thread: {text}");

    let (_, text) = call(&h.url, &coder_b, "list_threads", json!({"status": "all"}));
    assert!(
        !text.contains("secret alpha work"),
        "room B's listing leaked room A: {text}"
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
    let (agent_id, key) = h
        .store
        .create_agent(room, "main", "CODER", None, "", "")
        .unwrap();

    let (code, _) = rpc(&h.url, Some(&key), "initialize", json!({}));
    assert_eq!(code, 200);

    h.store.set_agent_revoked(agent_id, true).unwrap();
    let (code, _) = rpc(&h.url, Some(&key), "initialize", json!({}));
    assert_eq!(code, 401, "a revoked key must stop working immediately");

    // Rotating issues a working key and retires the old one.
    h.store.set_agent_revoked(agent_id, false).unwrap();
    let fresh = h.store.rotate_key(agent_id).unwrap();
    let (code, _) = rpc(&h.url, Some(&fresh), "initialize", json!({}));
    assert_eq!(code, 200);
    let (code, _) = rpc(&h.url, Some(&key), "initialize", json!({}));
    assert_eq!(code, 401, "the superseded key must no longer authenticate");

    let _ = std::fs::remove_dir_all(&h.dir);
}
