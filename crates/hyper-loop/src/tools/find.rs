//! `Glob` / `Grep`. Grep is ripgrep (Cursor); Glob walks the workspace.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

use super::{arg_path, arg_str, arg_u32, folded_response, BlobStore, ToolLimits, Workspace};
use crate::tool_calls::{ToolCall, ToolResponse, ToolState};

pub(crate) const SKIP_DIR: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    "third_party",
    "__pycache__",
    ".venv",
    "venv",
    "blobs",
    "AppData",
    "Application Data",
    "Local Settings",
    "Library",
    "Caches",
    "OneDrive",
];

/// Session JSONL / blob archives under the workspace overlay. Overnight scripts,
/// todos, and cron live beside these and must stay Glob/Grep-visible.
const HYPER_SKIP_DIR: &[&str] = &["sessions", "blobs", "doc-cache"];

const GLOB_CAP: usize = 200;
const GREP_CAP: usize = 80;

/// Unfiltered any-file globs. `**/*.rs` / `*.{rs,md}` / `**/AGENT.md` are not.
pub(crate) fn is_unfiltered_tree_glob(pattern: &str) -> bool {
    let p = pattern
        .trim()
        .trim_matches('\'')
        .trim_matches('"')
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string();
    matches!(p.as_str(), "*" | "*.*" | "**" | "**/*" | "**/*.*")
}

const MAX_FILE_BYTES: u64 = 1_048_576;
/// Fallback Glob/Grep walk cap. Windows home/Documents as workspace used to
/// freeze the hop when `rg` was missing from PATH.
const WALK_BUDGET: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, PartialEq, Eq)]
enum WalkEnd {
    Done,
    Capped,
    Budget,
}

pub fn glob_files(ws: &Workspace, call: &ToolCall, limits: ToolLimits) -> ToolResponse {
    let Some(pattern) = arg_str(&call.arguments, "glob_pattern")
        .or_else(|| arg_str(&call.arguments, "pattern"))
        .or_else(|| arg_str(&call.arguments, "glob"))
    else {
        return ToolResponse::text(
            &call.id,
            "Error: No `glob_pattern` provided.",
            ToolState::Error,
        );
    };
    let root = match glob_root(ws, &call.arguments) {
        Ok(p) => p,
        Err(e) => return ToolResponse::text(&call.id, e, ToolState::Error),
    };
    let matcher = match GlobMatcher::new(&pattern) {
        Ok(m) => m,
        Err(e) => {
            return ToolResponse::text(
                &call.id,
                format!("Error: invalid glob_pattern: {e}"),
                ToolState::Error,
            );
        }
    };
    let mut hits = Vec::new();
    let end = walk_files(&root, ws.root(), &mut |rel, _abs| {
        if matcher.matches(rel) {
            hits.push(rel.to_string());
        }
        hits.len() < GLOB_CAP
    });
    hits.sort();
    let n = hits.len();
    let mut text = if hits.is_empty() {
        format!("No files matching `{pattern}`.")
    } else {
        hits.join("\n")
    };
    if n >= GLOB_CAP {
        text.push_str(&format!(
            "\n… truncated at {GLOB_CAP} paths. Narrow the glob or use Grep."
        ));
    } else if end == WalkEnd::Budget {
        text.push_str(&format!(
            "\n… scan stopped after {}s.",
            WALK_BUDGET.as_secs()
        ));
    }
    folded_response(&call.id, text, ToolState::Success, limits, None)
}

