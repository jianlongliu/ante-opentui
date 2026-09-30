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
    routing::{get, patch, post},
};
use futures::stream::{self, Stream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::broadcast;

/// Pasted images: opencode's inline `data:` URL → Ante's `@path` mention.
mod attachments;

/// Where the shim settles a request when the client does not say. The client
/// normally passes `location[directory]` on every call, so this is only a
/// fallback — and it is derived at runtime, not baked in.
fn default_directory() -> String {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_else(|| ".".to_string())
}
/// antex has no release cadence of its own — it follows whatever Ante and
/// opencode are on — so its version is the day the binary was built.
const ANTEX_VERSION: &str = env!("ANTEX_VERSION");
/// The Ante backend's version, read once by the startup self-check. `health`
/// reports *this* as `version`: the shim stands in for Ante, so the client's
/// own version number (what it used to answer) said nothing about either end.
static ANTE_VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
/// The URL this instance listens on, set once the port is known (`ServerInfo`
/// declares `urls`, and the client builds its display address from it).
static SERVER_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
/// The model Ante is configured with, so the composer shows something real.
/// This is the fallback for a client that never picked one — it has to be the
/// catalog's name (provider-scoped), or the first turn comes back as an HTTP
/// 400 from the provider.
const MODEL: &str = "example/example-model";
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
        "directory": default_directory(),
        "project": { "id": "prj_shim", "directory": default_directory(), "canonical": default_directory() },
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
    /// The session the live connection is driving, as the *client* names it.
    /// Ante drives exactly one session per connection, so a prompt for any
    /// other id has to switch it over first.
    live: Arc<Mutex<Option<String>>>,
    /// While a resume is in flight: the op id of the `UserInput` that will
    /// start the turn we are actually waiting for, plus when the guard was
    /// armed. Everything the pump sees until then is the replay. See
    /// [`Ante::dropping`].
    replay_turn: Arc<Mutex<Option<(String, std::time::Instant)>>>,
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
    /// A turn is running. Decides between handing text to Ante as a queued input
    /// and steering it into the turn already in flight.
    busy: Arc<std::sync::atomic::AtomicBool>,
    /// The archive name Ante gave each session, keyed by the id the *client*
    /// used. Ante mints its own id at `StartSession` and never reports it in
    /// response, so `Evt::SessionStart` is the only place the pair shows up —
    /// without it a delete aimed at a session the client just created names no
    /// directory at all.
    archives: Arc<Mutex<HashMap<String, String>>>,
}

/// An Ante turn paused for approval, held until the TUI answers.
struct PendingApproval {
    request_id: String,
    turn_id: Id,
    tools: Vec<ToolUse>,
}

/// How long the replay guard waits for its own turn to appear. Ante replays a
/// resumed conversation in one burst, so a longer wait means the switch never
/// took effect.
const REPLAY_GUARD: std::time::Duration = std::time::Duration::from_secs(20);

