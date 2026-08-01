//! The tool surface agents actually see.
//!
//! Descriptions here are load-bearing: they are the only instructions most
//! agents will read before acting, so they say what the tool is *for* and what
//! will be rejected, not just what the parameters are.

use crate::error::{Error, Result};
use crate::models::{ContextInput, NewReply, NewThread};
use crate::store::{AgentCtx, Store};
use serde_json::{json, Value};
use std::sync::Arc;

use super::server::McpState;

fn tool(name: &str, description: &str, schema: Value, read_only: bool) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
        "annotations": { "readOnlyHint": read_only, "openWorldHint": false }
    })
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

pub fn common_tools() -> Vec<Value> {
    vec![
        tool(
            "whoami",
            "Who you are in this workspace: your agent name, role, room, project folder and the \
             tags available. Call this first.",
            obj(json!({}), &[]),
            true,
        ),
        tool(
            "list_threads",
            "Threads in your room. Defaults to those that still need attention. Threads that \
             mention nobody are open to every assistant; threads that mention specific agents are \
             only listed for those agents unless you pass mentions_me=false.",
            obj(
                json!({
                    "room": {"type": "string", "description": "Limit to one room. Omit to see every room you are in."},
                    "status": {"type": "string", "description": "open (default — still live work), resolved, blocked, all, or one exact status: OPEN, AWAITING_REPLIES, NEEDS_CODER, RESOLVED, BLOCKED, WONTFIX"},
                    "tag": {"type": "string", "description": "Filter to one tag, e.g. ADVERSARIAL_REVIEW"},
                    "mentions_me": {"type": "boolean", "description": "Default true for assistants: only threads addressed to you or to everyone"},
                    "sort": {"type": "string", "enum": ["last_reply", "created", "activity"], "description": "last_reply (default) — freshest conversation first; created — newest thread first; activity — busiest first"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200}
                }),
                &[],
            ),
            true,
        ),
        tool(
            "get_thread",
            "Read one thread in full: the topic, what the tag expects of you, every reply so far, \
             and the file excerpts and diff exactly as they were when the thread was opened. Read \
             this before replying — the pinned context is the reviewable artifact, not the current \
             working tree.",
            obj(json!({"thread_id": {"type": "integer"}}), &["thread_id"]),
            true,
        ),
        tool(
            "claim_thread",
            "Say that you are working on a thread. Do this *before* you start investigating, not \
             after. Once the first agent answers, a short window opens in which the rest must \
             claim; anyone silent through it is left out and the coder proceeds without them. \
             Claiming puts you in that set. If the work runs long, call it again — each call \
             refreshes the heartbeat, and a claim that goes quiet is dropped so one stalled agent \
             cannot hold the thread for ever.",
            obj(
                json!({
                    "thread_id": {"type": "integer"},
                    "note": {"type": "string", "description": "Optional, shown in the UI — e.g. 'reading the diff' or 'reproducing locally'."}
                }),
                &["thread_id"],
            ),
            false,
        ),
        tool(
            "reply",
            "Post a reply. Write `@name` to call another agent into the thread — use it when a \
             question needs someone else's expertise rather than guessing; `list_agents` shows who \
             is here. They are notified and get their own chance to answer. \
             Tags such as ADVERSARIAL_REVIEW and SECURITY_REVIEW require a verdict \
             and your reply will be rejected without one — the coder consumes verdicts \
             programmatically, so choose honestly rather than hedging. Attach `refs` pointing at \
             the exact file and line for every claim you make. You have a per-thread reply budget; \
             when you have said what you know, stop.",
            obj(
                json!({
                    "thread_id": {"type": "integer"},
                    "body": {"type": "string", "description": "Markdown. Be specific and concrete; give failing inputs, not adjectives."},
                    "verdict": {"type": "string", "description": "Required by most tags. get_thread tells you the allowed values."},
                    "severity": {"type": "string", "enum": ["CRITICAL","HIGH","MEDIUM","LOW","INFO"]},
                    "refs": {
                        "type": "array",
                        "description": "Where in the code this applies.",
                        "items": {"type": "object", "properties": {
                            "path": {"type": "string"},
                            "line": {"type": "integer"},
                            "note": {"type": "string"}
                        }, "required": ["path"]}
                    },
                    "tokens_in": {"type": "integer", "description": "Optional: your input token count, for the room's cost meter"},
                    "tokens_out": {"type": "integer"},
                    "cost_usd": {"type": "number"}
                }),
                &["thread_id", "body"],
            ),
            false,
        ),
        tool(
            "edit_reply",
            "Revise a reply you already posted. Use this when the thread has moved under you — the \
             coder edits the topic or a message you were answering, and your original answer no \
             longer fits. Editing keeps the conversation readable; posting a near-duplicate \
             correction does not, and it burns your reply budget. You can only edit your own \
             messages. Watch for `message.edited` events from wait_for_updates.",
            obj(
                json!({
                    "message_id": {"type": "integer", "description": "From get_thread."},
                    "body": {"type": "string", "description": "The full replacement text, not a diff."},
                    "verdict": {"type": "string", "description": "Re-state it; a tag that requires one still requires it. Change it if the new context changed your mind."},
                    "severity": {"type": "string", "enum": ["CRITICAL","HIGH","MEDIUM","LOW","INFO"]},
                    "refs": {
                        "type": "array",
                        "items": {"type": "object", "properties": {
                            "path": {"type": "string"},
                            "line": {"type": "integer"},
                            "note": {"type": "string"}
                        }, "required": ["path"]}
                    }
                }),
                &["message_id", "body"],
            ),
            false,
        ),
        tool(
            "wait_for_updates",
            "Block until something happens in your room, then return the events. Use this instead \
             of polling in a loop — it is the cheap way to stay resident. Pass the next_cursor from \
             the previous call to avoid missing anything. The reply also carries `needs_you`: the \
             threads in this batch that somebody else moved and that you can still act on. Start \
             there.",
            obj(
                json!({
                    "cursor": {"type": "integer", "description": "Last seq you have seen. Omit to start from now."},
                    "timeout_s": {"type": "integer", "minimum": 1, "maximum": 3600, "description": "Default 60. Go long — the call is cheap and returns the moment anything happens."}
                }),
                &[],
            ),
            true,
        ),
        tool(
            "read_file",
            "Read a file from the project folder. Read-only and jailed: paths outside the project, \
             .git, and anything that looks like a secret are refused, and every read is logged. \
             Pass a line range for large files.",
            obj(
                json!({
                    "path": {"type": "string", "description": "Relative to the project root."},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1}
                }),
                &["path"],
            ),
            true,
        ),
        tool(
            "list_files",
            "List files and directories under the project root. Build directories and secrets are \
             hidden.",
            obj(
                json!({
                    "path": {"type": "string", "description": "Subdirectory; omit for the root."},
                    "depth": {"type": "integer", "minimum": 1, "maximum": 6, "description": "Default 2."}
                }),
                &[],
            ),
            true,
        ),
        tool(
            "git_diff",
            "Diff of the project. With no base this is the uncommitted working-tree diff; pass the \
             thread's git_ref as `base` to see what changed since the thread was opened.",
            obj(
                json!({
                    "base": {"type": "string", "description": "A commit-ish, e.g. the thread's git_ref or HEAD~1."},
                    "path": {"type": "string", "description": "Limit to one path."}
                }),
                &[],
            ),
            true,
        ),
        tool(
            "list_agents",
            "Everyone in your room, with their ids — use these ids to mention specific agents.",
            obj(json!({}), &[]),
            true,
        ),
        tool(
            "search",
            "Full-text search across threads and replies in your room, including resolved ones. \
             Check here before re-litigating a decision.",
            obj(
                json!({
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100}
                }),
                &["query"],
            ),
            true,
        ),
    ]
}

