//! User-turn ReAct loop: run / drive / finish, dump detection, effort, logging.

use serde_json::Value;

use super::dispatch::{
    normalize_tool_calls, observed_from_messages, openai_stored, openai_tool_calls,
};
use super::{Agent, AgentOutcome, Completer, ModelTurn};
use crate::channel::take_steer;
use crate::error::Result;
use crate::paw_loop::{fs_tool_path, GateCtx, GateDecision, ToolFingerprint};
use crate::session::{PolicyReason, SessionEvent};
use crate::sticky;
use crate::template::{is_hidden_user_text, wrap_tool_response, ChatMessage};
use crate::tool_calls::ToolState;

impl<C: Completer> Agent<C> {
    pub async fn run(&mut self, prompt: &str) -> Result<AgentOutcome> {
        self.run_message(ChatMessage::user(prompt)).await
    }

    /// User turn that may carry image/video/audio parts.
    pub async fn run_message(&mut self, msg: ChatMessage) -> Result<AgentOutcome> {
        let raw = msg.text().to_string();
        let (forced_skill, text) = sticky::split_skill_prefix(&raw);
        let (forced_mcp, text) = sticky::split_mcp_prefix(&text);
        let mut msg = msg;
        msg.content = Some(text.clone());
        // 用户附带的媒体与 tool 侧同样落盘，否则 resume 重建后丢图。
        let stored = self.persist_turn_media(&msg.parts);
        let stubbed = sticky::stub_expired_notes(&mut self.messages);
        self.note_stubbed(stubbed);
        self.messages.push(msg);
        self.log_event(SessionEvent::user(text.clone()).with_media(stored));
        self.compact_at_user_turn().await;
        self.inject_notes(&text, forced_skill.as_deref(), forced_mcp.as_deref());
        self.inject_window_overlay_note();
        self.drive().await
    }

