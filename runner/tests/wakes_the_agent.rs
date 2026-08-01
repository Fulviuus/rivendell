//! Drives the real binary against a stand-in Rivendell.
//!
//! The unit tests cover which events count. This covers the wiring around them:
//! that it authenticates, primes its cursor so it does not replay history, and
//! actually runs the command with the thread ids filled in.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;

/// Answers `whoami`, then one empty poll, then one poll with work.
fn fake_rivendell() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());

    let handle = std::thread::spawn(move || {
        let mut poll = 0;
        for stream in listener.incoming().take(3) {
            let mut stream = stream.unwrap();
            let body = read_request(&mut stream);
            let req: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(req["params"]["name"].as_str().is_some(), true);

            let payload = match req["params"]["name"].as_str().unwrap() {
                "whoami" => serde_json::json!({
                    "agent_id": 7, "name": "scout",
                    "rooms": [{ "id": 1, "name": "general" }]
                }),
                "wait_for_updates" => {
                    poll += 1;
                    if poll == 1 {
                        // The priming call: hands back a cursor, no history.
                        assert_eq!(req["params"]["arguments"]["timeout_s"], 1);
                        serde_json::json!({ "next_cursor": 100, "events": [] })
                    } else {
                        // Must resume from the primed cursor, not from zero.
                        assert_eq!(req["params"]["arguments"]["cursor"], 100);
                        serde_json::json!({
                            "next_cursor": 104,
                            "events": [
                                { "kind": "message.created", "thread_id": 42, "actor_agent_id": 7 },
                                { "kind": "message.created", "thread_id": 43, "actor_agent_id": 9 },
                                { "kind": "run.finished", "thread_id": 44, "actor_agent_id": 9 },
                            ]
                        })
                    }
                }
                other => panic!("unexpected tool {other}"),
            };

            let envelope = serde_json::json!({
                "jsonrpc": "2.0", "id": req["id"],
                "result": { "content": [{ "type": "text", "text": payload.to_string() }] }
            })
            .to_string();
            let res = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                envelope.len(),
                envelope
            );
            stream.write_all(res.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
    });

    (url, handle)
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap();
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).unwrap();
    String::from_utf8(body).unwrap()
}

/// Without a wall clock one wedged run stops the watch for good, which is the
/// exact failure this program exists to prevent.
#[test]
fn a_run_that_never_finishes_is_stopped() {
    let (url, server) = fake_rivendell();
    let started = std::time::Instant::now();

    let status = Command::new(env!("CARGO_BIN_EXE_rivendell-run"))
        .args(["--key", "rvd_test", "--url", &url, "--wait", "5", "--limit", "1", "--once", "--"])
        .args(["sleep", "120"])
        .status()
        .unwrap();

    assert!(status.success());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "it waited for the whole sleep instead of enforcing --limit"
    );
    server.join().unwrap();
}

#[test]
fn runs_the_command_once_with_the_threads_that_need_it() {
    let (url, server) = fake_rivendell();
    let out = std::env::temp_dir().join(format!("rivendell-run-test-{}", std::process::id()));
    let _ = std::fs::remove_file(&out);

    let status = Command::new(env!("CARGO_BIN_EXE_rivendell-run"))
        .args(["--key", "rvd_test", "--url", &url, "--wait", "5", "--once", "--"])
        .args(["sh", "-c", &format!("printf %s '{{threads}}' > {}", out.display())])
        .status()
        .unwrap();
    assert!(status.success());
    server.join().unwrap();

    // 42 was this agent's own doing and 44 was bookkeeping; only 43 is work.
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "43");
    let _ = std::fs::remove_file(&out);
}
