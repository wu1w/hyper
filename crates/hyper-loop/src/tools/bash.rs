//! `bash`. QwenPaw `execute_shell_command`: fresh subprocess, workspace cwd, formatted output.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::{arg_str, arg_u32, folded_response, BlobStore, ToolLimits, Workspace};
use crate::tool_calls::{CancelFlag, ToolCall, ToolResponse, ToolState};

const OUTPUT_MAX_BYTES: usize = 1024 * 1024;
/// shell 退出后管道的收尾读窗口：孙进程（`sleep 30 & echo hi`）继承了
/// stdout/stderr 写端，EOF 可能永远不来，超时就放弃。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
enum ShellKind {
    Bash,
    Posix,
    PowerShell,
}

#[derive(Clone, Debug)]
struct ShellSpec {
    exe: PathBuf,
    kind: ShellKind,
}

static SHELL: OnceLock<ShellSpec> = OnceLock::new();

#[cfg(windows)]
struct WindowsJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl WindowsJob {
    fn attach(pid: u32) -> std::io::Result<Self> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };

        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() || job == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error());
            }
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                let e = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(e);
            }
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if process.is_null() || process == INVALID_HANDLE_VALUE {
                let e = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(e);
            }
            let assigned = AssignProcessToJobObject(job, process);
            CloseHandle(process);
            if assigned == 0 {
                let e = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(e);
            }
            Ok(Self(job))
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
unsafe impl Send for WindowsJob {}

pub async fn bash(
    ws: &Workspace,
    call: &ToolCall,
    cancel: CancelFlag,
    limits: ToolLimits,
    blobs: Option<&BlobStore>,
) -> ToolResponse {
    let Some(command) = arg_str(&call.arguments, "command") else {
        return ToolResponse::text(&call.id, "Error: No `command` provided.", ToolState::Error);
    };
    let command = command.trim().to_string();
    if command.is_empty() {
        return ToolResponse::text(&call.id, "Error: No `command` provided.", ToolState::Error);
    }
    let cwd = if let Some(raw) = arg_str(&call.arguments, "working_directory")
        .or_else(|| arg_str(&call.arguments, "workdir"))
        .or_else(|| arg_str(&call.arguments, "cwd"))
    {
        match ws.resolve(&raw) {
            Ok(p) => p,
            Err(e) => return ToolResponse::text(&call.id, e, ToolState::Error),
        }
    } else {
        ws.root().to_path_buf()
    };
    let (command, skipped_tree_diff) = match rewrite_skip_whole_tree_git_diff(&command) {
        GitDiffRewrite::Keep => (command, false),
        GitDiffRewrite::SkipAll => {
            return ToolResponse::text(&call.id, TREE_DIFF_HINT, ToolState::Success);
        }
        GitDiffRewrite::Rest(rest) => (rest, true),
    };
    let (command, skipped_tree_list) =
        match rewrite_skip_whole_tree_listing(&command, ws.root(), &cwd) {
            GitDiffRewrite::Keep => (command, false),
            GitDiffRewrite::SkipAll => {
                let sample = super::shallow_listing(&cwd, ws.root(), 40);
                let mut text = TREE_LIST_HINT.to_string();
                if !sample.is_empty() {
                    text.push_str("\n\nTop-level:\n");
                    text.push_str(&sample.join("\n"));
                }
                return ToolResponse::text(&call.id, text, ToolState::Success);
            }
            GitDiffRewrite::Rest(rest) => (rest, true),
        };
    // Cursor contract: `block_until_ms` waits then backgrounds. Inner bash
    // only dies on cancel; the coordinator offloads at that deadline.
    let mut child = match spawn_shell(&command, &cwd) {
        Ok(c) => c,
        Err(e) => {
            return ToolResponse::text(
                &call.id,
                format!("Error: failed to spawn shell: {e}"),
                ToolState::Error,
            );
        }
    };
    // A per-call Windows Job gives cancellation/drop the same descendant-tree
    // semantics as the Unix process group. Failure is non-fatal on restricted
    // hosts; direct-child cancellation still works.
    #[cfg(windows)]
    let _job = child.id().and_then(|pid| WindowsJob::attach(pid).ok());

    // 缓冲共享给读取任务：收尾读超时被 abort 时，已读到的部分不丢。
    let out_buf: Arc<Mutex<Vec<u8>>> = Arc::default();
    let err_buf: Arc<Mutex<Vec<u8>>> = Arc::default();
    let out_task = child
        .stdout
        .take()
        .map(|p| tokio::spawn(read_capped_into(p, out_buf.clone())));
    let err_task = child
        .stderr
        .take()
        .map(|p| tokio::spawn(read_capped_into(p, err_buf.clone())));

    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            // 杀整个进程组：只杀直接 shell 会留下孙进程（后台任务）继续
            // 持有管道写端，读取任务永远等不到 EOF。
            kill_group(&child);
            let _ = child.start_kill();
            let _ = child.wait().await;
            drain(out_task).await;
            drain(err_task).await;
            ToolResponse::text(
                &call.id,
                "Command failed with exit code -1.\n[stderr]\ncancelled",
                ToolState::Interrupted,
            )
        }
        status = child.wait() => {
            drain(out_task).await;
            drain(err_task).await;
            let stdout = take_text(&out_buf);
            let stderr = take_text(&err_buf);
            let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
            let mut text = format_shell(code, &stdout, &stderr);
            if skipped_tree_diff {
                text = format!("{TREE_DIFF_HINT}\n\n{text}");
            }
            if skipped_tree_list {
                text = format!("{TREE_LIST_HINT}\n\n{text}");
            }
            let state = if code == 0 {
                ToolState::Success
            } else {
                ToolState::Error
            };
            folded_response(&call.id, text, state, limits, blobs)
        }
    }
}

