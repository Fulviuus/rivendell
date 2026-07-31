//! End-to-end exercise of the MCP surface over real HTTP: auth, tool dispatch,
//! verdict enforcement, the reply cap, room isolation and the export on resolve.

use rivendell_lib::mcp::server::{serve, McpState};
use rivendell_lib::spawner::Spawner;
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
    let spawner = Arc::new(Spawner::new(store.clone()));
    let state = Arc::new(McpState {
        store: store.clone(),
        spawner,
    });
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
        .create_agent(room, "main", "CODER", Some(external), "", false, "")
        .unwrap();
    let (assistant_id, assistant_key) = h
        .store
        .create_agent(room, "skeptic", "ASSISTANT", Some(external), "", false, "")
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
        .create_agent(room_a, "a-coder", "CODER", None, "", false, "")
        .unwrap();
    let (_, coder_b) = h
        .store
        .create_agent(room_b, "b-coder", "CODER", None, "", false, "")
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
        .create_agent(room, "main", "CODER", None, "", false, "")
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
