//! Blocking IM controls for AskQuestion and mutating-tool approval.
//!
//! The agent-side hubs remain transport-neutral. This module turns their
//! requests into chat messages and lets the per-session worker consume the
//! next human reply before normal busy/steer routing sees it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::clarify::{ClarifyAsk, ClarifyDecision, ClarifyRequest};
use crate::permit::{ApprovalMode, PermitDecision, PermitHub, PermitRequest, PlanAction};
use crate::slash::SlashCmd;

use super::envelope::NativePayload;
use super::progress::ImLocale;
use super::ChannelEndpoint;

/// IM sessions default to ask unless `/approvals` or a controls file says otherwise.
const IM_DEFAULT_APPROVALS: ApprovalMode = ApprovalMode::Ask;
/// Ask / permit cards expire; a late click gets an explicit stale reply.
const PROMPT_TTL: Duration = Duration::from_secs(15 * 60);

struct Prompt {
    id: String,
    owner: String,
    zh: bool,
    born: Instant,
    /// Platform message id of the native choice card, if we got one.
    card_id: Option<String>,
    /// Original prompt body, so settle can keep the question and mark the picker.
    prompt_text: String,
    kind: PendingKind,
}

enum PendingKind {
    Clarify {
        ask: ClarifyAsk,
        reply: tokio::sync::oneshot::Sender<ClarifyDecision>,
    },
    Permit {
        tool: String,
        reply: tokio::sync::oneshot::Sender<PermitDecision>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionControls {
    pub plan: bool,
    pub clarify: bool,
    pub approvals: ApprovalMode,
    /// Empty = use process config model.
    pub model: String,
    /// First speaker in a group-bound session; mutating slashes must match.
    pub owner: String,
}

#[derive(Serialize, Deserialize, Default)]
struct DiskControls {
    #[serde(default)]
    plan: bool,
    #[serde(default)]
    clarify: bool,
    #[serde(default)]
    approvals: String,
    #[serde(default)]
    always: Vec<String>,
    #[serde(default)]
    model: String,
    #[serde(default)]
    owner: String,
}

struct State {
    pending: HashMap<String, VecDeque<Prompt>>,
    controls: HashMap<String, SessionControls>,
    always: HashMap<String, HashSet<String>>,
    hubs: HashMap<String, PermitHub>,
    pumps: HashMap<String, ChannelEndpoint>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(State {
            pending: HashMap::new(),
            controls: HashMap::new(),
            always: HashMap::new(),
            hubs: HashMap::new(),
            pumps: HashMap::new(),
        })
    })
}

/// Keeps the live `PermitHub` reachable so `/approvals yolo` can unblock the
/// current turn, not only the next one.
pub(crate) struct HubGuard {
    session: String,
}

impl HubGuard {
    pub(crate) fn bind(session: &str, hub: PermitHub) -> Self {
        if !session.is_empty() {
            let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
            state.hubs.insert(session.to_string(), hub);
        }
        Self {
            session: session.to_string(),
        }
    }
}

impl Drop for HubGuard {
    fn drop(&mut self) {
        if self.session.is_empty() {
            return;
        }
        let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
        state.hubs.remove(&self.session);
    }
}

fn controls_path(session: &str) -> Option<PathBuf> {
    let dir = crate::session::SessionLog::sessions_dir().ok()?;
    let safe: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        return None;
    }
    Some(dir.join(format!("{safe}.controls.json")))
}

fn load_disk(session: &str) -> Option<(SessionControls, HashSet<String>)> {
    let path = controls_path(session)?;
    let raw = std::fs::read_to_string(path).ok()?;
    let disk: DiskControls = serde_json::from_str(&raw).ok()?;
    let approvals = ApprovalMode::parse(&disk.approvals).unwrap_or(IM_DEFAULT_APPROVALS);
    Some((
        SessionControls {
            plan: disk.plan,
            clarify: disk.clarify || disk.plan,
            approvals,
            model: disk.model,
            owner: disk.owner,
        },
        disk.always.into_iter().collect(),
    ))
}