/// Whole-tree `git diff` dumped 1MB of dist/vendor on Feishu and hung the next hop.
const TREE_DIFF_HINT: &str = "[workset] already has git status. Whole-tree `git diff` / `git show` / `git log -p` was skipped (dist/vendor dumps blow the window). Diff a specific path: git diff -- path.";
const TREE_LIST_HINT: &str = "\
Workspace-root `find` / `ls -R` / `tree` / `git ls-files` / `fd` / `rg --files` \
is a top-level sample only (vendor / release / out skipped). Grep a symbol, or \
walk a subdirectory.";

#[derive(Debug, PartialEq, Eq)]
enum GitDiffRewrite {
    Keep,
    SkipAll,
    Rest(String),
}

fn rewrite_drop_segments(command: &str, drop: impl Fn(&str) -> bool) -> GitDiffRewrite {
    let segs = split_cmd_segments(command);
    if segs.is_empty() {
        return GitDiffRewrite::Keep;
    }
    let nonempty: Vec<&str> = segs
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let kept: Vec<&str> = nonempty.iter().copied().filter(|s| !drop(s)).collect();
    if kept.len() == nonempty.len() {
        GitDiffRewrite::Keep
    } else if kept.is_empty() {
        GitDiffRewrite::SkipAll
    } else {
        GitDiffRewrite::Rest(kept.join(" && "))
    }
}

fn rewrite_skip_whole_tree_git_diff(command: &str) -> GitDiffRewrite {
    rewrite_drop_segments(command, is_whole_tree_git_diff_segment)
}

fn rewrite_skip_whole_tree_listing(command: &str, ws_root: &Path, cwd: &Path) -> GitDiffRewrite {
    rewrite_drop_segments(command, |s| is_whole_tree_listing_segment(s, ws_root, cwd))
}

fn split_cmd_segments(command: &str) -> Vec<String> {
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => {
                quote = None;
                cur.push(c);
            }
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                cur.push(c);
            }
            None if c == '&' && chars.peek() == Some(&'&') => {
                chars.next();
                segs.push(std::mem::take(&mut cur));
            }
            None if c == ';' => {
                segs.push(std::mem::take(&mut cur));
            }
            None => cur.push(c),
        }
    }
    segs.push(cur);
    segs
}

fn is_whole_tree_git_diff_segment(seg: &str) -> bool {
    let tokens = shell_tokens(seg);
    let Some((sub, args)) = git_subcommand(&tokens) else {
        return false;
    };
    match sub {
        "diff" | "difftool" | "show" => git_dump_without_path(args, false),
        "log" => {
            let patch = args
                .iter()
                .any(|a| matches!(a.as_str(), "-p" | "-u" | "--patch" | "--full-diff"));
            patch && git_dump_without_path(args, true)
        }
        _ => false,
    }
}

fn is_whole_tree_listing_segment(seg: &str, ws_root: &Path, cwd: &Path) -> bool {
    let tokens = shell_tokens(seg);
    let Some((i, cmd)) = first_shell_cmd(&tokens) else {
        return false;
    };
    let args = tokens.get(i + 1..).unwrap_or(&[]);
    match cmd_basename(cmd) {
        "find" | "gfind" => find_is_workspace_dump(args, ws_root, cwd),
        "ls" | "gls" => ls_is_recursive_root(args, ws_root, cwd),
        "tree" => tree_is_workspace_root(args, ws_root, cwd),
        "git" => git_is_whole_tree_index_dump(&tokens),
        "fd" | "fdfind" => fd_is_workspace_dump(args, ws_root, cwd),
        "rg" | "ripgrep" => rg_files_is_root_dump(args, ws_root, cwd),
        _ => false,
    }
}

fn first_shell_cmd(tokens: &[String]) -> Option<(usize, &str)> {
    let mut i = 0;
    while i < tokens.len() && tokens[i].contains('=') && !tokens[i].starts_with('-') {
        i += 1;
    }
    tokens.get(i).map(|c| (i, c.as_str()))
}

fn cmd_basename(cmd: &str) -> &str {
    Path::new(cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cmd)
}

/// `cat` / `head` / `tail` / `nl` / `sed -n Np` of a single file, no pipes.
/// Used to fold Shell dumps of a path Search already showed.
pub(crate) fn cat_like_path(command: &str) -> Option<String> {
    let segs: Vec<String> = split_cmd_segments(command)
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if segs.len() != 1 {
        return None;
    }
    let raw = &segs[0];
    if raw.contains('|') || raw.contains('>') || raw.contains('<') {
        return None;
    }
    let tokens = shell_tokens(raw);
    let Some((i, cmd)) = first_shell_cmd(&tokens) else {
        return None;
    };
    let args = tokens.get(i + 1..).unwrap_or(&[]);
    match cmd_basename(cmd) {
        "cat" | "head" | "tail" | "more" | "less" | "bat" | "nl" => single_dump_file(args),
        "sed" | "gsed" => sed_n_print_path(args),
        _ => None,
    }
}

fn single_dump_file(args: &[String]) -> Option<String> {
    let mut files = Vec::new();
    let mut j = 0;
    while j < args.len() {
        let a = args[j].as_str();
        if a == "--" {
            files.extend(args.get(j + 1..).unwrap_or(&[]).iter().cloned());
            break;
        }
        if a.starts_with('-') {
            if matches!(a, "-n" | "-c" | "-q" | "-v" | "--lines" | "--bytes") {
                j = j.saturating_add(2);
                continue;
            }
            j += 1;
            continue;
        }
        files.push(a.to_string());
        j += 1;
    }
    one_dump_path(files)
}

