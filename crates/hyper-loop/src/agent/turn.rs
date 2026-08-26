//! User-turn ReAct loop: run / drive / finish, dump detection, effort, logging.

use serde_json::Value;

use super::dispatch::{
    bash_cmd, canon_ws_path, normalize_tool_calls, observed_from_messages, openai_stored,
    openai_tool_calls, write_path_body,
};
use super::{Agent, AgentOutcome, Completer, ModelTurn};
use crate::channel::take_steer;
use crate::echo::strip_greeting_echo;
use crate::error::Result;
use crate::paw_loop::{GateCtx, GateDecision, ToolFingerprint, fs_tool_path};
use crate::session::{PolicyReason, SessionEvent};
use crate::sticky;
use crate::template::{ChatMessage, is_hidden_user_text, wrap_tool_response};
use crate::tool_calls::{ToolCall, ToolState};
use crate::tools_schema::dispatch_name;

impl<C: Completer> Agent<C> {
    pub async fn run(&mut self, prompt: &str) -> Result<AgentOutcome> {
        self.run_message(ChatMessage::user(prompt)).await
    }

    /// User turn that may carry QwenPaw `content_parts` (image/video/audio).
    pub async fn run_message(&mut self, msg: ChatMessage) -> Result<AgentOutcome> {
        let raw = msg.text().to_string();
        let (forced_skill, text) = sticky::split_skill_prefix(&raw);
        let (forced_mcp, text) = sticky::split_mcp_prefix(&text);
        let mut msg = msg;
        msg.content = Some(text.clone());
        // 用户附带的媒体与 tool 侧同样落盘，否则 resume 重建后丢图。
        let stored: Vec<crate::session::StoredMedia> = msg
            .parts
            .iter()
            .map(|p| crate::session::StoredMedia {
                kind: p.kind.as_str().into(),
                mime: p.mime.clone(),
                url: p.url.clone(),
            })
            .collect();
        let stubbed = sticky::stub_expired_notes(&mut self.messages);
        self.note_stubbed(stubbed);
        self.messages.push(msg);
        self.log_event(SessionEvent::user(text.clone()).with_media(stored));
        self.compact_at_user_turn().await;
        self.inject_notes(&text, forced_skill.as_deref(), forced_mcp.as_deref());
        self.inject_window_overlay_note();
        self.inject_locate(&text);
        self.inject_web_hint(&text);
        self.inject_numeric_check_hint(&text);
        self.drive().await
    }

