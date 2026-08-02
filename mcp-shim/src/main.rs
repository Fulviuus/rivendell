//! stdio ⇄ HTTP bridge for clients that cannot speak streamable-HTTP MCP or
//! cannot attach an Authorization header — and the channel that pushes room
//! activity straight into the session.
//!
//! Reads newline-delimited JSON-RPC on stdin, POSTs each message to the
//! Rivendell endpoint with the bearer token, writes the reply to stdout.
//!
//!   RIVENDELL_URL=http://127.0.0.1:8787/mcp RIVENDELL_KEY=rvd_… rivendell-mcp
//!
//! The channel is the second half, and the more interesting one. An agent that
//! has ended its turn cannot be reached: no server notification, on any
//! transport, resumes a model that is not being asked for tokens. The one
//! documented exception is `notifications/claude/channel`, which the host
//! injects into the session's context and then answers. So this holds a long
//! poll against Rivendell in the background and turns anything that needs this
//! agent into one of those. No loop for the agent to remember, and no cost
//! while the room is quiet.

use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};

/// Shared because the channel writes to it from another thread, in between
/// request replies. Interleaving the two would corrupt the stream.
type Out = Arc<Mutex<std::io::Stdout>>;

fn main() {
    let url = match std::env::var("RIVENDELL_URL") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => fail("RIVENDELL_URL is not set. Point it at the URL shown in Rivendell, e.g. http://127.0.0.1:8787/mcp"),
    };
    let key = match std::env::var("RIVENDELL_KEY") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => fail("RIVENDELL_KEY is not set. Use the agent API key Rivendell issued."),
    };
    let auth = format!("Bearer {key}");

    // Long polls can legitimately sit for a while, and this has to outlast the
    // longest one it forwards or it hangs up on the answer.
    let agent = ureq::AgentBuilder::new()
        .timeout_read(std::time::Duration::from_secs(3660))
        .timeout_connect(std::time::Duration::from_secs(10))
        .build();

    let stdin = BufReader::new(std::io::stdin());
    let stdout: Out = Arc::new(Mutex::new(std::io::stdout()));
    let mut channel_started = false;

    for line in stdin.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("rivendell-mcp: stdin closed: {e}");
                return;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let is_initialize = line.contains("\"initialize\"");

        let response = agent
            .post(&url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .set("Authorization", &auth)
            .send_string(&line);

        let body = match response {
            Ok(r) => r.into_string().unwrap_or_default(),
            // A 4xx/5xx still carries a JSON-RPC error body worth forwarding.
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                if text.trim_start().starts_with('{') {
                    text
                } else {
                    rpc_error(&line, -32000, &format!("HTTP {code}: {text}"))
                }
            }
            Err(e) => rpc_error(
                &line,
                -32000,
                &format!("could not reach Rivendell at {url}: {e}. Is the app running?"),
            ),
        };

        // A notification gets an empty 202; there is nothing to forward.
        if body.trim().is_empty() {
            continue;
        }

        // Rivendell speaks plain MCP and knows nothing about channels — that is
        // a property of this host, not of the workspace. So the capability is
        // declared here, on the way past.
        let body = if is_initialize {
            declare_channel(&body)
        } else {
            body
        };

        if !write_line(&stdout, &body) {
            return;
        }

        // Only after the client has been told the capability exists.
        if is_initialize && !channel_started {
            channel_started = true;
            start_channel(url.clone(), auth.clone(), stdout.clone());
        }
    }
}

fn write_line(out: &Out, line: &str) -> bool {
    let mut o = out.lock().unwrap_or_else(|e| e.into_inner());
    writeln!(o, "{line}").is_ok() && o.flush().is_ok()
}

