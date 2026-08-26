//! P1 subagent runtime (Cursor `Task`).
//!
//! Parent can spawn children with their own message window. Depth limit is 1
//! (a child cannot spawn Task). Completer is not Clone — children reconnect
//! via [`crate::agent::HttpCompleter::connect`] from the parent Config snapshot.
//! Child kind / capability / depth live on the Agent (`ChildCtx` / `DispatchCtx`),
//! not thread-locals — tokio work-stealing would drop TLS. Do not exec `grok`.
//!
//! Call [`dispatch`] from tool execution (`Task` / `AwaitShell`). Child
//! capability filtering is [`filter_tool`] (hook from `gate_tool`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;
use crate::tool_calls::{ToolCall, ToolResponse, ToolState};

mod live;
mod policy;
mod registry;
mod spawn;
mod worktree;

pub use policy::{deny_child_tool, CapabilityMode, SubagentType};
pub use registry::{
    get, list_for_parent, running_count, snapshot_json, ChildRecord, MAX_CONCURRENT,
};
pub use spawn::{register_live_runner, ChildOutcome, SpawnReq};
pub use worktree::Isolation;

#[derive(Clone, Copy, Debug)]
pub struct ChildCtx {
    pub kind: SubagentType,
    pub capability: CapabilityMode,
}

#[derive(Clone, Debug, Default)]
pub struct ParentBind {
    pub session_id: String,
    pub workspace: PathBuf,
    pub plan_mode: bool,
}

/// Everything `Task` needs from the parent Agent. Passed explicitly so a
/// work-stealing runtime cannot lose child/parent context.
#[derive(Clone, Debug)]
pub struct DispatchCtx {
    pub depth: u32,
    pub parent: ParentBind,
    pub config: Config,
    pub persist: bool,
    pub session_dir: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// Parent live sink for Task cards (`subagent` events). Child tools stay off this stream.
    pub emit: Option<crate::sidecar::EventSink>,
    pub permit: Option<crate::permit::PermitHub>,
    pub clarify: Option<crate::clarify::ClarifyHub>,
    pub print: bool,
}

impl DispatchCtx {
    /// Fallback when `Task` is invoked outside an Agent (direct `run_tool`).
    pub fn from_workspace(workspace: PathBuf) -> Self {
        Self {
            depth: 0,
            parent: ParentBind {
                session_id: String::new(),
                workspace,
                plan_mode: false,
            },
            config: Config::default(),
            persist: false,
            session_dir: None,
            home: None,
            emit: None,
            permit: None,
            clarify: None,
            print: false,
        }
    }
}

pub fn handles(name: &str) -> bool {
    match crate::tools_schema::dispatch_name(name) {
        "task" | "awaitshell" => true,
        _ => matches!(
            name,
            "wait_commands_or_subagents" | "kill_command_or_subagent"
        ),
    }
}

/// Register the live Agent runner once. Safe to call from every `Agent::new`.
pub fn ensure_live_runner() {
    spawn::register_live_runner(std::sync::Arc::new(|req| Box::pin(live::run(req))));
}

/// Child-side tool filter. Hook from `Agent::gate_tool`. `None` = not a child
/// or the tool is allowed.
pub fn filter_tool(call: &ToolCall, child: Option<&ChildCtx>) -> Option<ToolResponse> {
    let ctx = child?;
    let msg = deny_child_tool(ctx.kind, ctx.capability, call)?;
    Some(ToolResponse::text(&call.id, msg, ToolState::Error))
}

/// Dispatch Cursor/Grok subagent tools by name.
pub async fn dispatch(call: &ToolCall, ctx: &DispatchCtx) -> ToolResponse {
    match crate::tools_schema::dispatch_name(&call.name) {
        "task" => dispatch_task(call, ctx).await,
        "awaitshell" => dispatch_await(call).await,
        _ => match call.name.as_str() {
            "wait_commands_or_subagents" => dispatch_wait_many(call).await,
            "kill_command_or_subagent" => dispatch_kill(call),
            other => ToolResponse::text(
                &call.id,
                format!("Error: unknown subagent tool `{other}`."),
                ToolState::Error,
            ),
        },
    }
}

