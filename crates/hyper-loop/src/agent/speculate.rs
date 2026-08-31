//! Speculative execution of read-only tools while the next call is still
//! streaming. Cursor starts call *i* once call *i+1* appears; Hyper does the
//! same for Read / Grep / Glob / Search / view.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::task::JoinHandle;

use crate::media::{MediaBins, MediaCaps};
use crate::subagent::ChildCtx;
use crate::tool_calls::{CancelFlag, ToolCall, ToolResponse, ToolState};
use crate::tools::{run_search, run_tool, view, BlobStore, CodeIndex, ToolLimits, Workspace};
use crate::tools_schema::{dispatch_name, is_parallel_safe};

const SPECULATE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct SpeculativeSlot {
    inner: Arc<Mutex<Inner>>,
    ctx: SpeculateCtx,
}

struct Inner {
    started: HashSet<String>,
    jobs: HashMap<String, JoinHandle<ToolResponse>>,
}

#[derive(Clone)]
pub struct SpeculateCtx {
    pub workspace: Workspace,
    pub limits: ToolLimits,
    pub inherit_env: bool,
    pub blobs: BlobStore,
    pub cancel: CancelFlag,
    pub code_index: Option<Arc<CodeIndex>>,
    pub media_caps: MediaCaps,
    pub media_bins: MediaBins,
    pub media_max_bytes: usize,
    pub child: Option<ChildCtx>,
    pub plan_mode: bool,
    /// User forbade Grep this turn — do not prefetch rg.
    pub skip_grep: bool,
    /// User forbade Glob this turn — do not prefetch directory listings.
    pub skip_glob: bool,
    /// Search queries already run this turn (paraphrase / cap skip).
    pub search_queries: Vec<String>,
    pub search_used: u32,
    /// Workspace-relative new files the user already named to Write.
    pub named_new: Vec<String>,
    /// Paths Search already dumped this turn; full-file Read prefetch is idle.
    pub search_located: Vec<String>,
    /// Snake/camel tokens already printed in this turn's Search dumps.
    pub search_shown_idents: Vec<String>,
    /// `view` is in tools[] (media.enabled). Hallucinated view is not prefetched.
    pub view_mounted: bool,
}

impl SpeculativeSlot {
    pub fn new(ctx: SpeculateCtx) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                started: HashSet::new(),
                jobs: HashMap::new(),
            })),
            ctx,
        }
    }

    pub fn offer(&self, calls: &[ToolCall]) {
        let mut behind_serial = false;
        for call in calls {
            if behind_serial {
                continue;
            }
            if !is_parallel_safe(&call.name) {
                // Mixed batches run serially. Prefetching a Read behind Shell
                // would mark it Started before the slow tool, and a steer
                // could not skip it.
                behind_serial = true;
                continue;
            }
            if !is_speculative(&call.name) || call.id.is_empty() {
                continue;
            }
            if crate::subagent::filter_tool(call, self.ctx.child.as_ref()).is_some() {
                continue;
            }
            if self.ctx.plan_mode && crate::permit::plan_mode_blocks(&call.name, &call.arguments) {
                continue;
            }
            if dispatch_name(&call.name) == "search" && self.ctx.code_index.is_none() {
                continue;
            }
            let name = dispatch_name(&call.name);
            if (name == "grep" && self.ctx.skip_grep) || (name == "glob" && self.ctx.skip_glob) {
                continue;
            }
            if name == "search" && skip_search_prefetch(&self.ctx, call) {
                continue;
            }
            if name == "view" && !self.ctx.view_mounted {
                continue;
            }
            if name == "glob" && skip_named_glob_prefetch(&self.ctx, call) {
                continue;
            }
            if name == "glob" && skip_tree_glob_prefetch(&self.ctx, call) {
                continue;
            }
            if name == "glob" && skip_glob_after_search_prefetch(&self.ctx, call) {
                continue;
            }
            if name == "grep" && skip_grep_after_search_prefetch(&self.ctx, call) {
                continue;
            }
            if name == "read" && skip_named_read_prefetch(&self.ctx, call) {
                continue;
            }
            if name == "read" && skip_search_span_read_prefetch(&self.ctx, call) {
                continue;
            }
            {
                let mut g = crate::lock_unpoison(&self.inner);
                if !g.started.insert(call.id.clone()) {
                    continue;
                }
            }
            let ctx = self.ctx.clone();
            let owned = call.clone();
            let id = call.id.clone();
            let handle = tokio::spawn(async move {
                match tokio::time::timeout(SPECULATE_TIMEOUT, ctx.dispatch(owned)).await {
                    Ok(r) => r,
                    Err(_) => ToolResponse::text(
                        &id,
                        "Error: speculative read timed out.",
                        ToolState::Interrupted,
                    ),
                }
            });
            crate::lock_unpoison(&self.inner)
                .jobs
                .insert(call.id.clone(), handle);
        }
    }

    pub async fn take(&self, id: &str) -> Option<ToolResponse> {
        let handle = crate::lock_unpoison(&self.inner).jobs.remove(id)?;
        handle.await.ok()
    }

    /// Tests: plant a finished prefetch the offer path would never start
    /// (e.g. Write) so `dispatch_with_prefetch` still has to `gate_tool`.
    #[cfg(test)]
    pub(super) fn inject_ready(&self, id: impl Into<String>, response: ToolResponse) {
        let id = id.into();
        crate::lock_unpoison(&self.inner).started.insert(id.clone());
        let handle = tokio::spawn(async move { response });
        crate::lock_unpoison(&self.inner).jobs.insert(id, handle);
    }

    /// Drop leftover prefetch jobs (watchdog retry, parse fail, unused ids).
    pub fn abort(&self) {
        let mut g = crate::lock_unpoison(&self.inner);
        for (_, h) in g.jobs.drain() {
            h.abort();
        }
        g.started.clear();
    }
}

