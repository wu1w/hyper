//! QQ official Bot gateway. Wash of QwenPaw `qq/channel.py` WS + HTTP send.
//!
//! Phone「连接中」clears only after IDENTIFY → READY on this socket.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

use crate::error::{Error, Result};

use super::envelope::{ContentPart, NativePayload};
use super::manager::ChannelManager;
use super::ChannelEndpoint;

const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
const API_PROD: &str = "https://api.sgroup.qq.com";
const API_SANDBOX: &str = "https://sandbox.api.sgroup.qq.com";

const OP_DISPATCH: i64 = 0;
const OP_HEARTBEAT: i64 = 1;
const OP_IDENTIFY: i64 = 2;
const OP_RECONNECT: i64 = 7;
const OP_INVALID_SESSION: i64 = 9;
const OP_HELLO: i64 = 10;

const INTENT_GUILD_MEMBERS: u64 = 1 << 1;
const INTENT_DIRECT_MESSAGE: u64 = 1 << 12;
const INTENT_GROUP_AND_C2C: u64 = 1 << 25;
const INTENT_INTERACTION: u64 = 1 << 26;
const INTENT_PUBLIC_GUILD_MESSAGES: u64 = 1 << 30;

static MSG_SEQ: AtomicU64 = AtomicU64::new(1);

/// QQ Bot C2C "对方正在输入". Hermes qqbot `MSG_TYPE_INPUT_NOTIFY`.
const MSG_TYPE_INPUT_NOTIFY: i64 = 6;

pub fn credentials(ep: &ChannelEndpoint) -> Option<(String, String)> {
    let app_id = ep
        .extra
        .get("app_id")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    let secret = ep
        .extra
        .get("client_secret")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    if app_id.is_empty() || secret.is_empty() {
        None
    } else {
        Some((app_id, secret))
    }
}

fn api_bases(ep: &ChannelEndpoint) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(b) = ep.extra.get("api_base").map(|s| s.trim().to_string()) {
        if !b.is_empty() {
            out.push(b.trim_end_matches('/').to_string());
        }
    }
    if let Ok(b) = std::env::var("QQ_API_BASE") {
        let b = b.trim().trim_end_matches('/').to_string();
        if !b.is_empty() && !out.iter().any(|x| x == &b) {
            out.push(b);
        }
    }
    for b in [API_PROD, API_SANDBOX] {
        if !out.iter().any(|x| x == b) {
            out.push(b.to_string());
        }
    }
    out
}

async fn access_token(http: &reqwest::Client, app_id: &str, secret: &str) -> Result<String> {
    let url = std::env::var("QQ_TOKEN_URL").unwrap_or_else(|_| TOKEN_URL.into());
    let data: Value = http
        .post(url)
        .json(&json!({"appId": app_id, "clientSecret": secret}))
        .send()
        .await?
        .json()
        .await?;
    data.get("access_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::msg(format!("qq token: {data}")))
}

async fn gateway_url(http: &reqwest::Client, token: &str, bases: &[String]) -> Result<String> {
    let mut last = Error::msg("qq gateway: no api base");
    for base in bases {
        let resp = match http
            .get(format!("{base}/gateway"))
            .header("Authorization", format!("QQBot {token}"))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last = e.into();
                continue;
            }
        };
        let status = resp.status();
        let data: Value = resp.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            if let Some(url) = data.get("url").and_then(Value::as_str) {
                eprintln!("hyper qq gateway {base}");
                return Ok(url.to_string());
            }
        }
        last = Error::msg(format!("qq gateway {base} HTTP {status}: {data}"));
    }
    Err(last)
}

