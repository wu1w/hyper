//! Working-window compact: soft/hard thresholds, local archive, official xAI compact.

use super::{
    over_hard_threshold, over_soft_threshold, should_compact_follow_up, should_compact_mid_turn,
    Agent, Completer,
};
use crate::family::Family;
use crate::media::MediaKind;
use crate::session::{compact_messages, derive_messages, plan_compact, SessionEvent};
use crate::template::{render, ChatMessage, RenderOpts};
use crate::tokenize::count_tokens;
use crate::tools_schema::strip_recall;
use serde_json::Value;

/// Painted into the think panel so a follow-up compact is not silent "等待模型".
pub(crate) const PREPARE_CONTEXT_NOTE: &str = "正在整理上下文…\n";

impl<C: Completer> Agent<C> {
    pub fn official_compaction(&self) -> Option<&crate::session::OfficialCompaction> {
        self.official_compaction.as_ref()
    }

    pub(crate) async fn compact_if_needed(&mut self) -> Option<String> {
        if self.working_window == 0 {
            return None;
        }
        if self.over_soft_window() || self.mid_turn_computer_use() {
            for _ in 0..2 {
                if !self.apply_compact_pass() {
                    break;
                }
                if !self.over_soft_window() && !self.mid_turn_computer_use() {
                    break;
                }
            }
        }
        let n = self.prefix_tokens_gate() as u64;
        let _ = self.try_official_compact(n).await;
        if !self.over_soft_window() {
            return None;
        }
        // Already wrapped this turn. Local compact had its chance; a second
        // hard return would tombstone the live reply (`finish(last_spoken,
        // None)` with empty text). Keep going — step/wall gates still bound
        // the loop.
        if self.physics_nudged {
            return None;
        }
        let hf = self.prefix_tokens_accurate().await;
        let n = match hf {
            Some(n) => n,
            None => return None,
        };
        if !over_hard_threshold(n, self.generation_reserve, self.working_window) {
            return None;
        }
        Some(format!(
            "budget:context ({n} prefix + {} reserve > {} window)",
            self.generation_reserve, self.working_window
        ))
    }

    /// Archive previous turns at the start of a follow-up user message so a
    /// finished long turn is not replayed as a cold prefill.
    ///
    /// Do not Jinja+HF-tokenize the fat transcript first, and do not POST it
    /// to `/v1/responses/compact` (120s). Both sit in front of the first hop
    /// with no thinking tokens. Local archive first; official compact only
    /// on the already-shrunk remainder.
    pub(crate) async fn compact_at_user_turn(&mut self) {
        if self.working_window == 0 {
            return;
        }
        if !self.can_apply_compact() {
            return;
        }
        if !should_compact_follow_up(
            self.prefix_tokens_gate(),
            self.generation_reserve,
            self.working_window,
            self.compact_ratio,
            live_tool_count(&self.messages),
            live_image_count(&self.messages),
        ) {
            return;
        }
        self.signal_preparing();
        tokio::task::yield_now().await;
        if !self.apply_compact_pass() {
            return;
        }
        let n = self.prefix_tokens_gate() as u64;
        let _ = self.try_official_compact(n).await;
    }

    fn signal_preparing(&self) {
        self.note("[compact] preparing follow-up context");
        if let Some(sink) = self.live_sink() {
            sink.reasoning(PREPARE_CONTEXT_NOTE);
        }
    }

    fn can_apply_compact(&self) -> bool {
        if let Some(log) = &self.log {
            plan_compact(log.events()).is_some()
        } else {
            compact_messages(&self.messages).is_some()
        }
    }

    fn prefix_tokens_gate(&self) -> u32 {
        let keep_reasoning = self
            .completer
            .prefix_meter()
            .map(|(f, _)| meter_keeps_reasoning(f))
            .unwrap_or(false);
        estimate_prefix_tokens(&self.messages, &self.tools, keep_reasoning)
    }

    fn mid_turn_computer_use(&self) -> bool {
        should_compact_mid_turn(
            live_computer_use_count(&self.messages),
            live_image_count(&self.messages),
        )
    }