pub fn coder_tools() -> Vec<Value> {
    vec![
        tool(
            "create_thread",
            "Open a thread. The tag decides what the assistants are told to do and what shape \
             their replies must take — pick it deliberately. Attach the code under discussion via \
             `context` or `include_diff`: it is snapshotted now, so the review stays valid even \
             after you keep working. Connected assistants pick the thread up on their next \
             `wait_for_updates`; you do not need to launch anything.",
            obj(
                json!({
                    "room": {"type": "string", "description": "Which room, by name. Only needed when you are in more than one — whoami lists them."},
                    "title": {"type": "string"},
                    "body": {"type": "string", "description": "Markdown. Say what you tried and what you expect, not just what is broken."},
                    "tag": {"type": "string", "description": "HELP_REQUEST, ADVERSARIAL_REVIEW, DESIGN_REVIEW, SECURITY_REVIEW, ARCHITECTURE_DECISION, SPEC_CLARIFICATION, PERF, FYI"},
                    "mentions": {"type": "array", "items": {"type": "integer"}, "description": "Agent ids. Omit to address the whole room. You can also write @name in the body."},
                    "include_diff": {"type": "boolean", "description": "Snapshot the current working-tree diff onto the thread."},
                    "context": {
                        "type": "array",
                        "description": "File excerpts to pin. Snapshotted at post time.",
                        "items": {"type": "object", "properties": {
                            "kind": {"type": "string", "enum": ["file","diff","note"]},
                            "path": {"type": "string"},
                            "start_line": {"type": "integer"},
                            "end_line": {"type": "integer"},
                            "content": {"type": "string", "description": "Only for kind=note."}
                        }, "required": ["kind"]}
                    },
                }),
                &["title", "body", "tag"],
            ),
            false,
        ),
        tool(
            "update_thread",
            "Rewrite your own thread's topic — use it to fold in what you have learned while the \
             assistants are working, so they are not reviewing a stale question.",
            obj(
                json!({"thread_id": {"type": "integer"}, "body": {"type": "string"}}),
                &["thread_id", "body"],
            ),
            false,
        ),
        tool(
            "resolve_thread",
            "Close a thread once you are satisfied. The summary is written into the repo at \
             .rivendell/threads/ as a durable decision record, so write it for someone reading it \
             in six months: what was decided and why, not just 'fixed'.",
            obj(
                json!({
                    "thread_id": {"type": "integer"},
                    "summary": {"type": "string"},
                    "status": {"type": "string", "enum": ["RESOLVED","WONTFIX","BLOCKED"], "description": "Default RESOLVED."}
                }),
                &["thread_id", "summary"],
            ),
            false,
        ),
        tool(
            "set_thread_status",
            "Move a thread without closing it, e.g. back to AWAITING_REPLIES after you post more \
             information.",
            obj(
                json!({
                    "thread_id": {"type": "integer"},
                    "status": {"type": "string", "enum": ["OPEN","AWAITING_REPLIES","NEEDS_CODER","BLOCKED"]}
                }),
                &["thread_id", "status"],
            ),
            false,
        ),
    ]
}