pub async fn run_gateway(ep: ChannelEndpoint, mgr: ChannelManager) -> Result<()> {
    let Some((app_id, secret)) = credentials(&ep) else {
        return Err(Error::msg(
            "qq: extra.app_id and extra.client_secret required",
        ));
    };
    let http = crate::llm_http::env_aware_client(20, TOKEN_URL)?;
    let bases = api_bases(&ep);
    eprintln!("hyper qq gateway starting app_id={app_id}");
    loop {
        match run_once(&http, &ep, &mgr, &app_id, &secret, &bases).await {
            Ok(()) => eprintln!("hyper qq: socket closed, reconnecting"),
            Err(e) => eprintln!("hyper qq: {e}; retry in 2s"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_once(
    http: &reqwest::Client,
    ep: &ChannelEndpoint,
    mgr: &ChannelManager,
    app_id: &str,
    secret: &str,
    bases: &[String],
) -> Result<()> {
    let token = access_token(http, app_id, secret).await?;
    let url = gateway_url(http, &token, bases).await?;
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| Error::msg(format!("qq ws connect: {e}")))?;
    let (write, mut read) = ws.split();
    let write = Arc::new(Mutex::new(write));
    let last_seq = Arc::new(Mutex::new(None::<i64>));
    let mut hb: Option<tokio::task::JoinHandle<()>> = None;
    let intents = INTENT_PUBLIC_GUILD_MESSAGES
        | INTENT_GUILD_MEMBERS
        | INTENT_INTERACTION
        | INTENT_DIRECT_MESSAGE
        | INTENT_GROUP_AND_C2C;

    while let Some(frame) = read.next().await {
        let frame = frame.map_err(|e| Error::msg(format!("qq ws: {e}")))?;
        let Message::Text(text) = frame else { continue };
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let op = payload.get("op").and_then(Value::as_i64).unwrap_or(-1);
        if let Some(s) = payload.get("s").and_then(Value::as_i64) {
            *last_seq.lock().await = Some(s);
        }
        match op {
            OP_HELLO => {
                let interval = payload["d"]["heartbeat_interval"]
                    .as_u64()
                    .unwrap_or(45_000)
                    .max(5_000);
                let identify = json!({
                    "op": OP_IDENTIFY,
                    "d": {
                        "token": format!("QQBot {token}"),
                        "intents": intents,
                        "shard": [0, 1],
                    }
                });
                write
                    .lock()
                    .await
                    .send(Message::Text(identify.to_string().into()))
                    .await
                    .map_err(|e| Error::msg(format!("qq identify: {e}")))?;
                if let Some(h) = hb.take() {
                    h.abort();
                }
                let w = write.clone();
                let seq = last_seq.clone();
                hb = Some(tokio::spawn(async move {
                    let mut tick = tokio::time::interval(Duration::from_millis(interval));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tick.tick().await;
                        let d = *seq.lock().await;
                        let body = json!({"op": OP_HEARTBEAT, "d": d});
                        if w.lock()
                            .await
                            .send(Message::Text(body.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
            }
            OP_DISPATCH => {
                let t = payload.get("t").and_then(Value::as_str).unwrap_or("");
                if t == "READY" {
                    let sid = payload["d"]["session_id"].as_str().unwrap_or("");
                    eprintln!("hyper qq ready session={sid}");
                } else if t == "INTERACTION_CREATE" {
                    let d = &payload["d"];
                    let iid = js_str(&d["id"]);
                    if !iid.is_empty() {
                        let _ = ack_interaction(http, &token, bases, &iid).await;
                    }
                    if let Some(mut env) = native_from_interaction(ep, d) {
                        super::stamp_endpoint(&mut env, ep);
                        if let Err(e) = mgr.ingest(env).await {
                            eprintln!("hyper qq interaction: {e}");
                        }
                    }
                } else if let Some(mut env) = native_from_event(ep, t, &payload["d"]) {
                    super::stamp_endpoint(&mut env, ep);
                    super::xfer::hydrate_http_parts(&mut env.content_parts).await;
                    env.text = super::xfer::query_text_of(&env.content_parts);
                    if env.content_parts.is_empty() {
                        continue;
                    }
                    if let Err(e) = mgr.ingest(env).await {
                        eprintln!("hyper qq ingest: {e}");
                    }
                }
            }
            OP_RECONNECT | OP_INVALID_SESSION => {
                if let Some(h) = hb.take() {
                    h.abort();
                }
                return Ok(());
            }
            _ => {}
        }
    }
    if let Some(h) = hb.take() {
        h.abort();
    }
    Ok(())
}

fn js_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn first_str(vals: &[&Value]) -> String {
    for v in vals {
        let s = js_str(v);
        if !s.is_empty() {
            return s;
        }
    }
    String::new()
}

fn qq_is_mentioned(event: &str, d: &Value) -> bool {
    match event {
        "GROUP_AT_MESSAGE_CREATE" | "AT_MESSAGE_CREATE" => true,
        "C2C_MESSAGE_CREATE" | "DIRECT_MESSAGE_CREATE" => false,
        _ => {
            d.get("mentions")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty())
                || d["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("<@") || c.contains("@everyone"))
        }
    }
}

fn native_from_event(ep: &ChannelEndpoint, t: &str, d: &Value) -> Option<NativePayload> {
    let author = &d["author"];
    let (msg_type, sender, group) = match t {
        "C2C_MESSAGE_CREATE" => (
            "c2c",
            first_str(&[&author["user_openid"], &author["id"]]),
            String::new(),
        ),
        "GROUP_AT_MESSAGE_CREATE" => (
            "group",
            first_str(&[&author["member_openid"], &author["id"]]),
            js_str(&d["group_openid"]),
        ),
        "AT_MESSAGE_CREATE" | "DIRECT_MESSAGE_CREATE" => {
            ("guild", first_str(&[&author["id"]]), String::new())
        }
        _ => return None,
    };
    if sender.is_empty() {
        return None;
    }
    let text = d["content"].as_str().unwrap_or("").trim().to_string();
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(ContentPart::text(&text));
    }
    if let Some(atts) = d.get("attachments").and_then(Value::as_array) {
        for a in atts {
            let name = first_str(&[&a["filename"], &a["file_name"], &a["name"]]);
            let url = first_str(&[&a["url"], &a["content"]]);
            let ctype = first_str(&[&a["content_type"], &a["contentType"]]).to_ascii_lowercase();
            let kind = if ctype.starts_with("image/")
                || name.to_ascii_lowercase().ends_with(".png")
                || name.to_ascii_lowercase().ends_with(".jpg")
                || name.to_ascii_lowercase().ends_with(".jpeg")
                || name.to_ascii_lowercase().ends_with(".gif")
                || name.to_ascii_lowercase().ends_with(".webp")
            {
                super::xfer::Kind::Image
            } else if ctype.starts_with("audio/") {
                super::xfer::Kind::Audio
            } else if ctype.starts_with("video/") {
                super::xfer::Kind::Video
            } else {
                super::xfer::Kind::File
            };
            if url.starts_with("http://") || url.starts_with("https://") {
                let mime = if ctype.is_empty() {
                    super::xfer::guess_mime(&name, kind).to_string()
                } else {
                    ctype
                };
                parts.push(match kind {
                    super::xfer::Kind::Image => ContentPart::Image {
                        image_url: url,
                        url: String::new(),
                        mime,
                    },
                    super::xfer::Kind::Audio => ContentPart::Audio {
                        audio_url: url,
                        url: String::new(),
                        mime,
                    },
                    super::xfer::Kind::Video => ContentPart::Video {
                        video_url: url,
                        url: String::new(),
                        mime,
                    },
                    super::xfer::Kind::File => ContentPart::File {
                        file_url: url,
                        file_id: String::new(),
                        name: if name.is_empty() { "file".into() } else { name },
                    },
                });
            } else if kind == super::xfer::Kind::Image {
                parts.push(ContentPart::text("[图片]"));
            } else if !name.is_empty() {
                parts.push(ContentPart::text(format!("[文件] {name}")));
            } else {
                parts.push(ContentPart::text("[文件]"));
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    let text = NativePayload {
        content_parts: parts.clone(),
        ..NativePayload::default()
    }
    .query_text();
    let mut env = NativePayload {
        channel: if ep.kind.is_empty() {
            "qq".into()
        } else {
            ep.kind.clone()
        },
        sender_id: sender.clone(),
        sender_name: author["username"].as_str().unwrap_or("").to_string(),
        content_parts: parts,
        text,
        ..NativePayload::default()
    };
    env.meta.insert("message_type".into(), json!(msg_type));
    env.meta
        .insert("is_group".into(), json!(msg_type == "group"));
    env.meta
        .insert("is_mentioned".into(), json!(qq_is_mentioned(t, d)));
    if let Some(id) = d["id"].as_str() {
        env.meta.insert("msg_id".into(), json!(id));
    }
    if !group.is_empty() {
        env.meta.insert("group_openid".into(), json!(group));
    }
    env.meta.insert("user_openid".into(), json!(sender));
    Some(env)
}

pub async fn send(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    let Some(ep) = ep else {
        return Ok(());
    };
    let Some((app_id, secret)) = credentials(ep) else {
        return Err(Error::msg("qq send: missing credentials"));
    };
    let http = crate::llm_http::env_aware_client(20, TOKEN_URL)?;
    let token = access_token(&http, &app_id, &secret).await?;
    let bases = api_bases(ep);
    let text = super::im_md::separated_plain(&super::xfer::spoken_text(parts));
    // Hermes smart chunking: long answers split into bubbles at natural
    // boundaries instead of being truncated. Only the first bubble rides the
    // passive-reply `msg_id`; the rest go out as active messages.
    let chunks = super::chunk::chunk_text(&text, QQ_TEXT_BUBBLE);
    for (i, chunk) in chunks.iter().enumerate() {
        let mut owned;
        let target = if i == 0 {
            env
        } else {
            owned = env.clone();
            owned.meta.remove("msg_id");
            &owned
        };
        send_typed(
            &http,
            &token,
            &bases,
            target,
            0,
            json!({ "content": chunk }),
        )
        .await?;
    }
    // 被动回复的 msg_id 已被首个文本泡用掉；其后的媒体和失败兜底一律走
    // 主动消息，否则平台会拒绝重复的 msg_id。
    let mut passive_used = !chunks.is_empty();
    for part in parts {
        if matches!(part, ContentPart::Text { .. }) {
            continue;
        }
        let mut owned;
        let target = if passive_used {
            owned = env.clone();
            owned.meta.remove("msg_id");
            &owned
        } else {
            passive_used = true;
            env
        };
        if let Err(e) = send_media(&http, &token, &bases, target, part).await {
            eprintln!("hyper qq media: {e}");
            let line = part.fallback_line().unwrap_or_else(|| "[文件]".into());
            let _ = send_typed(
                &http,
                &token,
                &bases,
                target,
                0,
                json!({ "content": clip_qq(&line) }),
            )
            .await;
        }
    }
    Ok(())
}

pub(crate) async fn send_choices(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    text: &str,
    buttons: &[(String, String)],
) -> Result<Option<String>> {
    let Some(ep) = ep else {
        return Ok(None);
    };
    let Some((app_id, secret)) = credentials(ep) else {
        return Err(Error::msg("qq send: missing credentials"));
    };
    let http = crate::llm_http::env_aware_client(20, TOKEN_URL)?;
    let token = access_token(&http, &app_id, &secret).await?;
    let bases = api_bases(ep);
    let path = messages_path(env)?;
    let seq = MSG_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut body = json!({
        "content": clip_qq(text),
        "msg_type": 0,
        "msg_seq": seq,
        "keyboard": qq_keyboard(buttons),
    });
    if let Some(id) = env.meta.get("msg_id").and_then(Value::as_str) {
        body["msg_id"] = json!(id);
    }
    let data = post_first_ok(&http, &token, &bases, &path, &body).await?;
    Ok(parse_qq_message_id(&data))
}

pub(crate) async fn settle_choices(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    message_id: &str,
    summary: &str,
) -> Result<()> {
    let _ = (ep, env, message_id);
    // QQ C2C/group messages cannot reliably drop an inline keyboard after
    // the fact; the follow-up ack already tells the user who picked what.
    let _ = summary;
    Ok(())
}

fn qq_keyboard(buttons: &[(String, String)]) -> Value {
    let rows: Vec<Value> = buttons
        .iter()
        .map(|(id, label)| {
            json!({
                "buttons": [{
                    "id": id,
                    "render_data": {
                        "label": clip_qq(label),
                        "visited_label": clip_qq(label),
                        "style": 1
                    },
                    "action": {
                        "type": 2,
                        "permission": { "type": 2 },
                        "data": id,
                        "unsupport_tips": "请升级QQ客户端"
                    }
                }]
            })
        })
        .collect();
    json!({ "content": { "rows": rows } })
}

fn parse_qq_message_id(data: &Value) -> Option<String> {
    for key in ["id", "message_id", "msg_id"] {
        let s = js_str(&data[key]);
        if !s.is_empty() {
            return Some(s);
        }
        let s = js_str(&data["data"][key]);
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

fn native_from_interaction(ep: &ChannelEndpoint, d: &Value) -> Option<NativePayload> {
    let choice = first_str(&[
        &d["data"]["resolved"]["button_data"],
        &d["data"]["resolved"]["button_id"],
        &d["data"]["button_data"],
        &d["data"]["id"],
    ]);
    if choice.is_empty() {
        return None;
    }
    let scene = js_str(&d["scene"]).to_ascii_lowercase();
    let chat_type = js_str(&d["chat_type"]);
    let is_group = scene == "group" || chat_type == "1" || !js_str(&d["group_openid"]).is_empty();
    let sender = first_str(&[
        &d["user_openid"],
        &d["data"]["resolved"]["user_openid"],
        &d["author"]["user_openid"],
        &d["author"]["id"],
    ]);
    if sender.is_empty() {
        return None;
    }
    let group = js_str(&d["group_openid"]);
    let mut env = NativePayload {
        channel: if ep.kind.is_empty() {
            "qq".into()
        } else {
            ep.kind.clone()
        },
        sender_id: sender.clone(),
        sender_name: String::new(),
        content_parts: vec![ContentPart::text(&choice)],
        text: choice,
        ..NativePayload::default()
    };
    env.meta.insert(
        "message_type".into(),
        json!(if is_group { "group" } else { "c2c" }),
    );
    env.meta.insert("is_group".into(), json!(is_group));
    env.meta.insert("is_mentioned".into(), json!(true));
    env.mark_choice_click();
    if !group.is_empty() {
        env.meta.insert("group_openid".into(), json!(group));
    }
    env.meta.insert("user_openid".into(), json!(sender));
    Some(env)
}

async fn ack_interaction(
    http: &reqwest::Client,
    token: &str,
    bases: &[String],
    id: &str,
) -> Result<()> {
    let mut last = Error::msg("qq interaction ack failed");
    for base in bases {
        let url = format!("{base}/interactions/{id}");
        let resp = match http
            .put(&url)
            .header("Authorization", format!("QQBot {token}"))
            .json(&json!({ "code": 0 }))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last = e.into();
                continue;
            }
        };
        if resp.status().is_success() {
            return Ok(());
        }
        last = Error::msg(format!(
            "qq interaction ack {} {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        ));
    }
    Err(last)
}

const TYPING_DEBOUNCE: Duration = Duration::from_secs(50);

fn typing_due(openid: &str) -> bool {
    static LAST: OnceLock<StdMutex<HashMap<String, Instant>>> = OnceLock::new();
    let last = LAST.get_or_init(|| StdMutex::new(HashMap::new()));
    let Ok(mut g) = last.lock() else {
        return true;
    };
    let now = Instant::now();
    if g.get(openid)
        .is_some_and(|t| now.duration_since(*t) < TYPING_DEBOUNCE)
    {
        return false;
    }
    g.insert(openid.to_string(), now);
    true
}

fn c2c_typing_ok(env: &NativePayload) -> bool {
    let msg_type = env
        .meta
        .get("message_type")
        .and_then(Value::as_str)
        .unwrap_or("c2c");
    if msg_type != "c2c" {
        return false;
    }
    !env.meta
        .get("msg_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
}

/// Hermes `_keep_typing`: C2C input_notify, 60s bubble, refresh before expiry.
pub async fn send_typing(ep: Option<&ChannelEndpoint>, env: &NativePayload) -> Result<()> {
    let Some(ep) = ep else {
        return Ok(());
    };
    if !c2c_typing_ok(env) {
        return Ok(());
    }
    let openid = env
        .meta
        .get("user_openid")
        .and_then(Value::as_str)
        .unwrap_or(&env.sender_id);
    if !openid.is_empty() && !typing_due(openid) {
        return Ok(());
    }
    let Some((app_id, secret)) = credentials(ep) else {
        return Ok(());
    };
    let http = crate::llm_http::env_aware_client(20, TOKEN_URL)?;
    let token = access_token(&http, &app_id, &secret).await?;
    let bases = api_bases(ep);
    send_typed(
        &http,
        &token,
        &bases,
        env,
        MSG_TYPE_INPUT_NOTIFY,
        json!({
            "input_notify": { "input_type": 1, "input_second": 60 }
        }),
    )
    .await
}

/// One QQ text bubble. Longer replies split via [`super::chunk::chunk_text`].
const QQ_TEXT_BUBBLE: usize = 2000;

fn clip_qq(text: &str) -> String {
    text.chars().take(2000).collect()
}

fn qq_file_type(kind: super::xfer::Kind) -> i32 {
    match kind {
        super::xfer::Kind::Image => 1,
        super::xfer::Kind::Video => 2,
        super::xfer::Kind::Audio => 3,
        super::xfer::Kind::File => 4,
    }
}

fn messages_path(env: &NativePayload) -> Result<String> {
    let msg_type = env
        .meta
        .get("message_type")
        .and_then(Value::as_str)
        .unwrap_or("c2c");
    if msg_type == "group" {
        let id = env
            .meta
            .get("group_openid")
            .and_then(Value::as_str)
            .unwrap_or("");
        if id.is_empty() {
            return Err(Error::msg("qq send: missing group_openid"));
        }
        Ok(format!("/v2/groups/{id}/messages"))
    } else {
        let id = env
            .meta
            .get("user_openid")
            .and_then(Value::as_str)
            .unwrap_or(&env.sender_id);
        if id.is_empty() {
            return Err(Error::msg("qq send: missing user_openid"));
        }
        Ok(format!("/v2/users/{id}/messages"))
    }
}

fn files_path(env: &NativePayload) -> Result<String> {
    let msg_type = env
        .meta
        .get("message_type")
        .and_then(Value::as_str)
        .unwrap_or("c2c");
    if msg_type == "group" {
        let id = env
            .meta
            .get("group_openid")
            .and_then(Value::as_str)
            .unwrap_or("");
        if id.is_empty() {
            return Err(Error::msg("qq send: missing group_openid"));
        }
        Ok(format!("/v2/groups/{id}/files"))
    } else {
        let id = env
            .meta
            .get("user_openid")
            .and_then(Value::as_str)
            .unwrap_or(&env.sender_id);
        if id.is_empty() {
            return Err(Error::msg("qq send: missing user_openid"));
        }
        Ok(format!("/v2/users/{id}/files"))
    }
}

async fn send_typed(
    http: &reqwest::Client,
    token: &str,
    bases: &[String],
    env: &NativePayload,
    msg_type: i64,
    extra: Value,
) -> Result<()> {
    let path = messages_path(env)?;
    let seq = MSG_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut body = extra;
    if !body.is_object() {
        body = json!({});
    }
    body["msg_type"] = json!(msg_type);
    body["msg_seq"] = json!(seq);
    if let Some(id) = env.meta.get("msg_id").and_then(Value::as_str) {
        body["msg_id"] = json!(id);
    }
    post_first_ok(http, token, bases, &path, &body).await?;
    Ok(())
}

async fn send_media(
    http: &reqwest::Client,
    token: &str,
    bases: &[String],
    env: &NativePayload,
    part: &ContentPart,
) -> Result<()> {
    let blob = super::xfer::load_part(part, None).await?;
    let file_type = qq_file_type(blob.kind);
    let info = upload_qq_file(http, token, bases, env, file_type, part, &blob).await?;
    send_typed(
        http,
        token,
        bases,
        env,
        7,
        json!({ "media": { "file_info": info } }),
    )
    .await
}

async fn upload_qq_file(
    http: &reqwest::Client,
    token: &str,
    bases: &[String],
    env: &NativePayload,
    file_type: i32,
    part: &ContentPart,
    blob: &super::xfer::Blob,
) -> Result<String> {
    let path = files_path(env)?;
    if let Some(url) = super::xfer::http_src(part) {
        let body = json!({
            "file_type": file_type,
            "url": url,
            "srv_send_msg": false,
        });
        if let Ok(data) = post_first_ok(http, token, bases, &path, &body).await {
            if let Some(info) = qq_file_info(&data) {
                return Ok(info);
            }
        }
    }
    let body = json!({
        "file_type": file_type,
        "file_data": STANDARD.encode(&blob.bytes),
        "srv_send_msg": false,
    });
    let data = post_first_ok(http, token, bases, &path, &body).await?;
    qq_file_info(&data).ok_or_else(|| Error::msg(format!("qq files: no file_info {data}")))
}

fn qq_file_info(data: &Value) -> Option<String> {
    for key in ["file_info", "fileInfo"] {
        let s = data.get(key).and_then(Value::as_str).unwrap_or("");
        if !s.is_empty() {
            return Some(s.to_string());
        }
        let s = data["data"][key].as_str().unwrap_or("");
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    None
}

async fn post_first_ok(
    http: &reqwest::Client,
    token: &str,
    bases: &[String],
    path: &str,
    body: &Value,
) -> Result<Value> {
    let mut last = Error::msg("qq send failed");
    for base in bases {
        let resp = match http
            .post(format!("{base}{path}"))
            .header("Authorization", format!("QQBot {token}"))
            .json(body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last = e.into();
                continue;
            }
        };
        let status = resp.status();
        let data: Value = resp.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(data);
        }
        last = Error::msg(format!("qq {path} {status} {data}"));
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_type_mapping() {
        assert_eq!(qq_file_type(super::super::xfer::Kind::Image), 1);
        assert_eq!(qq_file_type(super::super::xfer::Kind::Video), 2);
        assert_eq!(qq_file_type(super::super::xfer::Kind::Audio), 3);
        assert_eq!(qq_file_type(super::super::xfer::Kind::File), 4);
    }

    #[test]
    fn native_image_attachment() {
        let ep = ChannelEndpoint {
            kind: "qq".into(),
            ..ChannelEndpoint::default()
        };
        let d = json!({
            "author": {"user_openid": "u1", "username": "will"},
            "content": "see",
            "id": "m1",
            "attachments": [{
                "filename": "a.png",
                "url": "https://cdn.example/a.png",
                "content_type": "image/png"
            }]
        });
        let env = native_from_event(&ep, "C2C_MESSAGE_CREATE", &d).unwrap();
        assert_eq!(env.sender_id, "u1");
        assert!(env
            .content_parts
            .iter()
            .any(|p| matches!(p, ContentPart::Image { .. })));
        assert_eq!(messages_path(&env).unwrap(), "/v2/users/u1/messages");
        assert_eq!(files_path(&env).unwrap(), "/v2/users/u1/files");
    }

    #[test]
    fn group_paths() {
        let mut env = NativePayload::default();
        env.meta.insert("message_type".into(), json!("group"));
        env.meta.insert("group_openid".into(), json!("g1"));
        assert_eq!(messages_path(&env).unwrap(), "/v2/groups/g1/messages");
        assert_eq!(files_path(&env).unwrap(), "/v2/groups/g1/files");
        assert!(!c2c_typing_ok(&env));
    }

    #[test]
    fn c2c_typing_needs_msg_id() {
        let mut env = NativePayload::default();
        env.channel = "qq".into();
        env.meta.insert("message_type".into(), json!("c2c"));
        env.meta.insert("user_openid".into(), json!("u1"));
        assert!(!c2c_typing_ok(&env));
        env.meta.insert("msg_id".into(), json!("m1"));
        assert!(c2c_typing_ok(&env));
        env.meta.insert("message_type".into(), json!("group"));
        assert!(!c2c_typing_ok(&env));
    }

    #[test]
    fn group_at_is_mentioned_c2c_is_not() {
        let ep = ChannelEndpoint {
            kind: "qq".into(),
            ..ChannelEndpoint::default()
        };
        let c2c = native_from_event(
            &ep,
            "C2C_MESSAGE_CREATE",
            &json!({
                "author": {"user_openid": "u1", "username": "will"},
                "content": "删掉这个",
                "id": "m1",
            }),
        )
        .unwrap();
        assert!(!c2c.is_mentioned());
        assert!(!c2c.is_group());
        let group = native_from_event(
            &ep,
            "GROUP_AT_MESSAGE_CREATE",
            &json!({
                "author": {"member_openid": "u1", "username": "will"},
                "content": "删掉这个",
                "id": "m2",
                "group_openid": "g1",
            }),
        )
        .unwrap();
        assert!(group.is_mentioned());
        assert!(group.is_group());
        assert!(!qq_is_mentioned(
            "GROUP_MESSAGE_CREATE",
            &json!({"content": "删掉这个"})
        ));
    }

    #[test]
    fn keyboard_and_interaction_choice() {
        let kb = qq_keyboard(&[("p:abcd1234:1".into(), "允许".into())]);
        assert_eq!(
            kb["content"]["rows"][0]["buttons"][0]["action"]["data"],
            "p:abcd1234:1"
        );
        let ep = ChannelEndpoint {
            kind: "qq".into(),
            ..ChannelEndpoint::default()
        };
        let env = native_from_interaction(
            &ep,
            &json!({
                "id": "i1",
                "scene": "group",
                "group_openid": "g1",
                "user_openid": "u9",
                "data": { "resolved": { "button_data": "p:abcd1234:1" } }
            }),
        )
        .unwrap();
        assert_eq!(env.query_text(), "p:abcd1234:1");
        assert!(env.is_choice_click());
        assert!(env.is_group());
        assert_eq!(env.sender_id, "u9");
    }
}
