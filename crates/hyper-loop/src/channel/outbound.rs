//! Send QwenPaw `content_parts` back to the originating chat.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;

use crate::error::Result;

use super::envelope::{ContentPart, NativePayload};
use super::progress::PASSIVE_REPLY_TTL;
use super::ChannelEndpoint;

const DELIVER_ATTEMPTS: u32 = 3;
/// Hermes qqbot: input_notify lasts 60s, refresh at 50s.
const QQ_TYPING_REFRESH: Duration = Duration::from_secs(50);
/// First visible heartbeat if the turn is still running (QQ cannot edit a bubble).
const QQ_HEARTBEAT_FIRST: Duration = Duration::from_secs(90);
const QQ_HEARTBEAT_EVERY: Duration = Duration::from_secs(180);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeliveryIntent {
    id: String,
    endpoint_id: String,
    env: NativePayload,
    parts: Vec<ContentPart>,
    created_at_ms: u64,
}

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
    parts.to_vec()
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
    if owned.is_empty() {
        return Ok(());
    }
    let mut durable_env = env.clone();
    let delivery_id = delivery_id(&durable_env, &owned);
    durable_env.meta.insert(
        "delivery_id".into(),
        serde_json::Value::String(delivery_id.clone()),
    );
    let endpoint_id = ep
        .map(|e| e.id.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            durable_env
                .meta
                .get("endpoint_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| durable_env.channel.clone());
    let intent = DeliveryIntent {
        id: delivery_id,
        endpoint_id,
        env: durable_env,
        parts: owned,
        created_at_ms: unix_ms(),
    };
    if receipt_path(&intent.id)?.is_file() {
        return Ok(());
    }
    let pending = persist_intent(&intent)?;
    let result = deliver_intent(ep, &intent).await;
    if result.is_ok() {
        commit_receipt(&pending, &intent.id)?;
    }
    result
}

async fn deliver_intent(ep: Option<&ChannelEndpoint>, intent: &DeliveryIntent) -> Result<()> {
    let mut last = None;
    for attempt in 0..DELIVER_ATTEMPTS {
        match deliver_once(ep, &intent.env, &intent.parts).await {
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

/// Replay final replies that were durably accepted but did not reach a receipt
/// before the previous process exited. Progress/typing messages never enter this
/// outbox, so recovery cannot resurrect stale status bubbles.
pub(crate) async fn replay_pending(ep: Option<&ChannelEndpoint>) -> Result<usize> {
    let Some(ep) = ep else { return Ok(0) };
    let pending_dir = outbox_root()?.join("pending");
    if !pending_dir.is_dir() {
        return Ok(0);
    }
    let mut delivered = 0usize;
    for entry in fs::read_dir(&pending_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(intent) = serde_json::from_str::<DeliveryIntent>(&raw) else {
            continue;
        };
        if intent.endpoint_id != ep.id && intent.endpoint_id != ep.kind {
            continue;
        }
        if deliver_intent(Some(ep), &intent).await.is_ok() {
            commit_receipt(&path, &intent.id)?;
            delivered += 1;
        }
    }
    Ok(delivered)
}

async fn deliver_transient(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    let intent = DeliveryIntent {
        id: delivery_id(env, parts),
        endpoint_id: String::new(),
        env: env.clone(),
        parts: outbound_parts(parts),
        created_at_ms: unix_ms(),
    };
    deliver_intent(ep, &intent).await
}

/// After the QQ/WeChat passive-reply window, drop `msg_id` so the send is active.
pub(crate) fn outbound_env(env: &NativePayload, elapsed: Duration) -> NativePayload {
    let mut owned = env.clone();
    if elapsed >= PASSIVE_REPLY_TTL {
        owned.meta.remove("msg_id");
    }
    owned
}

pub async fn deliver_since(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
    started: Instant,
) -> Result<()> {
    let owned = outbound_env(env, started.elapsed());
    deliver(ep, &owned, parts).await
}

/// Permit / AskQuestion choices: native buttons on Telegram and Feishu,
/// numbered text everywhere else.
pub async fn deliver_choices(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    text: &str,
    buttons: &[(String, String)],
    started: Instant,
) -> Result<()> {
    let owned = outbound_env(env, started.elapsed());
    let kind = ep
        .map(|e| e.kind.as_str())
        .unwrap_or(owned.channel.as_str());
    if kind.eq_ignore_ascii_case("telegram") {
        return super::telegram::send_choices(ep, &owned, text, buttons).await;
    }
    if kind.eq_ignore_ascii_case("feishu") {
        return super::feishu::send_choices(ep, &owned, text, buttons).await;
    }
    let parts = reply_text(text);
    deliver(ep, &owned, &parts).await
}

/// Live IM progress (ACK / think / tools). WeCom stream, Telegram/Feishu
/// edit one bubble; webhook posts `progress`+`replace_key` so the receiver
/// can replace one bubble. Other native chats still send a line.
pub async fn deliver_progress_since(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
    started: Instant,
) -> Result<()> {
    let owned = outbound_env(env, started.elapsed());
    let kind = ep
        .map(|e| e.kind.as_str())
        .unwrap_or(owned.channel.as_str());
    if kind.eq_ignore_ascii_case("wecom") {
        super::wecom::send_progress(ep, &owned, parts).await
    } else if kind.eq_ignore_ascii_case("telegram") {
        super::telegram::send_progress(ep, &owned, parts).await
    } else if kind.eq_ignore_ascii_case("feishu") {
        super::feishu::send_progress(ep, &owned, parts).await
    } else if webhook_progress_kind(kind) {
        post_webhook(ep, &owned, &outbound_parts(parts), true).await
    } else {
        deliver_transient(ep, &owned, parts).await
    }
}

fn webhook_progress_kind(kind: &str) -> bool {
    kind.is_empty() || kind.eq_ignore_ascii_case("webhook")
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
        _ => post_webhook(ep, env, parts, false).await,
    }
}

async fn post_webhook(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
    progress: bool,
) -> Result<()> {
    let url = env
        .reply_url()
        .or_else(|| ep.and_then(|e| nonempty(&e.reply_url).map(|s| s.to_string())))
        .or_else(|| ep.and_then(|e| e.extra.get("reply_url").cloned()));
    let Some(url) = url else {
        return Ok(());
    };
    let body = webhook_payload(env, parts, progress);
    let client = crate::llm_http::env_aware_client(20, &url)?;
    let mut req = client.post(&url).json(&body);
    if let Some(id) = env.meta.get("delivery_id").and_then(Value::as_str) {
        req = req.header("Idempotency-Key", id);
    }
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

fn delivery_id(env: &NativePayload, parts: &[ContentPart]) -> String {
    if let Some(id) = env.meta.get("delivery_id").and_then(Value::as_str) {
        return id.to_string();
    }
    let origin = ["message_id", "event_id", "update_id", "msg_id"]
        .into_iter()
        .find_map(|key| env.meta.get(key))
        .map(Value::to_string);
    let Some(origin) = origin else {
        // A transport without an inbound identity cannot safely deduplicate two
        // legitimate identical replies. The generated id remains stable for all
        // retries and durable replay because it is stored in the intent.
        return format!("hyper-{}", uuid::Uuid::new_v4().simple());
    };
    let mut h = Sha256::new();
    h.update(env.channel.as_bytes());
    h.update([0]);
    h.update(env.session_id.as_bytes());
    h.update([0]);
    h.update(env.progress_bubble_key().as_bytes());
    h.update([0]);
    h.update(origin.as_bytes());
    h.update([0]);
    if let Ok(body) = serde_json::to_vec(parts) {
        h.update(body);
    }
    let hex = format!("{:x}", h.finalize());
    format!("hyper-{}", &hex[..32])
}

#[cfg(not(test))]
fn outbox_root() -> Result<PathBuf> {
    Ok(crate::config::Config::home_dir()?
        .join("channels")
        .join("outbox"))
}

#[cfg(test)]
fn outbox_root() -> Result<PathBuf> {
    // Channel unit tests exercise real delivery/retry paths. Keep their durable
    // intents away from the operator's live ~/.grok-hyper outbox.
    Ok(std::env::temp_dir().join(format!("hyper-loop-outbox-test-{}", std::process::id())))
}

fn persist_intent(intent: &DeliveryIntent) -> Result<PathBuf> {
    let dir = outbox_root()?.join("pending");
    fs::create_dir_all(&dir)?;
    set_private_dir(&dir);
    let path = dir.join(format!("{}.json", intent.id));
    if path.is_file() {
        return Ok(path);
    }
    let tmp = dir.join(format!("{}.{}.tmp", intent.id, std::process::id()));
    let body = serde_json::to_vec(intent).map_err(crate::error::Error::msg)?;
    fs::write(&tmp, body)?;
    fs::rename(&tmp, &path)?;
    set_private(&path);
    Ok(path)
}

fn commit_receipt(pending: &Path, id: &str) -> Result<()> {
    let dir = outbox_root()?.join("receipts");
    fs::create_dir_all(&dir)?;
    set_private_dir(&dir);
    let receipt = receipt_path(id)?;
    if pending.exists() {
        fs::rename(pending, &receipt)?;
    }
    set_private(&receipt);
    Ok(())
}

fn receipt_path(id: &str) -> Result<PathBuf> {
    Ok(outbox_root()?.join("receipts").join(format!("{id}.json")))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn set_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(unix)]
fn set_private_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn set_private(_path: &Path) {}

#[cfg(not(unix))]
fn set_private_dir(_path: &Path) {}

pub fn outbound_notification(env: &NativePayload, parts: &[ContentPart]) -> Value {
    webhook_payload(env, parts, false)
}

pub fn webhook_payload(env: &NativePayload, parts: &[ContentPart], progress: bool) -> Value {
    let mut body = json!({
        "channel": env.channel,
        "sender_id": env.sender_id,
        "session_id": env.session_id,
        "to_handle": env.chat_id(),
        "content_parts": parts,
        "text": parts_to_text(parts),
        "meta": env.meta,
    });
    if progress {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("progress".into(), json!(true));
            obj.insert("replace_key".into(), json!(env.progress_bubble_key()));
        }
    }
    body
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
    use serde_json::json;

    #[test]
    fn stale_passive_reply_drops_msg_id() {
        let mut env = NativePayload::default();
        env.meta.insert("msg_id".into(), json!("m1"));
        let fresh = outbound_env(&env, Duration::from_secs(10));
        assert_eq!(fresh.meta.get("msg_id"), Some(&json!("m1")));
        let stale = outbound_env(&env, PASSIVE_REPLY_TTL);
        assert!(stale.meta.get("msg_id").is_none());
    }

    #[test]
    fn empty_parts_stay_empty() {
        assert!(outbound_parts(&[]).is_empty());
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
    fn webhook_progress_payload_has_replace_key() {
        let mut env = NativePayload::text_only("webhook", "hi");
        env.meta.insert("chat_id".into(), json!("c1"));
        env.meta.insert("message_id".into(), json!("m9"));
        let parts = reply_text("思考中");
        let live = webhook_payload(&env, &parts, true);
        assert_eq!(live["progress"], json!(true));
        assert_eq!(live["replace_key"], json!("c1:m9"));
        assert_eq!(live["text"], json!("思考中"));
        let done = webhook_payload(&env, &reply_text("好了"), false);
        assert!(done.get("progress").is_none());
        assert!(done.get("replace_key").is_none());
        assert!(webhook_progress_kind("webhook"));
        assert!(webhook_progress_kind(""));
        assert!(!webhook_progress_kind("qq"));
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

    #[test]
    fn delivery_ids_dedupe_real_inbound_messages_only() {
        let parts = reply_text("same reply");
        let mut identified = NativePayload::text_only("feishu", "question");
        identified.session_id = "s1".into();
        identified.meta.insert("message_id".into(), json!("m1"));
        assert_eq!(
            delivery_id(&identified, &parts),
            delivery_id(&identified, &parts)
        );
        identified.meta.insert("message_id".into(), json!("m2"));
        assert_ne!(
            delivery_id(&identified, &parts),
            delivery_id(
                &NativePayload {
                    meta: serde_json::Map::from_iter([("message_id".into(), json!("m1"))]),
                    ..identified.clone()
                },
                &parts
            )
        );

        let anonymous = NativePayload::text_only("webhook", "question");
        assert_ne!(
            delivery_id(&anonymous, &parts),
            delivery_id(&anonymous, &parts)
        );
    }
}