impl SpeculateCtx {
    async fn dispatch(&self, call: ToolCall) -> ToolResponse {
        match dispatch_name(&call.name) {
            "search" => match &self.code_index {
                Some(idx) => run_search(idx, &self.workspace, &call, self.limits),
                None => {
                    ToolResponse::text(&call.id, crate::tools::SEARCH_WARMING, ToolState::Success)
                }
            },
            "view" if !self.view_mounted => ToolResponse::text(
                &call.id,
                format!("Error: unknown tool '{}'. Use Read for images.", call.name),
                ToolState::Error,
            ),
            "read" if super::dispatch::media_read_path(&call).is_some() => {
                view(
                    &self.workspace,
                    &call,
                    &self.media_caps,
                    &self.media_bins,
                    self.media_max_bytes,
                )
                .await
            }
            "view" => {
                view(
                    &self.workspace,
                    &call,
                    &self.media_caps,
                    &self.media_bins,
                    self.media_max_bytes,
                )
                .await
            }
            _ => {
                run_tool(
                    &self.workspace,
                    &call,
                    self.cancel.clone(),
                    self.limits,
                    self.inherit_env,
                    Some(&self.blobs),
                )
                .await
            }
        }
    }
}

fn skip_search_prefetch(ctx: &SpeculateCtx, call: &ToolCall) -> bool {
    if ctx.search_used >= super::dispatch::SEARCH_TURN_CAP {
        return true;
    }
    let query = crate::tools::arg_str(&call.arguments, "query").unwrap_or_default();
    if !query.trim().is_empty()
        && ctx
            .search_queries
            .iter()
            .any(|p| super::dispatch::is_search_paraphrase(p, &query))
    {
        return true;
    }
    if super::dispatch::query_code_idents(&query)
        .iter()
        .any(|id| ctx.search_shown_idents.iter().any(|s| s == id))
    {
        return true;
    }
    if named_search_query(&query, &ctx.named_new) {
        return true;
    }
    ctx.search_used > 0
        && super::dispatch::query_code_idents(&query).is_empty()
        && super::dispatch::looks_like_code_followup(&query)
}

fn named_search_query(query: &str, named: &[String]) -> bool {
    let q = query
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .replace('\\', "/");
    let q = q.trim();
    if q.is_empty() {
        return false;
    }
    named
        .iter()
        .any(|n| n == q || n.ends_with(&format!("/{q}")))
}