fn one_dump_path(mut files: Vec<String>) -> Option<String> {
    if files.len() != 1 {
        return None;
    }
    let p = files.remove(0);
    if p == "-" || p.is_empty() {
        return None;
    }
    Some(p)
}

/// `sed -n '1,40p' file` / `sed -n -e '20p' file`. In-place and `s///` stay Keep.
fn sed_n_print_path(args: &[String]) -> Option<String> {
    let mut quiet = false;
    let mut print_script = false;
    let mut files = Vec::new();
    let mut j = 0;
    while j < args.len() {
        let a = args[j].as_str();
        if a == "--" {
            files.extend(args.get(j + 1..).unwrap_or(&[]).iter().cloned());
            break;
        }
        if a == "-i" || a == "--in-place" || (a.starts_with("-i") && a != "-n" && a != "-e") {
            return None;
        }
        if a == "-n" || a == "--quiet" || a == "--silent" {
            quiet = true;
            j += 1;
            continue;
        }
        if a == "-e" || a == "--expression" {
            let Some(script) = args.get(j + 1) else {
                return None;
            };
            if !is_sed_print_script(script) {
                return None;
            }
            print_script = true;
            j += 2;
            continue;
        }
        if a == "-f" || a == "--file" || a.starts_with('-') {
            return None;
        }
        if !print_script && is_sed_print_script(a) {
            print_script = true;
            j += 1;
            continue;
        }
        files.push(a.to_string());
        j += 1;
    }
    if !quiet || !print_script {
        return None;
    }
    one_dump_path(files)
}

fn is_sed_print_script(raw: &str) -> bool {
    let t = raw
        .trim()
        .trim_matches('\'')
        .trim_matches('"')
        .trim()
        .trim_end_matches(';')
        .trim();
    let Some(range) = t.strip_suffix('p').or_else(|| t.strip_suffix('P')) else {
        return false;
    };
    if range.is_empty() {
        return false;
    }
    let parts: Vec<&str> = range.split(',').collect();
    (1..=2).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn path_is_workspace_root(raw: &str, ws_root: &Path, cwd: &Path) -> bool {
    let t = raw.trim();
    if t.is_empty() || t == "." || t == "./" || t == ".\\" {
        return paths_same(cwd, ws_root);
    }
    let p = if Path::new(t).is_absolute() {
        PathBuf::from(t)
    } else {
        cwd.join(t)
    };
    paths_same(&p, ws_root)
}

fn paths_same(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn find_is_workspace_dump(args: &[String], ws_root: &Path, cwd: &Path) -> bool {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if matches!(a, "-H" | "-L" | "-P") {
            i += 1;
            continue;
        }
        if a == "-D" || a == "-O" {
            i = i.saturating_add(2);
            continue;
        }
        if a.starts_with("-O") && a.len() > 2 {
            i += 1;
            continue;
        }
        break;
    }
    let mut paths = Vec::new();
    while i < args.len() {
        let a = args[i].as_str();
        if a.starts_with('-') || a == "!" || a == "(" || a == ")" {
            break;
        }
        paths.push(a);
        i += 1;
    }
    if paths.is_empty() {
        paths.push(".");
    }
    if !paths
        .iter()
        .any(|p| path_is_workspace_root(p, ws_root, cwd))
    {
        return false;
    }
    find_expr_is_broad(&args.get(i..).unwrap_or(&[]))
}

fn find_expr_is_broad(expr: &[String]) -> bool {
    if expr.is_empty() {
        return true;
    }
    const NAME: &[&str] = &[
        "-name",
        "-iname",
        "-lname",
        "-ilname",
        "-path",
        "-ipath",
        "-wholename",
        "-iwholename",
        "-regex",
        "-iregex",
    ];
    let mut i = 0;
    let mut saw_specific = false;
    while i < expr.len() {
        let a = expr[i].as_str();
        if NAME.contains(&a) {
            let val = expr.get(i + 1).map(|s| s.as_str()).unwrap_or("*");
            if name_is_star_glob(val) {
                return true;
            }
            saw_specific = true;
            i = i.saturating_add(2);
            continue;
        }
        i += 1;
    }
    !saw_specific
}

fn name_is_star_glob(val: &str) -> bool {
    super::is_unfiltered_tree_glob(val)
}

fn ls_is_recursive_root(args: &[String], ws_root: &Path, cwd: &Path) -> bool {
    let mut recursive = false;
    let mut paths = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            paths.extend(args.get(i + 1..).unwrap_or(&[]).iter().map(|s| s.as_str()));
            break;
        }
        if a == "--recursive" {
            recursive = true;
            i += 1;
            continue;
        }
        if a.starts_with("--") {
            i += 1;
            continue;
        }
        if a.starts_with('-') && a.chars().any(|c| c == 'R') {
            recursive = true;
            i += 1;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        paths.push(a);
        i += 1;
    }
    if !recursive {
        return false;
    }
    if paths.is_empty() {
        return path_is_workspace_root(".", ws_root, cwd);
    }
    paths
        .iter()
        .any(|p| path_is_workspace_root(p, ws_root, cwd))
}

fn tree_is_workspace_root(args: &[String], ws_root: &Path, cwd: &Path) -> bool {
    let mut i = 0;
    let mut paths = Vec::new();
    while i < args.len() {
        let a = args[i].as_str();
        if matches!(a, "-L" | "-I" | "--ignore" | "-H" | "-o" | "--output") {
            i = i.saturating_add(2);
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        paths.push(a);
        i += 1;
    }
    if paths.is_empty() {
        return path_is_workspace_root(".", ws_root, cwd);
    }
    paths
        .iter()
        .any(|p| path_is_workspace_root(p, ws_root, cwd))
}

fn git_is_whole_tree_index_dump(tokens: &[String]) -> bool {
    let Some((sub, args)) = git_subcommand(tokens) else {
        return false;
    };
    matches!(sub, "ls-files" | "ls-tree") && git_index_dump_without_path(args)
}

fn git_index_dump_without_path(args: &[String]) -> bool {
    let mut paths = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            paths.extend(args.get(i + 1..).unwrap_or(&[]).iter().map(|s| s.as_str()));
            break;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        paths.push(a);
        i += 1;
    }
    if paths.is_empty() {
        return true;
    }
    paths.iter().all(|p| pathspec_is_tree_star(p))
}

