//! Official xAI Responses compact (`POST /v1/responses/compact`).
//!
//! `encrypted_content` is opaque: never parse, never log the full blob.
//! Local archive compact (`plan_compact` / `apply_compact`) stays the
//! openai_compat fallback.

use std::fmt;

use serde_json::{json, Value};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::template::ChatMessage;

use super::{apply_compact, plan_compact};

/// Grok 4.6 input price cliff (tokens). Official compact should fire above this
/// even if the working-window ratio has not been crossed.
pub const PRICE_CLIFF_TOKENS: u64 = 200_000;

const DEFAULT_COMPACT_RATIO: f64 = 0.80;
const BLOB_DEBUG_CHARS: usize = 24;
const OFFICIAL_MODEL: &str = "grok-4.6";

/// Opaque compaction item from `POST /v1/responses/compact`.
///
/// Pass [`OfficialCompaction::as_input_item`] back verbatim as the head of the
/// next `/v1/responses` `input` array, then append new turns. Do not parse
/// `encrypted_content`.
#[derive(Clone)]
pub struct OfficialCompaction {
    pub id: String,
    pub model: String,
    /// Full `output` array from the compact response. Pass this verbatim.
    pub output: Vec<Value>,
    encrypted_content: String,
}

impl fmt::Debug for OfficialCompaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OfficialCompaction")
            .field("id", &self.id)
            .field("model", &self.model)
            .field("encrypted_content", &self.debug_blob())
            .finish()
    }
}

impl OfficialCompaction {
    pub fn encrypted_content(&self) -> &str {
        &self.encrypted_content
    }

    /// Single compaction object for the next Responses `input`.
    pub fn as_input_item(&self) -> Value {
        self.output.first().cloned().unwrap_or_else(|| {
            json!({
                "type": "compaction",
                "id": self.id,
                "encrypted_content": self.encrypted_content,
            })
        })
    }

    pub fn debug_blob(&self) -> String {
        truncate_blob(&self.encrypted_content)
    }
}

/// Result of [`compact_for_transport`]: official blob or local archive rewrite.
#[derive(Clone, Debug)]
pub enum TransportCompact {
    Official(OfficialCompaction),
    Local { messages: Vec<ChatMessage> },
}

pub fn clamp_compact_ratio(ratio: f64) -> f64 {
    if ratio.is_finite() {
        ratio.clamp(0.10, 1.0)
    } else {
        DEFAULT_COMPACT_RATIO
    }
}

/// Soft window (`prompt_tokens > working_window * ratio`) or the 200k price cliff.
pub fn should_official_compact(prompt_tokens: u64, window: u32, ratio: f64) -> bool {
    if prompt_tokens > PRICE_CLIFF_TOKENS {
        return true;
    }
    if window == 0 {
        return false;
    }
    (prompt_tokens as f64) > (window as f64) * clamp_compact_ratio(ratio)
}

/// xAI Responses / cli-chat-proxy transport (env or `cfg.server.base_url`).
pub fn is_xai_transport(base_url: &str) -> bool {
    if env_flag_xai() {
        return true;
    }
    let u = base_url.to_ascii_lowercase();
    u.contains("api.x.ai") || u.contains("cli-chat-proxy")
}

fn env_flag_xai() -> bool {
    for key in ["HYPER_TRANSPORT", "XAI_TRANSPORT", "HYPER_XAI_COMPACT"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_ascii_lowercase();
            if matches!(v.as_str(), "xai" | "1" | "true" | "yes" | "on") {
                return true;
            }
        }
    }
    false
}

pub fn compact_url(base_url: &str) -> String {
    let b = base_url.trim().trim_end_matches('/');
    if b.ends_with("/v1") || b.contains("/v1/") {
        format!("{b}/responses/compact")
    } else {
        format!("{b}/v1/responses/compact")
    }
}

/// Convert live ChatMessages into Cursor/xAI Responses `input` items.
///
/// Compact must not freeze the hosted identity: skip `system` (live
/// `instructions` already carry grok-hyper) and wash platform prefix text
/// out of other roles. Do not parse `encrypted_content`.
pub fn messages_to_responses_input(messages: &[ChatMessage]) -> Vec<Value> {
    messages.iter().flat_map(chat_to_input_items).collect()
}

