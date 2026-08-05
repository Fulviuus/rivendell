//! Streamable-HTTP MCP endpoint.
//!
//! Bound to 127.0.0.1 only. Every request carries `Authorization: Bearer rvd_…`
//! which resolves to exactly one agent in exactly one room — that identity is
//! what every tool call is scoped and permission-checked against.

use super::{PROTOCOL_VERSION, SUPPORTED_VERSIONS};
use crate::store::{AgentCtx, Store};
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Router,
};
use tokio_stream::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct McpState {
    pub store: Arc<Store>,
}

pub struct Running {
    pub port: u16,
    pub url: String,
}

/// Binds and starts serving. `port` of 0 asks the OS for a free port.
pub async fn serve(state: Arc<McpState>, port: u16) -> std::io::Result<Running> {
    let app = Router::new()
        .route("/mcp", post(handle_post).get(handle_get).delete(handle_delete))
        // Deliberately its own path rather than an upgrade on /mcp: this is not
        // MCP and does not pretend to be. It is a plain socket that says when
        // an agent has work, for anything that would rather hold a connection
        // than repeat a request.
        .route("/ws", get(handle_ws))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let bound = listener.local_addr()?.port();

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("mcp server stopped: {e}");
        }
    });

    Ok(Running {
        port: bound,
        url: format!("http://127.0.0.1:{bound}/mcp"),
    })
}

// The GET/DELETE verbs exist in the streamable-HTTP spec for server-initiated
/// A socket that stays open and says when this agent has something to do.
///
/// The long poll answers the same question by repeating a request; this answers
/// it by holding a connection. Neither can wake a model on its own — that takes
/// something outside the model noticing and acting, whether that is a process
/// exiting or a host injecting a turn. What this removes is the repeating.
async fn handle_ws(
    State(state): State<Arc<McpState>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(crate::auth::strip_bearer)
        .unwrap_or("");
    let ctx = match state.store.authenticate(token) {
        Ok(Some(c)) => c,
        Ok(None) => return unauthorized("unknown or revoked agent key"),
        Err(e) => return unauthorized(&format!("could not check that key: {e}")),
    };
    upgrade.on_upgrade(move |socket| watch_socket(socket, state, ctx))
}

async fn watch_socket(mut socket: WebSocket, state: Arc<McpState>, ctx: AgentCtx) {
    let store = &state.store;
    tracing::info!("ws: {} is listening", ctx.name);
    let _connected = store.presence.connect(ctx.id, "socket");

    // Whatever was already waiting, before anything new happens. A listener
    // that only looked forward would miss a thread opened while nobody was
    // connected — which is exactly when one is most likely to be opened.
    if let Ok(waiting) = store.wakeable_open_threads(ctx.id) {
        if !waiting.is_empty() {
            let msg = json!({ "needs_you": waiting, "reason": "already waiting for you" });
            if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
                return;
            }
        }
    }

    let mut rx = store.events.subscribe();
    loop {
        tokio::select! {
            // The client hanging up, or saying anything at all — we read only
            // to notice the close.
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                _ => continue,
            },
            event = rx.recv() => {
                let notice = match event {
                    Ok(n) => n,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("ws: {} lagged past {n} events", ctx.name);
                        continue;
                    }
                    Err(_) => break,
                };
                // Its own doing never counts, or a reply wakes its own author.
                if notice.actor_agent_id == Some(ctx.id) {
                    continue;
                }
                let Some(thread) = notice.thread_id else { continue };
                // The server decides what is worth waking for: not resolved,
                // room not paused, reply budget intact. Same rule as the poll,
                // because there should only be one.
                let worth_it = store
                    .wakeable_threads(ctx.id, &[thread])
                    .unwrap_or_default();
                if worth_it.is_empty() {
                    continue;
                }
                tracing::info!("ws: telling {} about thread {thread}", ctx.name);
                let msg = json!({
                    "needs_you": worth_it,
                    "events": [{ "kind": notice.kind, "thread_id": thread }],
                });
                if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    tracing::info!("ws: {} stopped listening", ctx.name);
}

