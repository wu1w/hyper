//! Tool execution: gates, AskQuestion, parallel dispatch.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::{json, Value};

use super::guard;
use super::notes::forbids_tools;
use super::turn::{NO_TOOL_ANSWER_RESERVE, NO_TOOL_THINK_FLOOR};
use super::verify;
use super::{Agent, Completer, ModelTurn, TokenSink};
use crate::error::Result;
use crate::mcp::run_mcp;
use crate::media::{MediaKind, MediaPart};
use crate::memory::run_memory_search;
use crate::paw_loop::fs_tool_path;
use crate::permit::{self, PermitDecision};
use crate::policy::ThinkPolicy;
use crate::session::{run_recall, OpenAiToolCall, PolicyReason, SessionEvent, StoredMedia};
use crate::skills::run_skill;
use crate::template::ChatMessage;
use crate::tool_calls::{CancelFlag, TextBlock, ToolCall, ToolResponse, ToolState};
use crate::tools::{run_search, run_tool, view, Workspace};
use crate::tools_schema::{dispatch_name, is_parallel_safe};

impl<C: Completer> Agent<C> {
    pub(crate) async fn execute_tools(&mut self, calls: Vec<ToolCall>) {
        if self.code_index.is_none() && calls.iter().any(|c| dispatch_name(&c.name) == "search") {
            self.start_code_index();
            self.settle_code_index().await;
        }
        for call in &calls {
            self.note(&format!("[{}] {}", call.name, preview_args(call)));
        }
        let write_priors = self.snapshot_write_priors(&calls);
        let mut responses = self.dispatch_with_prefetch(&calls).await;
        let mut harness = false;
        let mut test_red = false;
        let mut saw_test_output = false;
        let mut guard_notes: Vec<guard::GuardNote> = Vec::new();
        let mut edited: Vec<String> = Vec::new();
        let mut last_edit_id: Option<String> = None;
        for (call, response) in calls.iter().zip(responses.iter_mut()) {
            if is_harness_fail(&response) {
                harness = true;
            }
            if matches!(dispatch_name(&call.name), "read" | "view")
                && response.state == ToolState::Success
            {
                if let Some(path) = fs_tool_path(&call.name, &call.arguments) {
                    self.read_paths
                        .insert(canon_ws_path(&self.workspace, &path));
                }
            }
            if matches!(
                dispatch_name(&call.name),
                "read" | "view" | "write" | "edit"
            ) && response.state == ToolState::Success
            {
                if let Some(path) = fs_tool_path(&call.name, &call.arguments) {
                    self.observed_paths
                        .insert(canon_ws_path(&self.workspace, &path));
                }
            }
            if response.state == ToolState::Success {
                let prior = write_priors.get(&call.id).map(|s| s.as_str());
                guard_notes.extend(self.edit_guard.observe(&call.name, &call.arguments, prior));
                if matches!(dispatch_name(&call.name), "edit" | "write") {
                    if let Some(path) = fs_tool_path(&call.name, &call.arguments) {
                        if crate::channel::xfer::is_sendable_rel(&path) {
                            self.channel_files.push(path.clone());
                        }
                        if verify::is_code_path(&path) {
                            edited.push(path.clone());
                            last_edit_id = Some(call.id.clone());
                        }
                        if let Some(idx) = &self.code_index {
                            idx.refresh(&self.workspace, &path);
                            if verify::is_code_path(&path) && !verify::is_test_path(&path) {
                                let snippet = edit_snippet(call);
                                if let Some(hint) = idx.referrer_hint(&path, &snippet) {
                                    response.content.push(TextBlock { text: hint });
                                }
                            }
                        }
                    }
                }
            }
            if let Some(note) = self
                .edit_guard
                .observe_tool_output(&call.name, &response.joined_text())
            {
                guard_notes.push(note);
            }
            if guard::is_test_output(&call.name, &response.joined_text()) {
                saw_test_output = true;
            }
            if guard::is_test_fail(&call.name, &response.joined_text()) {
                test_red = true;
                if self.effort.note_test_fail() {
                    self.sync_effort(PolicyReason::Upgrade);
                }
            }
        }
        if let Some(diag) =
            verify::run_diagnostics_async(self.workspace.root(), &edited, &self.cancel).await
        {
            if let Some(id) = last_edit_id.as_deref() {
                if let Some(i) = calls.iter().position(|c| c.id == id) {
                    responses[i].content.push(TextBlock { text: diag });
                }
            }
            self.note("[diagnostics]");
        }
        for (call, response) in calls.iter().zip(responses) {
            self.commit_tool(&call.name, response);
        }
        if harness {
            let _ = self.effort.note_harness_fail();
            self.sync_effort(PolicyReason::Upgrade);
        }
        let mut thrash = false;
        for note in guard_notes {
            thrash |= note == guard::GuardNote::Thrash;
            self.apply_guard_note(note);
        }
        let oracle = self.oracle_tests_if_needed().await;
        let oracle_red = oracle == Some(true);
        if !test_red && (saw_test_output || oracle == Some(false)) {
            self.effort.note_tests_green();
        }
        // Consecutive test-reds must survive this round: a clean edit must not
        // zero `test_fails` before the next oracle/model run can count.
        if !harness && !test_red && !oracle_red {
            self.mark_clean();
        }
        if thrash && self.effort.note_thrash() {
            self.sync_effort(PolicyReason::Upgrade);
        }
    }