// ----------------------------------------------------------------- calls ---

pub async fn call(
    state: &Arc<McpState>,
    ctx: &AgentCtx,
    name: &str,
    args: Value,
) -> Result<String> {
    let store = &state.store;
    match name {
        "whoami" => whoami(store, ctx),
        "list_threads" => list_threads(store, ctx, &args),
        "get_thread" => {
            let id = int_arg(&args, "thread_id")?;
            let d = store.thread_detail(id)?;
            if !store.rooms_for(ctx.id)?.contains(&d.summary.room_id) {
                return Err(Error::Forbidden("you are not in that room".into()));
            }
            Ok(render_thread(store, &d)?)
        }
        "claim_thread" => {
            let id = int_arg(&args, "thread_id")?;
            store.claim_thread(
                ctx,
                id,
                args.get("note").and_then(|v| v.as_str()).unwrap_or(""),
            )?;
            Ok(format!(
                "Claimed thread {id}. The room can see you are on it. Call claim_thread again if \
                 this takes a while, then post your reply."
            ))
        }
        "reply" => {
            let input: NewReply = serde_json::from_value(args)
                .map_err(|e| Error::Invalid(format!("bad arguments: {e}")))?;
            let id = store.reply(ctx, input)?;
            Ok(format!(
                "Posted message {id}. The coder can see it now; do not repeat yourself."
            ))
        }
        "edit_reply" => {
            let message_id = int_arg(&args, "message_id")?;
            let input: NewReply = serde_json::from_value(serde_json::json!({
                "thread_id": 0,
                "body": args.get("body").cloned().unwrap_or_default(),
                "verdict": args.get("verdict").cloned(),
                "severity": args.get("severity").cloned(),
                "refs": args.get("refs").cloned(),
            }))
            .map_err(|e| Error::Invalid(format!("bad arguments: {e}")))?;
            store.edit_message(ctx, input, message_id)?;
            Ok(format!("Message {message_id} updated."))
        }
        "wait_for_updates" => wait_for_updates(state, ctx, &args).await,
        "read_file" => read_file(store, ctx, &args),
        "list_files" => list_files(ctx, &args),
        "git_diff" => git_diff(ctx, &args),
        "list_agents" => list_agents(store, ctx),
        "search" => {
            let q = str_arg(&args, "query")?;
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(20);
            let rows = store.search(room_arg(store, ctx, &args, "room")?, &q, limit)?;
            Ok(serde_json::to_string_pretty(&rows)?)
        }

        // ---- coder only ----
        "create_thread" => create_thread(store, ctx, args),
        "update_thread" => {
            let id = int_arg(&args, "thread_id")?;
            store.update_thread_body(ctx, id, &str_arg(&args, "body")?)?;
            Ok(format!("Thread {id} updated."))
        }
        "resolve_thread" => {
            let id = int_arg(&args, "thread_id")?;
            let status = args
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("RESOLVED");
            let path = store.resolve_thread(ctx, id, &str_arg(&args, "summary")?, status)?;
            Ok(match path {
                Some(p) => format!("Thread {id} is {status}. Decision record written to {p}"),
                None => format!("Thread {id} is {status}."),
            })
        }
        "set_thread_status" => {
            let id = int_arg(&args, "thread_id")?;
            let status = str_arg(&args, "status")?;
            store.set_thread_status(ctx, id, &status)?;
            Ok(format!("Thread {id} is now {status}."))
        }

        other => Err(Error::Invalid(format!("unknown tool `{other}`"))),
    }
}

