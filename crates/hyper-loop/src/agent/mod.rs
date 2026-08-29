//! ReAct agent. Cursor hop geometry: empty tool-hop text, no Qwen lectures.

mod delta;
mod dispatch;
mod guard;
mod http;
mod notes;
mod responses;
mod setup;
mod speculate;
#[cfg(test)]
mod tests;
mod turn;
mod verify;
mod window;
mod workset;

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::channel::SteerSlot;
use crate::config::{Config, WorkingWindowOverlay};
use crate::error::Result;
use crate::family::Family;
use crate::mcp::{McpConfig, McpRegistry};
use crate::media::{MediaBins, MediaCaps};
use crate::memory::MemoryStore;
use crate::paw_loop::StopHandler;
use crate::permit::PermitHub;
use crate::policy::{EffortController, TemplateKwargs, ThinkPolicy};
use crate::session::{SessionLog, SessionMode};
use crate::skills::SkillCatalog;
use crate::template::ChatMessage;
use crate::tool_calls::{CancelFlag, ToolCall, ToolCoordinator};
use crate::tools::{BlobStore, CodeIndex, ToolLimits, Workspace};

pub use delta::TokenSink;
pub use http::{parse_cached_tokens, parse_turn, HttpCompleter, ParseOutcome};
pub use responses::{ResponsesCompleter, TransportCompleter};
pub(crate) use speculate::SpeculativeSlot;

/// Used by `http.rs` / `responses.rs` when wrapping native tool_calls.
pub(super) use dispatch::openai_tool_calls;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolSet {
    None,
    #[default]
    Agent,
    Code,
}

#[derive(Clone, Debug)]
pub struct ModelTurn {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub raw_tool_calls: Option<Vec<Value>>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub watchdog_hit: bool,
    pub parse_fail: bool,
    /// Prefix cache hits. `None` if the engine omitted the field.
    pub cached_tokens: Option<u64>,
    /// llama.cpp `timings.predicted_per_second` (decode tok/s). `None` if omitted.
    pub decode_tok_s: Option<f64>,
    /// Host `image_generation_call` images (data URI or URL).
    pub media: Vec<crate::media::MediaPart>,
}

impl ModelTurn {
    fn watchdog() -> Self {
        Self {
            content: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            raw_tool_calls: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            watchdog_hit: true,
            parse_fail: false,
            cached_tokens: None,
            decode_tok_s: None,
            media: Vec::new(),
        }
    }
}

pub trait Completer: Send + Sync {
    fn complete(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
    ) -> impl Future<Output = Result<ModelTurn>> + Send;

    /// When set, the loop meters the local Jinja prefix against the working window.
    fn prefix_meter(&self) -> Option<(Family, TemplateKwargs)> {
        None
    }

    fn set_policy(&self, _p: ThinkPolicy) {}

    fn policy(&self) -> Option<ThinkPolicy> {
        None
    }

    /// Live token sink. Default no-op (scripted tests).
    fn set_token_sink(&self, _sink: Option<TokenSink>) {}

    /// Pin llama.cpp KV slot for this session id (cross-turn prefix cache).
    fn pin_session(&self, _session_id: &str) {}

    /// Opaque xAI compaction item. Next Responses `input` is `[compaction]+new turns`.
    fn set_official_compaction(&self, _item: Option<crate::session::OfficialCompaction>) {}

    /// When a compaction blob is present, skip this many non-system messages
    /// (the local archive) so Responses `input` is `[blob] + new turns` only.
    fn set_compaction_skip(&self, _n: usize) {}

    /// Responses / hosted xAI: recast product identity after their prefix.
    fn recasts_xai_product(&self) -> bool {
        false
    }

    /// Lossy overlay sampling (repetition_penalty 1.1). Default no-op.
    fn set_low_precision(&self, _on: bool) {}

    fn media_caps(&self) -> crate::media::MediaCaps {
        crate::media::MediaCaps::default()
    }

    /// Stream-time slot for speculative read-only tools. Default no-op (tests).
    fn set_speculate(&self, _slot: Option<SpeculativeSlot>) {}

    fn speculate(&self) -> Option<SpeculativeSlot> {
        None
    }
}

