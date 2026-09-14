//! Slash, busy/queue/steer, and channel inbound dispatch.

use serde_json::{json, Value};

use super::helpers::{make_start, parse_params, slash_policy, SlashParams, TurnStartParams};
use super::types::{Dispatch, RpcError};
use super::{EventStore, SidecarSession};
use crate::channel::{BusyDecision, SteerSlot};
use crate::config::Config;
use crate::media::MediaPart;
use crate::permit::{ApprovalMode, PlanAction, PLAN_IMPLEMENT};
use crate::policy::{Effort, XHIGH_WARN};
use crate::session::{new_session_id, PolicyReason, SessionEvent, SessionMode, SlashCmd};
use crate::slash::{
    approvals_text, clarify_text, config_text, context_text, diff_text, help_text, history_text,
    imagine_text, low_precision_text, mcp_text, parse_slash_with_periphery, plan_text, setup_text,
    skills_text, status_text, tools_text, unsupported_text, usage_view, version_text, SlashView,
};

impl SidecarSession {
    pub(crate) fn slash(&mut self, params: Option<&Value>) -> Dispatch {
        if !self.opened {
            return Dispatch::Error(RpcError::invalid_request("session not open"));
        }
        let p: SlashParams = match parse_params(params) {
            Ok(p) => p,
            Err(e) => return Dispatch::Error(e),
        };
        let skills = self.skill_catalog();
        let mcp = self.mcp_registry();
        let Some(cmd) = parse_slash_with_periphery(&p.text, &skills, Some(&mcp)) else {
            return Dispatch::Error(RpcError::invalid_params(format!(
                "unknown slash command: {}",
                p.text.trim()
            )));
        };
        self.dispatch_slash(cmd)
    }

    pub(crate) fn dispatch_slash(&mut self, cmd: SlashCmd) -> Dispatch {
        match cmd {
            SlashCmd::Help
            | SlashCmd::Status
            | SlashCmd::Context { .. }
            | SlashCmd::History
            | SlashCmd::Usage
            | SlashCmd::Tools
            | SlashCmd::Skills
            | SlashCmd::Mcp
            | SlashCmd::Version
            | SlashCmd::Config
            | SlashCmd::Diff { .. }
            | SlashCmd::Sessions { .. }
            | SlashCmd::Unsupported { .. }
            | SlashCmd::Setup
            | SlashCmd::Reload => self.slash_inspect(cmd),
            SlashCmd::Mode(_)
            | SlashCmd::Title { .. }
            | SlashCmd::New { .. }
            | SlashCmd::Clear
            | SlashCmd::Resume { .. }
            | SlashCmd::Fork { .. }
            | SlashCmd::Compress { .. }
            | SlashCmd::Undo
            | SlashCmd::Retry
            | SlashCmd::Model { .. } => self.slash_session(cmd),
            SlashCmd::Stop
            | SlashCmd::Queue { .. }
            | SlashCmd::Steer { .. }
            | SlashCmd::Background { .. }
            | SlashCmd::InvokeSkill { .. }
            | SlashCmd::InvokeMcp { .. }
            | SlashCmd::Plan { .. } => self.slash_turn(cmd),
            SlashCmd::Off
            | SlashCmd::Think(_)
            | SlashCmd::Busy { .. }
            | SlashCmd::Approvals { .. }
            | SlashCmd::Clarify { .. }
            | SlashCmd::Imagine { .. }
            | SlashCmd::LowPrecision { .. }
            | SlashCmd::Cron { .. } => self.slash_policy(cmd),
        }
    }

    fn slash_inspect(&mut self, cmd: SlashCmd) -> Dispatch {
        match cmd {
            SlashCmd::Help => self.reply_text(help_text()),
            SlashCmd::Status => {
                let view = self.view();
                self.reply_text(status_text(&view))
            }
            SlashCmd::Context { .. } => {
                let view = self.view();
                self.reply_text(context_text(&view))
            }
            SlashCmd::History => self.reply_text(history_text(self.events(), 8000)),
            SlashCmd::Usage => {
                let view = self.view();
                self.reply_text(usage_view(&view))
            }
            SlashCmd::Tools => self.reply_text(tools_text(&self.tools)),
            SlashCmd::Skills => {
                let cat = self.skill_catalog();
                self.reply_text(skills_text(&cat))
            }
            SlashCmd::Mcp => {
                let reg = self.mcp_registry();
                self.reply_text(mcp_text(&reg))
            }
            SlashCmd::Version => self.reply_text(version_text()),
            SlashCmd::Config => self.reply_text(config_text(
                &self.model,
                &self.workspace,
                self.mode,
                self.mailbox.busy,
            )),
            SlashCmd::Diff { args } => self.reply_text(diff_text(&self.workspace, &args)),
            SlashCmd::Sessions { search } => self.reply_sessions(search.as_deref()),
            SlashCmd::Unsupported { name } => self.reply_text(unsupported_text(&name)),
            SlashCmd::Setup => self.reply_text(setup_text()),
            SlashCmd::Reload => self.reply_text("config will apply on the next turn".into()),
            other => unreachable!("slash_inspect got {other:?}"),
        }
    }