fn skip_named_glob_prefetch(ctx: &SpeculateCtx, call: &ToolCall) -> bool {
    if ctx.named_new.is_empty() {
        return false;
    }
    let pat = crate::tools::arg_str(&call.arguments, "glob_pattern").unwrap_or_default();
    let dir = crate::tools::arg_str(&call.arguments, "target_directory").unwrap_or_default();
    let joined = if dir.is_empty() {
        pat
    } else {
        format!("{}/{pat}", dir.trim_end_matches('/'))
    };
    let p = joined.replace('\\', "/");
    let stem = p
        .trim_end_matches('*')
        .trim_end_matches('/')
        .trim_end_matches('*')
        .trim_end_matches('/');
    if stem.is_empty() || stem == "." || stem == "**" {
        return false;
    }
    ctx.named_new.iter().any(|n| {
        n.rsplit_once('/')
            .is_some_and(|(parent, _)| parent == stem || parent.ends_with(&format!("/{stem}")))
    })
}

fn skip_tree_glob_prefetch(ctx: &SpeculateCtx, call: &ToolCall) -> bool {
    let pat = crate::tools::arg_str(&call.arguments, "glob_pattern").unwrap_or_default();
    let dir = crate::tools::arg_str(&call.arguments, "target_directory").unwrap_or_default();
    super::dispatch::recursive_any_file_glob(&pat)
        && super::dispatch::glob_target_is_workspace_root(&ctx.workspace, &dir)
}

fn skip_glob_after_search_prefetch(ctx: &SpeculateCtx, call: &ToolCall) -> bool {
    if ctx.search_located.is_empty() {
        return false;
    }
    let pat = crate::tools::arg_str(&call.arguments, "glob_pattern").unwrap_or_default();
    super::dispatch::glob_covered_by_search_paths(&pat, &ctx.search_located)
}

fn skip_grep_after_search_prefetch(ctx: &SpeculateCtx, call: &ToolCall) -> bool {
    if ctx.search_queries.is_empty() {
        return false;
    }
    let pattern = crate::tools::arg_str(&call.arguments, "pattern").unwrap_or_default();
    super::dispatch::grep_covered_by_search(&pattern, &ctx.search_queries)
}

fn skip_search_span_read_prefetch(ctx: &SpeculateCtx, call: &ToolCall) -> bool {
    if ctx.search_located.is_empty() || !super::dispatch::read_is_full(call) {
        return false;
    }
    let path = crate::tools::arg_str(&call.arguments, "path").unwrap_or_default();
    if path.is_empty() {
        return false;
    }
    let key = super::dispatch::canon_ws_path(&ctx.workspace, &path);
    ctx.search_located.iter().any(|p| p == &key)
}

fn skip_named_read_prefetch(ctx: &SpeculateCtx, call: &ToolCall) -> bool {
    if ctx.named_new.is_empty() {
        return false;
    }
    let path = crate::tools::arg_str(&call.arguments, "path").unwrap_or_default();
    let p = path.replace('\\', "/").trim_end_matches('/').to_string();
    if p.is_empty() {
        return false;
    }
    ctx.named_new.iter().any(|n| {
        if n == &p || n.ends_with(&format!("/{p}")) || p.ends_with(&format!("/{n}")) {
            return true;
        }
        let np = n.rsplit_once('/').map(|(a, _)| a);
        np == Some(p.as_str())
            || p.rsplit_once('/')
                .is_some_and(|(rp, _)| np == Some(rp) || n.ends_with(&format!("/{rp}")))
    })
}

pub fn is_speculative(name: &str) -> bool {
    matches!(
        dispatch_name(name),
        "read" | "grep" | "glob" | "search" | "view"
    )
}

