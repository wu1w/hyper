//! Nested in-process child. Completer is not Clone — the live runner (registered
//! from `Agent::new`) reconnects via `HttpCompleter::connect`. Never execs `grok`.
//!
//! `run_child` must not call `Agent` directly: that creates an async type cycle
//! (`dispatch_one` → Task → Agent::run → `dispatch_one`). Live execution is a
//! registered `dyn Fn`. Child depth / capability live on `RunOpts.child`, not TLS.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use crate::config::Config;
use crate::tool_calls::CancelFlag;

use super::policy::{extract_key_paths, has_plan_section, CapabilityMode, SubagentType};
use super::registry::ChildStatus;
use super::worktree::{Isolation, Worktree};

pub struct SpawnReq {
    pub id: String,
    pub parent_session: String,
    pub prompt: String,
    pub description: String,
    pub kind: SubagentType,
    pub capability: CapabilityMode,
    pub model: Option<String>,
    pub cwd: PathBuf,
    pub cancel: CancelFlag,
    pub isolation: Isolation,
    pub persist: bool,
    pub session_dir: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub config: Config,
    pub emit: Option<crate::sidecar::EventSink>,
    pub permit: Option<crate::permit::PermitHub>,
    pub clarify: Option<crate::clarify::ClarifyHub>,
    pub print: bool,
}

pub struct ChildOutcome {
    pub summary: String,
    pub key_paths: Vec<String>,
    pub status: ChildStatus,
    pub error: Option<String>,
}

pub type ChildFuture = Pin<Box<dyn Future<Output = ChildOutcome> + Send>>;
pub type LiveRunner = Arc<dyn Fn(SpawnReq) -> ChildFuture + Send + Sync>;

static LIVE: OnceLock<LiveRunner> = OnceLock::new();

/// Install the in-process Agent runner. Called from `Agent::new`.
pub fn register_live_runner(f: LiveRunner) {
    let _ = LIVE.set(f);
}

pub async fn run_child(req: SpawnReq) -> ChildOutcome {
    if req.cancel.is_cancelled() {
        return cancelled();
    }

    let mut keep = super::registry::running_ids();
    if !keep.iter().any(|id| id == &req.id) {
        keep.push(req.id.clone());
    }
    super::worktree::prune_stale(req.home.as_deref(), &keep);

    let mut created = false;
    let wt = if req.isolation.wants_worktree(req.kind) {
        match Worktree::add(&req.cwd, &req.id, req.home.as_deref()) {
            Ok((w, c)) => {
                created = c;
                Some(w)
            }
            Err(e) => {
                if req.isolation == Isolation::Worktree {
                    return ChildOutcome {
                        summary: String::new(),
                        key_paths: Vec::new(),
                        status: ChildStatus::Failed,
                        error: Some(e),
                    };
                }
                None
            }
        }
    } else {
        None
    };

    let mut req = req;
    if let Some(w) = &wt {
        req.cwd = w.path.clone();
    }

    if req.cancel.is_cancelled() {
        if created {
            if let Some(w) = wt {
                w.remove();
            }
        }
        return cancelled();
    }

    #[cfg(test)]
    let mut out = if !force_live_child() {
        stub_outcome(&req)
    } else {
        run_live_or_missing(req).await
    };
    #[cfg(not(test))]
    let mut out = run_live_or_missing(req).await;

    if let Some(w) = wt {
        super::worktree::mark_keep(&w.path);
        out.summary = with_worktree_notice(out.summary, &w.path);
    }
    out
}

async fn run_live_or_missing(req: SpawnReq) -> ChildOutcome {
    if let Some(f) = LIVE.get() {
        return f(req).await;
    }
    ChildOutcome {
        summary: String::new(),
        key_paths: Vec::new(),
        status: ChildStatus::Failed,
        error: Some(
            "subagent live runner not registered (parent Agent::new should call register_live_runner)"
                .into(),
        ),
    }
}

fn cancelled() -> ChildOutcome {
    ChildOutcome {
        summary: String::new(),
        key_paths: Vec::new(),
        status: ChildStatus::Cancelled,
        error: Some("cancelled".into()),
    }
}

#[cfg(test)]
fn force_live_child() -> bool {
    std::env::var("HYPER_SUBAGENT_LIVE")
        .map(|v| matches!(v.trim(), "1" | "true"))
        .unwrap_or(false)
}

#[cfg(test)]
fn stub_outcome(req: &SpawnReq) -> ChildOutcome {
    let mut summary = format!(
        "SUMMARY\n[{} / {}] {}\n\n{}",
        req.kind.as_str(),
        req.description,
        req.id,
        req.prompt.chars().take(800).collect::<String>()
    );
    if req.kind == SubagentType::Plan && !has_plan_section(&summary) {
        summary.push_str(
            "\n\n## Critical Files\n- (stub: none)\n\n## Plan\n- inspect, then write plan.md",
        );
    }
    let key_paths = extract_key_paths(&req.prompt);
    ChildOutcome {
        summary,
        key_paths,
        status: ChildStatus::Done,
        error: None,
    }
}

/// Task-scope prefix only. Children keep the same system prompt as the parent.
pub fn wrap_prompt(kind: SubagentType, prompt: &str) -> String {
    match kind {
        SubagentType::Explore => format!(
            "This Task is explore (read-only: no Write/StrReplace/Delete; \
             Shell only for inspection). Return a SUMMARY and key paths.\n\n{prompt}"
        ),
        SubagentType::Plan => format!(
            "This Task is plan (read-only except plan.md). End with \
             ## Critical Files and a structured ## Plan.\n\n{prompt}"
        ),
        SubagentType::Office | SubagentType::GeneralPurpose => prompt.to_string(),
    }
}

pub fn summarize_text(kind: SubagentType, text: &str) -> (String, Vec<String>) {
    let mut text = text.to_string();
    if kind == SubagentType::Plan && !has_plan_section(&text) {
        text.push_str("\n\n(note: child did not include Critical Files or a ## Plan section)");
    }
    let key_paths = extract_key_paths(&text);
    let mut summary = String::from("SUMMARY\n");
    summary.push_str(&clip(&text, 4000));
    if !key_paths.is_empty() {
        summary.push_str("\n\nKEY PATHS\n");
        for p in &key_paths {
            summary.push_str("- ");
            summary.push_str(p);
            summary.push('\n');
        }
    }
    (summary, key_paths)
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn with_worktree_notice(summary: String, path: &std::path::Path) -> String {
    let note = format!(
        "\n\nWORKTREE {}\nChild writes stay in this directory (HEAD checkout; not merged into the parent workspace).",
        path.display()
    );
    if summary.is_empty() {
        format!("SUMMARY{note}")
    } else {
        format!("{summary}{note}")
    }
}
