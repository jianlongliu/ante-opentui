//! `/connect`: Ante's provider sign-in, served as opencode integrations.
//!
//! The client's `/connect` dialog is built from `GET /api/integration`: a list
//! of *integrations* (services that want credentials), each with the methods
//! that can supply them and the connections already held. Ante has the same
//! shape underneath — catalog providers declaring an `env_key` or an
//! `oauth_preset`, credentials under `auth/` — so this module maps one onto the
//! other rather than stubbing the endpoints.
//!
//! Signing in is Ante's work wherever Ante exposes it (`ante auth login antix`)
//! and this module's where it does not: the ChatGPT subscription preset is the
//! Codex OAuth flow (client id, redirect and `codex_cli_simplified_flow` are
//! read off Ante's own binary), whose result is written to `auth/openai.json` in
//! exactly the shape Ante writes. Everything listed here comes from
//! `ante catalog` — this module invents no providers.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path as FsPath, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use axum::{
    Json,
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};

/// The ChatGPT preset's OAuth app, as Ante's binary spells it: the same
/// registered client the Codex CLI uses, so the redirect is registered to that
/// client and the callback port is not ours to choose.
const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_ISSUER: &str = "https://auth.openai.com";
const OPENAI_REDIRECT: &str = "http://localhost:1455/auth/callback";
const OPENAI_REDIRECT_ENCODED: &str = "http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback";
const OPENAI_CALLBACK_PORT: u16 = 1455;
const OPENAI_SCOPE: &str = "openid%20profile%20email%20offline_access";

/// The issuer is a constant in production, and overridable for the same reason
/// Ante lets `ANTIX_API_URL` be: it is the only way to drive the whole sign-in
/// against a local fake and see what actually gets written, without a real
/// account. Nothing else about the flow changes.
fn openai_issuer() -> String {
    crate::env_or("ANTEX_OPENAI_ISSUER", OPENAI_ISSUER)
}

/// How long a started sign-in stays pollable, and how long the browser flow may
/// take. Ante's own callback server gives up after the same five minutes.
const ATTEMPT_TTL: Duration = Duration::from_secs(300);

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "message": message.into() }))).into_response()
}

/// Who opens the sign-in URL: Linux has `xdg-open`, macOS has `open`. Only the
/// browser launch differs — everything else in the flow is the same on both.
fn browser_opener() -> &'static str {
    if cfg!(target_os = "macos") { "open" } else { "xdg-open" }
}

// ----------------------------------------------------------------- providers --

#[derive(Clone)]
struct Provider {
    id: String,
    name: String,
    env_key: Option<String>,
    preset: Option<String>,
}