pub fn grep_files(
    ws: &Workspace,
    call: &ToolCall,
    limits: ToolLimits,
    blobs: Option<&BlobStore>,
) -> ToolResponse {
    let Some(pattern) =
        arg_str(&call.arguments, "pattern").or_else(|| arg_str(&call.arguments, "query"))
    else {
        return ToolResponse::text(&call.id, "Error: No `pattern` provided.", ToolState::Error);
    };
    let root = match grep_root(ws, &call.arguments) {
        Ok(p) => p,
        Err(e) => return ToolResponse::text(&call.id, e, ToolState::Error),
    };
    if root.is_file() {
        let rel = arg_path(&call.arguments)
            .map(|raw| ws.shown(&raw))
            .unwrap_or_else(|| {
                root.strip_prefix(ws.root())
                    .unwrap_or(&root)
                    .to_string_lossy()
                    .replace('\\', "/")
            });
        if super::doc::is_legacy_office(&rel) {
            return ToolResponse::text(&call.id, super::doc::legacy_error(&rel), ToolState::Error);
        }
        if super::doc::is_doc_path(&rel) {
            let ignore_case = grep_ignore_case(&call.arguments);
            let re = match compile_pattern(&pattern, ignore_case) {
                Ok(r) => r,
                Err(e) => {
                    return ToolResponse::text(
                        &call.id,
                        format!("Error: invalid pattern: {e}"),
                        ToolState::Error,
                    );
                }
            };
            let cap = grep_head_limit(&call.arguments);
            return match super::doc::grep_extracted(ws, &rel, &root, &re, cap) {
                Ok(lines) => {
                    let truncated = lines.len() >= cap;
                    let mut text = if lines.is_empty() {
                        format!("No matches for `{pattern}`.")
                    } else {
                        lines.join("\n")
                    };
                    if truncated {
                        text.push_str(&format!("\n… truncated at {cap} hits."));
                    }
                    folded_response(&call.id, text, ToolState::Success, limits, blobs)
                }
                Err(e) => ToolResponse::text(&call.id, e, ToolState::Error),
            };
        }
    }
    if let Some(text) = grep_ripgrep(ws, &root, &pattern, &call.arguments) {
        return folded_response(&call.id, text, ToolState::Success, limits, blobs);
    }
    grep_walk(
        ws,
        &root,
        &pattern,
        &call.arguments,
        limits,
        blobs,
        &call.id,
    )
}

fn grep_ignore_case(args: &Value) -> bool {
    matches!(
        args.get("-i").or_else(|| args.get("i")),
        Some(Value::Bool(true))
    ) || arg_str(args, "-i")
        .or_else(|| arg_str(args, "i"))
        .is_some_and(|s| s == "true" || s == "1")
}

fn grep_bool(args: &Value, key: &str) -> bool {
    matches!(args.get(key), Some(Value::Bool(true)))
        || arg_str(args, key).is_some_and(|s| s == "true" || s == "1")
}

fn grep_head_limit(args: &Value) -> usize {
    arg_u32(args, "head_limit")
        .unwrap_or(GREP_CAP as u32)
        .min(2000) as usize
}

fn grep_output_mode(args: &Value) -> &str {
    match arg_str(args, "output_mode").as_deref() {
        Some("files_with_matches") => "files_with_matches",
        Some("count") => "count",
        _ => "content",
    }
}

fn grep_ripgrep(ws: &Workspace, root: &Path, pattern: &str, args: &Value) -> Option<String> {
    let mut cmd = Command::new("rg");
    let path = rg_search_path();
    crate::proc_spawn::hide_window(&mut cmd);
    cmd.current_dir(ws.root())
        .env("TERM", "dumb")
        .env("PATH", &path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        cmd.env("Path", &path);
    }
    cmd.arg("--color=never");
    cmd.arg("--hidden");
    cmd.arg("--glob");
    cmd.arg("!.git/**");
    if path_under_hyper_overlay(ws, root) {
        // Overlay is gitignored; otherwise rg never sees overnight scripts.
        cmd.arg("--no-ignore-vcs");
        cmd.arg("--no-ignore-parent");
    }
    if !path_under_dir(ws, root, "third_party") {
        cmd.arg("--glob");
        cmd.arg("!third_party/**");
    }
    match grep_output_mode(args) {
        "files_with_matches" => {
            cmd.arg("-l");
        }
        "count" => {
            cmd.arg("-c");
            cmd.arg("--no-heading");
        }
        _ => {
            cmd.arg("-n");
            cmd.arg("--no-heading");
        }
    }
    if grep_ignore_case(args) {
        cmd.arg("-i");
    }
    if grep_bool(args, "multiline") {
        cmd.arg("--multiline");
        cmd.arg("--multiline-dotall");
    }
    if let Some(n) = arg_u32(args, "-C").or_else(|| arg_u32(args, "C")) {
        cmd.arg("-C").arg(n.to_string());
    } else {
        if let Some(n) = arg_u32(args, "-A").or_else(|| arg_u32(args, "A")) {
            cmd.arg("-A").arg(n.to_string());
        }
        if let Some(n) = arg_u32(args, "-B").or_else(|| arg_u32(args, "B")) {
            cmd.arg("-B").arg(n.to_string());
        }
    }
    if let Some(glob) = arg_str(args, "glob").or_else(|| arg_str(args, "glob_pattern")) {
        cmd.arg("--glob").arg(glob);
    }
    if let Some(ty) = arg_str(args, "type") {
        cmd.arg("--type").arg(ty);
    }
    let cap = grep_head_limit(args);
    cmd.arg("--max-count").arg(cap.to_string());
    cmd.arg("--max-filesize").arg("1M");
    cmd.arg("--").arg(pattern).arg(root);
    let mut child = cmd.spawn().ok()?;
    let started = Instant::now();
    let timeout = Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                let mut buf = Vec::new();
                if let Some(out) = child.stdout.as_mut() {
                    let _ = out.read_to_end(&mut buf);
                }
                let raw = String::from_utf8_lossy(&buf);
                return Some(format_rg_output(
                    ws,
                    pattern,
                    raw.as_ref(),
                    args,
                    st.success(),
                ));
            }
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
}

