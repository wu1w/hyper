//! Telegram Bot API long-poll. Wash of QwenPaw `telegram/channel.py` inbound
//! shape: native dict with `content_parts`, session key `telegram:dm:{id}`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{Error, Result};

use super::envelope::{ContentPart, NativePayload};
use super::manager::ChannelManager;
use super::ChannelEndpoint;

const API: &str = "https://api.telegram.org";

pub fn token(ep: &ChannelEndpoint) -> Option<String> {
    ep.extra
        .get("bot_token")
        .cloned()
        .or_else(|| ep.extra.get("token").cloned())
        .or_else(|| std::env::var("TELEGRAM_BOT_TOKEN").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub async fn run_long_poll(ep: ChannelEndpoint, mgr: ChannelManager) -> Result<()> {
    let Some(token) = token(&ep) else {
        return Err(Error::msg(
            "telegram: set extra.bot_token or TELEGRAM_BOT_TOKEN",
        ));
    };
    let client = crate::llm_http::env_aware_client(40, API)?;
    let mut offset = load_offset(&ep.id);
    eprintln!("hyper channel telegram long-poll ({})", ep.id);
    let mut conflict_n = 0u32;
    loop {
        let url = format!(
            "{API}/bot{token}/getUpdates?timeout=25&offset={offset}&allowed_updates=%5B%22message%22%2C%22callback_query%22%5D"
        );
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("hyper telegram poll: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let status = resp.status();
        let body: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("hyper telegram json: {e}");
                continue;
            }
        };
        if body["ok"] != true {
            let desc = body["description"].as_str().unwrap_or("");
            if let Some(secs) = retry_after_secs(&body, desc, status.as_u16()) {
                eprintln!("hyper telegram: rate-limited, retry in {secs}s");
                tokio::time::sleep(Duration::from_secs(secs)).await;
                continue;
            }
            if is_getupdates_conflict(desc, status.as_u16()) {
                conflict_n = conflict_n.saturating_add(1);
                let delay = conflict_backoff_s(conflict_n);
                eprintln!("hyper telegram: getUpdates conflict, retry in {delay}s");
                tokio::time::sleep(Duration::from_secs(delay)).await;
                continue;
            }
            eprintln!("hyper telegram: {desc}");
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        }
        conflict_n = 0;
        let Some(arr) = body["result"].as_array() else {
            continue;
        };
        for upd in arr {
            let id = upd["update_id"].as_i64().unwrap_or(0);
            offset = id + 1;
            if let Some(msg) = upd.get("message").or_else(|| upd.get("edited_message")) {
                match native_from_message(&client, &token, &ep, msg).await {
                    Ok(Some(mut env)) => {
                        super::stamp_endpoint(&mut env, &ep);
                        if let Err(e) = mgr.ingest(env).await {
                            eprintln!("hyper telegram ingest: {e}");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("hyper telegram msg: {e}"),
                }
            }
            if let Some(cq) = upd.get("callback_query") {
                let cq_id = cq["id"].as_str().unwrap_or("");
                if !cq_id.is_empty() {
                    let _ = answer_callback_query(&client, &token, cq_id).await;
                }
                if let Some(mut env) = native_from_callback(&ep, cq) {
                    super::stamp_endpoint(&mut env, &ep);
                    if let Err(e) = mgr.ingest(env).await {
                        eprintln!("hyper telegram callback: {e}");
                    }
                }
            }
        }
        save_offset(&ep.id, offset);
    }
}

pub async fn send(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    let Some(ep) = ep else {
        return Ok(());
    };
    let Some(token) = token(ep) else {
        return Err(Error::msg("telegram send: missing bot token"));
    };
    let chat_id = env.chat_id();
    if chat_id.is_empty() {
        return Err(Error::msg("telegram send: missing chat_id"));
    }
    let client = crate::llm_http::env_aware_client(30, API)?;
    let text = super::im_md::separated_plain(&super::xfer::spoken_text(parts));
    if !text.trim().is_empty() {
        promote_or_send(&client, &token, env, &chat_id, &text).await?;
    }
    for part in parts {
        if matches!(part, ContentPart::Text { .. }) {
            continue;
        }
        if let Err(e) = send_media(&client, &token, env, &chat_id, part).await {
            eprintln!("hyper telegram media: {e}");
            let fallback = part.fallback_line().unwrap_or_else(|| "[文件]".into());
            let _ = send_message(&client, &token, env, &chat_id, &fallback).await;
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
    let Some(token) = token(ep) else {
        return Err(Error::msg("telegram send: missing bot token"));
    };
    let chat_id = env.chat_id();
    if chat_id.is_empty() {
        return Err(Error::msg("telegram send: missing chat_id"));
    }
    let client = crate::llm_http::env_aware_client(30, API)?;
    let id = send_message_markup(&client, &token, env, &chat_id, text, buttons).await?;
    Ok(id.map(|n| n.to_string()))
}

pub(crate) async fn settle_choices(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    message_id: &str,
    summary: &str,
) -> Result<()> {
    let Some(ep) = ep else {
        return Ok(());
    };
    let Some(token) = token(ep) else {
        return Ok(());
    };
    let chat_id = env.chat_id();
    let Ok(mid) = message_id.parse::<i64>() else {
        return Ok(());
    };
    if chat_id.is_empty() {
        return Ok(());
    }
    let Ok(client) = crate::llm_http::env_aware_client(15, API) else {
        return Ok(());
    };
    let url = edit_message_url(&token);
    let body = json!({
        "chat_id": chat_id,
        "message_id": mid,
        "text": clip(summary, 3900),
        "reply_markup": { "inline_keyboard": [] },
    });
    let resp = client.post(&url).json(&body).send().await?;
    if !resp.status().is_success() {
        let t = resp.text().await.unwrap_or_default();
        return Err(Error::msg(format!("telegram settle choice: {t}")));
    }
    Ok(())
}

async fn answer_callback_query(
    client: &reqwest::Client,
    token: &str,
    callback_id: &str,
) -> Result<()> {
    let url = format!("{API}/bot{token}/answerCallbackQuery");
    let body = json!({ "callback_query_id": callback_id });
    let resp = client.post(&url).json(&body).send().await?;
    if !resp.status().is_success() {
        return Err(Error::msg(format!(
            "telegram answerCallbackQuery: {}",
            resp.text().await.unwrap_or_default()
        )));
    }
    Ok(())
}

pub(crate) async fn send_typing(ep: Option<&ChannelEndpoint>, env: &NativePayload) {
    let Some(ep) = ep else {
        return;
    };
    let Some(token) = token(ep) else {
        return;
    };
    let chat_id = env.chat_id();
    if chat_id.is_empty() {
        return;
    }
    let Ok(client) = crate::llm_http::env_aware_client(8, API) else {
        return;
    };
    let url = format!("{API}/bot{token}/sendChatAction");
    let body = json!({ "chat_id": chat_id, "action": "typing" });
    let _ = client.post(&url).json(&body).send().await;
}

/// ACK / think / tool lines: one bubble, edited in place (Hermes-style).
pub(crate) async fn send_progress(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    let Some(ep) = ep else {
        return Ok(());
    };
    let Some(token) = token(ep) else {
        return Ok(());
    };
    let chat_id = env.chat_id();
    if chat_id.is_empty() {
        return Ok(());
    }
    let text = super::im_md::separated_plain(&super::xfer::spoken_text(parts));
    if text.trim().is_empty() {
        return Ok(());
    }
    let Ok(client) = crate::llm_http::env_aware_client(15, API) else {
        return Ok(());
    };
    upsert_progress(&client, &token, env, &chat_id, &text).await
}

async fn promote_or_send(
    client: &reqwest::Client,
    token: &str,
    env: &NativePayload,
    chat_id: &str,
    text: &str,
) -> Result<()> {
    // Leave the progress bubble as-is; the spoken answer is a new message.
    // Long answers split into ordered bubbles instead of being clipped.
    take_bubble(&env.progress_bubble_key());
    for chunk in super::chunk::chunk_text(text, TG_TEXT_BUBBLE) {
        send_message(client, token, env, chat_id, &chunk).await?;
    }
    Ok(())
}

async fn upsert_progress(
    client: &reqwest::Client,
    token: &str,
    env: &NativePayload,
    chat_id: &str,
    text: &str,
) -> Result<()> {
    let key = env.progress_bubble_key();
    if let Some(mid) = peek_bubble(&key) {
        if edit_message(client, token, chat_id, mid, text)
            .await
            .is_ok()
        {
            return Ok(());
        }
    }
    if let Some(mid) = send_message(client, token, env, chat_id, text).await? {
        store_bubble(&key, mid);
    }
    Ok(())
}

fn bubbles() -> &'static StdMutex<HashMap<String, i64>> {
    static C: OnceLock<StdMutex<HashMap<String, i64>>> = OnceLock::new();
    C.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn store_bubble(key: &str, mid: i64) {
    let Ok(mut g) = bubbles().lock() else {
        return;
    };
    g.insert(key.to_string(), mid);
}

fn peek_bubble(key: &str) -> Option<i64> {
    let Ok(g) = bubbles().lock() else {
        return None;
    };
    g.get(key).copied()
}

fn take_bubble(key: &str) -> Option<i64> {
    let Ok(mut g) = bubbles().lock() else {
        return None;
    };
    g.remove(key)
}

fn parse_sent_message_id(body: &str) -> Option<i64> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.get("result")
        .and_then(|r| r.get("message_id"))
        .and_then(|m| m.as_i64().or_else(|| m.as_u64().map(|n| n as i64)))
}

fn edit_message_url(token: &str) -> String {
    format!("{API}/bot{token}/editMessageText")
}

async fn edit_message(
    client: &reqwest::Client,
    token: &str,
    chat_id: &str,
    message_id: i64,
    text: &str,
) -> Result<()> {
    let url = edit_message_url(token);
    let body = json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "text": clip(text, 3900),
    });
    let resp = client.post(&url).json(&body).send().await?;
    let status = resp.status();
    let t = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::msg(format!("telegram editMessageText: {t}")));
    }
    Ok(())
}