/// Ante's *merged* catalog: the user's `catalog.json` only adds to the bundled
/// one, so reading that file on disk would miss every built-in provider. The
/// binary is the only thing that prints the merge.
pub async fn merged_catalog() -> Vec<Value> {
    let bin = crate::ante_executable().unwrap_or_else(|| PathBuf::from("ante"));
    let Ok(out) = tokio::process::Command::new(bin).arg("catalog").output().await else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    serde_json::from_slice::<Value>(&out.stdout)
        .ok()
        .and_then(|value| value.get("providers").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// `auth` is written as one of `bearer` / `header` / `query`; the credential
/// kind never changes what we do with it.
fn auth_of(spec: &Value) -> (Option<String>, Option<String>) {
    let Some(auth) = spec.get("auth") else {
        return (None, None);
    };
    for kind in ["bearer", "header", "query"] {
        let Some(node) = auth.get(kind) else { continue };
        let env = node.get("env_key").and_then(Value::as_str).map(str::to_string);
        let preset = node.get("oauth_preset").and_then(Value::as_str).map(str::to_string);
        if env.is_some() || preset.is_some() {
            return (env, preset);
        }
    }
    (None, None)
}

async fn providers() -> Vec<Provider> {
    merged_catalog()
        .await
        .iter()
        .filter_map(|spec| {
            let id = spec.get("id").and_then(Value::as_str)?.to_string();
            let name = spec
                .get("display_name")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string();
            let (env_key, preset) = auth_of(spec);
            // No credential of either kind: nothing for `/connect` to offer.
            if env_key.is_none() && preset.is_none() {
                return None;
            }
            Some(Provider { id, name, env_key, preset })
        })
        .collect()
}

/// Whether one catalog entry's credentials are present right now: an env key, a
/// key in `auth/api_keys.json`, or an OAuth credential under `auth/`. An entry
/// that asks for nothing at all counts as reachable.
///
/// This is the bare rule, with no exceptions — [`connected_providers`] adds one
/// on top of it, and [`credentialed_ids`] deliberately does not.
fn has_credentials(spec: &Value, keys: &Map<String, Value>, presets: &[String]) -> bool {
    let (env_key, preset) = auth_of(spec);
    match (env_key, preset) {
        (None, None) => true,
        (Some(env), _) if std::env::var_os(&env).is_some() || keys.contains_key(&env) => true,
        (_, Some(preset)) => presets.iter().any(|held| held == &preset),
        _ => false,
    }
}

/// The providers Ante can actually reach right now: an env key exported in this
/// process, a key pasted into `auth/api_keys.json`, an OAuth credential under
/// `auth/`, or a provider that asks for nothing at all. The model picker is fed
/// from this — the merged catalog is everything Ante *could* run, and offering
/// a provider whose credentials are missing only produces a failed first turn.
/// The provider Ante is configured on stays in either way, so the pair the
/// composer opens with never disappears.
pub async fn connected_providers() -> Vec<Value> {
    let keys = read_api_keys();
    let presets = stored_presets();
    let active = crate::settings().get("provider").and_then(Value::as_str).map(str::to_string);
    merged_catalog()
        .await
        .into_iter()
        .filter(|spec| {
            let id = spec.get("id").and_then(Value::as_str);
            if active.as_deref().is_some() && active.as_deref() == id {
                return true;
            }
            has_credentials(spec, &keys, &presets)
        })
        .collect()
}

/// The ids whose credentials are actually present — [`has_credentials`] with no
/// exceptions, so the configured provider is *not* grandfathered in.
///
/// The quota panel gates on this: the picker's exception exists so the composer
/// never loses the pair it opens with, but a panel that showed a block for a
/// provider whose key was deleted would be reporting a subscription that is not
/// there.
pub async fn credentialed_ids() -> std::collections::HashSet<String> {
    let keys = read_api_keys();
    let presets = stored_presets();
    merged_catalog()
        .await
        .into_iter()
        .filter(|spec| has_credentials(spec, &keys, &presets))
        .filter_map(|spec| Some(spec.get("id")?.as_str()?.to_string()))
        .collect()
}

async fn provider(id: &str) -> Option<Provider> {
    providers().await.into_iter().find(|p| p.id == id)
}

// -------------------------------------------------------------- Ante's auth --

fn auth_dir() -> PathBuf {
    crate::ante_home().join("auth")
}

fn api_keys_path() -> PathBuf {
    auth_dir().join("api_keys.json")
}

/// Ante names an OAuth credential after the *preset* (`antix.json`,
/// `openai.json`), not after the provider entry that uses it.
fn oauth_path(preset: &str) -> PathBuf {
    auth_dir().join(format!("{preset}.json"))
}

fn read_api_keys() -> Map<String, Value> {
    std::fs::read_to_string(api_keys_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn stored_presets() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(auth_dir()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".json")?.to_string();
            (stem != "api_keys" && entry.path().is_file()).then_some(stem)
        })
        .collect()
}

/// Keep one copy of a credential file from before the first write we make to
/// it: this is the user's own sign-in, and a `.bak` is the cheapest way back.
fn backup_once(path: &FsPath) {
    if !path.exists() {
        return;
    }
    let mut raw = path.as_os_str().to_owned();
    raw.push(".bak");
    let backup = PathBuf::from(raw);
    if !backup.exists() {
        let _ = std::fs::copy(path, &backup);
    }
}

/// Owner-only, like Ante writes its own: a temp file plus a rename, so no
/// reader ever sees a half-written store.
fn write_private(path: &FsPath, body: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    backup_once(path);
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, body).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("cannot set permissions: {e}"))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot move into place {}: {e}", path.display()))
}

// --------------------------------------------------------- integration shape --

