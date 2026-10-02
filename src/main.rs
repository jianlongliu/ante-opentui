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
    ConnectOptions, EventReceiver, OpSender, connect,
    protocol::{
        Evt, Id, Op, PermissionMode, ReviewDecision, SessionRequest, SessionUpdate, SkillMetadata,
        ToolDecision, ToolUse, TurnPauseReason,
    },
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, patch, post},
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
/// The pair to fall back on when Ante's own `settings.json` records no
/// provider/model yet, so the composer still shows something. It has to name an
/// entry in *your* catalog (provider-scoped), or the first turn comes back as an
/// HTTP 400 from the provider — so it is a placeholder here, and meant to be
/// pointed at your own pair with `ANTEX_PROVIDER` / `ANTEX_MODEL`.
const MODEL: &str = "example/example-model";
const PROVIDER: &str = "example";

/// `ANTEX_PROVIDER` / `ANTEX_MODEL` when set, else the placeholder above.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

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
        .map(str::to_string)
        .unwrap_or_else(|| env_or("ANTEX_PROVIDER", PROVIDER));
    let model = settings
        .get("provider_model")
        .and_then(|v| v.get(&provider))
        .and_then(|v| v.as_str())
        .or_else(|| settings.get("model").and_then(|v| v.as_str()))
        .map(str::to_string)
        .unwrap_or_else(|| env_or("ANTEX_MODEL", MODEL));
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
    /// The skills Ante announced for the session it is driving, in the client's
    /// `Skill.Info` shape. `/skills` answers from here rather than off the disk:
    /// the announced list is already the one Ante can actually invoke (`no_skills`,
    /// project scope and all), so the dialog cannot offer a skill the session
    /// does not have.
    skills: Arc<Mutex<Vec<Value>>>,
    /// Where the connection to Ante stands. See [`Link`].
    link: Arc<Mutex<Link>>,
    /// When the last "backend is gone" row went into the transcript.
    notice_at: Arc<Mutex<Option<std::time::Instant>>>,
}

/// An Ante turn paused for approval, held until the TUI answers.
struct PendingApproval {
    request_id: String,
    turn_id: Id,
    tools: Vec<ToolUse>,
}

/// Where the connection to Ante stands.
///
/// Ante is a separate program, so it can go away at any point: a crash, an
/// upgrade, a `pkill`, a host that never started. The shim is the only one who
/// can see that — the client only ever talks to the shim — so it is the shim's
/// job to say so (`health`, the transcript) and to get back on its feet.
#[derive(Clone, PartialEq, Eq)]
enum Link {
    /// The first connect, or a retry after a failure.
    Connecting,
    Up,
    /// Down. `since` is when it went down (not when it was last retried), so a
    /// flapping host still reports how long the outage really has been.
    Down { reason: String, since: i64, attempt: u32 },
}

impl Link {
    fn reason(&self) -> String {
        match self {
            Link::Connecting => "还没连上".to_string(),
            Link::Up => "连接正常".to_string(),
            Link::Down { reason, .. } => reason.clone(),
        }
    }

    fn json(&self) -> Value {
        match self {
            Link::Connecting => json!({ "connected": false, "state": "connecting" }),
            Link::Up => json!({ "connected": true, "state": "up" }),
            Link::Down { reason, since, attempt } => json!({
                "connected": false,
                "state": "down",
                "reason": reason,
                "since": since,
                "attempt": attempt,
            }),
        }
    }
}

/// How long a connection has to last before its failure stops counting towards
/// the backoff. A host that dies the moment it starts should keep slowing down
/// instead of retrying once a second forever.
const LINK_HEALTHY: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the replay guard waits for its own turn to appear. Ante replays a
/// resumed conversation in one burst, so a longer wait means the switch never
/// took effect.
const REPLAY_GUARD: std::time::Duration = std::time::Duration::from_secs(20);

/// Floor between two "the backend is gone" rows in the transcript. A host that
/// flaps must not stack one per retry on the user's screen.
const OFFLINE_NOTICE_FLOOR: std::time::Duration = std::time::Duration::from_secs(20);

