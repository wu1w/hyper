//! User-turn ReAct loop: run / drive / finish, dump detection, effort, logging.

use serde_json::Value;

use super::dispatch::{
    normalize_tool_calls, observed_from_messages, openai_stored, openai_tool_calls,
};
use super::{Agent, AgentOutcome, Completer, ModelTurn};
use crate::channel::take_steer;
use crate::error::Result;
use crate::paw_loop::{fs_tool_path, GateCtx, GateDecision, ToolFingerprint};
use crate::session::{PolicyReason, RunPhase, SessionEvent, StepPhase, ToolLifecyclePhase};
use crate::sticky;
use crate::template::{is_hidden_user_text, wrap_tool_response, ChatMessage};
use crate::tool_calls::{ToolCall, ToolResponse, ToolState};
use crate::tools_schema::dispatch_name;

/// One hop in, one ruling out. `drive` only runs mechanisms.
enum Verdict {
    Continue,
    Tools {
        calls: Vec<ToolCall>,
        notes: Vec<String>,
    },
    Stop(StopCause),
}

enum StopCause {
    Aborted,
    Deliver {
        text: String,
        reason: Option<String>,
    },
    Exhausted,
}

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
        self.begin_run();
        let result = self.drive_inner().await;
        if let Err(err) = &result {
            self.end_run(RunPhase::Error, Some(err.to_string()));
        }
        result
    }

    async fn drive_inner(&mut self) -> Result<AgentOutcome> {
        // Per user turn, not per Agent lifetime. Sidecar/CLI construct a new
        // Agent each RPC, but in-process reuse (TUI, soak, channels) must not
        // inherit iteration/timeout/doom from the previous prompt.
        self.handler.reset_turn(&self.session_id);
        self.physics_nudged = false;
        self.channel_nudged = false;
        self.force_synthesis = false;
        self.write_nudge_count = 0;
        self.write_hold = false;
        self.watchdog_roomy_tried = false;
        self.wrap_up_after_tools = false;
        self.stub_nudged = false;
        self.length_truncations = 0;
        self.turn_steps = 0;
        self.turn_prompt_tokens = 0;
        self.turn_completion_tokens = 0;
        self.parse_retries = 0;
        self.progress.reset();
        self.last_spoken = None;
        self.tool_evidence.clear();
        self.read_paths.clear();
        self.search_calls
            .store(0, std::sync::atomic::Ordering::Relaxed);
        crate::lock_unpoison(&self.search_queries).clear();
        self.grep_calls
            .store(0, std::sync::atomic::Ordering::Relaxed);
        crate::lock_unpoison(&self.grep_queries).clear();
        crate::lock_unpoison(&self.read_full).clear();
        self.channel_files.clear();
        self.observed_paths = observed_from_messages(&self.messages, &self.workspace);
        let user = self.last_real_user().to_string();
        self.edit_guard.reset_turn(&user);
        self.oracle_cmd = None;
        self.snapshot_test_baseline().await;
        self.start_code_index();
        let mut first_hop = true;

        loop {
            self.drain_background();
            if self.cancel.is_cancelled() {
                return self.conclude(StopCause::Aborted);
            }
            if let Some(cause) = self.preflight_stop().await {
                return self.conclude(cause);
            }

            if !first_hop {
                self.settle_code_index().await;
            }
            first_hop = false;

            // Tools stay mounted on wrap hops. The schema is in the system
            // prefix; dropping `tools[]` mid-turn refills the prompt. Leaked
            // wrap-hop calls are cleared after parsing instead.
            let tools_owned = self.tools.clone();
            let tools = if tools_owned.is_empty() {
                None
            } else {
                Some(tools_owned.as_slice())
            };

            self.current_step = self.turn_steps.saturating_add(1);
            self.emit_step(StepPhase::Started, None, None);
            self.arm_speculate();
            let completed = self.complete_or_abort(tools).await;
            self.completer.set_speculate(None);
            let completed = match completed {
                Ok(turn) => turn,
                Err(err) => {
                    self.emit_step(StepPhase::Error, Some(err.to_string()), None);
                    return Err(err);
                }
            };
            let Some(mut turn) = completed else {
                self.emit_step(StepPhase::Error, Some("aborted".into()), None);
                self.drop_speculate();
                return self.conclude(StopCause::Aborted);
            };
            self.absorb_turn_usage(&turn);
            self.emit_step(
                StepPhase::Completed,
                None,
                Some((turn.prompt_tokens, turn.completion_tokens)),
            );

            match self.adjudicate(&mut turn, tools).await? {
                Verdict::Continue => {
                    self.drop_speculate();
                }
                Verdict::Tools { calls, notes } => {
                    if self.write_hold && hop_is_inspect_only(&calls) {
                        self.skip_held_inspect(&calls);
                    } else {
                        if !hop_is_inspect_only(&calls) {
                            self.write_hold = false;
                        }
                        self.settle_code_index().await;
                        let should_synth = self.execute_tools(calls).await;
                        if should_synth {
                            self.arm_write_nudge("inspect cap");
                        }
                    }
                    for note in notes {
                        self.push_hidden_user(note);
                    }
                    self.drop_speculate();
                    self.flush_steer();
                    if self.cancel.is_cancelled() {
                        return self.conclude(StopCause::Aborted);
                    }
                }
                Verdict::Stop(cause) => return self.conclude(cause),
            }
        }
    }

    fn conclude(&mut self, cause: StopCause) -> Result<AgentOutcome> {
        let (text, stop_reason) = match cause {
            StopCause::Aborted => (String::new(), Some("aborted".into())),
            StopCause::Deliver { text, reason } => (text, reason),
            StopCause::Exhausted => (String::new(), None),
        };
        self.finish(text, stop_reason, self.turn_steps)
    }

    /// Supervisor pre-flight: pending gate TERMINATE. Compact is a note only.
    async fn preflight_stop(&mut self) -> Option<StopCause> {
        if let Some(reason) = self.pending_stop.take() {
            let text = self.last_spoken.clone().unwrap_or_default();
            if self.arm_im_wrap_up(&text, &reason) {
                return None;
            }
            if reason.is_empty() || is_physics_stop(&reason) {
                if !reason.is_empty() {
                    self.note(&reason);
                }
                return Some(StopCause::Deliver { text, reason: None });
            }
            if is_quiet_repeat_stop(&reason) {
                return Some(StopCause::Deliver {
                    text,
                    reason: Some(reason),
                });
            }
            self.note(&reason);
            return Some(StopCause::Deliver {
                text,
                reason: Some(reason),
            });
        }
        if let Some(reason) = self.compact_if_needed().await {
            self.note(&reason);
        }
        None
    }

    fn absorb_turn_usage(&mut self, turn: &ModelTurn) {
        self.turn_steps = self.turn_steps.saturating_add(1);
        self.turn_prompt_tokens += turn.prompt_tokens;
        self.turn_completion_tokens += turn.completion_tokens;
        self.current_step = self.turn_steps;
    }

    /// Single termination authority. One hop in, one verdict out.
    async fn adjudicate(
        &mut self,
        turn: &mut ModelTurn,
        tools: Option<&[Value]>,
    ) -> Result<Verdict> {
        let mut hop_recorded = false;

        if turn.watchdog_hit
            && turn.tool_calls.is_empty()
            && !crate::stutter::is_substantial_reply(&turn.content)
            && !self.watchdog_roomy_tried
            && !self.wrap_up_after_tools
        {
            self.watchdog_roomy_tried = true;
            self.note("[watchdog] think cap; soft nudge and one roomy retry");
            hop_recorded = self.push_failed_hop(turn);
            self.drop_speculate();
            self.current_step = self.turn_steps.saturating_add(1);
            self.emit_step(StepPhase::Started, Some("watchdog retry".into()), None);
            self.arm_speculate();
            let widened = self.retry_with_runaway_room(tools).await;
            self.completer.set_speculate(None);
            if self.cancel.is_cancelled() {
                self.emit_step(
                    StepPhase::Error,
                    Some("watchdog retry aborted".into()),
                    None,
                );
                return Ok(Verdict::Stop(StopCause::Aborted));
            }
            match widened {
                Some(t) => {
                    self.absorb_turn_usage(&t);
                    self.emit_step(
                        StepPhase::Completed,
                        Some("watchdog retry".into()),
                        Some((t.prompt_tokens, t.completion_tokens)),
                    );
                    *turn = t;
                    hop_recorded = false;
                }
                None => self.emit_step(
                    StepPhase::Error,
                    Some("watchdog retry produced no turn".into()),
                    None,
                ),
            }
        }

        if turn.watchdog_hit {
            if !turn.tool_calls.is_empty() {
                self.length_truncations = self.length_truncations.saturating_add(1);
                if self.length_truncations >= LENGTH_TRUNCATION_ABORT {
                    return Ok(Verdict::Stop(StopCause::Exhausted));
                }
                self.note("[watchdog] length; fail truncated tool calls");
                self.emit_tools_scheduled(&turn.tool_calls);
                self.push_assistant(turn);
                self.fail_truncated_tools(std::mem::take(&mut turn.tool_calls));
                return Ok(Verdict::Continue);
            }
            if Self::hop_is_delivery(turn) {
                self.push_assistant(turn);
                self.last_spoken = Some(turn.content.clone());
                return Ok(Verdict::Stop(StopCause::Deliver {
                    text: turn.content.clone(),
                    reason: None,
                }));
            }
            if !hop_recorded {
                self.push_failed_hop(turn);
            }
            // Roomy retry already spent, or wrap hop. Thinking stays on;
            // truncated think is not the user-visible answer (Cursor).
            return Ok(Verdict::Stop(StopCause::Exhausted));
        }

        if self.wrap_up_after_tools && !turn.tool_calls.is_empty() {
            self.note("[watchdog] wrap-up leaked tools; not executed");
            turn.tool_calls.clear();
            turn.raw_tool_calls = None;
        }

        if !self.wrap_up_after_tools && turn.tool_calls.is_empty() {
            if let Some(calls) = lift_leaked_tool_json(&turn.content) {
                turn.tool_calls = calls;
                turn.content.clear();
            }
        }
        if !self.wrap_up_after_tools
            && turn.tool_calls.is_empty()
            && crate::stutter::is_leaked_write_narration(&turn.content)
        {
            self.arm_write_nudge("leaked write intent");
            if self.write_nudge_count > 1 {
                turn.content.clear();
            } else {
                return Ok(Verdict::Continue);
            }
        }

        if !turn.reasoning.is_empty()
            && self.print
            && !self.stdio.think_streamed()
            && turn.tool_calls.is_empty()
        {
            let vis = crate::think_visible::visible_think(turn.reasoning.trim());
            if !vis.trim().is_empty() {
                eprintln!("[think]\n{vis}");
            }
        }

        if turn.parse_fail {
            self.effort.note_parse_fail();
            self.sync_effort(PolicyReason::Upgrade);
            self.parse_retries += 1;
            self.note("[parse] retry");
            if self.parse_retries >= self.parse_stop_after {
                return Ok(Verdict::Stop(StopCause::Deliver {
                    text: self.last_spoken.clone().unwrap_or_default(),
                    reason: None,
                }));
            }
            return Ok(Verdict::Continue);
        }

        if turn.tool_calls.is_empty() {
            if let Some(body) = Self::promote_reasoning_reply(turn) {
                turn.content = body;
            }
        }
        if turn.tool_calls.is_empty() && !Self::hop_is_delivery(turn) {
            self.push_failed_hop(turn);
            return Ok(self.rescue_incomplete(turn));
        }

        self.emit_tools_scheduled(&turn.tool_calls);
        self.push_assistant(turn);
        if turn.tool_calls.is_empty() && crate::stutter::is_substantial_reply(&turn.content) {
            self.last_spoken = Some(turn.content.clone());
        }
        let decision = self.gate_decision(turn);

        if !turn.tool_calls.is_empty() {
            let mut notes: Vec<String> = Vec::new();
            match &decision {
                GateDecision::Stop { reason } if is_physics_stop(reason) => {
                    if !self.physics_nudged {
                        self.physics_nudged = true;
                        self.wrap_up_after_tools = true;
                        self.note(reason);
                        notes.push(PHYSICS_WRAP_NOTE.to_string());
                    } else {
                        self.note(reason);
                        self.pending_stop = Some(String::new());
                    }
                }
                GateDecision::Stop { reason } if !reason.is_empty() => {
                    self.pending_stop = Some(reason.clone());
                }
                _ => {}
            }
            return Ok(Verdict::Tools {
                calls: std::mem::take(&mut turn.tool_calls),
                notes,
            });
        }

        self.mark_clean();
        match decision {
            GateDecision::Continue { continuation, .. } => {
                let reason = if continuation.is_empty() {
                    None
                } else {
                    Some(continuation)
                };
                Ok(Verdict::Stop(StopCause::Deliver {
                    text: std::mem::take(&mut turn.content),
                    reason,
                }))
            }
            GateDecision::Stop { reason } => {
                if is_physics_stop(&reason) {
                    if self.arm_im_wrap_up(&turn.content, &reason) {
                        return Ok(Verdict::Continue);
                    }
                    self.note(&reason);
                    return Ok(Verdict::Stop(StopCause::Deliver {
                        text: std::mem::take(&mut turn.content),
                        reason: None,
                    }));
                }
                if is_quiet_repeat_stop(&reason) {
                    return Ok(Verdict::Stop(StopCause::Deliver {
                        text: std::mem::take(&mut turn.content),
                        reason: Some(reason),
                    }));
                }
                let reason = if reason.is_empty() {
                    None
                } else {
                    Some(reason)
                };
                Ok(Verdict::Stop(StopCause::Deliver {
                    text: std::mem::take(&mut turn.content),
                    reason,
                }))
            }
            GateDecision::Bypass => Ok(Verdict::Stop(StopCause::Deliver {
                text: std::mem::take(&mut turn.content),
                reason: None,
            })),
        }
    }

    fn push_failed_hop(&mut self, turn: &ModelTurn) -> bool {
        if turn.content.trim().is_empty() && turn.reasoning.trim().is_empty() {
            return false;
        }
        self.push_assistant(turn);
        true
    }

    fn hop_is_delivery(turn: &ModelTurn) -> bool {
        turn.tool_calls.is_empty()
            && !turn.content.trim().is_empty()
            && !crate::stutter::is_progress_narration(&turn.content)
            && !crate::stutter::is_leaked_write_narration(&turn.content)
    }

    fn rescue_incomplete(&mut self, turn: &ModelTurn) -> Verdict {
        if self.wrap_up_after_tools {
            return Verdict::Stop(StopCause::Exhausted);
        }
        if turn.content.trim().is_empty() {
            if self
                .last_spoken
                .as_deref()
                .map(str::trim)
                .is_some_and(|s| !s.is_empty())
            {
                return Verdict::Stop(StopCause::Deliver {
                    text: String::new(),
                    reason: None,
                });
            }
            if !self.channel_nudged {
                self.channel_nudged = true;
                self.note("[channel] empty visible reply; one wrap-up");
                self.push_hidden_user(EMPTY_CHANNEL_NOTE);
                return Verdict::Continue;
            }
            return Verdict::Stop(StopCause::Exhausted);
        }
        if !self.stub_nudged && !self.channel_nudged {
            self.stub_nudged = true;
            self.channel_nudged = true;
            self.note("[channel] next-step line without tools");
            self.push_hidden_user(STUB_CONTINUE_NOTE);
            return Verdict::Continue;
        }
        Verdict::Stop(StopCause::Exhausted)
    }

    fn fail_truncated_tools(&mut self, calls: Vec<ToolCall>) {
        for call in calls {
            let name = if call.name.is_empty() {
                "unknown"
            } else {
                call.name.as_str()
            };
            self.note(&format!("[{name}] skipped truncated"));
            self.commit_tool(
                name,
                ToolResponse::text(call.id.clone(), LENGTH_TRUNCATED_TOOL, ToolState::Error),
            );
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
        let raw = text.as_ref();
        let wrapped = wrap_tool_response(raw);
        self.messages.push(ChatMessage::user(wrapped.clone()));
        self.log_event(SessionEvent::context("runtime", raw));
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

    pub(crate) fn gate_decision(&self, turn: &ModelTurn) -> GateDecision {
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
        ctx.iteration = self.turn_steps;
        ctx.prompt_tokens = turn.prompt_tokens;
        ctx.completion_tokens = turn.completion_tokens;
        ctx.tokens_used = self.turn_prompt_tokens + self.turn_completion_tokens;
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
            return retry.filter(|t| !t.parse_fail);
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
        retry.filter(|t| !t.parse_fail)
    }

    fn arm_speculate(&mut self) {
        let slot = super::SpeculativeSlot::new(self.speculate_ctx());
        self.speculate = Some(slot.clone());
        self.completer.set_speculate(Some(slot));
    }

    /// Cursor keeps the frozen `tools[]` mounted. Inspect-cap / leaked
    /// Write-as-prose is a trajectory nudge, never `tools=None`.
    fn arm_write_nudge(&mut self, why: &str) {
        self.write_hold = true;
        self.force_synthesis = false;
        self.progress.clear_synthesis();
        self.write_nudge_count = self.write_nudge_count.saturating_add(1);
        if self.write_nudge_count == 1 {
            self.note(&format!("[trajectory] {why}; tools stay mounted"));
            self.push_hidden_user(super::progress::WRITE_NOW_NOTE);
        } else {
            self.note(&format!("[trajectory] {why}; inspect skipped, tools stay"));
        }
    }

    fn skip_held_inspect(&mut self, calls: &[ToolCall]) {
        for call in calls {
            let response = ToolResponse::text(
                call.id.clone(),
                super::progress::INSPECT_SKIP_MSG,
                ToolState::Success,
            );
            self.commit_tool(&call.name, response);
            self.emit_tool_lifecycle(
                call,
                ToolLifecyclePhase::Skipped,
                Some("inspection skipped; write or answer".into()),
            );
        }
    }

    fn drop_speculate(&mut self) {
        self.completer.set_speculate(None);
        if let Some(slot) = self.speculate.take() {
            slot.abort();
        }
    }

    /// Physics cap: one recap hop on every surface so follow-up compact has
    /// Decisions (Cursor / grok CLI keep a visible end-of-turn). Empty-visible
    /// wrap stays in `rescue_incomplete` (`EMPTY_CHANNEL_NOTE`).
    fn arm_im_wrap_up(&mut self, spoken: &str, reason: &str) -> bool {
        if self.physics_nudged || !spoken.trim().is_empty() {
            return false;
        }
        let physics = is_physics_stop(reason);
        if !physics && (super::interactive_channel(&self.channel) || self.channel.is_empty()) {
            return false;
        }
        self.physics_nudged = true;
        if !reason.is_empty() {
            self.note(reason);
        }
        self.push_hidden_user(PHYSICS_WRAP_NOTE);
        true
    }

    /// Lift a finished answer that landed only in `reasoning`. Scratch plans
    /// and long CoT stay in think and get a wrap-up hop. Cursor keeps thinking
    /// in the think panel; only a short finished reply is promoted.
    fn promote_reasoning_reply(turn: &ModelTurn) -> Option<String> {
        if !turn.tool_calls.is_empty() || !turn.content.trim().is_empty() {
            return None;
        }
        let r = turn.reasoning.trim();
        if crate::stutter::is_scratch_think(r) {
            return None;
        }
        if !crate::stutter::is_substantial_reply(r) {
            return None;
        }
        if r.chars().count() > PROMOTE_REASONING_MAX {
            return None;
        }
        Some(r.to_string())
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
        let forward = !matches!(event, SessionEvent::User(_) | SessionEvent::Context(_));
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

    fn begin_run(&mut self) {
        let id = uuid::Uuid::new_v4().simple().to_string();
        self.run_id = Some(id.clone());
        self.turn_id = Some(id.clone());
        self.current_step = 0;
        self.log_event(SessionEvent::run(
            id.clone(),
            id.clone(),
            RunPhase::Accepted,
            None,
        ));
        self.log_event(SessionEvent::run(id.clone(), id, RunPhase::Started, None));
    }

    pub(crate) fn lifecycle_ids(&self) -> Option<(String, String)> {
        Some((self.run_id.clone()?, self.turn_id.clone()?))
    }

    pub(crate) fn emit_step(
        &mut self,
        phase: StepPhase,
        reason: Option<String>,
        usage: Option<(u64, u64)>,
    ) {
        let Some((run_id, turn_id)) = self.lifecycle_ids() else {
            return;
        };
        self.log_event(SessionEvent::step(
            run_id,
            turn_id,
            self.current_step,
            phase,
            reason,
            usage,
        ));
    }

    fn end_run(&mut self, phase: RunPhase, reason: Option<String>) {
        let Some(run_id) = self.run_id.take() else {
            return;
        };
        let turn_id = self.turn_id.take().unwrap_or_else(|| run_id.clone());
        self.log_event(SessionEvent::run(run_id, turn_id, phase, reason));
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
        let reason = stop_reason.clone().unwrap_or_else(|| "end_turn".into());
        self.log_event(SessionEvent::stop(reason));
        let aborted = stop_reason.as_deref() == Some("aborted");
        let run_phase = if aborted {
            RunPhase::Aborted
        } else {
            RunPhase::Completed
        };
        self.end_run(run_phase, stop_reason.clone());
        let mut text = if aborted || !text.trim().is_empty() {
            text
        } else {
            self.last_spoken.clone().unwrap_or_default()
        };
        if !aborted && text.trim().is_empty() {
            if !super::interactive_channel(&self.channel) && !self.channel.is_empty() {
                text = im_no_reply_text(stop_reason.as_deref());
            } else {
                text = EMPTY_STOP_FALLBACK.to_string();
            }
        }
        if !aborted {
            self.maybe_write_chat_recap(&text);
        }
        Ok(AgentOutcome {
            text,
            stop_reason,
            steps,
            session_id: self.session_id.clone(),
            pending_steer: take_steer(&self.steer),
            streamed_text: self.stdio.text_streamed(),
            channel_files: std::mem::take(&mut self.channel_files),
            plan_mode: self.plan_mode,
            clarify_mode: self.clarify_mode,
        })
    }

    fn maybe_write_chat_recap(&self, text: &str) {
        if self.child.is_some() || !self.persist_session {
            return;
        }
        if crate::session::is_probe_session(&self.session_id) || self.channel == "subagent" {
            return;
        }
        let spoken = text.trim();
        if spoken.chars().count() < 40 {
            return;
        }
        let Some(mem) = &self.memory else {
            return;
        };
        let user = self
            .log
            .as_ref()
            .and_then(|l| {
                l.events().iter().rev().find_map(|e| match e {
                    SessionEvent::User(u) if !is_hidden_user_text(&u.text) => Some(u.text.clone()),
                    _ => None,
                })
            })
            .unwrap_or_else(|| self.last_real_user().to_string());
        let ws = self.workspace.root().display().to_string();
        let _ = mem.write_chat_recap(&self.session_id, &self.channel, &ws, &user, spoken);
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
/// Forced convergence is deliberately much smaller than a user-requested
/// no-tool deep-thinking turn. It should summarize evidence, not reopen work.
pub(crate) const SYNTHESIS_THINK_CAP: u32 = 768;
pub(crate) const SYNTHESIS_ANSWER_RESERVE: u32 = 2048;
/// Wire cap for the wrap hop (`max_output_tokens` on Grok Responses includes
/// reasoning). Local llama.cpp watchdog still uses [`SYNTHESIS_THINK_CAP`].
pub(crate) const SYNTHESIS_OUTPUT_CAP: u32 = SYNTHESIS_THINK_CAP + SYNTHESIS_ANSWER_RESERVE;

#[cfg(test)]
pub(crate) const THINK_DIVERGENCE_NOTE: &str = "[trajectory] This turn's thinking hit the length budget and may be diverging. \
Compress known facts and open questions first; answer or act if the evidence is enough, else take only the smallest missing step.";

pub(crate) const PHYSICS_WRAP_NOTE: &str =
    "[trajectory] No user-visible reply yet. Summarize what you found and accomplished; \
do not call any more tools.";

/// One wrap-up hop after a toolless hop with empty visible content.
/// Thinking is not the answer (Cursor hop geometry).
pub(crate) const EMPTY_CHANNEL_NOTE: &str = "\
[trajectory] The last hop had no user-visible reply. \
Write the full conclusion in the normal reply — thinking is not shown as the answer. \
If the task is unfinished, call a tool. Do not submit another empty reply.";

/// No-tool hop that only announced the next step.
pub(crate) const STUB_CONTINUE_NOTE: &str = "\
[trajectory] The last hop only announced a next step — no tool call and no conclusion. \
Call the tool, or write the full answer. Do not submit another next-step line.";

/// Truncated native tool calls are not executed (pi / grok CLI length salvage).
const LENGTH_TRUNCATED_TOOL: &str =
    "Tool call was not executed: the response hit the think/output \
token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.";

const LENGTH_TRUNCATION_ABORT: u32 = 3;

/// Console fallback when a wrap-up hop is still blank. IM uses `im_no_reply_text`.
pub(crate) const EMPTY_STOP_FALLBACK: &str =
    "没有可见回复：模型这一跳交了空正文。直接发「继续」我再收一次。";

/// Upper bound for promoting think-channel text into the visible reply.
/// Longer blobs are unfinished CoT, not an answer.
const PROMOTE_REASONING_MAX: usize = 800;

/// Hermes turn-completion explainer: IM never returns a blank box.
pub(crate) fn im_no_reply_text(reason: Option<&str>) -> String {
    let why = match reason {
        Some(r) if is_physics_stop(r) => {
            "这轮工具步数、时间或上下文用尽，模型没有写出给用户看的结论。"
        }
        Some(r) if r.contains("watchdog") => "思考长度触顶后仍没有正文。",
        _ => "模型只做了工具调用，没有写出给用户看的结论。",
    };
    format!("没有可见回复：{why}直接发「继续」我再收一次。")
}

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

/// Cursor identical-call halt. Ends the turn without a `[trajectory]` lecture.
pub(crate) fn is_quiet_repeat_stop(reason: &str) -> bool {
    reason.starts_with(crate::paw_loop::REPEAT_STOP)
}

/// Write-hold skips extra Read/Grep/Glob/Search, not Shell / TodoWrite / web.
fn hop_is_inspect_only(calls: &[ToolCall]) -> bool {
    !calls.is_empty()
        && calls
            .iter()
            .all(|c| super::progress::is_held_inspect(&c.name))
}

/// Grok sometimes paints a Write/StrReplace as a JSON fence instead of a
/// native tool call. Recover that as a real hop so the file actually lands.
pub(crate) fn lift_leaked_tool_json(content: &str) -> Option<Vec<ToolCall>> {
    let raw = json_tool_body(content)?;
    let v: Value = serde_json::from_str(raw).ok()?;
    let objs: Vec<Value> = match v {
        Value::Array(a) if (1..=4).contains(&a.len()) => a,
        Value::Object(_) => vec![v],
        _ => return None,
    };
    let mut calls = Vec::with_capacity(objs.len());
    for (i, obj) in objs.iter().enumerate() {
        calls.push(leaked_obj_to_call(obj, i)?);
    }
    Some(calls)
}

fn json_tool_body(content: &str) -> Option<&str> {
    let t = content.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest
            .strip_prefix("json")
            .or_else(|| rest.strip_prefix("JSON"))
            .unwrap_or(rest);
        let rest = rest
            .strip_prefix('\n')
            .or_else(|| rest.strip_prefix("\r\n"))?;
        let inner = rest.trim_end().strip_suffix("```")?.trim();
        if inner.starts_with('{') || inner.starts_with('[') {
            return Some(inner);
        }
        return None;
    }
    if (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']')) {
        return Some(t);
    }
    None
}

fn leaked_obj_to_call(obj: &Value, i: usize) -> Option<ToolCall> {
    let name = obj.get("name").and_then(Value::as_str)?;
    let key = dispatch_name(name);
    if !matches!(key, "write" | "edit" | "strreplace" | "delete") {
        return None;
    }
    let arguments = if let Some(args) = obj.get("arguments") {
        if args.is_object() {
            args.clone()
        } else if let Some(s) = args.as_str() {
            serde_json::from_str(s).ok()?
        } else {
            return None;
        }
    } else {
        let mut m = serde_json::Map::new();
        for k in ["path", "contents", "old_string", "new_string"] {
            if let Some(v) = obj.get(k) {
                m.insert(k.to_string(), v.clone());
            }
        }
        if m.is_empty() {
            return None;
        }
        Value::Object(m)
    };
    let path = crate::tools::arg_str(&arguments, "path").unwrap_or_default();
    if path.is_empty() {
        return None;
    }
    if key == "write" && crate::tools::arg_str(&arguments, "contents").is_none() {
        return None;
    }
    Some(ToolCall {
        id: format!("lift-{i}"),
        name: name.to_string(),
        arguments,
    })
}

#[cfg(test)]
mod lift_tests {
    use super::lift_leaked_tool_json;

    #[test]
    fn lifts_r96_write_fence() {
        let calls = lift_leaked_tool_json(
            "```json\n{\"name\": \"Write\", \"path\": \".grok-hyper/overnight/r96_ok.txt\", \"contents\": \"R96_OK\\n\"}\n```",
        )
        .expect("fence");
        assert_eq!(calls.len(), 1);
        assert_eq!(crate::tools_schema::dispatch_name(&calls[0].name), "write");
        assert_eq!(
            crate::tools::arg_str(&calls[0].arguments, "path").as_deref(),
            Some(".grok-hyper/overnight/r96_ok.txt")
        );
        assert_eq!(
            crate::tools::arg_str(&calls[0].arguments, "contents").as_deref(),
            Some("R96_OK\n")
        );
    }

    #[test]
    fn ignores_prose_and_non_write_json() {
        assert!(
            lift_leaked_tool_json("已写入 `.grok-hyper/overnight/r96_ok.txt`。 DESKTOP_R96")
                .is_none()
        );
        assert!(lift_leaked_tool_json("```json\n{\"foo\": 1}\n```").is_none());
        assert!(lift_leaked_tool_json(
            "```json\n{\"name\": \"Search\", \"query\": \"fold_idle_search\"}\n```"
        )
        .is_none());
        assert!(lift_leaked_tool_json(
            "先说明一下。\n```json\n{\"name\": \"Write\", \"path\": \"a.txt\", \"contents\": \"x\"}\n```\nDESKTOP_R96"
        )
        .is_none());
    }

    #[test]
    fn lifts_strreplace_fence() {
        let calls = lift_leaked_tool_json(
            "{\"name\":\"StrReplace\",\"path\":\"a.rs\",\"old_string\":\"a\",\"new_string\":\"b\"}",
        )
        .expect("strreplace");
        assert_eq!(crate::tools_schema::dispatch_name(&calls[0].name), "edit");
    }
}
