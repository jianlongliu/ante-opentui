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
    protocol::{Evt, Id, Op, ReviewDecision, SessionRequest, ToolDecision, ToolUse, TurnPauseReason},
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

fn ante_home() -> std::path::PathBuf {
    std::env::var_os("ANTE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join(".ante")
        })
}

/// Ante's catalog is the source of truth for what can actually be run, so the
/// picker is fed from it: every provider and model configured in Ante shows up,
/// instead of the one the shim used to hard-code.
fn catalog() -> Value {
    std::fs::read_to_string(ante_home().join("catalog.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or_else(|| json!({ "providers": {} }))
}

/// The pair a session reports before one is picked; always a catalog entry.
fn model_for_session() -> (String, String) {
    active_model()
}

fn settings() -> Value {
    std::fs::read_to_string(ante_home().join("settings.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or_else(|| json!({}))
}

/// The pair Ante itself is configured on: `provider` plus that provider's entry
/// in `provider_model`. The picker is ordered to lead with it, so the client's
/// default matches what Ante will actually run.
fn active_model() -> (String, String) {
    let settings = settings();
    let provider = settings
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or(PROVIDER)
        .to_string();
    let model = settings
        .get("provider_model")
        .and_then(|v| v.get(&provider))
        .and_then(|v| v.as_str())
        .or_else(|| settings.get("model").and_then(|v| v.as_str()))
        .unwrap_or(MODEL)
        .to_string();
    (provider, model)
}

fn catalog_models() -> Vec<Value> {
    let catalog = catalog();
    let Some(providers) = catalog.get("providers").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let (active_provider, active_model) = active_model();
    let mut out = Vec::new();
    let mut providers: Vec<(&String, &Value)> = providers.iter().collect();
    // Active provider's models lead, and the active model leads among them.
    providers.sort_by_key(|(id, _)| if **id == active_provider { 0 } else { 1 });
    for (provider, spec) in providers {
        let Some(models) = spec.get("preferred_models").and_then(|v| v.as_array()) else {
            continue;
        };
        for model in models {
            let Some(id) = model.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let name = model.get("description").and_then(|v| v.as_str()).unwrap_or(id);
            let context = model.get("context_limit").and_then(|v| v.as_u64()).unwrap_or(0);
            let limit = if context > 0 { json!({ "context": context }) } else { json!({}) };
            out.push(json!({
                "id": id,
                "modelID": id,
                "providerID": provider,
                "preferred": provider == &active_provider && id == active_model,
                "name": name,
                "capabilities": {},
                "variants": [],
                "time": { "created": now_ms() },
                "cost": [],
                "status": "active",
                "enabled": true,
                "limit": limit,
            }));
        }
    }
    // Stable, deterministic order for the picker; the active model first.
    out.sort_by_key(|model| {
        let is_active = model.get("preferred").and_then(|v| v.as_bool()).unwrap_or(false);
        let id = model.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        (if is_active { 0 } else { 1 }, id)
    });
    out
}

fn catalog_providers() -> Vec<Value> {
    let catalog = catalog();
    let Some(providers) = catalog.get("providers").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let (active_provider, _) = active_model();
    let mut providers: Vec<(&String, &Value)> = providers.iter().collect();
    providers.sort_by_key(|(id, _)| if **id == active_provider { 0 } else { 1 });
    providers
        .into_iter()
        .map(|(id, spec)| {
            let name = spec
                .get("display_name")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .to_string();
            json!({ "id": id, "name": name, "activation": "auto", "package": id })
        })
        .collect()
}

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
    /// A pause waiting for a decision: the Ante turn and tools, keyed by the
    /// request id the TUI will reply with.
    pending: Arc<Mutex<Option<PendingApproval>>>,
    /// The agent the client last created a session with. opencode's agents are
    /// permission configs, so this decides Ante's permission mode.
    agent: Arc<Mutex<String>>,
    /// Provider and model the client last picked, as Ante names them.
    model: Arc<Mutex<Option<(String, String)>>>,
    /// Last prompt, used as the `recent` field compaction events require.
    last_user: Arc<Mutex<String>>,
}

/// An Ante turn paused for approval, held until the TUI answers.
struct PendingApproval {
    request_id: String,
    turn_id: Id,
    tools: Vec<ToolUse>,
}

impl Ante {
    fn new() -> Self {
        Self {
            ops: Arc::new(tokio::sync::Mutex::new(None)),
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            active: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(None)),
            agent: Arc::new(Mutex::new(reported_agent().to_string())),
            model: Arc::new(Mutex::new(None)),
            last_user: Arc::new(Mutex::new(String::new())),
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
        if let Some(path) = std::env::var_os("ANTE_SHIM_TRACE") {
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path)
            {
                let _ = file.write_all(format!("PUB {name} {data}\n").as_bytes());
            }
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
        "agent": reported_agent(),
        "model": { "id": model_for_session().1, "providerID": model_for_session().0, "variant": "default" },
        "time": { "created": now_ms(), "updated": now_ms(), "idle": now_ms(), "viewed": now_ms() },
        "location": loc_plain(),
        "title": title,
    })
}

fn assistant_content(text: &str) -> Value {
    json!([{ "type": "text", "text": text }])
}

/// Every request, so the client's own behaviour is the specification.
/// One-command mode hands the terminal to the TUI, so request logs must not go to
/// stdout — they would scribble over the interface. `None` (serve mode) keeps
/// them on stdout, which is what a server-only run wants.
static LOG_FILE: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

async fn log_request(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    use std::io::Write as _;
    let line = format!("{} {}", req.method(), req.uri());
    match LOG_FILE.get().and_then(|slot| slot.as_ref()) {
        Some(path) => {
            if let Ok(mut file) =
                std::fs::OpenOptions::new().create(true).append(true).open(path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
        None => {
            println!("{line}");
            let _ = std::io::stdout().flush();
        }
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

/// Clip to a character budget, for session titles.
fn short(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let head: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{head}…")
}

/// With `ANTE_SHIM_TRACE=<file>`, record every Ante event variant. It sits above
/// the active-session check on purpose: events that arrive with no open
/// opencode session are exactly the ones worth seeing.
fn trace_ante(event: &Evt) {
    let Some(path) = std::env::var_os("ANTE_SHIM_TRACE") else {
        return;
    };
    use std::io::Write as _;
    let debug = format!("{event:?}");
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        // The payload matters when diagnosing a silent turn, so keep it whole.
        let _ = file.write_all(format!("{debug}\n").as_bytes());
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
/// The `ante` host to spawn: `$ANTE_BIN`, else `ante` on `PATH`, else Ante's own
/// install location. Without this the shim only worked if the caller had already
/// put `~/.ante/bin` on `PATH`.
fn ante_executable() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("ANTE_BIN") {
        let path = std::path::PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }
    if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("ante"))
            .find(|candidate| candidate.is_file())
    }) {
        return Some(path);
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    let fallback = home.join(".ante/bin/ante");
    fallback.is_file().then_some(fallback)
}

/// Which opencode client to hand the terminal to.
fn client_executable() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("ANTEX_CLIENT") {
        return std::path::PathBuf::from(path);
    }
    for name in ["opencode2", "opencode"] {
        if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(name))
                .find(|candidate| candidate.is_file())
        }) {
            return path;
        }
    }
    std::path::PathBuf::from("opencode2")
}

