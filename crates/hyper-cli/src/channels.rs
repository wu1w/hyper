//! `hyper --channels`: QwenPaw-form inbound (webhook + telegram) → Agent → reply.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use hyper_loop::channel::{reply_parts, reply_text, run_channels, ContentPart, NativePayload};
use hyper_loop::config::Config;
use hyper_loop::session::SessionMode;
use hyper_loop::slash::{low_precision_text, mcp_text, parse_slash_with_periphery, skills_text};
use hyper_loop::vendor;
use hyper_loop::{Agent, RunOpts, ToolSet, TransportCompleter};

use super::Cli;

pub async fn run(cli: Cli) -> Result<ExitCode> {
    let (cfg, path) = Config::load_or_init().context("load config")?;
    vendor::verify_qwen38().ok();
    eprintln!("config: {}", path.display());

    let workspace = cli
        .workspace
        .clone()
        .unwrap_or(std::env::current_dir().context("cwd")?);
    let policy = super::think_policy(&cfg, cli.fast, cli.think.as_deref(), cli.mode.as_deref());
    let mode = cli
        .mode
        .as_deref()
        .and_then(|s| s.parse::<SessionMode>().ok())
        .unwrap_or(SessionMode::Agent);

    let cfg = Arc::new(cfg);
    let cfg_h = cfg.clone();
    let workspace = workspace.clone();
    let agents_md = !cli.no_agents_md;
    let agents_md_head = cli.agents_md_head;
    let effort_locked =
        cli.fast || cli.think.is_some() || matches!(mode, SessionMode::Think | SessionMode::Chat);

    run_channels(cfg.channels.clone(), move |env: NativePayload| {
        let cfg = cfg_h.clone();
        let workspace = workspace.clone();
        let policy = policy.clone();
        async move {
            handle_inbound(
                cfg,
                workspace,
                mode,
                policy,
                effort_locked,
                agents_md,
                agents_md_head,
                env,
            )
            .await
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(ExitCode::SUCCESS)
}

async fn handle_inbound(
    cfg: Arc<Config>,
    workspace: std::path::PathBuf,
    mode: SessionMode,
    policy: hyper_loop::policy::ThinkPolicy,
    effort_locked: bool,
    agents_md: bool,
    agents_md_head: bool,
    env: NativePayload,
) -> hyper_loop::Result<Vec<ContentPart>> {
    let home = Config::home_dir().ok();
    let skills = hyper_loop::skills::SkillCatalog::load(
        home.as_deref().unwrap_or_else(|| std::path::Path::new("")),
        &workspace,
    );
    let mcp = hyper_loop::mcp::McpRegistry::load(home.as_deref(), &workspace, &cfg.mcp);
    let query = env.query_text();
    let mut msg = env.to_chat_message();
    if let Some(cmd) = parse_slash_with_periphery(&query, &skills, Some(&mcp)) {
        if let Some(text) = local_slash_text(&cmd) {
            return Ok(reply_text(text));
        }
        match cmd {
            hyper_loop::SlashCmd::Skills => return Ok(reply_text(skills_text(&skills))),
            hyper_loop::SlashCmd::Mcp => return Ok(reply_text(mcp_text(&mcp))),
            hyper_loop::SlashCmd::InvokeSkill { name, args } => {
                msg.content = Some(hyper_loop::sticky::skill_turn_prompt(&name, &args));
            }
            hyper_loop::SlashCmd::InvokeMcp { name, args } => {
                msg.content = Some(hyper_loop::sticky::mcp_turn_prompt(&name, &args));
            }
            hyper_loop::SlashCmd::Cron { args } => {
                return Ok(reply_text(hyper_loop::cron::apply_slash(&workspace, &args)));
            }
            hyper_loop::SlashCmd::LowPrecision { on } => {
                let flag = match on {
                    Some(v) => {
                        if let Ok(path) = Config::default_path() {
                            let _ = Config::mutate_disk(&path, |c| {
                                c.policy.low_precision = v;
                            });
                        }
                        v
                    }
                    None => cfg.policy.low_precision,
                };
                return Ok(reply_text(low_precision_text(flag)));
            }
            hyper_loop::SlashCmd::Clarify { on } => {
                return Ok(reply_text(hyper_loop::slash::clarify_text(
                    on.unwrap_or(true),
                    false,
                )));
            }
            hyper_loop::SlashCmd::Imagine { on, prompt } => {
                if let Some(p) = prompt.filter(|s| !s.trim().is_empty()) {
                    let cancel = hyper_loop::CancelFlag::new();
                    match hyper_loop::imagine::generate(&cfg, &p, &workspace, &cancel).await {
                        Ok(out) => {
                            let mut parts = Vec::new();
                            if !out.caption.is_empty() {
                                parts.push(ContentPart::text(out.caption));
                            }
                            for m in out.stored {
                                let url = if m.url.starts_with("http") || m.url.starts_with("data:")
                                {
                                    m.url
                                } else {
                                    workspace.join(&m.url).display().to_string()
                                };
                                parts.push(ContentPart::Image {
                                    image_url: url.clone(),
                                    url,
                                    mime: m.mime,
                                });
                            }
                            return Ok(if parts.is_empty() {
                                reply_text("generated image")
                            } else {
                                parts
                            });
                        }
                        Err(e) => return Ok(reply_text(e)),
                    }
                }
                return Ok(reply_text(hyper_loop::slash::imagine_text(
                    on.unwrap_or(true),
                )));
            }
            _ => {}
        }
    }

    let ws = workspace.clone();
    let mut opts = RunOpts::from_config(&cfg, workspace);
    opts.print = false;
    opts.session_id = if env.session_id.is_empty() {
        "channel".into()
    } else {
        env.session_id.clone()
    };
    opts.persist_session = true;
    opts.channel = if env.channel.trim().is_empty() {
        "im".into()
    } else {
        env.channel.clone()
    };
    opts.session_mode = mode;
    opts.agents_md = agents_md;
    opts.agents_md_head = agents_md_head;
    opts.effort_locked = effort_locked || !policy.enabled;
    match mode {
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
    hyper_loop::agent::apply_unattended_policy(&mut opts, &cfg);

    let completer = TransportCompleter::connect(&cfg, policy).await?;
    let mut agent = Agent::new(completer, opts)?;
    let out = agent.run_message(msg).await?;
    Ok(reply_parts(&out.text, &ws, &out.channel_files))
}

fn local_slash_text(cmd: &hyper_loop::SlashCmd) -> Option<String> {
    use hyper_loop::slash::{help_text, unsupported_text, version_text};
    match cmd {
        hyper_loop::SlashCmd::Help => Some(help_text()),
        hyper_loop::SlashCmd::Version => Some(version_text()),
        hyper_loop::SlashCmd::Unsupported { name } => Some(unsupported_text(name)),
        _ => None,
    }
}