    fn slash_session(&mut self, cmd: SlashCmd) -> Dispatch {
        match cmd {
            SlashCmd::Mode(mode) => self.fork_mode(mode),
            SlashCmd::Title { name } => self.set_title(&name),
            SlashCmd::New { title } => self.fresh_session(title.as_deref(), true),
            SlashCmd::Clear => self.fresh_session(None, false),
            SlashCmd::Resume { query } => self.resume_query(query.as_deref()),
            SlashCmd::Fork { directive } => self.fork_session(directive),
            SlashCmd::Compress { hint } => self.force_compact(hint.as_deref()),
            SlashCmd::Undo => self.undo_last(),
            SlashCmd::Retry => self.retry_last(),
            SlashCmd::Model { args } => self.switch_model(&args),
            other => unreachable!("slash_session got {other:?}"),
        }
    }

    fn turn_or_queue(&mut self, prompt: String) -> Dispatch {
        if self.turn_in_flight {
            self.enqueue_prompt(prompt, false)
        } else {
            self.turn_in_flight = true;
            Dispatch::turn(prompt)
        }
    }

    fn slash_turn(&mut self, cmd: SlashCmd) -> Dispatch {
        match cmd {
            SlashCmd::Stop => {
                let n = self.mailbox.clear_queue();
                let _ = self.mailbox.take_redirect();
                Dispatch::AbortClear { cleared: n }
            }
            SlashCmd::Queue { text } => self.enqueue_prompt(text, false),
            SlashCmd::Steer { text } => self.enqueue_prompt(text, true),
            SlashCmd::Background { prompt } => match prompt.filter(|p| !p.trim().is_empty()) {
                Some(text) => self.enqueue_prompt(text, false),
                None => self.reply_text(
                    "Send `/background <task>` to queue a job. In IM, `/background` detaches the live turn so the next message starts a new session.".into(),
                ),
            },
            SlashCmd::InvokeSkill { name, args } => {
                self.turn_or_queue(crate::sticky::skill_turn_prompt(&name, &args))
            }
            SlashCmd::InvokeMcp { name, args } => {
                self.turn_or_queue(crate::sticky::mcp_turn_prompt(&name, &args))
            }
            SlashCmd::Plan { action, prompt } => {
                let prompt = prompt.filter(|p| !p.trim().is_empty());
                if matches!(action, PlanAction::On) {
                    if let Some(prompt) = prompt {
                        let _ = self.set_plan(PlanAction::On);
                        return self.turn_or_queue(prompt);
                    }
                }
                self.set_plan(action)
            }
            other => unreachable!("slash_turn got {other:?}"),
        }
    }

    fn slash_policy(&mut self, cmd: SlashCmd) -> Dispatch {
        match cmd {
            SlashCmd::Off | SlashCmd::Think(_) => {
                self.policy = slash_policy(&cmd, &self.caps);
                self.effort_locked = true;
                if matches!(self.policy.effort, Some(Effort::Xhigh)) {
                    eprintln!("{XHIGH_WARN}");
                }
                let event = SessionEvent::policy(self.policy.clone(), PolicyReason::Slash);
                self.record(event.clone());
                Dispatch::Result {
                    result: json!({"ok": true, "text": format!("thinking {}", if self.policy.enabled { "on" } else { "off" })}),
                    events: vec![event],
                }
            }
            SlashCmd::Busy { policy } => match policy {
                None => self.reply_text(format!("busy={}", self.mailbox.busy.as_str())),
                Some(p) => {
                    self.mailbox.busy = p;
                    self.reply_text(format!("busy={}", p.as_str()))
                }
            },
            SlashCmd::Approvals { mode } => self.set_approvals(mode),
            SlashCmd::Clarify { on } => self.set_clarify(on),
            SlashCmd::Imagine { on, prompt } => self.set_imagine(on, prompt),
            SlashCmd::LowPrecision { on } => self.set_lossy(on),
            SlashCmd::Cron { args } => {
                self.reply_text(crate::cron::apply_slash(&self.workspace, &args))
            }
            other => unreachable!("slash_policy got {other:?}"),
        }
    }