fn pathspec_is_tree_star(raw: &str) -> bool {
    super::is_unfiltered_tree_glob(raw)
}

fn fd_is_workspace_dump(args: &[String], ws_root: &Path, cwd: &Path) -> bool {
    let mut pattern: Option<&str> = None;
    let mut paths = Vec::new();
    let mut narrowing = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            let rest = args.get(i + 1..).unwrap_or(&[]);
            if pattern.is_none() && !rest.is_empty() {
                pattern = Some(rest[0].as_str());
                paths.extend(rest.get(1..).unwrap_or(&[]).iter().map(|s| s.as_str()));
            } else {
                paths.extend(rest.iter().map(|s| s.as_str()));
            }
            break;
        }
        if matches!(a, "-e" | "--extension") {
            narrowing = true;
            i = i.saturating_add(2);
            continue;
        }
        if matches!(a, "-g" | "--glob") {
            let g = args.get(i + 1).map(|s| s.as_str()).unwrap_or("*");
            if !super::is_unfiltered_tree_glob(g) {
                narrowing = true;
            }
            i = i.saturating_add(2);
            continue;
        }
        if fd_flag_takes_value(a) {
            i = i.saturating_add(2);
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        if pattern.is_none() {
            pattern = Some(a);
        } else {
            paths.push(a);
        }
        i += 1;
    }
    let root = paths.is_empty()
        || paths
            .iter()
            .any(|p| path_is_workspace_root(p, ws_root, cwd));
    if !root || narrowing {
        return false;
    }
    match pattern {
        None => true,
        Some(p) => p == "." || p == "./" || name_is_star_glob(p),
    }
}

fn fd_flag_takes_value(a: &str) -> bool {
    matches!(
        a,
        "-d" | "--max-depth"
            | "-e"
            | "--extension"
            | "-E"
            | "--exclude"
            | "-t"
            | "--type"
            | "-S"
            | "--size"
            | "-x"
            | "--exec"
            | "-X"
            | "--exec-batch"
            | "-j"
            | "--threads"
            | "-g"
            | "--glob"
            | "--changed-within"
            | "--changed-before"
            | "--max-results"
            | "--search-path"
            | "--ignore-file"
    )
}

fn rg_files_is_root_dump(args: &[String], ws_root: &Path, cwd: &Path) -> bool {
    if !args.iter().any(|a| a == "--files") {
        return false;
    }
    let mut paths = Vec::new();
    let mut narrowing = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            paths.extend(args.get(i + 1..).unwrap_or(&[]).iter().map(|s| s.as_str()));
            break;
        }
        if matches!(a, "-t" | "--type" | "-T" | "--type-not") {
            narrowing = true;
            i = i.saturating_add(2);
            continue;
        }
        if matches!(a, "-g" | "--glob") {
            let g = args.get(i + 1).map(|s| s.as_str()).unwrap_or("*");
            if !super::is_unfiltered_tree_glob(g) && !g.starts_with('!') {
                narrowing = true;
            }
            i = i.saturating_add(2);
            continue;
        }
        if rg_flag_takes_value(a) {
            i = i.saturating_add(2);
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        paths.push(a);
        i += 1;
    }
    if narrowing {
        return false;
    }
    paths.is_empty()
        || paths
            .iter()
            .any(|p| path_is_workspace_root(p, ws_root, cwd))
}

fn rg_flag_takes_value(a: &str) -> bool {
    matches!(
        a,
        "-g" | "--glob"
            | "-t"
            | "--type"
            | "-T"
            | "--type-not"
            | "-A"
            | "--after-context"
            | "-B"
            | "--before-context"
            | "-C"
            | "--context"
            | "-m"
            | "--max-count"
            | "-j"
            | "--threads"
            | "--max-filesize"
            | "--max-depth"
            | "-f"
            | "--file"
            | "-e"
            | "--regexp"
    )
}

fn git_dump_without_path(args: &[String], ignore_compact: bool) -> bool {
    const COMPACT: &[&str] = &[
        "--stat",
        "--shortstat",
        "--numstat",
        "--name-only",
        "--name-status",
        "--check",
        "--dirstat",
        "--summary",
        "--raw",
        "--no-patch",
        "-s",
    ];
    if !ignore_compact && args.iter().any(|a| COMPACT.contains(&a.as_str())) {
        return false;
    }
    let mut after_dd = false;
    for a in args {
        if a == "--" {
            after_dd = true;
            continue;
        }
        if after_dd {
            return false;
        }
        if a.starts_with('-') {
            continue;
        }
        if looks_like_git_rev(a) {
            continue;
        }
        return false;
    }
    true
}

fn git_subcommand(tokens: &[String]) -> Option<(&str, &[String])> {
    let mut i = 0;
    while i < tokens.len() && tokens[i].contains('=') && !tokens[i].starts_with('-') {
        i += 1;
    }
    if tokens.get(i).map(|s| s.as_str()) != Some("git") {
        return None;
    }
    i += 1;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        if t == "-C" || t == "-c" {
            i = i.saturating_add(2);
            continue;
        }
        if t.starts_with('-') {
            i += 1;
            continue;
        }
        return Some((t, tokens.get(i + 1..).unwrap_or(&[])));
    }
    None
}