/// Adds `experimental["claude/channel"]` to the initialize result, and tells the
/// model what the events it is about to receive actually mean.
fn declare_channel(body: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };
    let Some(result) = v.get_mut("result").and_then(|r| r.as_object_mut()) else {
        return body.to_string();
    };
    // The caller decides by a string match on the request, which a tool call
    // that merely mentions the word would fool. An initialize result always
    // carries these, and nothing else does — so recognise it properly rather
    // than trusting that.
    if !result.contains_key("protocolVersion") && !result.contains_key("serverInfo") {
        return body.to_string();
    }

    let caps = result
        .entry("capabilities")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(caps) = caps.as_object_mut() {
        let exp = caps
            .entry("experimental")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(exp) = exp.as_object_mut() {
            exp.insert("claude/channel".into(), serde_json::json!({}));
        }
    }

    // Appended rather than replacing: the workspace's own instructions explain
    // the roles and the tags, and those still apply.
    let existing = result
        .get("instructions")
        .and_then(|i| i.as_str())
        .unwrap_or("")
        .to_string();
    result.insert(
        "instructions".into(),
        serde_json::json!(format!(
            "{existing}\n\n\
             This workspace also pushes to you. Activity in your rooms arrives on its own as \
             <channel source=\"rivendell\" thread=\"…\" kind=\"…\">, without you asking for it \
             and without you having to wait for it. When one arrives, read that thread with \
             get_thread and act if it needs you — then simply stop. Do not poll, do not sit in \
             wait_for_updates, and do not arrange to check back: you will be told. A thread that \
             does not concern you needs no reply, and saying nothing costs nobody anything."
        )),
    );
    v.to_string()
}

/// Holds the long poll and turns anything that needs this agent into a channel
/// event. Runs until the process ends.
fn start_channel(url: String, auth: String, out: Out) {
    std::thread::spawn(move || {
        let agent = ureq::AgentBuilder::new()
            .timeout_read(std::time::Duration::from_secs(120))
            .timeout_connect(std::time::Duration::from_secs(10))
            .build();

        let mut cursor: Option<i64> = None;
        let mut quiet_failures = 0u32;
        loop {
            let args = match cursor {
                // The first ask also picks up whatever was already waiting:
                // work does not stop existing because nobody was listening
                // when it arrived.
                None => serde_json::json!({ "timeout_s": 1, "watcher": true, "catch_up": true }),
                Some(c) => serde_json::json!({ "cursor": c, "timeout_s": 45, "watcher": true }),
            };
            let body = serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "wait_for_updates", "arguments": args }
            })
            .to_string();

            let reply = agent
                .post(&url)
                .set("Content-Type", "application/json")
                .set("Authorization", &auth)
                .send_string(&body);

            let text = match reply {
                Ok(r) => r.into_string().unwrap_or_default(),
                Err(e) => {
                    // The app restarting is ordinary. Say so once, then wait
                    // rather than hammering a door that is not there.
                    quiet_failures += 1;
                    if quiet_failures == 1 {
                        eprintln!("rivendell-mcp: channel waiting for Rivendell ({e})");
                    }
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    continue;
                }
            };
            quiet_failures = 0;

            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                std::thread::sleep(std::time::Duration::from_secs(5));
                continue;
            };
            let inner = v["result"]["content"][0]["text"].as_str().unwrap_or("{}");
            let Ok(update) = serde_json::from_str::<serde_json::Value>(inner) else {
                std::thread::sleep(std::time::Duration::from_secs(5));
                continue;
            };

            if let Some(next) = update["next_cursor"].as_i64() {
                cursor = Some(next);
            }

            // Rivendell decides what needs this agent: it knows about resolved
            // threads, paused rooms, spent reply budgets, and whose doing an
            // event was. Anything it does not name is not worth a turn.
            let needs: Vec<i64> = update["needs_you"]
                .as_array()
                .map(|a| a.iter().filter_map(|t| t.as_i64()).collect())
                .unwrap_or_default();
            if needs.is_empty() {
                continue;
            }

            for note in channel_events(&update, &needs) {
                if !write_line(&out, &note) {
                    return; // stdout closed; the session is gone
                }
            }
        }
    });
}

/// One channel notification per thread. Separate rather than combined so each
/// arrives with its own routing attributes, and so a busy room reads as a list
/// of things to do rather than one wall of text.
fn channel_events(update: &serde_json::Value, needs: &[i64]) -> Vec<String> {
    let empty = vec![];
    let events = update["events"].as_array().unwrap_or(&empty);

    needs
        .iter()
        .map(|thread| {
            // What actually happened to this thread, in the order it happened.
            let kinds: Vec<&str> = events
                .iter()
                .filter(|e| e["thread_id"].as_i64() == Some(*thread))
                .filter_map(|e| e["kind"].as_str())
                .collect();
            let what = if kinds.is_empty() {
                "is open and waiting for you".to_string()
            } else {
                kinds.join(", ")
            };

            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/claude/channel",
                "params": {
                    "content": format!(
                        "Thread #{thread}: {what}. Read it with get_thread({thread}) and reply \
                         if it needs you. If it does not concern you, leave it."
                    ),
                    // Identifiers only — a key with a hyphen in it is dropped
                    // silently, which would be a confusing thing to debug.
                    "meta": {
                        "thread": thread.to_string(),
                        "kind": kinds.first().copied().unwrap_or("waiting").replace('.', "_"),
                    }
                }
            })
            .to_string()
        })
        .collect()
}