    pub(crate) fn apply_guard_note(&mut self, note: guard::GuardNote) {
        self.note(&format!("[guard] {}", note.label()));
    }

    /// Cursor does not auto-run a test suite before the model acts.
    pub(crate) async fn snapshot_test_baseline(&mut self) {
        let _ = self;
    }

    /// Cursor does not inject a hidden oracle card after edits.
    pub(crate) async fn oracle_tests_if_needed(&mut self) -> Option<bool> {
        self.edit_guard.mark_oracle_ran();
        None
    }

    pub(crate) fn snapshot_write_priors(&self, calls: &[ToolCall]) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for call in calls {
            if dispatch_name(&call.name) != "write" {
                continue;
            }
            let Some(path) = fs_tool_path("write", &call.arguments) else {
                continue;
            };
            let Ok(abs) = self.workspace.resolve(&path) else {
                continue;
            };
            if let Ok(body) = std::fs::read_to_string(abs) {
                out.insert(call.id.clone(), body);
            }
        }
        out
    }

    pub(crate) fn dispatch_ctx(&self) -> crate::subagent::DispatchCtx {
        crate::subagent::DispatchCtx {
            depth: if self.child.is_some() { 1 } else { 0 },
            parent: crate::subagent::ParentBind {
                session_id: self.session_id.clone(),
                workspace: self.workspace.root().to_path_buf(),
                plan_mode: self.plan_mode,
            },
            config: self.config.clone(),
            persist: self.persist_session,
            session_dir: self.session_dir.clone(),
            home: self.home.clone(),
            emit: self.emit.clone(),
            permit: self.permit.clone(),
            clarify: self.clarify.clone(),
            print: self.print,
        }
    }

    pub(crate) async fn gate_tool(&self, call: &ToolCall) -> Option<ToolResponse> {
        if let Some(denied) = crate::subagent::filter_tool(call, self.child.as_ref()) {
            return Some(denied);
        }
        let name = call.name.as_str();
        if self.plan_mode && permit::plan_mode_blocks(name, &call.arguments) {
            return Some(ToolResponse::text(
                &call.id,
                permit::plan_denied(name),
                ToolState::Error,
            ));
        }
        let Some(hub) = &self.permit else {
            return None;
        };
        if dispatch_name(name) == "computeruse"
            && crate::tools::computer::is_observe(&call.arguments)
        {
            return None;
        }
        match hub.check(name, &preview_args(call), &self.cancel).await {
            PermitDecision::Allow => None,
            PermitDecision::Always => {
                hub.remember(name);
                None
            }
            PermitDecision::Deny => Some(ToolResponse::text(
                &call.id,
                permit::user_denied(name),
                ToolState::Error,
            )),
        }
    }

    pub(crate) async fn complete_or_abort(
        &self,
        tools: Option<&[Value]>,
    ) -> Result<Option<ModelTurn>> {
        let prev = self.widen_no_tool_think();
        let result = self.complete_resilient(tools).await;
        if let Some(p) = prev {
            self.completer.set_policy(p);
        }
        result
    }

    /// One model hop. Transient endpoint drops retry with backoff so a flaky
    /// path continues the same turn (tools already run stay) instead of erroring.
    pub(crate) async fn complete_resilient(
        &self,
        tools: Option<&[Value]>,
    ) -> Result<Option<ModelTurn>> {
        let started = std::time::Instant::now();
        let mut attempt = 0u32;
        loop {
            if self.cancel.is_cancelled() {
                return Ok(None);
            }
            self.arm_sink();
            if attempt == 0 {
                if let Some(sink) = self.live_sink() {
                    sink.reset();
                    sink.reasoning(crate::llm_http::CONNECT_HINT);
                }
            }
            let wire = self.wire_messages();
            let result = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Ok(None),
                turn = self.completer.complete(&wire, tools) => turn,
            };
            match result {
                Ok(turn) => return Ok(Some(turn)),
                Err(e) if crate::llm_http::is_transient(&e) => {
                    attempt += 1;
                    if let Some(slot) = self.completer.speculate() {
                        slot.abort();
                    }
                    if started.elapsed() >= crate::llm_http::RETRY_BUDGET {
                        return Err(e);
                    }
                    let wait = crate::llm_http::retry_delay(attempt);
                    self.signal_net_retry(attempt, wait, &e);
                    tokio::select! {
                        biased;
                        _ = self.cancel.cancelled() => return Ok(None),
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn signal_net_retry(&self, attempt: u32, wait: Duration, err: &crate::error::Error) {
        let line = crate::llm_http::retry_status_line(attempt, wait);
        self.note(&format!("[net] {line}{err}"));
        if let Some(sink) = self.live_sink() {
            sink.reset();
            sink.reasoning(&line);
        }
    }

    /// A turn that forbids tools spends its whole budget on one answer, so the
    /// runaway cap is the only thing bounding depth. Raise it for this
    /// completion only; restore afterwards so a later coding hop keeps the
    /// session policy. `--think` lock is honored (left alone).
    pub(crate) fn widen_no_tool_think(&self) -> Option<ThinkPolicy> {
        if self.effort.user_locked {
            return None;
        }
        let prev = self.completer.policy()?;
        if !prev.enabled || prev.max_think_tokens >= NO_TOOL_THINK_FLOOR {
            return None;
        }
        if !forbids_tools(self.last_real_user()) {
            return None;
        }
        let mut raised = prev.clone();
        raised.max_think_tokens = NO_TOOL_THINK_FLOOR;
        raised.raise_generation_cap(NO_TOOL_THINK_FLOOR + NO_TOOL_ANSWER_RESERVE);
        self.completer.set_policy(raised);
        Some(prev)
    }

    pub(crate) async fn run_ask(&self, call: &ToolCall) -> ToolResponse {
        let ask = match crate::clarify::parse_ask(&call.arguments) {
            Ok(a) => a,
            Err(e) => {
                return ToolResponse::text(&call.id, format!("Error: {e}"), ToolState::Error);
            }
        };
        let yolo_permit = self
            .permit
            .as_ref()
            .map(|p| p.mode() == crate::permit::ApprovalMode::Yolo)
            .unwrap_or(false);
        // IM has no ClarifyHub. Skip (first option) so a hop does not stall;
        // children without a hub still error (they should inherit the parent's).
        let im_skip = self.child.is_none() && !super::interactive_channel(&self.channel);
        let decision = if self.print || yolo_permit || im_skip {
            crate::clarify::ClarifyDecision::Skip
        } else if let Some(hub) = &self.clarify {
            hub.ask(ask.clone(), &self.cancel).await
        } else {
            return ToolResponse::text(
                &call.id,
                "Error: AskQuestion needs an interactive channel (TUI or web). Do not assume Skip.",
                ToolState::Error,
            );
        };
        ToolResponse::text(
            &call.id,
            crate::clarify::format_decision(&ask, decision),
            ToolState::Success,
        )
    }

    pub(crate) fn arm_sink(&self) {
        self.completer.set_token_sink(self.live_sink());
    }

    pub(crate) fn live_sink(&self) -> Option<TokenSink> {
        if self.child.is_some() {
            // Children must stream (cli-chat-proxy rejects non-SSE POSTs) but
            // tokens stay off the parent activity stream.
            return Some(TokenSink::discard());
        }
        if let Some(emit) = &self.emit {
            Some(TokenSink::events(emit.clone()))
        } else if self.print {
            Some(TokenSink::stdio(self.stdio.clone()))
        } else {
            None
        }
    }

    pub(crate) async fn dispatch_one(&self, call: &ToolCall) -> ToolResponse {
        if let Some(denied) = self.gate_tool(call).await {
            return denied;
        }
        if crate::subagent::handles(&call.name) {
            return crate::subagent::dispatch(call, &self.dispatch_ctx()).await;
        }
        match dispatch_name(&call.name) {
            "ask" => self.run_ask(call).await,
            "recall" => run_recall(self.log.as_ref(), &self.blobs, call, self.limits),
            "memory_search" => match &self.memory {
                Some(store) => run_memory_search(store, call, self.limits),
                None => ToolResponse::text(
                    &call.id,
                    "Error: memory store unavailable.",
                    ToolState::Error,
                ),
            },
            "search" => match &self.code_index {
                Some(idx) => run_search(idx, call, self.limits),
                None => {
                    ToolResponse::text(&call.id, "Error: code index unavailable.", ToolState::Error)
                }
            },
            "web" => {
                let Some(web) = self.web.clone() else {
                    return ToolResponse::text(
                        &call.id,
                        "Error: web tools are off (config.toml [web] enabled).",
                        ToolState::Error,
                    );
                };
                let blobs = self.blobs.clone();
                let limits = self.limits;
                let owned = call.clone();
                let agent_cancel = self.cancel.clone();
                self.coordinator
                    .execute(call.clone(), "hyper", None, move |per_call| async move {
                        let (merged, link) = spawn_cancel_bridge(agent_cancel, per_call);
                        let res = tokio::select! {
                            biased;
                            _ = merged.cancelled() => ToolResponse::text(
                                &owned.id,
                                "Error: tool task aborted",
                                ToolState::Interrupted,
                            ),
                            r = web.run(&owned, limits, Some(&blobs)) => r,
                        };
                        link.abort();
                        res
                    })
                    .await
            }
            "mcp" => {
                let mcp = self.mcp.clone();
                let blobs = self.blobs.clone();
                let limits = self.limits;
                let owned = call.clone();
                let agent_cancel = self.cancel.clone();
                self.coordinator
                    .execute(call.clone(), "hyper", None, move |per_call| async move {
                        let (merged, link) = spawn_cancel_bridge(agent_cancel, per_call);
                        let res = tokio::select! {
                            biased;
                            _ = merged.cancelled() => ToolResponse::text(
                                &owned.id,
                                "Error: tool task aborted",
                                ToolState::Interrupted,
                            ),
                            r = run_mcp(&mcp, &owned, limits, Some(&blobs)) => r,
                        };
                        link.abort();
                        res
                    })
                    .await
            }
            // Not in tools[]. XML / hallucinated native calls still need a result.
            "skill" => run_skill(&self.skills, call, self.limits, Some(&self.blobs)),
            "view" => {
                let ws = self.workspace.clone();
                let caps = self.media_caps.clone();
                let bins = self.media_bins.clone();
                let max_bytes = self.media_max_bytes;
                let owned = call.clone();
                let agent_cancel = self.cancel.clone();
                self.coordinator
                    .execute(call.clone(), "hyper", None, move |per_call| async move {
                        let (merged, link) = spawn_cancel_bridge(agent_cancel, per_call);
                        let res = tokio::select! {
                            biased;
                            _ = merged.cancelled() => ToolResponse::text(
                                &owned.id,
                                "Error: tool task aborted",
                                ToolState::Interrupted,
                            ),
                            r = view(&ws, &owned, &caps, &bins, max_bytes) => r,
                        };
                        link.abort();
                        res
                    })
                    .await
            }
            "computeruse" => {
                let owned = call.clone();
                let agent_cancel = self.cancel.clone();
                let session_id = self.session_id.clone();
                self.coordinator
                    .execute(call.clone(), "hyper", None, move |per_call| async move {
                        let (merged, link) = spawn_cancel_bridge(agent_cancel, per_call);
                        let res = tokio::select! {
                            biased;
                            _ = merged.cancelled() => ToolResponse::text(
                                &owned.id,
                                "Error: tool task aborted",
                                ToolState::Interrupted,
                            ),
                            r = crate::tools::computer::computer_use(&owned, merged.clone(), &session_id) => r,
                        };
                        link.abort();
                        res
                    })
                    .await
            }
            _ => {
                let ws = self.workspace.clone();
                let limits = self.limits;
                let inherit_env = self.inherit_env;
                let blobs = self.blobs.clone();
                let owned = call.clone();
                let cancel = self.cancel.clone();
                self.coordinator
                    .execute(call.clone(), "hyper", None, move |per_call| async move {
                        let (merged, link) = spawn_cancel_bridge(cancel, per_call);
                        let res =
                            run_tool(&ws, &owned, merged, limits, inherit_env, Some(&blobs)).await;
                        link.abort();
                        res
                    })
                    .await
            }
        }
    }

    pub(crate) async fn dispatch_parallel(&self, calls: &[ToolCall]) -> Vec<ToolResponse> {
        // Same handlers as the serial path. `parallel_safe_batch` admits
        // read/view/search/web/ask; mutating tools still run serially.
        futures::future::join_all(calls.iter().map(|c| self.dispatch_one(c))).await
    }

    async fn dispatch_with_prefetch(&self, calls: &[ToolCall]) -> Vec<ToolResponse> {
        let mut out: Vec<Option<ToolResponse>> = vec![None; calls.len()];
        if let Some(slot) = &self.speculate {
            for (i, call) in calls.iter().enumerate() {
                if let Some(r) = slot.take(&call.id).await {
                    out[i] = Some(r);
                }
            }
            slot.abort();
        }
        let pending: Vec<(usize, ToolCall)> = calls
            .iter()
            .enumerate()
            .filter(|(i, _)| out[*i].is_none())
            .map(|(i, c)| (i, c.clone()))
            .collect();
        if !pending.is_empty() {
            let pending_calls: Vec<ToolCall> = pending.iter().map(|(_, c)| c.clone()).collect();
            let rest = if parallel_safe_batch(&pending_calls) {
                self.dispatch_parallel(&pending_calls).await
            } else {
                let mut serial = Vec::with_capacity(pending_calls.len());
                for call in &pending_calls {
                    serial.push(self.dispatch_one(call).await);
                }
                serial
            };
            for ((i, _), r) in pending.into_iter().zip(rest) {
                out[i] = Some(r);
            }
        }
        out.into_iter()
            .map(|r| r.expect("every tool call has a response"))
            .collect()
    }

    pub(crate) fn commit_tool(&mut self, name: &str, response: ToolResponse) {
        self.remember_tool_output(&response.joined_text());
        let stored = self.persist_turn_media(&response.media);
        let live_parts = stored_to_live_parts(&stored, &response.media);
        if live_parts.is_empty() {
            self.messages
                .push(ChatMessage::tool(&response.id, response.joined_text()));
        } else {
            self.messages.push(ChatMessage::tool_media(
                &response.id,
                response.joined_text(),
                live_parts,
            ));
        }
        self.log_event(
            SessionEvent::tool_folded(
                &response.id,
                name,
                response.joined_text(),
                response.blob.clone(),
                response
                    .blob
                    .as_ref()
                    .map(|_| response.original_chars as u64),
            )
            .with_media(stored),
        );
    }
}

fn stored_to_live_parts(stored: &[StoredMedia], fallback: &[MediaPart]) -> Vec<MediaPart> {
    if stored.is_empty() {
        return fallback.to_vec();
    }
    stored
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if s.url.starts_with("data:") {
                fallback.get(i).cloned().unwrap_or_else(|| MediaPart {
                    kind: media_kind(&s.kind),
                    mime: s.mime.clone(),
                    url: s.url.clone(),
                })
            } else {
                MediaPart {
                    kind: media_kind(&s.kind),
                    mime: s.mime.clone(),
                    url: s.url.clone(),
                }
            }
        })
        .collect()
}

fn media_kind(kind: &str) -> MediaKind {
    match kind {
        "video" => MediaKind::Video,
        "audio" => MediaKind::Audio,
        _ => MediaKind::Image,
    }
}

pub(crate) fn openai_tool_calls(calls: &[ToolCall]) -> Vec<Value> {
    calls
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "type": "function",
                "function": {
                    "name": c.name,
                    "arguments": args_as_object(c.arguments.clone()),
                }
            })
        })
        .collect()
}