async fn send_message(
    client: &reqwest::Client,
    token: &str,
    env: &NativePayload,
    chat_id: &str,
    text: &str,
) -> Result<Option<i64>> {
    let url = format!("{API}/bot{token}/sendMessage");
    let body = with_thread(
        json!({
            "chat_id": chat_id,
            "text": clip(text, 3900),
        }),
        env,
    );
    let resp = client.post(&url).json(&body).send().await?;
    let status = resp.status();
    let t = resp.text().await.unwrap_or_default();
    if status.as_u16() == 429 {
        let secs = serde_json::from_str::<Value>(&t)
            .ok()
            .and_then(|v| retry_after_secs(&v, &t, 429))
            .unwrap_or(3);
        tokio::time::sleep(Duration::from_secs(secs)).await;
        let retry = client.post(&url).json(&body).send().await?;
        let status = retry.status();
        let t = retry.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::msg(format!("telegram sendMessage: {t}")));
        }
        return Ok(parse_sent_message_id(&t));
    }
    if !status.is_success() {
        return Err(Error::msg(format!("telegram sendMessage: {t}")));
    }
    Ok(parse_sent_message_id(&t))
}

async fn send_message_markup(
    client: &reqwest::Client,
    token: &str,
    env: &NativePayload,
    chat_id: &str,
    text: &str,
    buttons: &[(String, String)],
) -> Result<Option<i64>> {
    let url = format!("{API}/bot{token}/sendMessage");
    let keyboard: Vec<Vec<Value>> = buttons
        .iter()
        .map(|(id, label)| {
            vec![json!({
                "text": clip(label, 64),
                "callback_data": clip_callback(id),
            })]
        })
        .collect();
    let body = with_thread(
        json!({
            "chat_id": chat_id,
            "text": clip(text, 3900),
            "reply_markup": { "inline_keyboard": keyboard },
        }),
        env,
    );
    let resp = client.post(&url).json(&body).send().await?;
    let status = resp.status();
    let t = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::msg(format!("telegram sendMessage: {t}")));
    }
    Ok(parse_sent_message_id(&t))
}

