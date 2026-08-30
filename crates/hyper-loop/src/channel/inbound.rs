//! Run one in-process endpoint (used by `hyper --channels` and `hyper web`).

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;

use crate::agent::{Agent, RunOpts, ToolSet, TransportCompleter};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::session::SessionMode;
use crate::sidecar::EventSink;
use crate::tool_calls::CancelFlag;

use super::envelope::{ContentPart, NativePayload};
use super::manager::ChannelManager;
use super::router::SessionRouter;
use super::ChannelEndpoint;

const BACKOFF_MIN_SECS: u64 = 5;
const BACKOFF_MAX_SECS: u64 = 120;

/// Console/TUI keep `features.approvals` (ask). IM also defaults to ask;
/// `/approvals yolo` or a session controls file is required to skip prompts.
fn im_default_approvals() -> crate::permit::ApprovalMode {
    crate::permit::ApprovalMode::Ask
}

/// Watcher / console status while [`keep_client_watched`] is between attempts.
#[derive(Clone, Debug)]
pub enum ClientWatch {
    Running,
    Retry { detail: String, wait_secs: u64 },
    Fatal { detail: String },
}

pub fn supervise_backoff_secs(fail: u32) -> u64 {
    let exp = fail.saturating_sub(1).min(8);
    BACKOFF_MIN_SECS
        .saturating_mul(1u64 << exp)
        .min(BACKOFF_MAX_SECS)
}

/// A client that stayed up this long was healthy; the next drop is a new
/// incident, not a crash loop. Keeps overnight IM from sitting on a 120s
/// backoff after hours of successful polling.
pub const SUPERVISE_HEALTHY: Duration = Duration::from_secs(30);

pub fn supervise_fail_after(fail: u32, ran: Duration, ok: bool) -> u32 {
    if ran >= SUPERVISE_HEALTHY {
        if ok {
            0
        } else {
            1
        }
    } else {
        fail.saturating_add(1)
    }
}

pub fn is_fatal_serve_error(err: &str) -> bool {
    err.contains("no in-process client")
}

pub async fn catch_client<F>(fut: F) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(r) => r,
        Err(_) => Err(Error::msg("channel client panicked")),
    }
}