impl Ante {
    fn new() -> Self {
        Self {
            ops: Arc::new(tokio::sync::Mutex::new(None)),
            live: Arc::new(Mutex::new(None)),
            replay_turn: Arc::new(Mutex::new(None)),
            active: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(None)),
            agent: Arc::new(Mutex::new(reported_agent().to_string())),
            model: Arc::new(Mutex::new(None)),
            last_user: Arc::new(Mutex::new(String::new())),
            busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            archives: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Ante's archive name for a session the client knows under another id, or
    /// the id itself when they agree.
    fn archive_of(&self, id: &str) -> Option<String> {
        if stored_session(id) {
            return Some(id.to_string());
        }
        self.archives.lock().ok()?.get(id).cloned().filter(|name| stored_session(name))
    }

    fn remember_archive(&self, client_id: &str, archive: String) {
        if let Ok(mut archives) = self.archives.lock() {
            archives.insert(client_id.to_string(), archive);
        }
    }

    /// Whether a turn is in flight, so `steer` can mean what Ante means by it.
    fn busy(&self) -> bool {
        self.busy.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn set_busy(&self, value: bool) {
        self.busy.store(value, std::sync::atomic::Ordering::SeqCst);
    }

    /// The session the live connection drives, as the client names it.
    fn live_id(&self) -> Option<String> {
        self.live.lock().ok().and_then(|slot| slot.clone())
    }

    /// Arm the replay guard for a resume: the next turn this op id starts is
    /// ours, everything before it is history coming back.
    fn expect_turn(&self, turn_id: String) {
        if let Ok(mut slot) = self.replay_turn.lock() {
            *slot = Some((turn_id, std::time::Instant::now()));
        }
    }

    /// Whether this event is part of a resume's replay and must not reach the
    /// client — it already has that history from `/message`. The guard lifts on
    /// the turn it was armed for; a refusal or a timeout lifts it too, and
    /// drops the session binding so the next prompt resolves again rather than
    /// talking into the wrong session.
    fn dropping(&self, event: &Evt) -> bool {
        let Ok(mut slot) = self.replay_turn.lock() else {
            return false;
        };
        let Some((expected, armed)) = slot.as_ref() else {
            return false;
        };
        let ours = matches!(event, Evt::TurnStart { turn_id } if turn_id.to_string() == *expected);
        if ours {
            *slot = None;
            return false;
        }
        let refused = matches!(event, Evt::Error(_));
        if refused || armed.elapsed() > REPLAY_GUARD {
            *slot = None;
            if let Ok(mut live) = self.live.lock() {
                *live = None;
            }
            log_line(&format!(
                "ante: 恢复会话没生效（{}），这次重放已放行——下一条消息会重新定位会话",
                if refused { "Ante 拒绝了这个 id" } else { "等不到它自己的 turn" }
            ));
        }
        true
    }
}

#[derive(Clone)]
struct Store {
    ante: Ante,
    sessions: Arc<Mutex<Vec<Value>>>,
    /// What the shim handed Ante, as (staged text, the client's message id). The
    /// transcript is read back from Ante's log and opencode reconciles it by id,
    /// so a prompt submitted here has to come back under the id the client is
    /// already holding a row for.
    handed: Arc<Mutex<Vec<(String, String)>>>,
    /// Prompts the client queued instead of steering. opencode's inbox lives on
    /// the server, so ours does too: the shim holds them and hands each one over
    /// at the turn boundary (or right away when the user steers it). Ante cannot
    /// withdraw a queued input, so holding them here is also what makes the
    /// queue's delete/steer honest.
    pending: Arc<Mutex<HashMap<String, Vec<Value>>>>,
    /// Prompts Ante has taken but the model has not read yet, by session.
    /// `Evt::UserInput` only means Ante accepted the text — the fold happens at
    /// the *next* step — so `delivered` waits for that step. The client keeps
    /// drawing the marker until then, which is the whole point: "did the model
    /// see what I just said" is the question being answered.
    acked: Arc<Mutex<HashMap<String, Vec<String>>>>,
    /// The text actually handed to Ante, per held prompt. The client's copy is
    /// what the user typed; ours carries the `@` mentions staged for pasted
    /// images, and Ante echoes *that* back — plus the folder listing it loaded
    /// for the mention — so acknowledging a prompt has to compare against this.
    sent: Arc<Mutex<HashMap<String, String>>>,
    /// Frames fan out to every connected `/api/event` feed.
    events: broadcast::Sender<Value>,
}

impl Store {
    fn new() -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            sessions: Arc::new(Mutex::new(Vec::new())),
            handed: Arc::new(Mutex::new(Vec::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            acked: Arc::new(Mutex::new(HashMap::new())),
            sent: Arc::new(Mutex::new(HashMap::new())),
            events,
            ante: Ante::new(),
        }
    }

    /// The queued prompt with this inbox id, removed from the queue.
    fn take_queued(&self, session: &str, inbox_id: &str) -> Option<Value> {
        let mut queues = self.pending.lock().ok()?;
        let queue = queues.get_mut(session)?;
        let position = queue.iter().position(|item| item["id"] == inbox_id)?;
        let item = queue.remove(position);
        drop(queues);
        self.forget_sent(inbox_id);
        Some(item)
    }

    /// What Ante will be handed for this prompt: the text we staged, which is
    /// the user's own plus a mention per pasted image. `None` for a prompt we
    /// never staged, which the caller reads off the payload instead.
    fn sent_text(&self, inbox_id: &str) -> Option<String> {
        self.sent.lock().ok()?.get(inbox_id).cloned()
    }

    fn remember_sent(&self, inbox_id: &str, text: &str) {
        if let Ok(mut sent) = self.sent.lock() {
            sent.insert(inbox_id.to_string(), text.to_string());
        }
    }

    /// Record a prompt this process handed Ante, so the transcript read back from
    /// Ante's log can give it the id the client already has a row under. Unlike
    /// `sent`, this survives the hand-over: the row the client holds is read back
    /// long after the prompt stopped being queued.
    fn remember_handed(&self, inbox_id: &str, staged: &str) {
        if let Ok(mut handed) = self.handed.lock() {
            handed.push((staged.to_string(), inbox_id.to_string()));
        }
    }

    fn handed(&self) -> Vec<(String, String)> {
        self.handed.lock().map(|handed| handed.clone()).unwrap_or_default()
    }

    fn forget_sent(&self, inbox_id: &str) {
        if let Ok(mut sent) = self.sent.lock() {
            sent.remove(inbox_id);
        }
    }

    fn queued(&self, session: &str) -> Vec<Value> {
        self.pending
            .lock()
            .ok()
            .and_then(|queues| queues.get(session).cloned())
            .unwrap_or_default()
    }

    /// Re-label a held prompt. Steering one does not deliver it — it moves it from
    /// the queue to Ante's inbox, and the pump clears it when Ante says it took it.
    fn set_delivery(&self, session: &str, inbox_id: &str, delivery: &str) {
        if let Ok(mut queues) = self.pending.lock()
            && let Some(queue) = queues.get_mut(session)
            && let Some(item) = queue.iter_mut().find(|item| item["id"] == inbox_id)
        {
            item["delivery"] = json!(delivery);
        }
    }

    /// The oldest prompt the client *queued*, relabelled `steer`: it is being
    /// handed to Ante now, so it must stop looking queued (a later turn boundary
    /// must not offer the same text again) without being called delivered — the
    /// model only reads it at the next step. The text is the staged one, since
    /// that is what has to reach Ante; `None` when nothing is queued.
    fn steer_head(&self, session: &str) -> Option<(String, String)> {
        let mut queues = self.pending.lock().ok()?;
        let queue = queues.get_mut(session)?;
        let item = queue.iter_mut().find(|item| item["delivery"] == "queue")?;
        item["delivery"] = json!("steer");
        let id = item["id"].as_str()?.to_string();
        let display = item["payload"]["text"].as_str().unwrap_or("").to_string();
        Some((id.clone(), self.sent_text(&id).unwrap_or(display)))
    }

    /// Ante took this text. Remember which held prompt it was and leave it held:
    /// the model has not read it yet, and the next step boundary is when it will.
    fn ack_text(&self, session: &str, echoed: &str) -> Option<String> {
        // What Ante echoes is the text we sent, with whatever it loaded for a
        // mention appended — so a staged prompt comes back longer than it went.
        let stripped = attachments::strip_expansion(echoed);
        let id = {
            let queues = self.pending.lock().ok()?;
            queues
                .get(session)?
                .iter()
                .find(|item| {
                    let id = item["id"].as_str().unwrap_or("");
                    let display = item["payload"]["text"].as_str().unwrap_or("");
                    let staged = self.sent_text(id);
                    let sent = staged.as_deref().unwrap_or(display);
                    sent == echoed || sent == stripped || stripped.starts_with(sent)
                })?["id"]
                .as_str()?
                .to_string()
        };
        if let Ok(mut acked) = self.acked.lock() {
            acked.entry(session.to_string()).or_default().push(id.clone());
        }
        Some(id)
    }

    /// Prompts whose step boundary has arrived (or whose turn is over), cleared
    /// out so the caller can publish `delivered` for them.
    fn take_acked(&self, session: &str) -> Vec<String> {
        let Ok(mut acked) = self.acked.lock() else {
            return Vec::new();
        };
        let mut ids = acked.remove(session).unwrap_or_default();
        ids.sort();
        ids.dedup();
        for id in &ids {
            self.forget_sent(id);
        }
        ids
    }

    /// The steered prompts still held, taken off the queue; queued ones stay for
    /// `flush_queue`. Called at the turn boundary — a steer Ante never reported
    /// has no step left to fold into, and a marker that stays on for good would
    /// be worse than the rare prompt a turn genuinely dropped.
    fn take_steers(&self, session: &str) -> Vec<Value> {
        let Ok(mut queues) = self.pending.lock() else {
            return Vec::new();
        };
        let Some(queue) = queues.get_mut(session) else {
            return Vec::new();
        };
        let mut steers = Vec::new();
        queue.retain(|item| {
            if item["delivery"] == "steer" {
                steers.push(item.clone());
                return false;
            }
            true
        });
        drop(queues);
        for item in &steers {
            if let Some(id) = item["id"].as_str() {
                self.forget_sent(id);
            }
        }
        steers
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

    /// The pair the client last committed (session create or a model switch),
    /// falling back to the one Ante itself is configured on. Reporting the
    /// configured pair instead is what made a session list show a model the
    /// session had never run.
    fn current_model(&self) -> (String, String) {
        self.ante
            .model
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .unwrap_or_else(active_model)
    }
}

fn loc_plain() -> Value {
    json!({ "directory": default_directory() })
}

fn session_info(id: &str, title: &str, model: (String, String)) -> Value {
    json!({
        "id": id,
        "projectID": "prj_shim",
        "project": { "id": "prj_shim" },
        "cost": 0,
        "tokens": { "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
        "agent": reported_agent(),
        "model": { "id": model.1, "providerID": model.0, "variant": "default" },
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

/// Report something about the shim itself. One-command mode hands the terminal
/// to the TUI, so a bare `eprintln!` there is scribbled over (and `pkill`-style
/// post-mortems get nothing) — which is exactly how a dead Ante connection
/// becomes "the interface is up but nothing happens". Everything goes through
/// here instead: the file in one-command mode, stdout in serve mode.
fn log_line(line: &str) {
    use std::io::Write as _;
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
}

async fn log_request(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    log_line(&format!("{} {}", req.method(), req.uri()));
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

/// Startup-only reporting: the log line is the durable copy, plus stderr for
/// when the user is watching the terminal and the TUI has not taken it over
/// yet. In serve mode stdout already says it, so stderr would only double up.
fn report_startup(line: &str) {
    log_line(line);
    if LOG_FILE.get().and_then(|slot| slot.as_ref()).is_some() {
        eprintln!("{line}");
    }
}

/// Start-up self-check.
///
/// The shim is useless without an `ante` it can speak to, and the failure the
/// user actually sees is not an error but silence: the TUI opens fine and every
/// prompt goes nowhere. So this runs before the TUI does and writes down what
/// it found, including the one thing that silently breaks across Ante releases
/// — a protocol/`ante-sdk` version mismatch.
async fn ante_selfcheck() {
    let Some(bin) = ante_executable() else {
        let msg = format!(
            "自检：找不到 `ante` 可执行文件（找过 $ANTE_BIN、$PATH、~/.ante/bin/ante）。\n\
             没有它 TUI 照样能开，但每条消息都会石沉大海，所以这里直接退出。\n\
             装好 Ante，或用 ANTE_BIN 指到它的路径。"
        );
        report_startup(&msg);
        std::process::exit(1);
    };
    let built = env!("ANTE_SDK_VERSION");
    let found = ante_version(&bin).await;
    if let Some(found) = found.as_ref() {
        // Whatever `health` answers, it answers with this.
        let _ = ANTE_VERSION.set(found.clone());
    }
    match found {
        Some(found) if found != built => {
            let msg = format!(
                "自检：协议可能对不上——antex 是按 ante-sdk {built} 编的，本机 {} 是 ante {found}。\n\
                 若发消息没反应，就是这个：改 Cargo.toml 的 ante-sdk 版本后 `cargo build --release`。",
                bin.display()
            );
            report_startup(&msg);
        }
        Some(found) => log_line(&format!(
            "自检：{} → ante {found}；antex 编译于 ante-sdk {built}，版本一致",
            bin.display()
        )),
        None => log_line(&format!(
            "自检：{} --version 没给出可用版本号，无法核对协议版本（编译用的 ante-sdk {built}）",
            bin.display()
        )),
    }
}

/// `ante --version` → `ante 0.2.5` → `0.2.5`.
async fn ante_version(bin: &std::path::Path) -> Option<String> {
    let out = tokio::process::Command::new(bin).arg("--version").output().await.ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().last().map(str::to_string)
}

/// Which opencode client to hand the terminal to. `~/.local/bin/antex-tui` is
/// **our** build (vendored source + the Ante logo) and wins when present; the
/// stock binary is the fallback.
fn client_executable() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("ANTEX_CLIENT") {
        return std::path::PathBuf::from(path);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let patched = std::path::PathBuf::from(home).join(".local/bin/antex-tui");
        if patched.is_file() {
            return patched;
        }
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

/// Herdr integration: an agent reporting its own pane state, Herdr's official
/// route for agents whose vendor ships the integration —
/// <https://herdr.dev/docs/add-herdr-support>.
///
/// The shim is the right place for it: it already sees a turn start, end, or
/// stop for approval, so `working` / `idle` / `blocked` needs no guessing at
/// what the TUI happens to be drawing. Outside a Herdr pane (`HERDR_ENV`
/// unset) every call below does nothing. Herdr's built-in detection otherwise
/// sees the *client* — it reports the TUI as `opencode`.
mod herdr {
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// What the sidebar shows. The agent is Ante; the TUI is only its face.
    const AGENT: &str = "Ante";
    /// Identifies this integration. Stable, and never `herdr:`-prefixed —
    /// that prefix is Herdr's own integrations.
    const SOURCE: &str = "antex";

    struct Reporter {
        bin: PathBuf,
        pane: String,
        /// Herdr drops a report whose number is not above the last it accepted,
        /// so this only ever moves forward.
        seq: AtomicU64,
    }

    /// `Some` only inside a Herdr pane; set once by [`init`].
    static REPORTER: OnceLock<Option<Reporter>> = OnceLock::new();

    /// `--seq` only has to grow; a wall-clock stamp keeps it ahead of anything
    /// Herdr accepted from an earlier run of this agent in the same pane.
    fn stamp() -> u64 {
        super::now_ms().max(0) as u64
    }

    pub fn init() {
        let _ = REPORTER.set(Reporter::from_env());
    }

    impl Reporter {
        fn from_env() -> Option<Self> {
            // The pane's process *is* this one (`antex` runs the TUI as its
            // child), so Herdr's variables arrive here intact.
            if std::env::var("HERDR_ENV").ok()? != "1" {
                return None;
            }
            Some(Self {
                bin: PathBuf::from(std::env::var_os("HERDR_BIN_PATH")?),
                pane: std::env::var("HERDR_PANE_ID").ok()?,
                seq: AtomicU64::new(stamp()),
            })
        }

        fn next_seq(&self) -> u64 {
            let mut last = self.seq.load(Ordering::SeqCst);
            loop {
                let next = last.max(stamp()) + 1;
                match self.seq.compare_exchange(last, next, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => return next,
                    Err(observed) => last = observed,
                }
            }
        }

        fn command(&self, subcommand: &str) -> tokio::process::Command {
            let mut cmd = tokio::process::Command::new(&self.bin);
            cmd.arg("pane")
                .arg(subcommand)
                .arg(&self.pane)
                .arg("--source")
                .arg(SOURCE)
                .arg("--agent")
                .arg(AGENT)
                .arg("--seq")
                .arg(self.next_seq().to_string());
            // The TUI owns the terminal: a child writing to it would corrupt
            // the screen. Nothing here is worth showing anyway.
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            cmd
        }
    }

    /// Fire-and-forget, on purpose: reporting must never slow a turn down, and
    /// a failure is not worth surfacing — outside Herdr nobody is listening,
    /// and inside it Herdr clears the pane on its own once the shell is back.
    pub fn report(state: &str, message: Option<&str>, session: Option<&str>) {
        let Some(reporter) = REPORTER.get().and_then(Option::as_ref) else {
            return;
        };
        let mut cmd = reporter.command("report-agent");
        cmd.arg("--state").arg(state);
        if let Some(message) = message {
            cmd.arg("--message").arg(message);
        }
        if let Some(session) = session {
            cmd.arg("--agent-session-id").arg(session);
        }
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(3), cmd.status()).await;
        });
    }

    /// Hand the pane back on the way out; the timeout keeps a wedged Herdr from
    /// holding up our exit.
    pub async fn release() {
        let Some(reporter) = REPORTER.get().and_then(Option::as_ref) else {
            return;
        };
        let mut cmd = reporter.command("release-agent");
        let _ = tokio::time::timeout(Duration::from_secs(2), cmd.status()).await;
    }
}

async fn spawn_ante(store: Store) {
    let endpoint: ante_sdk::Endpoint = match "stdio".parse() {
        Ok(endpoint) => endpoint,
        Err(err) => {
            log_line(&format!("ante: 端点解析失败：{err}"));
            return;
        }
    };
    let mut options = ConnectOptions::default();
    let bin = ante_executable();
    options.executable = bin.clone();
    let client = match connect(endpoint, options).await {
        Ok(client) => client,
        Err(err) => {
            // The TUI is already on the user's screen by now, so this must land
            // in the log — stderr alone is how "nothing happens" starts.
            log_line(&format!(
                "ante: 连接失败：{err}\n  ante = {}；antex 编译于 ante-sdk {}",
                bin.map(|p| p.display().to_string()).unwrap_or_else(|| "（没找到）".into()),
                env!("ANTE_SDK_VERSION"),
            ));
            return;
        }
    };
    let (ops, mut rx) = client.into_parts();
    *store.ante.ops.lock().await = Some(ops);
    // Ante is up and waiting for input; that is what `idle` means here. The
    // session id lands with the first state change inside a session.
    herdr::report("idle", None, None);

    // A turn is made of steps (one model call each): Ante starts a step, may
    // call tools, then starts another. opencode models each step as its own
    // assistant message, so the step is opened lazily on its first content
    // event and closed once a tool ends. The message id is derived from the turn
    // and the step, not minted: a transcript read back from Ante's log names the
    // same steps the same way, and opencode reconciles a fetched transcript by id.
    let mut turn = String::from("turn");
    let mut steps = 0u32;
    let mut message_id = String::new();
    let mut step_open = false;
    let mut text_started = false;
    let mut streamed = false;
    let mut reasoning_open = false;
    // The thinking seen so far, so the block can be closed the moment the
    // answer starts. Ante's own `Thinking` aggregate only lands at the end of
    // the step, which left the row spinning through the whole reply.
    let mut reasoning_text = String::new();
    // Part identity in the client's row layer: `text:{ordinal}` addresses the
    // Nth text part of the message (`rows.ts`'s `resolvePart`), so an ordinal
    // belongs to a *part*, never to a delta — one per delta had the client
    // mint a row per token and then fail to resolve it.
    let mut text_part = 0u32;
    let mut text_ordinal = 0u32;
    let mut reasoning_part = 0u32;
    let mut reasoning_ordinal = 0u32;
    let mut tokens_in = 0u32;
    let mut tokens_out = 0u32;
    let mut compact_block: Option<String> = None;
    let mut compact_text = String::new();
    // The session Herdr was last told about, so a switch is reported once.
    let mut herdr_session: Option<String> = None;

    while let Some(msg) = rx.recv().await {
        trace_ante(&msg.event);
        // A resume hands the whole persisted conversation back; the client
        // already fetched that history, so rendering it again would double it.
        if store.ante.dropping(&msg.event) {
            continue;
        }
        let Some(session) = store.ante.active.lock().ok().and_then(|s| s.clone()) else {
            continue;
        };
        // A switch to another session is worth telling Herdr about on its own:
        // otherwise it only learns the id when a turn happens to run. `idle`
        // is right, because the point of a switch is to sit ready for input.
        if herdr_session.as_deref() != Some(session.as_str()) {
            herdr_session = Some(session.clone());
            herdr::report("idle", None, Some(&session));
        }
        // Open a step (a new assistant message) on demand.
        macro_rules! ensure_step {
            () => {
                if !step_open {
                    step_open = true;
                    text_started = false;
                    streamed = false;
                    reasoning_open = false;
                    reasoning_text.clear();
                    text_part = 0;
                    reasoning_part = 0;
                    steps += 1;
                    message_id = step_message_id(&turn, steps);
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
                    // A step is the boundary Ante folds accepted prompts into, so
                    // anything it took before this one is being read now.
                    for inbox_id in store.take_acked(&session) {
                        log_line(&format!("ante: 消息 {inbox_id} 送达（模型开始读）"));
                        store.publish_durable(
                            "session.inbox.delivered",
                            json!({ "inboxID": inbox_id, "sessionID": session }),
                            &session,
                        );
                    }
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
        // The real server sends the complete thinking block once (`reasoning.ended`
        // carries the text); Ante streams deltas and only repeats the whole block
        // when the step is over. Closing the block ourselves at the boundary keeps
        // the canonical order `reasoning.ended` → `text.started`: the row settles
        // into `Thought · Ns` as the answer starts, not seconds later.
        macro_rules! open_reasoning {
            () => {
                if !reasoning_open {
                    reasoning_open = true;
                    reasoning_ordinal = reasoning_part;
                    reasoning_part += 1;
                    store.publish_durable(
                        "session.reasoning.started",
                        json!({
                            "sessionID": session,
                            "assistantMessageID": message_id,
                            "ordinal": reasoning_ordinal,
                        }),
                        &session,
                    );
                }
            };
        }
        macro_rules! close_reasoning {
            () => {
                if reasoning_open {
                    reasoning_open = false;
                    store.publish_durable(
                        "session.reasoning.ended",
                        json!({
                            "sessionID": session,
                            "assistantMessageID": message_id,
                            "ordinal": reasoning_ordinal,
                            "text": std::mem::take(&mut reasoning_text),
                        }),
                        &session,
                    );
                }
            };
        }
        match msg.event {
            // The client named its session; Ante named the archive it writes to.
            // This is the one event that carries Ante's id, so remember the pair
            // while the client's id is the active one.
            Evt::SessionStart(info) => {
                if let Some(client) = store.ante.active.lock().ok().and_then(|slot| slot.clone()) {
                    let archive = info.session_id.to_string();
                    if archive != client {
                        log_line(&format!("ante: 会话 {client} 的存档是 {archive}"));
                        store.ante.remember_archive(&client, archive);
                    }
                }
            }
            // Ante took an input. It is *not* delivered yet: `Op::UserInput` and
            // `Op::Steer` only offer the text, and the model reads it at the step
            // that starts next — which is when the pump publishes `delivered`.
            // Until then the client keeps the prompt marked as not delivered,
            // which is exactly the question a steered message raises.
            Evt::UserInput(text) => {
                if let Some(inbox_id) = store.ack_text(&session, &text) {
                    log_line(&format!("ante: 消息 {inbox_id} 已被 Ante 收下，等下一步"));
                }
            }
            Evt::TurnStart { turn_id } => {
                turn = turn_id.to_string();
                steps = 0;
                step_open = false;
                store.ante.set_busy(true);
                herdr::report("working", None, Some(&session));
                store.publish_durable(
                    "session.execution.started",
                    json!({ "sessionID": session }),
                    &session,
                );
            }
            Evt::ThinkingDelta(delta) => {
                ensure_streamed!();
                ensure_step!();
                open_reasoning!();
                reasoning_text.push_str(&delta);
                store.publish(
                    "session.reasoning.delta",
                    json!({
                        "sessionID": session,
                        "assistantMessageID": message_id,
                        "ordinal": reasoning_ordinal,
                        "delta": delta,
                    }),
                );
            }
            Evt::Thinking(text) => {
                // A non-streaming block still has to reach the client (that is the
                // canonical shape: one `reasoning.ended` carrying the text). A
                // streamed one was already closed above, in which case this late
                // copy is only a duplicate — and opening a second reasoning part
                // for it would show the same thinking twice.
                if !text.trim().is_empty() && reasoning_part == 0 {
                    ensure_step!();
                    open_reasoning!();
                    reasoning_text = text;
                }
                close_reasoning!();
            }
            Evt::MessageDelta(delta) => {
                ensure_streamed!();
                ensure_step!();
                // Thinking is over the moment the answer starts.
                close_reasoning!();
                if !text_started {
                    text_started = true;
                    text_ordinal = text_part;
                    text_part += 1;
                    store.publish_durable(
                        "session.text.started",
                        json!({
                            "sessionID": session,
                            "assistantMessageID": message_id,
                            "ordinal": text_ordinal,
                        }),
                        &session,
                    );
                }
                store.publish(
                    "session.text.delta",
                    json!({
                        "sessionID": session,
                        "assistantMessageID": message_id,
                        "ordinal": text_ordinal,
                        "delta": delta,
                    }),
                );
            }
            Evt::AgentMessage(text) => {
                ensure_step!();
                close_reasoning!();
                store.publish_durable(
                    "session.text.ended",
                    json!({
                        "sessionID": session,
                        "assistantMessageID": message_id,
                        "ordinal": text_ordinal,
                        "text": text,
                    }),
                    &session,
                );
            }
            Evt::ToolStart(tool) => {
                ensure_step!();
                close_reasoning!();
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
                // The turn is over either way — the TUI is taking input again.
                herdr::report("idle", None, Some(&session));
                // A turn can end on thinking alone (interrupted, or a step that
                // never answered); the block must not be left open.
                close_reasoning!();
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
                // The turn is over: a prompt the client queued belongs now, and a
                // steer Ante never reported is past the boundary it was waiting
                // for, so it stops being "pending" either way.
                store.ante.set_busy(false);
                let mut settled = store.take_acked(&session);
                settled.extend(
                    store
                        .take_steers(&session)
                        .iter()
                        .filter_map(|item| item["id"].as_str().map(str::to_string)),
                );
                settled.sort();
                settled.dedup();
                for inbox_id in settled {
                    log_line(&format!("ante: 回合结束时 {inbox_id} 仍未送达，按已送达处理"));
                    store.publish_durable(
                        "session.inbox.delivered",
                        json!({ "inboxID": inbox_id, "sessionID": session }),
                        &session,
                    );
                }
                flush_queue(&store, &session).await;
            }
            // Ante pauses the turn for a decision; the TUI shows this as a
            // permission prompt and answers on the reply route.
            Evt::TurnPause {
                turn_id,
                reason: TurnPauseReason::Approval { tools, .. },
            } => {
                let request_id = uid("per");
                let first = tools.first();
                // `blocked`: the turn is waiting on the user, which is exactly
                // what Herdr wants to notify about and can wait on.
                let waiting = format!(
                    "等待批准：{}",
                    tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>().join("、")
                );
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
                herdr::report("blocked", Some(&waiting), Some(&session));
            }
            _ => {}
        }
    }
    // Ante's event stream ended: it exited, or it dropped us (a version
    // mismatch can do that mid-handshake). Later prompts will not be answered,
    // so say so rather than let the UI go quiet.
    log_line("ante: 事件流已结束——Ante 进程退出，或它在握手后断开了连接；后续消息不会有回复");
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
    Path(session_id): Path<String>,
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
    // The session list and `GET /session/{id}` answer from the stored copy, so
    // the switch has to land there too — otherwise they keep reporting the pair
    // the session was created with.
    if let Ok(mut sessions) = store.sessions.lock()
        && let Some(info) = sessions.iter_mut().find(|s| s["id"] == session_id.as_str())
    {
        info["model"] = json!({ "id": id.clone(), "providerID": provider.clone(), "variant": "default" });
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
    if store.ante.live_id().is_some()
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
    if store.ante.live_id().is_some()
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
    Json(json!({
        "healthy": true,
        // `version` is the Ante backend's — the shim stands in for Ante, so the
        // client's own version here told the user nothing about either end.
        "version": ANTE_VERSION.get().map(String::as_str).unwrap_or("unknown"),
        "antex": ANTEX_VERSION,
        "pid": std::process::id(),
        "urls": [SERVER_URL.get().map(String::as_str).unwrap_or("")],
        "paths": { "tmp": std::env::temp_dir().to_string_lossy() },
    }))
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
        .unwrap_or_else(|| default_directory().to_string());
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
        .unwrap_or_else(|| default_directory().to_string());
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

/// When a session started and what was first asked, read off its event log.
///
/// `meta.json` is Ante's own summary and Ante writes it when a turn *ends* — so a
/// session that never finished one (paused on a question, then the process was
/// killed) has `events.jsonl` and nothing else. Skipping those directories is why
/// such a session reads as deleted when nothing touched it; the log carries the
/// same two facts.
fn events_head(id: &str) -> Option<(i64, String)> {
    let raw = std::fs::read_to_string(ante_home().join("sessions").join(id).join("events.jsonl")).ok()?;
    let mut created = None;
    let mut title = None;
    for line in raw.lines() {
        let Ok(wrapper) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if created.is_none() {
            created = wrapper
                .get("timestamp")
                .and_then(|v| v.as_str())
                .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                .map(|when| when.timestamp_millis());
        }
        if title.is_none() {
            title = wrapper
                .get("event")
                .and_then(|event| event.get("UserInput"))
                .and_then(|v| v.as_str())
                .map(|text| {
                    // The log keeps the staged mentions and Ante's expansion of
                    // them; a title is the message, not the paths.
                    let (shown, _) = attachments::from_log(text);
                    let shown = if shown.trim().is_empty() { text.to_string() } else { shown };
                    short(&shown.replace(['\n', '\r'], " "), 60)
                });
        }
        if created.is_some() && title.is_some() {
            break;
        }
    }
    Some((created.unwrap_or_else(now_ms), title.unwrap_or_else(|| "untitled".into())))
}

/// Whether Ante has this session on disk. `meta.json` is the stricter test and
/// the wrong one: it only appears at `TurnEnd`, so a session killed mid-turn has
/// its log and no summary — and treating that as "not a real session" both hides
/// it from the picker and makes the next prompt start a *fresh* Ante session
/// instead of continuing it.
fn stored_session(id: &str) -> bool {
    safe_id(id) && ante_home().join("sessions").join(id).is_dir()
}

/// Session ids are a single path component. Anything else (a slash, `..`) would
/// let a request name a directory outside `~/.ante/sessions`.
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Ante keeps its sessions on disk; opencode wants a session list, so this reads
/// that directory and maps it into `Session.Info`.
fn ante_sessions() -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(ante_home().join("sessions")) else {
        return Vec::new();
    };

    let mut sessions: Vec<(i64, Value)> = entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.file_name().to_string_lossy().into_owned();
            let meta = session_meta(&dir);
            let (created, title, tokens, model, provider) = match meta.as_ref() {
                Some(meta) => {
                    let created = meta
                        .get("started_time")
                        .and_then(|v| v.as_str())
                        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                        .map(|when| when.timestamp_millis())
                        .unwrap_or_else(now_ms);
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
                    let model = meta.get("model").and_then(|v| v.as_str()).unwrap_or(MODEL).to_string();
                    let provider = meta.get("provider").and_then(|v| v.as_str()).unwrap_or(PROVIDER).to_string();
                    (created, title, tokens, model, provider)
                }
                // No summary yet: the session is still real. Its own turn never
                // ended, so the model has to come from what Ante is configured
                // on now — the log does not record it.
                None => {
                    let (created, title) = events_head(&dir)?;
                    let (provider, model) = active_model();
                    (created, title, tokens_json(0, 0), model, provider)
                }
            };
            let id = meta
                .as_ref()
                .and_then(|meta| meta.get("id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| dir.clone());
            let info = json!({
                "id": id,
                "projectID": "prj_shim",
                "agent": reported_agent(),
                "model": { "id": model, "providerID": provider },
                "cost": 0,
                "tokens": tokens,
                "time": { "created": created, "updated": created },
                "title": title,
                // Same rule as `ante_session_info`: only the shim's own location,
                // or the client ends up asking about a directory `/api/location`
                // will never confirm (that is what broke the session picker).
                "location": loc_plain(),
            });
            Some((created, info))
        })
        .collect();

    // Newest first, which is the order the picker shows.
    sessions.sort_by(|a, b| b.0.cmp(&a.0));
    sessions.into_iter().map(|(_, info)| info).collect()
}

/// `DELETE /api/session/{id}` — what `ctrl+d` (pressed twice) in the sessions
/// picker calls. The archive is Ante's, so removing the directory is what makes
/// the delete real for every front end rather than only hiding the row here.
async fn session_delete(State(store): State<Store>, Path(id): Path<String>) -> Response {
    if !safe_id(&id) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "message": format!("不认识的会话 id：{id}") })),
        )
            .into_response();
    }
    // The client's own new sessions carry the id *it* invented, while the
    // archive on disk is named after Ante's id; `SessionStart` gave us the pair.
    let Some(archive) = store.ante.archive_of(&id) else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({ "message": format!("没有 {id} 的会话存档") })),
        )
            .into_response();
    };
    let dir = ante_home().join("sessions").join(&archive);
    // Ante appends to the log while a turn runs; pulling the directory out from
    // under the live turn would leave it writing into a path that is gone.
    if store.ante.busy() && store.ante.live_id().as_deref() == Some(id.as_str()) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "message": "这个会话正在跑，先中断（Esc）再删" })),
        )
            .into_response();
    }
    if let Err(err) = std::fs::remove_dir_all(&dir) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "message": format!("删不掉 {id}：{err}") })),
        )
            .into_response();
    }
    if let Ok(mut sessions) = store.sessions.lock() {
        sessions.retain(|session| session["id"] != id.as_str() && session["id"] != archive.as_str());
    }
    let _ = store.pending.lock().map(|mut queues| queues.remove(&id));
    if let Ok(mut archives) = store.ante.archives.lock() {
        archives.retain(|client, name| client != &id && name != &archive);
    }
    // The next prompt for this id has to resolve from scratch: the archive it
    // would have resumed is the one just removed.
    if store.ante.live_id().as_deref() == Some(id.as_str())
        && let Ok(mut live) = store.ante.live.lock()
    {
        *live = None;
    }
    log_line(&format!("session: 删除会话 {id}（存档 {archive}，即 ~/.ante/sessions/{archive}/）"));
    store.publish_durable("session.deleted", json!({ "sessionID": id }), &id);
    axum::http::StatusCode::NO_CONTENT.into_response()
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
    let info = session_info(&id, "Ante session", store.current_model());
    if let Ok(mut sessions) = store.sessions.lock() {
        sessions.push(info.clone());
    }
    store.publish_durable("session.created", json!({ "sessionID": id }), &id);
    Json(json!({ "data": info }))
}