fn format_rg_output(ws: &Workspace, pattern: &str, raw: &str, args: &Value, ok: bool) -> String {
    let offset = arg_u32(args, "offset").unwrap_or(0) as usize;
    let cap = grep_head_limit(args);
    let mut lines: Vec<String> = raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let p = ws.root();
            let s = p.to_string_lossy();
            if let Some(rest) = l.strip_prefix(s.as_ref()) {
                rest.trim_start_matches(['/', '\\']).replace('\\', "/")
            } else {
                l.replace('\\', "/")
            }
        })
        .collect();
    if offset > 0 {
        if offset >= lines.len() {
            lines.clear();
        } else {
            lines = lines.split_off(offset);
        }
    }
    let truncated = lines.len() > cap;
    if truncated {
        lines.truncate(cap);
    }
    if lines.is_empty() {
        if ok || raw.is_empty() {
            return format!("No matches for `{pattern}`.");
        }
        return format!("No matches for `{pattern}`.");
    }
    let mut text = lines.join("\n");
    if truncated {
        text.push_str(&format!("\n… truncated at {cap} hits."));
    }
    text
}

fn grep_walk(
    ws: &Workspace,
    root: &Path,
    pattern: &str,
    args: &Value,
    limits: ToolLimits,
    blobs: Option<&BlobStore>,
    id: &str,
) -> ToolResponse {
    let ignore_case = grep_ignore_case(args);
    let re = match compile_pattern(pattern, ignore_case) {
        Ok(r) => r,
        Err(e) => {
            return ToolResponse::text(
                id,
                format!("Error: invalid pattern: {e}"),
                ToolState::Error,
            );
        }
    };
    let file_glob = arg_str(args, "glob")
        .or_else(|| arg_str(args, "glob_pattern"))
        .and_then(|p| GlobMatcher::new(&p).ok());
    let cap = grep_head_limit(args);
    let mut lines = Vec::new();
    if root.is_file() {
        let rel = arg_path(args).map(|raw| ws.shown(&raw)).unwrap_or_else(|| {
            root.strip_prefix(ws.root())
                .unwrap_or(root)
                .to_string_lossy()
                .replace('\\', "/")
        });
        match grep_one_file(ws, &rel, root, &re, cap) {
            Ok(hits) => lines = hits,
            Err(e) => return ToolResponse::text(id, e, ToolState::Error),
        }
    } else {
        let end = walk_files(root, ws.root(), &mut |rel, abs| {
            if let Some(g) = &file_glob {
                if !g.matches(rel) {
                    return true;
                }
            }
            if !abs.is_file() {
                return true;
            }
            if let Ok(meta) = abs.metadata() {
                if meta.len() > MAX_FILE_BYTES {
                    return true;
                }
            }
            let Ok(body) = fs::read_to_string(abs) else {
                return true;
            };
            if body.contains('\0') {
                return true;
            }
            for (i, line) in body.lines().enumerate() {
                if re.is_match(line) {
                    lines.push(format!("{rel}:{}:{line}", i + 1));
                    if lines.len() >= cap {
                        return false;
                    }
                }
            }
            true
        });
        if end == WalkEnd::Budget && lines.len() < cap {
            let mut text = if lines.is_empty() {
                format!("No matches for `{pattern}`.")
            } else {
                lines.join("\n")
            };
            text.push_str(&format!(
                "\n… scan stopped after {}s.",
                WALK_BUDGET.as_secs()
            ));
            return folded_response(id, text, ToolState::Success, limits, blobs);
        }
    }
    let truncated = lines.len() >= cap;
    let mut text = if lines.is_empty() {
        format!("No matches for `{pattern}`.")
    } else {
        lines.join("\n")
    };
    if truncated {
        text.push_str(&format!("\n… truncated at {cap} hits."));
    }
    folded_response(id, text, ToolState::Success, limits, blobs)
}

