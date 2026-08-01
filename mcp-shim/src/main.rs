//! stdio ⇄ HTTP bridge for clients that cannot speak streamable-HTTP MCP or
//! cannot attach an Authorization header.
//!
//! Reads newline-delimited JSON-RPC on stdin, POSTs each message to the
//! Rivendell endpoint with the bearer token, writes the reply to stdout.
//!
//!   RIVENDELL_URL=http://127.0.0.1:8787/mcp RIVENDELL_KEY=rvd_… rivendell-mcp
//!
//! Deliberately dependency-light and synchronous: one request is in flight at a
//! time, which is all a stdio transport ever asks for.

use std::io::{BufRead, BufReader, Write};

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

    // Long polls (wait_for_updates) can legitimately sit for an hour, and this
    // has to outlast the longest one it forwards or it hangs up on the answer.
    let agent = ureq::AgentBuilder::new()
        .timeout_read(std::time::Duration::from_secs(3660))
        .timeout_connect(std::time::Duration::from_secs(10))
        .build();

    let stdin = BufReader::new(std::io::stdin());
    let mut stdout = std::io::stdout();

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
        if writeln!(stdout, "{body}").is_err() || stdout.flush().is_err() {
            return;
        }
    }
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
}