/// One ChatMessage → zero or more Responses input items (`function_call`,
/// `function_call_output`, or a role/content message). Shared with the live
/// `/v1/responses` body so compact and hop-2 stay on the same wire.
pub fn chat_to_input_items(msg: &ChatMessage) -> Vec<Value> {
    match msg.role.as_str() {
        "system" | "" => Vec::new(),
        "tool" => {
            let name = msg.name.as_deref().unwrap_or("");
            if matches!(name, "web_search" | "x_search" | "image_generation") {
                return Vec::new();
            }
            let call_id = msg.tool_call_id.clone().unwrap_or_default();
            if call_id.is_empty() {
                return Vec::new();
            }
            vec![json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": msg.content.clone().unwrap_or_default(),
            })]
        }
        "assistant" => {
            let mut out = Vec::new();
            // `store:false` has no encrypted reasoning to continue. Replaying
            // captured think as summary_text makes Grok restart the same
            // preamble every hop (思考轨迹复读).
            if let Some(calls) = &msg.tool_calls {
                for c in calls {
                    if let Some(item) = openai_call_to_item(c) {
                        out.push(item);
                    }
                }
            }
            let text = crate::platform_prefix::wash_message_content(
                "assistant",
                msg.content.as_deref().unwrap_or(""),
            );
            if !text.trim().is_empty() || !msg.parts.is_empty() || out.is_empty() {
                out.push(message_item("assistant", msg));
            }
            out
        }
        "user" if is_cursor_noise_user(msg) => Vec::new(),
        role => vec![message_item(role, msg)],
    }
}

fn is_cursor_noise_user(msg: &ChatMessage) -> bool {
    let t = msg.content.as_deref().unwrap_or("");
    crate::template::is_hidden_user_text(t) && t.contains("[locate]")
}

fn openai_call_to_item(c: &Value) -> Option<Value> {
    let name = c
        .pointer("/function/name")
        .and_then(|v| v.as_str())
        .or_else(|| c.get("name").and_then(|v| v.as_str()))?;
    let id = c
        .get("id")
        .and_then(|v| v.as_str())
        .or_else(|| c.get("call_id").and_then(|v| v.as_str()))
        .unwrap_or("");
    let args = c
        .pointer("/function/arguments")
        .cloned()
        .or_else(|| c.get("arguments").cloned())
        .unwrap_or(json!({}));
    let args_s = match args {
        Value::String(s) => s,
        other => other.to_string(),
    };
    Some(json!({
        "type": "function_call",
        "name": name,
        "arguments": args_s,
        "call_id": id,
    }))
}

fn message_item(role: &str, msg: &ChatMessage) -> Value {
    let mut text =
        crate::platform_prefix::wash_message_content(role, msg.content.as_deref().unwrap_or(""));
    if role == "user" {
        text = unwrap_qwen_hidden(&text);
    }
    if msg.parts.is_empty() {
        return json!({
            "role": role,
            "content": text,
        });
    }
    // Chat Completions uses `image_url` parts. Responses only accepts
    // input_text / output_text / input_image / input_file / refusal.
    json!({
        "type": "message",
        "role": role,
        "content": responses_message_content(role, &text, &msg.parts),
    })
}

fn responses_message_content(
    role: &str,
    text: &str,
    parts: &[crate::media::MediaPart],
) -> Vec<Value> {
    let text_ty = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    let mut arr = Vec::new();
    if !text.is_empty() {
        arr.push(json!({ "type": text_ty, "text": text }));
    }
    for p in parts {
        match p.kind {
            crate::media::MediaKind::Image => {
                let url = p.url.trim();
                if url.is_empty() {
                    continue;
                }
                arr.push(json!({
                    "type": "input_image",
                    "image_url": url,
                }));
            }
            crate::media::MediaKind::Video | crate::media::MediaKind::Audio => {
                arr.push(json!({
                    "type": text_ty,
                    "text": format!("[{}]", p.kind.as_str()),
                }));
            }
        }
    }
    if arr.is_empty() {
        arr.push(json!({ "type": text_ty, "text": "" }));
    }
    arr
}

/// Qwen Jinja needs `<tool_response>` wraps so `last_query_index` stays on
/// the real user. Cursor / grok-4.6 Responses treats those tags as junk.
pub fn unwrap_qwen_hidden(text: &str) -> String {
    let t = text.trim();
    match t
        .strip_prefix("<tool_response>")
        .and_then(|s| s.strip_suffix("</tool_response>"))
    {
        Some(inner) => inner.trim().to_string(),
        None => text.to_string(),
    }
}

/// Next `/v1/responses` input: `[compaction] + new turns`.
pub fn responses_input_after(
    compaction: &OfficialCompaction,
    new_turns: &[ChatMessage],
) -> Vec<Value> {
    let mut out = vec![compaction.as_input_item()];
    out.extend(messages_to_responses_input(new_turns));
    out
}

