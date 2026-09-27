//! An opencode v2 server API implemented on top of the Ante agent.
//!
//! The point of this shim is to let opencode's *own* TUI run unchanged:
//! `opencode2 --server http://127.0.0.1:PORT` cannot tell it apart from the real
//! server. Everything Ante has no concept of (LSP, MCP, formatters, OAuth, git
//! operations) is stubbed, and the pieces Ante does have (a session with a
//! streaming reply) are translated into the message/event shapes the TUI reads.
//!
//! Shapes come from `packages/protocol/openapi.json` on opencode's `v2` branch.
//! Note the envelope is *not* uniform: most reads answer `{location, data}`,
//! some (like `/api/config`) are a bare array.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ante_sdk::{
    ConnectOptions, OpSender, connect,
    protocol::{Evt, Op, SessionRequest},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::stream::{self, Stream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::broadcast;

/// Where the shim settles a request when the client does not say.
const DIRECTORY: &str = "/home/user";
const VERSION: &str = "2.0.18";
/// The model Ante is configured with, so the composer shows something real.
const MODEL: &str = "deepseek-v4.1-flash";
const PROVIDER: &str = "example";

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn location() -> Value {
    json!({
        "directory": DIRECTORY,
        "project": { "id": "prj_shim", "directory": DIRECTORY, "canonical": DIRECTORY },
    })
}

/// Every read that is scoped to a place answers in this envelope.
fn envelope(data: Value) -> Json<Value> {
    Json(json!({ "location": location(), "data": data }))
}

fn uid(prefix: &str) -> String {
    // Monotonic enough for a session/message id; the client only needs stability.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{prefix}_{:016x}{:04x}", now_ms(), n)
}

/// The Ante side of the shim: one connection, driven from two places — the
/// event pump reads it, request handlers write to it.
#[derive(Clone)]
struct Ante {
    /// Present once connected; ops reach Ante through it.
    ops: Arc<tokio::sync::Mutex<Option<OpSender>>>,
    /// The Ante session has been opened.
    started: Arc<std::sync::atomic::AtomicBool>,
    /// Which opencode session the events belong to. One Ante session is
    /// mirrored, so this is the most recent one the TUI opened.
    active: Arc<Mutex<Option<String>>>,
}

impl Ante {
    fn new() -> Self {
        Self {
            ops: Arc::new(tokio::sync::Mutex::new(None)),
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            active: Arc::new(Mutex::new(None)),
        }
    }
}

#[derive(Clone)]
struct Store {
    ante: Ante,
    sessions: Arc<Mutex<Vec<Value>>>,
    messages: Arc<Mutex<HashMap<String, Vec<Value>>>>,
    /// Frames fan out to every connected `/api/event` feed.
    events: broadcast::Sender<Value>,
}

impl Store {
    fn new() -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            sessions: Arc::new(Mutex::new(Vec::new())),
            messages: Arc::new(Mutex::new(HashMap::new())),
            events,
            ante: Ante::new(),
        }
    }

    /// Ephemeral event: no durable envelope.
    fn publish(&self, name: &str, data: Value) {
        self.publish_inner(name, data, None);
    }

    /// Durable event. `Payload` for a durable definition requires the
    /// `{aggregateID, seq, version}` envelope on top of the common fields.
    fn publish_durable(&self, name: &str, data: Value, aggregate: &str) {
        self.publish_inner(name, data, Some(aggregate.to_string()));
    }

    fn publish_inner(&self, name: &str, data: Value, aggregate: Option<String>) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut event = json!({
            "id": uid("evt"),
            "type": name,
            "created": now_ms(),
            "data": data,
        });
        if let Some(aggregate_id) = aggregate {
            event["durable"] = json!({
                "aggregateID": aggregate_id,
                "seq": SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                "version": 1,
            });
        }
        let _ = self.events.send(event);
    }

    fn session(&self, id: &str) -> Option<Value> {
        self.sessions.lock().ok()?.iter().find(|s| s["id"] == id).cloned()
    }
}

fn loc_plain() -> Value {
    json!({ "directory": DIRECTORY })
}