fn clip_callback(id: &str) -> String {
    let trimmed = id.trim();
    if trimmed.chars().count() <= 64 {
        return trimmed.to_string();
    }
    trimmed.chars().take(64).collect()
}

async fn send_media(
    client: &reqwest::Client,
    token: &str,
    env: &NativePayload,
    chat_id: &str,
    part: &ContentPart,
) -> Result<()> {
    let blob = super::xfer::load_part(part, None).await?;
    let (method, field) = match blob.kind {
        super::xfer::Kind::Image => ("sendPhoto", "photo"),
        super::xfer::Kind::Audio => ("sendAudio", "audio"),
        super::xfer::Kind::Video => ("sendVideo", "video"),
        super::xfer::Kind::File => ("sendDocument", "document"),
    };
    let url = format!("{API}/bot{token}/{method}");
    let file = super::xfer::bytes_part(&blob);
    let mut form = reqwest::multipart::Form::new()
        .text("chat_id", chat_id.to_string())
        .part(field, file);
    if let Some(tid) = telegram_thread_id(env) {
        form = form.text("message_thread_id", tid.to_string());
    }
    let resp = client.post(&url).multipart(form).send().await?;
    if !resp.status().is_success() {
        return Err(Error::msg(format!(
            "telegram {method}: {}",
            resp.text().await.unwrap_or_default()
        )));
    }
    Ok(())
}