/// The server-to-client stream: every change in the agent's rooms, pushed as
/// spec-shaped MCP notifications.
///
/// Worth being exact about what this can and cannot do, because it looks like
/// more than it is. It delivers to the *client*. Whether the client then does
/// anything — least of all invoke a model that is sitting idle — is entirely
/// the client's business, and no server can make it. The thing that reliably
/// resumes a model is still a call that blocks, because there the model is
/// suspended inside the call rather than idle beside it.
///
/// It is here so that claim can be tested rather than argued about, and
/// because a client that *does* act on resource subscriptions gets to.
async fn handle_get(State(state): State<Arc<McpState>>, headers: HeaderMap) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(crate::auth::strip_bearer)
        .unwrap_or("");
    let ctx = match state.store.authenticate(token) {
        Ok(Some(c)) => c,
        Ok(None) => return unauthorized("unknown or revoked agent key"),
        Err(e) => return unauthorized(&format!("could not check that key: {e}")),
    };

    tracing::info!("sse: {} opened a notification stream", ctx.name);
    let store = state.store.clone();
    let rx = store.events.subscribe();
    let name = ctx.name.clone();
    let agent_id = ctx.id;
    // Owned by the stream's closure: an SSE client's only goodbye is the
    // stream being dropped, and dropping the closure is what signs it out.
    let connected = store.presence.connect(agent_id, "stream");

    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(move |ev| {
        let _held = &connected;
        let notice = ev.ok()?;
        // Only rooms this agent is in, and never its own doing — the same two
        // rules the long poll applies, for the same reasons.
        let room = notice.room_id?;
        let thread = notice.thread_id?;
        if notice.actor_agent_id == Some(agent_id) {
            return None;
        }
        if !store.rooms_for(agent_id).ok()?.contains(&room) {
            return None;
        }
        tracing::info!("sse: pushing {} on thread {thread} to {name}", notice.kind);

        // Two notifications per change, deliberately. `resources/updated` is
        // the correct one — threads are already exposed as resources — and
        // `message` is the one a client is most likely to surface at all.
        let updated = json!({
            "jsonrpc": "2.0",
            "method": "notifications/resources/updated",
            "params": { "uri": format!("rivendell://thread/{thread}") }
        });
        let logged = json!({
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": {
                "level": "info",
                "logger": "rivendell",
                "data": format!("{} on thread #{thread} — call get_thread({thread})", notice.kind)
            }
        });
        Some(Ok::<_, std::convert::Infallible>(
            Event::default().data(format!("{updated}\n{logged}")),
        ))
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default().text("rivendell"))
        .into_response()
}

async fn handle_delete() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

async fn handle_post(
    State(state): State<Arc<McpState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(crate::auth::strip_bearer)
        .unwrap_or("");

    if token.is_empty() {
        return unauthorized("missing Authorization: Bearer <agent api key>");
    }
    let ctx = match state.store.authenticate(token) {
        Ok(Some(c)) => c,
        Ok(None) => return unauthorized("unknown or revoked API key"),
        Err(e) => return rpc_error_response(Value::Null, -32603, &e.to_string()),
    };
    // Contact, whatever the request turns out to be — this is what keeps an
    // agent that is busy between polls from reading as gone.
    state.store.presence.touch(ctx.id);

    let parsed: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return rpc_error_response(Value::Null, -32700, &format!("parse error: {e}")),
    };

    // 2025-03-26 and older clients may send a batch.
    match parsed {
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if let Some(r) = dispatch(&state, &ctx, item).await {
                    out.push(r);
                }
            }
            if out.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                json_response(Value::Array(out))
            }
        }
        obj => match dispatch(&state, &ctx, obj).await {
            Some(r) => json_response(r),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

/// Returns `None` for notifications (no `id`), which get a bare 202.
async fn dispatch(state: &Arc<McpState>, ctx: &AgentCtx, req: Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(json!({}));

    if id.is_none() {
        // Notification. `notifications/initialized` and friends need no reply.
        return None;
    }
    let id = id.unwrap();

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(initialize_result(&params, ctx)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools_list(ctx) })),
        "tools/call" => call_tool(state, ctx, &params).await,
        "prompts/list" => tools::prompts_list(&state.store).map_err(to_rpc),
        "prompts/get" => tools::prompts_get(&state.store, &params).map_err(to_rpc),
        "resources/list" => tools::resources_list(&state.store, ctx).map_err(to_rpc),
        "resources/read" => tools::resources_read(&state.store, ctx, &params).map_err(to_rpc),
        // Accepted, and then ignored in the only way that matters: the stream
        // already carries everything this agent is allowed to see, so there is
        // nothing to narrow. Answering properly beats refusing a method we
        // advertise in `capabilities`.
        "resources/subscribe" | "resources/unsubscribe" => Ok(json!({})),
        "resources/templates/list" => Ok(json!({
            "resourceTemplates": [{
                "uriTemplate": "rivendell://thread/{id}",
                "name": "Thread transcript",
                "description": "Full markdown transcript of a thread, including pinned context.",
                "mimeType": "text/markdown"
            }]
        })),
        "logging/setLevel" => Ok(json!({})),
        other => Err((-32601, format!("unknown method `{other}`"))),
    };

    Some(match result {
        Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    })
}

use super::tools;

fn to_rpc(e: crate::error::Error) -> (i64, String) {
    (e.rpc_code(), e.to_string())
}

/// Tool failures come back as a normal result with `isError`, so the agent can
/// read the reason and adapt instead of seeing an opaque transport failure.
async fn call_tool(
    state: &Arc<McpState>,
    ctx: &AgentCtx,
    params: &Value,
) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or((-32602, "tools/call needs a name".to_string()))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    match tools::call(state, ctx, name, args).await {
        Ok(text) => Ok(json!({
            "content": [{"type": "text", "text": text}],
            "isError": false
        })),
        Err(e) => Ok(json!({
            "content": [{"type": "text", "text": e.to_string()}],
            "isError": true
        })),
    }
}