fn whoami(store: &Arc<Store>, ctx: &AgentCtx) -> Result<String> {
    let joined = store.rooms_for(ctx.id)?;
    let rooms: Vec<Value> = store
        .list_rooms()?
        .into_iter()
        .filter(|r| joined.contains(&r.id))
        .map(|r| json!({"room_id": r.id, "name": r.name, "purpose": r.purpose, "paused": r.paused}))
        .collect();
    let tags: Vec<Value> = store
        .list_tags()?
        .into_iter()
        .map(|t| {
            json!({
                "tag": t.key,
                "label": t.label,
                "expects": t.instruction,
                "verdicts": t.verdict_options,
                "verdict_required": t.requires_verdict,
                "expects_replies": t.expects_replies,
            })
        })
        .collect();

    Ok(serde_json::to_string_pretty(&json!({
        "agent_id": ctx.id,
        "name": ctx.name,
        "role": ctx.role,
        "project": ctx.project_name,
        "project_folder": ctx.folder_path,
        "rooms": rooms,
        "can_open_threads": ctx.is_coder(),
        "tags": tags,
    }))?)
}

fn list_threads(store: &Arc<Store>, ctx: &AgentCtx, args: &Value) -> Result<String> {
    let status = args.get("status").and_then(|v| v.as_str()).unwrap_or("open");
    let tag = args.get("tag").and_then(|v| v.as_str());
    let mentions_me = args
        .get("mentions_me")
        .and_then(|v| v.as_bool())
        .unwrap_or(ctx.role == "ASSISTANT");
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);

    let only = room_arg(store, ctx, args, "room")?;
    let joined = store.rooms_for(ctx.id)?;

    let mut rows = Vec::new();
    for room in joined {
        if let Some(one) = only {
            if one != room {
                continue;
            }
        }
        rows.extend(store.list_threads(
            Some(room),
            Some(status),
            tag,
            if mentions_me { Some(ctx.id) } else { None },
            args.get("sort").and_then(|v| v.as_str()),
            limit,
        )?);
    }
    Ok(serde_json::to_string_pretty(&rows)?)
}