fn persist_session(session: &str) {
    let (controls, always) = {
        let state = state().lock().unwrap_or_else(|e| e.into_inner());
        let Some(controls) = state.controls.get(session).cloned() else {
            return;
        };
        let always = state.always.get(session).cloned().unwrap_or_default();
        (controls, always)
    };
    let Some(path) = controls_path(session) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut always_tools: Vec<String> = always.into_iter().collect();
    always_tools.sort();
    let disk = DiskControls {
        plan: controls.plan,
        clarify: controls.clarify,
        approvals: controls.approvals.as_str().to_string(),
        always: always_tools,
        model: controls.model,
        owner: controls.owner,
    };
    let Ok(body) = serde_json::to_string_pretty(&disk) else {
        return;
    };
    if std::fs::write(&path, body).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

fn ensure_loaded(state: &mut State, session: &str, default_approvals: ApprovalMode) {
    if state.controls.contains_key(session) {
        return;
    }
    if let Some((controls, always)) = load_disk(session) {
        state
            .always
            .entry(session.to_string())
            .or_default()
            .extend(always);
        state.controls.insert(session.to_string(), controls);
        return;
    }
    state.controls.insert(
        session.to_string(),
        SessionControls {
            plan: false,
            clarify: false,
            approvals: default_approvals,
            model: String::new(),
            owner: String::new(),
        },
    );
}

pub(crate) fn controls(session: &str, default_approvals: ApprovalMode) -> SessionControls {
    let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
    ensure_loaded(&mut state, session, default_approvals);
    state.controls.get(session).expect("ensure_loaded").clone()
}

pub(crate) fn claim_owner(session: &str, sender: &str) {
    if session.is_empty() || sender.is_empty() {
        return;
    }
    {
        let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
        ensure_loaded(&mut state, session, IM_DEFAULT_APPROVALS);
        let entry = state.controls.get_mut(session).expect("ensure_loaded");
        if !entry.owner.is_empty() {
            return;
        }
        entry.owner = sender.to_string();
    }
    persist_session(session);
}

pub(crate) fn deny_foreign_control(env: &NativePayload, cmd: &SlashCmd) -> Option<String> {
    if !env.is_group() || !slash_mutates_session(cmd) {
        return None;
    }
    let owner = controls(&env.session_id, IM_DEFAULT_APPROVALS).owner;
    if owner.is_empty() || owner == env.sender_id {
        return None;
    }
    let zh = matches!(
        ImLocale::detect_channel(&env.query_text(), &env.channel),
        ImLocale::Zh
    );
    Some(if zh {
        "只有本会话发起者可以改控制项。".into()
    } else {
        "Only the user who started this session can change its controls.".into()
    })
}

fn slash_mutates_session(cmd: &SlashCmd) -> bool {
    matches!(
        cmd,
        SlashCmd::Approvals { .. }
            | SlashCmd::Plan { .. }
            | SlashCmd::Clarify { .. }
            | SlashCmd::Model { .. }
            | SlashCmd::Compress { .. }
            | SlashCmd::Undo
            | SlashCmd::Busy { .. }
            | SlashCmd::Background { .. }
            | SlashCmd::Title { .. }
            | SlashCmd::LowPrecision { .. }
    )
}

pub(crate) fn set_model(session: &str, name: &str) {
    {
        let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
        ensure_loaded(&mut state, session, IM_DEFAULT_APPROVALS);
        state
            .controls
            .get_mut(session)
            .expect("ensure_loaded")
            .model = name.to_string();
    }
    persist_session(session);
}

pub(crate) fn set_plan(session: &str, on: bool) {
    {
        let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
        ensure_loaded(&mut state, session, IM_DEFAULT_APPROVALS);
        let entry = state.controls.get_mut(session).expect("ensure_loaded");
        entry.plan = on;
        if on {
            entry.clarify = true;
        }
    }
    persist_session(session);
}

pub(crate) fn set_clarify(session: &str, on: bool) {
    {
        let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
        ensure_loaded(&mut state, session, IM_DEFAULT_APPROVALS);
        state
            .controls
            .get_mut(session)
            .expect("ensure_loaded")
            .clarify = on;
    }
    persist_session(session);
}

pub(crate) fn set_agent_mode(session: &str, mode: &str) {
    match mode {
        "plan" => {
            set_plan(session, true);
            set_clarify(session, true);
        }
        "ask" => {
            set_plan(session, false);
            set_clarify(session, true);
        }
        "agent" => {
            set_plan(session, false);
            set_clarify(session, false);
        }
        _ => {}
    }
}

pub(crate) fn set_approvals(session: &str, mode: ApprovalMode) {
    {
        let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
        ensure_loaded(&mut state, session, mode);
        state
            .controls
            .get_mut(session)
            .expect("ensure_loaded")
            .approvals = mode;
        if let Some(hub) = state.hubs.get(session) {
            hub.set_mode(mode);
        }
    }
    persist_session(session);
    resolve_pending_for_mode(session, mode);
}

fn resolve_pending_for_mode(session: &str, mode: ApprovalMode) {
    let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
    expire_locked(&mut state, session);
    let Some(queue) = state.pending.get_mut(session) else {
        return;
    };
    let mut kept = VecDeque::new();
    while let Some(prompt) = queue.pop_front() {
        match prompt.kind {
            PendingKind::Permit { tool, reply } => {
                let allow = match mode {
                    ApprovalMode::Yolo => true,
                    ApprovalMode::Auto => !PermitHub::needs_prompt(ApprovalMode::Auto, &tool),
                    ApprovalMode::Ask => false,
                };
                if allow {
                    let _ = reply.send(PermitDecision::Allow);
                } else {
                    kept.push_back(Prompt {
                        kind: PendingKind::Permit { tool, reply },
                        ..prompt
                    });
                }
            }
            other => kept.push_back(Prompt {
                kind: other,
                ..prompt
            }),
        }
    }
    *queue = kept;
}

pub(crate) fn remembered_tools(session: &str) -> Vec<String> {
    let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
    ensure_loaded(&mut state, session, IM_DEFAULT_APPROVALS);
    state
        .always
        .get(session)
        .map(|tools| tools.iter().cloned().collect())
        .unwrap_or_default()
}

pub(crate) enum Answer {
    Accepted(String),
    Rejected(String),
    None,
}

#[derive(Debug)]
pub(crate) enum MidTurn {
    Reply(String),
    Drop,
    Pass,
}

/// Busy-loop routing: slash controls, permit/clarify answers, leftover cards.
pub(crate) fn mid_turn(env: &NativePayload) -> MidTurn {
    if let Some(cmd) = crate::slash::parse_slash(&env.query_text()) {
        if matches!(
            cmd,
            SlashCmd::Stop
                | SlashCmd::Queue { .. }
                | SlashCmd::Steer { .. }
                | SlashCmd::Busy { .. }
                | SlashCmd::Background { .. }
        ) {
            return MidTurn::Pass;
        }
        return MidTurn::Reply(live_slash_reply(env, cmd));
    }
    match answer(env) {
        Answer::Accepted(reply) | Answer::Rejected(reply) => MidTurn::Reply(reply),
        Answer::None if env.is_choice_click() => MidTurn::Drop,
        Answer::None => MidTurn::Pass,
    }
}

fn live_slash_reply(env: &NativePayload, cmd: SlashCmd) -> String {
    if let Some(denied) = deny_foreign_control(env, &cmd) {
        return denied;
    }
    match cmd {
        SlashCmd::Approvals { mode } => {
            if let Some(mode) = mode {
                set_approvals(&env.session_id, mode);
            }
            crate::slash::approvals_text(controls(&env.session_id, IM_DEFAULT_APPROVALS).approvals)
        }
        SlashCmd::Plan { action, prompt } => match action {
            PlanAction::On => {
                set_plan(&env.session_id, true);
                let text = crate::slash::plan_text(true);
                if prompt.filter(|p| !p.trim().is_empty()).is_some() {
                    format!("{text}\n当前任务进行中，计划说明会在本轮结束后再生效。")
                } else {
                    text
                }
            }
            PlanAction::Off => {
                set_plan(&env.session_id, false);
                crate::slash::plan_text(false)
            }
            PlanAction::Go => "当前任务进行中。结束后再发送 `/plan go`。".into(),
        },
        SlashCmd::Clarify { on } => {
            if let Some(on) = on {
                set_clarify(&env.session_id, on);
            }
            let controls = controls(&env.session_id, IM_DEFAULT_APPROVALS);
            crate::slash::clarify_text(controls.clarify, controls.plan)
        }
        SlashCmd::Help => crate::slash::help_text(),
        SlashCmd::Version => crate::slash::version_text(),
        other => format!(
            "当前任务进行中，`{}` 未执行。可以 `/stop`，或等本轮结束再发。",
            slash_label(&other)
        ),
    }
}

fn slash_label(cmd: &SlashCmd) -> &'static str {
    match cmd {
        SlashCmd::Approvals { .. } => "/approvals",
        SlashCmd::Plan { .. } => "/plan",
        SlashCmd::Clarify { .. } => "/clarify",
        SlashCmd::Help => "/help",
        SlashCmd::Version => "/version",
        SlashCmd::Status => "/status",
        SlashCmd::Usage => "/usage",
        SlashCmd::History => "/history",
        SlashCmd::Model { .. } => "/model",
        SlashCmd::Compress { .. } => "/compact",
        SlashCmd::Undo => "/undo",
        SlashCmd::Background { .. } => "/background",
        SlashCmd::New { .. } => "/new",
        SlashCmd::Resume { .. } => "/resume",
        SlashCmd::Title { .. } => "/title",
        SlashCmd::Retry => "/retry",
        SlashCmd::Skills => "/skills",
        SlashCmd::Mcp => "/mcp",
        SlashCmd::Cron { .. } => "/cron",
        SlashCmd::Unsupported { .. } => "/…",
        _ => "/…",
    }
}