    pub(crate) fn reply_text(&self, text: String) -> Dispatch {
        Dispatch::Result {
            result: json!({"ok": true, "text": text}),
            events: Vec::new(),
        }
    }

    pub(crate) fn view(&self) -> SlashView<'_> {
        SlashView {
            session_id: &self.session_id,
            workspace: &self.workspace,
            mode: self.mode,
            policy: &self.policy,
            events: self.events(),
            model: &self.model,
            busy: self.mailbox.busy,
            channel: &self.channel,
            title: &self.title,
            tools: &self.tools,
            skill_count: self.skill_catalog().skills.len(),
            mcp_count: self.mcp_registry().servers.len(),
            window: self.window,
            family: self.family,
            queued: self.mailbox.queued(),
            plan_mode: self.plan_mode,
            clarify_mode: self.clarify_mode,
            imagine_mode: self.imagine_mode,
            approvals: self.approvals,
            low_precision: self.low_precision,
        }
    }

    fn set_approvals(&mut self, mode: Option<ApprovalMode>) -> Dispatch {
        if let Some(mode) = mode {
            self.approvals = mode;
            if let Ok(path) = Config::default_path() {
                let _ = Config::mutate_disk(&path, |cfg| {
                    cfg.features.approvals = mode.as_str().into();
                });
            }
        }
        self.reply_text(approvals_text(self.approvals))
    }

    fn set_lossy(&mut self, on: Option<bool>) -> Dispatch {
        if let Some(on) = on {
            self.low_precision = on;
            if let Ok(path) = Config::default_path() {
                let _ = Config::mutate_disk(&path, |cfg| {
                    cfg.policy.low_precision = on;
                });
            }
        }
        self.reply_text(low_precision_text(self.low_precision))
    }

    fn set_plan(&mut self, action: PlanAction) -> Dispatch {
        match action {
            PlanAction::On => {
                self.plan_mode = true;
                self.sync_ask();
                self.reply_text(plan_text(true))
            }
            PlanAction::Off => {
                self.plan_mode = false;
                self.sync_ask();
                self.reply_text(plan_text(false))
            }
            PlanAction::Go => {
                if !self.plan_mode {
                    return self.reply_text(
                        "not in plan mode. `/plan` first, then `/plan go` after you like the plan."
                            .into(),
                    );
                }
                self.plan_mode = false;
                self.sync_ask();
                Dispatch::turn(PLAN_IMPLEMENT)
            }
        }
    }

    fn set_clarify(&mut self, on: Option<bool>) -> Dispatch {
        if let Some(on) = on {
            self.clarify_mode = on;
            self.sync_ask();
        }
        self.reply_text(clarify_text(self.clarify_mode, self.plan_mode))
    }

    fn set_imagine(&mut self, on: Option<bool>, prompt: Option<String>) -> Dispatch {
        if let Some(on) = on {
            self.imagine_mode = on;
        }
        let prompt = prompt.filter(|p| !p.trim().is_empty());
        if let Some(prompt) = prompt {
            self.imagine_mode = true;
            return self.turn_or_queue(prompt);
        }
        self.reply_text(imagine_text(self.imagine_mode))
    }

    pub(crate) fn fork_mode(&mut self, mode: SessionMode) -> Dispatch {
        if self.turn_in_flight {
            return Dispatch::Error(RpcError::internal("turn in progress"));
        }
        let from_id = self.session_id.clone();
        let new_id = new_session_id();
        let policy = mode.default_policy_on(&self.caps.think_budget());
        let start = make_start(
            &new_id,
            &self.workspace,
            mode,
            policy.clone(),
            &self.channel,
        );
        let fork = SessionEvent::fork(&from_id);

        match &mut self.store {
            EventStore::Log(log) => {
                let Some(old) = log.start().cloned() else {
                    return Dispatch::Error(RpcError::internal("missing session/start"));
                };
                let new_start = old.for_fork(
                    new_id.clone(),
                    mode,
                    start.system.clone(),
                    start.tools_hash.clone(),
                );
                match log.fork(new_start.clone()) {
                    Ok(forked) => {
                        self.store = EventStore::Log(forked);
                    }
                    Err(e) => {
                        return Dispatch::Error(RpcError::internal(e.to_string()));
                    }
                }
            }
            EventStore::Memory(events) => {
                if events.is_empty() || !matches!(events[0], SessionEvent::Start(_)) {
                    return Dispatch::Error(RpcError::internal("missing session/start"));
                }
                events[0] = SessionEvent::Start(start.clone());
                events.push(fork.clone());
            }
        }

        self.session_id = new_id.clone();
        self.mode = mode;
        self.policy = policy;
        self.effort_locked = matches!(mode, SessionMode::Think | SessionMode::Chat);
        self.refresh_surface();
        self.remember_open_session();
        Dispatch::Result {
            result: json!({"ok": true, "session": new_id, "mode": mode.as_str()}),
            events: vec![SessionEvent::Start(start), fork],
        }
    }

    pub(crate) fn turn_start(&mut self, params: Option<&Value>) -> Dispatch {
        if !self.opened {
            return Dispatch::Error(RpcError::invalid_request("session not open"));
        }
        let p: TurnStartParams = match parse_params(params) {
            Ok(p) => p,
            Err(e) => return Dispatch::Error(e),
        };
        if let Some(ed) = p.editor.clone() {
            self.editor = ed.into_files();
        }
        let prompt = p.prompt();
        let parts = p.parts();
        if prompt.trim().is_empty() && parts.is_empty() {
            return Dispatch::Error(RpcError::invalid_params("prompt is required"));
        }
        self.accept_prompt_parts(
            if prompt.is_empty() {
                " ".into()
            } else {
                prompt
            },
            parts,
        )
    }

    pub(crate) fn turn_abort(&mut self) -> Dispatch {
        Dispatch::Abort
    }

    pub(crate) fn turn_queue(&mut self, params: Option<&Value>) -> Dispatch {
        let p: TurnStartParams = match parse_params(params) {
            Ok(p) => p,
            Err(e) => return Dispatch::Error(e),
        };
        if let Some(ed) = p.editor.clone() {
            self.editor = ed.into_files();
        }
        self.enqueue_parts(p.prompt(), p.parts(), false)
    }

    pub(crate) fn turn_steer(&mut self, params: Option<&Value>) -> Dispatch {
        let p: TurnStartParams = match parse_params(params) {
            Ok(p) => p,
            Err(e) => return Dispatch::Error(e),
        };
        if let Some(ed) = p.editor.clone() {
            self.editor = ed.into_files();
        }
        self.enqueue_parts(p.prompt(), p.parts(), true)
    }

    pub(crate) fn channel_inbound(&mut self, params: Option<&Value>) -> Dispatch {
        if !self.opened {
            return Dispatch::Error(RpcError::invalid_request("session not open"));
        }
        let mut env: crate::channel::NativePayload = match parse_params(params) {
            Ok(p) => p,
            Err(e) => return Dispatch::Error(e),
        };
        if !env.channel.is_empty() {
            self.channel = env.channel.clone();
        }
        if env.session_id.is_empty() {
            if let Ok(mut router) = crate::channel::SessionRouter::in_home() {
                if let Ok(id) = router.resolve(&env) {
                    env.session_id = id;
                }
            }
        }
        if !env.session_id.is_empty() && env.session_id != self.session_id {
            self.session_id = env.session_id.clone();
            let start = make_start(
                &self.session_id,
                &self.workspace,
                self.mode,
                self.policy.clone(),
                &self.channel,
            );
            let _ = self.bind_store(start);
            self.refresh_surface();
            self.refresh_title();
        }
        let prompt = env.query_text();
        let parts = env.media_parts();
        if prompt.trim().is_empty() && parts.is_empty() {
            return Dispatch::Error(RpcError::invalid_params("text/content_parts is required"));
        }
        if parts.is_empty() {
            if let Some(cmd) = parse_slash_with_periphery(
                &prompt,
                &self.skill_catalog(),
                Some(&self.mcp_registry()),
            ) {
                return self.dispatch_slash(cmd);
            }
        }
        self.accept_prompt_parts(
            if prompt.trim().is_empty() {
                " ".into()
            } else {
                prompt
            },
            parts,
        )
    }

    pub(crate) fn accept_prompt_parts(
        &mut self,
        prompt: String,
        parts: Vec<MediaPart>,
    ) -> Dispatch {
        if self.turn_in_flight {
            return match self.mailbox.offer_parts(prompt, parts) {
                BusyDecision::AbortThenRedirect => Dispatch::Abort,
                BusyDecision::Queued => Dispatch::Result {
                    result: json!({"ok": true, "queued": true, "n": self.mailbox.queued()}),
                    events: Vec::new(),
                },
                BusyDecision::Steered => Dispatch::Result {
                    result: json!({"ok": true, "steered": true}),
                    events: Vec::new(),
                },
            };
        }
        self.turn_in_flight = true;
        Dispatch::turn_parts(prompt, parts)
    }

    pub(crate) fn enqueue_prompt(&mut self, text: String, steer: bool) -> Dispatch {
        self.enqueue_parts(text, Vec::new(), steer)
    }

    pub(crate) fn enqueue_parts(
        &mut self,
        text: String,
        parts: Vec<MediaPart>,
        steer: bool,
    ) -> Dispatch {
        if text.trim().is_empty() && parts.is_empty() {
            return Dispatch::Error(RpcError::invalid_params("text/content_parts is required"));
        }
        if steer {
            if self.turn_in_flight {
                if parts.is_empty() {
                    self.mailbox.push_steer(text);
                } else {
                    let _ = self.mailbox.offer_parts(text, parts);
                }
                return Dispatch::Result {
                    result: json!({"ok": true, "steered": true}),
                    events: Vec::new(),
                };
            }
            self.mailbox.push_inbound(text, parts);
            return self.take_follow_up().unwrap_or_else(|| {
                self.reply_text("steered text queued until the next turn".into())
            });
        }
        if self.turn_in_flight {
            self.mailbox.push_inbound(text, parts);
            return Dispatch::Result {
                result: json!({"ok": true, "queued": true, "n": self.mailbox.queued()}),
                events: Vec::new(),
            };
        }
        self.turn_in_flight = true;
        Dispatch::turn_parts(text, parts)
    }

    pub(crate) fn take_follow_up(&mut self) -> Option<Dispatch> {
        let inbound = self
            .mailbox
            .take_redirect()
            .or_else(|| self.mailbox.pop_queue())?;
        self.turn_in_flight = true;
        Some(Dispatch::turn_parts(inbound.prompt, inbound.parts))
    }

    pub fn pop_follow_up(&mut self) -> Option<(String, Vec<MediaPart>)> {
        self.mailbox
            .take_redirect()
            .or_else(|| self.mailbox.pop_queue())
            .map(|i| (i.prompt, i.parts))
    }

    pub fn has_redirect(&self) -> bool {
        self.mailbox.has_redirect()
    }

    pub fn steer_slot(&self) -> SteerSlot {
        self.mailbox.steer_slot()
    }
}