    /// Drive an already-hydrated transcript (last message is the live user).
    pub async fn drive(&mut self) -> Result<AgentOutcome> {
        // Per user turn, not per Agent lifetime. Sidecar/CLI construct a new
        // Agent each RPC, but in-process reuse (TUI, soak, channels) must not
        // inherit iteration/timeout/doom from the previous prompt.
        self.handler.reset_turn(&self.session_id);
        self.physics_nudged = false;
        self.last_spoken = None;
        self.tool_evidence.clear();
        self.read_paths.clear();
        self.channel_files.clear();
        self.observed_paths = observed_from_messages(&self.messages, &self.workspace);
        let user = self.last_real_user().to_string();
        self.edit_guard.reset_turn(&user);
        self.oracle_cmd = None;
        self.snapshot_test_baseline().await;
        self.start_code_index();
        let mut steps = 0u32;
        let mut prompt_tokens = 0u64;
        let mut completion_tokens = 0u64;
        let mut parse_retries = 0u32;
        let mut first_hop = true;

        loop {
            self.drain_background();
            if self.cancel.is_cancelled() {
                return self.finish(String::new(), Some("aborted".into()), steps);
            }
            // Pending gate TERMINATE fires before the next model call.
            if let Some(reason) = self.pending_stop.take() {
                let text = self.last_spoken.clone().unwrap_or_default();
                if reason.is_empty() || is_physics_stop(&reason) {
                    if !reason.is_empty() {
                        self.note(&reason);
                    }
                    return self.finish(text, None, steps);
                }
                self.note(&reason);
                return self.finish(text, Some(reason), steps);
            }

            if let Some(reason) = self.compact_if_needed().await {
                self.note(&reason);
            }

            if !first_hop {
                self.settle_code_index().await;
            }
            first_hop = false;

            let tools_owned = self.tools.clone();
            let tools = if tools_owned.is_empty() {
                None
            } else {
                Some(tools_owned.as_slice())
            };

            self.arm_speculate();
            let completed = self.complete_or_abort(tools).await;
            self.completer.set_speculate(None);
            let Some(mut turn) = completed? else {
                self.drop_speculate();
                return self.finish(String::new(), Some("aborted".into()), steps);
            };
            steps += 1;
            prompt_tokens += turn.prompt_tokens;
            completion_tokens += turn.completion_tokens;

            if turn.watchdog_hit {
                // A cap hit is evidence of a runaway trajectory, not evidence
                // that thinking itself should be disabled. Give the model one
                // concise side observation and more room to choose a course.
                self.note("[watchdog] think cap; soft nudge and one roomy retry");
                self.drop_speculate();
                self.arm_speculate();
                let widened = self.retry_with_runaway_room(tools).await;
                self.completer.set_speculate(None);
                if self.cancel.is_cancelled() {
                    self.drop_speculate();
                    return self.finish(String::new(), Some("aborted".into()), steps);
                }
                match widened {
                    Some(t) => {
                        steps += 1;
                        prompt_tokens += t.prompt_tokens;
                        completion_tokens += t.completion_tokens;
                        if !t.watchdog_hit || !t.content.is_empty() || !t.tool_calls.is_empty() {
                            turn = t;
                        }
                    }
                    None => {}
                }
                if turn.watchdog_hit && turn.content.is_empty() && turn.tool_calls.is_empty() {
                    self.drop_speculate();
                    self.mark_clean();
                    return self.finish(self.last_spoken.clone().unwrap_or_default(), None, steps);
                }
            }

            if !turn.reasoning.is_empty()
                && self.print
                && !self.stdio.think_streamed()
                && turn.tool_calls.is_empty()
            {
                eprintln!("[think]\n{}", turn.reasoning.trim());
            }

            if turn.parse_fail {
                self.effort.note_parse_fail();
                self.sync_effort(PolicyReason::Upgrade);
                parse_retries += 1;
                self.note("[parse] retry");
                self.drop_speculate();
                if parse_retries >= self.parse_stop_after {
                    return self.finish(self.last_spoken.clone().unwrap_or_default(), None, steps);
                }
                continue;
            }

            self.push_assistant(&turn);
            if turn.tool_calls.is_empty() && crate::stutter::is_substantial_reply(&turn.content) {
                self.last_spoken = Some(turn.content.clone());
            }
            let decision = self.gate_decision(&turn, steps, prompt_tokens, completion_tokens);

            if !turn.tool_calls.is_empty() {
                match &decision {
                    GateDecision::Stop { reason } if is_physics_stop(reason) => {
                        self.note(reason);
                        self.pending_stop = Some(String::new());
                    }
                    GateDecision::Stop { reason } if !reason.is_empty() => {
                        self.pending_stop = Some(reason.clone());
                    }
                    _ => {}
                }
                let calls = std::mem::take(&mut turn.tool_calls);
                self.settle_code_index().await;
                self.execute_tools(calls).await;
                self.drop_speculate();
                self.flush_steer();
                if self.cancel.is_cancelled() {
                    return self.finish(String::new(), Some("aborted".into()), steps);
                }
                continue;
            }

            self.drop_speculate();
            match decision {
                GateDecision::Continue { continuation, .. } => {
                    self.mark_clean();
                    let stop_reason = if continuation.is_empty() {
                        None
                    } else {
                        Some(continuation)
                    };
                    return self.finish(turn.content, stop_reason, steps);
                }
                GateDecision::Stop { reason } => {
                    self.mark_clean();
                    if is_physics_stop(&reason) {
                        self.note(&reason);
                        return self.finish(turn.content, None, steps);
                    }
                    let stop_reason = if reason.is_empty() {
                        None
                    } else {
                        Some(reason)
                    };
                    return self.finish(turn.content, stop_reason, steps);
                }
                // Handler swallows per-gate Bypass; keep this arm so a future
                // handler contract change cannot panic the loop.
                GateDecision::Bypass => {
                    self.mark_clean();
                    return self.finish(turn.content, None, steps);
                }
            }
        }
    }

