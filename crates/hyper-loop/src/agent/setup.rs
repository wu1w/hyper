//! Construct the agent: periphery, session log, AGENTS.md, public setters.

use std::collections::HashSet;

use serde_json::Value;

use super::delta;
use super::guard;
use super::{clamp_generation_reserve, Agent, Completer, RunOpts, ToolSet};
use crate::channel::SteerSlot;
use crate::config::Config;
use crate::error::Result;
use crate::mcp::McpRegistry;
use crate::memory::MemoryStore;
use crate::paw_loop::{DoomLoopGate, Gate, IterationGate, StopHandler, TimeoutGate};
use crate::policy::{EffortController, ThinkPolicy};
use crate::prompt::{periphery_section, session_prompt};
use crate::session::{tools_hash, SessionLog, SessionStart};
use crate::skills::SkillCatalog;
use crate::sticky;
use crate::template::ChatMessage;
use crate::tool_calls::{CancelFlag, ToolCoordinator, COORDINATOR_OWNED_EXEC_TIMEOUT_SECS};
use crate::tools::{BlobStore, Workspace};
use crate::tools_schema::{
    agent_tools, code_tools, computer_use_tool, dispatch_name, dynamic_mcp_tools, search_tool,
    view_tool,
};

impl<C: Completer> Agent<C> {
    pub fn new(completer: C, opts: RunOpts) -> Result<Self> {
        crate::subagent::ensure_live_runner();
        if opts.persist_session {
            if let Some(dir) = &opts.session_dir {
                crate::subagent::reap_orphans(dir);
            } else if let Ok(h) = crate::config::Config::home_dir() {
                crate::subagent::reap_orphans(&h.join("sessions"));
            }
        }
        let workspace = Workspace::open(&opts.workspace, opts.confined)?;
        let tool_set = if !opts.with_tools {
            ToolSet::None
        } else {
            opts.tool_set
        };
        let (mut system, tools, memory, skills, mcp) = bind_periphery(&opts, &workspace, tool_set);
        if opts.agents_md {
            match read_agents_md(
                workspace.root(),
                opts.agents_md_max_tokens,
                opts.agents_md_head,
            ) {
                AgentsMd::Ok(extra) => {
                    system.push_str("\n\n# AGENTS.md\n");
                    system.push_str(&extra);
                }
                AgentsMd::TooLarge => {
                    if opts.print {
                        eprintln!(
                            "hyper: AGENTS.md omitted (over {} tok; pass --agents-md-head to clip)",
                            opts.agents_md_max_tokens
                        );
                    }
                }
                AgentsMd::Missing => {}
            }
        }
        if completer.recasts_xai_product() {
            crate::platform_prefix::append_xai_product_closer(&mut system);
        }
        if super::im_bridge_channel(&opts.channel) {
            sticky::ensure_im_system_lock(&mut system);
        }

        let gates = vec![
            Gate::from(DoomLoopGate::grok_default()),
            Gate::from(IterationGate::new(opts.max_steps.max(1))),
            Gate::from(TimeoutGate::new(opts.max_wall)),
        ];
        let handler = StopHandler::with_gates(gates);
        handler.reset_turn(&opts.session_id);

        let coordinator = ToolCoordinator::new(None);
        coordinator.register_hook(
            "bash",
            Some(opts.bash_timeout_secs),
            Some(COORDINATOR_OWNED_EXEC_TIMEOUT_SECS),
        );
        coordinator.register_hook(
            "Shell",
            Some(opts.bash_timeout_secs),
            Some(COORDINATOR_OWNED_EXEC_TIMEOUT_SECS),
        );
        coordinator.register_hook(
            "run_code",
            Some(opts.bash_timeout_secs),
            Some(COORDINATOR_OWNED_EXEC_TIMEOUT_SECS),
        );
        coordinator.set_offload_on_deadline(true);

        let wire = completer
            .policy()
            .unwrap_or_else(ThinkPolicy::agent_default);
        let (mut messages, log) = bind_session(&opts, &system, &tools, wire.clone());
        if super::im_bridge_channel(&opts.channel) {
            sticky::ensure_im_system_lock_on_messages(&mut messages);
        }
        let mut policy = if opts.effort_locked {
            wire.clone()
        } else {
            log.as_ref()
                .and_then(|l| l.policy())
                .unwrap_or_else(|| wire.clone())
        };
        // Old JSONL froze medium. Unlocked Agent/Code on grok-4.6 must match
        // the wire default (xhigh), including IM `Agent::new` each message.
        if !opts.effort_locked
            && matches!(
                opts.session_mode,
                crate::session::SessionMode::Agent | crate::session::SessionMode::Code
            )
            && wire.effort == Some(crate::policy::Effort::Xhigh)
            && policy.effort != Some(crate::policy::Effort::Xhigh)
        {
            policy = wire;
        }
        completer.set_policy(policy.clone());
        let effort = EffortController::new(policy.clone(), opts.effort_locked);
        let media_caps = completer.media_caps();
        let media_max_bytes = opts.media_max_bytes.max(1);

        let blobs = BlobStore::new(opts.blob_dir.clone().unwrap_or_else(|| {
            opts.session_dir
                .as_ref()
                .map(|d| d.join("blobs"))
                .or_else(|| Config::home_dir().ok().map(|h| h.join("blobs")))
                .unwrap_or_else(|| std::env::temp_dir().join("hyper-blobs"))
        }));
        if opts.child.is_none() {
            completer.pin_session(&opts.session_id);
        }
        if opts.print {
            if let Some(o) = &opts.working_window_overlay {
                eprintln!(
                    "hyper: HYPER_WORKING_WINDOW={} overlays config.toml working_window={}; compact uses the env value. Unset the env to use the file.",
                    o.from_env, o.from_file
                );
            }
        }
        // Search index: first user turn, never Agent::new (Windows home hang).
        let code_index: Option<std::sync::Arc<crate::tools::CodeIndex>> = None;
        let web = opts
            .web
            .enabled
            .then(|| crate::tools::WebRunner::new(opts.web.clone(), &mcp));
        Ok(Self {
            completer,
            workspace,
            handler,
            coordinator,
            session_id: opts.session_id,
            run_id: None,
            turn_id: None,
            current_step: 0,
            messages,
            tools,
            pending_stop: None,
            oracle_cmd: None,
            oracle_runs: 0,
            print: opts.print,
            limits: opts.tool_limits,
            inherit_env: opts.inherit_env,
            working_window: opts.working_window,
            generation_reserve: clamp_generation_reserve(
                opts.working_window,
                opts.generation_reserve,
            ),
            compact_ratio: opts.compact_ratio,
            effort,
            log,
            last_policy: policy,
            blobs,
            memory,
            skills,
            mcp,
            web,
            media_caps,
            media_max_bytes,
            media_bins: opts.media_bins,
            cancel: CancelFlag::new(),
            steer: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            emit: None,
            stdio: std::sync::Arc::new(delta::StdioState::default()),
            plan_mode: opts.plan_mode,
            clarify_mode: opts.clarify_mode,
            permit: opts.permit,
            clarify: opts.clarify,
            parse_stop_after: 3,
            last_spoken: None,
            tool_evidence: String::new(),
            read_paths: HashSet::new(),
            observed_paths: HashSet::new(),
            window_overlay: opts.working_window_overlay,
            edit_guard: guard::EditGuard::new(),
            channel: opts.channel,
            editor_files: opts.editor_files,
            code_index,
            index_build: None,
            speculate: None,
            in_flight_diag: None,
            model_hops: std::sync::atomic::AtomicU32::new(0),
            search_calls: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            search_queries: std::sync::Mutex::new(Vec::new()),
            grep_calls: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            read_full: std::sync::Mutex::new(HashSet::new()),
            physics_nudged: false,
            official_compaction: None,
            xai_compact: opts.xai_compact,
            config: opts.config,
            child: opts.child,
            persist_session: opts.persist_session,
            session_dir: opts.session_dir,
            home: opts.home,
            channel_files: Vec::new(),
        })
    }

