//! In-process child registry. Max 8 concurrent. No `grok` binary.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::lock_unpoison;
use crate::tool_calls::CancelFlag;

use super::policy::{CapabilityMode, SubagentType};
use super::worktree::Isolation;

pub const MAX_CONCURRENT: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl ChildStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChildRecord {
    pub id: String,
    pub parent_session: String,
    pub description: String,
    pub kind: SubagentType,
    pub capability: CapabilityMode,
    pub isolation: Isolation,
    pub status: ChildStatus,
    pub summary: String,
    pub key_paths: Vec<String>,
    pub error: Option<String>,
    pub started: Instant,
}

#[derive(Clone)]
pub struct ChildHandle {
    pub id: String,
    pub cancel: CancelFlag,
    pub notify: Arc<Notify>,
}

struct Slot {
    rec: ChildRecord,
    cancel: CancelFlag,
    notify: Arc<Notify>,
}

struct Registry {
    slots: HashMap<String, Slot>,
}

fn registry() -> &'static Mutex<Registry> {
    static REG: OnceLock<Mutex<Registry>> = OnceLock::new();
    REG.get_or_init(|| {
        Mutex::new(Registry {
            slots: HashMap::new(),
        })
    })
}

pub fn running_count() -> usize {
    running_ids().len()
}

pub fn running_ids() -> Vec<String> {
    lock_unpoison(registry())
        .slots
        .values()
        .filter(|s| s.rec.status == ChildStatus::Running)
        .map(|s| s.rec.id.clone())
        .collect()
}

pub fn insert_running(
    id: String,
    parent_session: String,
    description: String,
    kind: SubagentType,
    capability: CapabilityMode,
    isolation: Isolation,
) -> Result<ChildHandle, String> {
    let mut g = lock_unpoison(registry());
    if let Some(slot) = g.slots.get(&id) {
        if slot.rec.status == ChildStatus::Running {
            return Err(format!("Error: subagent `{id}` is still running."));
        }
    }
    let n = g
        .slots
        .values()
        .filter(|s| s.rec.status == ChildStatus::Running)
        .count();
    if n >= MAX_CONCURRENT {
        return Err(format!(
            "Error: subagent concurrency limit ({MAX_CONCURRENT}) reached."
        ));
    }
    let cancel = CancelFlag::new();
    let notify = Arc::new(Notify::new());
    g.slots.insert(
        id.clone(),
        Slot {
            rec: ChildRecord {
                id: id.clone(),
                parent_session,
                description,
                kind,
                capability,
                isolation,
                status: ChildStatus::Running,
                summary: String::new(),
                key_paths: Vec::new(),
                error: None,
                started: Instant::now(),
            },
            cancel: cancel.clone(),
            notify: notify.clone(),
        },
    );
    Ok(ChildHandle { id, cancel, notify })
}

pub fn finish(
    id: &str,
    status: ChildStatus,
    summary: String,
    key_paths: Vec<String>,
    error: Option<String>,
) {
    let mut g = lock_unpoison(registry());
    if let Some(slot) = g.slots.get_mut(id) {
        slot.rec.status = status;
        slot.rec.summary = summary;
        slot.rec.key_paths = key_paths;
        slot.rec.error = error;
        slot.notify.notify_waiters();
    }
}

pub fn get(id: &str) -> Option<ChildRecord> {
    lock_unpoison(registry())
        .slots
        .get(id)
        .map(|s| s.rec.clone())
}

pub fn kill(id: &str) -> Option<ChildRecord> {
    let mut g = lock_unpoison(registry());
    let slot = g.slots.get_mut(id)?;
    slot.cancel.cancel();
    if slot.rec.status == ChildStatus::Running {
        slot.rec.status = ChildStatus::Cancelled;
        slot.rec.error = Some("killed".into());
        slot.notify.notify_waiters();
    }
    Some(slot.rec.clone())
}

pub fn list_for_parent(parent: &str) -> Vec<ChildRecord> {
    lock_unpoison(registry())
        .slots
        .values()
        .filter(|s| parent.is_empty() || s.rec.parent_session == parent)
        .map(|s| s.rec.clone())
        .collect()
}

pub fn snapshot_json(parent: &str) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = list_for_parent(parent)
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "description": r.description,
                "type": r.kind.as_str(),
                "isolation": r.isolation.as_str(),
                "status": r.status.as_str(),
                "summary": r.summary,
                "key_paths": r.key_paths,
            })
        })
        .collect();
    serde_json::json!(rows)
}

pub async fn wait(id: &str, timeout: Option<Duration>) -> Option<ChildRecord> {
    let deadline = timeout.map(|d| tokio::time::Instant::now() + d);
    loop {
        let notify = {
            let g = lock_unpoison(registry());
            let slot = g.slots.get(id)?;
            if slot.rec.status != ChildStatus::Running {
                return Some(slot.rec.clone());
            }
            slot.notify.clone()
        };
        // Subscribe before the second status check so a completion between
        // the checks cannot be missed.
        let notified = notify.notified();
        {
            let g = lock_unpoison(registry());
            let slot = g.slots.get(id)?;
            if slot.rec.status != ChildStatus::Running {
                return Some(slot.rec.clone());
            }
        }
        match deadline {
            Some(d) => {
                let left = d.saturating_duration_since(tokio::time::Instant::now());
                if left.is_zero() {
                    return get(id);
                }
                if tokio::time::timeout(left, notified).await.is_err() {
                    return get(id);
                }
            }
            None => notified.await,
        }
    }
}

pub async fn wait_many(ids: &[String], timeout: Option<Duration>) -> Vec<ChildRecord> {
    let deadline = timeout.map(|d| Instant::now() + d);
    let mut out = Vec::new();
    for id in ids {
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if let Some(rec) = wait(id, left).await {
            out.push(rec);
        }
    }
    out
}

#[cfg(test)]
pub fn clear() {
    lock_unpoison(registry()).slots.clear();
}
