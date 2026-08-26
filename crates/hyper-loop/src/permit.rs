//! Grok-shaped permission + plan overlays, without extra OpenAI tools.
//!
//! `tools[]` stays frozen. Plan is a hidden-user card + mutating-tool deny
//! except writes whose path is `plan.md` (or the session plan file).
//! Ask/auto is a TUI oneshot in front of write/edit/bash/run_code/mcp.

use std::collections::HashSet;
use std::fmt;
use std::path::{Component, Path};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::tool_calls::CancelFlag;

/// Default session plan file. Other writes are denied while `plan_mode` is on.
pub const DEFAULT_PLAN_FILE: &str = "plan.md";

// 文案必须与 plan_mode_blocks 的放行面一致：只读 + plan.md 可写。
pub const PLAN_CARD: &str = "\
PLAN MODE. Allowed: Read, Glob, Grep, WebSearch, WebFetch, view, recall, \
AskQuestion, and Write/StrReplace of plan.md (the session plan file). Do not \
write other files. Mutating Shell, run_code, and mcp are blocked. Inspect the \
workspace and put the markdown plan in plan.md: files to change, steps, risks. \
Do not implement yet.";

pub const PLAN_IMPLEMENT: &str = "Implement the approved plan.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApprovalMode {
    /// Prompt on mutating tools (TUI). `--print` has no prompt and stays YOLO.
    #[default]
    Ask,
    /// Workspace edits pass; bash / run_code / mcp still prompt.
    Auto,
    /// Never prompt.
    Yolo,
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::Yolo => "yolo",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" | "default" | "on" => Some(Self::Ask),
            "auto" | "acceptedits" | "edits" => Some(Self::Auto),
            "yolo" | "bypass" | "off" | "bypasspermissions" => Some(Self::Yolo),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanAction {
    On,
    Off,
    Go,
}

#[derive(Clone, Debug)]
pub struct PermitAsk {
    pub tool: String,
    pub preview: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermitDecision {
    Allow,
    Always,
    Deny,
}

pub struct PermitRequest {
    pub ask: PermitAsk,
    pub reply: oneshot::Sender<PermitDecision>,
}

/// TUI owns the receiver. Agent clones the hub and `check()`s before mutating tools.
#[derive(Clone)]
pub struct PermitHub {
    tx: mpsc::UnboundedSender<PermitRequest>,
    mode: Arc<Mutex<ApprovalMode>>,
    always: Arc<Mutex<HashSet<String>>>,
}

impl fmt::Debug for PermitHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PermitHub")
            .field("mode", &self.mode())
            .finish_non_exhaustive()
    }
}

impl PermitHub {
    pub fn pair(mode: ApprovalMode) -> (Self, mpsc::UnboundedReceiver<PermitRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                mode: Arc::new(Mutex::new(mode)),
                always: Arc::new(Mutex::new(HashSet::new())),
            },
            rx,
        )
    }

    pub fn set_mode(&self, mode: ApprovalMode) {
        if let Ok(mut g) = self.mode.lock() {
            *g = mode;
        }
    }

    pub fn mode(&self) -> ApprovalMode {
        self.mode.lock().map(|g| *g).unwrap_or(ApprovalMode::Ask)
    }

    pub fn remember(&self, tool: &str) {
        if let Ok(mut g) = self.always.lock() {
            g.insert(tool.to_string());
        }
    }

    pub fn needs_prompt(mode: ApprovalMode, tool: &str) -> bool {
        if !is_mutating(tool) {
            return false;
        }
        match mode {
            ApprovalMode::Yolo => false,
            ApprovalMode::Ask => true,
            ApprovalMode::Auto => !matches!(
                normalize_tool(tool).as_str(),
                "write" | "edit" | "strreplace" | "delete"
            ),
        }
    }

    pub async fn check(&self, tool: &str, preview: &str, cancel: &CancelFlag) -> PermitDecision {
        if !Self::needs_prompt(self.mode(), tool) {
            return PermitDecision::Allow;
        }
        if self
            .always
            .lock()
            .map(|g| g.contains(tool))
            .unwrap_or(false)
        {
            return PermitDecision::Allow;
        }
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(PermitRequest {
                ask: PermitAsk {
                    tool: tool.to_string(),
                    preview: preview.to_string(),
                },
                reply,
            })
            .is_err()
        {
            return PermitDecision::Deny;
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => PermitDecision::Deny,
            dec = rx => dec.unwrap_or(PermitDecision::Deny),
        }
    }
}