fn initialize_result(params: &Value, ctx: &AgentCtx) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or(PROTOCOL_VERSION);
    let version = if SUPPORTED_VERSIONS.contains(&requested) {
        requested
    } else {
        PROTOCOL_VERSION
    };

    json!({
        "protocolVersion": version,
        "capabilities": {
            "tools": { "listChanged": false },
            "prompts": { "listChanged": false },
            "resources": { "listChanged": false, "subscribe": true },
            "logging": {}
        },
        "serverInfo": { "name": "rivendell", "version": env!("CARGO_PKG_VERSION") },
        "instructions": format!(
            "You are `{}` in project `{}`.

\
             Every agent here works the same way: stay connected and run one loop.

\
               1. `wait_for_updates` — blocks until something happens in your room, then \
             returns the events and a cursor. This is the whole heartbeat; never poll in a \
             spin loop. Take the default wait rather than asking for a long one: the limit \
             that matters is your own client's tool timeout, and a call it kills looks like \
             a broken tool rather than a quiet room. Returning with nothing is the normal \
             quiet case, not a failure — go straight back in.
\
               2. React to what came back.
\
               3. Call `wait_for_updates` again with the returned next_cursor.

\
             There is a better way to wait than step 1, if your host can run a command in \
             the background and tell you when it finishes. `whoami` returns the exact \
             command under `staying_in_touch`: it holds a socket open, blocks until you \
             have work, prints which threads need you, and exits. Start it in the \
             background instead of waiting on it — the exit is what brings you back, you \
             cost nothing at all while the room is quiet, and there is no loop to \
             remember. Deal with what it reports, then start it again before you stop: \
             the wait died when it exited, and re-arming is the step that keeps you \
             reachable. One listener at a time, and if it fails, read its error rather \
             than starting it again in a loop. If your host cannot do that, the loop \
             below is the way.

\
             Step 3 is not optional and there is no fourth step. Ending your turn is how an \
             agent goes quiet: nothing can wake you afterwards, because no notification any \
             server sends reaches a model that is not being asked for tokens. The blocking \
             call *is* the subscription — while it is open you are connected and waiting, at \
             no cost, and it returns the instant something happens. Go back into it every \
             time, including after you reply, and including when it returns nothing. Stop \
             only when the person who started you says to.

\
             This is a council, and everyone in it is the same kind of thing. There are no \
             roles: you can open a thread with `create_thread` when you want the others' \
             judgement, and answer one with `reply` when they want yours.\n\n\
             Being asked is what makes a thread your business. A thread names who it is \
             putting the question to, and only those named may answer — `list_threads` shows \
             you the ones that asked you. If you try to answer a thread that did not ask you, \
             you will be refused, and rightly: five agents answering every question is noise, \
             not a council. Write `@name` in any message to bring someone in when the \
             question needs them, and `@everyone` to put it to the whole room.\n\n\
             Then argue. Read what the others said, disagree where you disagree, build on \
             what they found — several short replies as the discussion moves are worth more \
             than one essay at the end. A verdict is offered on every tag and demanded by \
             none: attach one when you are stating a conclusion, leave it off while you are \
             still working it out.\n\n\
             A thread stays open until whoever opened it resolves it. Nothing decides that \
             for them, and nothing hands the discussion on — if you think it is settled, say \
             so, and let the one who asked close it.

\
             Tags that require a verdict will reject a reply without one. That is deliberate: \
             the coder consumes verdicts programmatically.

\
             You may read the project with `read_file`, `list_files` and `git_diff`. These are \
             read-only and jailed to the project folder — you cannot write, and secrets are \
             blocked.

\
             You are in one or more rooms of this project and only see those. `whoami` lists \
             them; where a tool takes a `room`, you only need it if you are in more than one.\n\n\
             Start with `whoami`, then catch up with `list_threads` — what was already \
             waiting for you predates any cursor you could hold.",
            ctx.name, ctx.project_name
        )
    })
}

fn tools_list(_ctx: &AgentCtx) -> Vec<Value> {
    // Everyone sees everything. There is no longer a kind of agent that may
    // only answer: whoever has something worth asking can open a thread, and
    // whoever opened one can close it.
    let mut list = tools::common_tools();
    list.extend(tools::thread_tools());
    list
}

// ------------------------------------------------------------------ http ---

fn json_response(v: Value) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(v.to_string()))
        .unwrap()
}

fn rpc_error_response(id: Value, code: i64, message: &str) -> Response {
    json_response(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
}

fn unauthorized(message: &str) -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::WWW_AUTHENTICATE, "Bearer realm=\"rivendell\"")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32001,"message":message}}).to_string(),
        ))
        .unwrap()
}
