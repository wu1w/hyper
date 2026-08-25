//! Tool execution: gates, AskQuestion, parallel dispatch, oracle, search fold.

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
use crate::memory::run_memory_search;
use crate::paw_loop::fs_tool_path;
use crate::permit::{self, PermitDecision};
use crate::policy::ThinkPolicy;
use crate::session::{run_recall, OpenAiToolCall, PolicyReason, SessionEvent};
use crate::skills::run_skill;
use crate::template::ChatMessage;
use crate::tool_calls::{CancelFlag, TextBlock, ToolCall, ToolResponse, ToolState};
use crate::tools::{bash_search_query, run_search, run_tool, search_dump_too_big, view, Workspace};
use crate::tools_schema::{dispatch_name, is_parallel_safe};

impl<C: Completer> Agent<C> {
    pub(crate) async fn execute_tools(&mut self, calls: Vec<ToolCall>) {
        for call in &calls {
            self.note(&format!("[{}] {}", call.name, preview_args(call)));
        }
        let write_priors = self.snapshot_write_priors(&calls);
        let mut responses = if parallel_safe_batch(&calls) {
            self.dispatch_parallel(&calls).await
        } else {
            let mut out = Vec::with_capacity(calls.len());
            for call in &calls {
                out.push(self.dispatch_one(call).await);
            }
            out
        };
        for (call, response) in calls.iter().zip(responses.iter_mut()) {
            self.fold_bash_search(call, response);
        }
        let fail_blob: String = responses
            .iter()
            .map(|r| r.joined_text())
            .collect::<Vec<_>>()
            .join("\n");
        let mut harness = false;
        let mut test_red = false;
        let mut saw_test_output = false;
        let mut guard_notes: Vec<guard::GuardNote> = Vec::new();
        for (call, mut response) in calls.iter().zip(responses) {
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
        self.inject_skill_from_tools(&fail_blob);
        let mut doc_hay = fail_blob;
        for call in &calls {
            if let Some(p) = fs_tool_path(&call.name, &call.arguments) {
                doc_hay.push('\n');
                doc_hay.push_str(&p);
            }
            for key in ["command", "glob_pattern", "path"] {
                if let Some(v) = call.arguments.get(key).and_then(|x| x.as_str()) {
                    doc_hay.push('\n');
                    doc_hay.push_str(v);
                }
            }
        }
        self.inject_doc_read(&doc_hay);
    }

    pub(crate) fn defer_divergent_tools(&mut self, calls: Vec<ToolCall>) {
        for call in calls {
            self.note(&format!("[{}] deferred low-information batch", call.name));
            self.commit_tool(
                &call.name,
                ToolResponse::text(
                    &call.id,
                    "Deferred: this batch repeated the visible answer or only staged/cleaned a scratch copy. Reassess using the trajectory observation, then choose the next step.",
                    ToolState::Error,
                ),
            );
        }
    }

    pub(crate) fn apply_guard_note(&mut self, note: guard::GuardNote) {
        self.note(&format!("[guard] {}", note.label()));
        self.push_hidden_user(note.text());
    }

    /// Sample a cheap suite before edits in `--print` only, so a later red run
    /// can be told apart from a tree that was already red. Interactive turns
    /// skip this and only run the post-edit scoped oracle.
    pub(crate) async fn snapshot_test_baseline(&mut self) {
        if !self.print || self.plan_mode || self.oracle_cmd.is_some() {
            return;
        }
        if !verify::workspace_has_tests(self.workspace.root()) {
            return;
        }
        let Some(cmd) = verify::workspace_default_test_cmd(self.workspace.root()) else {
            return;
        };
        let started = std::time::Instant::now();
        let out = self.run_oracle(&cmd).await;
        if !guard::is_test_output("bash", &out) {
            return;
        }
        if started.elapsed() > ORACLE_MAX_SUITE {
            self.note("[oracle] suite too slow; baseline only");
        } else {
            self.oracle_cmd = Some(cmd);
        }
        let red = guard::is_test_fail("bash", &out);
        self.edit_guard.set_baseline(red);
        self.push_hidden_user(format!(
            "[baseline] Pre-change tests are {}.\n{}",
            if red { "already failing" } else { "all green" },
            tail_chars(&out, ORACLE_TAIL_CHARS)
        ));
    }

    /// After a successful code edit, run a scoped test command and feed the
    /// tail back. Not gated on user keywords. Skips office docs, plan mode,
    /// and turns where the model already ran tests.
    /// Returns `None` if the oracle did not run, `Some(failed)` if it did.
    pub(crate) async fn oracle_tests_if_needed(&mut self) -> Option<bool> {
        if self.plan_mode || self.pending_stop.is_some() || !self.edit_guard.wants_oracle() {
            return None;
        }
        let cmd = verify::scoped_test_cmd(self.workspace.root(), self.edit_guard.code_paths())
            .or_else(|| self.oracle_cmd.clone());
        let Some(cmd) = cmd else {
            self.edit_guard.mark_oracle_ran();
            return None;
        };
        let out = self.run_oracle(&cmd).await;
        if let Some(note) = self.edit_guard.observe_oracle_output(&out) {
            self.apply_guard_note(note);
        }
        let red = guard::is_test_fail("bash", &out);
        if red && self.effort.note_test_fail() {
            self.sync_effort(PolicyReason::Upgrade);
        }
        self.push_hidden_user(format!("[oracle]\n{}", tail_chars(&out, ORACLE_TAIL_CHARS)));
        if guard::is_test_output("bash", &out) {
            self.oracle_cmd = Some(cmd);
        }
        Some(red)
    }

    /// The oracle uses Python's portable `-B` switch. Avoiding bytecode is both
    /// cheaper than managing a throwaway cache and works unchanged in Bash on
    /// macOS/Linux/Git Bash and in the PowerShell fallback.
    pub(crate) async fn run_oracle(&mut self, cmd: &str) -> String {
        self.note(&format!("[oracle] {cmd}"));
        let call = ToolCall {
            id: format!("oracle-{}", self.oracle_runs),
            name: "bash".into(),
            arguments: json!({"command": cmd}),
        };
        self.oracle_runs += 1;
        self.dispatch_one(&call).await.joined_text()
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

    pub(crate) fn fold_bash_search(&self, call: &ToolCall, response: &mut ToolResponse) {
        if dispatch_name(&call.name) != "bash" {
            return;
        }
        let Some(query) = bash_search_query(bash_cmd(call)) else {
            return;
        };
        if !search_dump_too_big(&response.joined_text()) {
            return;
        }
        let Some(idx) = &self.code_index else {
            return;
        };
        let Some(spans) = idx.render_query(&query, None) else {
            return;
        };
        let full = response.joined_text();
        if !search_fold_shrinks(&full, &spans) {
            return;
        }
        let blob = response
            .blob
            .clone()
            .or_else(|| self.blobs.put(full.as_bytes()).ok());
        let mut folded = ToolResponse::text(
            call.id.clone(),
            search_fold_text(&full, &query, &spans, blob.as_deref()),
            response.state.clone(),
        );
        folded.blob = blob;
        folded.original_chars = full.chars().count();
        *response = folded;
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

    /// dsh `FS_NOT_OBSERVED`, narrowed to the one destructive case: `write`
    /// replaces the whole file, so overwriting one the transcript never saw
    /// destroys content the model cannot know. `edit` needs no version guard —
    /// its exact `old_string` match is already a content CAS. Costs one `read`
    /// only when it actually fires.
    pub(crate) fn refuse_blind_overwrite(&self, call: &ToolCall) -> Option<ToolResponse> {
        if dispatch_name(&call.name) != "write" {
            return None;
        }
        let raw = fs_tool_path("write", &call.arguments)?;
        if self
            .observed_paths
            .contains(&canon_ws_path(&self.workspace, &raw))
        {
            return None;
        }
        let abs = self.workspace.resolve(&raw).ok()?;
        if !abs.is_file() {
            return None;
        }
        Some(ToolResponse::text(
            &call.id,
            format!(
                "Error: {raw} already exists and has not been Read this session. Write overwrites the whole file. Read(path=\"{raw}\") first; use StrReplace for a local change."
            ),
            ToolState::Error,
        ))
    }

    pub(crate) async fn complete_or_abort(
        &self,
        tools: Option<&[Value]>,
    ) -> Result<Option<ModelTurn>> {
        let prev = self.widen_no_tool_think();
        self.arm_sink();
        let wire = self.wire_messages();
        let result = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Ok(None),
            turn = self.completer.complete(&wire, tools) => Ok(Some(turn?)),
        };
        if let Some(p) = prev {
            self.completer.set_policy(p);
        }
        result
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
        let decision = if self.print || yolo_permit {
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
            return None;
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
        if let Some(refused) = self.refuse_blind_overwrite(call) {
            return refused;
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

    pub(crate) fn commit_tool(&mut self, name: &str, response: ToolResponse) {
        let stored: Vec<crate::session::StoredMedia> = response
            .media
            .iter()
            .map(|p| crate::session::StoredMedia {
                kind: p.kind.as_str().into(),
                mime: p.mime.clone(),
                url: p.url.clone(),
            })
            .collect();
        if response.media.is_empty() {
            self.messages
                .push(ChatMessage::tool(&response.id, response.joined_text()));
        } else {
            self.messages.push(ChatMessage::tool_media(
                &response.id,
                response.joined_text(),
                response.media.clone(),
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
/// (new Agent, old transcript) keeps the same `refuse_blind_overwrite` view.
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

pub(crate) fn write_path_body(call: &ToolCall) -> (&str, &str) {
    let path = call
        .arguments
        .get("path")
        .and_then(|v| v.as_str())
        .or_else(|| call.arguments.get("file_path").and_then(|v| v.as_str()))
        .unwrap_or("");
    let body = call
        .arguments
        .get("contents")
        .and_then(|v| v.as_str())
        .or_else(|| call.arguments.get("content").and_then(|v| v.as_str()))
        .unwrap_or("");
    (path, body)
}

pub(crate) fn bash_cmd(call: &ToolCall) -> &str {
    call.arguments
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

fn preview_args(call: &ToolCall) -> String {
    call.arguments
        .get("path")
        .or_else(|| call.arguments.get("file_path"))
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

/// Spans supplement the dump, they do not replace it: the model asked ripgrep a
/// precise question, so its own hits stay live and the rest stays recallable.
fn search_fold_text(full: &str, query: &str, spans: &str, blob: Option<&str>) -> String {
    format!(
        "{}\n[{} lines total; head kept{}]\n\n[index spans for `{query}`]\n{spans}",
        search_head(full),
        full.lines().count(),
        blob.map(|b| format!("; full output in blob {b} — recall(blob=…)"))
            .unwrap_or_default(),
    )
}

/// Per-round oracle runs are only worth it on a suite this fast.
const ORACLE_MAX_SUITE: Duration = Duration::from_secs(45);
const ORACLE_TAIL_CHARS: usize = 2000;

fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    s.chars().skip(count - n).collect()
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