async fn spawn_ante(store: Store) {
    let endpoint: ante_sdk::Endpoint = match "stdio".parse() {
        Ok(endpoint) => endpoint,
        Err(err) => {
            eprintln!("ante: bad endpoint: {err}");
            return;
        }
    };
    let mut options = ConnectOptions::default();
    options.executable = ante_executable();
    let client = match connect(endpoint, options).await {
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
    let mut streamed = false;
    let mut reasoning_open = false;
    let mut ordinal = 0u32;
    let mut tokens_in = 0u32;
    let mut tokens_out = 0u32;
    let mut compact_block: Option<String> = None;
    let mut compact_text = String::new();

    while let Some(msg) = rx.recv().await {
        trace_ante(&msg.event);
        let Some(session) = store.ante.active.lock().ok().and_then(|s| s.clone()) else {
            continue;
        };
        // Open a step (a new assistant message) on demand.
        macro_rules! ensure_step {
            () => {
                if !step_open {
                    step_open = true;
                    text_started = false;
                    streamed = false;
                    reasoning_open = false;
                    ordinal = 0;
                    message_id = uid("msg");
                    let (provider, model) = store
                        .ante
                        .model
                        .lock()
                        .ok()
                        .and_then(|slot| slot.clone())
                        .unwrap_or_else(|| active_model());
                    store.publish_durable(
                        "session.step.started",
                        json!({
                            "sessionID": session,
                            "assistantMessageID": message_id,
                            "agent": reported_agent(),
                            "model": { "id": model, "providerID": provider },
                            "started": now_ms(),
                        }),
                        &session,
                    );
                }
            };
        }
        // The real server marks each step once it starts streaming.
        macro_rules! ensure_streamed {
            () => {
                if step_open && !streamed {
                    streamed = true;
                    store.publish_durable(
                        "session.step.streamed",
                        json!({ "sessionID": session, "assistantMessageID": message_id }),
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
                ensure_streamed!();
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
                ensure_streamed!();
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
                        // SessionError.Error is a struct, not a string; a bare
                        // string fails the codec and the whole event is dropped,
                        // which leaves the tool cell spinning forever.
                        "error": { "type": "tool_error", "message": text },
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
            // Ante reports a compaction as an info block (`compact-op_…`), not
            // through CompactStart/CompactEnd — those only fire for a real
            // reduction, and the no-op path ("nothing to compact") sends just the
            // block. Both are mirrored so the client never sits on "queued".
            Evt::InfoBlockStart { id: block, header, .. } => {
                if block.starts_with("compact") {
                    compact_block = Some(block.clone());
                    compact_text.clear();
                    let recent = store.ante.last_user.lock().map(|s| s.clone()).unwrap_or_default();
                    store.publish_durable(
                        "session.compaction.started",
                        json!({ "sessionID": session, "reason": "manual", "recent": recent }),
                        &session,
                    );
                    compact_text = header;
                }
            }
            Evt::InfoBlockAppend { id: block, detail } => {
                if compact_block.as_deref() == Some(block.as_str()) {
                    store.publish_durable(
                        "session.compaction.delta",
                        json!({ "sessionID": session, "text": detail }),
                        &session,
                    );
                    // Ante has no end event: the first detail means the block is
                    // no longer loading, so this is where the client's item settles.
                    let text = if compact_text.is_empty() {
                        detail.clone()
                    } else {
                        format!("{compact_text} — {detail}")
                    };
                    let model_now = store
                        .ante
                        .model
                        .lock()
                        .ok()
                        .and_then(|slot| slot.clone())
                        .unwrap_or_else(active_model);
                    let recent = store.ante.last_user.lock().map(|s| s.clone()).unwrap_or_default();
                    store.publish_durable(
                        "session.compaction.ended",
                        json!({
                            "sessionID": session,
                            "reason": "manual",
                            "text": text,
                            "recent": recent,
                            "model": { "id": model_now.1, "providerID": model_now.0 },
                        }),
                        &session,
                    );
                    compact_block = None;
                }
            }
            // Ante compacts for real; mirror it as opencode's compaction events.
            Evt::CompactStart => {
                let recent = store.ante.last_user.lock().map(|s| s.clone()).unwrap_or_default();
                store.publish_durable(
                    "session.compaction.started",
                    json!({ "sessionID": session, "reason": "manual", "recent": recent }),
                    &session,
                );
            }
            Evt::CompactEnd { summary } => {
                let recent = store.ante.last_user.lock().map(|s| s.clone()).unwrap_or_default();
                let model_now = store
                    .ante
                    .model
                    .lock()
                    .ok()
                    .and_then(|slot| slot.clone())
                    .unwrap_or_else(active_model);
                match summary {
                    Some(text) => store.publish_durable(
                        "session.compaction.ended",
                        json!({
                            "sessionID": session,
                            "reason": "manual",
                            "text": text,
                            "recent": recent,
                            "model": { "id": model_now.1, "providerID": model_now.0 },
                        }),
                        &session,
                    ),
                    None => store.publish_durable(
                        "session.compaction.failed",
                        json!({
                            "sessionID": session,
                            "reason": "manual",
                            "recent": recent,
                            "error": { "type": "compaction_failed", "message": "Ante did not produce a summary" },
                        }),
                        &session,
                    ),
                }
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
                let mut failure = None;
                if let ante_sdk::protocol::TurnEndStatus::Error { kind, headline, details } = &status {
                    // Without this the failure is invisible: the turn ends with no
                    // text, so the TUI just sits there looking idle.
                    ensure_step!();
                    let message = if details.is_empty() {
                        headline.clone()
                    } else {
                        format!("{headline} — {}", details.join("; "))
                    };
                    let http_status = details
                        .iter()
                        .find_map(|detail| detail.strip_prefix("HTTP "))
                        .and_then(|rest| rest.split_whitespace().next())
                        .and_then(|code| code.parse::<i64>().ok())
                        .filter(|code| (100..=599).contains(code));
                    let mut error = json!({
                        "type": kind.clone().unwrap_or_else(|| "error".to_string()),
                        "message": message,
                    });
                    if let Some(code) = http_status {
                        error["status"] = json!(code);
                    }
                    store.publish_durable(
                        "session.step.failed",
                        json!({
                            "sessionID": session,
                            "assistantMessageID": message_id,
                            "error": error.clone(),
                            "executed": false,
                        }),
                        &session,
                    );
                    failure = Some(error);
                }
                // `session.execution.failed` carries the error too — its schema
                // requires it, and the client reads `error.message` off it.
                store.publish_durable(
                    if failed { "session.execution.failed" } else { "session.execution.succeeded" },
                    match &failure {
                        Some(error) => json!({ "sessionID": session, "error": error }),
                        None => json!({ "sessionID": session }),
                    },
                    &session,
                );
            }
            // Ante pauses the turn for a decision; the TUI shows this as a
            // permission prompt and answers on the reply route.
            Evt::TurnPause {
                turn_id,
                reason: TurnPauseReason::Approval { tools, .. },
            } => {
                let request_id = uid("per");
                let first = tools.first();
                let payload = json!({
                    "id": request_id,
                    "sessionID": session,
                    "action": first.map(|tool| tool.name.clone()).unwrap_or_default(),
                    "resources": first
                        .map(|tool| vec![tool.args.to_string()])
                        .unwrap_or_default(),
                });
                *store.ante.pending.lock().unwrap() = Some(PendingApproval {
                    request_id: request_id.clone(),
                    turn_id,
                    tools,
                });
                store.publish("permission.asked", payload);
            }
            _ => {}
        }
    }
}

/// What the TUI polls to learn about a pending permission prompt.
fn pending_requests(store: &Store) -> Json<Value> {
    let requests: Vec<Value> = match store.ante.pending.lock() {
        Ok(guard) => guard
            .iter()
            .map(|pending| {
                let first = pending.tools.first();
                json!({
                    "id": pending.request_id,
                    "sessionID": store.ante.active.lock().ok().and_then(|s| s.clone()).unwrap_or_default(),
                    "action": first.map(|tool| tool.name.clone()).unwrap_or_default(),
                    "resources": first.map(|tool| vec![tool.args.to_string()]).unwrap_or_default(),
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    envelope(json!(requests))
}

async fn permission_request(State(store): State<Store>) -> Json<Value> {
    pending_requests(&store)
}

async fn session_permissions(State(store): State<Store>) -> Json<Value> {
    let requests: Vec<Value> = match store.ante.pending.lock() {
        Ok(guard) => guard
            .iter()
            .map(|pending| {
                let first = pending.tools.first();
                json!({
                    "id": pending.request_id,
                    "sessionID": store.ante.active.lock().ok().and_then(|s| s.clone()).unwrap_or_default(),
                    "action": first.map(|tool| tool.name.clone()).unwrap_or_default(),
                    "resources": first.map(|tool| vec![tool.args.to_string()]).unwrap_or_default(),
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    Json(json!({ "data": requests }))
}

/// The TUI's answer. `once`/`always`/`reject` map onto Ante's review decisions.
async fn permission_reply(
    State(store): State<Store>,
    Path((session, request_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> axum::http::StatusCode {
    let decision = match body.get("decision").and_then(|v| v.as_str()) {
        Some("once") => ReviewDecision::Accept,
        Some("always") => ReviewDecision::AcceptAlways,
        Some("reject") => ReviewDecision::Deny,
        other => {
            eprintln!("permission: unknown decision {other:?}");
            return axum::http::StatusCode::BAD_REQUEST;
        }
    };
    let taken = store
        .ante
        .pending
        .lock()
        .ok()
        .and_then(|mut guard| match guard.as_ref() {
            Some(pending) if pending.request_id == request_id => guard.take(),
            _ => None,
        });
    let Some(pending) = taken else {
        eprintln!("permission: no pending request {request_id}");
        return axum::http::StatusCode::NOT_FOUND;
    };

    let responses: Vec<ToolDecision> = pending
        .tools
        .iter()
        .map(|tool| ToolDecision {
            tool_use_id: tool.id.clone(),
            decision: decision.clone(),
            message: None,
        })
        .collect();
    if let Some(ops) = store.ante.ops.lock().await.clone() {
        let op = Op::ApprovalResponse { turn_id: pending.turn_id, responses };
        if let Err(err) = ops.send(ante_sdk::protocol::op_msg(op)).await {
            eprintln!("permission: send failed: {err}");
        }
    }
    store.publish(
        "permission.replied",
        json!({ "sessionID": session, "requestID": request_id, "reply": body.get("decision") }),
    );
    axum::http::StatusCode::NO_CONTENT
}

/// Esc in the TUI. The client owns the gesture; this just relays it to Ante.
/// `/compact` — Ante has a real compaction op, so this forwards rather than fakes
/// it. The response shape is the inbox item the client expects to get back.
async fn session_compact(
    State(store): State<Store>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Json<Value> {
    if let Some(ops) = store.ante.ops.lock().await.clone() {
        let op = Op::Compact { instructions: None };
        if let Err(err) = ops.send(ante_sdk::protocol::op_msg(op)).await {
            eprintln!("compact: send failed: {err}");
        }
    } else {
        eprintln!("compact: not connected; request dropped");
    }
    let input_id = body
        .as_ref()
        .and_then(|Json(value)| value.get("id"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| uid("msg"));
    // Point the event pump here, then run the inbox handshake the client needs to
    // move the item out of "queued" and into the transcript.
    if let Ok(mut active) = store.ante.active.lock() {
        *active = Some(id.clone());
    }
    let item = json!({
        "id": input_id,
        "sessionID": id,
        "time": { "created": now_ms() },
        "type": "compaction",
        "payload": {},
        "delivery": {},
    });
    store.publish_durable(
        "session.inbox.enqueued",
        json!({ "inboxID": input_id, "sessionID": id, "item": item }),
        &id,
    );
    store.publish_durable(
        "session.inbox.delivered",
        json!({ "inboxID": input_id, "sessionID": id }),
        &id,
    );
    Json(json!({ "data": item }))
}

/// The model picker lands here (`{model:{id,providerID}}`); Ante takes the pair
/// directly, so the switch is real.
async fn session_model(
    State(store): State<Store>,
    Path(_id): Path<String>,
    Json(body): Json<Value>,
) -> axum::http::StatusCode {
    let model = body.get("model").cloned().unwrap_or_default();
    let id = model.get("id").and_then(|v| v.as_str()).unwrap_or(MODEL).to_string();
    let provider = model
        .get("providerID")
        .and_then(|v| v.as_str())
        .unwrap_or(PROVIDER)
        .to_string();
    // Read the outgoing pair before overwriting it, or `previous` ends up
    // reporting the new model.
    let previous = store
        .ante
        .model
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .map(|(provider, id)| json!({ "id": id, "providerID": provider }));
    if let Ok(mut slot) = store.ante.model.lock() {
        *slot = Some((provider.clone(), id.clone()));
    }
    if let Some(session) = store.ante.active.lock().ok().and_then(|s| s.clone()) {
        let mut payload = json!({
            "sessionID": session,
            "model": { "id": id.clone(), "providerID": provider.clone() },
        });
        if let Some(previous) = previous {
            payload["previous"] = previous;
        }
        store.publish_durable("session.model.selected", payload, &session);
    }
    // Only meaningful once a session exists; the stored pair is applied when the
    // session starts, and Ante answers "session not initialized" before that.
    if store.ante.started.load(std::sync::atomic::Ordering::SeqCst)
        && let Some(ops) = store.ante.ops.lock().await.clone()
    {
        let update = ante_sdk::protocol::SessionUpdate {
            provider: Some(provider),
            model: Some(ante_sdk::protocol::ModelSpec { id, ..Default::default() }),
            ..Default::default()
        };
        if let Err(err) = ops.send(ante_sdk::protocol::op_msg(Op::UpdateSession(update))).await {
            eprintln!("model: send failed: {err}");
        }
    }
    axum::http::StatusCode::NO_CONTENT
}

/// `shift+tab` lands here; the agent picks Ante's permission mode.
async fn session_agent(
    State(store): State<Store>,
    Path(_id): Path<String>,
    Json(body): Json<Value>,
) -> axum::http::StatusCode {
    let agent = body.get("agent").and_then(|v| v.as_str()).unwrap_or_else(|| reported_agent()).to_string();
    let mode = permission_mode_for(&agent);
    if store.ante.started.load(std::sync::atomic::Ordering::SeqCst)
        && let Some(ops) = store.ante.ops.lock().await.clone()
    {
        let update = ante_sdk::protocol::SessionUpdate {
            permission_mode: Some(mode),
            ..Default::default()
        };
        if let Err(err) = ops.send(ante_sdk::protocol::op_msg(Op::UpdateSession(update))).await {
            eprintln!("agent: send failed: {err}");
        }
    }
    axum::http::StatusCode::NO_CONTENT
}

async fn session_interrupt(State(store): State<Store>, Path(_id): Path<String>) -> Json<Value> {
    let mut interrupted = false;
    if let Some(ops) = store.ante.ops.lock().await.clone() {
        match ops.send(ante_sdk::protocol::op_msg(Op::Interrupt)).await {
            Ok(()) => interrupted = true,
            Err(err) => eprintln!("interrupt: send failed: {err}"),
        }
    }
    Json(json!({ "interrupted": interrupted }))
}

async fn health() -> Json<Value> {
    Json(json!({ "healthy": true, "version": VERSION, "pid": std::process::id() }))
}

async fn location_get() -> Json<Value> {
    Json(location())
}

/// `@` completion walks the working tree, so this reads the real filesystem —
/// Ante has no file API of its own to proxy.
fn fs_entries(dir: &std::path::Path, limit: usize) -> Vec<Value> {
    let mut entries: Vec<Value> = Vec::new();
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            let path = entry.path();
            let kind = if path.is_dir() { "directory" } else { "file" };
            entries.push(json!({
                "path": path.to_string_lossy(),
                "type": kind,
            }));
            if entries.len() >= limit {
                break;
            }
        }
    }
    entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    entries
}

/// Landing points for panels Ante has nothing to fill: the shape still has to
/// match the schema, or the client validates the response away and the panel
/// degrades silently instead of showing up empty.
async fn empty_object() -> Json<Value> {
    envelope(json!({}))
}

async fn bare_empty_array() -> Json<Value> {
    // `/api/project` answers a bare array, like `/api/config`.
    Json(json!([]))
}

async fn vcs_base_empty() -> Json<Value> {
    envelope(json!(null))
}

async fn mcp_resources_empty() -> Json<Value> {
    envelope(json!({ "resources": [], "templates": [] }))
}

async fn session_terminals_empty() -> Json<Value> {
    Json(json!({ "data": [] }))
}

async fn fs_list(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let base = params
        .get("path")
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or_else(|| DIRECTORY.to_string());
    envelope(json!(fs_entries(std::path::Path::new(&base), 500)))
}

/// Depth-limited search so a stray `@` does not walk the whole disk.
async fn fs_find(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let query = params.get("query").cloned().unwrap_or_default().to_lowercase();
    let limit: usize = params.get("limit").and_then(|v| v.parse().ok()).unwrap_or(50);
    let root = params
        .get("path")
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or_else(|| DIRECTORY.to_string());
    let mut found: Vec<Value> = Vec::new();
    let root = std::path::PathBuf::from(root);
    let mut queue = std::collections::VecDeque::from([(root, 0usize)]);
    while let Some((dir, depth)) = queue.pop_front() {
        if depth > 6 || found.len() >= limit {
            break;
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "node_modules" || name == ".git" {
                continue;
            }
            let is_dir = path.is_dir();
            if name.to_lowercase().contains(&query) {
                found.push(json!({
                    "path": path.to_string_lossy(),
                    "type": if is_dir { "directory" } else { "file" },
                }));
                if found.len() >= limit {
                    break;
                }
            }
            if is_dir {
                queue.push_back((path, depth + 1));
            }
        }
    }
    envelope(json!(found))
}

/// Agents, providers and models are not Ante concepts, but the composer needs
/// one of each before it will send: without them the prompt has no model and
/// the client silently refuses to submit.
/// opencode's agents are configs with permissions; Ante's nearest equivalent is
/// the permission mode, so each agent here *is* one — switching is real, not
/// cosmetic. Names are Ante's own (`strict`/`auto`/`yolo`) so the label needs no
/// translation. `shift+tab` cycles them.
const AGENTS: [&str; 3] = ["strict", "auto", "yolo"];

fn agent_description(agent: &str) -> &'static str {
    match agent {
        "strict" => "Ask unless provably safe（Ante 的默认）",
        "yolo" => "Never ask（跳过全部检查）",
        _ => "Act unless provably dangerous",
    }
}

/// The agent a session reports. It **must** be one of `AGENTS`: the client looks
/// it up, and `submit.ts` silently returns when it cannot resolve one.
fn reported_agent() -> &'static str {
    let configured = configured_permission_mode();
    AGENTS.iter().find(|id| **id == configured).copied().unwrap_or("auto")
}

fn permission_mode_for(agent: &str) -> ante_sdk::protocol::PermissionMode {
    match agent {
        "strict" => ante_sdk::protocol::PermissionMode::Strict,
        "yolo" => ante_sdk::protocol::PermissionMode::Yolo,
        _ => ante_sdk::protocol::PermissionMode::Auto,
    }
}

/// The mode Ante is configured with leads, so a bare `shift+tab` first lands on
/// what the user already expects.
fn configured_permission_mode() -> String {
    match settings().get("permission_mode").and_then(|v| v.as_str()) {
        Some(name) => name.to_string(),
        None => "auto".to_string(),
    }
}

async fn agents() -> Json<Value> {
    let configured = configured_permission_mode();
    let mut order: Vec<&str> = AGENTS.iter().copied().collect();
    order.sort_by_key(|id| if *id == configured { 0 } else { 1 });
    let list: Vec<Value> = order
        .iter()
        .map(|id| {
            let description = agent_description(id);
            json!({
                "id": id,
                "name": id,
                "description": description,
                "mode": "primary",
                "hidden": false,
                "request": { "settings": {}, "headers": {}, "body": {} },
                "permissions": [{ "action": "*", "resource": "*", "effect": "allow" }],
                "model": { "id": MODEL, "providerID": PROVIDER },
            })
        })
        .collect();
    envelope(json!(list))
}

async fn models() -> Json<Value> {
    let mut list = catalog_models();
    if list.is_empty() {
        // No readable catalog: still show the model Ante is actually on.
        let (provider, model) = active_model();
        list.push(json!({
            "id": model, "modelID": model, "providerID": provider,
            "name": "Example Model",
            "capabilities": {},
            "variants": [],
            "time": { "created": now_ms() },
            "cost": [],
            "status": "active",
            "enabled": true,
            "limit": {},
        }));
    }
    envelope(json!(list))
}

async fn config() -> Json<Value> {
    // Bare array: the client calls findLast on it.
    Json(json!([]))
}

async fn config_providers() -> Json<Value> {
    envelope(json!({ "providers": [], "default": {} }))
}

async fn providers() -> Json<Value> {
    let mut list = catalog_providers();
    if list.is_empty() {
        list.push(json!({
            "id": PROVIDER, "name": "Example AI", "activation": "auto", "package": "example",
        }));
    }
    envelope(json!(list))
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

/// `/inbox` and `/form` answer `{data: []}` and nothing else — their schemas set
/// `additionalProperties: false`, so the usual `location` field makes the whole
/// response fail validation (and the session view then refuses to open).
async fn bare_empty() -> Json<Value> {
    Json(json!({ "data": [] }))
}

/// `/api/session/active` and friends: the client's submit path reads
/// `.info.project.id`, so a bare empty envelope makes it throw.
async fn active_session() -> Json<Value> {
    Json(json!({ "data": {} }))
}

/// Ante keeps its session metadata on disk; opencode wants a session list, so
/// this reads that directory and maps it into `Session.Info`.
fn ante_sessions() -> Vec<Value> {
    let home = std::env::var_os("ANTE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
            home.join(".ante")
        });
    let Ok(entries) = std::fs::read_dir(home.join("sessions")) else {
        return Vec::new();
    };

    let mut sessions: Vec<(i64, Value)> = entries
        .flatten()
        .filter_map(|entry| {
            let raw = std::fs::read_to_string(entry.path().join("meta.json")).ok()?;
            let meta: Value = serde_json::from_str(&raw).ok()?;
            let id = meta.get("id")?.as_str()?.to_string();
            let created = meta
                .get("started_time")
                .and_then(|v| v.as_str())
                .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                .map(|when| when.timestamp_millis())
                .unwrap_or_else(now_ms);
            let directory = meta
                .get("dir")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let title = meta
                .get("first_user_message")
                .and_then(|v| v.as_str())
                .map(|text| short(&text.replace(['\n', '\r'], " "), 60))
                .unwrap_or_else(|| "untitled".into());
            let usage = meta.get("usage").cloned().unwrap_or_else(|| json!({}));
            let tokens = tokens_json(
                usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            );
            let model = meta.get("model").and_then(|v| v.as_str()).unwrap_or(MODEL);
            let provider = meta.get("provider").and_then(|v| v.as_str()).unwrap_or(PROVIDER);
            let info = json!({
                "id": id,
                "projectID": "prj_shim",
                "agent": reported_agent(),
                "model": { "id": model, "providerID": provider },
                "cost": 0,
                "tokens": tokens,
                "time": { "created": created, "updated": created },
                "title": title,
                "location": { "directory": directory },
            });
            Some((created, info))
        })
        .collect();

    // Newest first, which is the order the picker shows.
    sessions.sort_by(|a, b| b.0.cmp(&a.0));
    sessions.into_iter().map(|(_, info)| info).collect()
}

async fn sessions_list() -> Json<Value> {
    Json(json!({ "data": ante_sessions(), "cursor": {} }))
}

async fn session_create(
    State(store): State<Store>,
    body: Option<Json<Value>>,
) -> Json<Value> {
    if let Some(path) = std::env::var_os("ANTE_SHIM_TRACE") {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(
                format!("CREATE {:?}\n", body.as_ref().map(|Json(v)| v)).as_bytes(),
            );
        }
    }
    // The client picks the session id and the agent; honour both. Ignoring the
    // id made the two sides disagree about which session was open.
    let id = body
        .as_ref()
        .and_then(|Json(value)| value.get("id"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| uid("ses"));
    if let Some(agent) = body
        .as_ref()
        .and_then(|Json(value)| value.get("agent"))
        .and_then(|value| value.as_str())
        && let Ok(mut slot) = store.ante.agent.lock()
    {
        *slot = agent.to_string();
    }
    if let Some(model) = body.as_ref().and_then(|Json(value)| value.get("model"))
        && let (Some(id), Some(provider)) = (
            model.get("id").and_then(|value| value.as_str()),
            model.get("providerID").and_then(|value| value.as_str()),
        )
        && let Ok(mut slot) = store.ante.model.lock()
    {
        *slot = Some((provider.to_string(), id.to_string()));
    }
    let info = session_info(&id, "Ante session");
    if let Ok(mut sessions) = store.sessions.lock() {
        sessions.push(info.clone());
    }
    store.messages.lock().map(|mut m| m.insert(id.clone(), Vec::new())).ok();
    store.publish_durable("session.created", json!({ "sessionID": id }), &id);
    Json(json!({ "data": info }))
}

/// The session's own metadata, read back from Ante. Returning a synthesized one
/// loses the real directory, and the client uses that to decide whether the
/// session belongs to the location it is showing.
fn ante_session_info(id: &str) -> Option<Value> {
    let home = std::env::var_os("ANTE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
            home.join(".ante")
        });
    let raw = std::fs::read_to_string(home.join("sessions").join(id).join("meta.json")).ok()?;
    let meta: Value = serde_json::from_str(&raw).ok()?;
    let created = meta
        .get("started_time")
        .and_then(|v| v.as_str())
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|when| when.timestamp_millis())
        .unwrap_or_else(now_ms);
    let directory = meta.get("dir").and_then(|v| v.as_str()).unwrap_or(DIRECTORY);
    let usage = meta.get("usage").cloned().unwrap_or_else(|| json!({}));
    let title = meta
        .get("first_user_message")
        .and_then(|v| v.as_str())
        .map(|text| short(&text.replace(['\n', '\r'], " "), 60))
        .unwrap_or_else(|| "Ante session".into());
    Some(json!({
        "id": id,
        "projectID": "prj_shim",
        "agent": reported_agent(),
        "model": {
            "id": meta.get("model").and_then(|v| v.as_str()).unwrap_or(MODEL),
            "providerID": meta.get("provider").and_then(|v| v.as_str()).unwrap_or(PROVIDER),
        },
        "cost": 0,
        "tokens": tokens_json(
            usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        ),
        "time": { "created": created, "updated": created },
        "title": title,
        "location": { "directory": directory },
    }))
}

async fn session_get(State(store): State<Store>, Path(id): Path<String>) -> Json<Value> {
    let info = store
        .session(&id)
        .or_else(|| ante_session_info(&id))
        .unwrap_or_else(|| session_info(&id, "Ante session"));
    // The route's schema is `{data: Session.Info}` with additionalProperties:false;
    // a bare object makes the client read `.id` off undefined.
    Json(json!({ "data": info }))
}

/// Ante persists every session event to `events.jsonl`; replaying it rebuilds
/// the transcript opencode asks for when a session is opened.
///
/// Each line is `{timestamp, id, event: {<variant>: <payload>}, parent}`, and the
/// `event` field is exactly Ante's `Evt` in serde form, so the fold is the same
/// shape as the live pump. v1 covers user turns and assistant text.
fn replay_session(id: &str) -> Vec<Value> {
    let home = std::env::var_os("ANTE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
            home.join(".ante")
        });
    let Ok(raw) = std::fs::read_to_string(home.join("sessions").join(id).join("events.jsonl")) else {
        return Vec::new();
    };

    let mut messages: Vec<Value> = Vec::new();
    let mut ordinal = 0u64;
    for line in raw.lines() {
        let Ok(wrapper) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(event) = wrapper.get("event") else {
            continue;
        };
        let Ok(event) = serde_json::from_value::<Evt>(event.clone()) else {
            continue;
        };
        let created = wrapper
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .map(|when| when.timestamp_millis())
            .unwrap_or_else(now_ms);
        ordinal += 1;
        let id = format!("msg_{:016x}{:04x}", created as u64, ordinal);
        match event {
            Evt::UserInput(text) => messages.push(json!({
                "id": id,
                "type": "user",
                "text": text,
                "files": [],
                "agents": [],
                "skills": [],
                "time": { "created": created },
            })),
            Evt::TurnStart { .. } => messages.push(json!({
                "id": id,
                "type": "assistant",
                "agent": reported_agent(),
                "model": { "id": model_for_session().1, "providerID": model_for_session().0 },
                "content": [],
                "time": { "created": created },
            })),
            Evt::AgentMessage(text) => {
                if let Some(last) = messages.last_mut() {
                    if last["type"] == "assistant" {
                        last["content"] = json!([{ "type": "text", "text": text }]);
                        last["time"]["completed"] = json!(created);
                    }
                }
            }
            _ => {}
        }
    }
    messages
}

async fn session_messages(
    State(store): State<Store>,
    Path(id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    // A session this process is driving comes from the live store; anything else
    // is read back from Ante's event log.
    let live = store
        .messages
        .lock()
        .ok()
        .and_then(|m| m.get(&id).cloned())
        .unwrap_or_default();
    let mut data = if live.is_empty() { replay_session(&id) } else { live };
    // The transcript is read newest-first with a limit; honour both.
    if params.get("order").map(|value| value == "desc").unwrap_or(false) {
        data.reverse();
    }
    if let Some(limit) = params.get("limit").and_then(|value| value.parse::<usize>().ok()) {
        data.truncate(limit);
    }
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
    // The client sends its own message id; reusing it keeps its optimistic copy
    // and the inbox item the same entry instead of rendering the message twice.
    let user_id = body
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| uid("msg"));
    let user = json!({
        "id": user_id, "type": "user", "text": text, "time": { "created": now_ms() },
    });
    store.messages.lock().ok().and_then(|mut m| m.get_mut(&id).map(|list| list.push(user.clone())));
    store.publish("message.updated", json!({ "sessionID": id, "info": user }));

    // Tell the client about the user's message: `inbox.enqueued` admits it into
    // the transcript, and `delivered` moves it into place. Without these the
    // client only has its own optimistic copy and the order comes out wrong.
    let item = json!({
        "id": user_id,
        "sessionID": id,
        "time": { "created": now_ms() },
        "type": "user",
        "payload": { "text": text, "files": [], "agents": [], "skills": [] },
        "delivery": {},
    });
    store.publish_durable(
        "session.inbox.enqueued",
        json!({ "inboxID": user_id, "sessionID": id, "item": item }),
        &id,
    );
    store.publish_durable(
        "session.inbox.delivered",
        json!({ "inboxID": user_id, "sessionID": id }),
        &id,
    );

    // Point the event pump at this session, then hand the text to Ante.
    if let Ok(mut active) = store.ante.active.lock() {
        *active = Some(id.clone());
    }
    // The client carries the chosen agent in the prompt body (it does not call
    // the switch route for a plain `shift+tab`), so the agent is applied here.
    // An explicit SHIM_PERMISSION_MODE wins when set.
    let agent = store
        .ante
        .agent
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or_else(|_| reported_agent().to_string());
    let mode = match std::env::var("SHIM_PERMISSION_MODE").as_deref() {
        Ok("strict") => ante_sdk::protocol::PermissionMode::Strict,
        Ok("yolo") => ante_sdk::protocol::PermissionMode::Yolo,
        Ok("auto") => ante_sdk::protocol::PermissionMode::Auto,
        _ => permission_mode_for(&agent),
    };

    let ops = store.ante.ops.lock().await.clone();
    match ops {
        Some(ops) => {
            if !store.ante.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let chosen = store.ante.model.lock().ok().and_then(|slot| slot.clone());
                let (provider, model) = chosen.unwrap_or_else(|| (PROVIDER.into(), MODEL.into()));
                let request = SessionRequest {
                    permission_mode: Some(mode),
                    provider: Some(provider),
                    model: Some(model),
                    ..Default::default()
                };
                let _ = ops.send(ante_sdk::protocol::op_msg(Op::StartSession(request))).await;
                // The real server announces the title once; Ante titles from the
                // first message too, so mirror it here.
                let title: String = text.chars().take(60).collect();
                let title = title.trim().to_string();
                if !title.is_empty() {
                    store.publish_durable(
                        "session.renamed",
                        json!({ "sessionID": id, "title": title }),
                        &id,
                    );
                }
            } else {
                // Later prompts may arrive with a different agent.
                let update = ante_sdk::protocol::SessionUpdate {
                    permission_mode: Some(mode),
                    ..Default::default()
                };
                let _ = ops.send(ante_sdk::protocol::op_msg(Op::UpdateSession(update))).await;
            }
            if let Ok(mut slot) = store.ante.last_user.lock() {
                *slot = text.clone();
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

const USAGE: &str = "\
antex — 用 opencode 的 TUI 驱动 Ante

用法:
  antex                  起服务并直接进 TUI（一条命令，退出时服务一起停）
  antex PORT             同上，指定端口（默认 41999）
  antex serve [PORT]     只起服务，留在前台；另开终端连它
  antex -h | --help      显示本帮助

只起服务时，客户端这样连：
  opencode2 --server http://127.0.0.1:41999

环境变量:
  ANTE_BIN       指定 ante 可执行文件（默认 $PATH，再退到 ~/.ante/bin/ante）
  ANTEX_CLIENT   指定 opencode 客户端（默认 $PATH 上的 opencode2，再退到 opencode）
";

struct Args {
    port: u16,
    /// `serve` 子命令：只起服务，不接管终端。
    serve_only: bool,
}

/// `Ok(Some(args))` 正常，`Ok(None)` 已打印帮助并应退出，`Err` 是用法错误。
fn parse_args(args: Vec<String>) -> Result<Option<Args>, String> {
    let mut parsed = Args { port: 41999, serve_only: false };
    let mut rest = args.into_iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "serve" => parsed.serve_only = true,
            "--port" => {
                let value = rest.next().ok_or("--port 后面要跟端口号")?;
                parsed.port = value.parse().map_err(|_| format!("端口不是数字: {value}"))?;
            }
            other => match other.parse::<u16>() {
                Ok(value) => parsed.port = value,
                Err(_) => return Err(format!("不认识的参数: {other}")),
            },
        }
    }
    Ok(Some(parsed))
}

#[tokio::main]
async fn main() {
    let args = match parse_args(std::env::args().skip(1).collect()) {
        Ok(Some(args)) => args,
        Ok(None) => return,
        Err(err) => {
            eprintln!("错误：{err}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let store = Store::new();
    // Ante is connected lazily at startup; its events feed every opencode
    // session this shim serves.
    tokio::spawn(spawn_ante(store.clone()));
    let app = Router::new()
        .route("/api/health", get(health))
        // `server.info` is what the client's version check calls.
        .route("/api/info", get(health))
        .route("/health", get(health))
        .route("/api/location", get(location_get))
        .route("/api/form", get(empty_reads))
        .route("/api/shell", get(empty_reads).post(empty_reads))
        .route("/api/reference", get(empty_reads))
        .route("/api/integration", get(empty_reads))
        .route("/api/project", get(bare_empty_array))
        .route("/api/mcp/resource", get(mcp_resources_empty))
        .route("/api/vcs/base", get(vcs_base_empty))
        .route("/api/vcs/diff", get(empty_reads))
        .route(
            "/api/experimental/session/{id}/terminal",
            get(session_terminals_empty).post(session_terminals_empty),
        )
        .route("/api/fs/list", get(fs_list))
        .route("/api/fs/find", get(fs_find))
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
        .route("/api/session/{id}/interrupt", post(session_interrupt))
        .route("/api/session/{id}/permission", get(session_permissions))
        .route("/api/session/{id}/permission/{request_id}/reply", post(permission_reply))
        .route("/api/permission/request", get(permission_request))
        .route("/api/session/{id}/inbox", get(bare_empty))
        .route("/api/session/{id}/form", get(bare_empty))
        .route("/api/session/{id}/model", post(session_model))
        .route("/api/session/{id}/compact", post(session_compact))
        .route("/api/session/{id}/agent", post(session_agent))
        .route("/api/session/{id}/view", post(no_content))
        .route("/api/event", get(events))
        .fallback(fallback)
        .layer(axum::middleware::from_fn(log_request))
        .with_state(store);

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", args.port)).await {
        Ok(listener) => listener,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!(
                "端口 {} 已被占用。换一个端口，或先停掉占用它的进程：\n  fuser -k {}/tcp",
                args.port, args.port
            );
            std::process::exit(1);
        }
        Err(err) => {
            eprintln!("监听 127.0.0.1:{} 失败：{err}", args.port);
            std::process::exit(1);
        }
    };

    let port = args.port;
    let _ = LOG_FILE.set(if args.serve_only {
        None
    } else {
        Some(std::env::temp_dir().join("antex.log"))
    });
    if args.serve_only {
        println!("antex 服务已起在 http://127.0.0.1:{port}");
        println!("客户端这样连：opencode2 --server http://127.0.0.1:{port}");
        axum::serve(listener, app).await.expect("serve");
        return;
    }

    // Default mode: serve in the background, then hand the terminal to the real
    // opencode TUI — one command instead of two.
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("antex: 服务结束：{err}");
        }
    });
    let directory =
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(DIRECTORY));
    let client = client_executable();
    match std::process::Command::new(&client)
        .arg("--server")
        .arg(format!("http://127.0.0.1:{port}"))
        .arg(&directory)
        .status()
    {
        // The TUI owns the terminal; when it exits, so do we.
        Ok(status) => std::process::exit(status.code().unwrap_or(0)),
        Err(err) => {
            eprintln!("起不了 opencode 客户端（{}）：{err}", client.display());
            eprintln!("用 ANTEX_CLIENT 指定它的路径；或只起服务：antex serve {port}");
            std::process::exit(1);
        }
    }
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