fn list_agents(store: &Arc<Store>, ctx: &AgentCtx) -> Result<String> {
    let rows: Vec<Value> = store
        .list_agents(room_arg(store, ctx, &Value::Null, "room")?)?
        .into_iter()
        .map(|a| {
            json!({
                "agent_id": a.id,
                "name": a.name,
                "role": a.role,
                "kind": a.profile_label,
                "note": a.system_note,
                "active": a.revoked_at.is_none(),
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&rows)?)
}

/// Resolves a `room` argument by name or id, restricted to rooms the agent has
/// joined. Returns None when the caller did not name one and it is ambiguous —
/// an agent in exactly one room never has to say which.
fn room_arg(store: &Arc<Store>, ctx: &AgentCtx, args: &Value, key: &str) -> Result<Option<i64>> {
    let joined = store.rooms_for(ctx.id)?;
    let Some(v) = args.get(key) else {
        return Ok(if joined.len() == 1 { Some(joined[0]) } else { None });
    };
    let rooms = store.list_rooms()?;
    let found = if let Some(id) = v.as_i64() {
        rooms.iter().find(|r| r.id == id)
    } else if let Some(name) = v.as_str() {
        let name = name.trim_start_matches('#');
        rooms.iter().find(|r| r.name.eq_ignore_ascii_case(name))
    } else {
        None
    };
    let room = found.ok_or_else(|| Error::NotFound(format!("room {v}")))?;
    if !joined.contains(&room.id) {
        return Err(Error::Forbidden(format!("you are not in #{}", room.name)));
    }
    Ok(Some(room.id))
}

fn create_thread(store: &Arc<Store>, ctx: &AgentCtx, args: Value) -> Result<String> {
    let context: Vec<ContextInput> = match args.get("context") {
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| Error::Invalid(format!("bad context: {e}")))?,
        None => vec![],
    };
    let room_id = room_arg(store, ctx, &args, "room")?.ok_or_else(|| {
        Error::Invalid(
            "which room? pass `room` — whoami lists the ones you are in".into(),
        )
    })?;
    let input = NewThread {
        room_id,
        title: str_arg(&args, "title")?,
        body: str_arg(&args, "body")?,
        tag: str_arg(&args, "tag")?,
        mentions: args
            .get("mentions")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
            .unwrap_or_default(),
        context,
        include_diff: args
            .get("include_diff")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    };

    let thread_id = store.create_thread(ctx, input)?;
    Ok(format!(
        "Opened thread {thread_id}. Any connected assistant will see it on its next \
         wait_for_updates. Call wait_for_updates yourself to be told when replies land."
    ))
}

async fn wait_for_updates(state: &Arc<McpState>, ctx: &AgentCtx, args: &Value) -> Result<String> {
    let store = &state.store;
    // A supervised run was started to deal with named threads and is billing
    // the whole time it sits here. Parking it for an hour would turn one
    // wake-up into an hour-long session that answers nothing, so it gets long
    // enough to catch a reply that is already on its way and no longer.
    // A watcher is supervised too, but holding the poll is the whole of its
    // job — clamping it would turn one blocked socket into a poll every
    // fifteen seconds for ever. It says which it is, because they share a
    // credential and nothing else can tell them apart.
    let is_watcher = args.get("watcher").and_then(|v| v.as_bool()).unwrap_or(false);
    let ceiling = if ctx.supervised && !is_watcher { 15 } else { 3600 };
    let timeout_s = args
        .get("timeout_s")
        .and_then(|v| v.as_i64())
        .unwrap_or(60)
        .clamp(1, ceiling) as u64;
    // Subscribe before the first read, otherwise an event landing between the
    // query and the subscribe would be missed and we would block for nothing.
    let mut rx = store.events.subscribe();
    let mut cursor = match args.get("cursor").and_then(|v| v.as_i64()) {
        Some(c) => c,
        None => store.latest_seq()?,
    };

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_s);
    loop {
        // Re-read each time round: rooms can be joined or left while we wait,
        // and an agent revoked mid-poll must stop being told things.
        if !store.agent_is_live(ctx.id)? {
            return Err(Error::Forbidden("this agent's key is no longer valid".into()));
        }
        let joined = store.rooms_for(ctx.id)?;

        let scanned = store.events_since(cursor, None, 200)?;
        // Where we got to, whether or not any of it concerned us. Advancing
        // only past *matched* rows was a starvation bug: once 200 events for
        // other rooms sat ahead of the cursor, every call re-read that same
        // window and the agent never saw anything again.
        let last_scanned = scanned.last().map(|r| r.seq);
        let rows: Vec<_> = scanned
            .into_iter()
            .filter(|e| e.room_id.map(|r| joined.contains(&r)).unwrap_or(false))
            .take(100)
            .collect();

        if !rows.is_empty() {
            let next = rows.last().map(|r| r.seq).unwrap_or(cursor);

            // Threads somebody *else* moved, that this agent could still act
            // on: not resolved, room not paused, its reply budget not spent.
            //
            // A watcher could work most of this out itself, but it would be a
            // second copy of a rule that already lives in the store — and the
            // one place it could not look is the reply budget. Answering it
            // here is what stops a watcher starting a whole billable session
            // for an agent that will be refused the moment it speaks.
            let touched: std::collections::BTreeSet<i64> = rows
                .iter()
                .filter(|e| e.actor_agent_id != Some(ctx.id))
                .filter_map(|e| e.thread_id)
                .collect();
            let needs_you =
                store.wakeable_threads(ctx.id, &touched.into_iter().collect::<Vec<_>>())?;

            return Ok(serde_json::to_string_pretty(&json!({
                "next_cursor": next,
                "events": rows,
                "needs_you": needs_you,
                "then": if ctx.supervised {
                    "Rivendell started you for these. Deal with them and exit — you will be \
                     started again when there is more. Do not loop here."
                } else {
                    "Act on these, then call wait_for_updates again with this next_cursor. \
                     Staying in the loop is how you keep seeing work."
                },
            }))?);
        }

        if let Some(seq) = last_scanned {
            // None of it was ours, but we have moved past it and there may be
            // more already waiting.
            cursor = seq;
            continue;
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(serde_json::to_string_pretty(&json!({
                "next_cursor": cursor,
                "events": [],
                "note": if ctx.supervised {
                    "Nothing new. You were started for work already named in your instructions \
                     — finish that and exit rather than waiting here."
                } else {
                    "Nothing happened before the timeout. Call again with this next_cursor."
                },
            }))?);
        }
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) | Err(_) => continue,
        }
    }
}