fn oauth_label(preset: &str) -> String {
    match preset {
        "openai" => "Sign in with ChatGPT".to_string(),
        "antix" => "Sign in with Antix".to_string(),
        "anthropic" => "Sign in with Claude".to_string(),
        other => format!("Sign in with {other}"),
    }
}

fn integration_json(p: &Provider, keys: &Map<String, Value>, presets: &[String]) -> Value {
    let mut methods = Vec::new();
    let mut connections = Vec::new();
    if let Some(preset) = &p.preset {
        methods.push(json!({ "type": "oauth", "id": "oauth", "label": oauth_label(preset) }));
        if presets.iter().any(|held| held == preset) {
            connections.push(json!({
                "type": "credential", "id": p.id, "label": p.name, "method": "oauth",
            }));
        }
    }
    if let Some(env) = &p.env_key {
        methods.push(json!({ "type": "key", "label": format!("Paste {env}") }));
        methods.push(json!({ "type": "env", "names": [env] }));
        if std::env::var_os(env).is_some() {
            connections.push(json!({ "type": "env", "name": env }));
        }
        if keys.contains_key(env) {
            connections.push(json!({
                "type": "credential", "id": p.id, "method": "key",
                "label": format!("{env} (from auth/api_keys.json)"),
            }));
        }
    }
    json!({ "id": p.id, "name": p.name, "methods": methods, "connections": connections })
}

pub async fn list() -> Json<Value> {
    let keys = read_api_keys();
    let presets = stored_presets();
    let data: Vec<Value> = providers()
        .await
        .iter()
        .map(|p| integration_json(p, &keys, &presets))
        .collect();
    crate::envelope(json!(data))
}

pub async fn get(Path(id): Path<String>) -> Response {
    let keys = read_api_keys();
    let presets = stored_presets();
    match providers().await.iter().find(|p| p.id == id) {
        Some(p) => crate::envelope(integration_json(p, &keys, &presets)).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("Ante's catalog has no {id}")),
    }
}

// ------------------------------------------------------------- connecting --

/// A pasted key goes into the same file Ante's own `/welcome` writes, under the
/// provider's `env_key`, so both front-ends see one store.
pub async fn connect_key(Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let Some(p) = provider(&id).await else {
        return error(StatusCode::NOT_FOUND, format!("Ante's catalog has no {id}"));
    };
    let Some(env) = p.env_key.clone() else {
        return error(
            StatusCode::BAD_REQUEST,
            format!("{} is not a provider that signs in with an API key (its catalog entry has no env_key)", p.name),
        );
    };
    let key = body.get("key").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if key.is_empty() {
        return error(StatusCode::BAD_REQUEST, "the key is empty");
    }
    let mut keys = read_api_keys();
    keys.insert(env, Value::String(key));
    match write_private(&api_keys_path(), &Value::Object(keys).to_string()) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}

// The TUI reads connections off `/api/integration`; this is the same store in
// the shape other clients ask for. Secrets are left out — nothing in the shim
// needs to hand a token back out over HTTP.
pub async fn credentials() -> Json<Value> {
    let keys = read_api_keys();
    let mut data = Vec::new();
    for p in providers().await {
        if let Some(env) = &p.env_key {
            if keys.contains_key(env) {
                data.push(json!({
                    "id": p.id, "integrationID": p.id, "label": env, "active": false,
                    "value": { "type": "key", "key": "" },
                }));
            }
        }
        if let Some(preset) = &p.preset {
            if oauth_path(preset).is_file() {
                data.push(json!({
                    "id": p.id, "integrationID": p.id, "label": p.name, "active": false,
                    "value": { "type": "oauth", "methodID": "oauth", "refresh": "", "access": "", "expires": 0 },
                }));
            }
        }
    }
    Json(json!({ "data": data }))
}