fn grep_one_file(
    ws: &Workspace,
    rel: &str,
    abs: &Path,
    re: &Regex,
    cap: usize,
) -> Result<Vec<String>, String> {
    if super::doc::is_legacy_office(rel) {
        return Err(super::doc::legacy_error(rel));
    }
    if super::doc::is_doc_path(rel) {
        return super::doc::grep_extracted(ws, rel, abs, re, cap);
    }
    if let Ok(meta) = abs.metadata() {
        if meta.len() > MAX_FILE_BYTES {
            return Ok(Vec::new());
        }
    }
    let Ok(body) = fs::read_to_string(abs) else {
        return Ok(Vec::new());
    };
    if body.contains('\0') {
        return Ok(Vec::new());
    }
    let mut lines = Vec::new();
    for (i, line) in body.lines().enumerate() {
        if re.is_match(line) {
            lines.push(format!("{rel}:{}:{line}", i + 1));
            if lines.len() >= cap {
                break;
            }
        }
    }
    Ok(lines)
}

fn glob_root(ws: &Workspace, args: &Value) -> Result<PathBuf, String> {
    if let Some(raw) = arg_str(args, "target_directory").or_else(|| arg_str(args, "path")) {
        ws.resolve(&raw)
    } else {
        Ok(ws.root().to_path_buf())
    }
}

fn grep_root(ws: &Workspace, args: &Value) -> Result<PathBuf, String> {
    if let Some(raw) = arg_path(args) {
        ws.resolve(&raw)
    } else {
        Ok(ws.root().to_path_buf())
    }
}

fn compile_pattern(pattern: &str, ignore_case: bool) -> Result<Regex, regex::Error> {
    let mut b = regex::RegexBuilder::new(pattern);
    b.case_insensitive(ignore_case);
    match b.build() {
        Ok(re) => Ok(re),
        Err(_) => {
            let mut lit = regex::RegexBuilder::new(&regex::escape(pattern));
            lit.case_insensitive(ignore_case);
            lit.build()
        }
    }
}

struct GlobMatcher {
    re: Regex,
}

impl GlobMatcher {
    fn new(pattern: &str) -> Result<Self, regex::Error> {
        let re = Regex::new(&glob_to_regex(pattern))?;
        Ok(Self { re })
    }

    fn matches(&self, rel: &str) -> bool {
        let norm = rel.replace('\\', "/");
        if self.re.is_match(&norm) {
            return true;
        }
        Path::new(&norm)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| self.re.is_match(n))
    }
}

fn glob_to_regex(pattern: &str) -> String {
    let pat = pattern
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string();
    format!("^{}$", glob_body(&pat))
}

fn glob_body(pat: &str) -> String {
    let chars: Vec<char> = pat.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                if i + 2 < chars.len() && chars[i + 2] == '/' {
                    out.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    out.push_str(".*");
                    i += 2;
                }
            }
            '*' => {
                out.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            '{' => {
                if let Some((end, inner)) = glob_brace_alts(&chars, i) {
                    out.push_str("(?:");
                    for (n, alt) in inner.iter().enumerate() {
                        if n > 0 {
                            out.push('|');
                        }
                        out.push_str(&glob_body(alt));
                    }
                    out.push(')');
                    i = end;
                } else {
                    out.push_str("\\{");
                    i += 1;
                }
            }
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '}' | '[' | ']' => {
                out.push('\\');
                out.push(chars[i]);
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Cursor-style `{rs,toml,md}`. Nested braces and `{` without a comma stay literal.
fn glob_brace_alts(chars: &[char], open: usize) -> Option<(usize, Vec<String>)> {
    let mut depth = 0usize;
    let mut comma = false;
    for (j, &c) in chars.iter().enumerate().skip(open) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if !comma {
                        return None;
                    }
                    let inner: String = chars[open + 1..j].iter().collect();
                    if inner.contains('{') {
                        return None;
                    }
                    let alts: Vec<String> = inner.split(',').map(|s| s.to_string()).collect();
                    if alts.iter().any(|a| a.is_empty()) {
                        return None;
                    }
                    return Some((j + 1, alts));
                }
            }
            ',' if depth == 1 => comma = true,
            _ => {}
        }
    }
    None
}

