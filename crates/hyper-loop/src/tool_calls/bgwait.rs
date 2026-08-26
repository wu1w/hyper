//! Global wait map for coordinator-offloaded tool calls (background Shell).
//!
//! AwaitShell looks here after the subagent registry. Drain (`take_finished`)
//! skips ids already consumed by AwaitShell so the result is not double-posted.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;

use crate::lock_unpoison;

use super::types::ToolResponse;

struct Slot {
    name: String,
    response: Option<ToolResponse>,
    taken: bool,
    notify: Arc<Notify>,
}

struct Map {
    slots: HashMap<String, Slot>,
}

fn map() -> &'static Mutex<Map> {
    static MAP: OnceLock<Mutex<Map>> = OnceLock::new();
    MAP.get_or_init(|| {
        Mutex::new(Map {
            slots: HashMap::new(),
        })
    })
}

/// Register an offloaded call so AwaitShell can find it while it still runs.
pub fn register(id: impl Into<String>, name: impl Into<String>) {
    let id = id.into();
    let name = name.into();
    let mut g = lock_unpoison(map());
    g.slots.entry(id).or_insert_with(|| Slot {
        name,
        response: None,
        taken: false,
        notify: Arc::new(Notify::new()),
    });
}

/// Store the completed response and wake waiters.
pub fn finish(id: &str, response: ToolResponse) {
    let mut g = lock_unpoison(map());
    match g.slots.get_mut(id) {
        Some(slot) => {
            slot.response = Some(response);
            slot.notify.notify_waiters();
        }
        None => {
            g.slots.insert(
                id.to_string(),
                Slot {
                    name: String::new(),
                    response: Some(response),
                    taken: false,
                    notify: Arc::new(Notify::new()),
                },
            );
        }
    }
}

pub fn exists(id: &str) -> bool {
    lock_unpoison(map()).slots.contains_key(id)
}

pub fn is_taken(id: &str) -> bool {
    lock_unpoison(map()).slots.get(id).is_some_and(|s| s.taken)
}

/// Wait until `id` finishes, or until `timeout`. Marks a finished response
/// consumed so [`take_untaken`] / coordinator drain will skip it.
///
/// `None` means missing, still running when the timeout fired, or never registered.
pub async fn wait(id: &str, timeout: Option<Duration>) -> Option<ToolResponse> {
    let deadline = timeout.map(|d| tokio::time::Instant::now() + d);
    loop {
        let notify = {
            let mut g = lock_unpoison(map());
            let slot = g.slots.get_mut(id)?;
            if let Some(resp) = slot.response.clone() {
                slot.taken = true;
                return Some(resp);
            }
            slot.notify.clone()
        };
        let notified = notify.notified();
        {
            let mut g = lock_unpoison(map());
            let slot = g.slots.get_mut(id)?;
            if let Some(resp) = slot.response.clone() {
                slot.taken = true;
                return Some(resp);
            }
        }
        match deadline {
            Some(d) => {
                let left = d.saturating_duration_since(tokio::time::Instant::now());
                if left.is_zero() {
                    return None;
                }
                if tokio::time::timeout(left, notified).await.is_err() {
                    return None;
                }
            }
            None => notified.await,
        }
    }
}

/// Finished responses not consumed by AwaitShell. Drain marks them taken.
pub fn take_untaken() -> Vec<(String, ToolResponse)> {
    let mut g = lock_unpoison(map());
    let mut out = Vec::new();
    for slot in g.slots.values_mut() {
        if slot.taken {
            continue;
        }
        if let Some(resp) = slot.response.clone() {
            slot.taken = true;
            let name = if slot.name.is_empty() {
                resp.id.clone()
            } else {
                slot.name.clone()
            };
            out.push((name, resp));
        }
    }
    out
}

#[cfg(test)]
pub fn clear() {
    lock_unpoison(map()).slots.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_calls::{ToolCall, ToolCoordinator, ToolState};

    fn shell_bg(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "Shell".into(),
            arguments: serde_json::json!({"background": true}),
        }
    }

    #[tokio::test]
    async fn wait_receives_finish() {
        clear();
        register("w1", "bash");
        let waiter = wait("w1", None);
        tokio::pin!(waiter);
        tokio::task::yield_now().await;
        finish("w1", ToolResponse::text("w1", "done", ToolState::Success));
        let got = waiter.await.expect("finished");
        assert_eq!(got.joined_text(), "done");
        assert!(take_untaken().is_empty(), "wait must consume");
    }

    #[tokio::test]
    async fn take_untaken_skips_consumed() {
        clear();
        register("w2", "bash");
        finish("w2", ToolResponse::text("w2", "x", ToolState::Success));
        let first = take_untaken();
        assert_eq!(first.len(), 1);
        assert!(take_untaken().is_empty());
        let again = wait("w2", None).await.expect("still stored");
        assert_eq!(again.joined_text(), "x");
    }

    #[tokio::test]
    async fn missing_id_is_none() {
        clear();
        assert!(wait("no-such", Some(Duration::from_millis(10)))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn offload_id_can_be_awaited() {
        clear();
        let coord = ToolCoordinator::new(None);
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let id = "bgwait-shell-1";
        let exec = coord.execute(shell_bg(id), "agent", None, move |_cancel| async move {
            let _ = go_rx.await;
            ToolResponse::text(id, "late-bg", ToolState::Success)
        });
        let out = tokio::time::timeout(Duration::from_secs(2), exec)
            .await
            .expect("execute should offload immediately");
        assert!(out.offloaded, "{out:?}");
        assert!(out.joined_text().contains("running in background"));
        assert!(exists(id));

        let waiter = wait(id, Some(Duration::from_secs(2)));
        tokio::pin!(waiter);
        tokio::task::yield_now().await;
        let _ = go_tx.send(());
        let got = waiter.await.expect("background shell result");
        assert_eq!(got.joined_text(), "late-bg");
        assert_eq!(got.state, ToolState::Success);
        assert!(
            coord.take_finished().is_empty(),
            "AwaitShell consumption must not drain as a hidden note"
        );
    }
}