fn read_file(store: &Arc<Store>, ctx: &AgentCtx, args: &Value) -> Result<String> {
    let path = str_arg(args, "path")?;
    let root = ctx.root()?;
    let result = crate::fsjail::read_slice(
        &root,
        &path,
        args.get("start_line").and_then(|v| v.as_i64()),
        args.get("end_line").and_then(|v| v.as_i64()),
    );
    match result {
        Ok(s) => {
            store.log_file_access(ctx.id, None, &path, true, "");
            Ok(format!(
                "{} (lines {}-{} of {})\n\n{}",
                s.path, s.start_line, s.end_line, s.total_lines, s.content
            ))
        }
        Err(e) => {
            store.log_file_access(ctx.id, None, &path, false, &e.to_string());
            Err(e)
        }
    }
}

fn list_files(ctx: &AgentCtx, args: &Value) -> Result<String> {
    let root = ctx.root()?;
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let depth = args.get("depth").and_then(|v| v.as_i64()).unwrap_or(2).clamp(1, 6) as usize;
    Ok(crate::fsjail::list_dir(&root, path, depth)?.join("\n"))
}

fn git_diff(ctx: &AgentCtx, args: &Value) -> Result<String> {
    let root = ctx.root()?;
    let out = crate::git::diff(
        &root,
        args.get("base").and_then(|v| v.as_str()),
        args.get("path").and_then(|v| v.as_str()),
    )?;
    Ok(if out.trim().is_empty() {
        "No changes.".into()
    } else {
        out
    })
}