    /// Drive an already-hydrated transcript (last message is the live user).
    pub async fn drive(&mut self) -> Result<AgentOutcome> {
        // Per user turn, not per Agent lifetime. Sidecar/CLI construct a new
        // Agent each RPC, but in-process reuse (TUI, soak, channels) must not
        // inherit iteration/timeout/doom from the previous prompt.
        self.handler.reset_turn(&self.session_id);
        self.stutter_nudged = false;
        self.dump_nudged = false;
        self.physics_nudged = false;
        self.parse_nudged = false;
        self.last_spoken = None;
        self.last_essay = None;
        self.read_paths.clear();
        self.observed_paths = observed_from_messages(&self.messages, &self.workspace);
        let user = self.last_real_user().to_string();
        self.edit_guard.reset_turn(&user);
        self.oracle_cmd = None;
        self.snapshot_test_baseline().await;
        let mut steps = 0u32;
        let mut prompt_tokens = 0u64;
        let mut completion_tokens = 0u64;
        let mut parse_retries = 0u32;

        loop {
            self.drain_background();
            if self.cancel.is_cancelled() {
                return self.finish(String::new(), Some("aborted".into()), steps);
            }
            // QwenPaw: pending gate TERMINATE fires before the next model call.
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
                if self.physics_nudged || self.cursor_wire() {
                    return self.finish(self.last_spoken.clone().unwrap_or_default(), None, steps);
                }
                self.physics_nudged = true;
                self.push_hidden_user(PHYSICS_WRAP_NOTE);
            }

            let tools_owned = self.tools.clone();
            let tools = if tools_owned.is_empty() {
                None
            } else {
                Some(tools_owned.as_slice())
            };

            let Some(mut turn) = self.complete_or_abort(tools).await? else {
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
                if !self.cursor_wire() {
                    self.push_hidden_user(THINK_DIVERGENCE_NOTE);
                }
                let widened = self.retry_with_runaway_room(tools).await;
                if self.cancel.is_cancelled() {
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
                    self.mark_clean();
                    return self.finish(self.last_spoken.clone().unwrap_or_default(), None, steps);
                }
            }

            if !turn.reasoning.is_empty() && self.print && !self.stdio.think_streamed() {
                eprintln!("[think]\n{}", turn.reasoning.trim());
            }

            if turn.parse_fail {
                self.effort.note_parse_fail();
                self.sync_effort(PolicyReason::Upgrade);
                parse_retries += 1;
                self.note("[parse] retry");
                if parse_retries >= self.parse_stop_after {
                    if !self.parse_nudged && !self.cursor_wire() {
                        self.parse_nudged = true;
                        self.push_hidden_user(PARSE_REPAIR_NOTE);
                        continue;
                    }
                    return self.finish(self.last_spoken.clone().unwrap_or_default(), None, steps);
                }
                continue;
            }

            if turn.tool_calls.is_empty() {
                turn.content = strip_greeting_echo(self.last_real_user(), &turn.content);
            }
            if self.low_precision
                && !self.cursor_wire()
                && crate::stutter::is_stutter(&turn.content, &turn.reasoning)
            {
                if !self.stutter_nudged {
                    self.stutter_nudged = true;
                    self.push_hidden_user(crate::stutter::STUTTER_NOTE);
                    continue;
                }
            }
            if let Some(body) = Self::promote_write_reply(&turn) {
                turn.content = body;
            }
            let mut trajectory_note = None;
            let mut dump_hop = false;
            if self.cursor_wire()
                && turn.tool_calls.is_empty()
                && crate::stutter::is_blockquote_heavy(&turn.content)
                && self.last_essay.is_none()
                && self.last_spoken.is_none()
            {
                // First visible hop is a quote recap (usually of the user).
                // Keep the inner text once, not the `>` wall.
                turn.content = crate::stutter::strip_blockquote_prefix(&turn.content);
            }
            let dump_anchor = if self.cursor_wire() {
                self.last_spoken.clone().or_else(|| self.last_essay.clone())
            } else {
                self.last_spoken.clone()
            };
            if let Some(prev) = dump_anchor {
                let quote_dump = self.cursor_wire()
                    && turn.tool_calls.is_empty()
                    && crate::stutter::is_blockquote_heavy(&turn.content)
                    && crate::stutter::is_substantial_reply(&prev);
                if quote_dump || self.is_answer_dump_hop(&prev, &turn) {
                    let keep = self.last_spoken.clone().unwrap_or(prev);
                    if turn.tool_calls.is_empty() {
                        // No tools means the model chose to stop. Keep the first
                        // identical bubble without labelling that choice a
                        // harness failure.
                        self.mark_clean();
                        return self.finish(keep, None, steps);
                    }
                    dump_hop = true;
                    // grok-4.6 is trained to stop after a delivered answer.
                    // A Qwen-style dump lecture ("collapse to one conclusion")
                    // is itself a new user turn and makes it restate.
                    if self.cursor_wire() {
                        self.push_assistant(&turn);
                        self.defer_divergent_tools(std::mem::take(&mut turn.tool_calls));
                        self.mark_clean();
                        return self.finish(keep, None, steps);
                    }
                    if !self.dump_nudged {
                        self.dump_nudged = true;
                        trajectory_note = Some(crate::stutter::DUMP_NOTE);
                    }
                }
            }
            self.push_assistant(&turn);
            if crate::stutter::is_substantial_reply(&turn.content) && !dump_hop {
                self.last_essay = Some(turn.content.clone());
            }
            if Self::hop_locks_spoken(&turn) {
                self.last_spoken = Some(turn.content.clone());
            }
            let decision = self.gate_decision(&turn, steps, prompt_tokens, completion_tokens);

            if !turn.tool_calls.is_empty() {
                // Defer TERMINATE until after this tool batch. A gate Continue
                // with text is a one-shot trajectory observation after the
                // batch's results; later repeats stay silent.
                let mut gate_note = None;
                match &decision {
                    GateDecision::Stop { reason } if is_physics_stop(reason) => {
                        if self.cursor_wire() || self.physics_nudged {
                            self.note(reason);
                            self.pending_stop = Some(String::new());
                        } else {
                            self.physics_nudged = true;
                            self.note(reason);
                            gate_note = Some(PHYSICS_WRAP_NOTE.to_string());
                        }
                    }
                    GateDecision::Stop { reason } if !reason.is_empty() => {
                        self.pending_stop = Some(reason.clone());
                    }
                    GateDecision::Continue { continuation, .. }
                        if !continuation.is_empty() && !self.cursor_wire() =>
                    {
                        gate_note = Some(continuation.clone());
                    }
                    _ => {}
                }
                let calls = std::mem::take(&mut turn.tool_calls);
                if trajectory_note.is_some() || dump_hop {
                    // Do not execute a cleanup/write batch before the model has
                    // seen the divergence observation. Record well-formed tool
                    // results, then give control straight back to the model.
                    self.defer_divergent_tools(calls);
                } else {
                    self.execute_tools(calls).await;
                }
                if let Some(note) = gate_note {
                    self.push_hidden_user(note);
                }
                if let Some(note) = trajectory_note {
                    self.push_hidden_user(note);
                }
                self.flush_steer();
                if self.cancel.is_cancelled() {
                    return self.finish(String::new(), Some("aborted".into()), steps);
                }
                continue;
            }

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
        let reasoning = empty_to_none(&turn.reasoning);
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
        let content = if tool_calls.is_none() {
            Some(turn.content.clone())
        } else {
            empty_to_none(&turn.content)
        };
        let stored = self.persist_turn_media(&turn.media);
        let mut msg = ChatMessage::assistant_reply(content, reasoning, tool_calls);
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
                turn.content.clone(),
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

    fn persist_turn_media(
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
            self.push_hidden_user(format!("Steer: {note}"));
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
            let mut body = format!(
                "[background {name} {status} id={}]\n{}",
                response.id,
                response.joined_text()
            );
            if let Some(blob) = &response.blob {
                body.push_str(&format!("\n[blob {blob}]"));
            }
            self.push_hidden_user(body);
        }
    }