fn session_info(id: &str, title: &str) -> Value {
    json!({
        "id": id,
        "projectID": "prj_shim",
        "project": { "id": "prj_shim" },
        "cost": 0,
        "tokens": { "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
        "agent": "build",
        "model": { "id": MODEL, "providerID": PROVIDER, "variant": "default" },
        "time": { "created": now_ms(), "updated": now_ms(), "idle": now_ms(), "viewed": now_ms() },
        "location": loc_plain(),
        "title": title,
    })
}

fn assistant_content(text: &str) -> Value {
    json!([{ "type": "text", "text": text }])
}

/// Every request, so the client's own behaviour is the specification.
async fn log_request(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    {
        use std::io::Write;
        println!("{} {}", req.method(), req.uri());
        let _ = std::io::stdout().flush();
    }
    next.run(req).await
}

/// Mutations answer 204 No Content; the client treats anything else as an
/// unexpected status.
async fn no_content() -> axum::http::StatusCode {
    axum::http::StatusCode::NO_CONTENT
}

/// Debug switch for bisecting which tool event the client rejects:
/// `SHIM_TOOL_EVENTS=session.tool.input.started,session.tool.called`.
/// Unset means every tool event is emitted.
fn tool_event_enabled(name: &str) -> bool {
    match std::env::var("SHIM_TOOL_EVENTS") {
        Ok(list) => list.split(',').any(|item| item.trim() == name),
        Err(_) => true,
    }
}

/// `SHIM_SKIP_EVENTS=session.reasoning.started,session.reasoning.delta` drops
/// the named opencode events, for bisecting a bad payload.
fn event_skipped(name: &str) -> bool {
    std::env::var("SHIM_SKIP_EVENTS")
        .map(|list| list.split(',').any(|item| item.trim() == name))
        .unwrap_or(false)
}

/// The readable part of a tool result, for the tool cell.
fn result_text(result: &Value) -> String {
    match result {
        Value::String(text) => text.clone(),
        Value::Object(map) => map
            .get("stdout")
            .or_else(|| map.get("content"))
            .or_else(|| map.get("message"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| result.to_string()),
        other => other.to_string(),
    }
}

fn tokens_json(input: u32, output: u32) -> Value {
    json!({ "input": input, "output": output, "reasoning": 0,
            "cache": { "read": 0, "write": 0 } })
}

/// Connect to Ante and pump its events into opencode's event shapes.
///
/// One Ante session is mirrored into whichever opencode session the TUI most
/// recently opened. The reply is not faked: this is Ante's own output.
async fn spawn_ante(store: Store) {
    let endpoint: ante_sdk::Endpoint = match "stdio".parse() {
        Ok(endpoint) => endpoint,
        Err(err) => {
            eprintln!("ante: bad endpoint: {err}");
            return;
        }
    };
    let client = match connect(endpoint, ConnectOptions::default()).await {
        Ok(client) => client,
        Err(err) => {
            eprintln!("ante: connect failed: {err}");
            return;
        }
    };
    let (ops, mut rx) = client.into_parts();
    *store.ante.ops.lock().await = Some(ops);

    // A turn is made of steps (one model call each): Ante starts a step, may
    // call tools, then starts another. opencode models each step as its own
    // assistant message, so the step is opened lazily on its first content
    // event and closed once a tool ends.
    let mut message_id = String::new();
    let mut step_open = false;
    let mut text_started = false;
    let mut reasoning_open = false;
    let mut ordinal = 0u32;
    let mut tokens_in = 0u32;
    let mut tokens_out = 0u32;

    while let Some(msg) = rx.recv().await {
        let Some(session) = store.ante.active.lock().ok().and_then(|s| s.clone()) else {
            continue;
        };
        // Open a step (a new assistant message) on demand.
        macro_rules! ensure_step {
            () => {
                if !step_open {
                    step_open = true;
                    text_started = false;
                    reasoning_open = false;
                    ordinal = 0;
                    message_id = uid("msg");
                    store.publish_durable(
                        "session.step.started",
                        json!({
                            "sessionID": session,
                            "assistantMessageID": message_id,
                            "agent": "build",
                            "model": { "id": MODEL, "providerID": PROVIDER },
                            "started": now_ms(),
                        }),
                        &session,
                    );
                }
            };
        }
        match msg.event {
            Evt::TurnStart { .. } => {
                step_open = false;
                store.publish_durable(
                    "session.execution.started",
                    json!({ "sessionID": session }),
                    &session,
                );
            }
            Evt::ThinkingDelta(delta) => {
                ensure_step!();
                if !reasoning_open {
                    reasoning_open = true;
                    store.publish_durable(
                        "session.reasoning.started",
                        json!({ "sessionID": session, "assistantMessageID": message_id, "ordinal": 0 }),
                        &session,
                    );
                }
                store.publish(
                    "session.reasoning.delta",
                    json!({ "sessionID": session, "assistantMessageID": message_id, "delta": delta }),
                );
            }
            Evt::Thinking(text) => {
                reasoning_open = false;
                store.publish_durable(
                    "session.reasoning.ended",
                    json!({ "sessionID": session, "assistantMessageID": message_id, "ordinal": 0, "text": text }),
                    &session,
                );
            }
            Evt::MessageDelta(delta) => {
                ensure_step!();
                if !text_started {
                    text_started = true;
                    store.publish_durable(
                        "session.text.started",
                        json!({ "sessionID": session, "assistantMessageID": message_id, "ordinal": 0 }),
                        &session,
                    );
                }
                store.publish(
                    "session.text.delta",
                    json!({
                        "sessionID": session,
                        "assistantMessageID": message_id,
                        "ordinal": ordinal,
                        "delta": delta,
                    }),
                );
                ordinal += 1;
            }
            Evt::AgentMessage(text) => {
                ensure_step!();
                store.publish_durable(
                    "session.text.ended",
                    json!({
                        "sessionID": session,
                        "assistantMessageID": message_id,
                        "ordinal": ordinal,
                        "text": text,
                    }),
                    &session,
                );
            }
            Evt::ToolStart(tool) => {
                ensure_step!();
                let args = tool.args.to_string();
                store.publish_durable(
                    "session.tool.input.started",
                    json!({
                        "sessionID": session,
                        "assistantMessageID": message_id,
                        "id": tool.id,
                        "name": tool.name,
                    }),
                    &session,
                );
                store.publish_durable(
                    "session.tool.input.ended",
                    json!({
                        "sessionID": session,
                        "assistantMessageID": message_id,
                        "id": tool.id,
                        "text": args,
                    }),
                    &session,
                );
                store.publish_durable(
                    "session.tool.called",
                    json!({
                        "sessionID": session,
                        "assistantMessageID": message_id,
                        "id": tool.id,
                        "executed": false,
                        "input": tool.args,
                    }),
                    &session,
                );
            }
            Evt::ToolEnd(end) => {
                let failed = !matches!(end.status, ante_sdk::protocol::ToolEndStatus::Completed);
                let text = result_text(&end.result_json);
                let payload = if failed {
                    json!({
                        "sessionID": session,
                        "assistantMessageID": message_id,
                        "id": end.tool_use_id,
                        "error": text,
                        "metadata": {},
                        "content": [],
                        "executed": true,
                    })
                } else {
                    json!({
                        "sessionID": session,
                        "assistantMessageID": message_id,
                        "id": end.tool_use_id,
                        "metadata": {},
                        "content": [{ "type": "text", "text": text }],
                        "executed": true,
                    })
                };
                let name = if failed { "session.tool.failed" } else { "session.tool.success" };
                store.publish_durable(name, payload, &session);
                // The next content begins a fresh step (and message).
                step_open = false;
            }
            Evt::UsageUpdate { usage, .. } => {
                tokens_in = usage.input_tokens;
                tokens_out = usage.output_tokens;
            }
            Evt::TurnEnd { status, .. } => {
                let failed = matches!(status, ante_sdk::protocol::TurnEndStatus::Error { .. });
                if step_open {
                    store.publish_durable(
                        "session.step.ended",
                        json!({
                            "sessionID": session,
                            "assistantMessageID": message_id,
                            "finish": if failed { "error" } else { "stop" },
                            "cost": 0,
                            "tokens": tokens_json(tokens_in, tokens_out),
                        }),
                        &session,
                    );
                    step_open = false;
                }
                store.publish_durable(
                    if failed { "session.execution.failed" } else { "session.execution.succeeded" },
                    json!({ "sessionID": session }),
                    &session,
                );
            }
            // Approvals are next: Ante pauses the turn and waits for a decision.
            _ => {}
        }
    }
}

async fn health() -> Json<Value> {
    Json(json!({ "healthy": true, "version": VERSION, "pid": std::process::id() }))
}

async fn location_get() -> Json<Value> {
    Json(location())
}

async fn fs_list() -> Json<Value> {
    envelope(json!([]))
}

/// Agents, providers and models are not Ante concepts, but the composer needs
/// one of each before it will send: without them the prompt has no model and
/// the client silently refuses to submit.
async fn agents() -> Json<Value> {
    envelope(json!([{
        "id": "build",
        "name": "build",
        "mode": "primary",
        "hidden": false,
        "request": { "settings": {}, "headers": {}, "body": {} },
        "permissions": [{ "action": "*", "resource": "*", "effect": "allow" }],
        "model": { "id": MODEL, "providerID": PROVIDER },
    }]))
}

async fn models() -> Json<Value> {
    envelope(json!([{
        "id": MODEL, "modelID": MODEL, "providerID": PROVIDER,
        "name": "Example Model",
        "capabilities": {},
        "variants": [],
        "time": { "created": now_ms() },
        "cost": [],
        "status": "active",
        "enabled": true,
        "limit": {},
    }]))
}

async fn config() -> Json<Value> {
    // Bare array: the client calls findLast on it.
    Json(json!([]))
}

async fn config_providers() -> Json<Value> {
    envelope(json!({ "providers": [], "default": {} }))
}

async fn providers() -> Json<Value> {
    envelope(json!([{
        "id": PROVIDER, "name": "Example AI", "activation": "auto", "package": "example",
    }]))
}

async fn vcs() -> Json<Value> {
    envelope(json!({ "provider": "git", "branch": { "current": "main", "default": "main" } }))
}

async fn migration() -> Json<Value> {
    Json(json!({ "status": "completed" }))
}

async fn plugins() -> Json<Value> {
    envelope(json!([]))
}

async fn empty_reads() -> Json<Value> {
    envelope(json!([]))
}

/// `/api/session/active` and friends: the client's submit path reads
/// `.info.project.id`, so a bare empty envelope makes it throw.
async fn active_session() -> Json<Value> {
    Json(json!({ "info": { "project": { "id": "prj_shim" }, "id": null }, "location": location(), "data": null }))
}

async fn sessions_list(State(store): State<Store>) -> Json<Value> {
    let data = store.sessions.lock().map(|s| s.clone()).unwrap_or_default();
    Json(json!({ "data": data, "cursor": {} }))
}

async fn session_create(State(store): State<Store>) -> Json<Value> {
    let id = uid("ses");
    let info = session_info(&id, "Ante session");
    if let Ok(mut sessions) = store.sessions.lock() {
        sessions.push(info.clone());
    }
    store.messages.lock().map(|mut m| m.insert(id.clone(), Vec::new())).ok();
    store.publish_durable("session.created", json!({ "sessionID": id }), &id);
    Json(json!({ "data": info }))
}

async fn session_get(State(store): State<Store>, Path(id): Path<String>) -> Json<Value> {
    Json(store.session(&id).unwrap_or_else(|| session_info(&id, "Ante session")))
}

async fn session_messages(State(store): State<Store>, Path(id): Path<String>) -> Json<Value> {
    let data = store
        .messages
        .lock()
        .ok()
        .and_then(|m| m.get(&id).cloned())
        .unwrap_or_default();
    Json(json!({ "data": data, "cursor": {} }))
}

/// The prompt the user typed. For now the reply is a canned stream: the goal of
/// this step is to prove the message/event contract with the real TUI before
/// wiring Ante in behind it.
async fn session_prompt(
    State(store): State<Store>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let text = body.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let user_id = uid("msg");
    let user = json!({
        "id": user_id, "type": "user", "text": text, "time": { "created": now_ms() },
    });
    store.messages.lock().ok().and_then(|mut m| m.get_mut(&id).map(|list| list.push(user.clone())));
    store.publish("message.updated", json!({ "sessionID": id, "info": user }));

    // Point the event pump at this session, then hand the text to Ante.
    if let Ok(mut active) = store.ante.active.lock() {
        *active = Some(id.clone());
    }
    let ops = store.ante.ops.lock().await.clone();
    match ops {
        Some(ops) => {
            if !store.ante.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let _ = ops.send(ante_sdk::protocol::op_msg(Op::StartSession(SessionRequest::default()))).await;
            }
            if let Err(err) = ops.send(ante_sdk::protocol::op_msg(Op::UserInput(text.clone()))).await {
                eprintln!("ante: send failed: {err}");
            }
        }
        None => eprintln!("ante: not connected; prompt dropped"),
    }

    Json(json!({ "data": { "id": user_id, "sessionID": id, "type": "user", "time": { "created": now_ms() }, "payload": { "text": text }, "delivery": {} } }))
}

/// The server-scoped feed. Frames are `{id, type, data}` and the first one must
/// be `server.connected`, or the client gives up on the stream.
async fn events(State(store): State<Store>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = store.events.subscribe();
    let connected = json!({
        "id": "evt_0", "type": "server.connected", "created": now_ms(), "data": {},
    });
    let first = stream::once(async move { Ok(frame(&connected)) });
    let rest = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|item| async move {
        item.ok().map(|event| Ok(frame(&event)))
    });
    Sse::new(first.chain(rest)).keep_alive(KeepAlive::default())
}