    pub(crate) fn push_assistant(&mut self, turn: &ModelTurn) {
        let tool_calls = if turn.tool_calls.is_empty() {
            None
        } else {
            Some(
                turn.raw_tool_calls
                    .as_ref()
                    .map(|raw| normalize_tool_calls(raw))
                    .unwrap_or_else(|| openai_tool_calls(&turn.tool_calls)),
            )
        };
        let reasoning = if tool_calls.is_some() {
            None
        } else {
            empty_to_none(&turn.reasoning)
        };
        let content = if tool_calls.is_none() {
            Some(turn.content.clone())
        } else {
            // Cursor: tool hops keep visible text empty — persist that, or
            // grok-4.6 continues the essay on the next hop.
            None
        };
        let stored = self.persist_turn_media(&turn.media);
        let mut msg = ChatMessage::assistant_reply(content.clone(), reasoning, tool_calls);
        msg.parts = stored
            .iter()
            .filter_map(|m| {
                let kind = crate::media::MediaKind::parse(&m.kind)?;
                Some(crate::media::MediaPart {
                    kind,
                    mime: m.mime.clone(),
                    url: m.url.clone(),
                })
            })
            .collect();
        self.messages.push(msg);
        self.log_event(
            SessionEvent::assistant_usage(
                content.unwrap_or_default(),
                turn.reasoning.clone(),
                if turn.tool_calls.is_empty() {
                    None
                } else {
                    Some(openai_stored(&turn.tool_calls))
                },
                turn.prompt_tokens,
                turn.completion_tokens,
                turn.cached_tokens,
                turn.decode_tok_s,
            )
            .with_media(stored),
        );
    }

    pub(crate) fn persist_turn_media(
        &self,
        parts: &[crate::media::MediaPart],
    ) -> Vec<crate::session::StoredMedia> {
        let mut out = Vec::new();
        for p in parts {
            if let Some((mime, bytes)) = crate::media::decode_data_uri(&p.url)
                .or_else(|| crate::media::decode_image_payload(&p.url))
            {
                if let Some(rel) =
                    crate::media::persist_image_file(self.workspace.root(), &bytes, &mime)
                {
                    out.push(crate::session::StoredMedia {
                        kind: "image".into(),
                        mime,
                        url: rel,
                    });
                    continue;
                }
            }
            out.push(crate::session::StoredMedia {
                kind: p.kind.as_str().into(),
                mime: p.mime.clone(),
                url: p.url.clone(),
            });
        }
        out
    }

    pub(crate) fn wire_messages(&self) -> Vec<ChatMessage> {
        let mut msgs = self.messages.clone();
        crate::media::retain_referenced_media(&mut msgs);
        crate::office_edit::redact_stale_writes(&mut msgs);
        crate::media::inline_workspace_media(self.workspace.root(), &mut msgs, 12 * 1024 * 1024);
        msgs
    }

    pub(crate) fn flush_steer(&mut self) {
        for note in take_steer(&self.steer) {
            self.messages.push(ChatMessage::user(note.clone()));
            self.log_event(SessionEvent::user(note));
        }
    }

    pub(crate) fn drain_background(&mut self) {
        for (name, response) in self.coordinator.take_finished() {
            let status = match response.state {
                ToolState::Success => "finished",
                ToolState::Error => "failed",
                ToolState::Interrupted => "interrupted",
            };
            self.note(&format!("[background {name} {status}]"));
            // The foreground hop already posted "running in background" as
            // function_call_output. A second output for the same call_id is
            // illegal on Responses, and a hidden user note makes grok recap
            // stdout. The result stays on AwaitShell / bgwait.
        }
    }

    pub(crate) fn push_hidden_user(&mut self, text: impl AsRef<str>) {
        let wrapped = wrap_tool_response(text.as_ref());
        self.messages.push(ChatMessage::user(wrapped.clone()));
        self.log_event(SessionEvent::user(wrapped));
    }