async fn native_from_message(
    client: &reqwest::Client,
    token: &str,
    ep: &ChannelEndpoint,
    msg: &Value,
) -> Result<Option<NativePayload>> {
    let chat = &msg["chat"];
    let from = &msg["from"];
    let chat_id = chat["id"].to_string();
    let sender_id = from["id"].to_string();
    if sender_id == "null" || chat_id == "null" {
        return Ok(None);
    }
    let chat_type = chat["type"].as_str().unwrap_or("private");
    let is_group = matches!(chat_type, "group" | "supergroup");
    let bot_username = ep.extra.get("bot_username").cloned().unwrap_or_default();
    let text = msg["text"]
        .as_str()
        .or_else(|| msg["caption"].as_str())
        .unwrap_or("")
        .to_string();
    let mentioned = is_mentioned(&text, &bot_username)
        || msg["reply_to_message"]["from"]["is_bot"].as_bool() == Some(true);
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(ContentPart::text(strip_mention(&text, &bot_username)));
    }
    if let Some(photos) = msg["photo"].as_array() {
        if let Some(best) = photos.last() {
            if let Some(file_id) = best["file_id"].as_str() {
                match fetch_telegram_file(client, token, file_id, "image.jpg").await {
                    Ok(p) => parts.push(p),
                    Err(_) => parts.push(ContentPart::text("[图片]")),
                }
            }
        }
    }
    if let Some(doc) = msg.get("document") {
        if let Some(file_id) = doc["file_id"].as_str() {
            let name = doc["file_name"].as_str().unwrap_or("file").to_string();
            match fetch_telegram_file(client, token, file_id, &name).await {
                Ok(p) => parts.push(p),
                Err(_) => parts.push(ContentPart::text(format!("[文件] {name}"))),
            }
        }
    }
    for key in ["voice", "audio"] {
        if let Some(fid) = msg.get(key).and_then(|v| v["file_id"].as_str()) {
            match fetch_telegram_file(client, token, fid, "voice.ogg").await {
                Ok(p) => parts.push(p),
                Err(_) => parts.push(ContentPart::text("[语音]")),
            }
        }
    }
    for key in ["video", "video_note"] {
        if let Some(fid) = msg.get(key).and_then(|v| v["file_id"].as_str()) {
            match fetch_telegram_file(client, token, fid, "video.mp4").await {
                Ok(p) => parts.push(p),
                Err(_) => parts.push(ContentPart::text("[视频]")),
            }
        }
    }
    if msg.get("sticker").is_some() && parts.is_empty() {
        parts.push(ContentPart::text("[表情]"));
    }
    if parts.is_empty() {
        return Ok(None);
    }
    let mut env = NativePayload {
        channel: if ep.kind.is_empty() {
            "telegram".into()
        } else {
            ep.kind.clone()
        },
        sender_id,
        sender_name: from["first_name"].as_str().unwrap_or("").to_string(),
        content_parts: parts,
        ..NativePayload::default()
    };
    env.meta.insert("chat_id".into(), json!(chat_id));
    env.meta.insert("is_group".into(), json!(is_group));
    env.meta.insert("is_mentioned".into(), json!(mentioned));
    if let Some(mid) = inbound_message_id(msg) {
        env.meta.insert("message_id".into(), json!(mid));
    }
    env.meta.insert(
        "is_reply_to_bot".into(),
        json!(msg["reply_to_message"]["from"]["is_bot"].as_bool() == Some(true)),
    );
    stamp_telegram_thread(&mut env, msg);
    Ok(Some(env))
}