/// Consume a message as the answer to a pending interaction. Only the user who
/// started the run can answer in a shared chat. Tagged native clicks
/// (`p:{id}:{choice}`) address a specific prompt; untagged `1`/`2`/`3` take FIFO.
pub(crate) fn answer(env: &NativePayload) -> Answer {
    let text = env.query_text();
    let trimmed = text.trim();
    if crate::slash::parse_slash(trimmed).is_some() {
        return Answer::None;
    }
    let zh_env = matches!(
        ImLocale::detect_channel(&env.query_text(), &env.channel),
        ImLocale::Zh
    );
    let tagged = split_tagged(trimmed);
    if trimmed.is_empty() && tagged.is_none() && !env.is_choice_click() {
        return Answer::None;
    }
    let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
    expire_locked(&mut state, &env.session_id);
    let payload = tagged.as_ref().map(|(_, p)| p.as_str()).unwrap_or(trimmed);
    if payload.is_empty() && tagged.is_none() && !env.is_choice_click() {
        return Answer::None;
    }
    let Some(queue) = state.pending.get_mut(&env.session_id) else {
        if tagged.is_some() || env.is_choice_click() {
            return Answer::Rejected(expired_text(zh_env));
        }
        return Answer::None;
    };
    let idx = if let Some((id, _)) = tagged.as_ref() {
        match queue.iter().position(|p| &p.id == id) {
            Some(i) => i,
            None => return Answer::Rejected(expired_text(zh_env)),
        }
    } else if env.is_choice_click() {
        // Untagged leftover card click after the queue moved on.
        if queue.is_empty() {
            return Answer::Rejected(expired_text(zh_env));
        }
        0
    } else {
        0
    };
    let Some(front) = queue.get(idx) else {
        if tagged.is_some() || env.is_choice_click() {
            return Answer::Rejected(expired_text(zh_env));
        }
        return Answer::None;
    };
    let owner = front.owner.clone();
    let zh = front.zh;
    if !owner.is_empty() && owner != env.sender_id {
        return Answer::Rejected(if zh {
            "这项操作需要由任务发起者回复。".into()
        } else {
            "Only the user who started this task can answer.".into()
        });
    }
    if payload.is_empty() {
        return Answer::Rejected(expired_text(zh));
    }

    enum Resolved {
        Clarify {
            decision: ClarifyDecision,
            label: String,
        },
        Permit {
            decision: PermitDecision,
            tool: String,
        },
    }
    let resolved = match &front.kind {
        PendingKind::Clarify { ask, .. } => {
            let Some(decision) = clarify_decision(ask, payload) else {
                return Answer::Rejected(if zh {
                    "请回复选项序号/ID、`/skip`，或直接输入其他答案。".into()
                } else {
                    "Reply with an option number/ID, `/skip`, or type another answer.".into()
                });
            };
            let label = match &decision {
                ClarifyDecision::Pick { label, .. } => label.clone(),
                ClarifyDecision::Other { text } => text.clone(),
                ClarifyDecision::Skip => ask
                    .recommended()
                    .map(|o| o.label.clone())
                    .unwrap_or_else(|| "skip".into()),
            };
            Resolved::Clarify { decision, label }
        }
        PendingKind::Permit { tool, .. } => {
            let Some(decision) = permit_decision(payload) else {
                return Answer::Rejected(if zh {
                    "请回复 `1`（允许一次）、`2`（本会话始终允许）或 `3`（拒绝）。".into()
                } else {
                    "Reply `1` (allow once), `2` (always this session), or `3` (deny).".into()
                });
            };
            Resolved::Permit {
                decision,
                tool: tool.clone(),
            }
        }
    };
    let prompt = queue.remove(idx).expect("index checked");
    if queue.is_empty() {
        state.pending.remove(&env.session_id);
    }
    let ep = state.pumps.get(&env.session_id).cloned();
    let persist_always = matches!(
        &resolved,
        Resolved::Permit {
            decision: PermitDecision::Always,
            ..
        }
    );
    if persist_always {
        if let Resolved::Permit { tool, .. } = &resolved {
            state
                .always
                .entry(env.session_id.clone())
                .or_default()
                .insert(tool.clone());
        }
    }
    let card_id = prompt.card_id.clone();
    let prompt_text = prompt.prompt_text.clone();
    match prompt.kind {
        PendingKind::Clarify { reply, .. } => {
            if let Resolved::Clarify { decision, .. } = &resolved {
                let _ = reply.send(decision.clone());
            }
        }
        PendingKind::Permit { reply, .. } => {
            if let Resolved::Permit { decision, .. } = &resolved {
                let _ = reply.send(*decision);
            }
        }
    }
    drop(state);
    if persist_always {
        persist_session(&env.session_id);
    }
    let summary = match resolved {
        Resolved::Clarify { label, .. } => {
            let action = if zh {
                format!("已选择：{label}")
            } else {
                format!("Selected: {label}")
            };
            choice_marked(env, zh, &action)
        }
        Resolved::Permit { decision, tool } => {
            let label = match decision {
                PermitDecision::Allow => {
                    if zh {
                        "已允许一次"
                    } else {
                        "Allowed once"
                    }
                }
                PermitDecision::Always => {
                    if zh {
                        "本会话始终允许"
                    } else {
                        "Always allowed for this session"
                    }
                }
                PermitDecision::Deny => {
                    if zh {
                        "已拒绝"
                    } else {
                        "Denied"
                    }
                }
            };
            choice_marked(env, zh, &format!("{label}: {tool}"))
        }
    };
    if let (Some(card_id), Some(ep)) = (card_id, ep) {
        let env = env.clone();
        let body = settle_body(&prompt_text, &summary);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = super::outbound::settle_choices(Some(&ep), &env, &card_id, &body).await;
            });
        }
    }
    Answer::Accepted(summary)
}