pub fn is_mutating(tool: &str) -> bool {
    matches!(
        normalize_tool(tool).as_str(),
        "write"
            | "edit"
            | "delete"
            | "bash"
            | "shell"
            | "runcode"
            | "mcp"
            | "strreplace"
            | "todowrite"
    )
}

/// True when plan mode must reject this call. `ask` / AskQuestion always pass.
/// Write/edit/delete pass only when the path is `plan.md` (or `plan_file`).
/// Shell passes when the command is non-mutating, or only touches the plan file.
pub fn plan_mode_blocks(tool: &str, args: &Value) -> bool {
    plan_mode_blocks_in(tool, args, DEFAULT_PLAN_FILE)
}

pub fn plan_mode_blocks_in(tool: &str, args: &Value, plan_file: &str) -> bool {
    let name = normalize_tool(tool);
    if matches!(name.as_str(), "ask" | "askquestion") {
        return false;
    }
    match name.as_str() {
        "write" | "edit" | "delete" | "strreplace" => match tool_path(args) {
            Some(path) if is_plan_file_path(&path, plan_file) => false,
            _ => true,
        },
        "bash" | "shell" => {
            let cmd = tool_command(args).unwrap_or_default();
            !shell_allowed_in_plan(&cmd, plan_file)
        }
        "runcode" | "mcp" => true,
        _ => false,
    }
}

pub fn is_plan_file_path(path: &str, plan_file: &str) -> bool {
    let path = path.trim().trim_matches('"').trim_matches('\'');
    let plan_file = plan_file.trim();
    if path.is_empty() || plan_file.is_empty() {
        return false;
    }
    let p = Path::new(path);
    let plan = Path::new(plan_file);
    if p == plan || path.eq_ignore_ascii_case(plan_file) {
        return true;
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    let path_comps = meaningful_components(p);
    let plan_comps = meaningful_components(plan);
    if path_comps.is_empty() || plan_comps.is_empty() {
        return false;
    }
    if path_comps.len() == plan_comps.len() {
        return comps_eq(&path_comps, &plan_comps);
    }
    // Default `plan.md` is workspace-root only. Nested `notes/plan.md` is not.
    if plan_comps.len() == 1 {
        return false;
    }
    // Configured plan with directories: allow a workspace prefix.
    path_ends_with_plan(&path_comps, &plan_comps)
}

fn meaningful_components(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str().map(|t| t.to_string()),
            _ => None,
        })
        .collect()
}

fn comps_eq(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

fn path_ends_with_plan(path: &[String], plan: &[String]) -> bool {
    if path.len() < plan.len() {
        return false;
    }
    comps_eq(&path[path.len() - plan.len()..], plan)
}

/// Plan-mode shell: mutating unless every segment is a known read-only primary.
/// `python3`, `make`, and `cargo test` are mutating. `rg --pre` is mutating.
pub fn shell_is_mutating(cmd: &str) -> bool {
    let c = cmd.trim();
    if c.is_empty() {
        return false;
    }
    let lower = c.to_ascii_lowercase();
    if lower.contains(">>")
        || (lower.contains('|') && lower.contains("tee"))
        || has_stdout_redirect(&lower)
    {
        return true;
    }
    split_shell_segments(&lower)
        .into_iter()
        .any(segment_is_mutating)
}

fn split_shell_segments(cmd: &str) -> Vec<&str> {
    let b = cmd.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'&' && i + 1 < b.len() && b[i + 1] == b'&' {
            out.push(cmd[start..i].trim());
            i += 2;
            start = i;
            continue;
        }
        if b[i] == b'|' && i + 1 < b.len() && b[i + 1] == b'|' {
            out.push(cmd[start..i].trim());
            i += 2;
            start = i;
            continue;
        }
        if b[i] == b'|' || b[i] == b';' {
            out.push(cmd[start..i].trim());
            i += 1;
            start = i;
            continue;
        }
        i += 1;
    }
    out.push(cmd[start..].trim());
    out.into_iter().filter(|s| !s.is_empty()).collect()
}