/// Restart a live client after unexpected return / error / panic.
/// Fingerprint changes abort the task; this loop then stops.
pub async fn keep_client_watched<F, Fut, W, WFut>(kind: &str, id: &str, mut once: F, mut watch: W)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<()>>,
    W: FnMut(ClientWatch) -> WFut,
    WFut: Future<Output = ()>,
{
    let mut fail = 0u32;
    loop {
        match super::poll_lock::acquire(kind, id) {
            Ok(_lock) => {
                watch(ClientWatch::Running).await;
                let started = std::time::Instant::now();
                match catch_client(once()).await {
                    Ok(()) => {
                        fail = supervise_fail_after(fail, started.elapsed(), true);
                        if fail == 0 {
                            watch(ClientWatch::Running).await;
                            continue;
                        }
                        let wait = supervise_backoff_secs(fail);
                        let detail = "client exited".to_string();
                        eprintln!("hyper {kind} ({id}): {detail}; retry in {wait}s");
                        watch(ClientWatch::Retry {
                            detail,
                            wait_secs: wait,
                        })
                        .await;
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                    }
                    Err(e) => {
                        let s = e.to_string();
                        if is_fatal_serve_error(&s) {
                            eprintln!("hyper {kind}: {s}");
                            watch(ClientWatch::Fatal { detail: s }).await;
                            return;
                        }
                        fail = supervise_fail_after(fail, started.elapsed(), false);
                        let wait = supervise_backoff_secs(fail);
                        eprintln!("hyper {kind} ({id}): {s}; retry in {wait}s");
                        watch(ClientWatch::Retry {
                            detail: s,
                            wait_secs: wait,
                        })
                        .await;
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                    }
                }
            }
            Err(e) => {
                fail = fail.saturating_add(1);
                let wait = supervise_backoff_secs(fail);
                let s = e.to_string();
                eprintln!("hyper {kind} ({id}): {s}; retry in {wait}s");
                watch(ClientWatch::Retry {
                    detail: s,
                    wait_secs: wait,
                })
                .await;
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
        }
    }
}

/// Start the live client for one enabled endpoint (returns if the adapter exits).
pub async fn serve_endpoint(cfg: Config, workspace: PathBuf, ep: ChannelEndpoint) -> Result<()> {
    let kind = ep.kind.to_ascii_lowercase();
    let router = SessionRouter::in_home()?;
    let cfg = Arc::new(cfg);
    let cfg_h = cfg.clone();
    let workspace = workspace.clone();
    let mgr = ChannelManager::start(
        cfg.channels.clone(),
        router,
        move |env: NativePayload, cancel, steer| {
            let cfg = cfg_h.clone();
            let workspace = workspace.clone();
            async move { agent_inbound(&cfg, workspace, env, cancel, steer).await }
        },
    );
    let replay_ep = ep.clone();
    tokio::spawn(async move {
        if let Err(err) = super::outbound::replay_pending(Some(&replay_ep)).await {
            eprintln!("hyper channel outbox replay: {err}");
        }
    });
    match kind.as_str() {
        "telegram" => super::telegram::run_long_poll(ep, mgr).await,
        "webhook" | "http" | "console" => super::webhook::serve(ep, mgr).await,
        "qq" => super::qq::run_gateway(ep, mgr).await,
        "wechat" => super::wechat::run_poll(ep, mgr).await,
        "wecom" => super::wecom::run_gateway(ep, mgr).await,
        "dingtalk" => super::dingtalk::run_gateway(ep, mgr).await,
        "feishu" => super::feishu::run_ws(ep, mgr).await,
        other => Err(Error::msg(format!(
            "hyper channel {other}: no in-process client (enable {})",
            super::IN_PROCESS_HELP
        ))),
    }
}

/// Back-compat for the QQ-only hub watcher.
pub async fn serve_qq(cfg: Config, workspace: PathBuf, ep: ChannelEndpoint) -> Result<()> {
    serve_endpoint(cfg, workspace, ep).await
}

async fn agent_inbound(
    cfg: &Config,
    workspace: PathBuf,
    env: NativePayload,
    cancel: CancelFlag,
    steer: super::SteerSlot,
) -> Result<Vec<ContentPart>> {
    let home = Config::home_dir().ok();
    let skills = crate::skills::SkillCatalog::load(
        home.as_deref().unwrap_or_else(|| std::path::Path::new("")),
        &workspace,
    );
    let mcp = crate::mcp::McpRegistry::load(home.as_deref(), &workspace, &cfg.mcp);
    let query = env.query_text();
    let mut message = env.to_chat_message();
    if let Some(cmd) = crate::slash::parse_slash_with_periphery(&query, &skills, Some(&mcp)) {
        use crate::slash::SlashCmd;
        let reply = match cmd {
            SlashCmd::Help => Some(crate::slash::help_text()),
            SlashCmd::Version => Some(crate::slash::version_text()),
            SlashCmd::Unsupported { name } => Some(crate::slash::unsupported_text(&name)),
            SlashCmd::Skills => Some(crate::slash::skills_text(&skills)),
            SlashCmd::Mcp => Some(crate::slash::mcp_text(&mcp)),
            SlashCmd::Status => Some(im_status_text(cfg, &env)),
            SlashCmd::Sessions { search } => {
                Some(im_sessions_text(home.as_deref(), search.as_deref())?)
            }
            SlashCmd::History => Some(im_history_text(home.as_deref(), &env.session_id)?),
            SlashCmd::New { title } => Some(match title {
                Some(title) => format!(
                    "已新建会话 `{}`（标题：{}）。下一条消息会进入新会话。",
                    env.session_id, title
                ),
                None => format!("已新建会话 `{}`。下一条消息会进入新会话。", env.session_id),
            }),
            SlashCmd::Resume { query } => Some(
                if let Some(err) = env
                    .meta
                    .get("resume_error")
                    .and_then(serde_json::Value::as_str)
                {
                    format!(
                        "未找到会话：{}（{err}）",
                        query.unwrap_or_else(|| "latest".into())
                    )
                } else {
                    format!("已切换到会话 `{}`。", env.session_id)
                },
            ),
            SlashCmd::Title { name } => {
                let Some(home) = home.as_deref() else {
                    return Ok(super::outbound::reply_text("无法定位会话目录。"));
                };
                crate::session::catalog::set_title(home.join("sessions"), &env.session_id, &name)?;
                Some(format!("会话标题已改为：{name}"))
            }
            SlashCmd::Stop => Some("当前没有正在运行的任务。".into()),
            SlashCmd::Busy { policy } => Some(format!(
                "当前忙时策略：{}。运行中发送 `/busy steer|queue|interrupt` 可切换。",
                policy
                    .unwrap_or_else(|| cfg.channels.busy_policy())
                    .as_str()
            )),
            SlashCmd::Cron { args } => Some(crate::cron::apply_slash(&workspace, &args)),
            SlashCmd::Queue { text } | SlashCmd::Steer { text } => {
                message.content = Some(text);
                None
            }
            SlashCmd::InvokeSkill { name, args } => {
                message.content = Some(crate::sticky::skill_turn_prompt(&name, &args));
                None
            }
            SlashCmd::InvokeMcp { name, args } => {
                message.content = Some(crate::sticky::mcp_turn_prompt(&name, &args));
                None
            }
            SlashCmd::Retry => {
                let Some(text) = last_real_user(home.as_deref(), &env.session_id)? else {
                    return Ok(super::outbound::reply_text("没有可重试的上一条用户消息。"));
                };
                message = crate::template::ChatMessage::user(text);
                None
            }
            SlashCmd::Approvals { mode } => {
                let default = im_default_approvals();
                if let Some(mode) = mode {
                    super::interaction::set_approvals(&env.session_id, mode);
                }
                let current = super::interaction::controls(&env.session_id, default).approvals;
                Some(crate::slash::approvals_text(current))
            }
            SlashCmd::Plan { action, prompt } => {
                use crate::permit::PlanAction;
                match action {
                    PlanAction::On => {
                        super::interaction::set_plan(&env.session_id, true);
                        if let Some(prompt) = prompt.filter(|p| !p.trim().is_empty()) {
                            message.content = Some(prompt);
                            None
                        } else {
                            Some(crate::slash::plan_text(true))
                        }
                    }
                    PlanAction::Off => {
                        super::interaction::set_plan(&env.session_id, false);
                        Some(crate::slash::plan_text(false))
                    }
                    PlanAction::Go => {
                        let default = im_default_approvals();
                        let controls = super::interaction::controls(&env.session_id, default);
                        if !controls.plan {
                            Some("当前不在 plan mode。先发送 `/plan`。".into())
                        } else {
                            super::interaction::set_plan(&env.session_id, false);
                            message.content = Some(
                                prompt.unwrap_or_else(|| crate::permit::PLAN_IMPLEMENT.into()),
                            );
                            None
                        }
                    }
                }
            }
            SlashCmd::Clarify { on } => {
                let default = im_default_approvals();
                if let Some(on) = on {
                    super::interaction::set_clarify(&env.session_id, on);
                }
                let controls = super::interaction::controls(&env.session_id, default);
                Some(crate::slash::clarify_text(controls.clarify, controls.plan))
            }
            other => Some(format!(
                "命令 `{}` 尚未开放到 IM 控制面，请在 Web/CLI 使用。",
                im_cmd_name(&other)
            )),
        };
        if let Some(reply) = reply {
            return Ok(super::outbound::reply_text(reply));
        }
    }
    let policy = SessionMode::Agent.default_policy_with(&cfg.policy);
    let ws = workspace.clone();
    let mut opts = RunOpts::from_config(cfg, workspace);
    opts.print = false;
    opts.persist_session = true;
    opts.session_id = if env.session_id.is_empty() {
        if env.channel.is_empty() {
            "channel".into()
        } else {
            env.channel.clone()
        }
    } else {
        env.session_id.clone()
    };
    opts.session_mode = SessionMode::Agent;
    opts.with_tools = true;
    opts.tool_set = ToolSet::Agent;
    opts.channel = if env.channel.trim().is_empty() {
        "im".into()
    } else {
        env.channel.clone()
    };
    let default_approvals = im_default_approvals();
    let controls = super::interaction::controls(&env.session_id, default_approvals);
    opts.plan_mode = controls.plan;
    opts.clarify_mode = controls.clarify;
    let (permit, permit_rx) = crate::permit::PermitHub::pair(controls.approvals);
    let (clarify, clarify_rx) = crate::clarify::ClarifyHub::pair();
    for tool in super::interaction::remembered_tools(&env.session_id) {
        permit.remember(&tool);
    }
    let permit = permit.with_session(env.session_id.clone());
    let _hub_guard = super::interaction::HubGuard::bind(&env.session_id, permit.clone());
    opts.permit = Some(permit);
    opts.clarify = Some(clarify.with_session(env.session_id.clone()));
    crate::agent::apply_unattended_policy(&mut opts, cfg);
    let (emit, pump) = spawn_im_pump(cfg, &env);
    let interaction_pump = super::interaction::spawn_pump(
        cfg.channels.endpoint_for_payload(&env).cloned(),
        env.clone(),
        permit_rx,
        clarify_rx,
    );
    let completer = TransportCompleter::connect(cfg, policy).await?;
    let mut agent = tokio::task::spawn_blocking(move || Agent::new(completer, opts))
        .await
        .map_err(|e| Error::msg(format!("agent setup: {e}")))??;
    agent.set_emit(emit);
    agent.set_cancel(cancel);
    agent.set_steer(steer.clone());
    let out = agent.run_message(message).await?;
    // Notes that missed the last tool hop: session_worker queues a follow-up.
    for note in out.pending_steer {
        crate::channel::push_steer(&steer, note);
    }
    let parts = crate::channel::xfer::reply_parts(&out.text, &ws, &out.channel_files);
    drop(agent);
    let _ = pump.await;
    interaction_pump.abort();
    let _ = interaction_pump.await;
    super::interaction::clear_pending(&env.session_id);
    Ok(parts)
}

fn im_status_text(cfg: &Config, env: &NativePayload) -> String {
    let default = im_default_approvals();
    let controls = super::interaction::controls(&env.session_id, default);
    format!(
        "会话：`{}`\n频道：{}\n忙时策略：{}\n运行模式：{}\n审批：{}\n模型：{}",
        env.session_id,
        if env.channel.is_empty() {
            "im"
        } else {
            &env.channel
        },
        cfg.channels.busy_policy().as_str(),
        if controls.plan {
            "plan"
        } else if controls.clarify {
            "ask"
        } else {
            "agent"
        },
        controls.approvals.as_str(),
        cfg.server.model
    )
}

fn im_sessions_text(home: Option<&std::path::Path>, search: Option<&str>) -> Result<String> {
    let Some(home) = home else {
        return Ok("无法定位会话目录。".into());
    };
    let mut sessions = crate::session::catalog::list(home.join("sessions"))?;
    if let Some(q) = search.map(str::trim).filter(|q| !q.is_empty()) {
        let q = q.to_ascii_lowercase();
        sessions.retain(|s| {
            s.id.starts_with(&q)
                || s.title.to_ascii_lowercase().contains(&q)
                || s.preview.to_ascii_lowercase().contains(&q)
        });
    }
    if sessions.is_empty() {
        return Ok("没有匹配的会话。".into());
    }
    let mut lines = vec!["最近会话：".to_string()];
    for s in sessions.into_iter().take(12) {
        let label = if s.title.is_empty() {
            s.preview
        } else {
            s.title
        };
        lines.push(format!("- `{}` · {} · {}", s.id, s.channel, label));
    }
    lines.push("使用 `/resume <id或标题>` 切换。".into());
    Ok(lines.join("\n"))
}

fn im_history_text(home: Option<&std::path::Path>, session_id: &str) -> Result<String> {
    let Some(home) = home else {
        return Ok("无法定位会话目录。".into());
    };
    let log = crate::session::SessionLog::open_in(home.join("sessions"), session_id)?;
    Ok(crate::slash::history_text(log.events(), 8_000))
}

fn last_real_user(home: Option<&std::path::Path>, session_id: &str) -> Result<Option<String>> {
    let Some(home) = home else {
        return Ok(None);
    };
    let log = crate::session::SessionLog::open_in(home.join("sessions"), session_id)?;
    Ok(log.events().iter().rev().find_map(|event| match event {
        crate::session::SessionEvent::User(user)
            if !crate::template::is_hidden_user_text(&user.text)
                && !user.text.trim_start().starts_with('/') =>
        {
            Some(user.text.clone())
        }
        _ => None,
    }))
}

fn im_cmd_name(cmd: &crate::slash::SlashCmd) -> &'static str {
    use crate::slash::SlashCmd;
    match cmd {
        SlashCmd::Think(_) | SlashCmd::Off => "/think",
        SlashCmd::Mode(_) => "/mode",
        SlashCmd::Context { .. } => "/context",
        SlashCmd::Clear => "/clear",
        SlashCmd::Compress { .. } => "/compact",
        SlashCmd::Undo => "/undo",
        SlashCmd::Model { .. } => "/model",
        SlashCmd::Setup => "/setup",
        SlashCmd::Approvals { .. } => "/approvals",
        SlashCmd::Plan { .. } => "/plan",
        SlashCmd::Fork { .. } => "/fork",
        SlashCmd::Clarify { .. } => "/clarify",
        SlashCmd::Imagine { .. } => "/imagine",
        SlashCmd::Tools => "/tools",
        SlashCmd::Usage => "/usage",
        SlashCmd::Diff { .. } => "/diff",
        SlashCmd::Reload => "/reload",
        SlashCmd::Config => "/config",
        SlashCmd::LowPrecision { .. } => "/lossy",
        _ => "/command",
    }
}