    pub(crate) fn push_hidden_user(&mut self, text: impl AsRef<str>) {
        self.push_hidden_user_opt(text, true);
    }

    /// Live-only note (re-injected each turn). Keep JSONL to real user/assistant/tool.
    pub(crate) fn push_ephemeral_note(&mut self, text: impl AsRef<str>) {
        self.push_hidden_user_opt(text, false);
    }

    fn push_hidden_user_opt(&mut self, text: impl AsRef<str>, log: bool) {
        let wrapped = wrap_tool_response(text.as_ref());
        self.messages.push(ChatMessage::user(wrapped.clone()));
        if log {
            self.log_event(SessionEvent::user(wrapped));
        }
    }

    /// Spoken answer already exists, and this hop is only dump/placeholder/cleanup.
    /// Unique docs, edits, first-time reads, grep/test, and rereads-only still continue.
    pub(crate) fn is_answer_dump_hop(&self, spoken: &str, turn: &ModelTurn) -> bool {
        if !crate::stutter::is_substantial_reply(spoken) {
            return false;
        }
        if turn.tool_calls.is_empty() {
            return crate::stutter::is_restated_reply(spoken, &turn.content);
        }
        let restated = crate::stutter::is_restated_reply(spoken, &turn.content);
        if crate::stutter::is_substantial_reply(&turn.content) && !restated {
            return false;
        }
        let dump = turn
            .tool_calls
            .iter()
            .any(|c| self.is_dump_tool(spoken, &turn.content, c));
        let work = turn
            .tool_calls
            .iter()
            .any(|c| self.is_work_tool(spoken, &turn.content, c));
        dump && !work
    }

