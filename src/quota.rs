//! 侧栏的额度面板。
//!
//! 界面那一小块在自编客户端里（`tui/src/feature-plugins/sidebar/quota.tsx`），
//! 数据由这里出：客户端 `POST /api/rpc/antex.quota/panel` 问一次，拿回排好版的
//! 文本行。走 rpc 而不是自造一个 `/api/quota`，是因为客户端的生成式 client 里
//! 只有 `rpc.call` 是「任意 id + 任意方法」的通路 —— 打客户端补丁不必改生成代码。
//!
//! 取数一律是各家自己的接口，不依赖 opencode 的插件：
//!
//! - DeepSeek      `GET api.deepseek.com/user/balance`
//! - Command Code  `GET api.commandcode.ai/alpha/billing/credits`（差 M 窗口时再问 subscriptions）
//! - ChatGPT       `GET chatgpt.com/backend-api/wham/usage`
//! - OpenCode Go   `GET opencode.ai/zen/go/v1/usage`
//!
//! 凭据顺序：环境变量 → Ante 自己的 `auth/api_keys.json`。ChatGPT 用 `/connect`
//! 落下的 `auth/openai.json`，过期就现刷一次 —— 只用于本次请求，不写回文件（续期
//! 是 Ante 的事，两边同时写会打架）。
//!
//! 缓存：成功 5 分钟、失败 60 秒。侧栏每 60 秒问一次，而这些接口都算敏感，失败
//! 还去重试只会更快被限流。5 分钟对得上上游插件的 `minIntervalMs` 默认值。

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::{
    Json,
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{SecondsFormat, TimeZone, Utc};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

/// 面板按 36 列排版：上游插件的侧栏宽度，也是客户端侧栏的实际宽度。
const WIDTH: usize = 36;
const TIME_WIDTH: usize = 7;
const PERCENT_WIDTH: usize = 9; // "100% used"
/// 百分比行：名字 + 倒计时；值行：名字 + 值（值那一列就是百分比那一列）。
const NAME_WIDTH: usize = WIDTH - 2 - TIME_WIDTH;
const VALUE_NAME_WIDTH: usize = WIDTH - 2 - PERCENT_WIDTH;
const BAR_WIDTH: usize = WIDTH - 2 - PERCENT_WIDTH;

const CACHE_TTL: Duration = Duration::from_secs(300);
const FAILURE_TTL: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// 这些站点前面是 Cloudflare：默认 UA 会被挡成 403（code 1010），与 key 无关。
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36";

/// ChatGPT 的订阅用量与 Codex 用同一个 OAuth 应用，client id 从 Ante 二进制里读出来
/// （与 `integrations.rs` 的预设一致，续期要用同一个）。
const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Command Code 的月度额度不随 credits 接口给出，得按 planId 查表（同 omarchy 的
/// ai-subs 插件；官方没有别的说法）。
const COMMANDCODE_ALLOWANCES: &[(&str, f64)] = &[
    ("individual-go", 10.0),
    ("individual-pro", 30.0),
    ("individual-goat", 70.0),
    ("individual-pro-v1", 80.0),
    ("individual-max", 150.0),
    ("individual-ultra", 300.0),
    ("go", 10.0),
    ("pro", 30.0),
    ("goat", 70.0),
    ("max", 150.0),
    ("ultra", 300.0),
];

/// 面板的一行标题（`[名字]`，客户端会把这种行加粗）加若干数据行。
struct Block {
    header: String,
    rows: Vec<Row>,
    /// 这块到底有没有额度数据：只报到状态（没订阅、没登录、取数失败）的不算，
    /// 面板折叠时显示的「N active」数的是这个。
    available: bool,
}

enum Row {
    /// 有配额窗口的行：一条「名字 + 重置倒计时」加一条「进度条 + 百分比」。
    Window {
        label: String,
        used_percent: f64,
        reset_ms: Option<i64>,
    },
    /// 余额、状态这类单行值。
    Value { label: String, value: String },
}

/// `/api/rpc/{rpcID}/{method}`：只认自己的 id，别的一律按客户端的约定回
/// `rpc.unavailable`（客户端就知道是「这个位置没有这个 RPC」，不是网络坏了）。
pub async fn rpc(Path((rpc_id, method)): Path<(String, String)>, Json(body): Json<Value>) -> Response {
    if rpc_id != "antex.quota" || method != "panel" {
        return error(
            StatusCode::NOT_FOUND,
            "rpc.unavailable",
            format!("没有注册 {rpc_id}.{method}"),
        );
    }
    let session = body
        .get("input")
        .and_then(|input| input.get("sessionID"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let (lines, active) = panel(session.as_deref()).await;
    Json(json!({
        "output": { "data": { "lines": lines, "active": active, "updatedAt": now_ms() } }
    }))
    .into_response()
}

fn error(status: StatusCode, kind: &str, message: String) -> Response {
    (status, Json(json!({ "type": kind, "message": message }))).into_response()
}

/// 四家各出自己那一块，失败只影响自己那一块（面板照常显示其余部分）。
async fn panel(session: Option<&str>) -> (Vec<String>, usize) {
    let (deepseek, commandcode, chatgpt, opencode) = tokio::join!(
        cached("deepseek", deepseek()),
        cached("commandcode", commandcode()),
        cached("chatgpt", chatgpt()),
        cached("opencode", opencode()),
    );
    let blocks = [deepseek, commandcode, chatgpt, opencode];
    let active = blocks.iter().filter(|block| block.available).count();
    let mut lines: Vec<String> = Vec::new();
    for block in blocks.iter() {
        lines.push(block.header.clone());
        for row in &block.rows {
            lines.extend(row_lines(row));
        }
    }
    if let Some(tokens) = session_tokens(session).await {
        lines.push(String::new());
        lines.push("Session tokens".to_string());
        lines.push(tokens);
    }
    (lines, active)
}

// ------------------------------------------------------------------ 取数缓存 --

static CACHE: LazyLock<Mutex<HashMap<&'static str, (Instant, Duration, Block)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 按 provider 缓存整块，失败也缓存（短一些）——失败比成功更容易被限流。
async fn cached<F>(key: &'static str, fetch: F) -> Block
where
    F: std::future::Future<Output = Result<Block, String>>,
{
    let now = Instant::now();
    if let Some((at, ttl, block)) = CACHE.lock().await.get(key) {
        if now.duration_since(*at) < *ttl {
            return clone_block(block);
        }
    }
    let (ttl, block) = match fetch.await {
        Ok(block) => (CACHE_TTL, block),
        Err(message) => (
            FAILURE_TTL,
            Block {
                header: header(key),
                rows: vec![Row::Value {
                    label: "未取到".to_string(),
                    value: message,
                }],
                available: false,
            },
        ),
    };
    CACHE.lock().await.insert(key, (now, ttl, clone_block(&block)));
    block
}

fn clone_block(block: &Block) -> Block {
    Block {
        header: block.header.clone(),
        available: block.available,
        rows: block
            .rows
            .iter()
            .map(|row| match row {
                Row::Window {
                    label,
                    used_percent,
                    reset_ms,
                } => Row::Window {
                    label: label.clone(),
                    used_percent: *used_percent,
                    reset_ms: *reset_ms,
                },
                Row::Value { label, value } => Row::Value {
                    label: label.clone(),
                    value: value.clone(),
                },
            })
            .collect(),
    }
}

fn header(key: &str) -> String {
    match key {
        "deepseek" => "[DeepSeek]",
        "commandcode" => "[Command Code]",
        "chatgpt" => "[ChatGPT]",
        _ => "[OpenCode Go]",
    }
    .to_string()
}

// -------------------------------------------------------------------- 取数 --

async fn deepseek() -> Result<Block, String> {
    let key = credential(&["DEEPSEEK_API_KEY"]).ok_or("没有 DEEPSEEK_API_KEY")?;
    let body = get_json("https://api.deepseek.com/user/balance", &key, &[]).await?;
    let info = body
        .get("balance_infos")
        .and_then(Value::as_array)
        .and_then(|infos| infos.first())
        .ok_or("余额响应里没有 balance_infos")?;
    let currency = info.get("currency").and_then(Value::as_str).unwrap_or("CNY");
    let total = info.get("total_balance").and_then(Value::as_str).unwrap_or("0");
    Ok(Block {
        header: header("deepseek"),
        rows: vec![Row::Value {
            label: "余额".to_string(),
            value: format!("{currency} {total}"),
        }],
        available: true,
    })
}

async fn commandcode() -> Result<Block, String> {
    let key = credential(&["CMD_API_KEY", "COMMANDCODE_API_KEY"]).ok_or("没有 CMD_API_KEY")?;
    let body = get_json("https://api.commandcode.ai/alpha/billing/credits", &key, &[]).await?;
    let limits = body.get("windowLimits").and_then(Value::as_object);
    let mut rows = Vec::new();
    for (raw, label) in [("fiveHour", "5h"), ("weekly", "W"), ("monthly", "M")] {
        let Some(window) = limits.and_then(|limits| limits.get(raw)) else {
            continue;
        };
        let cap = number(window, "cap");
        let used = number(window, "used");
        if cap <= 0.0 {
            continue;
        }
        rows.push(Row::Window {
            label: label.to_string(),
            used_percent: (used / cap * 100.0).clamp(0.0, 100.0),
            reset_ms: window.get("resetAt").and_then(Value::as_i64),
        });
    }
    let remaining = body
        .get("credits")
        .map(|credits| {
            ["monthlyCredits", "purchasedCredits", "freeCredits"]
                .iter()
                .map(|field| number(credits, field))
                .sum::<f64>()
        })
        .unwrap_or(0.0);
    // 月度窗口更常见的是「剩下的钱」而不是一个 cap：按订阅档位的额度换算成百分比。
    if !rows.iter().any(|row| matches!(row, Row::Window { label, .. } if label == "M")) {
        if let Some((percent, reset_ms)) = commandcode_month_window(&key, remaining).await {
            rows.push(Row::Window {
                label: "M".to_string(),
                used_percent: percent,
                reset_ms,
            });
        }
    }
    if remaining > 0.0 {
        rows.push(Row::Value {
            label: "余额".to_string(),
            value: format!("${remaining:.2}"),
        });
    }
    if rows.is_empty() {
        return Err("接口没给任何窗口".to_string());
    }
    Ok(Block {
        header: header("commandcode"),
        rows,
        available: true,
    })
}

/// 订阅表只用来把「月度余额」还原成百分比；拿不到就只显示余额。
async fn commandcode_month_window(key: &str, remaining: f64) -> Option<(f64, Option<i64>)> {
    let body = get_json("https://api.commandcode.ai/alpha/billing/subscriptions", key, &[]).await.ok()?;
    let data = body.get("data")?;
    let status = data.get("status").and_then(Value::as_str).unwrap_or("");
    if !matches!(status, "active" | "trialing") {
        return None;
    }
    let plan = data.get("planId").and_then(Value::as_str)?.to_lowercase();
    let allowance = COMMANDCODE_ALLOWANCES
        .iter()
        .find(|(id, _)| *id == plan)
        .map(|(_, amount)| *amount)?;
    let used = (allowance - remaining).max(0.0);
    let reset = data
        .get("currentPeriodEnd")
        .and_then(Value::as_str)
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|at| at.timestamp_millis());
    Some(((used / allowance * 100.0).clamp(0.0, 100.0), reset))
}

async fn chatgpt() -> Result<Block, String> {
    let credential = openai_credential().await?;
    let account = account_id(&credential.access);
    let mut headers: Vec<(&str, &str)> = vec![("Authorization", credential.access.as_str())];
    if let Some(account) = account.as_deref() {
        headers.push(("ChatGPT-Account-Id", account));
    }
    let body = get_json("https://chatgpt.com/backend-api/wham/usage", "", &headers).await?;
    let mut rows = Vec::new();
    let rate_limit = body.get("rate_limit").and_then(Value::as_object);
    for (field, fallback) in [("primary_window", "5h"), ("secondary_window", "W")] {
        let Some(window) = rate_limit.and_then(|limit| limit.get(field)) else {
            continue;
        };
        let Some(used) = window.get("used_percent").and_then(Value::as_f64) else {
            continue;
        };
        let label = match window.get("limit_window_seconds").and_then(Value::as_i64) {
            Some(604_800) => "W",
            Some(2_628_000) => "M",
            Some(18_000) => "5h",
            _ => fallback,
        };
        rows.push(Row::Window {
            label: label.to_string(),
            used_percent: used.clamp(0.0, 100.0),
            reset_ms: window.get("reset_at").and_then(Value::as_i64).map(|at| at * 1000),
        });
    }
    if rows.is_empty() {
        return Err("额度窗口是空的（这个账号可能不共享 ChatGPT 用量）".to_string());
    }
    Ok(Block {
        header: header("chatgpt"),
        rows,
        available: true,
    })
}

struct OpenAiCredential {
    access: String,
}

/// `/connect` 写下的那份凭证：有 `access_token`/`refresh_token`/`expires_at`。
/// 过期就续一次（同一个 client id、JSON 编码），只在内存里用，不写回。
async fn openai_credential() -> Result<OpenAiCredential, String> {
    let path = crate::ante_home().join("auth").join("openai.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| "还没登录（/connect → OpenAI）".to_string())?;
    let stored: Value = serde_json::from_str(&text).map_err(|_| "auth/openai.json 读不出来".to_string())?;
    let access = stored
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or("凭证里没有 access_token")?
        .to_string();
    let expires_at = stored
        .get("expires_at")
        .and_then(Value::as_str)
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok());
    let wants_refresh = expires_at.is_some_and(|at| at.timestamp_millis() <= now_ms());
    if !wants_refresh {
        return Ok(OpenAiCredential { access });
    }
    let refresh = stored
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or("令牌已过期，且没有 refresh_token")?;
    let issuer = crate::env_or("ANTEX_OPENAI_ISSUER", "https://auth.openai.com");
    let response = http()
        .post(format!("{issuer}/oauth/token"))
        .json(&json!({
            "client_id": OPENAI_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh,
        }))
        .send()
        .await
        .map_err(|e| format!("续期请求发不出去：{e}"))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!("续期被拒（HTTP {status}），重新 /connect 一下"));
    }
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or("续期响应里没有 access_token")?
        .to_string();
    Ok(OpenAiCredential { access })
}

/// access token 里的 `chatgpt_account_id`（与 Ante 读的是同一个声明）。
fn account_id(access: &str) -> Option<String> {
    let payload = access.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

async fn opencode() -> Result<Block, String> {
    // console（网页登录）那条路能看到 Go 的 5h/周/月窗口，但它要的是 console 会话令牌；
    // 没有就退回 API key 的用量接口。
    if let Some(token) = credential(&["OPENCODE_CONSOLE_TOKEN"]) {
        let org = credential(&["OPENCODE_CONSOLE_ORG_ID"]).unwrap_or_default();
        let headers: Vec<(&str, &str)> = if org.is_empty() {
            vec![("Authorization", token.as_str())]
        } else {
            vec![("Authorization", token.as_str()), ("x-org-id", org.as_str())]
        };
        match get_json("https://opencode.ai/console/api/go/status", "", &headers).await {
            Ok(body) => return Ok(console_block(&body)),
            Err(message) => return Err(message),
        }
    }
    let key = credential(&["OPENCODE_ZEN_API_KEY"]).ok_or("没有 OPENCODE_ZEN_API_KEY")?;
    let body = match get_json("https://opencode.ai/zen/go/v1/usage", &key, &[]).await {
        Ok(body) => body,
        Err(message) => {
            // 403 EntitlementError 不是「拿不到」而是「没有订阅」，如实说清楚，
            // 免得看着像面板坏了。
            return Ok(Block {
                header: header("opencode"),
                rows: vec![Row::Value {
                    label: "状态".to_string(),
                    value: if message.contains("Entitlement") {
                        "无有效 Go 订阅".to_string()
                    } else {
                        message
                    },
                }],
                available: false,
            });
        }
    };
    let block = console_block(&body);
    if block.rows.is_empty() {
        return Err("用量接口没给窗口".to_string());
    }
    Ok(block)
}

/// console 的 `/api/go/status` 与用量接口的 meter 形状：`access.meters.{fiveHour,week,month}`
/// 记的是微分（micro-cents）。
fn console_block(body: &Value) -> Block {
    let meters = body
        .get("access")
        .and_then(|access| access.get("meters"))
        .and_then(Value::as_object);
    let mut rows = Vec::new();
    for (raw, label) in [("fiveHour", "5h"), ("week", "W"), ("month", "M")] {
        let Some(meter) = meters.and_then(|meters| meters.get(raw)) else {
            continue;
        };
        let used = number(meter, "usedMicroCents");
        let limit = number(meter, "limitMicroCents");
        if limit <= 0.0 {
            continue;
        }
        rows.push(Row::Window {
            label: label.to_string(),
            used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
            reset_ms: meter.get("resetAt").and_then(Value::as_i64),
        });
    }
    Block {
        available: !rows.is_empty(),
        header: header("opencode"),
        rows,
    }
}

// ------------------------------------------------------------------ 会话用量 --

/// 当前会话的 token 合计：Ante 自己的 `meta.json` 就有（面板要的就是这个数）。
async fn session_tokens(session: Option<&str>) -> Option<String> {
    let session = session?;
    let path = crate::ante_home().join("sessions").join(session).join("meta.json");
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let usage = meta.get("usage")?;
    let input = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output = usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
    if input == 0 && output == 0 {
        return None;
    }
    Some(format!(
        "{} in  {} out",
        tokens(input),
        tokens(output)
    ))
}

fn tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}K", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

// -------------------------------------------------------------------- 排版 --

fn row_lines(row: &Row) -> Vec<String> {
    match row {
        Row::Window {
            label,
            used_percent,
            reset_ms,
        } => {
            let percent = format!("{:.0}% used", used_percent);
            // 还有额度时不必显示重置时间（同上游：只有用掉一点才显示倒计时）。
            let time = if *used_percent > 0.0 {
                countdown(*reset_ms)
            } else {
                String::new()
            };
            let name = if time.is_empty() {
                pad_right(label, WIDTH)
            } else {
                format!(
                    "{}{}{}",
                    pad_right(label, NAME_WIDTH),
                    "  ",
                    pad_left(&time, TIME_WIDTH)
                )
            };
            vec![
                name,
                format!(
                    "{}{}{}",
                    bar(*used_percent),
                    "  ",
                    pad_left(&percent, PERCENT_WIDTH)
                ),
            ]
        }
        Row::Value { label, value } => {
            // 值窄（余额、百分比）就对齐到右侧一列；状态类长文字（「还没登录」
            // 「HTTP 403: …」）跟着标题排，别让 pad_left 从左边切掉半句话。
            if display_width(value) <= PERCENT_WIDTH {
                return vec![format!(
                    "{}{}{}",
                    pad_right(label, VALUE_NAME_WIDTH),
                    "  ",
                    pad_left(value, PERCENT_WIDTH)
                )];
            }
            wrap(&format!("{label} {value}"), WIDTH)
        }
    }
}

fn bar(used_percent: f64) -> String {
    let filled = ((used_percent.clamp(0.0, 100.0) / 100.0) * BAR_WIDTH as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(BAR_WIDTH - filled))
}

fn countdown(reset_ms: Option<i64>) -> String {
    let Some(reset_ms) = reset_ms else {
        return String::new();
    };
    let minutes = (reset_ms - now_ms()).max(0).div_euclid(60_000);
    if minutes == 0 {
        return "now".to_string();
    }
    let days = minutes / 1440;
    let hours = (minutes % 1440) / 60;
    let minutes = minutes % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// 显示宽度：中日韩文字与全角标点占两列，终端按列切，不按字符切。
/// 按字符数排的话「余额」这类标签会多占两列，值就被切掉尾巴（本机先错的就是这个）。
fn char_width(character: char) -> usize {
    match character as u32 {
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// 取前 `columns` 列（不足补空格）。
fn pad_right(text: &str, columns: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for character in text.chars() {
        let step = char_width(character);
        if used + step > columns {
            break;
        }
        out.push(character);
        used += step;
    }
    out.push_str(&" ".repeat(columns - used));
    out
}

/// 取后 `columns` 列（不足左侧补空格）。
fn pad_left(text: &str, columns: usize) -> String {
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for character in text.chars().rev() {
        let step = char_width(character);
        if used + step > columns {
            break;
        }
        tail.push(character);
        used += step;
    }
    let mut out = " ".repeat(columns - used);
    out.extend(tail.into_iter().rev());
    out
}

/// 按空格折行；中文没有空格，单个「词」就按列宽硬切。
fn wrap(text: &str, columns: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for word in text.split_whitespace() {
        let word_width = display_width(word);
        if !line.is_empty() && used + 1 + word_width <= columns {
            line.push(' ');
            line.push_str(word);
            used += 1 + word_width;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        let mut chunk = String::new();
        let mut chunk_width = 0;
        for character in word.chars() {
            let step = char_width(character);
            if chunk_width + step > columns {
                lines.push(std::mem::take(&mut chunk));
                chunk_width = 0;
            }
            chunk.push(character);
            chunk_width += step;
        }
        line = chunk;
        used = chunk_width;
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

// ------------------------------------------------------------- HTTP 与凭据 --

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .expect("http client")
});

fn http() -> &'static reqwest::Client {
    &HTTP
}

async fn get_json(url: &str, bearer: &str, extra: &[(&str, &str)]) -> Result<Value, String> {
    let mut request = http().get(url).header("Accept", "application/json");
    if !bearer.is_empty() {
        request = request.header("Authorization", format!("Bearer {bearer}"));
    }
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    let response = request.send().await.map_err(|e| format!("请求发不出去：{e}"))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let detail = body
            .get("error")
            .and_then(|error| error.get("type").or(error.get("message")))
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("接口拒绝了请求");
        return Err(format!("HTTP {status}: {detail}"));
    }
    Ok(body)
}

/// 环境变量 → `auth/api_keys.json`（`/connect` 存 API key 也是存这里）。
fn credential(names: &[&str]) -> Option<String> {
    for name in names {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return Some(value.trim().to_string());
            }
        }
    }
    let keys = api_keys();
    names
        .iter()
        .find_map(|name| keys.get(*name).and_then(Value::as_str))
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

fn api_keys() -> Map<String, Value> {
    std::fs::read_to_string(crate::ante_home().join("auth").join("api_keys.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn number(object: &Value, field: &str) -> f64 {
    object.get(field).and_then(Value::as_f64).unwrap_or(0.0)
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// `expires_at` 用的是 RFC3339（Ante 写纳秒精度），这里只做展示与比较，不回写。
#[allow(dead_code)]
fn rfc3339_ms(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|at| at.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_default()
}
