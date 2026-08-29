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
use crate::tools_schema::dispatch_name;

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
        for call in calls {
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
                Some(idx) => run_search(idx, &call, self.limits),
                None => {
                    ToolResponse::text(&call.id, "Error: code index unavailable.", ToolState::Error)
                }
            },
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
}