async fn dispatch_task(call: &ToolCall, ctx: &DispatchCtx) -> ToolResponse {
    if ctx.depth >= 1 {
        return ToolResponse::text(
            &call.id,
            "Error: subagent depth limit: Task cannot run inside a child (max depth 1).",
            ToolState::Error,
        );
    }

    let prompt = arg_str(&call.arguments, "prompt").unwrap_or_default();
    if prompt.trim().is_empty() {
        return ToolResponse::text(&call.id, "Error: Task requires `prompt`.", ToolState::Error);
    }
    let description = arg_str(&call.arguments, "description")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| default_description(&prompt));
    let kind_raw = arg_str(&call.arguments, "subagent_type")
        .or_else(|| arg_str(&call.arguments, "type"))
        .unwrap_or_else(|| "generalPurpose".into());
    let mut kind = SubagentType::parse(&kind_raw);
    let cap = arg_str(&call.arguments, "capability_mode")
        .and_then(|s| CapabilityMode::parse(&s))
        .unwrap_or_else(|| CapabilityMode::from_kind(kind));
    let background = arg_bool(&call.arguments, "run_in_background")
        .or_else(|| arg_bool(&call.arguments, "background"))
        .unwrap_or(false);
    let model = effective_model(arg_str(&call.arguments, "model").as_deref());
    let cwd = arg_str(&call.arguments, "cwd")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            if ctx.parent.workspace.as_os_str().is_empty() {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
            } else {
                ctx.parent.workspace.clone()
            }
        });

    if let Some(resume) = arg_str(&call.arguments, "resume")
        .or_else(|| arg_str(&call.arguments, "resume_from"))
        .filter(|s| !s.is_empty() && s != "self")
    {
        return resume_child(call, &resume, &prompt, background, ctx, model, cwd).await;
    }

    if ctx.parent.plan_mode && matches!(kind, SubagentType::GeneralPurpose | SubagentType::Office) {
        kind = SubagentType::Plan;
    }
    let isolation = match Isolation::parse(
        &arg_str(&call.arguments, "isolation").unwrap_or_else(|| "auto".into()),
    ) {
        Ok(i) => i,
        Err(e) => return ToolResponse::text(&call.id, e, ToolState::Error),
    };

    let child_short = uuid::Uuid::new_v4().simple().to_string();
    let child_short = &child_short[..8];
    let parent_id = if ctx.parent.session_id.is_empty() {
        "parent".to_string()
    } else {
        ctx.parent.session_id.clone()
    };
    let id = format!("{parent_id}-{child_short}");

    let handle = match registry::insert_running(
        id.clone(),
        parent_id.clone(),
        description.clone(),
        kind,
        cap,
        isolation,
    ) {
        Ok(h) => h,
        Err(e) => return ToolResponse::text(&call.id, e, ToolState::Error),
    };

    let req = spawn_req(
        handle.id.clone(),
        parent_id,
        prompt,
        description.clone(),
        kind,
        cap,
        model,
        cwd,
        handle.cancel.clone(),
        isolation,
        ctx,
    );
    launch(handle, req, background, &call.id, &description, kind).await
}

