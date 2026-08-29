//! Telegram Bot API long-poll. Wash of QwenPaw `telegram/channel.py` inbound
//! shape: native dict with `content_parts`, session key `telegram:dm:{id}`.

use std::path::PathBuf;
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
            "{API}/bot{token}/getUpdates?timeout=25&offset={offset}&allowed_updates=%5B%22message%22%5D"
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
                    Ok(Some(env)) => {
                        if let Err(e) = mgr.ingest(env).await {
                            eprintln!("hyper telegram ingest: {e}");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("hyper telegram msg: {e}"),
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
    let text = super::xfer::spoken_text(parts);
    if !text.trim().is_empty() {
        send_message(&client, &token, &chat_id, &text).await?;
    }
    for part in parts {
        if matches!(part, ContentPart::Text { .. }) {
            continue;
        }
        if let Err(e) = send_media(&client, &token, &chat_id, part).await {
            eprintln!("hyper telegram media: {e}");
            let fallback = part.fallback_line().unwrap_or_else(|| "[文件]".into());
            let _ = send_message(&client, &token, &chat_id, &fallback).await;
        }
    }
    Ok(())
}

async fn send_message(
    client: &reqwest::Client,
    token: &str,
    chat_id: &str,
    text: &str,
) -> Result<()> {
    let url = format!("{API}/bot{token}/sendMessage");
    let body = json!({
        "chat_id": chat_id,
        "text": clip(text, 3900),
    });
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
        if !retry.status().is_success() {
            let t = retry.text().await.unwrap_or_default();
            return Err(Error::msg(format!("telegram sendMessage: {t}")));
        }
        return Ok(());
    }
    if !status.is_success() {
        return Err(Error::msg(format!("telegram sendMessage: {t}")));
    }
    Ok(())
}

async fn send_media(
    client: &reqwest::Client,
    token: &str,
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
    let form = reqwest::multipart::Form::new()
        .text("chat_id", chat_id.to_string())
        .part(field, file);
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
    env.meta.insert(
        "is_reply_to_bot".into(),
        json!(msg["reply_to_message"]["from"]["is_bot"].as_bool() == Some(true)),
    );
    Ok(Some(env))
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
}