fn expired_text(zh: bool) -> String {
    if zh {
        "这条选项已过期。".into()
    } else {
        "This prompt has expired.".into()
    }
}

fn chooser_name(env: &NativePayload) -> String {
    let name = env.sender_name.trim();
    if !name.is_empty() {
        name.to_string()
    } else {
        env.sender_id.trim().to_string()
    }
}

fn choice_marked(env: &NativePayload, zh: bool, action: &str) -> String {
    let who = chooser_name(env);
    if who.is_empty() {
        action.to_string()
    } else if zh {
        format!("{who} {action}")
    } else {
        format!("{who} · {action}")
    }
}

fn settle_body(prompt_text: &str, summary: &str) -> String {
    let prompt = prompt_text.trim();
    if prompt.is_empty() {
        summary.to_string()
    } else {
        format!("{prompt}\n\n——\n{summary}")
    }
}

fn split_tagged(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix("p:")?;
    let (id, payload) = rest.split_once(':')?;
    if id.len() == 8 && id.chars().all(|c| c.is_ascii_alphanumeric()) {
        Some((id.to_string(), payload.to_string()))
    } else {
        None
    }
}

/// DingTalk `dtmd://` and webhook `choices` arrive as ordinary text. Stamp
/// them like a native click so group mention gates and leftover-card acks
/// still fire.
pub(crate) fn stamp_tagged_choice(env: &mut NativePayload) {
    if split_tagged(env.query_text().trim()).is_none() {
        return;
    }
    env.mark_choice_click();
    if env.is_group() {
        env.meta
            .insert("is_mentioned".into(), serde_json::json!(true));
    }
}

