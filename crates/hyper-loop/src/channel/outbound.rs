//! Send QwenPaw `content_parts` back to the originating chat.

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::task::JoinHandle;

use crate::error::Result;

use super::envelope::{ContentPart, NativePayload};
use super::ChannelEndpoint;

const EMPTY_REPLY: &str = "(无文本回复)";
const DELIVER_ATTEMPTS: u32 = 3;
/// Hermes qqbot: input_notify lasts 60s, refresh at 50s.
const QQ_TYPING_REFRESH: Duration = Duration::from_secs(50);
/// First visible heartbeat if the turn is still running (QQ cannot edit a bubble).
const QQ_HEARTBEAT_FIRST: Duration = Duration::from_secs(90);
const QQ_HEARTBEAT_EVERY: Duration = Duration::from_secs(180);

pub fn reply_text(body: impl Into<String>) -> Vec<ContentPart> {
    let body = body.into();
    if body.trim().is_empty() {
        Vec::new()
    } else {
        vec![ContentPart::text(body)]
    }
}

pub use super::xfer::reply_parts;

pub fn parts_to_text(parts: &[ContentPart]) -> String {
    let mut text = String::new();
    for p in parts {
        if let Some(t) = p.as_text() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(t);
        } else if let Some(line) = p.fallback_line() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&line);
        }
    }
    text
}

pub fn outbound_parts(parts: &[ContentPart]) -> Vec<ContentPart> {
    if parts.is_empty() {
        vec![ContentPart::text(EMPTY_REPLY)]
    } else {
        parts.to_vec()
    }
}

pub fn with_attempts<T, E>(
    attempts: u32,
    mut f: impl FnMut() -> std::result::Result<T, E>,
) -> std::result::Result<T, E> {
    let attempts = attempts.max(1);
    let mut last = None;
    for _ in 0..attempts {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => last = Some(e),
        }
    }
    Err(last.expect("attempts >= 1"))
}

pub async fn deliver(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    let owned = outbound_parts(parts);
    let mut last = None;
    for attempt in 0..DELIVER_ATTEMPTS {
        match deliver_once(ep, env, &owned).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                let retry =
                    attempt + 1 < DELIVER_ATTEMPTS && crate::llm_http::outbound_retryable(&e);
                last = Some(e);
                if retry {
                    tokio::time::sleep(Duration::from_millis(200 * (1u64 << attempt))).await;
                } else {
                    break;
                }
            }
        }
    }
    Err(last.expect("DELIVER_ATTEMPTS >= 1"))
}

/// Hermes gateway: typing + long-running notices run *while* the agent works,
/// not after `handle()` returns. QQ C2C uses input_notify; a text heartbeat
/// lands if the turn is still going after 90s (no message-edit on QQ).
pub fn spawn_live_presence(
    ep: Option<ChannelEndpoint>,
    env: NativePayload,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let kind = ep
            .as_ref()
            .map(|e| e.kind.as_str())
            .unwrap_or(env.channel.as_str());
        if kind != "qq" {
            std::future::pending::<()>().await;
            return;
        }
        if let Err(e) = super::qq::send_typing(ep.as_ref(), &env).await {
            eprintln!("hyper qq typing: {e}");
        }
        let start = Instant::now();
        let mut last_hb: Option<Instant> = None;
        loop {
            tokio::time::sleep(QQ_TYPING_REFRESH).await;
            if let Err(e) = super::qq::send_typing(ep.as_ref(), &env).await {
                eprintln!("hyper qq typing: {e}");
            }
            let elapsed = start.elapsed();
            let due = match last_hb {
                None => elapsed >= QQ_HEARTBEAT_FIRST,
                Some(t) => t.elapsed() >= QQ_HEARTBEAT_EVERY,
            };
            if due {
                let mins = elapsed.as_secs() / 60;
                let text = if mins == 0 {
                    "还在处理中…".to_string()
                } else {
                    format!("还在处理中（已 {mins} 分钟）…")
                };
                let parts = vec![ContentPart::text(text)];
                if let Err(e) = deliver_once(ep.as_ref(), &env, &parts).await {
                    eprintln!("hyper qq heartbeat: {e}");
                }
                last_hb = Some(Instant::now());
            }
        }
    })
}

async fn deliver_once(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    let kind = ep.map(|e| e.kind.as_str()).unwrap_or(env.channel.as_str());
    match kind {
        "telegram" => super::telegram::send(ep, env, parts).await,
        "qq" => super::qq::send(ep, env, parts).await,
        "wechat" => super::wechat::send(ep, env, parts).await,
        "wecom" => super::wecom::send(ep, env, parts).await,
        "dingtalk" => super::dingtalk::send(ep, env, parts).await,
        "feishu" => super::feishu::send(ep, env, parts).await,
        _ => post_webhook(ep, env, parts).await,
    }
}

async fn post_webhook(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    let url = env
        .reply_url()
        .or_else(|| ep.and_then(|e| nonempty(&e.reply_url).map(|s| s.to_string())))
        .or_else(|| ep.and_then(|e| e.extra.get("reply_url").cloned()));
    let Some(url) = url else {
        return Ok(());
    };
    let body = json!({
        "channel": env.channel,
        "sender_id": env.sender_id,
        "session_id": env.session_id,
        "to_handle": env.chat_id(),
        "content_parts": parts,
        "text": parts_to_text(parts),
        "meta": env.meta,
    });
    let client = crate::llm_http::env_aware_client(20, &url)?;
    let mut req = client.post(&url).json(&body);
    if let Some(secret) = ep.and_then(|e| nonempty(&e.secret).map(|s| s.to_string())) {
        req = req.header("X-Q38-Token", secret);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(crate::error::Error::msg(format!(
            "channel outbound {} {}",
            resp.status(),
            url
        )));
    }
    let _ = resp;
    Ok(())
}

pub fn outbound_notification(env: &NativePayload, parts: &[ContentPart]) -> Value {
    json!({
        "channel": env.channel,
        "sender_id": env.sender_id,
        "session_id": env.session_id,
        "to_handle": env.chat_id(),
        "content_parts": parts,
        "text": parts_to_text(parts),
        "meta": env.meta,
    })
}

fn nonempty(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_parts_get_a_placeholder() {
        let filled = outbound_parts(&[]);
        assert_eq!(filled.len(), 1);
        assert_eq!(filled[0].as_text(), Some(EMPTY_REPLY));
        let keep = vec![ContentPart::text("hi")];
        let out = outbound_parts(&keep);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].as_text(), Some("hi"));
    }

    #[test]
    fn retries_then_succeeds() {
        let mut n = 0u32;
        let r = with_attempts(3, || {
            n += 1;
            if n < 3 {
                Err("fail")
            } else {
                Ok("ok")
            }
        });
        assert_eq!(r, Ok("ok"));
        assert_eq!(n, 3);
    }

    #[test]
    fn retries_exhaust() {
        let mut n = 0u32;
        let r: std::result::Result<(), &str> = with_attempts(3, || {
            n += 1;
            Err("nope")
        });
        assert_eq!(r, Err("nope"));
        assert_eq!(n, 3);
    }
}