/// Deleting a connection deletes the credential itself, in Ante's own store —
/// the file for an OAuth preset, the entry for a saved key.
pub async fn credential_delete(Path(id): Path<String>) -> Response {
    let Some(p) = provider(&id).await else {
        return error(StatusCode::NOT_FOUND, format!("Ante's catalog has no {id}"));
    };
    let mut removed = false;
    if let Some(preset) = &p.preset {
        let path = oauth_path(preset);
        if path.is_file() {
            backup_once(&path);
            if let Err(e) = std::fs::remove_file(&path) {
                return error(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot delete {}: {e}", path.display()));
            }
            removed = true;
        }
    }
    if let Some(env) = &p.env_key {
        let mut keys = read_api_keys();
        if keys.remove(env).is_some() {
            if let Err(message) = write_private(&api_keys_path(), &Value::Object(keys).to_string()) {
                return error(StatusCode::INTERNAL_SERVER_ERROR, message);
            }
            removed = true;
        }
    }
    if removed {
        StatusCode::NO_CONTENT.into_response()
    } else {
        error(StatusCode::NOT_FOUND, format!("no stored credential for {}", p.name))
    }
}

/// Ante keeps one credential per provider, so there is nothing to switch
/// between — but the client calls this when a connection is clicked.
pub async fn credential_activate(Path(_id): Path<String>) -> Response {
    StatusCode::NO_CONTENT.into_response()
}

pub async fn credential_update(Path(_id): Path<String>, Json(_body): Json<Value>) -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "Ante's credentials have no editable label: they are named after the provider (auth/<preset>.json, or the env name in auth/api_keys.json)",
    )
}

// ---------------------------------------------------------------- sign-in --

type CancelFn = Box<dyn FnOnce() + Send>;

enum AttemptState {
    Pending,
    Done,
    Failed(String),
}

struct Attempt {
    state: AttemptState,
    created_ms: i64,
    cancel: Option<CancelFn>,
}

fn attempts() -> &'static Mutex<HashMap<String, Attempt>> {
    static ATTEMPTS: std::sync::OnceLock<Mutex<HashMap<String, Attempt>>> = std::sync::OnceLock::new();
    ATTEMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn set_state(id: &str, state: AttemptState) {
    let mut map = attempts().lock().await;
    if let Some(attempt) = map.get_mut(id) {
        attempt.state = state;
        attempt.cancel = None;
    }
}

fn attempt_deadline(created_ms: i64) -> i64 {
    created_ms + ATTEMPT_TTL.as_millis() as i64
}

fn status_json(attempt: &Attempt) -> Value {
    let time = json!({ "created": attempt.created_ms, "expires": attempt_deadline(attempt.created_ms) });
    match &attempt.state {
        AttemptState::Pending => json!({ "status": "pending", "time": time }),
        AttemptState::Done => json!({ "status": "complete", "time": time }),
        AttemptState::Failed(message) => json!({ "status": "failed", "message": message, "time": time }),
    }
}

pub async fn connect_oauth(Path(id): Path<String>, Json(_body): Json<Value>) -> Response {
    let Some(p) = provider(&id).await else {
        return error(StatusCode::NOT_FOUND, format!("Ante's catalog has no {id}"));
    };
    let Some(preset) = p.preset.clone() else {
        return error(StatusCode::BAD_REQUEST, format!("{} is not a provider that signs in with OAuth", p.name));
    };
    let attempt_id = format!("att_{}", token(9));
    let created_ms = crate::now_ms();
    {
        // One lock for both: finished attempts only linger so a late poll can
        // still read their outcome.
        let mut map = attempts().lock().await;
        map.retain(|_, held| held.created_ms + 600_000 > created_ms);
        map.insert(
            attempt_id.clone(),
            Attempt { state: AttemptState::Pending, created_ms, cancel: None },
        );
    }
    let started = match preset.as_str() {
        "openai" => start_openai(&attempt_id).await,
        "antix" => start_antix(&attempt_id).await,
        other => Err(format!(
            "antex has no OAuth for {other} yet. Of the three Ante can sign in, ChatGPT is handled here and Antix goes through `ante auth login`; {other} still has to be signed in from Ante's own TUI."
        )),
    };
    match started {
        Ok((url, instructions, cancel)) => {
            if let Some(attempt) = attempts().lock().await.get_mut(&attempt_id) {
                attempt.cancel = Some(cancel);
            }
            crate::envelope(json!({
                "attemptID": attempt_id,
                "url": url,
                "instructions": instructions,
                "mode": "auto",
                "time": { "created": created_ms, "expires": attempt_deadline(created_ms) },
            }))
            .into_response()
        }
        Err(message) => {
            attempts().lock().await.remove(&attempt_id);
            error(StatusCode::BAD_REQUEST, message)
        }
    }
}