    /// POST `/v1/responses/compact` when this session is on xAI/cli-chat-proxy.
    /// On success stores the opaque item and still applies local archive compact
    /// so Chat Completions stays bounded until ResponsesCompleter consumes it.
    pub(crate) async fn try_official_compact(&mut self, prompt_tokens: u64) -> bool {
        let Some((base, key)) = self.xai_compact.clone() else {
            return false;
        };
        if live_tool_count(&self.messages) >= super::TURN_START_COMPACT_TOOLS {
            return false;
        }
        if !crate::session::should_official_compact(
            prompt_tokens,
            self.working_window,
            self.compact_ratio,
        ) {
            return false;
        }
        match crate::session::run_official_compact(&base, &key, &self.messages).await {
            Ok(item) => {
                self.note(&format!(
                    "[compact] official id={} blob={}",
                    item.id,
                    item.debug_blob()
                ));
                self.official_compaction = Some(item.clone());
                self.completer
                    .set_official_compaction(self.official_compaction.clone());
                let _ = self.apply_compact_pass();
                true
            }
            Err(_) => {
                if prompt_tokens > crate::session::PRICE_CLIFF_TOKENS {
                    self.note("[compact] official failed; local archive");
                    let _ = self.apply_compact_pass();
                    true
                } else {
                    false
                }
            }
        }
    }

    pub(crate) fn apply_compact_pass(&mut self) -> bool {
        if !self.try_compact() {
            return false;
        }
        let skip = self.messages.iter().filter(|m| m.role != "system").count();
        self.completer.set_compaction_skip(skip);
        self.after_compact();
        let tools = strip_recall(&mut self.tools);
        if tools {
            self.note("[compact] cache_invalidated=compact,tools");
        } else {
            self.note("[compact] cache_invalidated=compact");
        }
        true
    }

    pub(crate) fn over_soft_window(&self) -> bool {
        over_soft_threshold(
            self.prefix_tokens_gate(),
            self.generation_reserve,
            self.working_window,
            self.compact_ratio,
        )
    }

    /// HuggingFace encode (sync). Prefer [`Self::prefix_tokens_accurate`] on
    /// the live loop so the tokenizer does not block the async worker.
    #[allow(dead_code)]
    pub(crate) fn prefix_tokens(&self) -> Option<u32> {
        let (family, kwargs) = self.completer.prefix_meter()?;
        let metered = meter_messages(&self.messages, meter_keeps_reasoning(family));
        let tools = if self.tools.is_empty() {
            None
        } else {
            Some(self.tools.as_slice())
        };
        let rendered = render(&RenderOpts {
            family,
            messages: &metered,
            tools,
            add_generation_prompt: true,
            kwargs,
        })
        .ok()?;
        count_tokens(family, &rendered.text).ok()
    }

    /// HuggingFace encode off the async worker. Used only for the hard
    /// `budget:context` fail, not the every-hop soft gate.
    async fn prefix_tokens_accurate(&self) -> Option<u32> {
        let (family, kwargs) = self.completer.prefix_meter()?;
        let metered = meter_messages(&self.messages, meter_keeps_reasoning(family));
        let tools = if self.tools.is_empty() {
            None
        } else {
            Some(self.tools.as_slice())
        };
        let rendered = render(&RenderOpts {
            family,
            messages: &metered,
            tools,
            add_generation_prompt: true,
            kwargs,
        })
        .ok()?;
        let text = rendered.text;
        tokio::task::spawn_blocking(move || count_tokens(family, &text).ok())
            .await
            .ok()
            .flatten()
    }

    pub(crate) fn try_compact(&mut self) -> bool {
        if self.log.is_some() {
            let plan = self.log.as_ref().and_then(|log| plan_compact(log.events()));
            let Some(plan) = plan else {
                return false;
            };
            if let Some(mem) = &self.memory {
                let _ =
                    mem.write_compact_note(&self.session_id, plan.until_seq, &plan.archive_body());
            }
            self.log_event(SessionEvent::compact(plan));
            self.messages = derive_messages(self.log.as_ref().unwrap().events());
            true
        } else if let Some((_plan, msgs)) = compact_messages(&self.messages) {
            self.messages = msgs;
            crate::sticky::stub_expired_notes(&mut self.messages);
            true
        } else {
            false
        }
    }

    pub(crate) fn after_compact(&mut self) {
        self.handler.reset_repeat(&self.session_id);
        self.observed_paths.clear();
        self.read_paths.clear();
    }
}

fn meter_keeps_reasoning(family: Family) -> bool {
    !matches!(family, Family::Grok46)
}

fn live_tool_count(messages: &[ChatMessage]) -> usize {
    messages.iter().filter(|m| m.role == "tool").count()
}

fn live_image_count(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|m| {
            m.parts
                .iter()
                .filter(|p| p.kind == MediaKind::Image)
                .count()
        })
        .sum()
}

fn live_computer_use_count(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .filter(|c| {
            let name = c
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .or_else(|| c.get("name").and_then(|n| n.as_str()))
                .unwrap_or("");
            crate::tools_schema::dispatch_name(name) == "computeruse"
        })
        .count()
}

