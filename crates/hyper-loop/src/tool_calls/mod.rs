//! Tool-call trajectory coordinator.
//!
//! Python source: `third_party/QwenPaw/src/qwenpaw/tool_calls/`
//! Behavior kept: single owner of in-flight calls, offload vs kill deadlines,
//! cooperative cancel then force-abort, four-tier timeout resolution.
//! Offload detaches the join from the agent hop and keeps the child running
//! until it finishes, is cancelled, or hits kill_at. The model collects the
//! real result with AwaitShell (`bgwait`); the loop does not inject a hidden
//! user note.
//!
//! Design changes vs Python:
//! - no ContextVar / `Any` / agentscope `ToolResponse`
//! - `tokio::select!` instead of a waiter dict + sentinel object
//! - cancellation is a `watch` flag, not a sentinel object

pub mod bgwait;
mod coordinator;
mod timeout;
mod types;

pub use coordinator::ToolCoordinator;
pub use timeout::{
    arm_kill_deadline, effective_timeout, COORDINATOR_OWNED_EXEC_TIMEOUT_SECS,
    MIN_BACKGROUND_WINDOW_SECS, OFFLOAD_TIMEOUT_RATIO,
};
pub use types::{
    CancelFlag, CancelReason, OffloadReason, TextBlock, ToolCall, ToolCallStatus, ToolResponse,
    ToolState,
};