fn session_meta(id: &str) -> Option<Value> {
    let raw = std::fs::read_to_string(ante_home().join("sessions").join(id).join("meta.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

/// The pair a session actually ran, as Ante recorded it — not the one Ante is
/// configured on now, which says nothing about an old session.
fn meta_model(meta: &Value) -> (String, String) {
    match (
        meta.get("provider").and_then(|v| v.as_str()),
        meta.get("model").and_then(|v| v.as_str()),
    ) {
        (Some(provider), Some(model)) => (provider.to_string(), model.to_string()),
        _ => active_model(),
    }
}

/// The session's own metadata, read back from Ante. Returning a synthesized one
/// loses the real directory, and the client uses that to decide whether the
/// session belongs to the location it is showing.
fn ante_session_info(id: &str) -> Option<Value> {
    let meta = session_meta(id)?;
    let (provider, model) = meta_model(&meta);
    let created = meta
        .get("started_time")
        .and_then(|v| v.as_str())
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|when| when.timestamp_millis())
        .unwrap_or_else(now_ms);
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
        "model": { "id": model, "providerID": provider },
        "cost": 0,
        "tokens": tokens_json(
            usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        ),
        "time": { "created": created, "updated": created },
        "title": title,
        // Ante's own `dir` for the session is NOT what the shim can confirm: this
        // shim has a single location (`prj_shim` at `default_directory()`, what
        // `/api/location` answers for every directory). Reporting the real one
        // made the client ask about a directory it can never resolve — the
        // session picker then failed outright with "Could not load sessions".
        "location": loc_plain(),
    }))
}