#[cfg(test)]
mod slash_dispatch_tests {
    use super::super::rpc::parse_request_line;
    use super::super::types::{Dispatch, SidecarOpts};
    use super::super::SidecarSession;
    use crate::session::SlashCmd;

    fn open_mem() -> SidecarSession {
        let mut session = SidecarSession::new(SidecarOpts::default());
        let open = parse_request_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"session.open","params":{"session":"s-slash","workspace":"/tmp/ws","mode":"agent"}}"#,
        )
        .unwrap();
        match session.handle(&open) {
            Dispatch::Result { .. } => {}
            other => panic!("{other:?}"),
        }
        session
    }

    fn ok_text(d: Dispatch) -> String {
        match d {
            Dispatch::Result { result, .. } => result["text"].as_str().unwrap_or("").to_string(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn inspect_help_and_status() {
        let mut s = open_mem();
        let help = ok_text(s.dispatch_slash(SlashCmd::Help));
        assert!(
            help.contains("/help") || help.contains("this list"),
            "{help}"
        );
        assert!(!ok_text(s.dispatch_slash(SlashCmd::Status)).is_empty());
        assert!(ok_text(s.dispatch_slash(SlashCmd::Reload)).contains("config will apply"));
    }

    #[test]
    fn stop_is_abort_clear() {
        let mut s = open_mem();
        match s.dispatch_slash(SlashCmd::Stop) {
            Dispatch::AbortClear { cleared } => assert_eq!(cleared, 0),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn think_off_records_policy() {
        let mut s = open_mem();
        match s.dispatch_slash(SlashCmd::Off) {
            Dispatch::Result { result, events } => {
                assert_eq!(result["ok"], true);
                assert!(result["text"].as_str().unwrap().contains("off"));
                assert!(!events.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn invoke_skill_starts_turn_then_queues() {
        let mut s = open_mem();
        match s.dispatch_slash(SlashCmd::InvokeSkill {
            name: "hyper-self".into(),
            args: String::new(),
        }) {
            Dispatch::TurnStart { prompt, .. } => {
                assert!(prompt.contains("hyper-self"), "{prompt}");
            }
            other => panic!("{other:?}"),
        }
        s.turn_in_flight = true;
        match s.dispatch_slash(SlashCmd::InvokeSkill {
            name: "hyper-self".into(),
            args: String::new(),
        }) {
            Dispatch::Result { result, .. } => assert_eq!(result["queued"], true),
            other => panic!("{other:?}"),
        }
    }
}
