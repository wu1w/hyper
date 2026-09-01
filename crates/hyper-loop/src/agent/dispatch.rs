//! Tool execution: gates, AskQuestion, parallel dispatch.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{json, Value};

use super::guard;
use super::notes::{forbids_glob, forbids_grep, forbids_tools};
use super::turn::{NO_TOOL_ANSWER_RESERVE, NO_TOOL_THINK_FLOOR};
use super::verify;
use super::{Agent, Completer, ModelTurn, TokenSink};
use crate::error::Result;
use crate::mcp::{call_dynamic_tool, fetch_mcp_resource, get_dynamic_tools, run_mcp};
use crate::media::{MediaKind, MediaPart};
use crate::memory::run_memory_search;
use crate::paw_loop::fs_tool_path;
use crate::permit::{self, PermitDecision};
use crate::policy::ThinkPolicy;
use crate::session::{
    run_recall, OpenAiToolCall, PolicyReason, SessionEvent, StoredMedia, ToolLifecyclePhase,
};
use crate::skills::run_skill;
use crate::template::ChatMessage;
use crate::tool_calls::{
    CancelFlag, TextBlock, ToolCall, ToolResponse, ToolState, OFFLOAD_TIMEOUT_RATIO,
};
use crate::tools::{run_search, run_tool, view, CodeIndex, Workspace};
use crate::tools_schema::{dispatch_name, has_tool, is_parallel_safe};

/// Per user turn. Hop-1 can still fire four symbols in parallel; later
/// paraphrases get a Success nudge to Read instead of another index scan.
pub(crate) const SEARCH_TURN_CAP: u32 = 4;
pub(crate) const SEARCH_TURN_CAP_MSG: &str =
    "Search budget for this turn is used. Do not call Search again. Read the files already located.";

pub(crate) fn claim_search_slot(calls: &std::sync::atomic::AtomicU32) -> bool {
    calls.fetch_add(1, Ordering::Relaxed) < SEARCH_TURN_CAP
}

pub(crate) const GREP_TURN_CAP: u32 = 12;
pub(crate) const GREP_TURN_CAP_MSG: &str =
    "Grep budget for this turn is used. Read the files already located instead of more Grep.";
pub(crate) const GREP_REPEAT_MSG: &str =
    "Already Grep'd a similar pattern this turn. Read the hits already returned instead of paraphrasing.";
pub(crate) const GREP_FORBIDDEN_MSG: &str = "The user forbade Grep this turn. Read instead.";
const STEER_SKIPPED_MSG: &str =
    "Skipped before launch because the user sent a steering update. Continue from the steering instruction after the paired tool results.";

pub(crate) fn claim_grep_slot(calls: &std::sync::atomic::AtomicU32) -> bool {
    calls.fetch_add(1, Ordering::Relaxed) < GREP_TURN_CAP
}

/// `## [def] path:start-end` / `## path:start-end` from a Search dump line.
pub(crate) fn span_header_path(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix("## ")?;
    let rest = rest.strip_prefix("[def] ").unwrap_or(rest);
    let (path, loc) = rest.rsplit_once(':')?;
    if path.is_empty() || !loc.contains('-') {
        return None;
    }
    Some(path)
}

pub(crate) fn located_search_paths(messages: &[ChatMessage]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for msg in messages {
        if msg.role != "tool" {
            continue;
        }
        let body = msg.content.as_deref().unwrap_or("");
        if body.contains(SEARCH_TURN_CAP_MSG)
            || body.contains(SEARCH_PARAPHRASE_MSG)
            || body.contains(GREP_AFTER_SEARCH_MSG)
            || body.contains(GREP_TURN_CAP_MSG)
            || body.contains(GREP_REPEAT_MSG)
            || body.contains(READ_SEARCH_SPAN_MSG)
            || body.contains(GLOB_AFTER_SEARCH_MSG)
            || body.contains(SHELL_CAT_SEARCH_MSG)
        {
            continue;
        }
        for line in body.lines() {
            let Some(path) = span_header_path(line) else {
                continue;
            };
            if seen.insert(path.to_string()) {
                paths.push(path.to_string());
            }
            if paths.len() >= 8 {
                return paths;
            }
        }
    }
    paths
}

/// Full-file Read of a path Search already showed (with line numbers) is idle.
pub(crate) fn search_dump_covers_read(
    ws: &Workspace,
    path: &str,
    messages: &[ChatMessage],
) -> bool {
    let key = canon_ws_path(ws, path);
    located_search_paths(messages)
        .iter()
        .any(|p| canon_ws_path(ws, p) == key)
}

pub(crate) const SEARCH_PARAPHRASE_MSG: &str =
    "Already searched a similar query this turn. Read the files already located instead of paraphrasing.";
pub(crate) const GREP_AFTER_SEARCH_MSG: &str =
    "Already located via Search this turn. Read the files already located instead of Grep.";
pub(crate) const READ_ALREADY_MSG: &str =
    "Already Read this path this turn. Use that content; do not Read the same file again. Page with offset only if the first Read was truncated.";
pub(crate) const READ_SEARCH_SPAN_MSG: &str =
    "Search already dumped a span from this file this turn. Use that. Read with offset around those lines only if you need more context.";
pub(crate) const GLOB_NAMED_WRITE_MSG: &str =
    "The user already named the file to Write. Do not Glob the parent directory to copy neighbors.";
pub(crate) const GLOB_FORBIDDEN_MSG: &str =
    "The user forbade Glob this turn. Write the named path or Read instead.";
pub(crate) use crate::tools::GLOB_TREE_MSG;
pub(crate) const GLOB_AFTER_SEARCH_MSG: &str =
    "Search already located this file this turn. Use that path; do not Glob for the same name.";
pub(crate) const SHELL_CAT_SEARCH_MSG: &str =
    "Search already dumped a span from this file this turn. Use that; do not Shell cat/head/tail/sed/nl it.";
pub(crate) const SEARCH_NAMED_WRITE_MSG: &str =
    "The user already named the file to Write. Do not Search for it.";
pub(crate) const READ_SIBLING_MSG: &str =
    "The user already named the file to Write. Do not Read sibling files for style. Write the named path.";
pub(crate) const READ_NAMED_NEW_MSG: &str =
    "The user already named this file to Write; it is not on disk yet. Write it instead of Read.";

