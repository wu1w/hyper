//! Blocking IM controls for AskQuestion and mutating-tool approval.
//!
//! The agent-side hubs remain transport-neutral. This module turns their
//! requests into chat messages and lets the per-session worker consume the
//! next human reply before normal busy/steer routing sees it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

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

enum Pending {
    Clarify {
        owner: String,
        zh: bool,
        ask: ClarifyAsk,
        reply: tokio::sync::oneshot::Sender<ClarifyDecision>,
    },
    Permit {
        owner: String,
        zh: bool,
        tool: String,
        reply: tokio::sync::oneshot::Sender<PermitDecision>,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct SessionControls {
    pub plan: bool,
    pub clarify: bool,
    pub approvals: ApprovalMode,
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
}

struct State {
    pending: HashMap<String, Pending>,
    controls: HashMap<String, SessionControls>,
    always: HashMap<String, HashSet<String>>,
    hubs: HashMap<String, PermitHub>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(State {
            pending: HashMap::new(),
            controls: HashMap::new(),
            always: HashMap::new(),
            hubs: HashMap::new(),
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
        },
        disk.always.into_iter().collect(),
    ))
}

fn persist_session(session: &str) {
    let (controls, always) = {
        let state = state().lock().unwrap_or_else(|e| e.into_inner());
        let Some(controls) = state.controls.get(session).copied() else {
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
        },
    );
}

pub(crate) fn controls(session: &str, default_approvals: ApprovalMode) -> SessionControls {
    let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
    ensure_loaded(&mut state, session, default_approvals);
    *state.controls.get(session).expect("ensure_loaded")
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
    let Some(Pending::Permit { tool, .. }) = state.pending.get(session) else {
        return;
    };
    let allow = match mode {
        ApprovalMode::Yolo => true,
        ApprovalMode::Auto => !PermitHub::needs_prompt(ApprovalMode::Auto, tool),
        ApprovalMode::Ask => false,
    };
    if !allow {
        return;
    }
    if let Some(Pending::Permit { reply, .. }) = state.pending.remove(session) {
        let _ = reply.send(PermitDecision::Allow);
    }
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
        SlashCmd::History => "/history",
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
/// started the run can answer in a shared chat.
pub(crate) fn answer(env: &NativePayload) -> Answer {
    let text = env.query_text();
    let trimmed = text.trim();
    if crate::slash::parse_slash(trimmed).is_some() {
        return Answer::None;
    }
    let mut state = state().lock().unwrap_or_else(|e| e.into_inner());
    let Some(pending) = state.pending.get(&env.session_id) else {
        return Answer::None;
    };
    let (owner, zh) = match pending {
        Pending::Clarify { owner, zh, .. } | Pending::Permit { owner, zh, .. } => (owner, *zh),
    };
    if !owner.is_empty() && owner != &env.sender_id {
        return Answer::Rejected(if zh {
            "这项操作需要由任务发起者回复。".into()
        } else {
            "Only the user who started this task can answer.".into()
        });
    }

    match pending {
        Pending::Clarify { ask, .. } => {
            let decision = clarify_decision(ask, trimmed);
            let Some(decision) = decision else {
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
            let Pending::Clarify { reply, .. } = state
                .pending
                .remove(&env.session_id)
                .expect("pending kind checked")
            else {
                unreachable!()
            };
            let _ = reply.send(decision);
            Answer::Accepted(if zh {
                format!("已选择：{label}")
            } else {
                format!("Selected: {label}")
            })
        }
        Pending::Permit { tool, .. } => {
            let Some(decision) = permit_decision(trimmed) else {
                return Answer::Rejected(if zh {
                    "请回复 `1`（允许一次）、`2`（本会话始终允许）或 `3`（拒绝）。".into()
                } else {
                    "Reply `1` (allow once), `2` (always this session), or `3` (deny).".into()
                });
            };
            let tool = tool.clone();
            let Pending::Permit { reply, .. } = state
                .pending
                .remove(&env.session_id)
                .expect("pending kind checked")
            else {
                unreachable!()
            };
            let _ = reply.send(decision);
            if decision == PermitDecision::Always {
                state
                    .always
                    .entry(env.session_id.clone())
                    .or_default()
                    .insert(tool.clone());
            }
            drop(state);
            if decision == PermitDecision::Always {
                persist_session(&env.session_id);
            }
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
            Answer::Accepted(format!("{label}: {tool}"))
        }
    }
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

fn put_pending(session: &str, pending: Pending) {
    state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pending
        .insert(session.to_string(), pending);
}

pub(crate) fn clear_pending(session: &str) {
    state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pending
        .remove(session);
}

pub(crate) fn spawn_pump(
    ep: Option<ChannelEndpoint>,
    env: NativePayload,
    mut permits: mpsc::UnboundedReceiver<PermitRequest>,
    mut clarifies: mpsc::UnboundedReceiver<ClarifyRequest>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        clear_pending(&env.session_id);
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
                        let buttons = permit_buttons(zh);
                        put_pending(&env.session_id, Pending::Permit {
                            owner: env.sender_id.clone(),
                            zh,
                            tool: request.ask.tool.clone(),
                            reply: request.reply,
                        });
                        if let Err(err) = super::outbound::deliver_choices(
                            ep.as_ref(),
                            &env,
                            &text,
                            &buttons,
                            Instant::now(),
                        ).await {
                            eprintln!("hyper channel approval prompt: {err}");
                            clear_pending(&env.session_id);
                        }
                    }
                    None => permit_open = false,
                },
                request = clarifies.recv(), if clarify_open => match request {
                    Some(request) => {
                        let zh = matches!(ImLocale::detect(&env.query_text()), ImLocale::Zh);
                        let text = clarify_prompt(&request.ask, zh);
                        let buttons = clarify_buttons(&request.ask);
                        put_pending(&env.session_id, Pending::Clarify {
                            owner: env.sender_id.clone(),
                            zh,
                            ask: request.ask,
                            reply: request.reply,
                        });
                        if let Err(err) = super::outbound::deliver_choices(
                            ep.as_ref(),
                            &env,
                            &text,
                            &buttons,
                            Instant::now(),
                        ).await {
                            eprintln!("hyper channel question prompt: {err}");
                            clear_pending(&env.session_id);
                        }
                    }
                    None => clarify_open = false,
                },
            }
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
    fn pending_reply_is_consumed_only_by_the_owner() {
        let session = format!("im-interaction-{}", uuid::Uuid::new_v4().simple());
        let (reply, mut rx) = tokio::sync::oneshot::channel();
        put_pending(
            &session,
            Pending::Permit {
                owner: "owner".into(),
                zh: true,
                tool: "Write".into(),
                reply,
            },
        );
        let mut stranger = NativePayload::text_only("feishu", "1");
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
        put_pending(
            &session,
            Pending::Permit {
                owner: "owner".into(),
                zh: true,
                tool: "Shell".into(),
                reply,
            },
        );
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
        assert!(matches!(mid_turn(&env), MidTurn::Drop));
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
}
