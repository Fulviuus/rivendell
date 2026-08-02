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
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
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
// SSE and session teardown. We are stateless and push nothing, so we answer
// honestly rather than pretending to hold a session.
async fn handle_get() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, "this server does not open SSE streams").into_response()
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
            "resources": { "listChanged": false, "subscribe": false },
            "logging": {}
        },
        "serverInfo": { "name": "rivendell", "version": env!("CARGO_PKG_VERSION") },
        "instructions": format!(
            "You are `{}` ({}) in project `{}`.

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
             Step 3 is not optional and there is no fourth step. Ending your turn is how an \
             agent goes quiet: nothing can wake you afterwards, because no notification any \
             server sends reaches a model that is not being asked for tokens. The blocking \
             call *is* the subscription — while it is open you are connected and waiting, at \
             no cost, and it returns the instant something happens. Go back into it every \
             time, including after you reply, and including when it returns nothing. Stop \
             only when the person who started you says to.

\
             What you react to depends on your role, and that is the only difference between \
             us. A CODER opens threads with `create_thread` and closes them with \
             `resolve_thread`. An ASSISTANT watches for threads that need it — \
             `list_threads` shows those addressed to you or to everyone — claims it with \
             `claim_thread` so the room knows it is being worked on, reads it in full with \
             `get_thread`, and answers with `reply`. Write `@name` in any message to call another \
             agent in; they are notified and get their own chance to answer.\n\n\
             A thread waits indefinitely for its first answer. That answer opens a short window \
             for the others to claim; whoever stays silent through it is left out.

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
             Start with `whoami`.",
            ctx.name, ctx.role, ctx.project_name
        )
    })
}

fn tools_list(ctx: &AgentCtx) -> Vec<Value> {
    let mut list = tools::common_tools();
    if ctx.is_coder() {
        list.extend(tools::coder_tools());
    }
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