fn looks_like_git_rev(tok: &str) -> bool {
    let t = tok.trim();
    if t.is_empty() {
        return false;
    }
    if matches!(
        t,
        "HEAD" | "@" | "FETCH_HEAD" | "ORIG_HEAD" | "MERGE_HEAD" | "CHERRY_PICK_HEAD"
    ) {
        return true;
    }
    if t.contains(':') {
        return false;
    }
    if t.starts_with("HEAD") || t.starts_with('@') || t.starts_with("refs/") {
        return true;
    }
    if t.contains("..") {
        return true;
    }
    let hex = t.chars().all(|c| c.is_ascii_hexdigit());
    hex && t.len() >= 7 && t.len() <= 40
}

fn shell_tokens(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c.is_whitespace() || matches!(c, '|' | ';' | '&' | '<' | '>') => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                if !c.is_whitespace() {
                    break;
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn spawn_shell(command: &str, cwd: &Path) -> std::io::Result<tokio::process::Child> {
    let shell = SHELL.get_or_init(detect_shell);
    let mut cmd = Command::new(&shell.exe);
    match shell.kind {
        ShellKind::Bash => {
            // Keep one command dialect on macOS, Linux, and Windows/Git Bash.
            // Skipping profiles makes every tool call deterministic and cheap.
            cmd.args(["--noprofile", "--norc", "-c", command]);
        }
        ShellKind::Posix => {
            cmd.args(["-c", command]);
        }
        ShellKind::PowerShell => {
            cmd.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                command,
            ]);
        }
    }
    cmd.current_dir(cwd)
        .env("PATH", tool_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // 独立进程组：pgid == 子 shell pid，取消时 kill(-pgid) 连孙进程一起杀。
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    crate::proc_spawn::hide_window_async(&mut cmd);
    cmd.spawn()
}

#[cfg_attr(not(test), allow(dead_code))]
fn resolved_block_until(args: &serde_json::Value) -> Option<Duration> {
    arg_u32(args, "block_until_ms")
        .or_else(|| arg_u32(args, "timeout"))
        .or_else(|| arg_u32(args, "timeout_ms"))
        .filter(|n| *n > 0)
        .map(|ms| Duration::from_millis(ms as u64))
}

/// Bash runs `--noprofile --norc`, so rustup's shell hook never runs.
/// GUI/Electron PATH is often just `/usr/bin:/bin`, which hides `~/.cargo/bin`.
fn tool_path() -> OsString {
    merge_tool_path(
        std::env::var_os("PATH"),
        crate::config::user_home().as_deref(),
        &extra_path_dirs(),
    )
}

fn extra_path_dirs() -> Vec<PathBuf> {
    static DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut dirs = Vec::new();
        if let Ok(raw) = std::env::var("HYPER_PATH") {
            for part in split_path_list(&raw) {
                if let Some(p) = expand_dir(&part) {
                    dirs.push(p);
                }
            }
        }
        for raw in crate::config::Config::load_file_or_default()
            .tools
            .extra_path
        {
            if let Some(p) = expand_dir(&raw) {
                dirs.push(p);
            }
        }
        dirs
    })
    .clone()
}

fn split_path_list(raw: &str) -> Vec<String> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    raw.split(sep)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn expand_dir(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw == "~" {
        return crate::config::user_home();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Some(crate::config::user_home()?.join(rest));
    }
    #[cfg(windows)]
    if let Some(rest) = raw.strip_prefix("~\\") {
        return Some(crate::config::user_home()?.join(rest));
    }
    Some(PathBuf::from(raw))
}

fn well_known_bins(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home {
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join("go/bin"));
        #[cfg(windows)]
        {
            dirs.push(home.join("scoop/shims"));
            dirs.push(home.join("AppData/Roaming/npm"));
            dirs.push(home.join("AppData/Local/Microsoft/WinGet/Links"));
        }
    }
    #[cfg(unix)]
    {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/opt/homebrew/sbin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/usr/local/sbin"));
    }
    #[cfg(windows)]
    {
        for key in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(base) = std::env::var_os(key) {
                let base = PathBuf::from(base);
                dirs.push(base.join("Git/cmd"));
                dirs.push(base.join("Git/bin"));
                dirs.push(base.join("nodejs"));
            }
        }
        if let Some(data) = std::env::var_os("ProgramData") {
            dirs.push(PathBuf::from(data).join("chocolatey/bin"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let local = PathBuf::from(local);
            dirs.push(local.join("Microsoft/WinGet/Links"));
            dirs.push(local.join("Programs/Git/cmd"));
        }
    }
    dirs
}

fn merge_tool_path(current: Option<OsString>, home: Option<&Path>, extra: &[PathBuf]) -> OsString {
    let mut ordered = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let push = |dir: PathBuf,
                ordered: &mut Vec<PathBuf>,
                seen: &mut std::collections::HashSet<PathBuf>| {
        if dir.as_os_str().is_empty() || !dir.is_dir() {
            return;
        }
        if seen.insert(dir.clone()) {
            ordered.push(dir);
        }
    };
    for dir in extra {
        push(dir.clone(), &mut ordered, &mut seen);
    }
    for dir in well_known_bins(home) {
        push(dir, &mut ordered, &mut seen);
    }
    if let Some(ref current) = current {
        for dir in std::env::split_paths(current) {
            push(dir, &mut ordered, &mut seen);
        }
    }
    std::env::join_paths(&ordered).unwrap_or_else(|_| current.unwrap_or_default())
}