// ------------------------------------------------------------- rendering ---

fn render_thread(store: &Arc<Store>, d: &crate::models::ThreadDetail) -> Result<String> {
    let s = &d.summary;
    let tag = store
        .list_tags()?
        .into_iter()
        .find(|t| t.key == s.tag);

    let mut out = String::new();
    out.push_str(&format!("# Thread {} — {}\n\n", s.id, s.title));
    out.push_str(&format!(
        "**Tag** {} · **Status** {} · **Opened by** {} · **Replies** {} from {} assistant(s){}\n",
        s.tag,
        s.status,
        s.author_name,
        s.reply_count,
        s.responder_count,
        if s.in_progress > 0 {
            format!(" · {} still working", s.in_progress)
        } else {
            String::new()
        }
    ));
    if let Some(g) = &s.git_ref {
        out.push_str(&format!(
            "**Pinned at** `{}`{}\n",
            &g[..g.len().min(12)],
            if d.git_dirty {
                " (working tree was dirty when posted)"
            } else {
                ""
            }
        ));
    }
    out.push('\n');

    if let Some(t) = &tag {
        out.push_str("## What this tag expects of you\n\n");
        out.push_str(&t.instruction);
        out.push_str("\n\n");
        if !t.verdict_options.is_empty() {
            out.push_str(&format!(
                "Your reply **must** carry one of these verdicts: `{}`.\n\n",
                t.verdict_options.join("`, `")
            ));
        }
    }

    out.push_str("## Topic\n\n");
    out.push_str(d.body.trim());
    out.push_str("\n\n");

    if !d.context.is_empty() {
        out.push_str("## Pinned context\n\n");
        out.push_str("_Snapshotted when the thread was opened. Review this, not the live tree._\n\n");
        for c in &d.context {
            let header = match (&c.path, c.start_line, c.end_line) {
                (Some(p), Some(a), Some(b)) => format!("{p}:{a}-{b}"),
                (Some(p), _, _) => p.clone(),
                _ => c.kind.clone(),
            };
            let lang = if c.kind == "diff" { "diff" } else { "" };
            out.push_str(&format!("### {header}\n\n```{lang}\n{}\n```\n\n", c.content.trim_end()));
        }
    }

    out.push_str("## Replies\n\n");
    if d.messages.is_empty() {
        out.push_str("_None yet._\n\n");
    }
    for m in &d.messages {
        let mut head = format!("### {} ({})", m.agent_name, m.agent_role);
        if let Some(v) = &m.verdict {
            head.push_str(&format!(" — {v}"));
        }
        if let Some(sev) = &m.severity {
            head.push_str(&format!(" · {sev}"));
        }
        if m.edited_at.is_some() {
            head.push_str(" · edited since first posted");
        }
        out.push_str(&format!("{head}\n\n{}\n\n", m.body.trim()));
        if let Some(refs) = m.refs.as_array() {
            for r in refs {
                out.push_str(&format!(
                    "- `{}{}` {}\n",
                    r.get("path").and_then(|v| v.as_str()).unwrap_or(""),
                    r.get("line").and_then(|v| v.as_i64()).map(|l| format!(":{l}")).unwrap_or_default(),
                    r.get("note").and_then(|v| v.as_str()).unwrap_or("")
                ));
            }
            if !refs.is_empty() {
                out.push('\n');
            }
        }
    }

    if let Some(r) = &d.resolution_summary {
        out.push_str(&format!("## Resolution\n\n{r}\n"));
    }
    Ok(out)
}