fn mint_prompt_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

fn expire_locked(state: &mut State, session: &str) {
    let Some(queue) = state.pending.remove(session) else {
        return;
    };
    let now = Instant::now();
    let mut kept = VecDeque::new();
    for prompt in queue {
        if now.saturating_duration_since(prompt.born) >= PROMPT_TTL {
            drop_prompt(prompt);
        } else {
            kept.push_back(prompt);
        }
    }
    if !kept.is_empty() {
        state.pending.insert(session.to_string(), kept);
    }
}

fn drop_prompt(prompt: Prompt) {
    match prompt.kind {
        PendingKind::Permit { reply, .. } => {
            let _ = reply.send(PermitDecision::Deny);
        }
        PendingKind::Clarify { reply, .. } => {
            let _ = reply.send(ClarifyDecision::Skip);
        }
    }
}

fn enqueue_prompt(session: &str, prompt: Prompt) {
    let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
    state
        .pending
        .entry(session.to_string())
        .or_default()
        .push_back(prompt);
}

fn set_prompt_card(session: &str, id: &str, card_id: Option<String>) {
    let Some(card_id) = card_id.filter(|s| !s.is_empty()) else {
        return;
    };
    let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
    let Some(queue) = state.pending.get_mut(session) else {
        return;
    };
    if let Some(prompt) = queue.iter_mut().find(|p| p.id == id) {
        prompt.card_id = Some(card_id);
    }
}

fn take_prompt(session: &str, id: &str) -> Option<Prompt> {
    let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
    let prompt = {
        let queue = state.pending.get_mut(session)?;
        let idx = queue.iter().position(|p| p.id == id)?;
        queue.remove(idx)?
    };
    if state
        .pending
        .get(session)
        .map(|q| q.is_empty())
        .unwrap_or(true)
    {
        state.pending.remove(session);
    }
    Some(prompt)
}

fn tag_buttons(prompt_id: &str, buttons: Vec<(String, String)>) -> Vec<(String, String)> {
    buttons
        .into_iter()
        .map(|(value, label)| {
            let tagged = format!("p:{prompt_id}:{value}");
            let value = if tagged.len() <= 64 { tagged } else { value };
            (value, label)
        })
        .collect()
}

fn clarify_decision(ask: &ClarifyAsk, text: &str) -> Option<ClarifyDecision> {
    let lower = text.to_ascii_lowercase();
    if matches!(lower.as_str(), "/skip" | "skip" | "跳过" | "略过") {
        return Some(ClarifyDecision::Skip);
    }
    if let Ok(index) = text.parse::<usize>() {
        if let Some(option) = index.checked_sub(1).and_then(|i| ask.options.get(i)) {
            return Some(ClarifyDecision::Pick {
                id: option.id.clone(),
                label: option.label.clone(),
            });
        }
    }
    if let Some(option) = ask
        .options
        .iter()
        .find(|o| o.id.eq_ignore_ascii_case(text) || o.label.eq_ignore_ascii_case(text))
    {
        return Some(ClarifyDecision::Pick {
            id: option.id.clone(),
            label: option.label.clone(),
        });
    }
    (!text.is_empty()).then(|| ClarifyDecision::Other {
        text: text.to_string(),
    })
}

fn permit_decision(text: &str) -> Option<PermitDecision> {
    match text.trim().to_ascii_lowercase().as_str() {
        "1" | "allow" | "yes" | "允许" | "同意" | "仅这次" => Some(PermitDecision::Allow),
        "2" | "always" | "always allow" | "始终允许" | "本会话允许" => {
            Some(PermitDecision::Always)
        }
        "3" | "deny" | "no" | "拒绝" | "不允许" => Some(PermitDecision::Deny),
        _ => None,
    }
}

pub(crate) fn clear_pending(session: &str) {
    let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(queue) = state.pending.remove(session) {
        for prompt in queue {
            drop_prompt(prompt);
        }
    }
}