fn detect_shell() -> ShellSpec {
    if let Some(exe) = std::env::var_os("HYPER_SHELL").filter(|s| !s.is_empty()) {
        let exe = PathBuf::from(exe);
        let name = exe
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        return ShellSpec {
            exe,
            kind: if name.contains("powershell") || name.starts_with("pwsh") {
                ShellKind::PowerShell
            } else if name.starts_with("bash") {
                ShellKind::Bash
            } else {
                ShellKind::Posix
            },
        };
    }

    #[cfg(windows)]
    {
        // Git for Windows is already present on most developer machines and
        // gives the model the same learned command language as macOS/Linux.
        let mut candidates = Vec::new();
        for key in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(key) {
                let base = PathBuf::from(base);
                candidates.push(base.join("Git/bin/bash.exe"));
                candidates.push(base.join("Programs/Git/bin/bash.exe"));
            }
        }
        if let Some(path_bash) = find_in_path("bash.exe", true) {
            candidates.push(path_bash);
        }
        if let Some(exe) = candidates.into_iter().find(|p| p.is_file()) {
            return ShellSpec {
                exe,
                kind: ShellKind::Bash,
            };
        }
        for name in ["pwsh.exe", "powershell.exe"] {
            if let Some(exe) = find_in_path(name, false) {
                return ShellSpec {
                    exe,
                    kind: ShellKind::PowerShell,
                };
            }
        }
        ShellSpec {
            exe: PathBuf::from("powershell.exe"),
            kind: ShellKind::PowerShell,
        }
    }

    #[cfg(not(windows))]
    {
        for exe in [PathBuf::from("/bin/bash"), PathBuf::from("/usr/bin/bash")] {
            if exe.is_file() {
                return ShellSpec {
                    exe,
                    kind: ShellKind::Bash,
                };
            }
        }
        if let Some(exe) = [PathBuf::from("/bin/sh"), PathBuf::from("/usr/bin/sh")]
            .into_iter()
            .find(|p| p.is_file())
        {
            ShellSpec {
                exe,
                kind: ShellKind::Posix,
            }
        } else {
            ShellSpec {
                exe: PathBuf::from("bash"),
                kind: ShellKind::Bash,
            }
        }
    }
}

#[cfg(windows)]
fn find_in_path(name: &str, git_only: bool) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|p| {
            p.is_file() && (!git_only || p.to_string_lossy().to_ascii_lowercase().contains("git"))
        })
}

/// 取消路径的整组击杀。`setpgid(0,0)` 保证 pgid 就是子 shell 的 pid。
fn kill_group(child: &tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// 限时收尾读：正常等 EOF，超时（孙进程仍握着写端）就 abort 放弃，
/// 共享缓冲里已读到的部分照常返回。
async fn drain(task: Option<tokio::task::JoinHandle<()>>) {
    let Some(mut task) = task else {
        return;
    };
    if tokio::time::timeout(DRAIN_TIMEOUT, &mut task)
        .await
        .is_err()
    {
        task.abort();
        // Await cancellation so the pipe handle is actually closed before
        // returning; otherwise a Windows grandchild can keep the test/process
        // alive until its own timeout even though the tool call already ended.
        let _ = task.await;
    }
}

fn take_text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    let b = buf.lock().unwrap_or_else(|e| e.into_inner());
    String::from_utf8_lossy(&b).into_owned()
}

fn format_shell(code: i32, stdout: &str, stderr: &str) -> String {
    if code == 0 {
        let mut text = if stdout.is_empty() {
            "Command executed successfully (no output).".to_string()
        } else {
            stdout.to_string()
        };
        if !stderr.is_empty() {
            text.push_str("\n[stderr]\n");
            text.push_str(stderr);
        }
        text
    } else {
        let mut parts = vec![format!("Command failed with exit code {code}.")];
        if !stdout.is_empty() {
            parts.push(format!("\n[stdout]\n{stdout}"));
        }
        if !stderr.is_empty() {
            parts.push(format!("\n[stderr]\n{stderr}"));
        }
        parts.concat()
    }
}