/// Pick official compact vs local archive. Official is used when the transport
/// looks like xAI/session; otherwise `plan_compact` / `apply_compact`.
pub async fn compact_for_transport(
    messages: &[ChatMessage],
    cfg: &Config,
    prompt_tokens: u64,
) -> Result<Option<TransportCompact>> {
    let window = cfg.context.working_window;
    let ratio = cfg.context.compact_ratio;
    if let Some((base, key)) = crate::transport::compact_creds(cfg) {
        if !should_official_compact(prompt_tokens, window, ratio) {
            return Ok(None);
        }
        let mut compact_msgs = messages.to_vec();
        crate::media::retain_referenced_media(&mut compact_msgs);
        match run_official_compact(&base, &key, &compact_msgs).await {
            Ok(item) => return Ok(Some(TransportCompact::Official(item))),
            Err(e) => {
                eprintln!(
                    "hyper: official responses/compact failed ({e}); falling back to local archive compact"
                );
            }
        }
    }
    let events = super::events_from_messages(messages);
    let Some(plan) = plan_compact(&events) else {
        return Ok(None);
    };
    Ok(Some(TransportCompact::Local {
        messages: apply_compact(messages, &plan),
    }))
}

pub async fn run_official_compact(
    base_url: &str,
    api_key: &str,
    messages: &[ChatMessage],
) -> Result<OfficialCompaction> {
    let url = compact_url(base_url);
    let input = messages_to_responses_input(messages);
    let body = json!({
        "model": OFFICIAL_MODEL,
        "input": input,
    });
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| Error::Http(e.to_string()))?;
    let mut req = client.post(&url).json(&body);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    if crate::transport::is_session_host(base_url) {
        req = crate::transport::apply_grok_headers(req, crate::transport::GrokTransport::Session);
    }
    let resp = req.send().await.map_err(|e| Error::Http(e.to_string()))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| Error::Http(e.to_string()))?;
    if !status.is_success() {
        return Err(Error::Http(format!(
            "responses/compact {status}: {}",
            clip_err(&text)
        )));
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|e| Error::Http(format!("compact json: {e}")))?;
    parse_official_compact_json(&value)
}

pub fn parse_official_compact_json(value: &Value) -> Result<OfficialCompaction> {
    let output = value
        .get("output")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let item = output.first().cloned().unwrap_or_else(|| value.clone());
    let encrypted = item
        .get("encrypted_content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if encrypted.is_empty() {
        return Err(Error::Http(
            "responses/compact: empty encrypted_content".into(),
        ));
    }
    let id = item
        .get("id")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("id").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let model = value
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(OFFICIAL_MODEL)
        .to_string();
    debug_assert!(
        truncate_blob(&encrypted).chars().count() <= BLOB_DEBUG_CHARS + 32,
        "debug blob helper must stay truncated"
    );
    Ok(OfficialCompaction {
        id,
        model,
        output: if output.is_empty() {
            vec![item]
        } else {
            output
        },
        encrypted_content: encrypted,
    })
}

fn truncate_blob(blob: &str) -> String {
    let n = blob.chars().count();
    if n <= BLOB_DEBUG_CHARS {
        return format!("<{n} chars>");
    }
    let head: String = blob.chars().take(BLOB_DEBUG_CHARS).collect();
    format!("{head}…({n} chars)")
}