async fn session_get(State(store): State<Store>, Path(id): Path<String>) -> Json<Value> {
    let info = store
        .session(&id)
        .or_else(|| ante_session_info(&id))
        .unwrap_or_else(|| session_info(&id, "Ante session", store.current_model()));
    // The route's schema is `{data: Session.Info}` with additionalProperties:false;
    // a bare object makes the client read `.id` off undefined.
    Json(json!({ "data": info }))
}

/// The assistant message a step writes into. Both halves of the shim need the
/// same id: the pump streams a live step under it, and a transcript read back from
/// Ante's log rebuilds that step under it too. opencode reconciles a fetched
/// transcript by id, so without this a re-read of a session this process is
/// driving doubles every step it already has on screen.
fn step_message_id(turn: &str, step: u32) -> String {
    format!("msg_{turn}_{step}")
}

/// Ante persists every session event to `events.jsonl`; replaying it rebuilds the
/// transcript opencode asks for when a session is opened.
///
/// Each line is `{timestamp, id, event: {<variant>: <payload>}, parent}`, and the
/// `event` field is exactly Ante's `Evt` in serde form, so this fold is the shape
/// the live pump publishes: one assistant message per *step* (one model call),
/// carrying that step's thinking, its answer and its tool calls as parts. Folding
/// a whole turn into a single text message is what lost the thinking blocks — and,
/// once a steered prompt had landed mid-turn, every answer after it.
///
/// `handed` is what this process gave Ante, as (staged text, the client's message
/// id), so a prompt submitted here keeps the row the client already has.
fn replay_session(id: &str, handed: &[(String, String)]) -> Vec<Value> {
    let Ok(raw) = std::fs::read_to_string(ante_home().join("sessions").join(id).join("events.jsonl")) else {
        return Vec::new();
    };
    let (provider, model) = session_meta(id).map(|meta| meta_model(&meta)).unwrap_or_else(active_model);

    /// The step being written: the assistant message it belongs to, and the parts
    /// already open in it.
    struct Step {
        message: usize,
        /// The open reasoning part, and how many this step has had: Ante repeats
        /// the whole block at the end of a streamed one, and that copy must not
        /// open a second part.
        reasoning: Option<usize>,
        blocks: u32,
        /// The step's answer is one text part; a later `AgentMessage` fills it.
        text: Option<usize>,
    }

    let mut messages: Vec<Value> = Vec::new();
    let mut ordinal = 0u64;
    let mut turn = String::from("turn");
    let mut steps = 0u32;
    let mut step: Option<Step> = None;
    // Tool parts by call id, so `ToolEnd` can close the part `ToolStart` opened.
    let mut calls: std::collections::HashMap<String, (usize, usize)> = std::collections::HashMap::new();
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();

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
        // Only rows with no better identity of their own are keyed by position.
        let log_id = format!("msg_{:016x}{:04x}", created as u64, ordinal);

        // A step is one model call: these open it on the first content event and
        // close it when a tool ends, exactly as the live pump does.
        macro_rules! ensure_step {
            () => {
                if step.is_none() {
                    // Opening the next step is what closes the one before it — the
                    // same way the client's own fold closes the assistant message
                    // it was streaming into. A prompt steered in mid-turn sits in
                    // between, so this looks back for it rather than at the tail.
                    if let Some(last) = messages.iter_mut().rev().find(|message| {
                        message["type"] == "assistant" && message["time"].get("completed").is_none()
                    }) {
                        last["time"]["completed"] = json!(created);
                    }
                    steps += 1;
                    messages.push(json!({
                        "id": step_message_id(&turn, steps),
                        "type": "assistant",
                        "agent": reported_agent(),
                        "model": { "id": model, "providerID": provider },
                        "content": [],
                        "cost": 0,
                        "time": { "created": created },
                    }));
                    step = Some(Step { message: messages.len() - 1, reasoning: None, blocks: 0, text: None });
                }
            };
        }
        macro_rules! close_reasoning {
            () => {
                if let Some(fold) = step.as_mut()
                    && let Some(index) = fold.reasoning.take()
                    && let Some(part) = messages.get_mut(fold.message).and_then(|m| m["content"].get_mut(index))
                {
                    part["time"]["completed"] = json!(created);
                }
            };
        }
        macro_rules! open_reasoning {
            () => {
                ensure_step!();
                if let Some(fold) = step.as_mut()
                    && fold.reasoning.is_none()
                {
                    let content = messages[fold.message]["content"].as_array_mut().expect("content");
                    content.push(json!({ "type": "reasoning", "text": "", "time": { "created": created } }));
                    fold.reasoning = Some(content.len() - 1);
                    fold.blocks += 1;
                }
            };
        }
        macro_rules! ensure_text {
            () => {
                ensure_step!();
                let fold = step.as_mut().expect("step");
                if fold.text.is_none() {
                    let content = messages[fold.message]["content"].as_array_mut().expect("content");
                    content.push(json!({ "type": "text", "text": "" }));
                    fold.text = Some(content.len() - 1);
                }
            };
        }

        match event {
            Evt::UserInput(recorded) => {
                // A recorded input is what we staged — the user's own text plus a
                // mention per pasted image — with whatever Ante loaded for that
                // mention appended, hence the same comparison the ack uses.
                let expanded = attachments::strip_expansion(&recorded);
                let id = handed
                    .iter()
                    .find(|(staged, id)| {
                        !claimed.contains(id)
                            && (staged == &recorded || staged == &expanded || expanded.starts_with(staged.as_str()))
                    })
                    .map(|(_, id)| {
                        claimed.insert(id.clone());
                        id.clone()
                    })
                    .unwrap_or_else(|| log_id.clone());
                // The transcript wants the message back, with the images.
                let (text, files) = attachments::from_log(&recorded);
                messages.push(json!({
                    "id": id,
                    "type": "user",
                    "text": text,
                    "files": files,
                    "agents": [],
                    "skills": [],
                    "time": { "created": created },
                }));
            }
            Evt::TurnStart { turn_id } => {
                turn = turn_id.to_string();
                steps = 0;
                step = None;
            }
            Evt::ThinkingDelta(delta) => {
                open_reasoning!();
                if let Some(fold) = step.as_ref()
                    && let Some(index) = fold.reasoning
                    && let Some(part) = messages.get_mut(fold.message).and_then(|m| m["content"].get_mut(index))
                {
                    let text = part["text"].as_str().unwrap_or("").to_string();
                    part["text"] = json!(format!("{text}{delta}"));
                }
            }
            Evt::Thinking(text) => {
                // The aggregate only opens a part when this step never streamed
                // one; otherwise it is the same block, already closed.
                if !text.trim().is_empty() && step.as_ref().is_none_or(|fold| fold.blocks == 0) {
                    open_reasoning!();
                    if let Some(fold) = step.as_mut()
                        && let Some(index) = fold.reasoning
                    {
                        messages[fold.message]["content"][index]["text"] = json!(text);
                    }
                }
                close_reasoning!();
            }
            Evt::MessageDelta(delta) => {
                ensure_text!();
                close_reasoning!();
                if let Some(fold) = step.as_ref()
                    && let Some(index) = fold.text
                    && let Some(part) = messages.get_mut(fold.message).and_then(|m| m["content"].get_mut(index))
                {
                    let text = part["text"].as_str().unwrap_or("").to_string();
                    part["text"] = json!(format!("{text}{delta}"));
                }
            }
            Evt::AgentMessage(text) => {
                ensure_text!();
                close_reasoning!();
                if let Some(fold) = step.as_ref()
                    && let Some(index) = fold.text
                {
                    messages[fold.message]["content"][index]["text"] = json!(text);
                }
            }
            Evt::ToolStart(tool) => {
                ensure_step!();
                close_reasoning!();
                if let Some(fold) = step.as_mut() {
                    let content = messages[fold.message]["content"].as_array_mut().expect("content");
                    content.push(json!({
                        "type": "tool",
                        "id": tool.id,
                        "name": tool.name,
                        "executed": false,
                        "state": { "status": "running", "input": tool.args, "metadata": {} },
                        "time": { "created": created, "ran": created },
                    }));
                    calls.insert(tool.id.clone(), (fold.message, content.len() - 1));
                }
            }
            Evt::ToolEnd(end) => {
                let failed = !matches!(end.status, ante_sdk::protocol::ToolEndStatus::Completed);
                let text = result_text(&end.result_json);
                if let Some((message, index)) = calls.remove(&end.tool_use_id)
                    && let Some(part) = messages.get_mut(message).and_then(|m| m["content"].get_mut(index))
                {
                    let input = part["state"]["input"].clone();
                    part["state"] = if failed {
                        json!({
                            "status": "error",
                            "input": input,
                            "error": { "type": "tool_error", "message": text },
                            "metadata": {},
                            "content": [],
                        })
                    } else {
                        json!({
                            "status": "completed",
                            "input": input,
                            "metadata": {},
                            "content": [{ "type": "text", "text": text }],
                        })
                    };
                    part["executed"] = json!(true);
                    part["time"]["completed"] = json!(created);
                }
                // The next content begins a fresh step (and message).
                step = None;
            }
            Evt::UsageUpdate { usage, .. } => {
                if let Some(fold) = step.as_ref() {
                    messages[fold.message]["tokens"] = json!({
                        "input": usage.input_tokens,
                        "output": usage.output_tokens,
                        "reasoning": 0,
                        "cache": { "read": usage.cache_read_tokens, "write": usage.cache_creation_tokens },
                    });
                }
            }
            Evt::TurnEnd { status, .. } => {
                close_reasoning!();
                let failed = matches!(&status, ante_sdk::protocol::TurnEndStatus::Error { .. });
                if let Some(fold) = step.take() {
                    messages[fold.message]["finish"] = json!(if failed { "error" } else { "stop" });
                    messages[fold.message]["time"]["completed"] = json!(created);
                }
                // A failed turn ends with no answer at all; the log is the only
                // place the reason survives, so it lands as its own message.
                if let ante_sdk::protocol::TurnEndStatus::Error { kind, headline, details } = &status {
                    let message = if details.is_empty() {
                        headline.clone()
                    } else {
                        format!("{headline} — {}", details.join("; "))
                    };
                    messages.push(json!({
                        "id": log_id.clone(),
                        "type": "assistant",
                        "agent": reported_agent(),
                        "model": { "id": model, "providerID": provider },
                        "content": [],
                        "cost": 0,
                        "error": { "type": kind.clone().unwrap_or_else(|| "error".to_string()), "message": message },
                        "finish": "error",
                        "time": { "created": created, "completed": created },
                    }));
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
    // Ante's log is the transcript, live session or not: it holds every step and
    // every part of the conversation, where the shim's own store only ever held
    // the prompts it submitted. A session the client named itself lives under the
    // archive Ante opened for it, so that is what gets read back.
    let archive = store.ante.archive_of(&id).unwrap_or_else(|| id.clone());
    let mut data = replay_session(&archive, &store.handed());
    // The transcript is read newest-first with a limit; honour both.
    if params.get("order").map(|value| value == "desc").unwrap_or(false) {
        data.reverse();
    }
    if let Some(limit) = params.get("limit").and_then(|value| value.parse::<usize>().ok()) {
        data.truncate(limit);
    }
    Json(json!({ "data": data, "cursor": {} }))
}

/// Hand the oldest queued prompt to Ante. A queued prompt belongs at the turn
/// boundary, and only the live session can take one — anything else would land
/// in whichever session the connection happens to be driving.
async fn flush_queue(store: &Store, session: &str) {
    if store.ante.busy() || store.ante.live_id().as_deref() != Some(session) {
        return;
    }
    let Some((inbox_id, text)) = store.steer_head(session) else {
        return;
    };
    let Some(ops) = store.ante.ops.lock().await.clone() else {
        store.set_delivery(session, &inbox_id, "queue");
        return;
    };
    if let Err(err) = ops.send(ante_sdk::protocol::op_msg(Op::UserInput(text))).await {
        log_line(&format!("ante: 排队消息发送失败：{err}"));
        store.set_delivery(session, &inbox_id, "queue");
        return;
    }
    log_line(&format!("ante: 排队消息 {inbox_id} 交给 Ante（回合已结束）"));
}

/// `GET /api/session/{id}/inbox` — what the client queued and we are holding.
async fn session_inbox(State(store): State<Store>, Path(id): Path<String>) -> Json<Value> {
    Json(json!({ "data": store.queued(&id) }))
}

/// `PATCH /api/session/{id}/inbox/{inbox_id}` — steer a queued prompt into the
/// turn in flight (`{"delivery": "steer"}`) or leave it queued.
async fn session_inbox_update(
    State(store): State<Store>,
    Path((id, inbox_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> axum::http::StatusCode {
    let delivery = body.get("delivery").and_then(|v| v.as_str()).unwrap_or("queue").to_string();
    let Some(item) = store.queued(&id).into_iter().find(|item| item["id"] == inbox_id.as_str()) else {
        // Already handed over: the real server answers 409 for that.
        return axum::http::StatusCode::CONFLICT;
    };
    if item["delivery"] == "steer" {
        // `steer` in the held list means Ante already has the text — pulling it
        // back to the queue would only put it into the session a second time.
        log_line(&format!("ante: {inbox_id} 已经交给 Ante，改不回去了"));
        return axum::http::StatusCode::CONFLICT;
    }
    if delivery == "queue" {
        store.publish_durable(
            "session.inbox.delivery.changed",
            json!({ "sessionID": id, "inboxID": inbox_id, "delivery": "queue" }),
            &id,
        );
        return axum::http::StatusCode::NO_CONTENT;
    }
    // The text Ante gets is the staged one — the user's own plus the mentions for
    // any pasted images — while the payload keeps what the client displays.
    let display = item["payload"]["text"].as_str().unwrap_or("").to_string();
    let text = store.sent_text(&inbox_id).unwrap_or(display);
    // It stops being *queued* but is not delivered either: it stays held, now
    // labelled `steer`, and the pump clears it when Ante reports the input. The
    // client keeps drawing it as pending in the meantime — which is the truth
    // until Ante folds it into the turn.
    store.set_delivery(&id, &inbox_id, "steer");
    let live = store.ante.live_id().as_deref() == Some(id.as_str());
    match store.ante.ops.lock().await.clone() {
        Some(ops) => {
            // Steering only means something in the live session's running turn.
            let op = if live && store.ante.busy() {
                log_line("ante: 插嘴（Steer）——把排队的那条并进正在跑的这一轮");
                Op::Steer(text)
            } else {
                log_line("ante: 排队的那条现在交给 Ante（当前没有正在跑的回合）");
                Op::UserInput(text)
            };
            if let Err(err) = ops.send(ante_sdk::protocol::op_msg(op)).await {
                log_line(&format!("ante: 排队消息插嘴失败：{err}"));
                store.set_delivery(&id, &inbox_id, "queue");
                return axum::http::StatusCode::NO_CONTENT;
            }
            store.publish_durable(
                "session.inbox.delivery.changed",
                json!({ "sessionID": id, "inboxID": inbox_id, "delivery": "steer" }),
                &id,
            );
        }
        None => {
            store.set_delivery(&id, &inbox_id, "queue");
            log_line("ante: 与 Ante 未连接，排队消息没送出去");
        }
    }
    axum::http::StatusCode::NO_CONTENT
}

/// `DELETE /api/session/{id}/inbox/{inbox_id}` — drop a prompt we are holding. A
/// steered one is not ours to drop (Ante already has the text), so it answers 409
/// like the steer route does.
async fn session_inbox_cancel(
    State(store): State<Store>,
    Path((id, inbox_id)): Path<(String, String)>,
) -> axum::http::StatusCode {
    if store
        .queued(&id)
        .iter()
        .any(|item| item["id"] == inbox_id.as_str() && item["delivery"] == "steer")
    {
        log_line(&format!("ante: {inbox_id} 已经交给 Ante，删不掉了"));
        return axum::http::StatusCode::CONFLICT;
    }
    if store.take_queued(&id, &inbox_id).is_some() {
        store.publish_durable(
            "session.inbox.cancelled",
            json!({ "sessionID": id, "inboxID": inbox_id }),
            &id,
        );
    }
    axum::http::StatusCode::NO_CONTENT
}

/// The prompt the user typed. opencode has two deliveries for it — `steer` (into
/// the turn in flight, or a new turn when idle) and `queue` (held here until the
/// turn boundary) — and Ante has the matching ops: `Steer` and `UserInput`.
async fn session_prompt(
    State(store): State<Store>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let text = body.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
    // `steer` is what the client sends by default: typed while a turn is running
    // it goes *into* that turn (Ante's own `Ctrl+S`), otherwise it starts one.
    // `queue` is held by the shim until the turn boundary — see `Store::pending`.
    let delivery = match body.get("delivery").and_then(|v| v.as_str()) {
        Some("queue") => "queue",
        _ => "steer",
    };
    // The client sends its own message id; reusing it keeps its optimistic copy
    // and the inbox item the same entry instead of rendering the message twice.
    let user_id = body
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| uid("msg"));
    // Pasted images: the client sends inline `data:` URLs, Ante only reads
    // mentions — so the body becomes a staged copy in Ante's paste cache plus an
    // `@path` appended to the text we hand over. The client keeps its own text;
    // the attachments it draws come from the echo below.
    let attachments_in = body.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
    let (staged, files) = attachments::stage(&text, &attachments_in);
    if !files.is_empty() {
        log_line(&format!("attachments: 暂存 {} 个附件，随消息提及给 Ante", files.len()));
    }
    let user = json!({
        "id": user_id,
        "type": "user",
        "text": text,
        "files": files,
        "agents": [],
        "skills": [],
        "time": { "created": now_ms() },
    });
    store.publish("message.updated", json!({ "sessionID": id, "info": user }));

    // Tell the client about the user's message: `inbox.enqueued` admits it into
    // the transcript, and `delivered` moves it into place. Without these the
    // client only has its own optimistic copy and the order comes out wrong.
    let item = json!({
        "id": user_id,
        "sessionID": id,
        "time": { "created": now_ms() },
        "type": "user",
        "payload": { "text": text, "files": files, "agents": [], "skills": [] },
        "delivery": delivery,
    });
    store.publish_durable(
        "session.inbox.enqueued",
        json!({ "inboxID": user_id, "sessionID": id, "item": item }),
        &id,
    );
    // Held either way until Ante reports the input (`Evt::UserInput` → the pump
    // publishes `delivered`): `queue` waits for the turn boundary, `steer` for
    // Ante to fold it into the running step. Both are what the client draws a
    // pending prompt from, so neither may claim delivery here.
    if let Ok(mut queues) = store.pending.lock() {
        queues.entry(id.clone()).or_default().push(item.clone());
    }
    store.remember_sent(&user_id, &staged);
    store.remember_handed(&user_id, &staged);
    if delivery == "queue" {
        // Held, not handed over: it leaves the queue at the turn boundary — or
        // because the user steered/deleted it.
        return Json(json!({ "data": item }));
    }

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
    // Ante drives one session per connection, so a prompt aimed at a session
    // other than the live one switches it over first. Without this every prompt
    // landed in whichever session this run happened to open, and the reply was
    // merely *shown* under the session the TUI was looking at.
    let switch = store.ante.live_id().as_deref() != Some(id.as_str());
    match ops {
        Some(ops) => {
            if switch {
                // What our own turn will be answered with: Ante replays a
                // resumed conversation before it gets to this `UserInput`, and
                // the guard drops that replay until the turn this op id starts.
                let input = ante_sdk::protocol::op_msg(Op::UserInput(staged.clone()));
                // A session Ante has on disk is a real conversation to resume;
                // anything else (the id the client just invented) starts fresh.
                // The test is the directory, not `meta.json`: Ante writes that
                // only at `TurnEnd`, so a session killed mid-turn has the log
                // and no summary — and it is still the conversation to continue.
                let saved = if stored_session(&id) { id.parse::<Id>().ok() } else { None };
                if let Some(session_id) = saved {
                    store.ante.expect_turn(input.id.to_string());
                    let op = Op::ResumeSession { session_id, unattended: false };
                    if let Err(err) = ops.send(ante_sdk::protocol::op_msg(op)).await {
                        log_line(&format!("ante: 恢复会话 {id} 发送失败：{err}"));
                    }
                    // Resume resolves the permission mode from the host's own
                    // settings, so the TUI's `shift+tab` choice has to be put
                    // back on the session explicitly.
                    let update = ante_sdk::protocol::SessionUpdate {
                        permission_mode: Some(mode),
                        ..Default::default()
                    };
                    let _ = ops.send(ante_sdk::protocol::op_msg(Op::UpdateSession(update))).await;
                    log_line(&format!("ante: 切到旧会话 {id}（ResumeSession）"));
                } else {
                    let chosen = store.ante.model.lock().ok().and_then(|slot| slot.clone());
                    let (provider, model) = chosen.unwrap_or_else(|| (PROVIDER.into(), MODEL.into()));
                    let request = SessionRequest {
                        permission_mode: Some(mode),
                        provider: Some(provider),
                        model: Some(model),
                        ..Default::default()
                    };
                    if let Err(err) = ops.send(ante_sdk::protocol::op_msg(Op::StartSession(request))).await {
                        log_line(&format!("ante: 新建会话发送失败：{err}"));
                    }
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
                }
                if let Ok(mut live) = store.ante.live.lock() {
                    *live = Some(id.clone());
                }
                if let Err(err) = ops.send(input).await {
                    log_line(&format!("ante: 消息发送失败：{err}"));
                }
            } else if delivery == "steer" && store.ante.busy() {
                // `Ctrl+S` / plain Enter while a turn is running: Ante's `Steer`
                // folds the text into that turn instead of queueing it behind.
                match ops.send(ante_sdk::protocol::op_msg(Op::Steer(staged.clone()))).await {
                    Ok(()) => log_line("ante: 插嘴（Steer）——并进正在跑的这一轮"),
                    Err(err) => log_line(&format!("ante: 插嘴发送失败：{err}")),
                }
            } else {
                // Same session, nothing to switch: the agent may still have
                // changed, since `shift+tab` never calls the switch route.
                let update = ante_sdk::protocol::SessionUpdate {
                    permission_mode: Some(mode),
                    ..Default::default()
                };
                let _ = ops.send(ante_sdk::protocol::op_msg(Op::UpdateSession(update))).await;
                if let Err(err) = ops.send(ante_sdk::protocol::op_msg(Op::UserInput(staged.clone()))).await {
                    log_line(&format!("ante: 消息发送失败：{err}"));
                }
            }
            if let Ok(mut slot) = store.ante.last_user.lock() {
                *slot = text.clone();
            }
        }
        None => log_line("ante: 与 Ante 未连接，这条消息被丢弃（看日志开头的自检那行）"),
    }
    // A session that just became live may still carry a queue from before.
    flush_queue(&store, &id).await;

    Json(json!({ "data": { "id": user_id, "sessionID": id, "type": "user", "time": { "created": now_ms() }, "payload": { "text": text, "files": files, "agents": [], "skills": [] }, "delivery": delivery } }))
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
    // Where our own messages go, decided before anything can report a problem:
    // one-command mode hands the terminal to the TUI, so a file is the only
    // place they survive being printed. Serve mode keeps them on stdout.
    let _ = LOG_FILE.set(if args.serve_only {
        None
    } else {
        Some(std::env::temp_dir().join("antex.log"))
    });

    // Inside a Herdr pane, report what this pane's agent is doing; elsewhere
    // this is inert. Read here so a later failure still shows up as the pane
    // being `Ante` rather than the client's name.
    herdr::init();

    // Fail loudly here instead of turning into an interface that does nothing.
    ante_selfcheck().await;

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
        .route("/api/session/{id}", get(session_get).delete(session_delete))
        .route("/api/session/{id}/message", get(session_messages))
        .route("/api/session/{id}/prompt", post(session_prompt))
        .route("/api/session/{id}/interrupt", post(session_interrupt))
        .route("/api/session/{id}/permission", get(session_permissions))
        .route("/api/session/{id}/permission/{request_id}/reply", post(permission_reply))
        .route("/api/permission/request", get(permission_request))
        .route("/api/session/{id}/inbox", get(session_inbox))
        .route(
            "/api/session/{id}/inbox/{inbox_id}",
            patch(session_inbox_update).delete(session_inbox_cancel),
        )
        .route("/api/session/{id}/form", get(bare_empty))
        .route("/api/session/{id}/model", post(session_model))
        .route("/api/session/{id}/compact", post(session_compact))
        .route("/api/session/{id}/agent", post(session_agent))
        .route("/api/session/{id}/view", post(no_content))
        .route("/api/event", get(events))
        .fallback(fallback)
        .layer(axum::middleware::from_fn(log_request))
        .with_state(store);

    // 一体化模式可以多开：端口被占就往后找，不是错误。`serve` 模式按你给的端口，
    // 占了就明确报错（那是「我只起服务」的意思，换端口会让人连错）。
    let mut listener = None;
    let mut bound_port = args.port;
    for offset in 0..20u16 {
        let candidate = args.port.saturating_add(offset);
        match tokio::net::TcpListener::bind(("127.0.0.1", candidate)).await {
            Ok(l) => {
                listener = Some(l);
                bound_port = candidate;
                break;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse && !args.serve_only => continue,
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                eprintln!(
                    "端口 {candidate} 已被占用。换一个端口，或先停掉占用它的进程：\n  fuser -k {candidate}/tcp"
                );
                std::process::exit(1);
            }
            Err(err) => {
                eprintln!("监听 127.0.0.1:{candidate} 失败：{err}");
                std::process::exit(1);
            }
        }
    }
    let Some(listener) = listener else {
        eprintln!("从 {} 起连续 20 个端口都被占用，换个起点：antex {}", args.port, args.port + 50);
        std::process::exit(1);
    };

    let port = bound_port;
    let _ = SERVER_URL.set(format!("http://127.0.0.1:{port}"));
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
    if port != args.port {
        println!("端口 {} 被占，本实例改用 {port}", args.port);
    }
    let directory =
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(default_directory()));
    let client = client_executable();
    match std::process::Command::new(&client)
        .arg("--server")
        .arg(format!("http://127.0.0.1:{port}"))
        .arg(&directory)
        .status()
    {
        // The TUI owns the terminal; when it exits, so do we.
        Ok(status) => {
            // Hand the pane back before we go. Herdr would clear it anyway once
            // the shell is back, but that takes a second or two.
            herdr::release().await;
            std::process::exit(status.code().unwrap_or(0))
        }
        Err(err) => {
            eprintln!("起不了 opencode 客户端（{}）：{err}", client.display());
            eprintln!("用 ANTEX_CLIENT 指定它的路径；或只起服务：antex serve {port}");
            herdr::release().await;
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