pub(crate) fn spawn_pump(
    ep: Option<ChannelEndpoint>,
    env: NativePayload,
    mut permits: mpsc::UnboundedReceiver<PermitRequest>,
    mut clarifies: mpsc::UnboundedReceiver<ClarifyRequest>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        clear_pending(&env.session_id);
        if let Some(ep) = ep.clone() {
            let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
            state.pumps.insert(env.session_id.clone(), ep);
        }
        let mut permit_open = true;
        let mut clarify_open = true;
        while permit_open || clarify_open {
            tokio::select! {
                request = permits.recv(), if permit_open => match request {
                    Some(request) => {
                        let mode = controls(&env.session_id, IM_DEFAULT_APPROVALS).approvals;
                        let remembered = remembered_tools(&env.session_id);
                        if !PermitHub::needs_prompt(mode, &request.ask.tool)
                            || remembered.iter().any(|t| t == &request.ask.tool)
                        {
                            let _ = request.reply.send(PermitDecision::Allow);
                            continue;
                        }
                        let zh = matches!(ImLocale::detect(&env.query_text()), ImLocale::Zh);
                        let text = permit_prompt(&request, zh);
                        let id = mint_prompt_id();
                        let buttons = tag_buttons(&id, permit_buttons(zh));
                        enqueue_prompt(&env.session_id, Prompt {
                            id: id.clone(),
                            owner: env.sender_id.clone(),
                            zh,
                            born: Instant::now(),
                            card_id: None,
                            prompt_text: text.clone(),
                            kind: PendingKind::Permit {
                                tool: request.ask.tool.clone(),
                                reply: request.reply,
                            },
                        });
                        match super::outbound::deliver_choices(
                            ep.as_ref(),
                            &env,
                            &text,
                            &buttons,
                            Instant::now(),
                        ).await {
                            Ok(card) => set_prompt_card(&env.session_id, &id, card),
                            Err(err) => {
                                eprintln!("hyper channel approval prompt: {err}");
                                if let Some(prompt) = take_prompt(&env.session_id, &id) {
                                    drop_prompt(prompt);
                                }
                            }
                        }
                    }
                    None => permit_open = false,
                },
                request = clarifies.recv(), if clarify_open => match request {
                    Some(request) => {
                        let zh = matches!(ImLocale::detect(&env.query_text()), ImLocale::Zh);
                        let text = clarify_prompt(&request.ask, zh);
                        let id = mint_prompt_id();
                        let buttons = tag_buttons(&id, clarify_buttons(&request.ask));
                        enqueue_prompt(&env.session_id, Prompt {
                            id: id.clone(),
                            owner: env.sender_id.clone(),
                            zh,
                            born: Instant::now(),
                            card_id: None,
                            prompt_text: text.clone(),
                            kind: PendingKind::Clarify {
                                ask: request.ask,
                                reply: request.reply,
                            },
                        });
                        match super::outbound::deliver_choices(
                            ep.as_ref(),
                            &env,
                            &text,
                            &buttons,
                            Instant::now(),
                        ).await {
                            Ok(card) => set_prompt_card(&env.session_id, &id, card),
                            Err(err) => {
                                eprintln!("hyper channel question prompt: {err}");
                                if let Some(prompt) = take_prompt(&env.session_id, &id) {
                                    drop_prompt(prompt);
                                }
                            }
                        }
                    }
                    None => clarify_open = false,
                },
            }
        }
        {
            let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
            state.pumps.remove(&env.session_id);
        }
        clear_pending(&env.session_id);
    })
}

fn permit_buttons(zh: bool) -> Vec<(String, String)> {
    if zh {
        vec![
            ("1".into(), "仅这次允许".into()),
            ("2".into(), "本会话始终允许".into()),
            ("3".into(), "拒绝".into()),
        ]
    } else {
        vec![
            ("1".into(), "Allow once".into()),
            ("2".into(), "Always this session".into()),
            ("3".into(), "Deny".into()),
        ]
    }
}

fn clarify_buttons(ask: &ClarifyAsk) -> Vec<(String, String)> {
    ask.options
        .iter()
        .enumerate()
        .map(|(index, option)| {
            let id = if option.id.trim().is_empty() {
                (index + 1).to_string()
            } else {
                option.id.clone()
            };
            (id, option.label.clone())
        })
        .collect()
}

fn clarify_prompt(ask: &ClarifyAsk, zh: bool) -> String {
    let mut lines = if zh {
        vec![format!("需要你的选择 · {}", ask.title), ask.prompt.clone()]
    } else {
        vec![
            format!("Your choice is needed · {}", ask.title),
            ask.prompt.clone(),
        ]
    };
    for (index, option) in ask.options.iter().enumerate() {
        let recommended = if index == 0 {
            if zh {
                "（推荐）"
            } else {
                " (recommended)"
            }
        } else {
            ""
        };
        lines.push(format!("{}. {}{}", index + 1, option.label, recommended));
    }
    lines.push(if zh {
        "回复序号/选项 ID；`/skip` 采用推荐项；也可直接输入其他答案。".into()
    } else {
        "Reply with a number/option ID; `/skip` uses the recommendation; or type another answer."
            .into()
    });
    lines.join("\n")
}

