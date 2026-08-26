//! Shared agent turn for sidecar / TUI / web. Frozen `tools[]` — vision goes
//! through `ChatMessage.parts`, not a new OpenAI tool.

use crate::config::Config;
use crate::media::MediaPart;
use crate::session::{SessionEvent, SessionLog, SessionMode, StoredMedia};
use crate::template::ChatMessage;
use crate::{Agent, RunOpts, ToolSet, TransportCompleter};

use super::types::{TurnRequest, TurnResult};

pub async fn execute_turn(
    cfg: Config,
    agents_md: bool,
    agents_md_head: bool,
    req: TurnRequest,
) -> TurnResult {
    if req.cancel.is_cancelled() {
        return TurnResult::aborted();
    }
    if req.snapshot.imagine_mode {
        return run_imagine(cfg, req).await;
    }

    let mut cfg = cfg;
    if !req.snapshot.model.is_empty()
        && std::env::var("HYPER_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_none()
    {
        cfg.server.model = req.snapshot.model.clone();
    }

    let mut opts = RunOpts::from_config(&cfg, req.snapshot.workspace.clone());
    opts.print = false;
    opts.session_id = if req.snapshot.session_id.is_empty() {
        "hyper".into()
    } else {
        req.snapshot.session_id.clone()
    };
    opts.agents_md = agents_md;
    opts.agents_md_head = agents_md_head;
    match req.snapshot.mode {
        SessionMode::Chat => {
            opts.with_tools = false;
            opts.tool_set = ToolSet::None;
        }
        SessionMode::Code => {
            opts.with_tools = true;
            opts.tool_set = ToolSet::Code;
        }
        SessionMode::Think => {
            opts.with_tools = true;
            opts.tool_set = ToolSet::Agent;
            opts.max_steps = cfg.policy.max_steps_think;
        }
        SessionMode::Agent => {
            opts.with_tools = true;
            opts.tool_set = ToolSet::Agent;
        }
    }
    opts.effort_locked = req.snapshot.effort_locked
        || matches!(req.snapshot.mode, SessionMode::Think | SessionMode::Chat)
        || !req.snapshot.policy.enabled;
    opts.persist_session = req.persist;
    opts.session_mode = req.snapshot.mode;
    opts.plan_mode = req.snapshot.plan_mode;
    opts.clarify_mode = req.snapshot.clarify_mode;
    opts.low_precision = req.snapshot.low_precision;
    opts.confined = req.snapshot.workspace_confined;
    opts.permit = req.permit;
    opts.clarify = req.clarify;
    apply_turn_window(&mut opts, req.snapshot.window);

    let cancel = req.cancel.clone();
    let steer = req.steer.clone();
    let messages = req.messages;
    let persist = req.persist;
    let prompt = req.prompt;
    let parts = req.parts;
    let policy = req.snapshot.policy;
    let emit = req.emit.clone();
    let completer = match crate::llm_http::retry_transient(
        &cancel,
        || TransportCompleter::connect(&cfg, policy.clone()),
        |attempt, wait, err| {
            emit.append(crate::session::SessionEvent::delta_reset());
            emit.append(crate::session::SessionEvent::delta_chunk(
                crate::session::DeltaChannel::Reasoning,
                crate::llm_http::retry_status_line(attempt, wait),
            ));
            eprintln!("[net] connect retry #{attempt} after {wait:?}: {err}");
        },
    )
    .await
    {
        Ok(c) => {
            emit.append(crate::session::SessionEvent::delta_reset());
            c
        }
        Err(e) => {
            return if cancel.is_cancelled() || e.to_string().contains("aborted") {
                TurnResult::aborted()
            } else {
                TurnResult::fail(e.to_string())
            };
        }
    };
    let mut agent = match Agent::new(completer, opts) {
        Ok(a) => a,
        Err(e) => return TurnResult::fail(e.to_string()),
    };
    agent.set_cancel(cancel.clone());
    agent.set_steer(steer);
    agent.set_emit(req.emit);
    let out = if persist || messages.is_empty() {
        if parts.is_empty() {
            agent.run(&prompt).await
        } else {
            let text = if prompt.trim().is_empty() {
                " "
            } else {
                prompt.as_str()
            };
            let mut msg = ChatMessage::user(text);
            msg.parts = parts;
            agent.run_message(msg).await
        }
    } else {
        agent.load_messages(messages);
        agent.drive().await
    };
    match out {
        Ok(out) => TurnResult {
            text: out.text,
            stop_reason: out.stop_reason.clone(),
            aborted: out.stop_reason.as_deref() == Some("aborted"),
            error: None,
            events: Vec::new(),
            pending_steer: out.pending_steer,
            streamed: true,
        },
        Err(e) => {
            if cancel.is_cancelled() {
                TurnResult::aborted()
            } else {
                TurnResult::fail(e.to_string())
            }
        }
    }
}

async fn run_imagine(cfg: Config, req: TurnRequest) -> TurnResult {
    if req.cancel.is_cancelled() {
        return TurnResult::aborted();
    }
    let prompt = req.prompt.trim();
    if prompt.is_empty() {
        return TurnResult::fail("image generation needs a text prompt");
    }
    let user_media: Vec<StoredMedia> = req
        .parts
        .iter()
        .map(|p: &MediaPart| StoredMedia {
            kind: p.kind.as_str().into(),
            mime: p.mime.clone(),
            url: p.url.clone(),
        })
        .collect();
    let mut log = if req.persist && !req.snapshot.session_id.is_empty() {
        SessionLog::open(&req.snapshot.session_id).ok()
    } else {
        None
    };
    if let Some(log) = log.as_mut() {
        let _ = log.append(SessionEvent::user(req.prompt.clone()).with_media(user_media));
    }
    // No fake image_generation tool row: the assistant event already carries
    // the shot (same bubble as uploaded images). A persisted tool card made
    // the turn draw twice.
    let out = tokio::select! {
        biased;
        _ = req.cancel.cancelled() => return TurnResult::aborted(),
        r = crate::imagine::generate(&cfg, &req.prompt, &req.snapshot.workspace, &req.cancel) => r,
    };
    match out {
        Ok(out) => {
            let assistant = SessionEvent::assistant(out.caption.clone(), String::new(), None)
                .with_media(out.stored);
            req.emit.append(assistant.clone());
            let stop = SessionEvent::stop("stop");
            req.emit.append(stop.clone());
            if let Some(log) = log.as_mut() {
                let _ = log.append(assistant);
                let _ = log.append(stop);
            }
            TurnResult {
                text: out.caption,
                stop_reason: Some("stop".into()),
                streamed: true,
                ..TurnResult::default()
            }
        }
        Err(e) => {
            if e == "aborted" || req.cancel.is_cancelled() {
                TurnResult::aborted()
            } else {
                TurnResult::fail(e)
            }
        }
    }
}

fn apply_turn_window(opts: &mut RunOpts, window: u32) {
    if window == 0 {
        return;
    }
    opts.working_window = window;
    opts.generation_reserve =
        crate::agent::clamp_generation_reserve(window, crate::agent::DEFAULT_GENERATION_RESERVE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::DEFAULT_GENERATION_RESERVE;
    use crate::config::{Config, CODING_CTX_TOKENS};

    #[test]
    fn saved_262k_overrides_config_window_and_reserve() {
        let mut cfg = Config::default();
        cfg.context.working_window = 500_000;
        let mut opts = RunOpts::from_config(&cfg, std::path::PathBuf::from("/tmp/ws"));
        assert_eq!(opts.working_window, 500_000);
        apply_turn_window(&mut opts, CODING_CTX_TOKENS);
        assert_eq!(opts.working_window, CODING_CTX_TOKENS);
        assert_eq!(
            opts.generation_reserve,
            crate::agent::clamp_generation_reserve(CODING_CTX_TOKENS, DEFAULT_GENERATION_RESERVE)
        );
        assert_eq!(opts.generation_reserve, DEFAULT_GENERATION_RESERVE);
    }

    #[test]
    fn small_window_clamps_reserve() {
        let cfg = Config::default();
        let mut opts = RunOpts::from_config(&cfg, std::path::PathBuf::from("/tmp/ws"));
        apply_turn_window(&mut opts, 8_192);
        assert_eq!(opts.working_window, 8_192);
        assert_eq!(opts.generation_reserve, 8_192 / 4);
    }
}
