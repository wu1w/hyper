//! In-process child registry. Max 8 concurrent. No `grok` binary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

    fn parse(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Done,
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
    persist_dir: Option<PathBuf>,
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
    persist_dir: Option<PathBuf>,
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
    let rec = ChildRecord {
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
    };
    if let Some(dir) = &persist_dir {
        write_disk(dir, &rec);
    }
    g.slots.insert(
        id.clone(),
        Slot {
            rec,
            cancel: cancel.clone(),
            notify: notify.clone(),
            persist_dir,
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
        if let Some(dir) = &slot.persist_dir {
            write_disk(dir, &slot.rec);
        }
        slot.notify.notify_waiters();
    }
}

pub fn get(id: &str) -> Option<ChildRecord> {
    lock_unpoison(registry())
        .slots
        .get(id)
        .map(|s| s.rec.clone())
}

/// Memory first, then `{dir}/{id}.task.json`. A disk row still marked running
/// is an orphan (Cursor: interrupted agent) and is reaped to failed.
pub fn get_or_load(id: &str, dir: Option<&Path>) -> Option<ChildRecord> {
    if let Some(rec) = get(id) {
        return Some(rec);
    }
    load_and_hydrate(dir?, id)
}

pub fn kill(id: &str) -> Option<ChildRecord> {
    let mut g = lock_unpoison(registry());
    let slot = g.slots.get_mut(id)?;
    slot.cancel.cancel();
    if slot.rec.status == ChildStatus::Running {
        slot.rec.status = ChildStatus::Cancelled;
        slot.rec.error = Some("killed".into());
        if let Some(dir) = &slot.persist_dir {
            write_disk(dir, &slot.rec);
        }
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

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedChild {
    id: String,
    parent_session: String,
    description: String,
    kind: String,
    capability: String,
    isolation: String,
    status: String,
    summary: String,
    key_paths: Vec<String>,
    error: Option<String>,
}

fn task_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.task.json"))
}

fn write_disk(dir: &Path, rec: &ChildRecord) {
    let _ = std::fs::create_dir_all(dir);
    let row = PersistedChild {
        id: rec.id.clone(),
        parent_session: rec.parent_session.clone(),
        description: rec.description.clone(),
        kind: rec.kind.as_str().into(),
        capability: rec.capability.as_str().into(),
        isolation: rec.isolation.as_str().into(),
        status: rec.status.as_str().into(),
        summary: rec.summary.clone(),
        key_paths: rec.key_paths.clone(),
        error: rec.error.clone(),
    };
    if let Ok(bytes) = serde_json::to_vec_pretty(&row) {
        let path = task_path(dir, &rec.id);
        let _ = std::fs::write(path, bytes);
    }
}

fn read_disk(dir: &Path, id: &str) -> Option<ChildRecord> {
    let bytes = std::fs::read(task_path(dir, id)).ok()?;
    let row: PersistedChild = serde_json::from_slice(&bytes).ok()?;
    Some(ChildRecord {
        id: row.id,
        parent_session: row.parent_session,
        description: row.description,
        kind: SubagentType::parse(&row.kind),
        capability: CapabilityMode::parse(&row.capability)
            .unwrap_or_else(|| CapabilityMode::from_kind(SubagentType::parse(&row.kind))),
        isolation: Isolation::parse(&row.isolation).unwrap_or(Isolation::Auto),
        status: ChildStatus::parse(&row.status),
        summary: row.summary,
        key_paths: row.key_paths,
        error: row.error,
        started: Instant::now(),
    })
}

fn hydrate(rec: ChildRecord, persist_dir: Option<PathBuf>) {
    let mut g = lock_unpoison(registry());
    if g.slots.contains_key(&rec.id) {
        return;
    }
    g.slots.insert(
        rec.id.clone(),
        Slot {
            rec,
            cancel: CancelFlag::new(),
            notify: Arc::new(Notify::new()),
            persist_dir,
        },
    );
}

fn load_and_hydrate(dir: &Path, id: &str) -> Option<ChildRecord> {
    let mut rec = read_disk(dir, id)?;
    if rec.status == ChildStatus::Running {
        rec.status = ChildStatus::Failed;
        rec.error = Some("interrupted: process restarted".into());
        write_disk(dir, &rec);
    }
    hydrate(rec.clone(), Some(dir.to_path_buf()));
    Some(rec)
}

/// Cursor-style: child ids survive process death. Running rows become failed
/// orphans; `Task resume` continues from the child JSONL.
pub fn reap_orphans(dir: &Path) {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        let Some(id) = name.strip_suffix(".task.json") else {
            continue;
        };
        let _ = load_and_hydrate(dir, id);
    }
}

#[cfg(test)]
pub fn clear() {
    lock_unpoison(registry()).slots.clear();
}