#[derive(Clone, Debug)]
pub struct RunOpts {
    pub workspace: PathBuf,
    pub session_id: String,
    pub confined: bool,
    pub with_tools: bool,
    pub tool_set: ToolSet,
    pub max_steps: u32,
    pub max_wall: Duration,
    pub agents_md: bool,
    pub agents_md_max_tokens: u32,
    /// Clip AGENTS.md to `agents_md_max_tokens` instead of omitting it.
    pub agents_md_head: bool,
    pub print: bool,
    pub tool_limits: ToolLimits,
    pub bash_timeout_secs: f64,
    pub inherit_env: bool,
    pub working_window: u32,
    pub generation_reserve: u32,
    /// Fraction of `working_window` that starts compact (clamped 0.10..=1.0).
    pub compact_ratio: f64,
    /// Set when `HYPER_WORKING_WINDOW` replaced the file value. One-shot card.
    pub working_window_overlay: Option<WorkingWindowOverlay>,
    /// CLI `--think` / `--mode think` / `--fast` (and slash depth) lock auto effort.
    pub effort_locked: bool,
    pub persist_session: bool,
    pub session_mode: SessionMode,
    /// Override `~/.grok-hyper/sessions` (tests).
    pub session_dir: Option<PathBuf>,
    /// Override `~/.grok-hyper/blobs` (tests).
    pub blob_dir: Option<PathBuf>,
    /// Override `~/.grok-hyper` (tests). Isolates MEMORY.md / skills / memory.sqlite.
    pub home: Option<PathBuf>,
    /// Append memory_search / mcp (never splice the frozen four). `skill` stays
    /// out of tools[]; bodies are hidden-user notes. `mcp` is one extra blob,
    /// appended only when servers are mounted.
    pub peripheral: bool,
    pub skills_auto_catalog: bool,
    pub mcp_auto_catalog: bool,
    pub mcp: McpConfig,
    /// Append `web` (search/fetch) at session start. Builtin engines need no
    /// key; a Tavily key upgrades the backend transparently.
    pub web: crate::config::WebConfig,
    /// Append `view` in agent mode (not spliced into the frozen four).
    pub media: bool,
    /// Append `ComputerUse` in agent mode (Windows / macOS desktop control).
    pub computer_use: bool,
    pub media_max_bytes: usize,
    pub media_bins: MediaBins,
    /// `AGENT.md` filename (workspace then home).
    pub prompt_file: String,
    /// Unused: builtin identity is one agent contract. Kept so RunOpts stay stable.
    pub coding_identity: bool,
    /// Hidden plan card + mutating-tool deny. Does not change the frozen four.
    pub plan_mode: bool,
    /// Session `/clarify`. Combined with plan_mode, appends the `ask` tool.
    pub clarify_mode: bool,
    /// TUI permission bridge. None = YOLO (`--print`, tests).
    pub permit: Option<crate::permit::PermitHub>,
    /// Blocking ask overlay. None = error unless `--print` / YOLO / IM Skip.
    pub clarify: Option<crate::clarify::ClarifyHub>,
    /// Parent Config snapshot for child Task (no `apply_env` in the child).
    pub config: Config,
    /// Set on nested children. Gates tools and Task depth without TLS.
    pub child: Option<crate::subagent::ChildCtx>,
    /// User switch: tighter doom/parse/repeat guards. Default off.
    pub low_precision: bool,
    /// IM / console channel stamped on new JSONL `session/start`. Empty = default `cli`.
    pub channel: String,
    /// Interactive narration style card (TUI/web only; `--print` and IM stay silent).
    pub narrate: bool,
    /// When set, compact POSTs `/v1/responses/compact` before local archive.
    /// `(base_url, api_key)` — never log the key.
    pub xai_compact: Option<(String, String)>,
}