    pub fn load_messages(&mut self, messages: Vec<ChatMessage>) {
        self.messages = messages;
    }

    pub fn set_cancel(&mut self, cancel: CancelFlag) {
        self.cancel = cancel;
    }

    pub fn set_steer(&mut self, steer: SteerSlot) {
        self.steer = steer;
    }

    pub fn set_emit(&mut self, emit: crate::sidecar::EventSink) {
        self.emit = Some(emit);
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    pub(crate) fn start_code_index(&mut self) {
        if self.code_index.is_some() || self.index_build.is_some() {
            return;
        }
        if !crate::tools_schema::has_tool(&self.tools, "search") {
            return;
        }
        let root = self.workspace.root().to_path_buf();
        self.index_build = Some(tokio::task::spawn_blocking(move || {
            std::sync::Arc::new(crate::tools::CodeIndex::build(&root))
        }));
    }

    pub(crate) async fn settle_code_index(&mut self) {
        if self.code_index.is_some() {
            return;
        }
        let Some(h) = self.index_build.take() else {
            return;
        };
        if let Ok(idx) = h.await {
            self.code_index = Some(idx);
        }
    }

    pub(crate) fn speculate_ctx(&self) -> super::speculate::SpeculateCtx {
        super::speculate::SpeculateCtx {
            workspace: self.workspace.clone(),
            limits: self.limits,
            inherit_env: self.inherit_env,
            blobs: self.blobs.clone(),
            cancel: self.cancel.clone(),
            code_index: self.code_index.clone(),
            media_caps: self.media_caps.clone(),
            media_bins: self.media_bins.clone(),
            media_max_bytes: self.media_max_bytes,
            child: self.child,
            plan_mode: self.plan_mode,
            skip_grep: super::notes::forbids_grep(self.last_real_user()),
            skip_glob: super::notes::forbids_glob(self.last_real_user()),
            search_queries: crate::lock_unpoison(&self.search_queries).clone(),
            search_used: self.search_calls.load(std::sync::atomic::Ordering::Relaxed),
            named_new: super::dispatch::named_new_files(&self.workspace, self.last_real_user()),
            search_located: super::dispatch::located_search_paths(&self.messages)
                .into_iter()
                .map(|p| super::dispatch::canon_ws_path(&self.workspace, &p))
                .collect(),
            search_shown_idents: super::dispatch::shown_dump_idents(&self.messages),
        }
    }
}

pub(crate) fn bind_session(
    opts: &RunOpts,
    system: &str,
    tools: &[Value],
    policy: ThinkPolicy,
) -> (Vec<ChatMessage>, Option<SessionLog>) {
    let fresh = vec![ChatMessage::system(system.to_string())];
    if !opts.persist_session || opts.session_id.is_empty() {
        return (fresh, None);
    }
    let open = |id: &str| {
        if let Some(dir) = &opts.session_dir {
            SessionLog::open_in(dir, id)
        } else {
            SessionLog::open(id)
        }
    };
    let create = |start: SessionStart| {
        if let Some(dir) = &opts.session_dir {
            SessionLog::create_in(dir, start)
        } else {
            SessionLog::create(start)
        }
    };
    match open(&opts.session_id) {
        Ok(log) => (log.messages(), Some(log)),
        Err(_) => {
            let mut start = SessionStart::new(
                opts.session_id.clone(),
                opts.workspace.display().to_string(),
                opts.session_mode,
                system,
                tools_hash(tools),
                policy,
            );
            if !opts.channel.is_empty() {
                start.channel = opts.channel.clone();
            }
            match create(start) {
                Ok(log) => (fresh, Some(log)),
                Err(_) => (fresh, None),
            }
        }
    }
}

pub(crate) fn bind_periphery(
    opts: &RunOpts,
    workspace: &Workspace,
    tool_set: ToolSet,
) -> (
    String,
    Vec<Value>,
    Option<MemoryStore>,
    SkillCatalog,
    McpRegistry,
) {
    let home = opts.home.clone().or_else(|| Config::home_dir().ok());
    let extra_tools = opts.peripheral && matches!(tool_set, ToolSet::Agent);
    let memory = if opts.peripheral {
        home.as_ref().and_then(|h| MemoryStore::open(h).ok())
    } else {
        None
    };
    let skills = if opts.peripheral {
        SkillCatalog::load(
            home.as_deref().unwrap_or_else(|| std::path::Path::new("")),
            workspace.root(),
        )
    } else {
        SkillCatalog::default()
    };
    let mcp = if opts.peripheral {
        McpRegistry::load(home.as_deref(), workspace.root(), &opts.mcp)
    } else {
        McpRegistry::default()
    };

    let mut system = session_prompt(
        workspace.root(),
        home.as_deref(),
        &opts.prompt_file,
        opts.coding_identity,
    );
    if extra_tools {
        let skills_md = if opts.skills_auto_catalog {
            skills.catalog_markdown()
        } else {
            String::new()
        };
        let mcp_md = if opts.mcp_auto_catalog {
            mcp.catalog_markdown()
        } else {
            String::new()
        };
        system.push_str(&periphery_section(&skills_md, &mcp_md));
    }

    let mut tools = match tool_set {
        ToolSet::None => Vec::new(),
        ToolSet::Agent => agent_tools(),
        ToolSet::Code => code_tools(),
    };
    if extra_tools && !mcp.servers.is_empty() {
        tools.extend(dynamic_mcp_tools());
    }
    if opts.code_search && matches!(tool_set, ToolSet::Agent) {
        tools.push(search_tool());
    }
    if opts.media && matches!(tool_set, ToolSet::Agent) {
        tools.push(view_tool());
    }
    if opts.computer_use && matches!(tool_set, ToolSet::Agent) && opts.child.is_none() {
        tools.push(computer_use_tool());
    }
    if opts.child.is_some() {
        tools.retain(|t| {
            t["function"]["name"]
                .as_str()
                .map(|n| dispatch_name(n) != "task")
                .unwrap_or(true)
        });
    }
    (system, tools, memory, skills, mcp)
}

pub(crate) enum AgentsMd {
    Missing,
    Ok(String),
    TooLarge,
}

pub(crate) fn read_agents_md(root: &std::path::Path, max_tokens: u32, head: bool) -> AgentsMd {
    let path = root.join("AGENTS.md");
    let Ok(raw) = std::fs::read_to_string(path) else {
        return AgentsMd::Missing;
    };
    if raw.trim().is_empty() {
        return AgentsMd::Missing;
    }
    let n = sticky::tokens(&raw);
    if n <= max_tokens {
        return AgentsMd::Ok(raw);
    }
    if head {
        let clipped = sticky::clip_to_tokens(&raw, max_tokens);
        if clipped.is_empty() {
            AgentsMd::TooLarge
        } else {
            AgentsMd::Ok(clipped)
        }
    } else {
        AgentsMd::TooLarge
    }
}