/// Builds a JSON-RPC error that echoes the id of the request that failed, so
/// the client can match it up instead of hanging.
fn rpc_error(request: &str, code: i64, message: &str) -> String {
    let id = extract_id(request).unwrap_or_else(|| "null".to_string());
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":{code},"message":"{}"}}}}"#,
        escape(message)
    )
}

/// Minimal scan for the top-level `"id"` value. Avoids pulling in a JSON parser
/// for the one field we need on the error path.
fn extract_id(request: &str) -> Option<String> {
    let at = request.find("\"id\"")?;
    let rest = request[at + 4..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    if let Some(s) = rest.strip_prefix('"') {
        let end = s.find('"')?;
        return Some(format!("\"{}\"", escape(&s[..end])));
    }
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(rest.len());
    let num = &rest[..end];
    if num.is_empty() {
        None
    } else {
        Some(num.to_string())
    }
}

fn escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            '\n' | '\r' | '\t' => vec![' '],
            c if (c as u32) < 0x20 => vec![' '],
            c => vec![c],
        })
        .collect()
}

fn fail(message: &str) -> ! {
    eprintln!("rivendell-mcp: {message}");
    std::process::exit(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_numeric_and_string_ids() {
        assert_eq!(extract_id(r#"{"jsonrpc":"2.0","id":7,"method":"x"}"#).as_deref(), Some("7"));
        assert_eq!(
            extract_id(r#"{"id": "abc", "method":"x"}"#).as_deref(),
            Some("\"abc\"")
        );
        assert_eq!(extract_id(r#"{"method":"notify"}"#), None);
    }

    #[test]
    fn error_payload_is_valid_json_shape() {
        let out = rpc_error(r#"{"id":3}"#, -32000, "boom \"quoted\"");
        assert!(out.contains(r#""id":3"#));
        assert!(out.contains(r#"\"quoted\""#));
        assert!(!out.contains('\n'));
    }

    /// The capability is the whole thing: without it the host never registers a
    /// listener and every event is dropped in silence.
    #[test]
    fn initialize_gains_the_channel_capability() {
        let out = declare_channel(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"instructions":"be good"}}"#,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["result"]["capabilities"]["experimental"]["claude/channel"], serde_json::json!({}));
        // What was already there survives.
        assert_eq!(v["result"]["capabilities"]["tools"], serde_json::json!({}));
        let inst = v["result"]["instructions"].as_str().unwrap();
        assert!(inst.starts_with("be good"), "clobbered the workspace's own: {inst}");
        assert!(inst.contains("you will be told"));
    }

    #[test]
    fn a_reply_that_is_not_initialize_passes_through_untouched() {
        let body = r#"{"jsonrpc":"2.0","id":2,"result":{"content":[]}}"#;
        assert_eq!(declare_channel(body), body.to_string());
    }

    #[test]
    fn one_event_per_thread_that_needs_this_agent() {
        let update = serde_json::json!({
            "needs_you": [16, 17],
            "events": [
                {"kind": "message.created", "thread_id": 16},
                {"kind": "thread.created", "thread_id": 17},
                {"kind": "message.created", "thread_id": 99},
            ]
        });
        let out = channel_events(&update, &[16, 17]);
        assert_eq!(out.len(), 2, "one per thread");
        assert!(out[0].contains("notifications/claude/channel"));
        assert!(out[0].contains("get_thread(16)"));
        assert!(out[1].contains("get_thread(17)"));
        // 99 was not in needs_you, so it is not this agent's problem.
        assert!(!out.iter().any(|e| e.contains("get_thread(99)")));
    }

    /// A meta key with a hyphen or a dot is dropped silently by the host, which
    /// would be a miserable thing to debug.
    #[test]
    fn meta_keys_and_values_are_identifier_safe() {
        let update = serde_json::json!({
            "needs_you": [5],
            "events": [{"kind": "message.created", "thread_id": 5}]
        });
        let v: serde_json::Value =
            serde_json::from_str(&channel_events(&update, &[5])[0]).unwrap();
        let meta = v["params"]["meta"].as_object().unwrap();
        for k in meta.keys() {
            assert!(
                k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "meta key {k:?} would be dropped"
            );
        }
        assert_eq!(meta["kind"], "message_created");
        assert_eq!(meta["thread"], "5");
    }
}