async fn resume_child(
    call: &ToolCall,
    resume: &str,
    prompt: &str,
    background: bool,
    ctx: &DispatchCtx,
    model: Option<String>,
    cwd: PathBuf,
) -> ToolResponse {
    let Some(prev) = registry::get(resume) else {
        return ToolResponse::text(
            &call.id,
            format!("Error: no subagent `{resume}` to resume."),
            ToolState::Error,
        );
    };
    if prev.status == registry::ChildStatus::Running {
        let rec = registry::wait(resume, None).await;
        return format_record(&call.id, rec);
    }
    let prompt = if child_transcript_exists(ctx, &prev.id) {
        prompt.to_string()
    } else {
        format!("Resume. Prior summary:\n{}\n\n{prompt}", prev.summary)
    };
    let handle = match registry::insert_running(
        prev.id.clone(),
        prev.parent_session.clone(),
        prev.description.clone(),
        prev.kind,
        prev.capability,
        prev.isolation,
    ) {
        Ok(h) => h,
        Err(e) => return ToolResponse::text(&call.id, e, ToolState::Error),
    };
    let req = spawn_req(
        handle.id.clone(),
        prev.parent_session.clone(),
        prompt,
        prev.description.clone(),
        prev.kind,
        prev.capability,
        model,
        cwd,
        handle.cancel.clone(),
        prev.isolation,
        ctx,
    );
    launch(
        handle,
        req,
        background,
        &call.id,
        &prev.description,
        prev.kind,
    )
    .await
}

fn spawn_req(
    id: String,
    parent_session: String,
    prompt: String,
    description: String,
    kind: SubagentType,
    capability: CapabilityMode,
    model: Option<String>,
    cwd: PathBuf,
    cancel: crate::tool_calls::CancelFlag,
    isolation: Isolation,
    ctx: &DispatchCtx,
) -> spawn::SpawnReq {
    spawn::SpawnReq {
        id,
        parent_session,
        prompt,
        description,
        kind,
        capability,
        model,
        cwd,
        cancel,
        isolation,
        persist: ctx.persist,
        session_dir: ctx.session_dir.clone(),
        home: ctx.home.clone(),
        config: ctx.config.clone(),
        emit: ctx.emit.clone(),
        permit: ctx.permit.clone(),
        clarify: ctx.clarify.clone(),
        print: ctx.print,
    }
}

async fn launch(
    handle: registry::ChildHandle,
    req: spawn::SpawnReq,
    background: bool,
    call_id: &str,
    description: &str,
    kind: SubagentType,
) -> ToolResponse {
    emit_child(
        req.emit.as_ref(),
        &handle.id,
        description,
        kind,
        "running",
        None,
        call_id,
    );
    let notify = handle.notify.clone();
    let wait_id = handle.id.clone();
    let spawn_id = handle.id.clone();
    let emit = req.emit.clone();
    let desc = description.to_string();
    let tool_call_id = call_id.to_string();
    tokio::spawn(async move {
        let out = spawn::run_child(req).await;
        let status = out.status.as_str().to_string();
        let summary = out.summary.clone();
        registry::finish(&spawn_id, out.status, out.summary, out.key_paths, out.error);
        emit_child(
            emit.as_ref(),
            &spawn_id,
            &desc,
            kind,
            &status,
            Some(summary.as_str()),
            &tool_call_id,
        );
        notify.notify_waiters();
    });

    if background {
        return ToolResponse::text(
            call_id,
            format!("BACKGROUND {wait_id} ({description} / {})", kind.as_str()),
            ToolState::Success,
        );
    }
    let rec = registry::wait(&wait_id, None).await;
    format_record(call_id, rec)
}

fn child_transcript_exists(ctx: &DispatchCtx, id: &str) -> bool {
    if !ctx.persist {
        return false;
    }
    let dir = match &ctx.session_dir {
        Some(d) => d.clone(),
        None => match Config::home_dir() {
            Ok(h) => h.join("sessions"),
            Err(_) => return false,
        },
    };
    Path::new(&dir.join(format!("{id}.jsonl"))).is_file()
}