    pub(crate) fn remember_tool_output(&mut self, text: &str) {
        let t = text.trim();
        if t.is_empty() || t.contains("running in background") {
            return;
        }
        if t.chars().count() < 80 {
            return;
        }
        if !self.tool_evidence.is_empty() {
            self.tool_evidence.push('\n');
        }
        self.tool_evidence.push_str(t);
        const CAP: usize = 16_384;
        if self.tool_evidence.len() > CAP {
            let mut i = self.tool_evidence.len() - CAP;
            while i < self.tool_evidence.len() && !self.tool_evidence.is_char_boundary(i) {
                i += 1;
            }
            self.tool_evidence = self.tool_evidence[i..].to_string();
        }
    }

    pub(crate) fn gate_decision(
        &self,
        turn: &ModelTurn,
        steps: u32,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) -> GateDecision {
        let fingerprints: Vec<ToolFingerprint> = turn
            .tool_calls
            .iter()
            .map(|c| {
                ToolFingerprint::new(&c.name, &c.arguments.to_string())
                    .with_path(fs_tool_path(&c.name, &c.arguments))
            })
            .collect();
        let names: Vec<String> = turn.tool_calls.iter().map(|c| c.name.clone()).collect();
        let mut ctx = GateCtx::new(&self.session_id);
        ctx.iteration = steps;
        ctx.prompt_tokens = turn.prompt_tokens;
        ctx.completion_tokens = turn.completion_tokens;
        ctx.tokens_used = prompt_tokens + completion_tokens;
        ctx.tool_names = &names;
        ctx.fingerprints = &fingerprints;
        ctx.last_tool = fingerprints.last();
        self.handler.run(&ctx)
    }

    /// After a true think-cap hit, preserve the model's selected reasoning mode
    /// and give it one wider retry. The hidden trajectory note is injected by
    /// the caller; a second cap hit is a hard resource exhaustion, not a signal
    /// to switch the model off thinking.
    pub(crate) async fn retry_with_runaway_room(
        &self,
        tools: Option<&[Value]>,
    ) -> Option<ModelTurn> {
        let Some(prev) = self.completer.policy() else {
            let retry = self.complete_resilient(tools).await.ok().flatten();
            return retry.filter(|t| !t.watchdog_hit && !t.parse_fail);
        };
        if !prev.enabled {
            return None;
        }
        let mut raised = prev.clone();
        raised.max_think_tokens = raised.max_think_tokens.max(NO_TOOL_THINK_FLOOR);
        raised.raise_generation_cap(raised.max_think_tokens + NO_TOOL_ANSWER_RESERVE);
        self.completer.set_policy(raised);
        let retry = self.complete_resilient(tools).await.ok().flatten();
        self.completer.set_policy(prev);
        retry.filter(|t| !t.watchdog_hit && !t.parse_fail)
    }

    fn arm_speculate(&mut self) {
        let slot = super::SpeculativeSlot::new(self.speculate_ctx());
        self.speculate = Some(slot.clone());
        self.completer.set_speculate(Some(slot));
    }

    fn drop_speculate(&mut self) {
        self.completer.set_speculate(None);
        if let Some(slot) = self.speculate.take() {
            slot.abort();
        }
    }

    pub(crate) fn last_real_user(&self) -> &str {
        self.messages
            .iter()
            .rev()
            .filter(|m| m.role == "user")
            .find(|m| !is_hidden_user_text(m.content.as_deref().unwrap_or("")))
            .and_then(|m| m.content.as_deref())
            .unwrap_or("")
    }

    pub(crate) fn mark_clean(&mut self) {
        if self.effort.note_clean_step() {
            self.sync_effort(PolicyReason::Downgrade);
        }
    }

    pub(crate) fn sync_effort(&mut self, reason: PolicyReason) {
        let p = self.effort.policy().clone();
        if self.last_policy == p {
            return;
        }
        self.completer.set_policy(p.clone());
        self.log_event(SessionEvent::policy(p.clone(), reason));
        self.last_policy = p;
    }