impl RunOpts {
    pub fn from_config(cfg: &Config, workspace: PathBuf) -> Self {
        Self {
            workspace,
            session_id: "print".into(),
            confined: cfg.features.workspace_write_only,
            with_tools: true,
            tool_set: ToolSet::Agent,
            max_steps: cfg.policy.max_steps,
            max_wall: Duration::from_secs(cfg.policy.max_wall_seconds),
            agents_md: true,
            agents_md_max_tokens: cfg.context.agents_md_max_tokens,
            agents_md_head: false,
            print: true,
            tool_limits: ToolLimits::from(&cfg.tools),
            bash_timeout_secs: cfg.code_mode.timeout_s as f64,
            inherit_env: cfg.code_mode.inherit_env,
            working_window: cfg.context.working_window,
            compact_ratio: if cfg.context.compact_ratio.is_finite() {
                cfg.context.compact_ratio
            } else {
                DEFAULT_COMPACT_RATIO
            },
            working_window_overlay: cfg.working_window_overlay,
            generation_reserve: clamp_generation_reserve(
                cfg.context.working_window,
                DEFAULT_GENERATION_RESERVE,
            ),
            effort_locked: false,
            persist_session: false,
            session_mode: SessionMode::Agent,
            session_dir: None,
            blob_dir: None,
            home: Config::home_dir().ok(),
            peripheral: true,
            skills_auto_catalog: cfg.features.skills_auto_catalog,
            mcp_auto_catalog: cfg.features.mcp_auto_catalog,
            mcp: cfg.mcp.clone(),
            web: cfg.web.clone(),
            media: cfg.media.enabled,
            computer_use: cfg.features.computer_use,
            media_max_bytes: cfg.media.max_bytes as usize,
            media_bins: MediaBins::from_config(&cfg.media),
            prompt_file: cfg.prompt.file.clone(),
            coding_identity: cfg.prompt.coding,
            plan_mode: false,
            clarify_mode: false,
            permit: None,
            clarify: None,
            low_precision: cfg.policy.low_precision,
            channel: String::new(),
            narrate: cfg.prompt.narrate,
            xai_compact: crate::transport::compact_creds(cfg),
            config: cfg.clone(),
            child: None,
        }
    }
}

/// Console-facing channels where a human watches progress live. IM bridges
/// deliver per-message and would surface narration as chat spam.
pub(crate) fn interactive_channel(channel: &str) -> bool {
    matches!(channel, "" | "cli" | "tui" | "web" | "console")
}

/// Hermes-shaped unattended caps: gateway `max_turns` 500, plus a wall so IM cannot stall forever.
pub fn apply_unattended_policy(opts: &mut RunOpts, cfg: &crate::config::Config) {
    if interactive_channel(&opts.channel) {
        return;
    }
    if cfg.policy.max_steps_unattended > 0 {
        opts.max_steps = cfg.policy.max_steps_unattended;
    }
    opts.max_wall = Duration::from_secs(cfg.policy.max_wall_unattended_seconds);
}

const DEFAULT_COMPACT_RATIO: f64 = 0.80;
const TURN_START_COMPACT_PREFIX: u32 = 120_000;
/// A finished tool-heavy turn should not be replayed as a cold prefill, even
/// when the cheap byte estimate sits under 120k (ComputerUse captions are short).
const TURN_START_COMPACT_TOOLS: usize = 8;
/// In-memory screenshots from the previous turn. Wire caps at 4; archive sooner.
const TURN_START_COMPACT_IMAGES: usize = 4;
/// Mid-turn ComputerUse: archive older closed groups so 60 screenshot hops
/// are not replayed as a cold prefill. Ordinary Read/Grep batches stay intact
/// (prefix-cache).
const MID_TURN_COMPACT_CU: usize = 16;
/// Compact-window headroom when the host has no generation cap.
pub(crate) const DEFAULT_GENERATION_RESERVE: u32 = 32_768;

pub(crate) fn clamp_generation_reserve(window: u32, reserve: u32) -> u32 {
    if window == 0 {
        return reserve;
    }
    let cap = (window / 4).max(64).min(window.saturating_sub(1));
    reserve.min(cap)
}

fn clamp_compact_ratio(ratio: f64) -> f64 {
    if ratio.is_finite() {
        ratio.clamp(0.10, 1.0)
    } else {
        DEFAULT_COMPACT_RATIO
    }
}

#[cfg(test)]
fn compact_soft_limit(working_window: u32, compact_ratio: f64) -> u32 {
    (working_window as f64 * clamp_compact_ratio(compact_ratio)) as u32
}

fn over_soft_threshold(prefix: u32, reserve: u32, working_window: u32, compact_ratio: f64) -> bool {
    working_window != 0
        && (prefix.saturating_add(reserve) as f64)
            > (working_window as f64) * clamp_compact_ratio(compact_ratio)
}

fn over_hard_threshold(prefix: u32, reserve: u32, working_window: u32) -> bool {
    working_window != 0 && prefix.saturating_add(reserve) > working_window
}

fn should_compact_at_user_turn(
    prefix: u32,
    reserve: u32,
    working_window: u32,
    compact_ratio: f64,
) -> bool {
    if working_window == 0 {
        return false;
    }
    over_soft_threshold(prefix, reserve, working_window, compact_ratio)
        || prefix > TURN_START_COMPACT_PREFIX
}