async fn dispatch_await(call: &ToolCall) -> ToolResponse {
    let id = arg_str(&call.arguments, "task_id")
        .or_else(|| arg_str(&call.arguments, "shell_id"))
        .or_else(|| arg_str(&call.arguments, "id"))
        .unwrap_or_default();
    if id.is_empty() {
        return ToolResponse::text(
            &call.id,
            "Error: task_id/shell_id is required.",
            ToolState::Error,
        );
    }
    let timeout = arg_u64(&call.arguments, "timeout_ms")
        .or_else(|| arg_u64(&call.arguments, "block_until_ms"))
        .map(Duration::from_millis);
    let rec = registry::wait(&id, timeout).await;
    if rec.is_some() {
        return format_record(&call.id, rec);
    }
    if crate::tool_calls::bgwait::exists(&id) {
        return match crate::tool_calls::bgwait::wait(&id, timeout).await {
            Some(mut resp) => {
                resp.id = call.id.clone();
                resp.offloaded = false;
                resp
            }
            None => ToolResponse::text(
                &call.id,
                format!("timed out waiting for background tool `{id}` (still running)."),
                ToolState::Success,
            ),
        };
    }
    format_record(&call.id, None)
}

async fn dispatch_wait_many(call: &ToolCall) -> ToolResponse {
    let ids = arg_ids(&call.arguments);
    if ids.is_empty() {
        return ToolResponse::text(
            &call.id,
            "Error: task_ids (or task_id) is required.",
            ToolState::Error,
        );
    }
    let timeout = arg_u64(&call.arguments, "timeout_ms")
        .or_else(|| arg_u64(&call.arguments, "block_until_ms"))
        .map(Duration::from_millis);
    let recs = registry::wait_many(&ids, timeout).await;
    let body = recs
        .iter()
        .map(record_text)
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    ToolResponse::text(&call.id, body, ToolState::Success)
}

fn dispatch_kill(call: &ToolCall) -> ToolResponse {
    let id = arg_str(&call.arguments, "task_id")
        .or_else(|| arg_str(&call.arguments, "shell_id"))
        .or_else(|| arg_str(&call.arguments, "id"))
        .unwrap_or_default();
    if id.is_empty() {
        return ToolResponse::text(
            &call.id,
            "Error: task_id/shell_id is required.",
            ToolState::Error,
        );
    }
    match registry::kill(&id) {
        Some(rec) => ToolResponse::text(
            &call.id,
            format!("killed {}\n{}", rec.id, record_text(&rec)),
            ToolState::Success,
        ),
        None => ToolResponse::text(
            &call.id,
            format!("Error: no subagent `{id}`."),
            ToolState::Error,
        ),
    }
}

fn emit_child(
    emit: Option<&crate::sidecar::EventSink>,
    id: &str,
    description: &str,
    kind: SubagentType,
    status: &str,
    summary: Option<&str>,
    parent_tool_call_id: &str,
) {
    let Some(sink) = emit else {
        return;
    };
    sink.append(crate::session::SessionEvent::subagent(
        id,
        description,
        kind.as_str(),
        status,
        summary.map(str::to_string),
        parent_tool_call_id,
    ));
}

fn default_description(prompt: &str) -> String {
    let words: Vec<&str> = prompt.split_whitespace().take(5).collect();
    let s = words.join(" ");
    if s.is_empty() {
        "subagent".into()
    } else if s.chars().count() > 48 {
        let head: String = s.chars().take(47).collect();
        format!("{head}…")
    } else {
        s
    }
}

/// Parent may open a child transcript only when the id is `{parent}-{short}`
/// or the in-process registry lists this session as parent.
pub fn parent_owns_child(parent: &str, id: &str) -> bool {
    if parent.is_empty() || id.is_empty() || id == parent {
        return false;
    }
    if id.starts_with(&format!("{parent}-")) {
        return true;
    }
    registry::get(id).is_some_and(|r| r.parent_session == parent)
}

pub fn load_transcript(id: &str) -> Vec<crate::session::SessionEvent> {
    load_transcript_in(None, id)
}