pub(crate) fn normalize_tool_calls(calls: &[Value]) -> Vec<Value> {
    calls.iter().cloned().map(normalize_one_tool_call).collect()
}

fn normalize_one_tool_call(mut v: Value) -> Value {
    if let Some(args) = v.pointer_mut("/function/arguments") {
        *args = args_as_object(args.take());
    }
    v
}

fn args_as_object(v: Value) -> Value {
    match v {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        other => other,
    }
}

fn canon_read_path(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_string()
}

/// 守卫集合的键：先经 `workspace.resolve` 归一，吸收 `./a.rs` vs `a.rs`、
/// 绝对/相对混用（小模型常见）；resolve 失败回退纯字符串归一。
/// insert 与 lookup 两侧必须走同一函数。
pub(crate) fn canon_ws_path(ws: &Workspace, path: &str) -> String {
    match ws.resolve(path) {
        Ok(p) => canon_read_path(&p.to_string_lossy()),
        Err(_) => canon_read_path(path),
    }
}

/// `ToolState` 不落盘，重建只能靠文案判失败。新契约：非 Success 一律
/// "Error:" 开头。旧会话日志里还有三种未带前缀的失败文案，保守兼容；
/// coordinator 中断文案（cancelled/timeout）同样意味着 transcript 没看到内容。
fn tool_text_failed(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("Error:")
        || t.starts_with("plan mode:")
        || t.starts_with("User denied")
        || t.starts_with("tool task aborted")
        || t.trim_end() == "cancelled"
        || t.trim_end() == "timeout"
}