/// Cheap compact gate. Skips HuggingFace encode and does not walk data-URI
/// payloads. English/JSON ≈ 4 bytes/token; CJK still crosses 120k on a 60-tool
/// fold. Grok Responses does not replay `reasoning_content`.
pub(crate) fn estimate_prefix_tokens(
    messages: &[ChatMessage],
    tools: &[Value],
    keep_reasoning: bool,
) -> u32 {
    let mut bytes = 0usize;
    for m in messages {
        if let Some(c) = &m.content {
            bytes += charged_text_len(c);
        }
        if keep_reasoning {
            if let Some(r) = &m.reasoning_content {
                bytes += charged_text_len(r);
            }
        }
        if let Some(calls) = &m.tool_calls {
            bytes += calls.iter().map(|v| v.to_string().len()).sum::<usize>();
        }
        for p in &m.parts {
            bytes += if p.url.starts_with("data:") {
                1024
            } else {
                p.url.len().min(256)
            };
        }
    }
    for t in tools {
        bytes += t.to_string().len();
    }
    (bytes / 4) as u32
}

fn charged_text_len(s: &str) -> usize {
    if s.starts_with("data:") {
        return 1024;
    }
    if s.len() > 16_384 && (s.contains("data:image") || s.contains(";base64,")) {
        2048
    } else {
        s.len()
    }
}

fn meter_messages(messages: &[ChatMessage], keep_reasoning: bool) -> Vec<ChatMessage> {
    messages
        .iter()
        .map(|m| ChatMessage {
            role: m.role.clone(),
            content: m.content.as_ref().map(|s| clip_inline_data(s)),
            reasoning_content: if keep_reasoning {
                m.reasoning_content.clone()
            } else {
                None
            },
            tool_calls: m.tool_calls.clone(),
            name: m.name.clone(),
            tool_call_id: m.tool_call_id.clone(),
            parts: m.parts.iter().map(stub_media_part).collect(),
        })
        .collect()
}

fn stub_media_part(p: &crate::media::MediaPart) -> crate::media::MediaPart {
    crate::media::MediaPart {
        kind: p.kind,
        mime: p.mime.clone(),
        url: if p.url.starts_with("data:") {
            "data:image/png;base64,xx".into()
        } else {
            p.url.clone()
        },
    }
}

fn clip_inline_data(s: &str) -> String {
    if s.starts_with("data:") || (s.len() > 16_384 && s.contains(";base64,")) {
        s.chars().take(2048).collect()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod estimate_tests {
    use super::*;
    use crate::media::MediaPart;

    #[test]
    fn estimate_skips_data_uri_payload() {
        let mut msg = ChatMessage::tool("1", "screenshot ok");
        msg.parts = vec![MediaPart::image_url(format!(
            "data:image/png;base64,{}",
            "A".repeat(2_000_000)
        ))];
        let n = estimate_prefix_tokens(std::slice::from_ref(&msg), &[], false);
        assert!(
            n < 2_000,
            "data URI must not dominate the compact gate: {n}"
        );
    }

    #[test]
    fn estimate_counts_folded_tool_text() {
        let msg = ChatMessage::tool("1", "x".repeat(12_000));
        let n = estimate_prefix_tokens(&[msg], &[], false);
        assert!(n > 2_000, "{n}");
        assert!(n < 6_000, "{n}");
    }

    #[test]
    fn estimate_ignores_grok_reasoning() {
        let mut msg = ChatMessage::assistant("ok");
        msg.reasoning_content = Some("t".repeat(500_000));
        let n = estimate_prefix_tokens(&[msg], &[], false);
        assert!(n < 100, "grok think is not on the wire: {n}");
    }

    #[test]
    fn computer_use_count_sees_model_facing_name() {
        let msg = ChatMessage::assistant_tools(
            None,
            vec![serde_json::json!({
                "id": "c1",
                "type": "function",
                "function": {"name": "ComputerUse", "arguments": "{\"action\":\"screenshot\"}"}
            })],
        );
        assert_eq!(live_computer_use_count(std::slice::from_ref(&msg)), 1);
        let read = ChatMessage::assistant_tools(
            None,
            vec![serde_json::json!({
                "id": "c2",
                "type": "function",
                "function": {"name": "Read", "arguments": "{\"path\":\"a.rs\"}"}
            })],
        );
        assert_eq!(live_computer_use_count(std::slice::from_ref(&read)), 0);
    }
}