pub async fn oauth_status(Path((_integration, attempt_id)): Path<(String, String)>) -> Response {
    let map = attempts().lock().await;
    match map.get(&attempt_id) {
        Some(attempt) => crate::envelope(status_json(attempt)).into_response(),
        None => error(StatusCode::NOT_FOUND, "no such sign-in attempt (it has already finished)"),
    }
}

pub async fn oauth_cancel(Path((_integration, attempt_id)): Path<(String, String)>) -> Response {
    let held = attempts().lock().await.remove(&attempt_id);
    if let Some(mut attempt) = held {
        if let Some(cancel) = attempt.cancel.take() {
            cancel();
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Every sign-in started here finishes on its own callback (`mode: auto`), so a
/// pasted code has nothing to attach to. Saying so beats pretending it worked.
pub async fn oauth_complete(Path((_integration, attempt_id)): Path<(String, String)>, Json(_body): Json<Value>) -> Response {
    if attempts().lock().await.contains_key(&attempt_id) {
        error(
            StatusCode::BAD_REQUEST,
            "this sign-in finishes on its own browser callback; there is no code to paste",
        )
    } else {
        error(StatusCode::NOT_FOUND, "no such sign-in attempt (it has already finished)")
    }
}

// ------------------------------------------------------------------ ChatGPT --

async fn start_openai(attempt_id: &str) -> Result<(String, String, CancelFn), String> {
    let verifier = token(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = token(24);
    let issuer = openai_issuer();
    let url = format!(
        "{issuer}/oauth/authorize?response_type=code&client_id={OPENAI_CLIENT_ID}\
&redirect_uri={OPENAI_REDIRECT_ENCODED}&scope={OPENAI_SCOPE}\
&code_challenge={challenge}&code_challenge_method=S256&state={state}\
&id_token_add_organizations=true&codex_cli_simplified_flow=true"
    );
    let listener = TcpListener::bind(("127.0.0.1", OPENAI_CALLBACK_PORT)).await.map_err(|e| {
        format!("callback port {OPENAI_CALLBACK_PORT} is unusable (is Ante's own /connect or the Codex CLI signing in already?): {e}")
    })?;
    let (tx_code, rx_code) = oneshot::channel::<Result<String, String>>();
    let (tx_stop, rx_stop) = oneshot::channel::<()>();
    let (tx_cancel, rx_cancel) = oneshot::channel::<()>();
    tokio::spawn(callback_server(listener, state, tx_code, rx_stop));
    // The browser is ours to open: the client only shows the URL.
    let opened = url.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let _ = tokio::process::Command::new(browser_opener()).arg(&opened).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    });
    let id = attempt_id.to_string();
    tokio::spawn(async move {
        let outcome = tokio::select! {
            landed = rx_code => match landed {
                Ok(verdict) => verdict,
                Err(_) => Err("the callback listener stopped".to_string()),
            },
            _ = rx_cancel => Err(String::new()),
            _ = tokio::time::sleep(ATTEMPT_TTL) => Err("authorization timed out (5 minutes)".to_string()),
        };
        let _ = tx_stop.send(());
        match outcome {
            Ok(code) => match exchange_openai(&code, &verifier).await.and_then(|tokens| write_openai_credential(&tokens)) {
                Ok(()) => set_state(&id, AttemptState::Done).await,
                Err(message) => set_state(&id, AttemptState::Failed(message)).await,
            },
            Err(message) if message.is_empty() => set_state(&id, AttemptState::Failed("cancelled".into())).await,
            Err(message) => set_state(&id, AttemptState::Failed(message)).await,
        }
    });
    Ok((
        url,
        format!("Finish signing in to ChatGPT in the browser. The callback lands on {OPENAI_REDIRECT}; on success the credential is written to auth/openai.json."),
        Box::new(move || {
            let _ = tx_cancel.send(());
        }),
    ))
}

/// What a request on the callback port turned out to be.
enum Callback {
    Code(String),
    /// The provider's own `error=` answer: this sign-in is over.
    Failed(String),
    /// Something that is not the redirect (a favicon, a stray probe) — or a
    /// redirect carrying the wrong `state`. Neither ends the attempt: the real
    /// callback may still be on its way.
    Stray,
}

async fn callback_server(
    listener: TcpListener,
    expected_state: String,
    tx_code: oneshot::Sender<Result<String, String>>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut sender = Some(tx_code);
    loop {
        let accepted = tokio::select! {
            _ = &mut stop => break,
            accepted = listener.accept() => accepted,
        };
        let Ok((mut socket, _)) = accepted else { break };
        let request = read_request(&mut socket).await;
        let (verdict, status, message) = match parse_callback(&request, &expected_state) {
            Callback::Code(code) => (
                Some(Ok(code)),
                200,
                "Signed in. You can close this window and go back to the terminal.".to_string(),
            ),
            Callback::Failed(message) => (
                Some(Err(message.clone())),
                400,
                message,
            ),
            Callback::Stray => (None, 404, "Nothing here.".to_string()),
        };
        let page = format!(
            "<!doctype html><meta charset=utf-8><body style='font-family:sans-serif'><p>{message}</p></body>"
        );
        let head = format!(
            "HTTP/1.1 {status} {}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            if status == 200 { "OK" } else { "Error" },
            page.len()
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(page.as_bytes()).await;
        let _ = socket.shutdown().await;
        if let Some(verdict) = verdict {
            if let Some(sender) = sender.take() {
                let _ = sender.send(verdict);
            }
            break;
        }
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 1024];
    while raw.len() < 8192 {
        match socket.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => {
                raw.extend_from_slice(&chunk[..read]);
                if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&raw).into_owned()
}

fn parse_callback(request: &str, expected_state: &str) -> Callback {
    let Some(target) = request.lines().next().and_then(|line| line.split_whitespace().nth(1)) else {
        return Callback::Stray;
    };
    let Some((path, query)) = target.split_once('?') else {
        return Callback::Stray;
    };
    if path != "/auth/callback" {
        return Callback::Stray;
    }
    let (mut code, mut state, mut failure) = (None, None, None);
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "code" => code = Some(percent_decode(value)),
            "state" => state = Some(percent_decode(value)),
            "error_description" | "error" => failure = Some(percent_decode(value)),
            _ => {}
        }
    }
    if let Some(message) = failure {
        return Callback::Failed(message);
    }
    match (code, state) {
        (Some(code), Some(state)) if state == expected_state => Callback::Code(code),
        // Not this attempt's redirect. Leave the listener up: the browser may
        // still deliver the one we are waiting for.
        _ => Callback::Stray,
    }
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn exchange_openai(code: &str, verifier: &str) -> Result<Value, String> {
    let response = reqwest::Client::new()
        .post(format!("{}/oauth/token", openai_issuer()))
        .json(&json!({
            "client_id": OPENAI_CLIENT_ID,
            "code": code,
            "code_verifier": verifier,
            "grant_type": "authorization_code",
            "redirect_uri": OPENAI_REDIRECT,
        }))
        .send()
        .await
        .map_err(|e| format!("request to {} failed: {e}", openai_issuer()))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!("token exchange rejected (HTTP {status}): {}", failure_detail(&body)));
    }
    Ok(body)
}

/// OAuth error bodies are not uniform: `error_description`, a bare `error`
/// string, or (OpenAI's newer shape) an `error` object. Falling back to the
/// body keeps a rejected sign-in explainable instead of "HTTP 401:".
fn failure_detail(body: &Value) -> String {
    if let Some(text) = body.get("error_description").and_then(Value::as_str) {
        return text.to_string();
    }
    match body.get("error") {
        Some(Value::String(text)) => return text.clone(),
        Some(Value::Object(object)) => {
            for key in ["message", "code", "type"] {
                if let Some(text) = object.get(key).and_then(Value::as_str) {
                    return text.to_string();
                }
            }
        }
        _ => {}
    }
    let raw = body.to_string();
    raw.chars().take(200).collect()
}

/// Exactly the file Ante writes for this preset: `client_id`, `token_issuer`
/// and `token_request_encoding` name how the refresh is done, and the rest is
/// the token set. Ante parses the access token for `chatgpt_account_id`, so the
/// real token matters — none of it is synthesized here.
fn write_openai_credential(tokens: &Value) -> Result<(), String> {
    let access = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("the token response has no access_token")?;
    let refresh = tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .ok_or("the token response has no refresh_token — the sign-in cannot be renewed, so Ante cannot use it")?;
    let expires_in = tokens.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);
    let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(expires_in))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let scopes: Vec<String> = tokens
        .get("scope")
        .and_then(Value::as_str)
        .map(|raw| raw.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    let credential = json!({
        "client_id": OPENAI_CLIENT_ID,
        "token_issuer": openai_issuer(),
        "token_request_encoding": "json",
        "id_token": tokens.get("id_token").cloned().unwrap_or(Value::String(String::new())),
        "access_token": access,
        "refresh_token": refresh,
        "expires_at": expires_at,
        "scopes": scopes,
    });
    write_private(&oauth_path("openai"), &credential.to_string())
}