pub(crate) fn search_query_tokens(query: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut HashSet<String>| {
        if cur.is_empty() {
            return;
        }
        let t = cur.to_ascii_lowercase();
        cur.clear();
        let chars = t.chars().count();
        let cjk = t.chars().any(|c| c as u32 > 127);
        if ((!cjk && t.len() >= 4) || (cjk && chars >= 2)) && !is_search_syntax_token(&t) {
            out.insert(t);
        }
    };
    for c in query.chars() {
        if c.is_ascii_alphanumeric() {
            cur.push(c);
        } else if !c.is_ascii() {
            if cur.chars().all(|x| x.is_ascii()) {
                flush(&mut cur, &mut out);
            }
            cur.push(c);
        } else {
            flush(&mut cur, &mut out);
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn is_search_syntax_token(t: &str) -> bool {
    matches!(
        t,
        "crate"
            | "async"
            | "await"
            | "function"
            | "struct"
            | "enum"
            | "impl"
            | "trait"
            | "class"
            | "const"
            | "static"
            | "export"
            | "unsafe"
            | "interface"
    )
}

pub(crate) fn is_search_paraphrase(a: &str, b: &str) -> bool {
    let ta = search_query_tokens(a);
    let tb = search_query_tokens(b);
    if ta.is_empty() || tb.is_empty() {
        return a.trim().eq_ignore_ascii_case(b.trim());
    }
    if ta == tb {
        return true;
    }
    let (small, large) = if ta.len() <= tb.len() {
        (&ta, &tb)
    } else {
        (&tb, &ta)
    };
    if small.iter().all(|t| large.contains(t)) {
        return true;
    }
    // NL leftover: neither query is a subset ("process" vs "stub"), but
    // three shared content words still means the same locate.
    ta.intersection(&tb).count() >= 3
}

fn search_nudge_reply(lead: &str, messages: &[ChatMessage]) -> String {
    let paths = located_search_paths(messages);
    if paths.is_empty() {
        return lead.to_string();
    }
    let mut s = String::from(lead);
    s.push_str("\nLocated:\n");
    for p in &paths {
        s.push_str("- ");
        s.push_str(p);
        s.push('\n');
    }
    s
}

pub(crate) fn search_cap_reply(messages: &[ChatMessage]) -> String {
    search_nudge_reply(SEARCH_TURN_CAP_MSG, messages)
}

pub(crate) fn search_paraphrase_reply(messages: &[ChatMessage]) -> String {
    search_nudge_reply(SEARCH_PARAPHRASE_MSG, messages)
}

pub(crate) fn grep_after_search_reply(messages: &[ChatMessage]) -> String {
    search_nudge_reply(GREP_AFTER_SEARCH_MSG, messages)
}

pub(crate) fn glob_after_search_reply(messages: &[ChatMessage]) -> String {
    search_nudge_reply(GLOB_AFTER_SEARCH_MSG, messages)
}

/// Last path component of a Glob pattern, if it is a concrete file name.
pub(crate) fn glob_filename(pattern: &str) -> Option<String> {
    let p = pattern.replace('\\', "/");
    let name = p.rsplit('/').next()?.trim();
    if name.is_empty() || name.contains('*') || name.contains('?') || name.contains('[') {
        return None;
    }
    if name.chars().count() < 3 {
        return None;
    }
    Some(name.to_string())
}

pub(crate) fn glob_covered_by_search_paths(pattern: &str, paths: &[String]) -> bool {
    let Some(name) = glob_filename(pattern) else {
        return false;
    };
    paths.iter().any(|p| {
        p.replace('\\', "/")
            .rsplit('/')
            .next()
            .is_some_and(|n| n.eq_ignore_ascii_case(&name))
    })
}

pub(crate) fn glob_covered_by_search(pattern: &str, messages: &[ChatMessage]) -> bool {
    glob_covered_by_search_paths(pattern, &located_search_paths(messages))
}

/// Grep is idle when this turn already Search'd the same locate.
pub(crate) fn grep_covered_by_search(pattern: &str, search_queries: &[String]) -> bool {
    let p = pattern.trim();
    if p.is_empty() {
        return false;
    }
    search_queries.iter().any(|q| is_search_paraphrase(q, p))
}

fn is_code_ident_token(t: &str) -> bool {
    if t.len() < 4 || is_search_syntax_token(&t.to_ascii_lowercase()) {
        return false;
    }
    if t.contains('_') {
        return true;
    }
    // Inner capital: HttpError / enThink. Leading-only "English" is prose.
    let mut chars = t.chars();
    let Some(_) = chars.next() else {
        return false;
    };
    chars.any(|c| c.is_ascii_uppercase()) && t.chars().any(|c| c.is_ascii_lowercase())
}

/// Snake / camel / SCREAMING_SNAKE tokens in a Search query (spaces and
/// `slot.take` dots split). `fn take prefetch` has none — that is a
/// follow-up, not a new symbol.
pub(crate) fn query_code_idents(query: &str) -> Vec<String> {
    let q = query.trim();
    let mut out = Vec::new();
    if !q.contains(' ') && is_code_ident_token(q) {
        out.push(q.to_string());
    }
    for t in q.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        if t.is_empty() || !is_code_ident_token(t) {
            continue;
        }
        if !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    out
}

pub(crate) fn looks_like_code_followup(query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return false;
    }
    if q.contains('.')
        && q.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    {
        return true;
    }
    q.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|w| {
            matches!(
                w,
                "fn" | "pub" | "impl" | "async" | "const" | "let" | "struct" | "mod" | "use"
            )
        })
}

fn dump_is_fold_nudge(body: &str) -> bool {
    body.contains(SEARCH_TURN_CAP_MSG)
        || body.contains(SEARCH_PARAPHRASE_MSG)
        || body.contains(GREP_AFTER_SEARCH_MSG)
        || body.contains(GREP_TURN_CAP_MSG)
        || body.contains(GREP_REPEAT_MSG)
        || body.contains(READ_SEARCH_SPAN_MSG)
        || body.contains(GLOB_AFTER_SEARCH_MSG)
        || body.contains(SHELL_CAT_SEARCH_MSG)
}

fn ident_token_in_dumps(ident: &str, messages: &[ChatMessage]) -> bool {
    for msg in messages {
        if msg.role != "tool" {
            continue;
        }
        let body = msg.content.as_deref().unwrap_or("");
        if dump_is_fold_nudge(body) {
            continue;
        }
        if body
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|t| t == ident)
        {
            return true;
        }
    }
    false
}

/// Tokens from this turn's Search dumps, for speculate skip. Capped.
pub(crate) fn shown_dump_idents(messages: &[ChatMessage]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for msg in messages {
        if msg.role != "tool" {
            continue;
        }
        let body = msg.content.as_deref().unwrap_or("");
        if dump_is_fold_nudge(body) {
            continue;
        }
        for t in body.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            if t.len() < 4 || !is_code_ident_token(t) || !seen.insert(t) {
                continue;
            }
            out.push(t.to_string());
            if out.len() >= 128 {
                return out;
            }
        }
    }
    out
}

/// A later Search for a symbol already printed in this turn's dump is idle.
/// Spaced `fn take …` / `slot.take` queries with no new snake/camel ident
/// also fold once a Search dump already named a file (Read that file).
pub(crate) fn search_ident_already_shown(query: &str, messages: &[ChatMessage]) -> bool {
    let q = query.trim();
    if q.len() < 4 {
        return false;
    }
    let idents = query_code_idents(q);
    if idents.iter().any(|id| ident_token_in_dumps(id, messages)) {
        return true;
    }
    idents.is_empty() && looks_like_code_followup(q) && !located_search_paths(messages).is_empty()
}