fn clip_err(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 240 {
        flat
    } else {
        let head: String = flat.chars().take(239).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_official_compact_math() {
        // 262144 * 0.80 = 209715.2 — 210k is over ratio, 200k is not (but cliff is >200k).
        assert!(!should_official_compact(100_000, 262_144, 0.80));
        assert!(should_official_compact(210_000, 262_144, 0.80));
        assert!(!should_official_compact(200_000, 262_144, 0.80));
        assert!(should_official_compact(200_001, 262_144, 0.80));
        // Price cliff independent of window.
        assert!(should_official_compact(200_001, 0, 0.80));
        assert!(!should_official_compact(50_000, 0, 0.80));
        // Small window: 800 > 1000 * 0.70.
        assert!(should_official_compact(800, 1000, 0.70));
        assert!(!should_official_compact(700, 1000, 0.70));
        // NaN ratio falls back to 0.80.
        assert!(should_official_compact(900, 1000, f64::NAN));
        assert!(!should_official_compact(700, 1000, f64::NAN));
    }

    #[test]
    fn xai_url_and_transport() {
        assert!(is_xai_transport("https://api.x.ai/v1"));
        assert!(is_xai_transport("https://cli-chat-proxy.local/v1"));
        assert!(!is_xai_transport("http://127.0.0.1:8080/v1"));
        assert_eq!(
            compact_url("https://api.x.ai/v1"),
            "https://api.x.ai/v1/responses/compact"
        );
        assert_eq!(
            compact_url("https://api.x.ai"),
            "https://api.x.ai/v1/responses/compact"
        );
    }

    #[test]
    fn parse_compact_keeps_blob_opaque() {
        let v = json!({
            "id": "cmp_01TEST",
            "object": "response.compaction",
            "model": "grok-4.6",
            "output": [{
                "type": "compaction",
                "id": "cmp_01TEST",
                "encrypted_content": "SECRETBLOB-do-not-parse"
            }]
        });
        let c = parse_official_compact_json(&v).unwrap();
        assert_eq!(c.id, "cmp_01TEST");
        assert_eq!(c.encrypted_content(), "SECRETBLOB-do-not-parse");
        let dbg = c.debug_blob();
        assert!(
            !dbg.contains("SECRETBLOB-do-not-parse") || dbg.contains('…') || dbg.contains("chars")
        );
        let next = responses_input_after(&c, &[ChatMessage::user("next")]);
        assert_eq!(next[0]["type"], "compaction");
        assert_eq!(next[0]["encrypted_content"], "SECRETBLOB-do-not-parse");
        assert_eq!(next[1]["role"], "user");
        assert_eq!(next[1]["content"], "next");
    }

    #[test]
    fn compact_input_omits_system_and_hosted_identity() {
        let msgs = vec![
            ChatMessage::system(
                "You are Grok, a helpful and maximally truthful AI built by xAI.\nYou are grok-hyper.",
            ),
            ChatMessage::user("edit notes.md"),
            ChatMessage::assistant(
                "You are Grok, a helpful and maximally truthful AI built by xAI.\nDone.",
            ),
        ];
        let input = messages_to_responses_input(&msgs);
        let blob = serde_json::to_string(&input).unwrap();
        assert!(input.iter().all(|v| v["role"] != "system"), "{blob}");
        assert!(!blob.contains("You are Grok,"), "{blob}");
        assert!(!blob.contains("maximally truthful"), "{blob}");
        assert!(blob.contains("edit notes.md"), "{blob}");
        assert!(blob.contains("Done."), "{blob}");
    }

    #[test]
    fn hidden_user_is_unwrapped_without_qwen_tags() {
        let msgs = vec![
            ChatMessage::user("task"),
            ChatMessage::hidden_user("[trajectory] same tool twice."),
        ];
        let input = messages_to_responses_input(&msgs);
        let blob = serde_json::to_string(&input).unwrap();
        assert_eq!(input.len(), 2, "{blob}");
        assert!(!blob.contains("<tool_response>"), "{blob}");
        assert!(blob.contains("[trajectory] same tool twice."), "{blob}");
    }

    #[test]
    fn tool_calls_become_function_call_items() {
        let msgs = vec![
            ChatMessage::assistant_tools(
                None,
                vec![json!({
                    "id": "c1",
                    "type": "function",
                    "function": {"name": "Read", "arguments": "{\"path\":\"a\"}"}
                })],
            ),
            ChatMessage::tool("c1", "ok"),
        ];
        let input = messages_to_responses_input(&msgs);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["name"], "Read");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "c1");
    }

    #[test]
    fn assistant_reasoning_is_not_replayed_as_summary() {
        let mut msg = ChatMessage::assistant_tools(
            None,
            vec![json!({
                "id": "c1",
                "type": "function",
                "function": {"name": "Read", "arguments": "{\"path\":\"a\"}"}
            })],
        );
        msg.reasoning_content = Some("I should read a first.".into());
        let input = messages_to_responses_input(&[msg]);
        assert!(input.iter().all(|i| i["type"] != "reasoning"), "{input:?}");
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["name"], "Read");
    }

    #[test]
    fn assistant_generated_image_replays_in_responses_input() {
        let mut msg = ChatMessage::assistant("here you go");
        msg.parts = vec![crate::media::MediaPart::image_url(
            "data:image/png;base64,xx",
        )];
        let input = messages_to_responses_input(&[msg]);
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        let content = input[0]["content"].as_array().expect("content array");
        assert!(
            content.iter().any(|p| {
                p["type"] == "input_image" && p["image_url"] == "data:image/png;base64,xx"
            }),
            "{content:?}"
        );
        assert!(
            content
                .iter()
                .any(|p| p["type"] == "output_text" && p["text"] == "here you go"),
            "{content:?}"
        );
        assert!(
            content
                .iter()
                .all(|p| p["type"] != "image_url" && p["type"] != "text"),
            "{content:?}"
        );
    }

    #[test]
    fn user_image_uses_input_image_not_chat_completions_parts() {
        let mut msg = ChatMessage::user("what is this?");
        msg.parts = vec![crate::media::MediaPart::image_url(
            "data:image/jpeg;base64,yy",
        )];
        let input = messages_to_responses_input(&[msg]);
        let content = input[0]["content"].as_array().expect("content array");
        assert!(
            content
                .iter()
                .any(|p| p["type"] == "input_text" && p["text"] == "what is this?"),
            "{content:?}"
        );
        assert!(
            content.iter().any(|p| p["type"] == "input_image"
                && p["image_url"] == "data:image/jpeg;base64,yy"
                && p["image_url"].is_string()),
            "{content:?}"
        );
        assert!(
            content.iter().all(|p| p["type"] != "image_url"),
            "{content:?}"
        );
    }

    #[test]
    fn assistant_image_only_replays_even_with_reasoning() {
        let mut msg = ChatMessage::assistant("");
        msg.reasoning_content = Some("draw it".into());
        msg.parts = vec![crate::media::MediaPart::image_url(
            "data:image/jpeg;base64,yy",
        )];
        let input = messages_to_responses_input(&[msg]);
        assert_eq!(input.len(), 1, "{input:?}");
        assert_eq!(input[0]["type"], "message");
        assert!(
            input[0]["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["type"] == "input_image"),
            "{input:?}"
        );
    }

    #[test]
    fn unreferenced_generated_image_is_dropped_before_responses() {
        let mut shot = ChatMessage::assistant("");
        shot.parts = vec![crate::media::MediaPart::image_url(
            "data:image/jpeg;base64,yy",
        )];
        let mut msgs = vec![
            ChatMessage::user("draw a logo"),
            shot,
            ChatMessage::user("write a ppt skill"),
        ];
        crate::media::retain_referenced_media(&mut msgs);
        let blob = serde_json::to_string(&messages_to_responses_input(&msgs)).unwrap();
        assert!(!blob.contains("input_image"), "{blob}");
        assert!(!blob.contains("data:image/jpeg"), "{blob}");
    }

    #[test]
    fn referenced_image_stays_in_responses_input() {
        let mut shot = ChatMessage::assistant("");
        shot.parts = vec![crate::media::MediaPart::image_url(
            "data:image/jpeg;base64,yy",
        )];
        let mut msgs = vec![
            ChatMessage::user("draw a logo"),
            shot,
            ChatMessage::user("把这张图调亮一点"),
        ];
        crate::media::retain_referenced_media(&mut msgs);
        let blob = serde_json::to_string(&messages_to_responses_input(&msgs)).unwrap();
        assert!(blob.contains("\"type\":\"input_image\""), "{blob}");
        assert!(blob.contains("data:image/jpeg;base64,yy"), "{blob}");
    }

    #[test]
    fn current_attachment_without_mention_stays_in_responses() {
        let mut prior = ChatMessage::assistant("");
        prior.parts = vec![crate::media::MediaPart::image_url(
            "data:image/jpeg;base64,old",
        )];
        let mut user = ChatMessage::user("write a ppt skill");
        user.parts = vec![crate::media::MediaPart::image_url(
            "data:image/png;base64,att",
        )];
        let mut msgs = vec![prior, user];
        crate::media::retain_referenced_media(&mut msgs);
        let blob = serde_json::to_string(&messages_to_responses_input(&msgs)).unwrap();
        assert!(blob.contains("data:image/png;base64,att"), "{blob}");
        assert!(!blob.contains("data:image/jpeg;base64,old"), "{blob}");
    }

    #[test]
    fn host_search_tool_messages_are_not_function_outputs() {
        let mut msg = ChatMessage::tool("server-web_search-xAI", "xAI");
        msg.name = Some("web_search".into());
        assert!(messages_to_responses_input(&[msg]).is_empty());
        let mut img = ChatMessage::tool("server-image_generation-cat", "a cat");
        img.name = Some("image_generation".into());
        assert!(messages_to_responses_input(&[img]).is_empty());
    }
}