fn segment_is_mutating(seg: &str) -> bool {
    let raw: Vec<&str> = seg.split_whitespace().collect();
    let mut i = 0;
    while i < raw.len() {
        let t = raw[i].trim_matches('"').trim_matches('\'');
        if t.contains('=') && !t.starts_with('-') {
            i += 1;
            continue;
        }
        break;
    }
    if i >= raw.len() {
        return false;
    }
    let tokens: Vec<&str> = raw[i..]
        .iter()
        .map(|t| t.trim_matches('"').trim_matches('\''))
        .collect();
    let primary = command_basename(tokens[0]).to_ascii_lowercase();
    match primary.as_str() {
        "ls" | "cat" | "pwd" | "date" | "whoami" | "head" | "tail" | "wc" | "grep" => false,
        "rg" => tokens
            .iter()
            .any(|t| *t == "--pre" || t.starts_with("--pre=")),
        "git" => !git_subcommand_readonly(&tokens),
        _ => true,
    }
}

fn command_basename(tok: &str) -> &str {
    Path::new(tok)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(tok)
}

fn git_subcommand_readonly(tokens: &[&str]) -> bool {
    let mut i = 1;
    while i < tokens.len() {
        let t = tokens[i];
        if t == "-C" || t == "-c" || t == "--git-dir" || t == "--work-tree" {
            i += 2;
            continue;
        }
        if t.starts_with('-') {
            i += 1;
            continue;
        }
        return matches!(
            t,
            "status" | "log" | "diff" | "show" | "branch" | "rev-parse"
        );
    }
    false
}

fn shell_allowed_in_plan(cmd: &str, plan_file: &str) -> bool {
    if !shell_is_mutating(cmd) {
        return true;
    }
    let plan_name = Path::new(plan_file)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(plan_file);
    let mut saw_plan = false;
    for tok in cmd.split_whitespace() {
        let t = tok.trim_matches('"').trim_matches('\'');
        if looks_like_path(t) {
            if is_plan_file_path(t, plan_file) || t.eq_ignore_ascii_case(plan_name) {
                saw_plan = true;
            } else {
                return false;
            }
        }
    }
    saw_plan
}

fn looks_like_path(tok: &str) -> bool {
    tok.contains('/')
        || tok.contains('\\')
        || tok.ends_with(".md")
        || tok.ends_with(".rs")
        || tok.ends_with(".toml")
        || tok.contains('.')
}

