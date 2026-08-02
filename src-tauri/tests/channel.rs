//! The channel, end to end: a real Rivendell, the real stdio bridge, and a
//! stand-in for the host that spawns it.
//!
//! What this proves is the server half — that a thread needing this agent
//! becomes a correctly-shaped `notifications/claude/channel` on the bridge's
//! stdout, unprompted. What happens to it after that belongs to the host and
//! cannot be asserted from here.

use rivendell_lib::mcp::server::{serve, McpState};
use rivendell_lib::store::Store;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;

fn bridge() -> std::path::PathBuf {
    // Built by rivendell.sh; skipped rather than failed if it is not there.
    std::path::Path::new("../mcp-shim/target/release/rivendell-mcp").to_path_buf()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn activity_arrives_as_a_channel_event_without_being_asked() {
    if !bridge().is_file() {
        eprintln!("skipped: cargo build --release --manifest-path mcp-shim/Cargo.toml");
        return;
    }

    let dir = std::env::temp_dir().join(format!("rivendell-channel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(Store::open(&dir.join("db.sqlite")).unwrap());
    let running = serve(Arc::new(McpState { store: store.clone() }), 0)
        .await
        .unwrap();

    let project = store.create_project("demo", dir.to_str().unwrap()).unwrap();
    let room = store.create_room(project.id, "general", "").unwrap();
    let (coder, _ck) = store
        .create_agent(project.id, "dev", "CODER", None, "", "")
        .unwrap();
    store.join_room(room, coder).unwrap();
    let (scout, scout_key) = store
        .create_agent(project.id, "scout", "ASSISTANT", None, "", "")
        .unwrap();
    store.join_room(room, scout).unwrap();

    // Stand in for the host: spawn the bridge, speak initialize, then listen.
    let mut child = std::process::Command::new(bridge())
        .env("RIVENDELL_URL", &running.url)
        .env("RIVENDELL_KEY", &scout_key)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut init = String::new();
    out.read_line(&mut init).unwrap();
    let init: Value = serde_json::from_str(&init).expect("initialize reply");
    assert_eq!(
        init["result"]["capabilities"]["experimental"]["claude/channel"],
        json!({}),
        "without the capability the host never registers a listener"
    );

    // Nothing further is sent on stdin. Anything that arrives now arrived
    // because the workspace pushed it, which is the entire point.
    let ctx = store.agent_ctx(coder).unwrap();
    let thread = store
        .create_thread(
            &ctx,
            rivendell_lib::models::NewThread {
                room_id: room,
                title: "does this reach you on its own".into(),
                body: "well?".into(),
                tag: "HELP_REQUEST".into(),
                mentions: vec![],
                context: vec![],
                include_diff: false,
            },
        )
        .unwrap();

    // Read until a channel event turns up or the reader gives out.
    let found = tokio::task::spawn_blocking(move || {
        for _ in 0..10 {
            let mut line = String::new();
            if out.read_line(&mut line).unwrap_or(0) == 0 {
                return None;
            }
            if line.contains("notifications/claude/channel") {
                return serde_json::from_str::<Value>(&line).ok();
            }
        }
        None
    })
    .await
    .unwrap();

    let _ = child.kill();
    let ev = found.expect("no channel event arrived");

    assert_eq!(ev["method"], "notifications/claude/channel");
    let content = ev["params"]["content"].as_str().unwrap_or("");
    assert!(
        content.contains(&format!("get_thread({thread})")),
        "should tell the agent what to do: {content:?}"
    );
    assert_eq!(ev["params"]["meta"]["thread"], thread.to_string());
    // Keys with anything but letters, digits and underscores are dropped by the
    // host in silence.
    for k in ev["params"]["meta"].as_object().unwrap().keys() {
        assert!(
            k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "meta key {k:?} would be dropped"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