impl<C: Completer> Agent<C> {
    pub(crate) fn emit_tools_scheduled(&mut self, calls: &[ToolCall]) {
        for call in calls {
            self.emit_tool_lifecycle(
                call,
                ToolLifecyclePhase::Scheduled,
                Some(preview_args(call)),
            );
        }
    }

    pub(crate) async fn execute_tools(&mut self, calls: Vec<ToolCall>) -> bool {
        if self.code_index.is_none() && calls.iter().any(needs_search_index) {
            self.start_code_index();
            self.settle_code_index().await;
        }
        if let Some(h) = self.in_flight_diag.take() {
            h.abort();
        }
        for call in &calls {
            self.note(&format!("[{}] {}", call.name, preview_args(call)));
        }
        let write_priors = self.snapshot_write_priors(&calls);
        let (mut responses, skipped) = self.dispatch_with_prefetch(&calls).await;
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
                    let key = canon_ws_path(&self.workspace, &path);
                    self.read_paths.insert(key.clone());
                    if dispatch_name(&call.name) == "read" && read_is_full(call) {
                        crate::lock_unpoison(&self.read_full).insert(key);
                    }
                }
            }
            if matches!(
                dispatch_name(&call.name),
                "read" | "view" | "write" | "edit" | "editnotebook"
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
                if matches!(dispatch_name(&call.name), "edit" | "write" | "editnotebook") {
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
                if dispatch_name(&call.name) == "generateimage" {
                    for part in &response.media {
                        if crate::channel::xfer::is_sendable_rel(&part.url) {
                            self.channel_files.push(part.url.clone());
                        }
                    }
                }
            }
            if response.state == ToolState::Success {
                if let Some(idx) = &self.code_index {
                    if let Some(folded) = fold_search_dump_for(idx, call, &response.joined_text()) {
                        response.content = vec![TextBlock { text: folded }];
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
        let diag = if let Some(h) = self.in_flight_diag.take() {
            h.await.ok().flatten()
        } else {
            verify::run_diagnostics_async(self.workspace.root(), &edited, &self.cancel).await
        };
        if let Some(diag) = diag.as_ref() {
            if let Some(id) = last_edit_id.as_deref() {
                if let Some(i) = calls.iter().position(|c| c.id == id) {
                    responses[i].content.push(TextBlock { text: diag.clone() });
                }
            }
            self.note("[diagnostics]");
        }
        self.progress.fold_and_observe(
            &self.workspace,
            &calls,
            &mut responses,
            test_red,
            saw_test_output,
            diag.as_deref(),
        );
        for (call, response) in calls.iter().zip(responses) {
            let state = response.state.clone();
            let summary = response.joined_text();
            self.commit_tool(&call.name, response);
            let phase = if skipped.contains(&call.id) {
                ToolLifecyclePhase::Skipped
            } else {
                match state {
                    ToolState::Success => ToolLifecyclePhase::Completed,
                    ToolState::Error => ToolLifecyclePhase::Error,
                    ToolState::Interrupted => ToolLifecyclePhase::Interrupted,
                }
            };
            self.emit_tool_lifecycle(call, phase, Some(clip_lifecycle_summary(&summary)));
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
        self.progress.should_synthesize()
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
        if dispatch_name(name) == "computeruse" && !has_tool(&self.tools, "ComputerUse") {
            return Some(unknown_tool_reply(&call.id, name));
        }
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
        if dispatch_name(name) == "fetchmcpresource"
            && !permit::fetch_writes_workspace(&call.arguments)
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
        let hop = self.model_hops.fetch_add(1, Ordering::Relaxed);
        loop {
            if self.cancel.is_cancelled() {
                return Ok(None);
            }
            self.arm_sink();
            // Hop 0: leave execute_turn's PREPARE_HINT until the first token.
            // Later hops: clear the previous think panel. Do not paint
            // CONNECT_HINT — that made every tool round look like a reconnect.
            if attempt == 0 && hop > 0 {
                if let Some(sink) = self.live_sink() {
                    sink.reset();
                }
            }
            let wire = self.wire_messages();
            let result = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Ok(None),
                turn = self.completer.complete(&wire, tools) => turn,
            };
            match result {
                Ok(turn) => {
                    if attempt == 0 && is_blank_transport(&turn) {
                        attempt += 1;
                        if let Some(slot) = self.completer.speculate() {
                            slot.abort();
                        }
                        self.note("[net] empty completion; retry once");
                        continue;
                    }
                    return Ok(Some(turn));
                }
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
        if self.force_synthesis {
            // Wrap hops use a tight max_output_tokens / think cap. Raising to
            // the generic 8k floor is what let Grok think for 14 minutes.
            return None;
        }
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
        // Old headless integrations without a hub still skip instead of
        // stalling. In-process IM installs a real hub and must get first chance.
        let im_skip = self.child.is_none() && !super::interactive_channel(&self.channel);
        let decision = if self.print || yolo_permit {
            crate::clarify::ClarifyDecision::Skip
        } else if let Some(hub) = &self.clarify {
            hub.ask(ask.clone(), &self.cancel).await
        } else if im_skip {
            crate::clarify::ClarifyDecision::Skip
        } else {
            return ToolResponse::text(
                &call.id,
                "Error: AskQuestion needs an interactive ClarifyHub. Do not assume Skip.",
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
        let sink = self.live_sink().map(|sink| {
            if self.force_synthesis {
                sink.content_only()
            } else {
                sink
            }
        });
        self.completer.set_token_sink(sink);
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
        } else if !super::interactive_channel(&self.channel) && !self.channel.is_empty() {
            // IM has no console sink. Still stream: cli-chat-proxy rejects
            // non-SSE POSTs, and non-stream JSON often has `"error": null`.
            Some(TokenSink::discard())
        } else {
            None
        }
    }

    /// None = run Search. Some = Success nudge (paraphrase or turn cap).
    fn search_gate(&self, call: &ToolCall) -> Option<String> {
        let query = crate::tools::arg_str(&call.arguments, "query").unwrap_or_default();
        if self.idle_named_write_search(&query) {
            return Some(search_nudge_reply(SEARCH_NAMED_WRITE_MSG, &self.messages));
        }
        let mut prev = crate::lock_unpoison(&self.search_queries);
        if !query.trim().is_empty() && prev.iter().any(|p| is_search_paraphrase(p, &query)) {
            return Some(search_paraphrase_reply(&self.messages));
        }
        if search_ident_already_shown(&query, &self.messages) {
            return Some(search_paraphrase_reply(&self.messages));
        }
        if !claim_search_slot(&self.search_calls) {
            return Some(search_cap_reply(&self.messages));
        }
        if !query.trim().is_empty() {
            prev.push(query);
        }
        None
    }

    fn grep_gate(&self, call: &ToolCall) -> Option<String> {
        if forbids_grep(self.last_real_user()) {
            return Some(search_nudge_reply(GREP_FORBIDDEN_MSG, &self.messages));
        }
        if let Some(pattern) = crate::tools::arg_str(&call.arguments, "pattern") {
            let covered = {
                let queries = crate::lock_unpoison(&self.search_queries);
                grep_covered_by_search(&pattern, &queries)
            };
            if covered {
                return Some(grep_after_search_reply(&self.messages));
            }
            let mut prev = crate::lock_unpoison(&self.grep_queries);
            if !pattern.trim().is_empty() && prev.iter().any(|p| is_search_paraphrase(p, &pattern))
            {
                return Some(search_nudge_reply(GREP_REPEAT_MSG, &self.messages));
            }
            if !self.grep_is_file_scoped(call) && !claim_grep_slot(&self.grep_calls) {
                return Some(search_nudge_reply(GREP_TURN_CAP_MSG, &self.messages));
            }
            if !pattern.trim().is_empty() {
                prev.push(pattern);
            }
            return None;
        }
        if !self.grep_is_file_scoped(call) && !claim_grep_slot(&self.grep_calls) {
            return Some(search_nudge_reply(GREP_TURN_CAP_MSG, &self.messages));
        }
        None
    }

    fn grep_is_file_scoped(&self, call: &ToolCall) -> bool {
        let Some(raw) = crate::tools::arg_path(&call.arguments) else {
            return false;
        };
        self.workspace
            .resolve(&raw)
            .map(|p| p.is_file())
            .unwrap_or(false)
    }

    fn read_gate(&self, call: &ToolCall) -> Option<String> {
        if dispatch_name(&call.name) != "read" {
            return None;
        }
        let path = crate::tools::arg_str(&call.arguments, "path")?;
        if self.idle_named_write_self_read(&path) {
            return Some(search_nudge_reply(READ_NAMED_NEW_MSG, &self.messages));
        }
        if self.idle_named_write_read(&path) {
            return Some(search_nudge_reply(READ_SIBLING_MSG, &self.messages));
        }
        if search_dump_covers_read(&self.workspace, &path, &self.messages)
            && read_repeats_search_span(call)
        {
            return Some(search_nudge_reply(READ_SEARCH_SPAN_MSG, &self.messages));
        }
        if !read_is_full(call) {
            return None;
        }
        let key = canon_ws_path(&self.workspace, &path);
        if crate::lock_unpoison(&self.read_full).contains(&key) {
            return Some(search_nudge_reply(READ_ALREADY_MSG, &self.messages));
        }
        None
    }

    fn glob_gate(&self, call: &ToolCall) -> Option<String> {
        if dispatch_name(&call.name) != "glob" {
            return None;
        }
        if forbids_glob(self.last_real_user()) {
            return Some(search_nudge_reply(GLOB_FORBIDDEN_MSG, &self.messages));
        }
        let pat = crate::tools::arg_str(&call.arguments, "glob_pattern").unwrap_or_default();
        let dir = crate::tools::arg_str(&call.arguments, "target_directory").unwrap_or_default();
        if glob_covered_by_search(&pat, &self.messages) {
            return Some(search_nudge_reply(GLOB_AFTER_SEARCH_MSG, &self.messages));
        }
        let joined = if dir.is_empty() {
            pat
        } else {
            format!("{}/{pat}", dir.trim_end_matches('/'))
        };
        if self.idle_named_write_glob(&joined) {
            return Some(search_nudge_reply(GLOB_NAMED_WRITE_MSG, &self.messages));
        }
        None
    }

    fn bash_cat_gate(&self, call: &ToolCall) -> Option<String> {
        if dispatch_name(&call.name) != "bash" {
            return None;
        }
        let cmd = crate::tools::arg_str(&call.arguments, "command")?;
        let path = crate::tools::cat_like_path(&cmd)?;
        if search_dump_covers_read(&self.workspace, &path, &self.messages) {
            return Some(search_nudge_reply(SHELL_CAT_SEARCH_MSG, &self.messages));
        }
        None
    }

    fn named_new_files(&self) -> Vec<String> {
        named_new_files(&self.workspace, self.last_real_user())
    }

    fn idle_named_write_glob(&self, pattern: &str) -> bool {
        let Some(stem) = glob_parent_stem(pattern) else {
            return false;
        };
        let stem_key = canon_ws_path(&self.workspace, &stem);
        self.named_new_files().iter().any(|n| {
            parent_slash(n)
                .is_some_and(|p| p == stem_key || canon_ws_path(&self.workspace, p) == stem_key)
        })
    }

    fn idle_named_write_self_read(&self, path: &str) -> bool {
        let read = canon_ws_path(&self.workspace, path);
        self.named_new_files().iter().any(|n| *n == read)
    }

    fn idle_named_write_read(&self, path: &str) -> bool {
        let read = canon_ws_path(&self.workspace, path);
        let named = self.named_new_files();
        named.iter().any(|n| {
            if read == *n {
                return false;
            }
            let Some(np) = parent_slash(n) else {
                return false;
            };
            let np = canon_ws_path(&self.workspace, np);
            read == np
                || parent_slash(&read).is_some_and(|rp| canon_ws_path(&self.workspace, rp) == np)
        })
    }

    fn idle_named_write_search(&self, query: &str) -> bool {
        let q = query
            .trim()
            .trim_matches('`')
            .trim_matches('"')
            .replace('\\', "/");
        let q = q.trim();
        if q.is_empty() {
            return false;
        }
        let q_key = canon_ws_path(&self.workspace, q);
        self.named_new_files().iter().any(|n| {
            if *n == q || *n == q_key {
                return true;
            }
            has_code_ext(q) && !q.contains('/') && n.rsplit('/').next() == Some(q)
        })
    }

    pub(crate) async fn dispatch_one(&self, call: &ToolCall) -> ToolResponse {
        if let Some(denied) = self.gate_tool(call).await {
            return denied;
        }
        if crate::subagent::handles(&call.name) {
            return crate::subagent::dispatch(call, &self.dispatch_ctx()).await;
        }
        if dispatch_name(&call.name) == "grep" {
            if let Some(msg) = self.grep_gate(call) {
                return ToolResponse::text(&call.id, msg, ToolState::Success);
            }
        }
        if dispatch_name(&call.name) == "read" {
            if let Some(msg) = self.read_gate(call) {
                return ToolResponse::text(&call.id, msg, ToolState::Success);
            }
            if media_read_path(call).is_some() {
                return self.dispatch_view(call).await;
            }
        }
        if dispatch_name(&call.name) == "glob" {
            if let Some(msg) = self.glob_gate(call) {
                return ToolResponse::text(&call.id, msg, ToolState::Success);
            }
        }
        if dispatch_name(&call.name) == "bash" {
            if let Some(msg) = self.bash_cat_gate(call) {
                return ToolResponse::text(&call.id, msg, ToolState::Success);
            }
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
            "search" => {
                if !has_tool(&self.tools, "Search") {
                    return unknown_tool_reply(&call.id, &call.name);
                }
                match &self.code_index {
                    Some(idx) => {
                        if let Some(msg) = self.search_gate(call) {
                            return ToolResponse::text(&call.id, msg, ToolState::Success);
                        }
                        run_search(idx, &self.workspace, call, self.limits)
                    }
                    None => ToolResponse::text(
                        &call.id,
                        crate::tools::SEARCH_WARMING,
                        ToolState::Success,
                    ),
                }
            }
            "readlints" => {
                let mut paths: Vec<String> = call
                    .arguments
                    .get("paths")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .map(str::to_string)
                    .collect();
                if let Some(path) = crate::tools::arg_str(&call.arguments, "path") {
                    if !path.trim().is_empty() && !paths.iter().any(|item| item == &path) {
                        paths.push(path);
                    }
                }
                if paths.is_empty() {
                    paths.extend(
                        self.observed_paths
                            .iter()
                            .filter(|path| verify::is_code_path(path))
                            .cloned(),
                    );
                    paths.sort();
                }
                if paths.is_empty() {
                    return ToolResponse::text(
                        &call.id,
                        "Error: ReadLints needs `paths` before any code file has been observed.",
                        ToolState::Error,
                    );
                }
                let report =
                    verify::run_lints_async(self.workspace.root(), &paths, &self.cancel).await;
                let (text, state) = verify::read_lints_reply(&report, &paths);
                ToolResponse::text(&call.id, text, state)
            }
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
            "getdynamictools" => {
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
                            r = get_dynamic_tools(&mcp, &owned, limits, Some(&blobs)) => r,
                        };
                        link.abort();
                        res
                    })
                    .await
            }
            "calldynamictool" => {
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
                            r = call_dynamic_tool(&mcp, &owned, limits, Some(&blobs)) => r,
                        };
                        link.abort();
                        res
                    })
                    .await
            }
            "fetchmcpresource" => {
                let mcp = self.mcp.clone();
                let blobs = self.blobs.clone();
                let limits = self.limits;
                let owned = call.clone();
                let ws = self.workspace.clone();
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
                            r = fetch_mcp_resource(
                                &mcp,
                                &owned,
                                limits,
                                Some(&blobs),
                                Some(&ws),
                            ) => r,
                        };
                        link.abort();
                        res
                    })
                    .await
            }
            "generateimage" => {
                let prompt = crate::tools::arg_str_any(&call.arguments, &["description", "prompt"])
                    .unwrap_or_default();
                let filename = crate::tools::arg_str(&call.arguments, "filename");
                let cfg = self.config.clone();
                let root = self.workspace.root().to_path_buf();
                let owned = call.clone();
                let agent_cancel = self.cancel.clone();
                let ws = self.workspace.clone();
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
                            r = crate::imagine::generate(&cfg, &prompt, &root, &merged) => {
                                match r {
                                    Ok(out) => finish_generate_image(&owned.id, out, filename.as_deref(), &ws),
                                    Err(e) => ToolResponse::text(
                                        &owned.id,
                                        format!("Error: {e}"),
                                        ToolState::Error,
                                    ),
                                }
                            },
                        };
                        link.abort();
                        res
                    })
                    .await
            }
            // Not in tools[]. XML / hallucinated native calls still need a result.
            "skill" => run_skill(&self.skills, call, self.limits, Some(&self.blobs)),
            "view" => {
                if !has_tool(&self.tools, "view") {
                    return unknown_tool_reply(&call.id, &call.name);
                }
                self.dispatch_view(call).await
            }
            "computeruse" => {
                if !has_tool(&self.tools, "ComputerUse") {
                    return unknown_tool_reply(&call.id, &call.name);
                }
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
                    .execute(
                        call.clone(),
                        "hyper",
                        bash_coordinator_timeout_secs(call),
                        move |per_call| async move {
                            let (merged, link) = spawn_cancel_bridge(cancel, per_call);
                            let res =
                                run_tool(&ws, &owned, merged, limits, inherit_env, Some(&blobs))
                                    .await;
                            link.abort();
                            res
                        },
                    )
                    .await
            }
        }
    }

    async fn dispatch_view(&self, call: &ToolCall) -> ToolResponse {
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

    pub(crate) async fn dispatch_parallel(&self, calls: &[ToolCall]) -> Vec<ToolResponse> {
        // Same handlers as the serial path. `parallel_safe_batch` admits
        // read/view/search/web/readlints/todowrite; mutating tools still run serially.
        futures::future::join_all(calls.iter().map(|c| self.dispatch_one(c))).await
    }

    /// Prefetch already ran the tool. Still apply `gate_tool` so a later
    /// permit/plan/child deny cannot leak the prefetched body, then the
    /// same idle Search/Grep/Read/Glob folds as `dispatch_one`.
    async fn finish_prefetch(&self, call: &ToolCall, r: ToolResponse) -> ToolResponse {
        if let Some(denied) = self.gate_tool(call).await {
            return denied;
        }
        match dispatch_name(&call.name) {
            "search" => self
                .search_gate(call)
                .map(|msg| ToolResponse::text(&call.id, msg, ToolState::Success))
                .unwrap_or(r),
            "grep" => self
                .grep_gate(call)
                .map(|msg| ToolResponse::text(&call.id, msg, ToolState::Success))
                .unwrap_or(r),
            "read" => self
                .read_gate(call)
                .map(|msg| ToolResponse::text(&call.id, msg, ToolState::Success))
                .unwrap_or(r),
            "glob" => self
                .glob_gate(call)
                .map(|msg| ToolResponse::text(&call.id, msg, ToolState::Success))
                .unwrap_or(r),
            _ => r,
        }
    }

    fn kick_diagnostics(&mut self, edited: &[String]) {
        if edited.is_empty() {
            return;
        }
        if let Some(h) = self.in_flight_diag.take() {
            h.abort();
        }
        let root = self.workspace.root().to_path_buf();
        let cancel = self.cancel.clone();
        let edited = edited.to_vec();
        self.in_flight_diag = Some(tokio::spawn(async move {
            verify::run_diagnostics_async(&root, &edited, &cancel).await
        }));
    }

    fn maybe_kick_diag(
        &mut self,
        call: &ToolCall,
        response: &ToolResponse,
        edited: &mut Vec<String>,
    ) {
        if response.state != ToolState::Success {
            return;
        }
        if !matches!(dispatch_name(&call.name), "edit" | "write" | "editnotebook") {
            return;
        }
        let Some(path) = fs_tool_path(&call.name, &call.arguments) else {
            return;
        };
        if !verify::is_code_path(&path) {
            return;
        }
        edited.push(path);
        self.kick_diagnostics(edited);
    }

    async fn dispatch_with_prefetch(
        &mut self,
        calls: &[ToolCall],
    ) -> (Vec<ToolResponse>, HashSet<String>) {
        let mut out: Vec<Option<ToolResponse>> = vec![None; calls.len()];
        let mut skipped = HashSet::new();
        let mut diag_paths: Vec<String> = Vec::new();
        let slot = self.speculate.clone();
        if parallel_safe_batch(calls) {
            if let Some(slot) = &slot {
                for (i, call) in calls.iter().enumerate() {
                    if let Some(r) = slot.take(&call.id).await {
                        self.emit_tool_lifecycle(
                            call,
                            ToolLifecyclePhase::Started,
                            Some(preview_args(call)),
                        );
                        let r = self.finish_prefetch(call, r).await;
                        self.maybe_kick_diag(call, &r, &mut diag_paths);
                        out[i] = Some(r);
                    }
                }
            }
            let pending: Vec<(usize, ToolCall)> = calls
                .iter()
                .enumerate()
                .filter(|(i, _)| out[*i].is_none())
                .map(|(i, c)| (i, c.clone()))
                .collect();
            if !pending.is_empty() {
                let pending_calls: Vec<ToolCall> = pending.iter().map(|(_, c)| c.clone()).collect();
                let rest = if crate::channel::has_steer(&self.steer) {
                    pending_calls
                        .iter()
                        .map(|call| {
                            skipped.insert(call.id.clone());
                            ToolResponse::text(&call.id, STEER_SKIPPED_MSG, ToolState::Interrupted)
                        })
                        .collect()
                } else {
                    for call in &pending_calls {
                        self.emit_tool_lifecycle(
                            call,
                            ToolLifecyclePhase::Started,
                            Some(preview_args(call)),
                        );
                    }
                    self.dispatch_parallel(&pending_calls).await
                };
                for ((i, _), r) in pending.into_iter().zip(rest) {
                    out[i] = Some(r);
                }
            }
        } else {
            for (i, call) in calls.iter().enumerate() {
                if crate::channel::has_steer(&self.steer) {
                    skipped.insert(call.id.clone());
                    out[i] = Some(ToolResponse::text(
                        &call.id,
                        STEER_SKIPPED_MSG,
                        ToolState::Interrupted,
                    ));
                    continue;
                }
                if let Some(slot) = &slot {
                    if let Some(r) = slot.take(&call.id).await {
                        self.emit_tool_lifecycle(
                            call,
                            ToolLifecyclePhase::Started,
                            Some(preview_args(call)),
                        );
                        let r = self.finish_prefetch(call, r).await;
                        self.maybe_kick_diag(call, &r, &mut diag_paths);
                        out[i] = Some(r);
                        continue;
                    }
                }
                self.emit_tool_lifecycle(
                    call,
                    ToolLifecyclePhase::Started,
                    Some(preview_args(call)),
                );
                let r = if dispatch_name(&call.name) == "switchmode" {
                    self.run_switch_mode(call)
                } else {
                    self.dispatch_one(call).await
                };
                self.maybe_kick_diag(call, &r, &mut diag_paths);
                out[i] = Some(r);
            }
        }
        if let Some(slot) = &slot {
            slot.abort();
        }
        let mut responses: Vec<ToolResponse> = out
            .into_iter()
            .map(|r| r.expect("every tool call has a response"))
            .collect();
        self.fold_idle_search(calls, &mut responses);
        self.fold_idle_grep(calls, &mut responses);
        self.fold_idle_read(calls, &mut responses);
        self.fold_idle_glob(calls, &mut responses);
        self.fold_idle_cat(calls, &mut responses);
        for (call, response) in calls.iter().zip(responses.iter_mut()) {
            if skipped.contains(&call.id) {
                *response = ToolResponse::text(&call.id, STEER_SKIPPED_MSG, ToolState::Interrupted);
            }
        }
        (responses, skipped)
    }

    fn run_switch_mode(&mut self, call: &ToolCall) -> ToolResponse {
        let mode = crate::tools::arg_str_any(&call.arguments, &["mode", "target_mode_id"])
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let text = match mode.as_str() {
            "plan" => {
                self.plan_mode = true;
                self.clarify_mode = true;
                "Switched to plan mode. Inspect the workspace and write only plan.md; all other mutations are blocked. Ask a blocking question if a user choice changes the plan."
            }
            "agent" => {
                self.plan_mode = false;
                self.clarify_mode = false;
                "Switched to agent mode. Continue implementing and verify the result."
            }
            "ask" => {
                self.plan_mode = false;
                self.clarify_mode = true;
                "Switched to ask mode. Use AskQuestion now and wait for the user's answer before continuing."
            }
            _ => {
                return ToolResponse::text(
                    &call.id,
                    "Error: SwitchMode mode must be agent, plan, or ask.",
                    ToolState::Error,
                );
            }
        };
        if super::im_bridge_channel(&self.channel) {
            crate::channel::interaction::set_agent_mode(&self.session_id, &mode);
        }
        ToolResponse::text(&call.id, text, ToolState::Success)
    }

    pub(crate) fn emit_tool_lifecycle(
        &mut self,
        call: &ToolCall,
        phase: ToolLifecyclePhase,
        summary: Option<String>,
    ) {
        let Some((run_id, turn_id)) = self.lifecycle_ids() else {
            return;
        };
        self.log_event(SessionEvent::tool_lifecycle(
            run_id,
            turn_id,
            self.current_step,
            &call.id,
            &call.name,
            phase,
            summary,
        ));
    }

    fn fold_idle_search(&self, calls: &[ToolCall], responses: &mut [ToolResponse]) {
        let mut msgs = self.messages.clone();
        let mut hop_queries: Vec<String> = Vec::new();
        for (call, response) in calls.iter().zip(responses.iter_mut()) {
            if dispatch_name(&call.name) != "search" {
                continue;
            }
            let q = crate::tools::arg_str(&call.arguments, "query").unwrap_or_default();
            let text = response.joined_text();
            if text.contains(SEARCH_TURN_CAP_MSG) || text.contains(SEARCH_PARAPHRASE_MSG) {
                hop_queries.push(q);
                msgs.push(ChatMessage::tool(&call.id, text));
                continue;
            }
            let idle = hop_queries.iter().any(|p| is_search_paraphrase(p, &q))
                || search_ident_already_shown(&q, &msgs);
            if idle {
                *response = ToolResponse::text(
                    &call.id,
                    search_paraphrase_reply(&msgs),
                    ToolState::Success,
                );
            }
            hop_queries.push(q);
            msgs.push(ChatMessage::tool(&call.id, response.joined_text()));
        }
    }

    fn fold_idle_grep(&self, calls: &[ToolCall], responses: &mut [ToolResponse]) {
        let queries = crate::lock_unpoison(&self.search_queries).clone();
        if queries.is_empty() {
            return;
        }
        let mut msgs = self.messages.clone();
        for (call, response) in calls.iter().zip(responses.iter()) {
            if dispatch_name(&call.name) == "search" {
                msgs.push(ChatMessage::tool(&call.id, response.joined_text()));
            }
        }
        for (call, response) in calls.iter().zip(responses.iter_mut()) {
            if dispatch_name(&call.name) != "grep" {
                continue;
            }
            let Some(pattern) = crate::tools::arg_str(&call.arguments, "pattern") else {
                continue;
            };
            if !grep_covered_by_search(&pattern, &queries) {
                continue;
            }
            *response =
                ToolResponse::text(&call.id, grep_after_search_reply(&msgs), ToolState::Success);
        }
    }

    fn fold_idle_read(&self, calls: &[ToolCall], responses: &mut [ToolResponse]) {
        let mut msgs = self.messages.clone();
        for (call, response) in calls.iter().zip(responses.iter()) {
            if dispatch_name(&call.name) == "search" {
                msgs.push(ChatMessage::tool(&call.id, response.joined_text()));
            }
        }
        let mut seen = crate::lock_unpoison(&self.read_full).clone();
        for (call, response) in calls.iter().zip(responses.iter_mut()) {
            if dispatch_name(&call.name) != "read" {
                continue;
            }
            if response.joined_text().contains(READ_ALREADY_MSG)
                || response.joined_text().contains(READ_SEARCH_SPAN_MSG)
            {
                continue;
            }
            let Some(path) = crate::tools::arg_str(&call.arguments, "path") else {
                continue;
            };
            let key = canon_ws_path(&self.workspace, &path);
            if search_dump_covers_read(&self.workspace, &path, &msgs)
                && read_repeats_search_span(call)
            {
                *response = ToolResponse::text(
                    &call.id,
                    search_nudge_reply(READ_SEARCH_SPAN_MSG, &self.messages),
                    ToolState::Success,
                );
                continue;
            }
            if !read_is_full(call) {
                continue;
            }
            if seen.contains(&key) {
                *response = ToolResponse::text(
                    &call.id,
                    search_nudge_reply(READ_ALREADY_MSG, &self.messages),
                    ToolState::Success,
                );
                continue;
            }
            if response.state == ToolState::Success {
                seen.insert(key);
            }
        }
    }

    fn fold_idle_glob(&self, calls: &[ToolCall], responses: &mut [ToolResponse]) {
        let mut msgs = self.messages.clone();
        for (call, response) in calls.iter().zip(responses.iter()) {
            if dispatch_name(&call.name) == "search" {
                msgs.push(ChatMessage::tool(&call.id, response.joined_text()));
            }
        }
        for (call, response) in calls.iter().zip(responses.iter_mut()) {
            if dispatch_name(&call.name) != "glob" {
                continue;
            }
            let text = response.joined_text();
            if text.contains(GLOB_TREE_MSG) || text.contains(GLOB_AFTER_SEARCH_MSG) {
                continue;
            }
            let pat = crate::tools::arg_str(&call.arguments, "glob_pattern").unwrap_or_default();
            if !glob_covered_by_search(&pat, &msgs) {
                continue;
            }
            *response =
                ToolResponse::text(&call.id, glob_after_search_reply(&msgs), ToolState::Success);
        }
    }

    fn fold_idle_cat(&self, calls: &[ToolCall], responses: &mut [ToolResponse]) {
        let mut msgs = self.messages.clone();
        for (call, response) in calls.iter().zip(responses.iter()) {
            if dispatch_name(&call.name) == "search" {
                msgs.push(ChatMessage::tool(&call.id, response.joined_text()));
            }
        }
        for (call, response) in calls.iter().zip(responses.iter_mut()) {
            if dispatch_name(&call.name) != "bash" {
                continue;
            }
            if response.joined_text().contains(SHELL_CAT_SEARCH_MSG) {
                continue;
            }
            let Some(cmd) = crate::tools::arg_str(&call.arguments, "command") else {
                continue;
            };
            let Some(path) = crate::tools::cat_like_path(&cmd) else {
                continue;
            };
            if !search_dump_covers_read(&self.workspace, &path, &msgs) {
                continue;
            }
            *response = ToolResponse::text(
                &call.id,
                search_nudge_reply(SHELL_CAT_SEARCH_MSG, &msgs),
                ToolState::Success,
            );
        }
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

fn finish_generate_image(
    id: &str,
    out: crate::imagine::ImagineOut,
    filename: Option<&str>,
    ws: &Workspace,
) -> ToolResponse {
    let mut urls: Vec<String> = Vec::new();
    if let Some(name) = filename.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(src) = out.stored.first() {
            match copy_generated_file(ws, &src.url, name) {
                Ok(shown) => urls.push(shown),
                Err(e) => {
                    let msg = if e.starts_with("Error:") {
                        e
                    } else {
                        format!("Error: {e}")
                    };
                    return ToolResponse::text(id, msg, ToolState::Error);
                }
            }
        }
    }
    for stored in &out.stored {
        if !urls.iter().any(|u| u == &stored.url) {
            urls.push(stored.url.clone());
        }
    }
    let mut lines = Vec::new();
    if !out.caption.trim().is_empty() {
        lines.push(out.caption.trim().to_string());
    }
    for url in &urls {
        lines.push(format!("Saved {url}"));
    }
    if lines.is_empty() {
        lines.push("Generated image saved.".into());
    }
    let mut response = ToolResponse::text(id, lines.join("\n"), ToolState::Success);
    response.media = out
        .stored
        .into_iter()
        .map(|s| MediaPart {
            kind: MediaKind::parse(&s.kind).unwrap_or(MediaKind::Image),
            mime: s.mime,
            url: s.url,
        })
        .collect();
    response
}

fn copy_generated_file(
    ws: &Workspace,
    src_rel: &str,
    dest_rel: &str,
) -> std::result::Result<String, String> {
    let src = ws.resolve(src_rel)?;
    let dest = ws.resolve(dest_rel)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::copy(&src, &dest).map_err(|e| e.to_string())?;
    Ok(ws.shown(dest_rel))
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

fn is_blank_transport(turn: &super::ModelTurn) -> bool {
    turn.content.trim().is_empty()
        && turn.reasoning.trim().is_empty()
        && turn.tool_calls.is_empty()
        && !turn.watchdog_hit
        && !turn.parse_fail
        && turn.prompt_tokens == 0
        && turn.completion_tokens == 0
}

fn clip_lifecycle_summary(text: &str) -> String {
    let one = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= 180 {
        one
    } else {
        format!("{}…", one.chars().take(179).collect::<String>())
    }
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

pub(crate) fn read_is_full(call: &ToolCall) -> bool {
    crate::tools::arg_u32(&call.arguments, "offset").is_none()
        && crate::tools::arg_u32(&call.arguments, "start_line").is_none()
        && crate::tools::arg_u32(&call.arguments, "limit").is_none()
        && crate::tools::arg_u32(&call.arguments, "end_line").is_none()
}

/// Full file, or a from-start dump big enough to repeat the Search span.
/// Tiny pages (`offset: 1, limit: 1`) still run.
pub(crate) fn read_repeats_search_span(call: &ToolCall) -> bool {
    if read_is_full(call) {
        return true;
    }
    let off = crate::tools::arg_u32(&call.arguments, "offset")
        .or_else(|| crate::tools::arg_u32(&call.arguments, "start_line"))
        .unwrap_or(1);
    if off > 1 {
        return false;
    }
    let limit = crate::tools::arg_u32(&call.arguments, "limit");
    let end = crate::tools::arg_u32(&call.arguments, "end_line");
    match (limit, end) {
        (None, None) => true,
        (Some(n), _) if n >= 32 => true,
        (_, Some(e)) if e >= 32 => true,
        _ => false,
    }
}

fn parent_slash(path: &str) -> Option<&str> {
    let p = path.trim_end_matches('/').trim_end_matches('\\');
    let idx = p.rfind('/').or_else(|| p.rfind('\\'))?;
    let a = &p[..idx];
    if a.is_empty() {
        None
    } else {
        Some(a)
    }
}

pub(crate) fn glob_target_is_workspace_root(ws: &Workspace, target_directory: &str) -> bool {
    let dir = target_directory.trim();
    if dir.is_empty() || dir == "." || dir == "./" {
        return true;
    }
    let Ok(resolved) = ws.resolve(dir) else {
        return false;
    };
    let root = ws.root();
    if resolved == root {
        return true;
    }
    match (resolved.canonicalize(), root.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `**` / `**/*` / `**/*.*` from the walk root. `**/*.rs` and `**/*.{rs,md}` are not.
pub(crate) fn recursive_any_file_glob(pattern: &str) -> bool {
    let p = pattern
        .trim()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string();
    crate::tools::is_unfiltered_tree_glob(&p) && (p == "**" || p.starts_with("**/"))
}

fn glob_parent_stem(pattern: &str) -> Option<String> {
    let p = pattern.replace('\\', "/");
    let stem = p
        .trim_end_matches('*')
        .trim_end_matches('/')
        .trim_end_matches('*')
        .trim_end_matches('/');
    if stem.is_empty() || stem == "." || stem == "**" {
        return None;
    }
    Some(stem.to_string())
}

fn looks_like_named_file(t: &str) -> bool {
    (t.contains('/') || t.contains('\\')) && has_code_ext(t)
}

fn has_code_ext(t: &str) -> bool {
    let lower = t.to_ascii_lowercase();
    [
        ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".md", ".toml", ".json", ".go", ".css", ".html",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

/// Workspace-relative files the live user already named that are not on disk yet.
pub(crate) fn named_new_files(ws: &Workspace, user: &str) -> Vec<String> {
    let normalized: String = user
        .chars()
        .map(|c| match c {
            '「' | '」' | '『' | '』' | '“' | '”' | '‘' | '’' | '（' | '）' | '、' | '`' | '：'
            | '。' | '，' | '；' | '！' | '？' | '—' => ' ',
            other => other,
        })
        .collect();
    let mut out = Vec::new();
    for raw in normalized.split_whitespace() {
        let t = raw.trim_matches(|c: char| {
            matches!(
                c,
                '`' | '"' | '\'' | ',' | ';' | ':' | ')' | '(' | '[' | ']' | '。' | '，'
            )
        });
        if t.contains("..") || !looks_like_named_file(t) {
            continue;
        }
        if ws
            .resolve(t)
            .map(|p| p.is_file() || p.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let key = canon_ws_path(ws, t);
        if !out.iter().any(|p| p == &key) {
            out.push(key);
        }
        if out.len() >= 8 {
            break;
        }
    }
    out
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
    if matches!(dispatch_name(&call.name), "search") {
        if let Some(q) = call
            .arguments
            .get("query")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            return q.to_string();
        }
    }
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

pub(crate) fn media_read_path(call: &ToolCall) -> Option<String> {
    if dispatch_name(&call.name) != "read" {
        return None;
    }
    let path = crate::tools::arg_path(&call.arguments)?;
    crate::media::is_media_ext(&path).then_some(path)
}

fn unknown_tool_reply(id: &str, name: &str) -> ToolResponse {
    let hint = match dispatch_name(name) {
        "view" => " Use Read for images.",
        "search" => " Use Grep.",
        "computeruse" => " Desktop control is off unless features.computer_use is enabled.",
        _ => "",
    };
    ToolResponse::text(
        id,
        format!("Error: unknown tool '{name}'.{hint}"),
        ToolState::Error,
    )
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

/// Explicit `block_until_ms` is foreground wait. Coordinator offloads at 50% of
/// kill, so stretch kill until that window matches the model's request.
pub(crate) fn bash_coordinator_timeout_secs(call: &ToolCall) -> Option<f64> {
    if dispatch_name(&call.name) != "bash" {
        return None;
    }
    let ms = crate::tools::arg_u32(&call.arguments, "block_until_ms")
        .or_else(|| crate::tools::arg_u32(&call.arguments, "timeout_ms"))
        .filter(|&n| n > 0)?;
    Some((ms as f64) / 1000.0 / OFFLOAD_TIMEOUT_RATIO)
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

fn needs_search_index(call: &ToolCall) -> bool {
    match dispatch_name(&call.name) {
        "search" | "grep" => true,
        "bash" => crate::tools::arg_str(&call.arguments, "command")
            .and_then(|cmd| crate::tools::bash_search_query(&cmd))
            .is_some(),
        _ => false,
    }
}

fn fold_search_dump_for(index: &CodeIndex, call: &ToolCall, full: &str) -> Option<String> {
    let query = match dispatch_name(&call.name) {
        "bash" => {
            let cmd = crate::tools::arg_str(&call.arguments, "command")?;
            crate::tools::bash_search_query(&cmd)?
        }
        "grep" => crate::tools::arg_str(&call.arguments, "pattern")?,
        _ => return None,
    };
    fold_search_dump(index, &query, full)
}

/// Keep a short rg/grep head and replace the rest with index spans.
pub(crate) fn fold_search_dump(index: &CodeIndex, query: &str, full: &str) -> Option<String> {
    if query.trim().is_empty() || !crate::tools::search_dump_too_big(full) {
        return None;
    }
    let spans = crate::tools::render_query_spans(index, query);
    if spans.is_empty() || !search_fold_shrinks(full, &spans) {
        return None;
    }
    Some(format!("{}\n\n{spans}", search_head(full)))
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