fn rg_search_path() -> OsString {
    let mut dirs = Vec::new();
    if let Some(home) = crate::config::user_home() {
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".local/bin"));
        #[cfg(windows)]
        {
            dirs.push(home.join("scoop/shims"));
            dirs.push(home.join("AppData/Local/Microsoft/WinGet/Links"));
            if let Ok(pd) = std::env::var("ProgramData") {
                dirs.push(PathBuf::from(pd).join("chocolatey/bin"));
            }
            if let Ok(pf) = std::env::var("ProgramFiles") {
                dirs.push(PathBuf::from(pf).join("Git/usr/bin"));
            }
        }
        #[cfg(unix)]
        {
            dirs.push(PathBuf::from("/opt/homebrew/bin"));
            dirs.push(PathBuf::from("/usr/local/bin"));
        }
    }
    let current = std::env::var_os("PATH").or_else(|| std::env::var_os("Path"));
    let mut parts: Vec<PathBuf> = dirs.into_iter().filter(|p| p.is_dir()).collect();
    if let Some(c) = current {
        parts.extend(std::env::split_paths(&c));
    }
    std::env::join_paths(parts).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

fn path_under_hyper_overlay(ws: &Workspace, root: &Path) -> bool {
    path_under_dir(ws, root, ".grok-hyper")
}

fn path_under_dir(ws: &Workspace, root: &Path, name: &str) -> bool {
    let Ok(rel) = root.strip_prefix(ws.root()) else {
        return root
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == name);
    };
    let s = rel.to_string_lossy().replace('\\', "/");
    s == name || s.starts_with(&format!("{name}/"))
}

fn skip_walk_dir(name: &str, parent_rel: &str) -> bool {
    if SKIP_DIR.contains(&name) {
        return true;
    }
    let under_hyper = parent_rel == ".grok-hyper" || parent_rel.starts_with(".grok-hyper/");
    under_hyper && HYPER_SKIP_DIR.contains(&name)
}