    pub(crate) fn log_event(&mut self, event: SessionEvent) {
        // Children keep their own jsonl. Do not mix nested tools/tokens into the
        // parent activity stream — the console opens a Task card to view them.
        if self.child.is_some() {
            if event.is_ephemeral() {
                return;
            }
            if let Some(log) = self.log.as_mut() {
                let _ = log.append(event);
            }
            return;
        }
        let forward = !matches!(event, SessionEvent::User(_));
        if event.is_ephemeral() {
            if forward {
                if let Some(sink) = &self.emit {
                    sink.append(event);
                }
            }
            return;
        }
        if forward {
            if let Some(sink) = &self.emit {
                sink.append(event.clone());
            }
        }
        if let Some(log) = self.log.as_mut() {
            let _ = log.append(event);
        }
    }

    pub(crate) fn finish(
        &mut self,
        text: String,
        stop_reason: Option<String>,
        steps: u32,
    ) -> Result<AgentOutcome> {
        self.drop_speculate();
        if let Some(h) = self.index_build.take() {
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move {
                    let _ = h.await;
                });
            }
        }
        self.drain_background();
        if stop_reason.as_deref() == Some("aborted") {
            self.coordinator.cancel_background();
        }
        self.mark_clean();
        if self.print {
            self.stdio.close_think();
        }
        let reason = stop_reason.clone().unwrap_or_else(|| "stop".into());
        self.log_event(SessionEvent::stop(reason));
        let text = if stop_reason.as_deref() == Some("aborted") || !text.trim().is_empty() {
            text
        } else {
            self.last_spoken.clone().unwrap_or_default()
        };
        Ok(AgentOutcome {
            text,
            stop_reason,
            steps,
            session_id: self.session_id.clone(),
            pending_steer: take_steer(&self.steer),
            streamed_text: self.stdio.text_streamed(),
            channel_files: std::mem::take(&mut self.channel_files),
        })
    }
    pub(crate) fn note(&self, line: &str) {
        if self.print {
            self.stdio.close_think();
            eprintln!("{line}");
        }
    }

    /// sticky 卡原位替换会击穿前缀缓存，与 compact 路径同风格留一条
    /// 观测线（stderr debug，不进模型上下文）。
    pub(crate) fn note_stubbed(&self, n: usize) {
        if n > 0 {
            self.note(&format!("[sticky] cache_invalidated=stub n={n}"));
        }
    }
}

pub(crate) fn empty_to_none(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Live failure at 2048 (M002, 7^222 mod 1000): the derivation overran the cap
/// at temp 1.0. dsh on the same weights finished with more room (~7.7k chars)
/// and was correct. A turn's think length is high-variance; the cap's job is to
/// catch true runaways, then give the model one observed, roomy retry rather
/// than silently replacing its reasoning policy.
pub(crate) const NO_TOOL_THINK_FLOOR: u32 = 8192;

/// Generation room reserved past the think floor for the visible answer.
pub(crate) const NO_TOOL_ANSWER_RESERVE: u32 = 4096;

#[cfg(test)]
pub(crate) const THINK_DIVERGENCE_NOTE: &str = "[trajectory] This turn's thinking hit the length budget and may be diverging. \
Compress known facts and open questions first; answer or act if the evidence is enough, else take only the smallest missing step.";

#[cfg(test)]
pub(crate) const PHYSICS_WRAP_NOTE: &str =
    "[trajectory] This turn is near the step, time, or context cap. \
Close with a user-visible conclusion from the evidence you have; do not start a new tool loop.";

#[cfg(test)]
pub(crate) const PARSE_REPAIR_NOTE: &str = "[trajectory] The last hop's tool call did not parse. \
Use a complete native tool call, or give a visible conclusion directly.";

pub(crate) fn is_physics_stop(reason: &str) -> bool {
    reason.starts_with("budget:context")
        || reason.contains("Max iterations")
        || reason.contains("time limit")
        || reason.contains("Token budget")
        || reason.contains("call budget")
}