pub fn load_transcript_in(dir: Option<&Path>, id: &str) -> Vec<crate::session::SessionEvent> {
    let open = if let Some(dir) = dir {
        crate::session::SessionLog::open_in(dir, id)
    } else {
        crate::session::SessionLog::open(id)
    };
    open.map(|log| log.events().to_vec()).unwrap_or_default()
}

/// Cursor `model` on Task. `inherit` / `fast` / `composer-*` keep the parent model.
pub(crate) fn effective_model(raw: Option<&str>) -> Option<String> {
    let s = raw.map(str::trim).filter(|s| !s.is_empty())?;
    let lower = s.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "inherit" | "default" | "parent" | "auto" | "fast"
    ) || lower.starts_with("composer")
        || lower.starts_with("cursor-")
    {
        return None;
    }
    Some(s.to_string())
}

fn format_record(call_id: &str, rec: Option<registry::ChildRecord>) -> ToolResponse {
    match rec {
        Some(r) => {
            let state = if r.status == registry::ChildStatus::Failed {
                ToolState::Error
            } else {
                ToolState::Success
            };
            ToolResponse::text(call_id, record_text(&r), state)
        }
        None => ToolResponse::text(call_id, "Error: subagent not found.", ToolState::Error),
    }
}

fn record_text(r: &registry::ChildRecord) -> String {
    let mut s = format!(
        "STATUS {} id={}\ntype={} {}\n",
        r.status.as_str(),
        r.id,
        r.kind.as_str(),
        r.description
    );
    if !r.summary.is_empty() {
        s.push('\n');
        s.push_str(&r.summary);
        s.push('\n');
    }
    if !r.key_paths.is_empty() && !r.summary.contains("KEY PATHS") {
        s.push_str("\nKEY PATHS\n");
        for p in &r.key_paths {
            s.push_str("- ");
            s.push_str(p);
            s.push('\n');
        }
    }
    if let Some(err) = &r.error {
        s.push_str("\nerror: ");
        s.push_str(err);
    }
    s
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn arg_bool(args: &Value, key: &str) -> Option<bool> {
    match args.get(key) {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn arg_ids(args: &Value) -> Vec<String> {
    if let Some(arr) = args
        .get("task_ids")
        .or_else(|| args.get("ids"))
        .and_then(|v| v.as_array())
    {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
            .collect();
    }
    arg_str(args, "task_id")
        .or_else(|| arg_str(args, "shell_id"))
        .into_iter()
        .collect()
}

#[cfg(test)]
pub(crate) async fn lock_registry_for_test() -> tokio::sync::MutexGuard<'static, ()> {
    static M: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    M.lock().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::Command;

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: args,
        }
    }

    fn parent_ctx(workspace: PathBuf) -> DispatchCtx {
        DispatchCtx {
            depth: 0,
            parent: ParentBind {
                session_id: "sess".into(),
                workspace,
                plan_mode: false,
            },
            config: Config::default(),
            persist: false,
            session_dir: None,
            home: None,
            emit: None,
            permit: None,
            clarify: None,
            print: false,
        }
    }

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hyper-sub-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_init(dir: &Path) {
        assert!(Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "i",
            ])
            .current_dir(dir)
            .status()
            .unwrap()
            .success());
    }

    fn extract_id(text: &str) -> String {
        text.lines()
            .find_map(|l| l.strip_prefix("STATUS "))
            .and_then(|l| l.split(" id=").nth(1))
            .unwrap_or("")
            .to_string()
    }

    #[tokio::test]
    async fn depth_limit_rejects_nested_task() {
        let _g = lock_registry_for_test().await;
        registry::clear();
        let mut ctx = parent_ctx(PathBuf::from("/tmp"));
        ctx.depth = 1;
        let resp = dispatch(
            &call(
                "Task",
                json!({
                    "prompt": "look around",
                    "description": "nested explore",
                    "subagent_type": "explore"
                }),
            ),
            &ctx,
        )
        .await;
        assert_eq!(resp.state, ToolState::Error);
        assert!(
            resp.joined_text().contains("depth limit"),
            "{}",
            resp.joined_text()
        );
    }

    #[tokio::test]
    async fn isolation_typo_is_rejected() {
        let _g = lock_registry_for_test().await;
        registry::clear();
        let resp = dispatch(
            &call(
                "Task",
                json!({
                    "prompt": "x",
                    "description": "iso",
                    "subagent_type": "explore",
                    "isolation": "wortree"
                }),
            ),
            &parent_ctx(PathBuf::from("/tmp")),
        )
        .await;
        assert_eq!(resp.state, ToolState::Error);
        let text = resp.joined_text();
        assert!(text.contains("none|worktree|auto"), "{text}");
        assert!(!text.contains("SUMMARY"), "{text}");
    }

    #[tokio::test]
    async fn worktree_isolation_requires_git() {
        let _g = lock_registry_for_test().await;
        registry::clear();
        let dir = tmp();
        let resp = dispatch(
            &call(
                "Task",
                json!({
                    "prompt": "x",
                    "description": "iso",
                    "subagent_type": "explore",
                    "isolation": "worktree"
                }),
            ),
            &parent_ctx(dir.clone()),
        )
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(resp.state, ToolState::Error);
        let text = resp.joined_text();
        assert!(
            text.contains("not a git repository") || text.contains("git worktree"),
            "{text}"
        );
        assert!(!text.contains("not enabled"));
    }

    #[tokio::test]
    async fn worktree_isolation_on_git_repo() {
        let _g = lock_registry_for_test().await;
        registry::clear();
        let dir = tmp();
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        git_init(&dir);
        let mut ctx = parent_ctx(dir.clone());
        ctx.home = Some(home.clone());
        let resp = dispatch(
            &call(
                "Task",
                json!({
                    "prompt": "x",
                    "description": "iso",
                    "subagent_type": "explore",
                    "isolation": "worktree"
                }),
            ),
            &ctx,
        )
        .await;
        let text = resp.joined_text();
        assert_eq!(resp.state, ToolState::Success, "{text}");
        assert!(text.contains("SUMMARY"), "{text}");
        assert!(text.contains("WORKTREE"), "{text}");
        let id = extract_id(&text);
        assert!(!id.is_empty(), "{text}");
        assert_eq!(
            registry::get(&id).map(|r| r.isolation),
            Some(Isolation::Worktree)
        );
        let dest = home.join("worktrees").join(&id);
        assert!(
            dest.is_dir(),
            "worktree dest should remain: {dest:?}\n{text}"
        );
        assert!(dest.join(".grok-hyper-keep").is_file(), "{dest:?}");
        std::fs::write(dest.join("draft.txt"), "keep me").unwrap();
        let second = dispatch(
            &call(
                "Task",
                json!({
                    "prompt": "again",
                    "description": "iso",
                    "subagent_type": "explore",
                    "resume": id,
                }),
            ),
            &ctx,
        )
        .await;
        let text2 = second.joined_text();
        assert_eq!(second.state, ToolState::Success, "{text2}");
        assert!(text2.contains("WORKTREE"), "{text2}");
        assert_eq!(
            registry::get(&id).map(|r| r.isolation),
            Some(Isolation::Worktree)
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("draft.txt")).unwrap(),
            "keep me"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn auto_isolation_does_not_create_worktree_on_git_repo() {
        let _g = lock_registry_for_test().await;
        registry::clear();
        let dir = tmp();
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        git_init(&dir);
        std::fs::write(dir.join("draft.txt"), "user is looking at this").unwrap();
        let mut ctx = parent_ctx(dir.clone());
        ctx.home = Some(home.clone());
        let resp = dispatch(
            &call(
                "Task",
                json!({
                    "prompt": "edit draft.txt",
                    "description": "office",
                    "subagent_type": "office"
                }),
            ),
            &ctx,
        )
        .await;
        let text = resp.joined_text();
        let wt_root = home.join("worktrees");
        let leftover = wt_root.exists()
            && std::fs::read_dir(&wt_root)
                .map(|rd| rd.filter_map(|e| e.ok()).any(|e| e.path().is_dir()))
                .unwrap_or(false);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(resp.state, ToolState::Success, "{text}");
        assert!(!text.contains("WORKTREE"), "{text}");
        assert!(
            !leftover,
            "auto must not create a worktree on a git office dir"
        );
    }

    #[tokio::test]
    async fn foreground_task_returns_summary() {
        let _g = lock_registry_for_test().await;
        registry::clear();
        let resp = dispatch(
            &call(
                "Task",
                json!({
                    "prompt": "read src/lib.rs and report",
                    "description": "scan lib",
                    "subagent_type": "explore"
                }),
            ),
            &parent_ctx(PathBuf::from("/tmp")),
        )
        .await;
        assert_eq!(resp.state, ToolState::Success, "{}", resp.joined_text());
        let text = resp.joined_text();
        assert!(text.contains("SUMMARY"), "{text}");
        assert!(
            text.contains("src/lib.rs") || text.contains("KEY PATHS") || text.contains("explore"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn resume_reuses_child_id() {
        let _g = lock_registry_for_test().await;
        registry::clear();
        let ctx = parent_ctx(PathBuf::from("/tmp"));
        let first = dispatch(
            &call(
                "Task",
                json!({
                    "prompt": "first pass",
                    "description": "scan",
                    "subagent_type": "explore"
                }),
            ),
            &ctx,
        )
        .await;
        assert_eq!(first.state, ToolState::Success, "{}", first.joined_text());
        let id = extract_id(&first.joined_text());
        assert!(!id.is_empty(), "{}", first.joined_text());
        let second = dispatch(
            &call(
                "Task",
                json!({
                    "prompt": "follow up",
                    "description": "scan",
                    "subagent_type": "explore",
                    "resume": id,
                }),
            ),
            &ctx,
        )
        .await;
        let text = second.joined_text();
        assert_eq!(second.state, ToolState::Success, "{text}");
        assert!(text.contains(&id), "{text}");
        assert!(
            text.contains("Resume. Prior summary") || text.contains("follow up"),
            "{text}"
        );
        assert_eq!(
            registry::get(&id).map(|r| r.status),
            Some(registry::ChildStatus::Done)
        );
    }

    #[tokio::test]
    async fn explore_filter_blocks_write() {
        let ctx = ChildCtx {
            kind: SubagentType::Explore,
            capability: CapabilityMode::ReadOnly,
        };
        let denied = filter_tool(
            &call("write", json!({"path": "a.rs", "content": "x"})),
            Some(&ctx),
        );
        assert!(denied.is_some());
        assert!(denied.unwrap().joined_text().contains("explore"));
        assert!(filter_tool(
            &call("write", json!({"path": "a.rs", "content": "x"})),
            None
        )
        .is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn child_ctx_survives_yield() {
        let ctx = ChildCtx {
            kind: SubagentType::Explore,
            capability: CapabilityMode::ReadOnly,
        };
        tokio::task::yield_now().await;
        assert!(filter_tool(
            &call("Write", json!({"path": "a.rs", "contents": "x"})),
            Some(&ctx)
        )
        .is_some());
    }

    #[test]
    fn wrap_prompt_does_not_reassign_identity() {
        let p = spawn::wrap_prompt(SubagentType::Office, "draft the memo");
        assert_eq!(p, "draft the memo");
        assert!(!p.to_ascii_lowercase().contains("you are"));
        let p = spawn::wrap_prompt(SubagentType::GeneralPurpose, "fix the bug");
        assert_eq!(p, "fix the bug");
        let p = spawn::wrap_prompt(SubagentType::Explore, "look around");
        assert!(p.contains("This Task is explore"));
        assert!(!p.contains("You are an explore"));
        assert!(p.contains("look around"));
    }

    #[test]
    fn parse_accepts_cursor_aliases() {
        assert_eq!(SubagentType::parse("inherit"), SubagentType::GeneralPurpose);
        assert_eq!(SubagentType::parse("shell"), SubagentType::GeneralPurpose);
        assert_eq!(SubagentType::parse(""), SubagentType::GeneralPurpose);
        assert_eq!(SubagentType::parse("nope"), SubagentType::GeneralPurpose);
        assert_eq!(SubagentType::parse("explore"), SubagentType::Explore);
        assert_eq!(SubagentType::parse("cursor-guide"), SubagentType::Explore);
        assert_eq!(SubagentType::parse("plan"), SubagentType::Plan);
        assert_eq!(SubagentType::parse("office"), SubagentType::Office);
    }

    #[tokio::test]
    async fn inherit_and_run_in_background_do_not_error() {
        let _g = lock_registry_for_test().await;
        registry::clear();
        let resp = dispatch(
            &call(
                "Task",
                json!({
                    "prompt": "look around the tree",
                    "description": "scan tree",
                    "subagent_type": "inherit",
                    "run_in_background": true
                }),
            ),
            &parent_ctx(PathBuf::from("/tmp")),
        )
        .await;
        let text = resp.joined_text();
        assert_eq!(resp.state, ToolState::Success, "{text}");
        assert!(text.starts_with("BACKGROUND "), "{text}");
        assert!(text.contains("generalPurpose"), "{text}");
    }

    #[tokio::test]
    async fn launch_emits_subagent_card() {
        let _g = lock_registry_for_test().await;
        registry::clear();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = parent_ctx(PathBuf::from("/tmp"));
        ctx.emit = Some(crate::sidecar::EventSink::new(tx));
        let resp = dispatch(
            &call(
                "Task",
                json!({
                    "prompt": "look around",
                    "description": "scan lib",
                    "subagent_type": "explore",
                    "run_in_background": true
                }),
            ),
            &ctx,
        )
        .await;
        assert_eq!(resp.state, ToolState::Success, "{}", resp.joined_text());
        let mut kinds = Vec::new();
        while let Ok(e) = rx.try_recv() {
            kinds.push(e.type_name().to_string());
        }
        assert!(
            kinds.iter().any(|k| k == "subagent"),
            "expected a Task card event, got {kinds:?}"
        );
    }

    #[test]
    fn parent_owns_hyphenated_child_id() {
        assert!(parent_owns_child("sess", "sess-abcd1234"));
        assert!(!parent_owns_child("sess", "sess"));
        assert!(!parent_owns_child("sess", "other-abcd1234"));
    }

    #[test]
    fn cursor_model_aliases_inherit_parent() {
        assert_eq!(effective_model(None), None);
        assert_eq!(effective_model(Some("")), None);
        assert_eq!(effective_model(Some("inherit")), None);
        assert_eq!(effective_model(Some("fast")), None);
        assert_eq!(effective_model(Some("composer-2")), None);
        assert_eq!(effective_model(Some("cursor-small")), None);
        assert_eq!(
            effective_model(Some("grok-4.6")).as_deref(),
            Some("grok-4.6")
        );
    }

    #[tokio::test]
    async fn model_fast_does_not_error_on_spawn() {
        let _g = lock_registry_for_test().await;
        registry::clear();
        let resp = dispatch(
            &call(
                "Task",
                json!({
                    "prompt": "ping",
                    "description": "probe connectivity",
                    "subagent_type": "explore",
                    "model": "fast",
                    "run_in_background": true
                }),
            ),
            &parent_ctx(PathBuf::from("/tmp")),
        )
        .await;
        let text = resp.joined_text();
        assert_eq!(resp.state, ToolState::Success, "{text}");
        assert!(text.starts_with("BACKGROUND "), "{text}");
        assert!(
            !text.to_ascii_lowercase().contains("model not found"),
            "{text}"
        );
    }
}