fn should_compact_follow_up(
    prefix: u32,
    reserve: u32,
    working_window: u32,
    compact_ratio: f64,
    tool_messages: usize,
    image_parts: usize,
) -> bool {
    if working_window == 0 {
        return false;
    }
    tool_messages >= TURN_START_COMPACT_TOOLS
        || image_parts > TURN_START_COMPACT_IMAGES
        || should_compact_at_user_turn(prefix, reserve, working_window, compact_ratio)
}

fn should_compact_mid_turn(cu_tools: usize, image_parts: usize) -> bool {
    image_parts > TURN_START_COMPACT_IMAGES || cu_tools >= MID_TURN_COMPACT_CU
}

#[derive(Clone, Debug)]
pub struct AgentOutcome {
    pub text: String,
    pub stop_reason: Option<String>,
    pub steps: u32,
    pub session_id: String,
    pub pending_steer: Vec<String>,
    /// CLI `print` already painted answer tokens to stdout.
    pub streamed_text: bool,
    /// Workspace-relative files this turn wrote that channels should send.
    pub channel_files: Vec<String>,
}

pub struct Agent<C> {
    completer: C,
    workspace: Workspace,
    handler: StopHandler,
    coordinator: ToolCoordinator,
    session_id: String,
    messages: Vec<ChatMessage>,
    tools: Vec<Value>,
    pending_stop: Option<String>,
    /// Runner proven usable by the turn-start baseline probe (`--print` only).
    oracle_cmd: Option<String>,
    oracle_runs: u32,
    print: bool,
    limits: ToolLimits,
    inherit_env: bool,
    working_window: u32,
    generation_reserve: u32,
    compact_ratio: f64,
    effort: EffortController,
    log: Option<SessionLog>,
    last_policy: ThinkPolicy,
    blobs: BlobStore,
    memory: Option<MemoryStore>,
    skills: SkillCatalog,
    mcp: McpRegistry,
    web: Option<crate::tools::WebRunner>,
    media_caps: MediaCaps,
    media_max_bytes: usize,
    media_bins: MediaBins,
    cancel: CancelFlag,
    steer: SteerSlot,
    emit: Option<crate::sidecar::EventSink>,
    stdio: std::sync::Arc<delta::StdioState>,
    plan_mode: bool,
    clarify_mode: bool,
    permit: Option<PermitHub>,
    clarify: Option<crate::clarify::ClarifyHub>,
    parse_stop_after: u32,
    /// Last substantial assistant content this user turn. Harness-only.
    last_spoken: Option<String>,
    /// Concatenated substantial tool results this user turn (capped).
    tool_evidence: String,
    /// Paths successfully `read` this user turn. Re-reads after an answer are not progress.
    read_paths: HashSet<String>,
    /// Paths whose content the live transcript has seen (read/view/write/edit).
    /// Rebuilt from the transcript each turn; cleared on compact.
    observed_paths: HashSet<String>,
    /// Consumed on the first `run_message` so a soak leftover env is visible once.
    window_overlay: Option<WorkingWindowOverlay>,
    /// S1/S2 judgment guards over successful edits.
    edit_guard: guard::EditGuard,
    /// Empty / cli / tui / web / console are interactive; IM skips AskQuestion.
    channel: String,
    /// Optional FTS for the `Search` tool. Built on the first user turn, not
    /// in `Agent::new` (Windows home-folder hang).
    code_index: Option<std::sync::Arc<CodeIndex>>,
    /// In-flight `CodeIndex::build` so hop-1 HTTP can overlap the 3s scan.
    index_build: Option<tokio::task::JoinHandle<std::sync::Arc<CodeIndex>>>,
    /// Prefetch handles for the in-flight model hop.
    speculate: Option<SpeculativeSlot>,
    /// Physics cap (steps / wall / context / tool budget) already got a wrap-up hop.
    physics_nudged: bool,
    /// Last official xAI compaction item (opaque). Next Responses `input` should
    /// be `[compaction] + new turns`. Never log `encrypted_content` in full.
    official_compaction: Option<crate::session::OfficialCompaction>,
    xai_compact: Option<(String, String)>,
    config: Config,
    child: Option<crate::subagent::ChildCtx>,
    persist_session: bool,
    session_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    channel_files: Vec<String>,
}