async fn read_capped_into<R: AsyncRead + Unpin>(mut pipe: R, buf: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                let mut b = buf.lock().unwrap_or_else(|e| e.into_inner());
                let room = OUTPUT_MAX_BYTES.saturating_sub(b.len());
                if room > 0 {
                    b.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_calls::ToolCall;
    use serde_json::json;
    use std::path::PathBuf;
    use tokio::io::AsyncWriteExt;

    fn scratch() -> (Workspace, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("hyper-bash-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = Workspace::open(&dir, true).unwrap();
        (w, dir)
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: "bash".into(),
            arguments: args,
        }
    }

    #[test]
    fn whole_tree_git_diff_is_skipped() {
        assert_eq!(
            rewrite_skip_whole_tree_git_diff("git diff"),
            GitDiffRewrite::SkipAll
        );
        assert_eq!(
            rewrite_skip_whole_tree_git_diff("git diff HEAD"),
            GitDiffRewrite::SkipAll
        );
        assert_eq!(
            rewrite_skip_whole_tree_git_diff("git diff --cached"),
            GitDiffRewrite::SkipAll
        );
        assert_eq!(
            rewrite_skip_whole_tree_git_diff(
                "git status && git log -8 --oneline && git diff --stat HEAD && git diff HEAD"
            ),
            GitDiffRewrite::Rest(
                "git status && git log -8 --oneline && git diff --stat HEAD".into()
            )
        );
        assert_eq!(
            rewrite_skip_whole_tree_git_diff("git diff --stat HEAD"),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_git_diff("git diff HEAD -- crates/hyper-loop/src/sticky.rs"),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_git_diff("git diff crates/hyper-loop/src/sticky.rs"),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_git_diff("echo hi"),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_git_diff("git show HEAD"),
            GitDiffRewrite::SkipAll
        );
        assert_eq!(
            rewrite_skip_whole_tree_git_diff("git show HEAD:crates/hyper-loop/src/sticky.rs"),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_git_diff("git log -8 --oneline"),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_git_diff("git log -p"),
            GitDiffRewrite::SkipAll
        );
        assert_eq!(
            rewrite_skip_whole_tree_git_diff("git log -p -- crates/hyper-loop/src/sticky.rs"),
            GitDiffRewrite::Keep
        );
    }

    #[tokio::test]
    async fn whole_tree_git_diff_does_not_spawn() {
        let (ws, dir) = scratch();
        let started = std::time::Instant::now();
        let out = bash(
            &ws,
            &call(json!({"command": "git diff HEAD"})),
            CancelFlag::new(),
            ToolLimits::default(),
            None,
        )
        .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "whole-tree git diff must not run: {:?}",
            started.elapsed()
        );
        assert_eq!(out.state, ToolState::Success, "{}", out.joined_text());
        let text = out.joined_text();
        assert!(text.contains("Whole-tree"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn whole_tree_listing_is_skipped() {
        let ws = Path::new("/ws");
        let cwd = Path::new("/ws");
        let sub = Path::new("/ws/crates");
        assert_eq!(
            rewrite_skip_whole_tree_listing("find .", ws, cwd),
            GitDiffRewrite::SkipAll
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("find . -name '*.rs'", ws, cwd),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("find . -name sticky.rs", ws, cwd),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("find crates/hyper-loop -name '*.rs'", ws, cwd),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("find .", ws, sub),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("find /ws", ws, sub),
            GitDiffRewrite::SkipAll
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("ls -R", ws, cwd),
            GitDiffRewrite::SkipAll
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("ls -la", ws, cwd),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("ls -R crates", ws, cwd),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("tree", ws, cwd),
            GitDiffRewrite::SkipAll
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("tree crates/foo", ws, cwd),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("echo hi && find .", ws, cwd),
            GitDiffRewrite::Rest("echo hi".into())
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("git ls-files", ws, cwd),
            GitDiffRewrite::SkipAll
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("git ls-files '*.rs'", ws, cwd),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("git ls-files crates/hyper-loop", ws, cwd),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("fd", ws, cwd),
            GitDiffRewrite::SkipAll
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("fd -e rs", ws, cwd),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("fd TREE_LIST_HINT", ws, cwd),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("fd . crates/hyper-loop", ws, cwd),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("rg --files", ws, cwd),
            GitDiffRewrite::SkipAll
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("rg --files crates/hyper-loop", ws, cwd),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("rg --files -t rust", ws, cwd),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("rg --files -g '*.rs'", ws, cwd),
            GitDiffRewrite::Keep
        );
        assert_eq!(
            rewrite_skip_whole_tree_listing("rg TREE_LIST_HINT", ws, cwd),
            GitDiffRewrite::Keep
        );
    }

    #[test]
    fn cat_like_path_detects_simple_dumps() {
        assert_eq!(
            cat_like_path("cat src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            cat_like_path("head -n 20 src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            cat_like_path("tail -20 src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        assert!(cat_like_path("cat src/lib.rs | wc").is_none());
        assert!(cat_like_path("cat a.rs b.rs").is_none());
        assert!(cat_like_path("python3 r92_test.py").is_none());
        assert!(cat_like_path("cat src/lib.rs && echo ok").is_none());
        assert_eq!(
            cat_like_path("nl -ba src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            cat_like_path("sed -n '1,40p' src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            cat_like_path("sed -n -e '20p' src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        assert!(cat_like_path("sed -i -n '1p' src/lib.rs").is_none());
        assert!(cat_like_path("sed -n 's/foo/bar/p' src/lib.rs").is_none());
        assert!(cat_like_path("sed '1,20p' src/lib.rs").is_none());
    }

    #[tokio::test]
    async fn whole_tree_find_does_not_spawn() {
        let (ws, dir) = scratch();
        let started = std::time::Instant::now();
        let out = bash(
            &ws,
            &call(json!({"command": "find . -exec sleep 30 {} +"})),
            CancelFlag::new(),
            ToolLimits::default(),
            None,
        )
        .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "workspace-root find must not run: {:?}",
            started.elapsed()
        );
        assert_eq!(out.state, ToolState::Success, "{}", out.joined_text());
        let text = out.joined_text();
        assert!(text.contains("`find`"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_timeout_does_not_hard_kill() {
        assert!(resolved_block_until(&json!({})).is_none());
        assert!(resolved_block_until(&json!({"block_until_ms": 0})).is_none());
    }

    #[test]
    fn explicit_block_until_ms_wins() {
        assert_eq!(
            resolved_block_until(&json!({"block_until_ms": 50})),
            Some(std::time::Duration::from_millis(50))
        );
        assert_eq!(
            resolved_block_until(&json!({"timeout_ms": 80})),
            Some(std::time::Duration::from_millis(80))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn block_until_ms_does_not_hard_kill() {
        let (ws, dir) = scratch();
        let started = std::time::Instant::now();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            bash(
                &ws,
                &call(json!({
                    "command": "sleep 0.25; echo survived",
                    "block_until_ms": 50
                })),
                CancelFlag::new(),
                ToolLimits::default(),
                None,
            ),
        )
        .await
        .expect("bash hung");
        assert_eq!(out.state, ToolState::Success, "{}", out.joined_text());
        assert!(
            out.joined_text().contains("survived"),
            "{}",
            out.joined_text()
        );
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(200),
            "inner bash returned before sleep finished: {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn read_capped_drains_past_cap_to_eof() {
        let (mut writer, reader) = tokio::io::duplex(8192);
        let buf: Arc<Mutex<Vec<u8>>> = Arc::default();
        let reader_task = tokio::spawn(read_capped_into(reader, buf.clone()));
        let writer_task = tokio::spawn(async move {
            let first = vec![b'a'; OUTPUT_MAX_BYTES + 256 * 1024];
            writer.write_all(&first).await?;
            writer.write_all(b"TAIL").await?;
            Ok::<_, std::io::Error>(())
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), reader_task)
            .await
            .expect("read_capped hung")
            .expect("join");
        writer_task
            .await
            .expect("writer join")
            .expect("follow-on write must complete (pipe drained to EOF)");
        let out = take_text(&buf);
        assert_eq!(out.len(), OUTPUT_MAX_BYTES);
        assert!(out.starts_with("aaaa"));
        assert!(!out.contains("TAIL"));
    }

    #[tokio::test]
    async fn background_grandchild_does_not_hang_bash() {
        // 孙进程继承 stdout 写端：shell 退出后收尾读限时放弃，
        // 不能等 sleep 30 结束才返回。
        let (ws, dir) = scratch();
        let started = std::time::Instant::now();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            bash(
                &ws,
                &call(json!({"command": "sleep 30 & echo hi"})),
                CancelFlag::new(),
                ToolLimits::default(),
                None,
            ),
        )
        .await
        .expect("bash hung on background grandchild");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(8),
            "took {:?}",
            started.elapsed()
        );
        assert_eq!(out.state, ToolState::Success, "{}", out.joined_text());
        assert!(out.joined_text().contains("hi"), "{}", out.joined_text());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_kills_grandchild_via_process_group() {
        let (ws, dir) = scratch();
        let cancel = CancelFlag::new();
        let pid_file = dir.join("pid.txt");
        // shell 把孙进程 pid 写盘后 wait 挂住，等 cancel 杀整组。
        let cmd = "sleep 30 & echo $! > pid.txt; wait".to_string();
        let task = {
            let ws = ws.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                bash(
                    &ws,
                    &call(json!({"command": cmd})),
                    cancel,
                    ToolLimits::default(),
                    None,
                )
                .await
            })
        };
        // 等孙进程 pid 落盘再取消。
        let mut pid: Option<i32> = None;
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&pid_file) {
                if let Ok(p) = s.trim().parse::<i32>() {
                    pid = Some(p);
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let pid = pid.expect("grandchild pid never appeared");
        cancel.cancel();
        let out = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("cancelled bash hung")
            .expect("join");
        assert_eq!(out.state, ToolState::Interrupted, "{}", out.joined_text());
        // kill(pid, 0) 返回 -1/ESRCH 即孙进程已死；收养/收尸留点余量。
        let mut dead = false;
        for _ in 0..100 {
            if unsafe { libc::kill(pid, 0) } == -1 {
                dead = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(dead, "grandchild {pid} survived the group kill");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bash_nonzero_exit_is_error() {
        let (ws, dir) = scratch();
        let out = bash(
            &ws,
            &call(json!({"command": "echo hello >&2; echo out; exit 1"})),
            CancelFlag::new(),
            ToolLimits::default(),
            None,
        )
        .await;
        assert_eq!(out.state, ToolState::Error, "{}", out.joined_text());
        let text = out.joined_text();
        assert!(text.contains("exit code 1"), "{text}");
        assert!(text.contains("out"), "{text}");
        assert!(text.contains("hello"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bash_large_stdout_returns_prefix_without_hang() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("big.txt"), "a".repeat(2_000_000)).unwrap();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            bash(
                &ws,
                &call(json!({"command": "cat big.txt"})),
                CancelFlag::new(),
                ToolLimits::default(),
                None,
            ),
        )
        .await
        .expect("bash hung on large stdout");
        assert_eq!(out.state, ToolState::Success, "{}", out.joined_text());
        let live = out.joined_text();
        assert!(live.contains("aaa"), "{live}");
        assert!(!live.contains("Command failed"), "{live}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tool_path_prepends_existing_cargo_bin() {
        let home =
            std::env::temp_dir().join(format!("hyper-path-{}", uuid::Uuid::new_v4().simple()));
        let cargo_bin = home.join(".cargo/bin");
        std::fs::create_dir_all(&cargo_bin).unwrap();
        let merged = merge_tool_path(Some("/usr/bin".into()), Some(home.as_path()), &[]);
        let dirs: Vec<_> = std::env::split_paths(&merged).collect();
        assert_eq!(
            dirs.first().map(PathBuf::as_path),
            Some(cargo_bin.as_path())
        );
        assert!(dirs.iter().any(|d| d == Path::new("/usr/bin")));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn tool_path_skips_missing_well_known_dirs() {
        let home = std::env::temp_dir().join(format!(
            "hyper-path-missing-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let merged = merge_tool_path(Some("/usr/bin".into()), Some(home.as_path()), &[]);
        let dirs: Vec<_> = std::env::split_paths(&merged).collect();
        assert!(!dirs.iter().any(|d| d.ends_with(".cargo/bin")));
        assert!(dirs.iter().any(|d| d == Path::new("/usr/bin")));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn tool_path_extra_dirs_win_over_well_known() {
        let root = std::env::temp_dir().join(format!(
            "hyper-path-extra-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let extra = root.join("extra");
        let cargo_bin = root.join("home/.cargo/bin");
        std::fs::create_dir_all(&extra).unwrap();
        std::fs::create_dir_all(&cargo_bin).unwrap();
        let merged = merge_tool_path(
            Some("/usr/bin".into()),
            Some(&root.join("home")),
            &[extra.clone()],
        );
        let dirs: Vec<_> = std::env::split_paths(&merged).collect();
        assert_eq!(dirs.first().map(PathBuf::as_path), Some(extra.as_path()));
        assert_eq!(dirs.get(1).map(PathBuf::as_path), Some(cargo_bin.as_path()));
        let _ = std::fs::remove_dir_all(root);
    }
}