pub fn spawn_im_pump(
    cfg: &Config,
    env: &NativePayload,
) -> (EventSink, tokio::task::JoinHandle<()>) {
    let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let ep = cfg.channels.endpoint_for_payload(&env).cloned();
    let env_p = env.clone();
    let started = std::time::Instant::now();
    let pump = tokio::spawn(async move {
        pump_im_progress(ev_rx, ep, env_p, started).await;
    });
    (EventSink::new(ev_tx), pump)
}

async fn pump_im_progress(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::session::SessionEvent>,
    ep: Option<super::ChannelEndpoint>,
    env: NativePayload,
    started: std::time::Instant,
) {
    use super::progress::ProgressBuf;
    let mut buf = ProgressBuf::for_channel(started, &env.query_text(), &env.channel);
    let mut typing = tokio::time::interval(std::time::Duration::from_secs(4));
    typing.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let _ = typing.tick().await;
    if env.channel.eq_ignore_ascii_case("feishu") {
        super::feishu::send_typing(ep.as_ref(), &env).await;
    }
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else {
                    for line in buf.finish(std::time::Instant::now()) {
                        let parts = super::outbound::reply_text(line);
                        let _ = super::outbound::deliver_progress_since(
                            ep.as_ref(),
                            &env,
                            &parts,
                            started,
                        )
                        .await;
                    }
                    if env.channel.eq_ignore_ascii_case("feishu") {
                        super::feishu::stop_typing(ep.as_ref(), &env).await;
                    }
                    break;
                };
                for line in buf.ingest(&ev, std::time::Instant::now()) {
                    let parts = super::outbound::reply_text(line);
                    if let Err(e) = super::outbound::deliver_progress_since(
                        ep.as_ref(),
                        &env,
                        &parts,
                        started,
                    )
                    .await
                    {
                        eprintln!("hyper channel progress: {e}");
                    }
                }
            }
            _ = typing.tick() => {
                if env.channel.eq_ignore_ascii_case("telegram") {
                    super::telegram::send_typing(ep.as_ref(), &env).await;
                } else if env.channel.eq_ignore_ascii_case("qq") {
                    let _ = super::qq::send_typing(ep.as_ref(), &env).await;
                } else if env.channel.eq_ignore_ascii_case("wechat") {
                    super::wechat::send_typing(ep.as_ref(), &env).await;
                } else if env.channel.eq_ignore_ascii_case("feishu") {
                    super::feishu::send_typing(ep.as_ref(), &env).await;
                }
                for line in buf.tick(std::time::Instant::now()) {
                    let parts = super::outbound::reply_text(line);
                    let _ = super::outbound::deliver_progress_since(
                        ep.as_ref(),
                        &env,
                        &parts,
                        started,
                    )
                    .await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_caps() {
        assert_eq!(supervise_backoff_secs(1), 5);
        assert_eq!(supervise_backoff_secs(2), 10);
        assert_eq!(supervise_backoff_secs(3), 20);
        assert_eq!(supervise_backoff_secs(20), BACKOFF_MAX_SECS);
    }

    #[test]
    fn healthy_run_resets_crash_loop_backoff() {
        assert_eq!(supervise_fail_after(8, Duration::from_secs(1), true), 9);
        assert_eq!(supervise_fail_after(8, SUPERVISE_HEALTHY, true), 0);
        assert_eq!(supervise_fail_after(8, SUPERVISE_HEALTHY, false), 1);
        assert_eq!(
            supervise_backoff_secs(supervise_fail_after(8, SUPERVISE_HEALTHY, false)),
            5
        );
    }

    #[test]
    fn fatal_unknown_kind() {
        assert!(is_fatal_serve_error(
            "hyper channel discord: no in-process client"
        ));
        assert!(!is_fatal_serve_error("wechat HTTP 502"));
    }

    #[tokio::test]
    #[ignore = "live grok IM webhook soak"]
    async fn live_im_webhook_chinese_full_loop() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio::sync::mpsc;

        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("repo root");
        let py = workspace.join(".grok-hyper/overnight/im_live.py");
        let _ = std::fs::remove_file(&py);

        let (cfg, _) = Config::load_or_init().expect("config");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while tokio::time::Instant::now() < deadline {
                    match tokio::time::timeout_at(deadline, sock.read(&mut tmp)).await {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                        _ => break,
                    }
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&buf[..i]);
                        let want = headers
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                if k.eq_ignore_ascii_case("content-length") {
                                    v.trim().parse::<usize>().ok()
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);
                        if buf.len() >= i + 4 + want {
                            let body =
                                String::from_utf8_lossy(&buf[i + 4..i + 4 + want]).into_owned();
                            let _ = tx.send(body);
                            let _ = sock
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                .await;
                            break;
                        }
                    }
                }
            }
        });

        let dir =
            std::env::temp_dir().join(format!("hyper-im-live-{}", uuid::Uuid::new_v4().simple()));
        let router = SessionRouter::open(dir.join("routes.json")).unwrap();
        let cfg = std::sync::Arc::new(cfg);
        let cfg_h = cfg.clone();
        let ws = workspace.clone();
        let mgr = ChannelManager::start(
            crate::channel::ChannelsConfig::default(),
            router,
            move |env, cancel, steer| {
                let cfg_h = cfg_h.clone();
                let ws = ws.clone();
                async move { agent_inbound(&cfg_h, ws, env, cancel, steer).await }
            },
        );
        let sid = format!(
            "overnight-im-live-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
        let mut env = NativePayload::text_only(
            "webhook",
            "工作区就是当前仓库。Write `.grok-hyper/overnight/im_live.py`，只打印一行 IM_LIVE_OK。Shell 跑 python3 .grok-hyper/overnight/im_live.py。不要改 crates/。中文一句结束。",
        );
        env.session_id = sid.clone();
        env.meta.insert(
            "reply_url".into(),
            serde_json::json!(format!("http://{addr}/")),
        );
        mgr.ingest(env).await.unwrap();

        let mut got = Vec::new();
        let mut saw_progress = false;
        let mut last_activity = std::time::Instant::now();
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Some(body)) => {
                    got.push(body);
                    last_activity = std::time::Instant::now();
                    let blob = got.join("\n");
                    if blob.contains("收到")
                        && (blob.contains("思考中")
                            || blob.contains("读取")
                            || blob.contains("写入")
                            || blob.contains("找文件")
                            || blob.contains("命令"))
                    {
                        saw_progress = true;
                    }
                }
                _ => {
                    // Let Write/Shell/final reply land; early drop cancelled the last hop.
                    if py.is_file()
                        && saw_progress
                        && last_activity.elapsed() > Duration::from_secs(6)
                    {
                        break;
                    }
                }
            }
        }
        drop(mgr);
        let blob = got.join("\n---\n");
        let _ = std::fs::write(
            workspace.join(".grok-hyper/overnight/im_live_posts.txt"),
            &blob,
        );
        assert!(
            blob.contains("收到"),
            "Chinese ACK missing in IM posts: {blob}"
        );
        assert!(
            !blob.contains("The user wants"),
            "English CoT must not be posted to Chinese IM: {blob}"
        );
        assert!(
            !blob.contains("keep visible text") && !blob.contains("The last hop without tools"),
            "mixed English CoT must not be posted to Chinese IM: {blob}"
        );
        assert!(
            blob.contains("思考中")
                || blob.contains("读取")
                || blob.contains("写入")
                || blob.contains("定位")
                || blob.contains("命令"),
            "no Chinese think/tool progress: {blob}"
        );
        assert!(py.is_file(), "webhook agent did not write {}", py.display());
        assert!(
            blob.contains("写入") || blob.contains("命令"),
            "IM turn must show write/shell progress before close: {blob}"
        );
        let out = std::process::Command::new("python3")
            .arg(&py)
            .output()
            .expect("run im_live.py");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("IM_LIVE_OK"),
            "stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore = "live grok IM webhook soak"]
    async fn live_im_webhook_chinese_queue() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio::sync::mpsc;

        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("repo root");
        let py = workspace.join(".grok-hyper/overnight/im_queue.py");
        let _ = std::fs::remove_file(&py);

        let (cfg, _) = Config::load_or_init().expect("config");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reply = format!("http://{addr}/");
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while tokio::time::Instant::now() < deadline {
                    match tokio::time::timeout_at(deadline, sock.read(&mut tmp)).await {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                        _ => break,
                    }
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&buf[..i]);
                        let want = headers
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                if k.eq_ignore_ascii_case("content-length") {
                                    v.trim().parse::<usize>().ok()
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);
                        if buf.len() >= i + 4 + want {
                            let body =
                                String::from_utf8_lossy(&buf[i + 4..i + 4 + want]).into_owned();
                            let _ = tx.send(body);
                            let _ = sock
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                .await;
                            break;
                        }
                    }
                }
            }
        });

        let dir =
            std::env::temp_dir().join(format!("hyper-im-q-{}", uuid::Uuid::new_v4().simple()));
        let router = SessionRouter::open(dir.join("routes.json")).unwrap();
        let cfg = std::sync::Arc::new(cfg);
        let cfg_h = cfg.clone();
        let ws = workspace.clone();
        let mgr = ChannelManager::start(
            crate::channel::ChannelsConfig::default(),
            router,
            move |env, cancel, steer| {
                let cfg_h = cfg_h.clone();
                let ws = ws.clone();
                async move { agent_inbound(&cfg_h, ws, env, cancel, steer).await }
            },
        );
        let sid = format!(
            "overnight-im-queue-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
        let mut first = NativePayload::text_only(
            "webhook",
            "工作区就是当前仓库。Write `.grok-hyper/overnight/im_queue.py`，只打印一行 IM_QUEUE_OK。Shell 跑 python3 .grok-hyper/overnight/im_queue.py。不要改 crates/。中文一句结束。",
        );
        first.session_id = sid.clone();
        first
            .meta
            .insert("reply_url".into(), serde_json::json!(reply.clone()));
        let ingested = mgr.ingest(first).await.unwrap();
        assert_eq!(ingested.session_id, sid);

        let mut got = Vec::new();
        let mut queued_follow = false;
        let mut last_activity = std::time::Instant::now();
        let deadline = std::time::Instant::now() + Duration::from_secs(180);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Some(body)) => {
                    got.push(body.clone());
                    last_activity = std::time::Instant::now();
                    if !queued_follow && body.contains("收到，正在处理") {
                        let mut follow = NativePayload::text_only(
                            "webhook",
                            "在 `.grok-hyper/overnight/im_queue.py` 末尾追加一行注释 # IM_QUEUE_MARK。不要改 crates/。中文一句结束。",
                        );
                        follow.session_id = sid.clone();
                        follow
                            .meta
                            .insert("reply_url".into(), serde_json::json!(reply.clone()));
                        mgr.ingest(follow).await.unwrap();
                        queued_follow = true;
                    }
                }
                _ => {
                    let text = std::fs::read_to_string(&py).unwrap_or_default();
                    let blob = got.join("\n");
                    if queued_follow
                        && text.contains("IM_QUEUE_OK")
                        && text.contains("IM_QUEUE_MARK")
                        && blob.contains("当前任务结束后回复")
                        && last_activity.elapsed() > Duration::from_secs(6)
                    {
                        break;
                    }
                }
            }
        }
        drop(mgr);
        let blob = got.join("\n---\n");
        let _ = std::fs::write(
            workspace.join(".grok-hyper/overnight/im_queue_posts.txt"),
            &blob,
        );
        assert!(blob.contains("收到，正在处理"), "first ACK missing: {blob}");
        assert!(
            blob.contains("当前任务结束后回复"),
            "queue ACK missing: {blob}"
        );
        assert!(
            !blob.contains("The user wants"),
            "English CoT must not be posted: {blob}"
        );
        let text = std::fs::read_to_string(&py).unwrap_or_default();
        assert!(
            text.contains("IM_QUEUE_OK") && text.contains("IM_QUEUE_MARK"),
            "queued follow-up did not land in {}: {text}",
            py.display()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore = "live grok IM webhook soak"]
    async fn live_im_webhook_chinese_heartbeat() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio::sync::mpsc;

        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("repo root");
        let marker = workspace.join(".grok-hyper/overnight/im_hb.ok");
        let _ = std::fs::remove_file(&marker);

        let (cfg, _) = Config::load_or_init().expect("config");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while tokio::time::Instant::now() < deadline {
                    match tokio::time::timeout_at(deadline, sock.read(&mut tmp)).await {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                        _ => break,
                    }
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&buf[..i]);
                        let want = headers
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                if k.eq_ignore_ascii_case("content-length") {
                                    v.trim().parse::<usize>().ok()
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);
                        if buf.len() >= i + 4 + want {
                            let body =
                                String::from_utf8_lossy(&buf[i + 4..i + 4 + want]).into_owned();
                            let _ = tx.send(body);
                            let _ = sock
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                .await;
                            break;
                        }
                    }
                }
            }
        });

        let dir =
            std::env::temp_dir().join(format!("hyper-im-hb-{}", uuid::Uuid::new_v4().simple()));
        let router = SessionRouter::open(dir.join("routes.json")).unwrap();
        let cfg = std::sync::Arc::new(cfg);
        let cfg_h = cfg.clone();
        let ws = workspace.clone();
        let mgr = ChannelManager::start(
            crate::channel::ChannelsConfig::default(),
            router,
            move |env, cancel, steer| {
                let cfg_h = cfg_h.clone();
                let ws = ws.clone();
                async move { agent_inbound(&cfg_h, ws, env, cancel, steer).await }
            },
        );
        let sid = format!(
            "overnight-im-hb-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
        let mut env = NativePayload::text_only(
            "webhook",
            "工作区就是当前仓库。只用一次 Shell：python3 -c \"import time,pathlib; time.sleep(45); pathlib.Path('.grok-hyper/overnight/im_hb.ok').write_text('IM_HB_OK\\n'); print('IM_HB_OK')\"。不要 Write，不要 Glob，不要 Grep，不要 Search，不要改 crates/。中文一句结束。",
        );
        env.session_id = sid.clone();
        env.meta.insert(
            "reply_url".into(),
            serde_json::json!(format!("http://{addr}/")),
        );
        mgr.ingest(env).await.unwrap();

        let mut got = Vec::new();
        let mut last_activity = std::time::Instant::now();
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Some(body)) => {
                    got.push(body);
                    last_activity = std::time::Instant::now();
                }
                _ => {
                    let blob = got.join("\n");
                    if marker.is_file()
                        && blob.matches("还在处理").count() >= 2
                        && last_activity.elapsed() > Duration::from_secs(6)
                    {
                        break;
                    }
                }
            }
        }
        drop(mgr);
        let blob = got.join("\n---\n");
        let _ = std::fs::write(
            workspace.join(".grok-hyper/overnight/im_hb_posts.txt"),
            &blob,
        );
        assert!(blob.contains("收到"), "ACK missing: {blob}");
        assert!(
            blob.matches("还在处理").count() >= 2,
            "long Shell must keep beating every 20s: {blob}"
        );
        assert!(
            !blob.contains("The user wants"),
            "English CoT must not be posted: {blob}"
        );
        let text = std::fs::read_to_string(&marker).unwrap_or_default();
        assert!(
            text.contains("IM_HB_OK"),
            "long Shell did not finish: {}",
            marker.display()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