// --------------------------------------------------------------- prompts ---

pub fn prompts_list(store: &Arc<Store>) -> Result<Value> {
    let prompts: Vec<Value> = store
        .list_tags()?
        .into_iter()
        .map(|t| {
            json!({
                "name": t.key.to_lowercase(),
                "title": t.label,
                "description": format!("Brief for a {} thread.", t.label.to_lowercase()),
                "arguments": [
                    {"name": "thread_id", "description": "Thread to work on", "required": true}
                ]
            })
        })
        .collect();
    Ok(json!({ "prompts": prompts }))
}

pub fn prompts_get(store: &Arc<Store>, params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Invalid("prompts/get needs a name".into()))?;
    let tag = store
        .list_tags()?
        .into_iter()
        .find(|t| t.key.eq_ignore_ascii_case(name))
        .ok_or_else(|| Error::NotFound(format!("prompt `{name}`")))?;

    let thread_id = params
        .get("arguments")
        .and_then(|a| a.get("thread_id"))
        .map(|v| v.to_string().trim_matches('"').to_string())
        .unwrap_or_default();

    let text = format!(
        "{}\n\nCall get_thread({}) first to read the topic and the pinned context, then post \
         exactly one `reply` carrying a verdict{}.",
        tag.instruction,
        if thread_id.is_empty() { "<thread_id>".into() } else { thread_id },
        if tag.verdict_options.is_empty() {
            String::new()
        } else {
            format!(" from: {}", tag.verdict_options.join(", "))
        }
    );

    Ok(json!({
        "description": tag.label,
        "messages": [{"role": "user", "content": {"type": "text", "text": text}}]
    }))
}

// ------------------------------------------------------------- resources ---

pub fn resources_list(store: &Arc<Store>, ctx: &AgentCtx) -> Result<Value> {
    let rows: Vec<Value> = store
        .list_threads(None, Some("open"), None, None, None, 100)?
        .into_iter()
        .filter(|t| store.rooms_for(ctx.id).map(|r| r.contains(&t.room_id)).unwrap_or(false))
        .collect::<Vec<_>>()
        .into_iter()
        .into_iter()
        .map(|t| {
            json!({
                "uri": format!("rivendell://thread/{}", t.id),
                "name": format!("#{} {}", t.id, t.title),
                "description": format!("{} · {}", t.tag, t.status),
                "mimeType": "text/markdown"
            })
        })
        .collect();
    Ok(json!({ "resources": rows }))
}

pub fn resources_read(store: &Arc<Store>, ctx: &AgentCtx, params: &Value) -> Result<Value> {
    let uri = params
        .get("uri")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Invalid("resources/read needs a uri".into()))?;
    let id: i64 = uri
        .strip_prefix("rivendell://thread/")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::Invalid(format!("unsupported uri `{uri}`")))?;

    let d = store.thread_detail(id)?;
    if !store.rooms_for(ctx.id)?.contains(&d.summary.room_id) {
        return Err(Error::Forbidden("you are not in that room".into()));
    }
    Ok(json!({
        "contents": [{
            "uri": uri,
            "mimeType": "text/markdown",
            "text": render_thread(store, &d)?
        }]
    }))
}

// ----------------------------------------------------------------- utils ---

fn str_arg(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| Error::Invalid(format!("`{key}` is required and must be a string")))
}

fn int_arg(args: &Value, key: &str) -> Result<i64> {
    args.get(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .ok_or_else(|| Error::Invalid(format!("`{key}` is required and must be an integer")))
}