/// Paths whose content the live transcript shows: read/view/write/edit calls
/// whose tool result is not an error. Rebuilt each turn so sidecar re-hydration
/// (new Agent, old transcript) keeps the same observed-path set.
pub(crate) fn observed_from_messages(messages: &[ChatMessage], ws: &Workspace) -> HashSet<String> {
    let mut ok: HashMap<String, bool> = HashMap::new();
    for m in messages {
        if m.role != "tool" {
            continue;
        }
        let Some(id) = m.tool_call_id.as_deref() else {
            continue;
        };
        let failed = tool_text_failed(m.content.as_deref().unwrap_or(""));
        ok.insert(id.to_string(), !failed);
    }
    let mut out = HashSet::new();
    for m in messages {
        if m.role != "assistant" {
            continue;
        }
        let Some(calls) = &m.tool_calls else {
            continue;
        };
        for c in calls {
            let name = c["function"]["name"].as_str().unwrap_or("");
            if !matches!(
                crate::tools_schema::dispatch_name(name),
                "read" | "view" | "write" | "edit"
            ) {
                continue;
            }
            let id = c["id"].as_str().unwrap_or("");
            if !ok.get(id).copied().unwrap_or(false) {
                continue;
            }
            let args = args_as_object(c["function"]["arguments"].clone());
            if let Some(path) = fs_tool_path(name, &args) {
                out.insert(canon_ws_path(ws, &path));
            }
        }
    }
    out
}

