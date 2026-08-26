//! Live child Agent. Kept out of `dispatch`’s type graph (registered as `dyn Fn`).
//! Uses the parent Config snapshot on `SpawnReq` — no `load_file` / `apply_env`.

use crate::agent::{Agent, RunOpts, ToolSet, TransportCompleter};

use super::policy::{CapabilityMode, SubagentType};
use super::registry::ChildStatus;
use super::spawn::{summarize_text, wrap_prompt, ChildOutcome, SpawnReq};
use super::ChildCtx;

pub async fn run(req: SpawnReq) -> ChildOutcome {
    match run_inner(req).await {
        Ok(out) => out,
        Err(e) => ChildOutcome {
            summary: String::new(),
            key_paths: Vec::new(),
            status: if e == "cancelled" {
                ChildStatus::Cancelled
            } else {
                ChildStatus::Failed
            },
            error: Some(e),
        },
    }
}

async fn run_inner(req: SpawnReq) -> Result<ChildOutcome, String> {
    if req.cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    let mut cfg = req.config.clone();
    if let Some(model) = super::effective_model(req.model.as_deref()) {
        cfg.server.model = model;
    }
    let budget = cfg.think_budget();
    let policy = req.kind.think_policy(&budget);
    let completer = tokio::select! {
        biased;
        _ = req.cancel.cancelled() => return Err("cancelled".into()),
        c = TransportCompleter::connect(&cfg, policy) => c.map_err(|e| e.to_string())?,
    };

    let mut opts = RunOpts::from_config(&cfg, req.cwd.clone());
    opts.session_id = req.id.clone();
    opts.print = req.print;
    opts.persist_session = true;
    opts.session_dir = req.session_dir.clone().or_else(|| {
        req.home.as_ref().map(|h| h.join("sessions")).or_else(|| {
            crate::config::Config::home_dir()
                .ok()
                .map(|h| h.join("sessions"))
        })
    });
    opts.home = req.home.clone();
    opts.child = Some(ChildCtx {
        kind: req.kind,
        capability: req.capability,
    });
    opts.with_tools = true;
    opts.tool_set = ToolSet::Agent;
    opts.plan_mode = matches!(req.kind, SubagentType::Explore | SubagentType::Plan)
        || req.capability == CapabilityMode::ReadOnly;
    opts.channel = "subagent".into();
    opts.max_steps = req.kind.default_max_steps();
    opts.permit = req.permit;
    opts.clarify = req.clarify;

    let mut agent = Agent::new(completer, opts).map_err(|e| e.to_string())?;
    agent.set_cancel(req.cancel.clone());
    let prompt = wrap_prompt(req.kind, &req.prompt);
    let out = tokio::select! {
        biased;
        _ = req.cancel.cancelled() => return Err("cancelled".into()),
        r = agent.run(&prompt) => r.map_err(|e| e.to_string())?,
    };
    let (summary, key_paths) = summarize_text(req.kind, &out.text);
    let status = if out.stop_reason.as_deref() == Some("aborted") {
        ChildStatus::Cancelled
    } else {
        ChildStatus::Done
    };
    Ok(ChildOutcome {
        summary,
        key_paths,
        status,
        error: None,
    })
}