fn permit_prompt(request: &PermitRequest, zh: bool) -> String {
    if zh {
        format!(
            "需要批准\n工具：{}\n{}\n\n1. 仅这次允许\n2. 本会话始终允许\n3. 拒绝",
            request.ask.tool, request.ask.preview
        )
    } else {
        format!(
            "Approval needed\nTool: {}\n{}\n\n1. Allow once\n2. Always allow this session\n3. Deny",
            request.ask.tool, request.ask.preview
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clarify::ClarifyOption;
    use serde_json::json;

    fn enqueue_permit(
        session: &str,
        owner: &str,
        zh: bool,
        tool: &str,
        reply: tokio::sync::oneshot::Sender<PermitDecision>,
    ) -> String {
        let id = mint_prompt_id();
        enqueue_prompt(
            session,
            Prompt {
                id: id.clone(),
                owner: owner.into(),
                zh,
                born: Instant::now(),
                card_id: None,
                prompt_text: String::new(),
                kind: PendingKind::Permit {
                    tool: tool.into(),
                    reply,
                },
            },
        );
        id
    }

    fn group_env(channel: &str, text: &str, sender: &str, session: &str) -> NativePayload {
        let mut env = NativePayload::text_only(channel, text);
        env.session_id = session.into();
        env.sender_id = sender.into();
        env.meta.insert("is_group".into(), json!(true));
        env.meta.insert("chat_id".into(), json!("oc_room"));
        env
    }

    #[test]
    fn parses_question_choices_and_other() {
        let ask = ClarifyAsk {
            title: "Scope".into(),
            prompt: "Which?".into(),
            options: vec![
                ClarifyOption {
                    id: "all".into(),
                    label: "All".into(),
                },
                ClarifyOption {
                    id: "api".into(),
                    label: "API".into(),
                },
            ],
        };
        assert!(
            matches!(clarify_decision(&ask, "2"), Some(ClarifyDecision::Pick { id, .. }) if id == "api")
        );
        assert!(
            matches!(clarify_decision(&ask, "all"), Some(ClarifyDecision::Pick { id, .. }) if id == "all")
        );
        assert_eq!(clarify_decision(&ask, "/skip"), Some(ClarifyDecision::Skip));
        assert!(matches!(
            clarify_decision(&ask, "custom"),
            Some(ClarifyDecision::Other { .. })
        ));
    }

    #[test]
    fn parses_permission_choices() {
        assert_eq!(permit_decision("1"), Some(PermitDecision::Allow));
        assert_eq!(permit_decision("始终允许"), Some(PermitDecision::Always));
        assert_eq!(permit_decision("deny"), Some(PermitDecision::Deny));
        assert_eq!(permit_decision("maybe"), None);
    }

    #[test]
    fn settle_body_keeps_prompt_and_marks_chooser() {
        let mut env = NativePayload::text_only("feishu", "2");
        env.sender_name = "小明".into();
        env.sender_id = "ou_1".into();
        let marked = choice_marked(&env, true, "已允许一次: Write");
        assert!(marked.contains("小明"));
        assert!(marked.contains("已允许一次"));
        let body = settle_body("需要批准\n工具：Write", &marked);
        assert!(body.contains("需要批准"));
        assert!(body.contains("——"));
        assert!(body.contains("小明"));
    }

    #[test]
    fn stamp_tagged_choice_marks_group_mention() {
        let mut env = NativePayload::text_only("dingtalk", "p:abcd1234:1");
        env.meta.insert("is_group".into(), serde_json::json!(true));
        stamp_tagged_choice(&mut env);
        assert!(env.is_choice_click());
        assert!(env.is_mentioned());
        let mut plain = NativePayload::text_only("dingtalk", "hello");
        stamp_tagged_choice(&mut plain);
        assert!(!plain.is_choice_click());
    }

    #[test]
    fn pending_reply_is_consumed_only_by_the_owner() {
        let session = format!("im-interaction-{}", uuid::Uuid::new_v4().simple());
        let (reply, mut rx) = tokio::sync::oneshot::channel();
        enqueue_permit(&session, "owner", true, "Write", reply);
        let mut stranger = NativePayload::text_only("feishu", "2");
        stranger.session_id = session.clone();
        stranger.sender_id = "stranger".into();
        assert!(matches!(answer(&stranger), Answer::Rejected(_)));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        let mut owner = NativePayload::text_only("feishu", "2");
        owner.session_id = session.clone();
        owner.sender_id = "owner".into();
        assert!(matches!(answer(&owner), Answer::Accepted(_)));
        assert_eq!(rx.try_recv().unwrap(), PermitDecision::Always);
        assert_eq!(remembered_tools(&session), vec!["Write"]);
        assert!(matches!(answer(&owner), Answer::None));
        clear_pending(&session);
        forget_session(&session);
    }

    #[test]
    fn slash_approvals_unblocks_pending_permit() {
        let session = format!("im-slash-{}", uuid::Uuid::new_v4().simple());
        forget_session(&session);
        let (reply, mut rx) = tokio::sync::oneshot::channel();
        enqueue_permit(&session, "owner", true, "Shell", reply);
        let mut env = NativePayload::text_only("feishu", "/approvals yolo");
        env.session_id = session.clone();
        env.sender_id = "owner".into();
        assert!(matches!(answer(&env), Answer::None));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let MidTurn::Reply(text) = mid_turn(&env) else {
            panic!("expected slash reply");
        };
        assert!(text.to_ascii_lowercase().contains("yolo"));
        assert_eq!(rx.try_recv().unwrap(), PermitDecision::Allow);
        assert_eq!(
            controls(&session, ApprovalMode::Ask).approvals,
            ApprovalMode::Yolo
        );
        forget_session(&session);
    }

    #[test]
    fn leftover_choice_click_is_dropped() {
        let mut env = NativePayload::text_only("feishu", "2");
        env.mark_choice_click();
        env.session_id = "no-pending".into();
        env.sender_id = "owner".into();
        let MidTurn::Reply(text) = mid_turn(&env) else {
            panic!("expired click should tell the user");
        };
        assert!(text.contains("过期") || text.to_ascii_lowercase().contains("expired"));
        env.meta.remove("choice_click");
        assert!(matches!(mid_turn(&env), MidTurn::Pass));
    }

    #[test]
    fn yolo_updates_live_hub_mode() {
        let session = format!("im-hub-{}", uuid::Uuid::new_v4().simple());
        forget_session(&session);
        let (hub, _rx) = PermitHub::pair(ApprovalMode::Ask);
        let hub = hub.with_session(&session);
        let _guard = HubGuard::bind(&session, hub.clone());
        assert_eq!(hub.mode(), ApprovalMode::Ask);
        set_approvals(&session, ApprovalMode::Yolo);
        assert_eq!(hub.mode(), ApprovalMode::Yolo);
        forget_session(&session);
    }

    fn forget_session(session: &str) {
        {
            let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
            state.controls.remove(session);
            state.always.remove(session);
            state.pending.remove(session);
            state.hubs.remove(session);
            state.pumps.remove(session);
        }
        if let Some(path) = controls_path(session) {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn persists_controls_across_memory_reset() {
        let session = format!("im-controls-{}", uuid::Uuid::new_v4().simple());
        forget_session(&session);
        set_plan(&session, true);
        set_approvals(&session, ApprovalMode::Ask);
        {
            let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
            state.controls.remove(&session);
            state.always.remove(&session);
        }
        let loaded = controls(&session, ApprovalMode::Ask);
        assert!(loaded.plan);
        assert!(loaded.clarify);
        assert_eq!(loaded.approvals, ApprovalMode::Ask);
        forget_session(&session);
        let fresh = controls(&session, ApprovalMode::Ask);
        assert!(!fresh.plan);
        assert_eq!(fresh.approvals, ApprovalMode::Ask);
        forget_session(&session);
    }

    #[test]
    fn fifo_queue_keeps_second_permit() {
        let session = format!("im-fifo-{}", uuid::Uuid::new_v4().simple());
        let (a_tx, mut a_rx) = tokio::sync::oneshot::channel();
        let (b_tx, mut b_rx) = tokio::sync::oneshot::channel();
        enqueue_permit(&session, "owner", true, "Write", a_tx);
        enqueue_permit(&session, "owner", true, "Shell", b_tx);
        let mut owner = NativePayload::text_only("feishu", "1");
        owner.session_id = session.clone();
        owner.sender_id = "owner".into();
        assert!(matches!(answer(&owner), Answer::Accepted(s) if s.contains("Write")));
        assert_eq!(a_rx.try_recv().unwrap(), PermitDecision::Allow);
        assert!(matches!(
            b_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(answer(&owner), Answer::Accepted(s) if s.contains("Shell")));
        assert_eq!(b_rx.try_recv().unwrap(), PermitDecision::Allow);
        forget_session(&session);
    }

    #[test]
    fn tagged_click_addresses_specific_prompt() {
        let session = format!("im-tag-{}", uuid::Uuid::new_v4().simple());
        let (a_tx, mut a_rx) = tokio::sync::oneshot::channel();
        let (b_tx, mut b_rx) = tokio::sync::oneshot::channel();
        enqueue_permit(&session, "owner", true, "Write", a_tx);
        let id_b = enqueue_permit(&session, "owner", true, "Shell", b_tx);
        let mut owner = NativePayload::text_only("feishu", &format!("p:{id_b}:3"));
        owner.session_id = session.clone();
        owner.sender_id = "owner".into();
        owner.mark_choice_click();
        assert!(matches!(answer(&owner), Answer::Accepted(s) if s.contains("Shell")));
        assert_eq!(b_rx.try_recv().unwrap(), PermitDecision::Deny);
        assert!(matches!(
            a_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        forget_session(&session);
    }

    #[test]
    fn expired_prompt_denies_and_click_says_stale() {
        let session = format!("im-ttl-{}", uuid::Uuid::new_v4().simple());
        let (reply, mut rx) = tokio::sync::oneshot::channel();
        let id = mint_prompt_id();
        enqueue_prompt(
            &session,
            Prompt {
                id: id.clone(),
                owner: "owner".into(),
                zh: true,
                born: Instant::now() - (PROMPT_TTL + Duration::from_secs(1)),
                card_id: None,
                prompt_text: String::new(),
                kind: PendingKind::Permit {
                    tool: "Write".into(),
                    reply,
                },
            },
        );
        let mut click = NativePayload::text_only("feishu", &format!("p:{id}:1"));
        click.session_id = session.clone();
        click.sender_id = "owner".into();
        click.mark_choice_click();
        let Answer::Rejected(text) = answer(&click) else {
            panic!("expected expired");
        };
        assert!(text.contains("过期"));
        assert_eq!(rx.try_recv().unwrap(), PermitDecision::Deny);
        forget_session(&session);
    }

    #[test]
    fn group_member_cannot_flip_approvals() {
        let session = format!("im-owner-{}", uuid::Uuid::new_v4().simple());
        forget_session(&session);
        claim_owner(&session, "ou_owner");
        let stranger = group_env("feishu", "/approvals yolo", "ou_other", &session);
        let cmd = crate::slash::parse_slash("/approvals yolo").unwrap();
        assert!(deny_foreign_control(&stranger, &cmd).is_some());
        let MidTurn::Reply(text) = mid_turn(&stranger) else {
            panic!("expected denial");
        };
        assert!(text.contains("发起者"));
        assert_eq!(
            controls(&session, ApprovalMode::Ask).approvals,
            ApprovalMode::Ask
        );
        forget_session(&session);
    }

    #[test]
    fn persists_owner_and_model() {
        let session = format!("im-owner-disk-{}", uuid::Uuid::new_v4().simple());
        forget_session(&session);
        claim_owner(&session, "ou_a");
        set_model(&session, "grok-4.6");
        {
            let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
            state.controls.remove(&session);
        }
        let loaded = controls(&session, ApprovalMode::Ask);
        assert_eq!(loaded.owner, "ou_a");
        assert_eq!(loaded.model, "grok-4.6");
        forget_session(&session);
    }
}