fn has_stdout_redirect(cmd: &str) -> bool {
    let bytes = cmd.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'>' {
            let prev_ok = i == 0 || bytes[i - 1] != b'>';
            if prev_ok && (i == 0 || bytes[i - 1] != b'2' && bytes[i - 1] != b'<') {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn normalize_tool(tool: &str) -> String {
    tool.trim().to_ascii_lowercase().replace(['_', '-'], "")
}

fn tool_path(args: &Value) -> Option<String> {
    const KEYS: &[&str] = &["path", "file_path", "target_file", "target", "dest"];
    for k in KEYS {
        if let Some(s) = args.get(*k).and_then(|v| v.as_str()).map(str::trim) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn tool_command(args: &Value) -> Option<String> {
    args.get("command")
        .or_else(|| args.get("cmd"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

// 失败文案契约：ToolState 不落盘，`observed_from_messages` 靠 "Error:" 前缀
// 判失败重建盲覆写守卫。所有非 Success 文案必须以 "Error:" 开头。
pub fn plan_denied(tool: &str) -> String {
    format!(
        "Error: plan mode: `{tool}` blocked. Only {DEFAULT_PLAN_FILE} (session plan file) \
         is writable. Stay otherwise read-only. The user will /plan go when they want you \
         to implement."
    )
}

pub fn user_denied(tool: &str) -> String {
    format!("Error: User denied `{tool}`. Continue without that call, or ask a different way.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_allows_edits_asks_bash() {
        assert!(!PermitHub::needs_prompt(ApprovalMode::Auto, "write"));
        assert!(!PermitHub::needs_prompt(ApprovalMode::Auto, "Write"));
        assert!(!PermitHub::needs_prompt(ApprovalMode::Auto, "edit"));
        assert!(!PermitHub::needs_prompt(ApprovalMode::Auto, "StrReplace"));
        assert!(!PermitHub::needs_prompt(ApprovalMode::Auto, "Delete"));
        assert!(PermitHub::needs_prompt(ApprovalMode::Auto, "bash"));
        assert!(PermitHub::needs_prompt(ApprovalMode::Auto, "Shell"));
        assert!(!PermitHub::needs_prompt(ApprovalMode::Ask, "read"));
        assert!(!PermitHub::needs_prompt(ApprovalMode::Ask, "Read"));
        assert!(PermitHub::needs_prompt(ApprovalMode::Ask, "write"));
        assert!(PermitHub::needs_prompt(ApprovalMode::Ask, "Write"));
        assert!(!PermitHub::needs_prompt(ApprovalMode::Yolo, "bash"));
        assert!(!PermitHub::needs_prompt(ApprovalMode::Ask, "Task"));
        assert!(!PermitHub::needs_prompt(ApprovalMode::Auto, "Task"));
        assert!(!is_mutating("task"));
        assert!(!is_mutating("Task"));
    }

    #[test]
    fn plan_mode_path_allowlist() {
        let plan = serde_json::json!({"path": "plan.md", "content": "# plan"});
        let nested = serde_json::json!({"path": "docs/plan.md"});
        let other = serde_json::json!({"path": "src/lib.rs", "content": "nope"});
        let edit_plan = serde_json::json!({
            "path": "plan.md",
            "old_string": "a",
            "new_string": "b"
        });
        let ask = serde_json::json!({"prompt": "pick", "options": [{"label": "a"}]});
        let ls = serde_json::json!({"command": "ls -la && git status"});
        let rm = serde_json::json!({"command": "rm -rf src"});
        let redirect_plan = serde_json::json!({"command": "cat > plan.md <<'EOF'\nhi\nEOF"});

        assert!(!plan_mode_blocks("write", &plan));
        assert!(!plan_mode_blocks("Write", &plan));
        assert!(!plan_mode_blocks("edit", &edit_plan));
        assert!(!plan_mode_blocks("StrReplace", &edit_plan));
        assert!(plan_mode_blocks("write", &nested));
        assert!(plan_mode_blocks("write", &other));
        assert!(plan_mode_blocks("Delete", &other));
        assert!(!plan_mode_blocks("ask", &ask));
        assert!(!plan_mode_blocks("AskQuestion", &ask));
        assert!(!plan_mode_blocks("bash", &ls));
        assert!(plan_mode_blocks("Shell", &rm));
        assert!(!plan_mode_blocks("bash", &redirect_plan));
        assert!(plan_mode_blocks(
            "run_code",
            &serde_json::json!({"code": "print(1)"})
        ));
        assert!(!plan_mode_blocks(
            "task",
            &serde_json::json!({"prompt": "explore"})
        ));
        assert!(!plan_mode_blocks(
            "Task",
            &serde_json::json!({"prompt": "explore"})
        ));
        assert!(plan_mode_blocks(
            "Shell",
            &serde_json::json!({"command": "python3 script.py"})
        ));
        assert!(plan_mode_blocks(
            "bash",
            &serde_json::json!({"command": "make"})
        ));
        assert!(plan_mode_blocks(
            "Shell",
            &serde_json::json!({"command": "cargo test"})
        ));
        assert!(plan_mode_blocks(
            "Shell",
            &serde_json::json!({"command": "rg --pre python foo"})
        ));
        assert!(!plan_mode_blocks(
            "Shell",
            &serde_json::json!({"command": "rg foo"})
        ));
        assert!(!is_plan_file_path("notes/plan.md", DEFAULT_PLAN_FILE));
        assert!(!is_plan_file_path("notes/todo.md", DEFAULT_PLAN_FILE));
        assert!(is_plan_file_path("plan.md", DEFAULT_PLAN_FILE));
        assert!(is_plan_file_path(
            "/tmp/ws/.grok-hyper/plan.md",
            ".grok-hyper/plan.md"
        ));
    }
}