fn native_from_callback(ep: &ChannelEndpoint, cq: &Value) -> Option<NativePayload> {
    let data = cq["data"].as_str().unwrap_or("").trim().to_string();
    if data.is_empty() {
        return None;
    }
    let from = &cq["from"];
    let msg = cq.get("message").unwrap_or(&Value::Null);
    let chat = &msg["chat"];
    let chat_id = chat["id"].to_string();
    let sender_id = from["id"].to_string();
    if sender_id == "null" {
        return None;
    }
    let chat_type = chat["type"].as_str().unwrap_or("private");
    let is_group = matches!(chat_type, "group" | "supergroup");
    let mut env = NativePayload {
        channel: if ep.kind.is_empty() {
            "telegram".into()
        } else {
            ep.kind.clone()
        },
        sender_id,
        sender_name: from["first_name"].as_str().unwrap_or("").to_string(),
        content_parts: vec![ContentPart::text(&data)],
        text: data,
        ..NativePayload::default()
    };
    env.meta.insert(
        "chat_id".into(),
        json!(if chat_id == "null" {
            String::new()
        } else {
            chat_id
        }),
    );
    env.meta.insert("is_group".into(), json!(is_group));
    env.meta.insert("is_mentioned".into(), json!(true));
    if let Some(mid) = inbound_message_id(msg) {
        env.meta.insert("message_id".into(), json!(mid));
    }
    if let Some(id) = cq["id"].as_str() {
        env.meta.insert("callback_query_id".into(), json!(id));
    }
    env.mark_choice_click();
    stamp_telegram_thread(&mut env, msg);
    Some(env)
}

async fn fetch_telegram_file(
    client: &reqwest::Client,
    token: &str,
    file_id: &str,
    name: &str,
) -> Result<ContentPart> {
    let meta: Value = client
        .get(format!("{API}/bot{token}/getFile?file_id={file_id}"))
        .send()
        .await?
        .json()
        .await?;
    let path = meta["result"]["file_path"]
        .as_str()
        .ok_or_else(|| Error::msg("telegram getFile: no file_path"))?;
    let bytes = client
        .get(format!("{API}/file/bot{token}/{path}"))
        .send()
        .await?
        .bytes()
        .await?;
    if bytes.len() > super::xfer::FETCH_CAP {
        return Err(Error::msg("telegram file over cap"));
    }
    let kind = super::xfer::kind_from_name(name);
    let mime = super::xfer::guess_mime(name, kind).to_string();
    let blob = super::xfer::Blob {
        kind,
        mime,
        name: name.to_string(),
        bytes: bytes.to_vec(),
    };
    super::xfer::blob_to_inbound_part(blob)
}