fn walk_files(dir: &Path, root: &Path, visit: &mut dyn FnMut(&str, &Path) -> bool) -> WalkEnd {
    let started = Instant::now();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        if started.elapsed() >= WALK_BUDGET {
            return WalkEnd::Budget;
        }
        let Ok(entries) = fs::read_dir(&cur) else {
            continue;
        };
        for entry in entries.flatten() {
            if started.elapsed() >= WALK_BUDGET {
                return WalkEnd::Budget;
            }
            let path = entry.path();
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                let parent_rel = cur
                    .strip_prefix(root)
                    .unwrap_or(&cur)
                    .to_string_lossy()
                    .replace('\\', "/");
                if skip_walk_dir(name_s.as_ref(), &parent_rel) {
                    continue;
                }
                if super::path::is_reparse_or_symlink(&path) {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if !visit(&rel, &path) {
                return WalkEnd::Capped;
            }
        }
    }
    WalkEnd::Done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_calls::ToolCall;
    use serde_json::json;
    use std::path::PathBuf;

    fn scratch() -> (Workspace, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("hyper-find-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = Workspace::open(&dir, true).unwrap();
        (w, dir)
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: "Glob".into(),
            arguments: args,
        }
    }

    #[test]
    fn unfiltered_tree_glob_is_only_star_stars() {
        assert!(is_unfiltered_tree_glob("**/*"));
        assert!(is_unfiltered_tree_glob("*"));
        assert!(is_unfiltered_tree_glob("*.*"));
        assert!(!is_unfiltered_tree_glob("**/*.rs"));
        assert!(!is_unfiltered_tree_glob("**/*.{rs,toml,md}"));
        assert!(!is_unfiltered_tree_glob("**/AGENT.md"));
        assert!(!is_unfiltered_tree_glob("*.md"));
    }

    #[test]
    fn glob_sees_hyper_overnight_but_skips_sessions() {
        let (ws, dir) = scratch();
        let overnight = dir.join(".grok-hyper/overnight");
        let sessions = dir.join(".grok-hyper/sessions");
        std::fs::create_dir_all(&overnight).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(overnight.join("ping.py"), "print('ok')\n").unwrap();
        std::fs::write(sessions.join("sid.jsonl"), "{}\n").unwrap();
        let by_pattern = glob_files(
            &ws,
            &call(json!({"glob_pattern": ".grok-hyper/overnight/**"})),
            ToolLimits::default(),
        );
        let t = by_pattern.joined_text();
        assert!(t.contains("ping.py"), "{t}");
        assert!(!t.contains("sid.jsonl"), "{t}");
        let by_star = glob_files(
            &ws,
            &call(json!({"glob_pattern": "**/*.py"})),
            ToolLimits::default(),
        );
        let s = by_star.joined_text();
        assert!(s.contains("ping.py"), "{s}");
        let all = glob_files(
            &ws,
            &call(json!({"glob_pattern": ".grok-hyper/**"})),
            ToolLimits::default(),
        );
        let a = all.joined_text();
        assert!(a.contains("ping.py"), "{a}");
        assert!(
            !a.contains("sid.jsonl"),
            "session dumps must stay skipped: {a}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_overlay_finds_overnight_file() {
        let (ws, dir) = scratch();
        let overnight = dir.join(".grok-hyper/overnight");
        std::fs::create_dir_all(&overnight).unwrap();
        std::fs::write(overnight.join("ping.py"), "print('OVERNIGHT_OK')\n").unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({
                    "pattern": "OVERNIGHT_OK",
                    "path": ".grok-hyper/overnight"
                }),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert!(t.contains("OVERNIGHT_OK"), "{t}");
        assert!(t.contains("ping.py"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_brace_alts_match_extensions() {
        let m = GlobMatcher::new("**/*.{rs,toml,md}").unwrap();
        assert!(m.matches("crates/hyper-loop/src/lib.rs"), "rs");
        assert!(m.matches("config.toml"), "toml");
        assert!(m.matches("docs/architecture.md"), "md");
        assert!(!m.matches("fold_idle.py"), "py must not match brace glob");
        let (ws, dir) = scratch();
        std::fs::create_dir_all(dir.join("crates")).unwrap();
        std::fs::write(dir.join("crates/lib.rs"), "fn x() {}\n").unwrap();
        std::fs::write(dir.join("config.toml"), "").unwrap();
        std::fs::write(dir.join("readme.md"), "# h\n").unwrap();
        std::fs::write(dir.join("skip.py"), "").unwrap();
        let t = glob_files(
            &ws,
            &call(json!({"glob_pattern": "**/*.{rs,toml,md}"})),
            ToolLimits::default(),
        )
        .joined_text();
        assert!(t.contains("lib.rs"), "{t}");
        assert!(t.contains("config.toml"), "{t}");
        assert!(t.contains("readme.md"), "{t}");
        assert!(!t.contains("skip.py"), "{t}");
        assert!(
            !t.starts_with("No files matching"),
            "brace glob must not empty-hit: {t}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_skips_vendored_third_party() {
        let (ws, dir) = scratch();
        std::fs::create_dir_all(dir.join("crates")).unwrap();
        std::fs::create_dir_all(dir.join("third_party/grok-build")).unwrap();
        std::fs::write(dir.join("crates/lib.rs"), "fn x() {}\n").unwrap();
        std::fs::write(
            dir.join("third_party/grok-build/lib.rs"),
            "fn vendored() {}\n",
        )
        .unwrap();
        let t = glob_files(
            &ws,
            &call(json!({"glob_pattern": "**/*.rs"})),
            ToolLimits::default(),
        )
        .joined_text();
        assert!(t.contains("crates/lib.rs"), "{t}");
        assert!(
            !t.contains("third_party"),
            "vendored clones must not fill the glob cap: {t}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_skips_vendored_third_party() {
        let (ws, dir) = scratch();
        std::fs::create_dir_all(dir.join("crates")).unwrap();
        std::fs::create_dir_all(dir.join("third_party/grok-build")).unwrap();
        std::fs::write(dir.join("crates/lib.rs"), "fn vendor_mark_alpha() {}\n").unwrap();
        std::fs::write(
            dir.join("third_party/grok-build/lib.rs"),
            "fn vendor_mark_alpha() {}\n",
        )
        .unwrap();
        let t = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "vendor_mark_alpha"}),
            },
            ToolLimits::default(),
            None,
        )
        .joined_text();
        assert!(t.contains("crates/lib.rs"), "{t}");
        assert!(
            !t.contains("third_party"),
            "rg must not dump vendored clones: {t}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