/// OpenAI `tool_calls` slots that are complete enough to run.
///
/// Index *n* seals when index *n+1* exists, or when the stream has ended.
/// Incomplete JSON arguments stay pending.
pub fn openai_ready_calls(raw: &[Value], stream_ended: bool) -> Vec<ToolCall> {
    let n = raw.len();
    let mut out = Vec::new();
    for (i, v) in raw.iter().enumerate() {
        if !(stream_ended || i + 1 < n) {
            continue;
        }
        if let Some(call) = parse_ready_openai(v) {
            out.push(call);
        }
    }
    out
}

fn parse_ready_openai(v: &Value) -> Option<ToolCall> {
    let id = v["id"].as_str().filter(|s| !s.is_empty())?.to_string();
    let name = v["function"]["name"].as_str()?.to_string();
    if name.is_empty() {
        return None;
    }
    let arguments = match &v["function"]["arguments"] {
        Value::String(s) => {
            let parsed: Value = serde_json::from_str(s).ok()?;
            if !parsed.is_object() && !parsed.is_array() {
                return None;
            }
            parsed
        }
        Value::Object(_) | Value::Array(_) => v["function"]["arguments"].clone(),
        _ => return None,
    };
    Some(ToolCall {
        id,
        name,
        arguments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn slot(name: &str, args: &str) -> Value {
        json!({
            "id": format!("call_{name}"),
            "type": "function",
            "function": { "name": name, "arguments": args }
        })
    }

    #[test]
    fn next_index_seals_previous_call() {
        let raw = vec![
            slot("Read", r#"{"path":"a.rs"}"#),
            slot("Grep", r#"{"pattern":"foo"#),
        ];
        let ready = openai_ready_calls(&raw, false);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].name, "Read");
        assert_eq!(ready[0].arguments["path"], "a.rs");
    }

    #[test]
    fn incomplete_json_stays_pending() {
        let raw = vec![slot("Read", r#"{"path":"a.r"#)];
        assert!(openai_ready_calls(&raw, false).is_empty());
        assert!(openai_ready_calls(&raw, true).is_empty());
    }

    #[test]
    fn stream_end_seals_complete_json() {
        let raw = vec![slot("Glob", r#"{"glob_pattern":"**/*.rs"}"#)];
        let ready = openai_ready_calls(&raw, true);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].name, "Glob");
    }

    #[test]
    fn missing_id_is_not_ready() {
        let raw = vec![json!({
            "type": "function",
            "function": { "name": "Read", "arguments": "{\"path\":\"a.rs\"}" }
        })];
        assert!(openai_ready_calls(&raw, true).is_empty());
    }

    #[test]
    fn writes_are_not_speculative() {
        assert!(is_speculative("Read"));
        assert!(is_speculative("Grep"));
        assert!(is_speculative("Search"));
        assert!(is_speculative("view"));
        assert!(!is_speculative("Write"));
        assert!(!is_speculative("StrReplace"));
        assert!(!is_speculative("Shell"));
        assert!(!is_speculative("WebSearch"));
        assert!(!is_speculative("Task"));
    }

    #[tokio::test]
    async fn offer_then_take_runs_read() {
        let dir =
            std::env::temp_dir().join(format!("hyper-spec-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "hello speculative\n").unwrap();
        let ws = Workspace::open(&dir, true).unwrap();
        let slot = SpeculativeSlot::new(SpeculateCtx {
            workspace: ws,
            limits: ToolLimits::default(),
            inherit_env: false,
            blobs: BlobStore::new(dir.join("blobs")),
            cancel: CancelFlag::new(),
            code_index: None,
            media_caps: MediaCaps::default(),
            media_bins: MediaBins::none(),
            media_max_bytes: 1024,
            child: None,
            plan_mode: false,
            skip_grep: false,
            skip_glob: false,
            search_queries: Vec::new(),
            search_used: 0,
            named_new: Vec::new(),
            search_located: Vec::new(),
            search_shown_idents: Vec::new(),
            view_mounted: false,
        });
        slot.offer(&[ToolCall {
            id: "call_1".into(),
            name: "Read".into(),
            arguments: json!({"path": "note.txt"}),
        }]);
        let got = slot.take("call_1").await.unwrap();
        assert_eq!(got.state, ToolState::Success);
        assert!(got.joined_text().contains("hello speculative"), "{got:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn skip_grep_does_not_prefetch() {
        let dir =
            std::env::temp_dir().join(format!("hyper-spec-skip-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "secret-token\n").unwrap();
        let ws = Workspace::open(&dir, true).unwrap();
        let slot = SpeculativeSlot::new(SpeculateCtx {
            workspace: ws,
            limits: ToolLimits::default(),
            inherit_env: false,
            blobs: BlobStore::new(dir.join("blobs")),
            cancel: CancelFlag::new(),
            code_index: None,
            media_caps: MediaCaps::default(),
            media_bins: MediaBins::none(),
            media_max_bytes: 1024,
            child: None,
            plan_mode: false,
            skip_grep: true,
            skip_glob: false,
            search_queries: Vec::new(),
            search_used: 0,
            named_new: Vec::new(),
            search_located: Vec::new(),
            search_shown_idents: Vec::new(),
            view_mounted: false,
        });
        slot.offer(&[ToolCall {
            id: "call_g".into(),
            name: "Grep".into(),
            arguments: json!({"pattern": "secret-token"}),
        }]);
        assert!(
            slot.take("call_g").await.is_none(),
            "forbidden Grep must not prefetch"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn skip_search_paraphrase_does_not_prefetch() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-spec-search-skip-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::open(&dir, true).unwrap();
        let slot = SpeculativeSlot::new(SpeculateCtx {
            workspace: ws,
            limits: ToolLimits::default(),
            inherit_env: false,
            blobs: BlobStore::new(dir.join("blobs")),
            cancel: CancelFlag::new(),
            code_index: None,
            media_caps: MediaCaps::default(),
            media_bins: MediaBins::none(),
            media_max_bytes: 1024,
            child: None,
            plan_mode: false,
            skip_grep: false,
            skip_glob: false,
            search_queries: vec!["forbids_glob".into()],
            search_used: 1,
            named_new: Vec::new(),
            search_located: Vec::new(),
            search_shown_idents: Vec::new(),
            view_mounted: false,
        });
        slot.offer(&[ToolCall {
            id: "call_s".into(),
            name: "Search".into(),
            arguments: json!({"query": "forbids_glob function"}),
        }]);
        assert!(
            slot.take("call_s").await.is_none(),
            "paraphrase Search must not prefetch"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn skip_search_code_followup_does_not_prefetch() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-spec-search-follow-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ctx = spec_ctx(&dir, Vec::new());
        ctx.search_used = 1;
        let slot = SpeculativeSlot::new(ctx);
        slot.offer(&[ToolCall {
            id: "call_s".into(),
            name: "Search".into(),
            arguments: json!({"query": "fn take prefetch glob speculate"}),
        }]);
        assert!(
            slot.take("call_s").await.is_none(),
            "code-followup Search must not prefetch after a hit"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn spec_ctx(dir: &std::path::Path, named_new: Vec<String>) -> SpeculateCtx {
        let ws = Workspace::open(dir, true).unwrap();
        SpeculateCtx {
            workspace: ws,
            limits: ToolLimits::default(),
            inherit_env: false,
            blobs: BlobStore::new(dir.join("blobs")),
            cancel: CancelFlag::new(),
            code_index: None,
            media_caps: MediaCaps::default(),
            media_bins: MediaBins::none(),
            media_max_bytes: 1024,
            child: None,
            plan_mode: false,
            skip_grep: false,
            skip_glob: false,
            search_queries: Vec::new(),
            search_used: 0,
            named_new,
            search_located: Vec::new(),
            search_shown_idents: Vec::new(),
            view_mounted: false,
        }
    }

    #[tokio::test]
    async fn skip_named_glob_does_not_prefetch() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-spec-named-glob-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(dir.join("overnight")).unwrap();
        std::fs::write(dir.join("overnight/old.py"), "print('OLD')\n").unwrap();
        let slot = SpeculativeSlot::new(spec_ctx(&dir, vec!["overnight/new.py".into()]));
        slot.offer(&[ToolCall {
            id: "call_g".into(),
            name: "Glob".into(),
            arguments: json!({"glob_pattern": "overnight/*"}),
        }]);
        assert!(
            slot.take("call_g").await.is_none(),
            "parent Glob of a named new file must not prefetch"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn skip_named_read_does_not_prefetch() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-spec-named-read-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(dir.join("overnight")).unwrap();
        std::fs::write(dir.join("overnight/old.py"), "print('OLD')\n").unwrap();
        let slot = SpeculativeSlot::new(spec_ctx(&dir, vec!["overnight/new.py".into()]));
        slot.offer(&[
            ToolCall {
                id: "call_self".into(),
                name: "Read".into(),
                arguments: json!({"path": "overnight/new.py"}),
            },
            ToolCall {
                id: "call_sib".into(),
                name: "Read".into(),
                arguments: json!({"path": "overnight/old.py"}),
            },
        ]);
        assert!(
            slot.take("call_self").await.is_none(),
            "Read of the named new file must not prefetch"
        );
        assert!(
            slot.take("call_sib").await.is_none(),
            "sibling Read of a named new Write must not prefetch"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn skip_search_span_read_does_not_prefetch() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-spec-search-span-read-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(dir.join("overnight")).unwrap();
        std::fs::write(dir.join("overnight/old.py"), "print('OLD')\n").unwrap();
        std::fs::write(dir.join("overnight/other.py"), "print('OTHER')\n").unwrap();
        let mut ctx = spec_ctx(&dir, Vec::new());
        let key = super::super::dispatch::canon_ws_path(&ctx.workspace, "overnight/old.py");
        ctx.search_located = vec![key];
        let slot = SpeculativeSlot::new(ctx);
        slot.offer(&[
            ToolCall {
                id: "call_hit".into(),
                name: "Read".into(),
                arguments: json!({"path": "overnight/old.py"}),
            },
            ToolCall {
                id: "call_other".into(),
                name: "Read".into(),
                arguments: json!({"path": "overnight/other.py"}),
            },
            ToolCall {
                id: "call_page".into(),
                name: "Read".into(),
                arguments: json!({"path": "overnight/old.py", "offset": 1, "limit": 1}),
            },
        ]);
        assert!(
            slot.take("call_hit").await.is_none(),
            "full Read of a Search-located path must not prefetch"
        );
        let other = slot.take("call_other").await.unwrap();
        assert_eq!(other.state, ToolState::Success);
        assert!(
            other.joined_text().contains("OTHER"),
            "unrelated Read must still prefetch: {other:?}"
        );
        let page = slot.take("call_page").await.unwrap();
        assert_eq!(page.state, ToolState::Success);
        assert!(
            page.joined_text().contains("OLD"),
            "offset Read of a Search hit must still prefetch: {page:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn unrelated_glob_still_prefetches() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-spec-other-glob-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(dir.join("other")).unwrap();
        std::fs::write(dir.join("other/a.rs"), "fn ping() {}\n").unwrap();
        let slot = SpeculativeSlot::new(spec_ctx(&dir, vec!["overnight/new.py".into()]));
        slot.offer(&[ToolCall {
            id: "call_g".into(),
            name: "Glob".into(),
            arguments: json!({"glob_pattern": "other/*"}),
        }]);
        let got = slot.take("call_g").await.unwrap();
        assert_eq!(got.state, ToolState::Success);
        assert!(
            got.joined_text().contains("a.rs"),
            "unrelated Glob must still prefetch: {got:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn skip_tree_glob_does_not_prefetch() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-spec-tree-glob-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(dir.join("other")).unwrap();
        std::fs::write(dir.join("other/a.rs"), "fn ping() {}\n").unwrap();
        let slot = SpeculativeSlot::new(spec_ctx(&dir, Vec::new()));
        slot.offer(&[
            ToolCall {
                id: "call_tree".into(),
                name: "Glob".into(),
                arguments: json!({"glob_pattern": "**/*"}),
            },
            ToolCall {
                id: "call_rs".into(),
                name: "Glob".into(),
                arguments: json!({"glob_pattern": "**/*.rs"}),
            },
            ToolCall {
                id: "call_sub".into(),
                name: "Glob".into(),
                arguments: json!({"glob_pattern": "other/*"}),
            },
        ]);
        assert!(
            slot.take("call_tree").await.is_none(),
            "workspace-root Glob **/* must not prefetch"
        );
        let rs = slot.take("call_rs").await.unwrap();
        assert_eq!(rs.state, ToolState::Success);
        assert!(
            rs.joined_text().contains("a.rs"),
            "workspace-root Glob **/*.rs must still prefetch: {rs:?}"
        );
        let sub = slot.take("call_sub").await.unwrap();
        assert_eq!(sub.state, ToolState::Success);
        assert!(
            sub.joined_text().contains("a.rs"),
            "subdirectory Glob must still prefetch: {sub:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn skip_grep_after_search_does_not_prefetch() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-spec-grep-after-search-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("lib.rs"),
            "fn quiet_helper() {}\nfn unique_zzz_symbol() {}\n",
        )
        .unwrap();
        let mut ctx = spec_ctx(&dir, Vec::new());
        ctx.search_queries = vec!["quiet_helper".into()];
        let slot = SpeculativeSlot::new(ctx);
        slot.offer(&[
            ToolCall {
                id: "call_idle".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "quiet_helper"}),
            },
            ToolCall {
                id: "call_fresh".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "unique_zzz_symbol"}),
            },
        ]);
        assert!(
            slot.take("call_idle").await.is_none(),
            "Grep of a Search query must not prefetch"
        );
        let fresh = slot.take("call_fresh").await.unwrap();
        assert_eq!(fresh.state, ToolState::Success);
        assert!(
            fresh.joined_text().contains("unique_zzz_symbol")
                || fresh.joined_text().contains("lib.rs"),
            "distinct Grep must still prefetch: {fresh:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn skip_glob_after_search_does_not_prefetch() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-spec-glob-after-search-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "fn ping() {}\n").unwrap();
        std::fs::write(dir.join("src/other.rs"), "fn other() {}\n").unwrap();
        let mut ctx = spec_ctx(&dir, Vec::new());
        ctx.search_located = vec!["src/lib.rs".into()];
        let slot = SpeculativeSlot::new(ctx);
        slot.offer(&[
            ToolCall {
                id: "call_idle".into(),
                name: "Glob".into(),
                arguments: json!({"glob_pattern": "**/lib.rs"}),
            },
            ToolCall {
                id: "call_fresh".into(),
                name: "Glob".into(),
                arguments: json!({"glob_pattern": "**/other.rs"}),
            },
        ]);
        assert!(
            slot.take("call_idle").await.is_none(),
            "Glob of a Search-located file name must not prefetch"
        );
        let fresh = slot.take("call_fresh").await.unwrap();
        assert_eq!(fresh.state, ToolState::Success);
        assert!(
            fresh.joined_text().contains("other.rs"),
            "distinct Glob must still prefetch: {fresh:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn skip_search_ident_in_dump_does_not_prefetch() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-spec-search-ident-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ctx = spec_ctx(&dir, Vec::new());
        ctx.search_shown_idents = vec!["bash_cat_gate".into()];
        let slot = SpeculativeSlot::new(ctx);
        slot.offer(&[ToolCall {
            id: "call_idle".into(),
            name: "Search".into(),
            arguments: json!({"query": "bash_cat_gate"}),
        }]);
        assert!(
            slot.take("call_idle").await.is_none(),
            "Search of a token already in a dump must not prefetch"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn mixed_serial_batch_does_not_prefetch_trailing_read() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-spec-serial-read-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "hello\n").unwrap();
        let slot = SpeculativeSlot::new(spec_ctx(&dir, Vec::new()));
        slot.offer(&[
            ToolCall {
                id: "slow".into(),
                name: "Shell".into(),
                arguments: json!({"command": "true"}),
            },
            ToolCall {
                id: "later-read".into(),
                name: "Read".into(),
                arguments: json!({"path": "note.txt"}),
            },
        ]);
        assert!(
            slot.take("later-read").await.is_none(),
            "Read behind Shell must wait for the serial boundary"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