    /// Lock the visible answer only when this hop is a delivery, not exploration.
    /// Reads-only narration (the 27B "let me check X" paragraph) must not freeze the turn.
    pub(crate) fn hop_locks_spoken(turn: &ModelTurn) -> bool {
        if !crate::stutter::is_substantial_reply(&turn.content) {
            return false;
        }
        if turn.tool_calls.is_empty() {
            return true;
        }
        let mut dumpish = false;
        for call in &turn.tool_calls {
            match dispatch_name(&call.name) {
                "read" | "view" => {}
                "write" => {
                    let (path, body) = write_path_body(call);
                    if crate::stutter::is_placeholder_write(path, body)
                        || crate::stutter::is_restated_reply(&turn.content, body)
                    {
                        dumpish = true;
                    } else {
                        return false;
                    }
                }
                "bash" => {
                    let cmd = bash_cmd(call);
                    if crate::stutter::is_cleanup_bash(cmd) {
                        dumpish = true;
                    } else {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        dumpish
    }

    pub(crate) fn is_dump_tool(&self, spoken: &str, content: &str, call: &ToolCall) -> bool {
        match dispatch_name(&call.name) {
            "write" => {
                let (path, body) = write_path_body(call);
                crate::stutter::is_placeholder_write(path, body)
                    || crate::stutter::is_restated_reply(spoken, body)
                    || (!content.trim().is_empty()
                        && crate::stutter::is_restated_reply(content, body))
            }
            "bash" => crate::stutter::is_cleanup_bash(bash_cmd(call)),
            _ => false,
        }
    }

    pub(crate) fn is_work_tool(&self, spoken: &str, content: &str, call: &ToolCall) -> bool {
        match dispatch_name(&call.name) {
            "read" | "view" => {
                let Some(path) = fs_tool_path(&call.name, &call.arguments) else {
                    return true;
                };
                !self
                    .read_paths
                    .contains(&canon_ws_path(&self.workspace, &path))
            }
            "write" | "bash" => !self.is_dump_tool(spoken, content, call),
            _ => true,
        }
    }

    pub(crate) fn promote_write_reply(turn: &ModelTurn) -> Option<String> {
        let bodies: Vec<&str> = turn
            .tool_calls
            .iter()
            .filter(|c| dispatch_name(&c.name) == "write")
            .filter_map(|c| {
                c.arguments
                    .get("contents")
                    .or_else(|| c.arguments.get("content"))
                    .and_then(|v| v.as_str())
            })
            .collect();
        crate::stutter::promote_dumped_reply(&turn.content, &bodies)
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

    /// Cursor / grok-4.6 Responses path. Tool names match Cursor; the loop
    /// must not inject QwenPaw 27B babysitting (style cards, trajectory
    /// lectures, auto-oracle, dump notes) — those are off-distribution and
    /// make grok restate.
    pub(crate) fn cursor_wire(&self) -> bool {
        self.completer.recasts_xai_product()
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
        Ok(AgentOutcome {
            text,
            stop_reason,
            steps,
            session_id: self.session_id.clone(),
            pending_steer: take_steer(&self.steer),
            streamed_text: self.stdio.text_streamed(),
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

/// Only injected after the streaming watchdog has actually fired. This keeps
/// the common path free of process rules and lets the model decide how to
/// converge once it has one concrete observation about its trajectory.
pub(crate) const THINK_DIVERGENCE_NOTE: &str = "[trajectory] This turn's thinking hit the length budget and may be diverging. \
Compress known facts and open questions first; answer or act if the evidence is enough, else take only the smallest missing step.";

/// One wrap-up hop when a physics cap would otherwise tombstone the turn.
pub(crate) const PHYSICS_WRAP_NOTE: &str = "[trajectory] This turn is near the step, time, or context cap. \
Close with a user-visible conclusion from the evidence you have; do not start a new tool loop.";

/// One repair hop after the parse-fail retry budget is spent.
pub(crate) const PARSE_REPAIR_NOTE: &str = "[trajectory] The last hop's tool call did not parse. \
Use a complete native tool call, or give a visible conclusion directly.";

pub(crate) fn is_physics_stop(reason: &str) -> bool {
    reason.starts_with("budget:context")
        || reason.contains("Max iterations")
        || reason.contains("time limit")
        || reason.contains("Token budget")
        || reason.contains("call budget")
}