/// Backoff between reconnect attempts, capped. The first retry is quick because
/// the common case by far is a host someone restarted by hand.
fn reconnect_delay(failures: u32) -> f32 {
    match failures {
        0 | 1 => 1.0,
        2 => 2.0,
        3 => 4.0,
        4 => 8.0,
        5 => 15.0,
        _ => 30.0,
    }
}

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
            skills: Arc::new(Mutex::new(Vec::new())),
            link: Arc::new(Mutex::new(Link::Connecting)),
            notice_at: Arc::new(Mutex::new(None)),
        }
    }

    fn link(&self) -> Link {
        self.link.lock().map(|link| link.clone()).unwrap_or(Link::Connecting)
    }

    fn link_up(&self) {
        if let Ok(mut link) = self.link.lock() {
            *link = Link::Up;
        }
    }

    /// Note that the link went down. `since` is kept from the previous failure,
    /// so a connection that keeps failing reports one outage rather than a
    /// stream of new ones.
    fn link_down(&self, reason: String, attempt: u32) {
        if let Ok(mut link) = self.link.lock() {
            let since = match &*link {
                Link::Down { since, .. } => *since,
                _ => now_ms(),
            };
            *link = Link::Down { reason, since, attempt };
        }
    }

    /// Drop everything that described the connection that just died: which
    /// session it drove, the replay guard it was waiting on, the approval it was
    /// sitting on, and the turn it was running. Anything left behind would be
    /// read as if it were still true of the *new* connection.
    fn forget_link_state(&self) {
        self.set_busy(false);
        if let Ok(mut live) = self.live.lock() {
            *live = None;
        }
        if let Ok(mut replay) = self.replay_turn.lock() {
            *replay = None;
        }
        if let Ok(mut pending) = self.pending.lock() {
            *pending = None;
        }
    }

    /// Whether a "backend is gone" row is due, and if so take the slot. Called
    /// once per notice, so the throttle cannot be fooled by two callers racing.
    fn notice_due(&self, floor: std::time::Duration) -> bool {
        let Ok(mut last) = self.notice_at.lock() else {
            return false;
        };
        let now = std::time::Instant::now();
        if last.is_some_and(|last| now.duration_since(last) < floor) {
            return false;
        }
        *last = Some(now);
        true
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

    /// Keep the skills the session announced. Ante repeats the whole list on
    /// every announcement, so this replaces rather than merges.
    fn remember_skills(&self, skills: &[SkillMetadata]) {
        if let Ok(mut slot) = self.skills.lock() {
            *slot = skills.iter().map(skill_info).collect();
        }
    }

    fn announced_skills(&self) -> Vec<Value> {
        self.skills.lock().map(|slot| slot.clone()).unwrap_or_default()
    }

    /// The archive [`attach_session`] may hand to `ResumeSession`: the one Ante
    /// has, and can open.
    ///
    /// `meta.json` is what Ante writes when a turn ends, so a session killed
    /// mid-flight leaves its directory — and its log — without it. `ResumeSession`
    /// on such a directory comes back as `Failed to resume session: No such file
    /// or directory (os error 2)`, a refusal the reconnect path cannot answer:
    /// by the time it arrives the queued prompt has been handed over and is gone.
    /// So it is treated as "no archive" — the transcript is still read back
    /// (`archive_of` still names it, which is also what delete needs), but the
    /// conversation restarts.
    fn resumable_archive_of(&self, id: &str) -> Option<String> {
        self.archive_of(id)
            .filter(|archive| ante_home().join("sessions").join(archive).join("meta.json").is_file())
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
    fn dropping(&self, event: &Evt) -> Replay {
        let Ok(mut slot) = self.replay_turn.lock() else {
            return Replay::Pass;
        };
        let Some((expected, armed)) = slot.as_ref() else {
            return Replay::Pass;
        };
        let ours = matches!(event, Evt::TurnStart { turn_id } if turn_id.to_string() == *expected);
        if ours {
            *slot = None;
            return Replay::Pass;
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
            return Replay::Unresumed;
        }
        Replay::Replay
    }
}

/// What [`Ante::dropping`] decided about an event that arrived while a resume
/// was in flight.
#[derive(PartialEq, Eq, Debug)]
enum Replay {
    /// No resume in flight: ordinary traffic.
    Pass,
    /// Part of the conversation Ante replayed — the client already has it, and
    /// drawing it again would double the transcript.
    Replay,
    /// The resume never took: Ante refused it, or its own turn never came. This
    /// is not a replay, and whatever was handed to that connection is still
    /// owed to the user.
    Unresumed,
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
    /// Inbox ids the client has already been told are delivered. The turn-end
    /// sweep exists to clear a marker for a prompt Ante never reported — but a
    /// steered prompt is *still* held there after its step boundary, so without
    /// this the sweep would send a second `delivered` for it. That matters: the
    /// client answers `delivered` by moving the prompt to the end of the
    /// transcript, so the repeat would shove it past the reply it had just been
    /// placed in front of.
    delivered: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Questions Ante asked and is still waiting on, keyed by the form id the
    /// client was given. A call shows up in the tool cell as "Asked N questions"
    /// — the options live in opencode's *form* prompt — so the call is mirrored
    /// into one and held here until the user picks something.
    forms: Arc<Mutex<HashMap<String, Value>>>,
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
            delivered: Arc::new(Mutex::new(std::collections::HashSet::new())),
            forms: Arc::new(Mutex::new(HashMap::new())),
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

    /// Take back every prompt this session holds that was handed to a session
    /// which never opened. Handing one over relabels it (see [`Self::steer_head`])
    /// and only an ack from Ante clears the marker, so a hand-over into a
    /// connection that refused the session leaves it looking taken when the text
    /// went nowhere. Returns how many came back.
    fn requeue_undelivered(&self, session: &str) -> usize {
        let Ok(mut queues) = self.pending.lock() else {
            return 0;
        };
        let Some(queue) = queues.get_mut(session) else {
            return 0;
        };
        let mut back = 0;
        for item in queue.iter_mut() {
            if item["delivery"] == "steer" {
                item["delivery"] = json!("queue");
                back += 1;
            }
        }
        back
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

    /// Note a prompt whose `delivered` just went out at a step boundary.
    fn mark_delivered(&self, inbox_id: &str) {
        if let Ok(mut delivered) = self.delivered.lock() {
            delivered.insert(inbox_id.to_string());
        }
    }

    /// Whether this prompt already had a `delivered`, and drop the note: the
    /// turn-end sweep is the last caller that asks.
    fn claim_delivered(&self, inbox_id: &str) -> bool {
        match self.delivered.lock() {
            Ok(mut delivered) => delivered.remove(inbox_id),
            Err(_) => false,
        }
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

    /// Hold a question form until it is answered or its call is over.
    fn hold_form(&self, form: Value) {
        let Some(id) = form["id"].as_str().map(str::to_string) else {
            return;
        };
        if let Ok(mut forms) = self.forms.lock() {
            forms.insert(id, form);
        }
    }

    fn take_form(&self, form_id: &str) -> Option<Value> {
        self.forms.lock().ok()?.remove(form_id)
    }

    /// The questions still open in this session, for the client's form list.
    fn held_forms(&self, session: &str) -> Vec<Value> {
        self.forms
            .lock()
            .map(|forms| {
                forms
                    .values()
                    .filter(|form| form["sessionID"] == session)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Forget the forms a finished tool call was holding, by the call's id.
    fn drop_tool_forms(&self, session: &str, tool_call: &str) -> Vec<String> {
        let Ok(mut forms) = self.forms.lock() else {
            return Vec::new();
        };
        let done: Vec<String> = forms
            .iter()
            .filter(|(_, form)| {
                form["sessionID"] == session && form["metadata"]["tool"]["id"] == tool_call
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &done {
            forms.remove(id);
        }
        done
    }

    /// Ephemeral event: no durable envelope.
    fn publish(&self, name: &str, data: Value) {
        self.publish_inner(name, data, None, None);
    }

    /// An event the client routes *by location*. Its handler for these sits
    /// behind `if (!event.location) return` in the client's store, so an event
    /// published without one is read and thrown away — silently, with no error
    /// anywhere. `form.*` is such a family (`permission.*`, which works, is
    /// handled before that gate).
    fn publish_located(&self, name: &str, data: Value) {
        self.publish_inner(name, data, None, Some(loc_plain()));
    }

    /// Durable event. `Payload` for a durable definition requires the
    /// `{aggregateID, seq, version}` envelope on top of the common fields.
    fn publish_durable(&self, name: &str, data: Value, aggregate: &str) {
        self.publish_inner(name, data, Some(aggregate.to_string()), None);
    }

    fn publish_inner(
        &self,
        name: &str,
        data: Value,
        aggregate: Option<String>,
        location: Option<Value>,
    ) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut event = json!({
            "id": uid("evt"),
            "type": name,
            "created": now_ms(),
            "data": data,
        });
        if let Some(place) = location {
            event["location"] = place;
        }
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

/// One announced skill in the client's `Skill.Info` shape.
///
/// `id` is Ante's own name for the skill, which is also what the client inserts
/// as `@<id>` — Ante resolves that mention itself from the listing it already
/// has in context. The dialog draws `name` and `description`; `path`/`content`
/// are carried for the type's sake, filled from the skill's own file when the
/// disk still has it.
fn skill_info(skill: &SkillMetadata) -> Value {
    let (path, content) = skill_file(&skill.name).unwrap_or_default();
    let mut info = json!({
        "id": skill.name,
        "name": skill.name,
        "path": path,
        "content": content,
        // Ante's listing is the model's menu as much as the user's, so a skill
        // is invocable both ways; the client only ever reads name/id/description.
        "autoinvoke": true,
    });
    if let Some(description) = skill.description.as_ref().filter(|text| !text.trim().is_empty()) {
        info["description"] = json!(description);
    }
    info
}

/// The directories Ante discovers skills from: the user's own, its system scope,
/// and the working directory's. `/skills` reads the same three when a session has
/// not announced its list yet (nothing is announced until the first prompt).
fn skill_roots() -> Vec<std::path::PathBuf> {
    vec![
        ante_home().join("skills"),
        ante_home().join(".system").join("skills"),
        std::path::Path::new(&default_directory()).join(".ante").join("skills"),
    ]
}

/// A skill's file, as `(path, body)` with the frontmatter stripped: `<root>/<name>/SKILL.md`.
fn skill_file(name: &str) -> Option<(String, String)> {
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return None;
    }
    for root in skill_roots() {
        let file = root.join(name).join("SKILL.md");
        if let Ok(text) = std::fs::read_to_string(&file) {
            return Some((file.to_string_lossy().into_owned(), skill_body(&text)));
        }
    }
    None
}

/// `SKILL.md` opens with a `---` YAML block; the client wants the body, and the
/// two fields here are the ones its dialog draws.
fn skill_body(text: &str) -> String {
    let (_, body) = split_frontmatter(text);
    body
}

fn split_frontmatter(text: &str) -> (String, String) {
    let rest = text.strip_prefix("---").unwrap_or("");
    if rest.is_empty() {
        return (String::new(), text.to_string());
    }
    match rest.split_once("\n---") {
        Some((head, body)) => (head.to_string(), body.trim_start_matches(['\n', '\r']).to_string()),
        None => (String::new(), text.to_string()),
    }
}

/// One `key: value` field out of a frontmatter block, unquoted and unwrapped.
fn frontmatter_field(head: &str, key: &str) -> Option<String> {
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        let value = value.trim().trim_matches(['"', '\'']).trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// Skills read off the disk, for the window before a session has announced any.
fn disk_skills() -> Vec<Value> {
    let mut found: Vec<Value> = Vec::new();
    for root in skill_roots() {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(text) = std::fs::read_to_string(entry.path().join("SKILL.md")) else {
                continue;
            };
            if found.iter().any(|skill| skill["id"] == json!(name)) {
                continue;
            }
            let (head, body) = split_frontmatter(&text);
            let skill = SkillMetadata {
                name: name.clone(),
                description: frontmatter_field(&head, "description"),
                scope: ante_sdk::protocol::Scope::User,
                argument_hint: None,
            };
            let mut info = skill_info(&skill);
            info["name"] = json!(frontmatter_field(&head, "name").unwrap_or(name));
            info["path"] = json!(entry.path().join("SKILL.md").to_string_lossy());
            info["content"] = json!(body);
            found.push(info);
        }
    }
    found.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    found
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

/// Ante names its tools (`Bash`, `Read`, `Agent`); the client draws its rich
/// per-tool cells only for the names it ships (`shell`, `read`, `subagent`, …)
/// and falls back to a generic key/value block for anything else. Renaming a
/// call to the client's spelling is what turns `Agent [description=…]` into the
/// client's own subagent cell. Tools without a counterpart there — `Edit` (its
/// cell wants a precomputed patch), `TodoWrite`, `ViewImage` — keep Ante's name
/// and stay generic rather than degrade into an empty cell.
fn client_tool_name(name: &str) -> &str {
    match name {
        "Bash" => "shell",
        "Read" => "read",
        "Write" => "write",
        "Glob" => "glob",
        "Grep" => "grep",
        "WebFetch" => "webfetch",
        "WebSearch" => "websearch",
        "Agent" => "subagent",
        "AskUser" => "question",
        other => other,
    }
}

/// `Read` and `Write` pass `file_path`; the client's views read `path`. Every
/// other argument key agrees, so the rest goes through untouched.
fn client_tool_args(name: &str, args: &Value) -> Value {
    let mut args = args.clone();
    if (name == "Read" || name == "Write")
        && let Some(object) = args.as_object_mut()
        && let Some(path) = object.get("file_path").cloned()
    {
        object.insert("path".to_string(), path);
    }
    args
}

/// Ante's `AskUser` as one of opencode's question forms.
///
/// The two sides do not agree on a shape: Ante sends
/// `{questions: [{header, question, options: [{label, description}], multiple}]}`
/// and the client draws *fields*. The option list is the whole point — the tool
/// cell can only say "Asked N questions" — so every question becomes a field: a
/// single pick a `string` carrying `options` (there is no enum field type), a
/// multiple one a `multiselect`.
fn askuser_form(args: &Value, session: &str, message_id: &str, tool_call: &str) -> Option<Value> {
    let questions = args["questions"].as_array().filter(|q| !q.is_empty())?;
    let mut fields = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        let header = question["header"].as_str().unwrap_or("");
        let ask = question["question"].as_str().unwrap_or("");
        // Ante has no field for a headline, so the header becomes the label and
        // the question itself the line under it.
        let label = if header.is_empty() { ask } else { header };
        // An option's label is also its value: the answer travels back to Ante as
        // the user's own message, so the words they saw are the words to send.
        let options: Vec<Value> = question["options"]
            .as_array()
            .map(|options| {
                options
                    .iter()
                    .filter_map(|option| {
                        let label = option["label"].as_str()?;
                        let mut mapped = json!({ "value": label, "label": label });
                        if let Some(description) = option["description"].as_str() {
                            mapped["description"] = json!(description);
                        }
                        Some(mapped)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let multiple = question["multiple"].as_bool().unwrap_or(false);
        let mut field = json!({
            "key": format!("q{index}"),
            "type": if multiple { "multiselect" } else { "string" },
            "title": if label.is_empty() { format!("q{index}") } else { label.to_string() },
            "required": true,
            // Writing an answer the model did not offer has to stay possible:
            // these are options, not the last word.
            "custom": question["custom"].as_bool().unwrap_or(true),
        });
        if !ask.is_empty() {
            field["description"] = json!(ask);
        }
        if !options.is_empty() {
            field["options"] = json!(options);
        }
        fields.push(field);
    }
    let title = match fields.len() {
        1 => short(fields[0]["title"].as_str().unwrap_or("Question"), 60),
        count => format!("{count} questions"),
    };
    Some(json!({
        // Derived from the call, so the same question never opens twice.
        "id": format!("frm_{tool_call}"),
        "sessionID": session,
        "title": title,
        "metadata": { "kind": "question", "tool": { "messageID": message_id, "id": tool_call } },
        "fields": fields,
    }))
}

/// What the user's answer becomes on the way back to Ante.
///
/// A plain message, because that is what settles the call there — Ante has no op
/// that answers an `AskUser` (a question is over when the next user input
/// arrives). The picked labels are the words the user saw in the prompt.
fn question_answer_text(form: &Value, answer: &Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    for field in form["fields"].as_array().into_iter().flatten() {
        let key = field["key"].as_str().unwrap_or_default();
        let picked = match &answer[key] {
            Value::Null => continue,
            Value::Array(values) => values
                .iter()
                .map(|value| value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string()))
                .collect::<Vec<_>>()
                .join(", "),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let picked = picked.trim();
        if picked.is_empty() {
            continue;
        }
        let label = field["title"]
            .as_str()
            .or_else(|| field["description"].as_str())
            .unwrap_or(key);
        lines.push(format!("- {label}：{picked}"));
    }
    if lines.is_empty() {
        // The client allows skipping a question, and "nothing" is an answer the
        // model should not have to guess at.
        return "（用户跳过了这次提问，没有作答）".to_string();
    }
    format!("回答提问：\n{}", lines.join("\n"))
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

/// Upstream plugins that front features Ante has no side for: VCS/diff, usage
/// stats, the opencode plugin system, `/btw`, the sidebar's MCP card. The client
/// takes `plugins` as an ordered list where an `-opencode.<id>` entry turns a
/// builtin off (`tui/plugin/context.tsx`), matching by exact id or `<prefix>.*`.
const DISABLED_PLUGINS: &[&str] = &[
    "-opencode.diffs",
    "-opencode.plugins",
    "-opencode.btw",
    "-opencode.sidebar.mcp",
];
// `-opencode.stats` is gone from this list: `/stats` is served now, off Ante's own
// session summaries (`/api/experimental/session/stats`).

/// Binds that ship live for commands with nothing behind them
/// (`tui/config/keybind.ts`: `<leader>t`, `ctrl+b`, `<leader>u`, `<leader>r`,
/// `<leader>down/up`). The command blacklist in the keymap patch only keeps them
/// out of the palette — the keys still fire without this. `session.export` left
/// this list: `/export` and `/copy` read the shim's own export endpoint, so
/// `<leader>x` is a working key again.
const DEAD_KEYBINDS: &[&str] = &[
    "session.undo",
    "session.redo",
    "session.background",
    "terminal.toggle",
    "terminal.select",
    "terminal.close",
];

/// The client's own config has three answers we do not want: image previews off,
/// session tabs on (`tabs.mode` absent → `auto` → on), and live entries/keys for
/// commands with nothing behind them. The client rewrites `cli.json` wholesale,
/// so a key that is not in memory is gone with it, and an unreadable file counts
/// as an empty one — either way the defaults come back (2026-10-01: previews
/// vanished after a rewrite, tabs came back while the file was momentarily
/// missing, the `-opencode.*` list was gone too). The client reads
/// `OPENCODE_CLI_CONFIG_CONTENT` and merges it **over** the file
/// (`cli/config/config.ts`), so filling in just the keys the file leaves unsaid
/// is the durable fix: nothing the file does can flip them again.
fn client_config_override(file: Option<&str>, env_set: bool) -> Option<String> {
    if env_set {
        return None;
    }
    // Unreadable or hand-edited JSONC counts as no opinion — the injection is
    // additive, so the worst case is the same two defaults we started from.
    let config: Option<Value> = file.and_then(|text| serde_json::from_str(text).ok());
    fn said<'a>(config: Option<&'a Value>, keys: &[&str]) -> bool {
        let mut node = match config {
            Some(node) => node,
            None => return false,
        };
        for key in keys {
            match node.get(key) {
                Some(next) => node = next,
                None => return false,
            }
        }
        true
    }
    let mut fill = json!({});
    if !said(config.as_ref(), &["tabs", "mode"]) {
        fill["tabs"] = json!({ "mode": "off" });
    }
    for section in ["session", "prompt"] {
        if !said(config.as_ref(), &[section, "image_preview"]) {
            fill[section] = json!({ "image_preview": true });
        }
    }
    // `plugins` and `keybinds` are the "hide what Ante cannot serve" layers, and
    // both were dropped from this file once already, so pin them here too.
    let mut plugins: Vec<Value> = config
        .as_ref()
        .and_then(|config| config.get("plugins"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let carried = plugins.len();
    for entry in DISABLED_PLUGINS {
        if !plugins.iter().any(|existing| existing.as_str() == Some(entry)) {
            plugins.push(json!(entry));
        }
    }
    // The merge replaces arrays instead of appending to them, so the file's own
    // entries only survive because they were carried into this list by hand.
    if plugins.len() > carried {
        fill["plugins"] = json!(plugins);
    }
    let mut keybinds = serde_json::Map::new();
    for key in DEAD_KEYBINDS {
        if !said(config.as_ref(), &["keybinds", key]) {
            keybinds.insert((*key).to_string(), json!("none"));
        }
    }
    if !keybinds.is_empty() {
        fill["keybinds"] = Value::Object(keybinds);
    }
    if fill.as_object().is_some_and(serde_json::Map::is_empty) {
        return None;
    }
    Some(fill.to_string())
}

/// Where the client's `cli.json` lives, resolved the way the client resolves it:
/// `OPENCODE_CONFIG_DIR` **is** the config directory (the client hands it
/// straight to `Global.Path.config`), while the XDG/HOME fallbacks name its
/// parent. Reading a different file is not harmless — a miss looks like "no
/// opinion", which costs the file's own entries.
fn client_config_path(getenv: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
    if let Some(dir) = getenv("OPENCODE_CONFIG_DIR") {
        return Some(std::path::PathBuf::from(dir).join("cli.json"));
    }
    getenv("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| getenv("HOME").map(|home| std::path::PathBuf::from(home).join(".config")))
        .map(|base| base.join("opencode").join("cli.json"))
}

/// `client_config_override` against the real filesystem.
fn client_config_from_disk() -> Option<String> {
    if std::env::var_os("OPENCODE_CLI_CONFIG_CONTENT").is_some() {
        return None;
    }
    let text = client_config_path(|key| std::env::var_os(key))
        .and_then(|file| std::fs::read_to_string(file).ok());
    client_config_override(text.as_deref(), false)
}

/// Pasted images ride in the prompt body as a `data:` URL, and a screenshot's
/// base64 runs to several megabytes — past axum's 2 MB default, which answers
/// 413 `Failed to buffer the request body: length limit exceeded` and reaches
/// the user as a bare "发不出去" with nothing in the log (2026-10-01; small
/// pastes kept working, which is what made it look random). Only the wire limit
/// needs to be generous: `attachments::stage` still caps the decoded bytes at
/// 20 MB and says so in the log.
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

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

/// Dial Ante. Success is transport-level — the child spawned, or the socket
/// dialed — so a host that dies on startup only shows up as a stream that
/// closes immediately, which the caller's backoff handles.
async fn connect_ante() -> Result<ante_sdk::Client, String> {
    let endpoint: ante_sdk::Endpoint = "stdio".parse().map_err(|err| format!("端点解析失败：{err}"))?;
    let bin = ante_executable();
    let mut options = ConnectOptions::default();
    options.executable = bin.clone();
    connect(endpoint, options).await.map_err(|err| {
        format!(
            "连接失败：{err}\n  ante = {}；antex 编译于 ante-sdk {}",
            bin.map(|p| p.display().to_string()).unwrap_or_else(|| "（没找到）".into()),
            env!("ANTE_SDK_VERSION"),
        )
    })
}

/// Keep Ante attached: connect, pump its events, and when the stream ends,
/// reconnect with backoff and pick the conversation back up.
///
/// It used to connect once and let the link die quietly — the interface stayed
/// up, `health` said `healthy`, and every later prompt went nowhere. The shim
/// cannot keep Ante alive, but it can say what happened and come back.
async fn spawn_ante(store: Store) {
    let mut failures: u32 = 0;
    loop {
        let started = std::time::Instant::now();
        // 短句给界面和 `health`，细节给日志：一条断线说明塞进转录行里没人看。
        let (reason, detail) = match connect_ante().await {
            Ok(client) => {
                let (ops, rx) = client.into_parts();
                *store.ante.ops.lock().await = Some(ops);
                store.ante.link_up();
                log_line("ante: 已连接，开始转发事件");
                // Ante is up and waiting for input; that is what `idle` means
                // here. The session id lands with the first state change inside
                // a session.
                herdr::report("idle", None, None);
                // Anything the client queued while we were down goes out now —
                // a prompt typed during the outage is held, not dropped.
                flush_after_reconnect(&store).await;
                let reason = pump_events(&store, rx).await;
                (reason.clone(), reason)
            }
            Err(err) => ("连不上 ante".to_string(), err),
        };
        // The connection is gone. Drop the handle first: it is what makes a
        // later prompt take the "not connected" path instead of sending into a
        // dead pipe, and dropping the last `OpSender` closes the child's stdin,
        // which is how it learns to exit.
        *store.ante.ops.lock().await = None;
        store.ante.forget_link_state();
        // 一连好过 5 秒的就算了结，退避从第一次重来（多数情况是主机刚被人
        // 重启）；紧接着又断才算「一直在失败」，退避才往上走。
        failures = if started.elapsed() >= LINK_HEALTHY { 0 } else { failures } + 1;
        store.ante.link_down(reason, failures);
        let delay = reconnect_delay(failures);
        log_line(&format!("ante: {detail}；{delay}s 后重连（第 {failures} 次）"));
        herdr::report("blocked", Some(&detail), None);
        tokio::time::sleep(std::time::Duration::from_secs_f32(delay)).await;
    }
}

/// 重连之后把掉线期间排队的消息交出去。交之前会先把连接切回那条会话，所以
/// 这一步顺带把「会话回来了」做了；没有排队的消息就什么都不做——不为恢复而
/// 恢复，Ante 那份历史重放留给下一条真消息去挡（见 [`attach_session`]）。
async fn flush_after_reconnect(store: &Store) {
    let viewed = store.ante.active.lock().ok().and_then(|slot| slot.clone());
    let mut sessions: Vec<String> = store
        .pending
        .lock()
        .map(|queues| {
            queues.iter().filter(|(_, queue)| !queue.is_empty()).map(|(id, _)| id.clone()).collect()
        })
        .unwrap_or_default();
    // 用户正看着的那条先来：它多半就是刚断掉的那条。
    sessions.sort_by_key(|id| Some(id) == viewed.as_ref());
    for session in sessions {
        flush_queue(store, &session).await;
    }
}

/// Point the live connection at `id`: resume its archive when Ante has one on
/// disk, otherwise start a fresh session with the client's chosen model.
///
/// `expect` is the op id of the `UserInput` that follows: a resume hands the
/// whole conversation back, and the replay guard drops that until the turn that
/// op starts shows up. Both the prompt route and the reconnect path go through
/// here, so a conversation survives the backend dying.
async fn attach_session(store: &Store, ops: &OpSender, id: &str, mode: PermissionMode, expect: &Id) {
    // The client's id and Ante's archive name are not the same thing, so the
    // archive is looked up by mapping, not by guessing.
    let known = store.ante.archive_of(id);
    let archive = store.ante.resumable_archive_of(id).and_then(|archive| archive.parse::<Id>().ok());
    match archive {
        Some(session_id) => {
            store.ante.expect_turn(expect.to_string());
            let op = Op::ResumeSession { session_id, unattended: false };
            if let Err(err) = ops.send(ante_sdk::protocol::op_msg(op)).await {
                log_line(&format!("ante: 恢复会话 {id} 发送失败：{err}"));
            }
            log_line(&format!("ante: 切到旧会话 {id}（ResumeSession）"));
        }
        None => {
            // An archive is there and Ante still cannot open it — the turn that
            // was writing it was killed — so this starts a new conversation
            // under the client's id. Say so; the alternative is a reply that
            // quietly ignores everything above it.
            if known.is_some() {
                announce_unresumed(store, id);
            }
            let chosen = store.ante.model.lock().ok().and_then(|slot| slot.clone());
            let (provider, model) = chosen.unwrap_or_else(|| {
                (env_or("ANTEX_PROVIDER", PROVIDER), env_or("ANTEX_MODEL", MODEL))
            });
            let request = SessionRequest {
                permission_mode: Some(mode),
                provider: Some(provider),
                model: Some(model),
                ..Default::default()
            };
            if let Err(err) = ops.send(ante_sdk::protocol::op_msg(Op::StartSession(request))).await {
                log_line(&format!("ante: 新建会话发送失败：{err}"));
            }
        }
    }
    // Resume resolves the permission mode from the host's own settings, so the
    // TUI's `shift+tab` choice has to be put back on the session explicitly.
    let update = SessionUpdate { permission_mode: Some(mode), ..Default::default() };
    let _ = ops.send(ante_sdk::protocol::op_msg(Op::UpdateSession(update))).await;
    if let Ok(mut live) = store.ante.live.lock() {
        *live = Some(id.to_string());
    }
}

/// The permission mode this session should run with: an explicit
/// `SHIM_PERMISSION_MODE` wins (debugging), otherwise it follows the agent the
/// client picked.
fn current_permission_mode(store: &Store) -> PermissionMode {
    match std::env::var("SHIM_PERMISSION_MODE").as_deref() {
        Ok("strict") => return PermissionMode::Strict,
        Ok("yolo") => return PermissionMode::Yolo,
        Ok("auto") => return PermissionMode::Auto,
        _ => {}
    }
    let agent = store
        .ante
        .agent
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or_else(|_| reported_agent().to_string());
    permission_mode_for(&agent)
}

/// Put "the backend is gone" in front of the user, in the place they are
/// already looking.
fn announce_offline(store: &Store, session: &str, reason: &str, open_message: Option<&str>) {
    if !store.ante.notice_due(OFFLINE_NOTICE_FLOOR) {
        return;
    }
    publish_failure(
        store,
        session,
        json!({
            "type": "ante_disconnected",
            "message": format!(
                "与 Ante 后端的连接断了（{reason}）。antex 正在自动重连，之后排队的消息会自动发出去。"
            ),
        }),
        open_message,
    );
}

/// The conversation cannot be picked back up, so it restarts. Said out loud,
/// because the transcript in front of the user still shows the old one and a
/// model that has forgotten it is not something to discover from a reply that
/// no longer fits.
fn announce_unresumed(store: &Store, session: &str) {
    publish_failure(
        store,
        session,
        json!({
            "type": "session_unresumed",
            "message": "这条会话在 Ante 里打不开（上一轮没跑完就被杀了，存档没落盘），接不上上次那段；掉线期间排队的消息还在，下一条消息会新开一条会话带走它。",
        }),
        None,
    );
}

/// A failure in the transcript, plus the events that put the session back to
/// idle: without them the client keeps its spinner up, and it ignores a failure
/// for a message it never saw start. `open_message` is the assistant row a dead
/// turn left open — failing *that* row is better than appending another one —
/// and `None` opens a row to fail.
fn publish_failure(store: &Store, session: &str, error: Value, open_message: Option<&str>) {
    let message_id = match open_message {
        Some(message_id) => message_id.to_string(),
        None => {
            let id = format!("msg_offline_{}", now_ms());
            let (provider, model) = store.current_model();
            store.publish_durable(
                "session.step.started",
                json!({
                    "sessionID": session,
                    "assistantMessageID": id,
                    "agent": reported_agent(),
                    "model": { "id": model, "providerID": provider },
                    "started": now_ms(),
                }),
                session,
            );
            id
        }
    };
    store.publish_durable(
        "session.step.failed",
        json!({
            "sessionID": session,
            "assistantMessageID": message_id,
            "error": error,
            "executed": false,
        }),
        session,
    );
    // Also the thing that puts the session back to idle: without it the TUI
    // keeps its spinner up for a turn that will never end.
    store.publish_durable(
        "session.execution.failed",
        json!({ "sessionID": session, "error": error }),
        session,
    );
}

/// Ante's events, translated into opencode's. Returns why the stream ended —
/// Ante exited, or it dropped us.
async fn pump_events(store: &Store, mut rx: EventReceiver) -> String {
    let mut goodbye = false;
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
        let unresumed = match store.ante.dropping(&msg.event) {
            Replay::Replay => continue,
            Replay::Pass => false,
            Replay::Unresumed => true,
        };
        let Some(session) = store.ante.active.lock().ok().and_then(|s| s.clone()) else {
            continue;
        };
        // The session this connection was switching to never opened, so the
        // prompts the flush just handed over went to a session that does not
        // exist: `Op::UserInput` is answered with `session not initialized` and
        // the text is dropped. Take them back, and say what happened — the next
        // prompt starts a new conversation, which is not something to find out
        // from a history that quietly stops matching.
        if unresumed {
            store.requeue_undelivered(&session);
            announce_unresumed(store, &session);
        }
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
                    // A step is the boundary Ante folds accepted prompts into, so
                    // anything it took before this one is being read now. Send
                    // `delivered` **before** `step.started`: the client answers it by
                    // moving that prompt to the end of the transcript, and the step
                    // appends the assistant message right after it. The other order
                    // prints the reply above the prompt it answers.
                    let acked = store.take_acked(&session);
                    for inbox_id in &acked {
                        log_line(&format!("ante: 消息 {inbox_id} 送达（模型开始读）"));
                        store.mark_delivered(inbox_id);
                        store.publish_durable(
                            "session.inbox.delivered",
                            json!({ "inboxID": inbox_id.as_str(), "sessionID": session }),
                            &session,
                        );
                    }
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
                store.ante.remember_skills(&info.skills);
                if let Some(client) = store.ante.active.lock().ok().and_then(|slot| slot.clone()) {
                    let archive = info.session_id.to_string();
                    if archive != client {
                        log_line(&format!("ante: 会话 {client} 的存档是 {archive}"));
                        store.ante.remember_archive(&client, archive.clone());
                    }
                    // A title Ante already has — one set from its own front end, or
                    // by a rename this shim asked for — is what the picker should
                    // read back, so keep the shim's copy in step with it.
                    if let Some(title) = info.title.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
                        write_title(&archive, Some(title));
                    }
                }
            }
            // Ante echoing a settings change back; for this shim the one that
            // matters is a title, since it is the only field the TUI can set here.
            Evt::SessionUpdated(info) => {
                store.ante.remember_skills(&info.skills);
                let archive = info.session_id.to_string();
                if let Some(title) = info.title.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
                    write_title(&archive, Some(title));
                    let client = store
                        .ante
                        .active
                        .lock()
                        .ok()
                        .and_then(|slot| slot.clone())
                        .unwrap_or_else(|| archive.clone());
                    if let Ok(mut sessions) = store.sessions.lock()
                        && let Some(entry) = sessions
                            .iter_mut()
                            .find(|session| session["id"].as_str() == Some(client.as_str()))
                    {
                        entry["title"] = json!(title);
                    }
                    store.publish_durable(
                        "session.renamed",
                        json!({ "sessionID": client, "title": title }),
                        &client,
                    );
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
                let name = client_tool_name(&tool.name);
                let args = client_tool_args(&tool.name, &tool.args);
                store.publish_durable(
                    "session.tool.input.started",
                    json!({
                        "sessionID": session,
                        "assistantMessageID": message_id,
                        "id": tool.id,
                        "name": name,
                    }),
                    &session,
                );
                store.publish_durable(
                    "session.tool.input.ended",
                    json!({
                        "sessionID": session,
                        "assistantMessageID": message_id,
                        "id": tool.id,
                        "text": args.to_string(),
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
                        "input": args,
                    }),
                    &session,
                );
                // The cell can only say "Asked N questions" — the options are the
                // client's form prompt, so the call is mirrored into one. The
                // location has to ride both the event and the form itself: the
                // client's `form.created` branch sits behind a `location` gate,
                // and its `removeForm` keeps any form that carries none.
                if tool.name == "AskUser"
                    && let Some(mut form) = askuser_form(&tool.args, &session, &message_id, &tool.id)
                {
                    form["location"] = loc_plain();
                    store.hold_form(form.clone());
                    store.publish_located("form.created", json!({ "form": form }));
                }
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
                // A question stands only while its call does: answered (the reply
                // came back as the user's message) or overtaken, the prompt goes.
                for form in store.drop_tool_forms(&session, &end.tool_use_id) {
                    store.publish_located("form.cancelled", json!({ "sessionID": session, "id": form }));
                }
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
                    // A steered prompt keeps its queue entry past its step, so the
                    // sweep would otherwise announce it twice.
                    if store.claim_delivered(&inbox_id) {
                        continue;
                    }
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
            // Ante answered `Shutdown`: this connection is ending on purpose
            // rather than dying, which is worth distinguishing in the log.
            Evt::Goodbye => goodbye = true,
            _ => {}
        }
    }
    // The stream ended. A turn that was running will never get its `TurnEnd`,
    // so if one was open, fail it here — otherwise the TUI spins forever over a
    // reply that Ante never had the chance to send.
    if let Some(session) = store.ante.active.lock().ok().and_then(|slot| slot.clone()) {
        let open = step_open.then(|| message_id.clone());
        announce_offline(
            store,
            &session,
            if goodbye { "Ante 关闭了连接" } else { "事件流结束" },
            open.as_deref(),
        );
    }
    if goodbye {
        "Ante 关闭了连接".to_string()
    } else {
        "事件流结束".to_string()
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
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> axum::http::StatusCode {
    let model = body.get("model").cloned().unwrap_or_default();
    let id = model
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| env_or("ANTEX_MODEL", MODEL));
    let provider = model
        .get("providerID")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| env_or("ANTEX_PROVIDER", PROVIDER));
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

/// The shim's own health is not in question — it is up, that is how you got
/// here. What the client (and a human with `curl`) cannot otherwise see is
/// whether the *backend* is: `ante` carries that, and `healthy` stays true on
/// purpose, because the client reads it as "is this server usable".
async fn health(State(store): State<Store>) -> Json<Value> {
    Json(json!({
        "healthy": true,
        // `version` is the Ante backend's — the shim stands in for Ante, so the
        // client's own version here told the user nothing about either end.
        "version": ANTE_VERSION.get().map(String::as_str).unwrap_or("unknown"),
        "antex": ANTEX_VERSION,
        "pid": std::process::id(),
        "urls": [SERVER_URL.get().map(String::as_str).unwrap_or("")],
        "paths": { "tmp": std::env::temp_dir().to_string_lossy() },
        // `{connected, state, reason, since, attempt}` — the connection to Ante,
        // which this process reconnects by itself.
        "ante": store.ante.link().json(),
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
                "model": {
                    "id": env_or("ANTEX_MODEL", MODEL),
                    "providerID": env_or("ANTEX_PROVIDER", PROVIDER),
                },
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
            "name": model.clone(),
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
        let provider = env_or("ANTEX_PROVIDER", PROVIDER);
        list.push(json!({
            "id": provider, "name": provider, "activation": "auto", "package": provider,
        }));
    }
    envelope(json!(list))
}

async fn vcs() -> Json<Value> {
    // Ante has no VCS. An empty branch keeps the client from appending a fake
    // `:main` to every location label (`dir:branch` in the prompt and sidebar footers).
    envelope(json!({ "branch": {} }))
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
    Json(json!({ "data": {} }))
}

/// The title a rename wrote, and the file it lives in.
///
/// Ante has titles of its own (`SessionUpdate.title`, echoed back in `SessionInfo`),
/// but it only tells us about them while it is driving the session — the session
/// *picker* is built off the directory, so a rename writes this file next to
/// Ante's own. Ante neither reads nor rewrites it: it owns `meta.json`,
/// `events.jsonl` and its snapshot, and this is not one of those.
fn title_file(dir: &str) -> std::path::PathBuf {
    ante_home().join("sessions").join(dir).join("title.txt")
}

fn title_override(dir: &str) -> Option<String> {
    let text = std::fs::read_to_string(title_file(dir)).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| short(&text.replace(['\n', '\r'], " "), 60))
}

/// Set the title, or (`None`) drop the override so the derived one shows again.
///
/// Nothing is written for a session Ante has not opened yet: a directory holding
/// only `title.txt` would show up as an archive with nothing in it, while a rename
/// before the first prompt has nothing to persist anyway (the name shows in the
/// open session either way).
fn write_title(dir: &str, title: Option<&str>) {
    let dir = ante_home().join("sessions").join(dir);
    if !dir.is_dir() {
        log_line(&format!("rename: {} 还没有存档目录，标题只留在内存里", dir.display()));
        return;
    }
    let path = dir.join("title.txt");
    match title.map(str::trim).filter(|text| !text.is_empty()) {
        Some(title) => {
            let _ = std::fs::write(&path, format!("{title}\n"));
        }
        None => {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// What the session is called with no rename to go on: Ante's own summary of the
/// first thing asked, or the same fact read back off the log when the turn that
/// would have written the summary never ended.
fn derived_title(dir: &str) -> String {
    if let Some(title) = title_override(dir) {
        return title;
    }
    if let Some(title) = session_meta(dir)
        .and_then(|meta| meta.get("first_user_message").and_then(|v| v.as_str()).map(str::to_string))
    {
        return short(&title.replace(['\n', '\r'], " "), 60);
    }
    log_title(dir).unwrap_or_else(|| "untitled".into())
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
    for line in raw.lines() {
        let Ok(wrapper) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        created = wrapper
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .map(|when| when.timestamp_millis());
        if created.is_some() {
            break;
        }
    }
    Some((created.unwrap_or_else(now_ms), derived_title(id)))
}

/// The title the *log* implies: the first thing the user asked.
fn log_title(id: &str) -> Option<String> {
    let raw = std::fs::read_to_string(ante_home().join("sessions").join(id).join("events.jsonl")).ok()?;
    for line in raw.lines() {
        let Ok(wrapper) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(text) = wrapper
            .get("event")
            .and_then(|event| event.get("UserInput"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        // The log keeps the staged mentions and Ante's expansion of them; a title
        // is the message, not the paths.
        let (shown, _) = attachments::from_log(text);
        let shown = if shown.trim().is_empty() { text.to_string() } else { shown };
        return Some(short(&shown.replace(['\n', '\r'], " "), 60));
    }
    None
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
                    let title = derived_title(&dir);
                    let usage = meta.get("usage").cloned().unwrap_or_else(|| json!({}));
                    let tokens = tokens_json(
                        usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                        usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                    );
                    let model = meta
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| env_or("ANTEX_MODEL", MODEL));
                    let provider = meta
                        .get("provider")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| env_or("ANTEX_PROVIDER", PROVIDER));
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
    let title = {
        let derived = derived_title(id);
        if derived == "untitled" { "Ante session".to_string() } else { derived }
    };
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
                    let name = client_tool_name(&tool.name);
                    let input = client_tool_args(&tool.name, &tool.args);
                    content.push(json!({
                        "type": "tool",
                        "id": tool.id,
                        "name": name,
                        "executed": false,
                        "state": { "status": "running", "input": input, "metadata": {} },
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

/// A refusal the client shows verbatim: `message` is what it puts in the toast.
fn bad_request(message: String) -> Response {
    (axum::http::StatusCode::BAD_REQUEST, Json(json!({ "message": message }))).into_response()
}

/// `PATCH /api/session/{id}` — the client's `/rename`, and the only one of the
/// route's three body keys Ante has a field for.
///
/// The title is Ante's own (`SessionUpdate.title`): it travels with the session,
/// so a resume brings it back and Ante's other front ends show it too. Only the
/// session Ante is *driving* can be renamed through it — there is no "rename that
/// one over there" op, and resuming a session merely to rename it would hijack the
/// connection the TUI is watching, replay and all. So a rename aimed elsewhere
/// lands in the shim's own `title.txt` and is honoured by the picker, which is
/// where the client reads titles from anyway.
async fn session_update(
    State(store): State<Store>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if !safe_id(&id) {
        return bad_request(format!("不认识的会话 id：{id}"));
    }
    // `metadata` and `permissions` are the route's other two keys: neither is
    // something Ante has, and the client only ever sends them beside a title.
    let Some(title) = body.get("title").and_then(Value::as_str) else {
        return axum::http::StatusCode::NO_CONTENT.into_response();
    };
    let title = title.trim().to_string();
    let archive = store.ante.archive_of(&id).unwrap_or_else(|| id.clone());
    let shown = if title.is_empty() {
        // Empty clears it, as Ante's own contract says; the derived title is what
        // both sides fall back to.
        write_title(&archive, None);
        derived_title(&archive)
    } else {
        write_title(&archive, Some(&title));
        title
    };
    if let Ok(mut sessions) = store.sessions.lock()
        && let Some(info) = sessions.iter_mut().find(|session| session["id"].as_str() == Some(id.as_str()))
    {
        info["title"] = json!(shown);
    }
    if store.ante.live_id().as_deref() == Some(id.as_str())
        && let Some(ops) = store.ante.ops.lock().await.clone()
    {
        let update = SessionUpdate { title: Some(shown.clone()), ..Default::default() };
        if let Err(err) = ops.send(ante_sdk::protocol::op_msg(Op::UpdateSession(update))).await {
            log_line(&format!("rename: 发送失败：{err}"));
        }
    } else {
        log_line(&format!("rename: {id} 不是当前会话，标题只落在 title.txt"));
    }
    store.publish_durable("session.renamed", json!({ "sessionID": id, "title": shown }), &id);
    axum::http::StatusCode::NO_CONTENT.into_response()
}

/// `POST /api/session/{id}/move` — the client's in-session `/cd`.
///
/// Ante fixes a session's directory at `StartSession` (`SessionRequest.cwd`) and
/// `SessionUpdate` carries none, so there is nothing to move *to*: the honest
/// answer is a refusal that says so. The generic fallback used to answer 200 here,
/// which the client reports as `UnexpectedStatus: 200` — true, but unreadable.
async fn session_move(Path(_id): Path<String>, Json(_body): Json<Value>) -> Response {
    bad_request(
        "Ante 的会话目录在建会话时就定死了，没有换目录的接口。要换目录请开一条新会话（/new）"
            .to_string(),
    )
}

/// `GET /api/experimental/session/{id}/export` — what `/copy` and `/export` read.
///
/// The same transcript the message route serves, wrapped with the session it
/// belongs to. Upstream's `sanitize` flag strips absolute paths; this payload is
/// Ante's own log and is not rewritten, so both spellings answer alike.
async fn session_export(State(store): State<Store>, Path(id): Path<String>) -> Json<Value> {
    let archive = store.ante.archive_of(&id).unwrap_or_else(|| id.clone());
    let info = store
        .session(&id)
        .or_else(|| ante_session_info(&id))
        .unwrap_or_else(|| session_info(&id, &derived_title(&archive), store.current_model()));
    let messages = replay_session(&archive, &store.handed());
    Json(json!({ "data": { "info": info, "messages": messages } }))
}

/// One session as the stats fold wants it: when it started, Ante's own summary of
/// it, and how many prompts its log recorded.
struct SessionRow {
    created: i64,
    meta: Value,
    prompts: u64,
}

/// How many prompts Ante's log recorded (`Evt::UserInput` is written per input).
fn count_prompts(dir: &str) -> u64 {
    let Ok(raw) = std::fs::read_to_string(ante_home().join("sessions").join(dir).join("events.jsonl"))
    else {
        return 0;
    };
    raw.lines().filter(|line| line.contains("\"UserInput\"")).count() as u64
}

/// The local calendar day of a millisecond timestamp, which is what the client's
/// activity calendar buckets by.
fn day_of(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|when| when.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Fold Ante's session summaries into the client's `SessionStatsInfo`.
///
/// Every number is Ante's own: sessions and tokens out of `usage`, activity by the
/// day each session started, prompts by counting the inputs its log recorded.
/// Anything Ante does not write down is reported as zero rather than guessed —
/// there is no cost accounting (the protocol carries no prices) and no subagent
/// tally (a subagent is a tool call inside its parent's turn).
fn stats_from_rows(rows: &[SessionRow], from: Option<i64>, to: i64) -> Value {
    let mut sessions = 0u64;
    let mut prompts = 0u64;
    let mut steps = 0u64;
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;
    let mut earliest = None;
    let mut days: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut models: Vec<(String, String, u64, u64, u64, u64, u64)> = Vec::new();

    for row in rows {
        if from.is_some_and(|from| row.created < from) || row.created > to {
            continue;
        }
        sessions += 1;
        prompts += row.prompts;
        earliest = Some(earliest.map_or(row.created, |first: i64| first.min(row.created)));
        let count = row.meta.get("message_count").and_then(Value::as_u64).unwrap_or(0);
        steps += count;
        *days.entry(day_of(row.created)).or_default() += count;
        let usage = row.meta.get("usage").cloned().unwrap_or_else(|| json!({}));
        let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        let (cue, coutput, cread, cwrite) = (
            field("input_tokens"),
            field("output_tokens"),
            field("cache_read_tokens"),
            field("cache_creation_tokens"),
        );
        input += cue;
        output += coutput;
        cache_read += cread;
        cache_write += cwrite;
        let (provider, model) = meta_model(&row.meta);
        match models
            .iter_mut()
            .find(|entry| entry.0 == provider && entry.1 == model)
        {
            Some(entry) => {
                entry.2 += count;
                entry.3 += cue;
                entry.4 += coutput;
                entry.5 += cread;
                entry.6 += cwrite;
            }
            None => models.push((provider, model, count, cue, coutput, cread, cwrite)),
        }
    }

    // Best streak: the longest run of consecutive days that saw a session.
    let mut streak = 0u64;
    let mut run = 0u64;
    let mut previous: Option<chrono::NaiveDate> = None;
    for date in days.keys().filter_map(|date| chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()) {
        run = match previous {
            Some(last) if last.succ_opt() == Some(date) => run + 1,
            _ => 1,
        };
        streak = streak.max(run);
        previous = Some(date);
    }

    models.sort_by(|a, b| b.2.cmp(&a.2));
    let activity: Vec<Value> =
        days.iter().map(|(date, steps)| json!({ "date": date, "steps": steps })).collect();
    let models: Vec<Value> = models
        .into_iter()
        .map(|(provider, model, steps, input, output, cache_read, cache_write)| {
            json!({
                "model": { "id": model, "providerID": provider, "variant": "default" },
                "steps": steps,
                "tokens": { "input": input, "output": output, "reasoning": 0,
                            "cache": { "read": cache_read, "write": cache_write } },
                "cost": 0,
            })
        })
        .collect();

    json!({
        "range": { "from": from.or(earliest).unwrap_or(to), "to": to },
        "sessions": sessions,
        "subagents": 0,
        "prompts": prompts,
        "steps": steps,
        "tokens": { "input": input, "output": output, "reasoning": 0,
                    "cache": { "read": cache_read, "write": cache_write } },
        "cost": 0,
        "tools": { "mode": "none" },
        "activeDays": days.len(),
        "streak": streak,
        "activity": activity,
        "models": models,
    })
}

/// `GET /api/experimental/session/stats` — `/stats`.
async fn session_stats(
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Json<Value> {
    let from = params.get("from").and_then(|value| value.parse::<i64>().ok());
    let to = now_ms();
    let mut rows = Vec::new();
    if let Ok(entries) = std::fs::read_dir(ante_home().join("sessions")) {
        for entry in entries.flatten() {
            let dir = entry.file_name().to_string_lossy().into_owned();
            // Only sessions Ante wrote a summary for carry numbers at all.
            let Some(meta) = session_meta(&dir) else {
                continue;
            };
            let created = meta
                .get("started_time")
                .and_then(Value::as_str)
                .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                .map(|when| when.timestamp_millis())
                .unwrap_or_else(now_ms);
            // Out of range sessions are skipped before their log is read: this is
            // one file per session, and the client asks for a year at a time.
            if from.is_some_and(|from| created < from) {
                continue;
            }
            rows.push(SessionRow { created, prompts: count_prompts(&dir), meta });
        }
    }
    Json(json!({ "data": stats_from_rows(&rows, from, to) }))
}

/// `GET /api/skill` — `/skills`.
///
/// Ante announces the skills a session has, so that list is the answer: it is
/// already the set that can actually be invoked (`no_skills`, project scope and
/// all). The disk is read only before a session has announced anything, which is
/// the window between opening the TUI and sending the first prompt.
async fn skills(State(store): State<Store>) -> Json<Value> {
    let announced = store.ante.announced_skills();
    let list = if announced.is_empty() { disk_skills() } else { announced };
    envelope(json!(list))
}

/// Hand the oldest queued prompt to Ante. A queued prompt belongs at the turn
/// boundary, so a running turn keeps it waiting. If the connection is not on
/// this session — after a disconnect, say — the switch happens here first, which
/// is also what puts the conversation back on its feet; anything else would land
/// in whichever session the connection happens to be driving.
async fn flush_queue(store: &Store, session: &str) {
    if store.ante.busy() {
        return;
    }
    let Some((inbox_id, text)) = store.steer_head(session) else {
        return;
    };
    let Some(ops) = store.ante.ops.lock().await.clone() else {
        store.set_delivery(session, &inbox_id, "queue");
        return;
    };
    let input = ante_sdk::protocol::op_msg(Op::UserInput(text));
    if store.ante.live_id().as_deref() != Some(session) {
        attach_session(store, &ops, session, current_permission_mode(store), &input.id).await;
    }
    if let Err(err) = ops.send(input).await {
        log_line(&format!("ante: 排队消息发送失败：{err}"));
        store.set_delivery(session, &inbox_id, "queue");
        return;
    }
    log_line(&format!("ante: 排队消息 {inbox_id} 交给 Ante"));
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

/// `GET /api/session/{id}/form` — the questions still open in this session. The
/// client reads it when a session is opened, so answering has to survive a tab
/// switch; a question that is over is gone from here, not left as a stale prompt.
/// Like `/inbox`, the schema is strict (`additionalProperties: false`), so the
/// answer is a bare `{data: […]}` — a `location` field fails the whole read and
/// the session view then refuses to open.
async fn session_forms(State(store): State<Store>, Path(id): Path<String>) -> Json<Value> {
    Json(json!({ "data": store.held_forms(&id) }))
}

/// `DELETE /api/session/{id}/form/{form_id}` — the user dismissed the question.
/// Nothing goes to Ante: it cannot withdraw the call, and the next message the
/// user sends settles it either way.
async fn session_form_cancel(
    State(store): State<Store>,
    Path((id, form_id)): Path<(String, String)>,
) -> axum::http::StatusCode {
    if store.take_form(&form_id).is_none() {
        return axum::http::StatusCode::NOT_FOUND;
    }
    store.publish_located("form.cancelled", json!({ "sessionID": id, "id": form_id }));
    axum::http::StatusCode::NO_CONTENT
}

/// `POST /api/session/{id}/form/{form_id}/reply` — the user picked an option.
///
/// The answer goes to Ante as the user's next message, which is the only thing
/// that settles an `AskUser` there (see `askuser_form`) — so the reply rides the
/// ordinary prompt route instead of a channel of its own, and the picked label
/// lands in the transcript as what it is: what the user said.
async fn session_form_reply(
    State(store): State<Store>,
    Path((id, form_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> axum::http::StatusCode {
    let Some(form) = store.take_form(&form_id) else {
        return axum::http::StatusCode::NOT_FOUND;
    };
    let answer = body.get("answer").cloned().unwrap_or(json!({}));
    let text = question_answer_text(&form, &answer);
    store.publish_located("form.replied", json!({ "sessionID": id, "id": form_id, "answer": answer }));
    let prompt = json!({ "text": text, "id": uid("msg"), "delivery": "steer" });
    let _ = session_prompt(State(store.clone()), Path(id), Json(prompt)).await;
    axum::http::StatusCode::NO_CONTENT
}

/// Fire-and-forget card in the corner of the shell — the same channel the
/// clipboard shim uses, and the one surface a notice reaches the user through
/// without the client having to draw it. `omarchy-osd` only exists on Omarchy:
/// a spawn that fails is simply no card, and the log line is the whole record.
fn osd_notice(message: &str) {
    const DURATION_MS: &str = "6000";
    let args = ["-m", message, "-d", DURATION_MS];
    let run = |bin: std::path::PathBuf| {
        std::process::Command::new(bin)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    };
    if run(std::path::PathBuf::from("omarchy-osd")).is_ok() {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else { return };
    let bin = std::path::PathBuf::from(home).join(".local/share/omarchy/bin/omarchy-osd");
    if let Err(err) = run(bin) {
        log_line(&format!("attachments: 卡片弹不出来：{err}"));
    }
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
    let (staged, files, dropped) = attachments::stage(&text, &attachments_in);
    let (mut text, mut staged) = (text, staged);
    if !dropped.is_empty() {
        // An attachment that could not be staged has no channel of its own: the
        // client turns a failed send into a bare status code, and a pushed
        // transcript row is not drawn. So the sentence rides in the message —
        // Ante is told the image never arrived instead of guessing — and the
        // user gets a card in the corner.
        for reason in &dropped {
            let note = format!("\n【附件没有发出去：{reason}】");
            text.push_str(&note);
            staged.push_str(&note);
        }
        osd_notice(&format!("附件没有发出去：{}", dropped.join("；")));
    }
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
    let mode = current_permission_mode(&store);

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
                let fresh = store.ante.resumable_archive_of(&id).is_none();
                attach_session(&store, &ops, &id, mode, &input.id).await;
                if fresh {
                    // The real server announces the title once; Ante titles from
                    // the first message too, so mirror it here.
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
                let update = SessionUpdate { permission_mode: Some(mode), ..Default::default() };
                let _ = ops.send(ante_sdk::protocol::op_msg(Op::UpdateSession(update))).await;
                if let Err(err) = ops.send(ante_sdk::protocol::op_msg(Op::UserInput(staged.clone()))).await {
                    log_line(&format!("ante: 消息发送失败：{err}"));
                }
            }
            if let Ok(mut slot) = store.ante.last_user.lock() {
                *slot = text.clone();
            }
        }
        None => {
            // 与 Ante 没连上。这条消息不进 Ante，但也不丢：它留在队列里，重连
            // 之后由 `flush_after_reconnect` 交出去。同时把断线摆到界面上——
            // 只写日志的话，用户看到的是一条永远 Pending 的消息。
            let reason = store.ante.link().reason();
            store.set_delivery(&id, &user_id, "queue");
            store.publish_durable(
                "session.inbox.delivery.changed",
                json!({ "sessionID": id, "inboxID": user_id, "delivery": "queue" }),
                &id,
            );
            log_line(&format!("ante: 与 Ante 未连接（{reason}），这条消息排进队列等重连"));
            announce_offline(&store, &id, &reason, None);
        }
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
        .route("/api/skill", get(skills))
        .route("/api/command", get(empty_reads))
        .route("/api/mcp", get(empty_reads))
        .route("/api/experimental/migration/v1", get(migration))
        .route("/api/experimental/capabilities", get(empty_reads))
        .route("/api/experimental/session/stats", get(session_stats))
        .route("/api/experimental/session/{id}/export", get(session_export))
        .route("/api/session", get(sessions_list).post(session_create))
        .route("/api/session/active", get(active_session))
        .route("/api/session/{id}", get(session_get).delete(session_delete).patch(session_update))
        .route("/api/session/{id}/move", post(session_move))
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
        .route("/api/session/{id}/form", get(session_forms))
        .route("/api/session/{id}/form/{form_id}", delete(session_form_cancel))
        .route("/api/session/{id}/form/{form_id}/reply", post(session_form_reply))
        .route("/api/session/{id}/model", post(session_model))
        .route("/api/session/{id}/compact", post(session_compact))
        .route("/api/session/{id}/agent", post(session_agent))
        .route("/api/session/{id}/view", post(no_content))
        .route("/api/event", get(events))
        .fallback(fallback)
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
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
        // A manual client misses the override the one-command mode adds, so hand
        // it over as a paste-ready prefix when the user has no opinion of their own.
        if let Some(config) = client_config_from_disk() {
            println!("（贴图预览、标签栏、以及做不了的那些入口与键位，都在这一份里：在前面加上 OPENCODE_CLI_CONFIG_CONTENT='{config}'）");
        }
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
    let mut command = std::process::Command::new(&client);
    command
        .arg("--server")
        .arg(format!("http://127.0.0.1:{port}"))
        .arg(&directory);
    if let Some(config) = client_config_from_disk() {
        command.env("OPENCODE_CLI_CONFIG_CONTENT", config);
    }
    match command.status() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_the_client_draws_get_its_name() {
        assert_eq!(client_tool_name("Bash"), "shell");
        assert_eq!(client_tool_name("Agent"), "subagent");
        assert_eq!(client_tool_name("AskUser"), "question");
        // No counterpart on the client side: Ante's own name, generic cell.
        assert_eq!(client_tool_name("Edit"), "Edit");
        assert_eq!(client_tool_name("TodoWrite"), "TodoWrite");
    }

    #[test]
    fn read_and_write_gain_the_path_key_the_client_reads() {
        let read = client_tool_args("Read", &json!({ "file_path": "/tmp/a", "limit": 10 }));
        assert_eq!(read["path"], json!("/tmp/a"));
        assert_eq!(read["limit"], json!(10));
        // Keys the two sides already agree on stay as they are.
        let grep = client_tool_args("Grep", &json!({ "pattern": "x", "path": "/tmp" }));
        assert_eq!(grep["path"], json!("/tmp"));
        assert_eq!(grep.get("file_path"), None);
    }

    #[test]
    fn reconnect_backoff_grows_then_holds() {
        // The first retry is quick — a host restarted by hand comes back at once.
        assert_eq!(reconnect_delay(1), 1.0);
        let delays: Vec<f32> = (1..=8).map(reconnect_delay).collect();
        for pair in delays.windows(2) {
            assert!(pair[1] >= pair[0], "backoff must not shrink: {delays:?}");
        }
        assert_eq!(delays.last(), Some(&30.0), "and it must stop growing: {delays:?}");
    }

    #[test]
    fn a_failing_link_reports_one_outage_not_one_per_retry() {
        let ante = Ante::new();
        assert!(!ante.link().json()["connected"].as_bool().unwrap());
        assert_eq!(ante.link().json()["state"], json!("connecting"));

        ante.link_down("第一次".to_string(), 1);
        let first = ante.link().json()["attempt"].clone();
        assert_eq!(first, json!(1));

        ante.link_up();
        assert_eq!(ante.link().json()["state"], json!("up"));
        assert_eq!(ante.link().json()["connected"], json!(true));

        ante.link_down("第二次".to_string(), 3);
        assert_eq!(ante.link().json()["reason"], json!("第二次"));
        assert_eq!(ante.link().json()["attempt"], json!(3));

        // 同一次断线里的重试不重置起点，否则「断了多久」永远只有几秒。
        let since = ante.link().json()["since"].clone();
        ante.link_down("第二次，又试了一遍".to_string(), 4);
        assert_eq!(ante.link().json()["since"], since);
    }

    #[test]
    fn the_offline_row_is_throttled() {
        let ante = Ante::new();
        let floor = std::time::Duration::from_secs(20);
        assert!(ante.notice_due(floor), "第一条要放行");
        assert!(!ante.notice_due(floor), "紧接着的第二条要被挡住");
        // 门槛为零就等于不限流，测试和调试要的就是这个。
        assert!(ante.notice_due(std::time::Duration::ZERO));
    }

    #[test]
    fn ask_user_becomes_a_form_with_its_options() {
        // The shape Ante sends, verbatim.
        let args = json!({ "questions": [
            { "header": "发版", "question": "这两个改动要现在发版装上吗？", "options": [
                { "label": "发，走 CI（推荐）", "description": "改 pkgver → 提交 → tag → 推" },
                { "label": "先不发，攒着" },
            ]},
            { "header": "Checks", "question": "Pick previews", "multiple": true, "options": [
                { "label": "Diff" }, { "label": "Subagent" },
            ]},
        ]});
        let form = askuser_form(&args, "ses_1", "msg_1", "call_9").unwrap();
        assert_eq!(form["id"], json!("frm_call_9"));
        assert_eq!(form["sessionID"], json!("ses_1"));
        assert_eq!(form["metadata"]["kind"], json!("question"));
        assert_eq!(form["metadata"]["tool"]["id"], json!("call_9"));
        assert_eq!(form["metadata"]["tool"]["messageID"], json!("msg_1"));
        let fields = form["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 2);
        // One pick is a `string` carrying options — the client has no enum field.
        assert_eq!(fields[0]["type"], json!("string"));
        assert_eq!(fields[0]["title"], json!("发版"));
        assert_eq!(fields[0]["description"], json!("这两个改动要现在发版装上吗？"));
        // The label is also the value: the answer travels back as the words the
        // user saw.
        assert_eq!(fields[0]["options"][0]["value"], json!("发，走 CI（推荐）"));
        assert_eq!(fields[0]["options"][1]["description"], Value::Null);
        // A multi pick is a `multiselect`, and free text stays possible either
        // way: these are options, not the last word.
        assert_eq!(fields[1]["type"], json!("multiselect"));
        assert_eq!(fields[1]["options"][1]["value"], json!("Subagent"));
        assert_eq!(fields[0]["custom"], json!(true));
        // Nothing to ask is not a form.
        assert!(askuser_form(&json!({ "questions": [] }), "s", "m", "c").is_none());
        assert!(askuser_form(&json!({}), "s", "m", "c").is_none());
    }

    #[test]
    fn the_answer_reads_back_as_the_user_would_say_it() {
        let form = json!({ "fields": [
            { "key": "q0", "title": "发版" },
            { "key": "q1", "title": "Checks" },
        ]});
        let answer = json!({ "q0": "先不发，攒着", "q1": ["Diff", "Subagent"] });
        assert_eq!(
            question_answer_text(&form, &answer),
            "回答提问：\n- 发版：先不发，攒着\n- Checks：Diff, Subagent"
        );
        // A question left blank is not an answer, and a fully blank form is still
        // worth saying out loud — "nothing" is not something to guess at.
        assert!(question_answer_text(&form, &json!({ "q0": "" })).contains("跳过"));
        assert_eq!(question_answer_text(&form, &json!({})), "（用户跳过了这次提问，没有作答）");
    }

    #[test]
    fn a_form_event_carries_the_location_the_client_routes_by() {
        // Without it the client reads the frame and throws it away — no error, no
        // form. Both the event and the form itself need it: the client's
        // `removeForm` keeps a form that carries none, so a reply would never
        // clear the prompt.
        let store = Store::new();
        let mut feed = store.events.subscribe();
        let mut form = json!({ "id": "frm_1", "sessionID": "ses_1", "fields": [] });
        form["location"] = loc_plain();
        store.publish_located("form.created", json!({ "form": form }));
        let event = feed.try_recv().expect("the frame reaches the feed");
        assert_eq!(event["type"], json!("form.created"));
        assert_eq!(event["location"]["directory"], json!(default_directory()));
        assert_eq!(event["data"]["form"]["location"]["directory"], json!(default_directory()));
        assert!(event.get("durable").is_none(), "form events are not durable");

        // Everything else stays location-free on purpose: the client's catalog
        // branch is behind that gate too, and it must not start firing.
        store.publish("message.updated", json!({ "sessionID": "ses_1" }));
        assert!(feed.try_recv().expect("the second frame").get("location").is_none());
    }

    #[test]
    fn the_client_override_fills_in_only_what_the_file_leaves_unsaid() {
        // Injected keys are merged *over* `cli.json`, so a deliberate choice in
        // the file has to win — otherwise the fix would be as bad as the bug.
        let filled = client_config_override(Some("{}"), false).expect("an empty file gets both");
        let value: Value = serde_json::from_str(&filled).expect("valid JSON");
        assert_eq!(value["tabs"]["mode"], json!("off"));
        assert_eq!(value["session"]["image_preview"], json!(true));
        assert_eq!(value["prompt"]["image_preview"], json!(true));

        // An unreadable or JSONC file is no opinion at all: same two defaults.
        assert!(client_config_override(None, false).is_some());
        assert!(client_config_override(Some("{ // comment\n}"), false).is_some());

        // Every key can be spoken for, and then there is nothing to inject.
        let mine = r#"{"tabs":{"mode":"on"},"session":{"image_preview":false},"prompt":{"image_preview":false},
            "plugins":["-opencode.diffs","-opencode.plugins","-opencode.btw","-opencode.sidebar.mcp"],
            "keybinds":{"session.undo":"none","session.redo":"none","session.background":"none",
                        "terminal.toggle":"none","terminal.select":"none","terminal.close":"none"}}"#;
        assert_eq!(client_config_override(Some(mine), false), None);

        // …and one key being theirs does not silence the others.
        let partial = client_config_override(Some(r#"{"tabs":{"mode":"on"}}"#), false)
            .expect("the preview keys are still unsaid");
        let value: Value = serde_json::from_str(&partial).expect("valid JSON");
        assert!(value.get("tabs").is_none(), "an explicit mode is left alone");
        assert_eq!(value["session"]["image_preview"], json!(true));

        assert_eq!(client_config_override(Some("{}"), true), None, "the variable itself wins");
    }

    #[test]
    fn the_config_path_follows_the_clients_own_lookup() {
        let env = |pairs: Vec<(&'static str, &'static str)>| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| std::ffi::OsString::from(*value))
            }
        };
        let path = |path: Option<std::path::PathBuf>| path.map(|path| path.to_string_lossy().into_owned());

        // The variable names the directory itself; the fallbacks name its parent.
        assert_eq!(
            path(client_config_path(env(vec![("OPENCODE_CONFIG_DIR", "/cfg")]))),
            Some("/cfg/cli.json".to_string())
        );
        assert_eq!(
            path(client_config_path(env(vec![("XDG_CONFIG_HOME", "/xdg")]))),
            Some("/xdg/opencode/cli.json".to_string())
        );
        assert_eq!(
            path(client_config_path(env(vec![("HOME", "/home/u")]))),
            Some("/home/u/.config/opencode/cli.json".to_string())
        );
        assert_eq!(path(client_config_path(env(vec![]))), None);
        assert_eq!(
            path(client_config_path(env(vec![("OPENCODE_CONFIG_DIR", "/cfg"), ("HOME", "/home/u")]))),
            Some("/cfg/cli.json".to_string()),
            "the most specific one wins"
        );
    }

    #[test]
    fn the_plugin_list_keeps_the_files_own_entries_and_gains_the_disables() {
        // An array is replaced, not merged, so the file's plugin has to be
        // carried through by hand or the injection would be what breaks it.
        let kept = client_config_override(Some(r#"{"plugins":["./herdr-opencode"]}"#), false)
            .expect("the disables are missing");
        let value: Value = serde_json::from_str(&kept).expect("valid JSON");
        let plugins: Vec<&str> = value["plugins"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|entry| entry.as_str().expect("a string"))
            .collect();
        assert_eq!(plugins[0], "./herdr-opencode");
        for entry in ["-opencode.diffs", "-opencode.sidebar.mcp", "-opencode.btw"] {
            assert!(plugins.contains(&entry), "{entry} is pinned");
        }

        // A file that already says them is left alone: no duplicates, no key.
        let mine = r#"{"plugins":["-opencode.diffs","-opencode.plugins","-opencode.btw","-opencode.sidebar.mcp"]}"#;
        let filled = client_config_override(Some(mine), false).expect("the keybinds are still unsaid");
        let value: Value = serde_json::from_str(&filled).expect("valid JSON");
        assert!(value.get("plugins").is_none(), "nothing to add means nothing to inject");
    }

    #[test]
    fn the_dead_keybinds_are_switched_off_unless_the_file_binds_them() {
        let filled = client_config_override(Some("{}"), false).expect("both layers");
        let value: Value = serde_json::from_str(&filled).expect("valid JSON");
        assert_eq!(value["keybinds"]["terminal.toggle"], json!("none"));
        assert_eq!(value["keybinds"]["session.background"], json!("none"));

        // A deliberate bind in the file wins, and the rest are still filled in.
        let filled = client_config_override(Some(r#"{"keybinds":{"terminal.toggle":"<leader>t"}}"#), false)
            .expect("the other keys are unsaid");
        let value: Value = serde_json::from_str(&filled).expect("valid JSON");
        assert!(value["keybinds"].get("terminal.toggle").is_none());
        assert_eq!(value["keybinds"]["terminal.select"], json!("none"));
    }

    #[test]
    fn the_commands_that_got_an_endpoint_are_no_longer_hidden() {
        // `/stats` was dark only because its plugin was switched off, `/export`
        // because its key was; `/rename` while `session.update` answered 200 with
        // nothing. All three are served now, so the layers that hid them must not
        // come back — this is the regression that would silently undo the work.
        assert!(!DISABLED_PLUGINS.contains(&"-opencode.stats"));
        assert!(!DEAD_KEYBINDS.contains(&"session.export"));
        assert!(!DEAD_KEYBINDS.contains(&"session.rename"));
    }

    #[test]
    fn the_stats_fold_counts_only_what_ante_wrote_down() {
        let row = |created: i64, prompts: u64, messages: u64| SessionRow {
            created,
            prompts,
            meta: json!({
                "provider": "example",
                "model": "example-model",
                "message_count": messages,
                "usage": { "input_tokens": 10, "output_tokens": 4,
                           "cache_read_tokens": 2, "cache_creation_tokens": 1 },
            }),
        };
        let day = 86_400_000i64;
        let rows = vec![row(day * 10, 2, 3), row(day * 11, 1, 5), row(day * 20, 9, 9)];
        let stats = stats_from_rows(&rows, Some(day * 10), day * 12);
        assert_eq!(stats["sessions"], json!(2), "the third row is out of range");
        assert_eq!(stats["prompts"], json!(3));
        assert_eq!(stats["steps"], json!(8));
        assert_eq!(stats["tokens"]["input"], json!(20));
        assert_eq!(stats["tokens"]["cache"]["write"], json!(2));
        assert_eq!(stats["activeDays"], json!(2));
        assert_eq!(stats["streak"], json!(2), "two days in a row");
        assert_eq!(stats["range"]["from"], json!(day * 10));
        assert_eq!(stats["models"][0]["model"]["id"], json!("example-model"));
        assert_eq!(stats["models"][0]["steps"], json!(8));
        // Ante has no prices and no subagent tally, so those read as zero.
        assert_eq!(stats["cost"], json!(0));
        assert_eq!(stats["subagents"], json!(0));
        assert_eq!(stats["tools"], json!({ "mode": "none" }));

        // An empty range still answers the shape the client validates.
        let empty = stats_from_rows(&[], None, day);
        assert_eq!(empty["sessions"], json!(0));
        assert_eq!(empty["range"]["from"], json!(day), "no sessions, so now");
        assert_eq!(empty["activity"], json!([]));
    }

    #[test]
    fn an_announced_skill_becomes_the_shape_the_client_reads() {
        let skill = SkillMetadata {
            name: "no-such-skill-on-disk".into(),
            description: Some("一句话说明".into()),
            scope: ante_sdk::protocol::Scope::User,
            argument_hint: None,
        };
        let info = skill_info(&skill);
        assert_eq!(info["id"], json!("no-such-skill-on-disk"));
        assert_eq!(info["name"], json!("no-such-skill-on-disk"));
        assert_eq!(info["description"], json!("一句话说明"));
        assert_eq!(info["autoinvoke"], json!(true));
        assert_eq!(info["path"], json!(""), "no file on disk, so nothing to point at");

        // A skill with no description leaves the key out rather than sending null.
        let bare = SkillMetadata { description: None, ..skill };
        assert!(skill_info(&bare).get("description").is_none());
    }

    #[test]
    fn skill_frontmatter_is_split_from_the_body() {
        let text = "---\nname: writing-docs\ndescription: \"记下来\"\n---\n\n# 正文\n";
        let (head, body) = split_frontmatter(text);
        assert_eq!(frontmatter_field(&head, "name").as_deref(), Some("writing-docs"));
        assert_eq!(frontmatter_field(&head, "description").as_deref(), Some("记下来"));
        assert!(body.starts_with("# 正文"));

        // No frontmatter at all: the whole file is the body.
        let (head, body) = split_frontmatter("# 没有 frontmatter\n");
        assert!(head.is_empty());
        assert!(body.starts_with("# 没有"));
    }

    #[test]
    fn a_session_with_no_name_at_all_still_has_one() {
        // The title chain ends here: a rename that clears the title falls back to
        // the derived one, and a session with nothing to derive from gets this.
        assert_eq!(derived_title("no-such-session-dir"), "untitled");
        assert_eq!(title_override("no-such-session-dir"), None);
    }
}