fn preview_args(call: &ToolCall) -> String {
    call.arguments
        .get("path")
        .or_else(|| call.arguments.get("command"))
        .or_else(|| call.arguments.get("code"))
        .or_else(|| call.arguments.get("query"))
        .or_else(|| call.arguments.get("blob"))
        .or_else(|| call.arguments.get("name"))
        .or_else(|| call.arguments.get("method"))
        .or_else(|| call.arguments.get("server"))
        .or_else(|| call.arguments.get("prompt"))
        .or_else(|| call.arguments.get("pattern"))
        .or_else(|| call.arguments.get("glob_pattern"))
        .or_else(|| call.arguments.get("search_term"))
        .or_else(|| call.arguments.get("action"))
        .or_else(|| call.arguments.get("keys"))
        .or_else(|| call.arguments.get("text"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| call.arguments.get("seq").map(|v| v.to_string()))
        .unwrap_or_else(|| "…".into())
}

pub(crate) fn openai_stored(calls: &[ToolCall]) -> Vec<OpenAiToolCall> {
    calls
        .iter()
        .map(|c| OpenAiToolCall::function(&c.id, &c.name, c.arguments.to_string()))
        .collect()
}

pub(crate) fn parallel_safe_batch(calls: &[ToolCall]) -> bool {
    calls.len() > 1 && calls.iter().all(|c| is_parallel_safe(&c.name))
}

/// Merge agent-level stop with the coordinator's per-call flag. Native tools
/// take one `CancelFlag`; mcp/view select on the merged flag because they
/// do not take one.
fn spawn_cancel_bridge(
    agent: CancelFlag,
    per_call: CancelFlag,
) -> (CancelFlag, tokio::task::JoinHandle<()>) {
    let merged = CancelFlag::new();
    let link = merged.clone();
    let handle = tokio::spawn(async move {
        tokio::select! {
            _ = agent.cancelled() => link.cancel(),
            _ = per_call.cancelled() => link.cancel(),
        }
    });
    (merged, handle)
}

fn is_harness_fail(response: &ToolResponse) -> bool {
    response.state == ToolState::Interrupted && response.joined_text() == "Error: tool task aborted"
}

/// Real ripgrep hits kept live when the dump is also folded into index spans.
const SEARCH_HEAD_LINES: usize = 12;

fn search_head(full: &str) -> String {
    full.lines()
        .take(SEARCH_HEAD_LINES)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Folding must shrink the context. Spans that are as long as the dump (a query
/// matching one whole file) would cost KV instead of saving it.
pub(crate) fn search_fold_shrinks(full: &str, spans: &str) -> bool {
    search_head(full).len() + spans.len() < full.len()
}

fn edit_snippet(call: &ToolCall) -> String {
    match dispatch_name(&call.name) {
        "edit" => {
            let old = call
                .arguments
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = call
                .arguments
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("{old}\n{new}")
        }
        "write" => call
            .arguments
            .get("contents")
            .and_then(|v| v.as_str())
            .or_else(|| call.arguments.get("content").and_then(|v| v.as_str()))
            .unwrap_or("")
            .chars()
            .take(4000)
            .collect(),
        _ => String::new(),
    }
}