fn frame(event: &Value) -> Event {
    let id = event["id"].as_str().unwrap_or("");
    let name = event["type"].as_str().unwrap_or("");
    Event::default().id(id.to_string()).event(name.to_string()).data(event.to_string())
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(41999);
    let store = Store::new();
    // Ante is connected lazily at startup; its events feed every opencode
    // session this shim serves.
    tokio::spawn(spawn_ante(store.clone()));
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/health", get(health))
        .route("/api/location", get(location_get))
        .route("/api/fs/list", get(fs_list))
        .route("/api/agent", get(agents))
        .route("/api/config", get(config))
        .route("/api/config/providers", get(config_providers))
        .route("/api/provider", get(providers))
        .route("/api/model", get(models))
        .route("/api/vcs", get(vcs))
        .route("/api/plugin", get(plugins))
        .route("/api/skill", get(empty_reads))
        .route("/api/command", get(empty_reads))
        .route("/api/mcp", get(empty_reads))
        .route("/api/experimental/migration/v1", get(migration))
        .route("/api/experimental/capabilities", get(empty_reads))
        .route("/api/session", get(sessions_list).post(session_create))
        .route("/api/session/active", get(active_session))
        .route("/api/session/{id}", get(session_get))
        .route("/api/session/{id}/message", get(session_messages))
        .route("/api/session/{id}/prompt", post(session_prompt))
        .route("/api/session/{id}/model", post(no_content))
        .route("/api/session/{id}/agent", post(no_content))
        .route("/api/session/{id}/view", post(no_content))
        .route("/api/event", get(events))
        .fallback(fallback)
        .layer(axum::middleware::from_fn(log_request))
        .with_state(store);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    println!("opencode-shim listening on http://127.0.0.1:{port}");
    axum::serve(listener, app).await.expect("serve");
}

/// Unknown reads answer with the envelope so list-shaped reads do not blow up;
/// the client's next error names the shape that is actually wanted.
async fn fallback() -> Response {
    Json(json!({
        "location": location(),
        "data": [],
        "info": { "project": { "id": "prj_shim" } },
    }))
    .into_response()
}