// -------------------------------------------------------------------- Antix --

/// Antix is the one preset the CLI can sign into, so the child does the whole
/// flow (browser, callback, credential file) and we only carry its URL to the
/// dialog and its exit status to the poller.
async fn start_antix(attempt_id: &str) -> Result<(String, String, CancelFn), String> {
    let bin = crate::ante_executable().unwrap_or_else(|| PathBuf::from("ante"));
    let mut child = tokio::process::Command::new(bin)
        .args(["auth", "login", "antix"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not run `ante auth login antix`: {e}"))?;
    let stdout = child.stdout.take().ok_or("could not read the output of `ante auth login antix`")?;
    let (tx_url, rx_url) = oneshot::channel::<Result<String, String>>();
    // Draining to EOF matters: the child writes to this pipe, and a closed pipe
    // would end the sign-in before it finishes.
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut sender = Some(tx_url);
        let mut seen = false;
        while let Ok(Some(line)) = lines.next_line().await {
            if seen {
                continue;
            }
            if let Some(url) = line.trim().strip_prefix("Authorize URL:") {
                seen = true;
                if let Some(sender) = sender.take() {
                    let _ = sender.send(Ok(url.trim().to_string()));
                }
            }
        }
        if !seen {
            if let Some(sender) = sender.take() {
                let _ = sender.send(Err("`ante auth login antix` printed no authorize link".into()));
            }
        }
    });
    let url = tokio::time::timeout(Duration::from_secs(20), rx_url)
        .await
        .map_err(|_| "timed out waiting for `ante auth login antix` to print the authorize link".to_string())?
        .map_err(|_| "`ante auth login antix` exited early".to_string())??;
    let (tx_cancel, rx_cancel) = oneshot::channel::<()>();
    let id = attempt_id.to_string();
    tokio::spawn(async move {
        let outcome = tokio::select! {
            status = child.wait() => match status {
                Ok(status) if status.success() => AttemptState::Done,
                Ok(status) => AttemptState::Failed(format!("`ante auth login antix` exited with {status}: the sign-in did not complete")),
                Err(e) => AttemptState::Failed(format!("waiting for `ante auth login antix` failed: {e}")),
            },
            _ = rx_cancel => {
                let _ = child.kill().await;
                AttemptState::Failed("cancelled".into())
            }
        };
        set_state(&id, outcome).await;
    });
    Ok((
        url,
        "Finish signing in to Antix in the browser; Ante handles the callback and writes the credential itself.".to_string(),
        Box::new(move || {
            let _ = tx_cancel.send(());
        }),
    ))
}

// -------------------------------------------------------------------- odds --

/// PKCE verifiers and attempt ids: unpredictability is the point, so this reads
/// the OS pool rather than the clock.
fn token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut buf))
        .is_ok();
    if !filled {
        let mut seed = crate::now_ms() as u64 ^ (std::process::id() as u64) << 32;
        for byte in buf.iter_mut() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *byte = (seed >> 33) as u8;
        }
    }
    URL_SAFE_NO_PAD.encode(buf)
}