fn inbound_message_id(msg: &Value) -> Option<String> {
    match msg.get("message_id") {
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn stamp_telegram_thread(env: &mut NativePayload, msg: &Value) {
    match msg.get("message_thread_id") {
        Some(Value::Number(n)) => env.stamp_thread(n.to_string()),
        Some(Value::String(s)) => env.stamp_thread(s),
        _ => {}
    }
}

fn telegram_thread_id(env: &NativePayload) -> Option<i64> {
    env.thread_id()?.parse().ok()
}

fn with_thread(mut body: Value, env: &NativePayload) -> Value {
    if let Some(tid) = telegram_thread_id(env) {
        body["message_thread_id"] = json!(tid);
    }
    body
}

fn is_mentioned(text: &str, bot_username: &str) -> bool {
    if bot_username.is_empty() {
        return false;
    }
    let tag = format!("@{}", bot_username.trim_start_matches('@'));
    text.split_whitespace()
        .any(|w| w.eq_ignore_ascii_case(&tag))
}

fn strip_mention(text: &str, bot_username: &str) -> String {
    if bot_username.is_empty() {
        return text.to_string();
    }
    let tag = format!("@{}", bot_username.trim_start_matches('@'));
    text.split_whitespace()
        .filter(|w| !w.eq_ignore_ascii_case(&tag))
        .collect::<Vec<_>>()
        .join(" ")
}

fn offset_path(id: &str) -> PathBuf {
    crate::config::Config::home_dir()
        .map(|h| h.join("channels").join(format!("{id}.offset")))
        .unwrap_or_else(|_| PathBuf::from(format!("/tmp/hyper-{id}.offset")))
}

fn load_offset(id: &str) -> i64 {
    std::fs::read_to_string(offset_path(id))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn save_offset(id: &str, offset: i64) {
    let path = offset_path(id);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, offset.to_string());
}

fn retry_after_secs(body: &Value, desc: &str, http_status: u16) -> Option<u64> {
    if let Some(n) = body["parameters"]["retry_after"].as_u64() {
        return Some(n.clamp(1, 120));
    }
    if http_status != 429 && !desc.to_ascii_lowercase().contains("retry after") {
        return None;
    }
    let lower = desc.to_ascii_lowercase();
    let Some(idx) = lower.rfind("retry after") else {
        return (http_status == 429).then_some(3);
    };
    let rest = desc[idx + "retry after".len()..].trim();
    let n: u64 = rest
        .split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())?;
    Some(n.clamp(1, 120))
}

fn is_getupdates_conflict(desc: &str, http_status: u16) -> bool {
    http_status == 409
        || desc
            .to_ascii_lowercase()
            .contains("terminated by other getupdates")
        || desc.to_ascii_lowercase().contains("conflict")
}

fn conflict_backoff_s(attempt: u32) -> u64 {
    let base = 5.0_f64;
    let cap = 21.0_f64;
    let n = (base * 1.8_f64.powi(attempt.saturating_sub(1) as i32)).min(cap);
    n.ceil() as u64
}

/// One Telegram bubble (platform max is 4096; keep the old 3900 margin).
/// Longer finals split via [`super::chunk::chunk_text`]; edit/progress paths
/// still [`clip`] because a preview bubble must stay one message.
const TG_TEXT_BUBBLE: usize = 3900;

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!(
            "{}…",
            s.chars().take(n.saturating_sub(1)).collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mention_strip() {
        assert!(is_mentioned("hey @hyperbot do it", "hyperbot"));
        assert_eq!(strip_mention("@hyperbot do it", "hyperbot"), "do it");
    }

    #[test]
    fn retry_after_from_parameters() {
        let body = json!({"ok":false,"parameters":{"retry_after":12}});
        assert_eq!(retry_after_secs(&body, "Too Many Requests", 429), Some(12));
    }

    #[test]
    fn retry_after_from_description() {
        let body = json!({"ok": false});
        assert_eq!(
            retry_after_secs(&body, "Too Many Requests: retry after 8", 429),
            Some(8)
        );
    }

    #[test]
    fn conflict_detected() {
        assert!(is_getupdates_conflict(
            "Conflict: terminated by other getUpdates request",
            409
        ));
        assert_eq!(conflict_backoff_s(1), 5);
        assert!(conflict_backoff_s(8) <= 21);
    }

    #[test]
    fn parse_sent_message_id_from_send_ok() {
        assert_eq!(
            parse_sent_message_id(r#"{"ok":true,"result":{"message_id":77,"chat":{"id":1}}}"#),
            Some(77)
        );
        assert!(parse_sent_message_id(r#"{"ok":false}"#).is_none());
    }

    #[test]
    fn inbound_message_id_stringifies_number() {
        assert_eq!(
            inbound_message_id(&json!({"message_id": 42})).as_deref(),
            Some("42")
        );
        assert!(inbound_message_id(&json!({"text": "hi"})).is_none());
    }

    #[test]
    fn edit_url_and_bubble_slot() {
        assert!(edit_message_url("tok").ends_with("/bottok/editMessageText"));
        store_bubble("c:1", 9);
        assert_eq!(peek_bubble("c:1"), Some(9));
        assert_eq!(take_bubble("c:1"), Some(9));
        assert_eq!(peek_bubble("c:1"), None);
    }

    #[test]
    fn callback_query_becomes_choice_text() {
        let ep = ChannelEndpoint {
            kind: "telegram".into(),
            ..ChannelEndpoint::default()
        };
        let cq = json!({
            "id": "cb1",
            "from": {"id": 42, "first_name": "W"},
            "message": {"message_id": 9, "chat": {"id": 42, "type": "private"}},
            "data": "2"
        });
        let env = native_from_callback(&ep, &cq).unwrap();
        assert_eq!(env.sender_id, "42");
        assert_eq!(env.query_text(), "2");
        assert!(env.is_choice_click());
        assert_eq!(env.meta["is_mentioned"], json!(true));
        assert_eq!(env.meta["chat_id"], json!("42"));
        assert_eq!(clip_callback(&"a".repeat(80)).chars().count(), 64);
        let topic = json!({
            "id": "cb2",
            "from": {"id": 7, "first_name": "A"},
            "message": {
                "message_id": 3,
                "message_thread_id": 99,
                "chat": {"id": -100, "type": "supergroup"}
            },
            "data": "1"
        });
        let env = native_from_callback(&ep, &topic).unwrap();
        assert_eq!(env.thread_id().as_deref(), Some("99"));
        assert!(env.is_group());
    }
}
