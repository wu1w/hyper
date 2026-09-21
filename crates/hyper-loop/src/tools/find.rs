//! `Glob` / `Grep`. Grep is ripgrep (Cursor); Glob walks the workspace.

use std::ffi::OsString;
use std::fs;
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
    "release",
    "out",
    "unpacked",
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

/// Footer on a workspace-root unfiltered glob. The listing above is top-level
/// only; a recursive dump is not useful.
pub(crate) const GLOB_TREE_MSG: &str = "\
Top-level sample only (vendor / release / out skipped). Grep a symbol, or \
Glob with an extension / inside a subdirectory — a full recursive **/* dump \
is not useful.";

const SHALLOW_SAMPLE: usize = 40;
const MAX_FILE_BYTES: u64 = super::path::MAX_TEXT_SLURP_BYTES;
/// Fallback Glob/Grep walk cap. Windows home/Documents as workspace used to
/// freeze the hop when `rg` was missing from PATH.
const WALK_BUDGET: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, PartialEq, Eq)]
enum WalkEnd {
    Done,
    Capped,
    Budget,
}

struct WalkOutcome {
    end: WalkEnd,
}

pub fn glob_files(ws: &Workspace, call: &ToolCall, limits: ToolLimits) -> ToolResponse {
    let Some(pattern) = arg_str(&call.arguments, "glob_pattern")
        .or_else(|| arg_str(&call.arguments, "pattern"))
        .or_else(|| arg_str(&call.arguments, "glob"))
        .filter(|s| !s.trim().is_empty())
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
    if root.is_file() {
        let shown = root
            .strip_prefix(ws.root())
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| root.display().to_string());
        return ToolResponse::text(
            &call.id,
            format!(
                "Error: `target_directory` is a file ({shown}). Glob walks a directory. Read that path, or Glob its parent."
            ),
            ToolState::Error,
        );
    }
    if let Some(raw) =
        arg_str(&call.arguments, "target_directory").or_else(|| arg_str(&call.arguments, "path"))
    {
        if !root.exists() {
            return ToolResponse::text(
                &call.id,
                format!("Error: The directory {} does not exist.", ws.shown(&raw)),
                ToolState::Error,
            );
        }
        if !root.is_dir() {
            return ToolResponse::text(
                &call.id,
                format!(
                    "Error: `target_directory` is not a directory ({}).",
                    ws.shown(&raw)
                ),
                ToolState::Error,
            );
        }
    }
    if let Err(e) = fs::read_dir(&root) {
        let shown = arg_str(&call.arguments, "target_directory")
            .or_else(|| arg_str(&call.arguments, "path"))
            .map(|raw| ws.shown(&raw))
            .unwrap_or_else(|| ".".into());
        return ToolResponse::text(
            &call.id,
            format!("Error: {shown}: {}", super::path::io_user_msg(&e)),
            ToolState::Error,
        );
    }
    if is_unfiltered_tree_glob(&pattern) && listing_is_ws_root(ws, &root) {
        let hits = shallow_listing(&root, ws.root(), SHALLOW_SAMPLE);
        let sample = if hits.is_empty() {
            format!("No files matching `{pattern}`.")
        } else {
            hits.join("\n")
        };
        let text = format!("Error: {GLOB_TREE_MSG}\n\n{sample}");
        return folded_response(&call.id, text, ToolState::Error, limits, None);
    }
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
    let mut skipped = 0usize;
    let outcome = walk_files(&root, ws.root(), &mut |rel, _abs| {
        if matcher.matches(rel) {
            hits.push(rel.to_string());
        }
        hits.len() < GLOB_CAP
    }, &mut skipped);
    hits.sort();
    let n = hits.len();
    if n == 0 && skipped > 0 {
        return ToolResponse::text(
            &call.id,
            "Error: some directories were unreadable.",
            ToolState::Error,
        );
    }
    let mut text = if hits.is_empty() {
        format!("No files matching `{pattern}`.")
    } else {
        hits.join("\n")
    };
    if n >= GLOB_CAP {
        let mut text = format!(
            "Error: Glob truncated at {GLOB_CAP} paths (incomplete). Narrow the glob or use Grep."
        );
        if !hits.is_empty() {
            text.push('\n');
            text.push_str(&hits.join("\n"));
        }
        if skipped > 0 {
            text.push_str("\n… some directories were unreadable.");
        }
        return folded_response(&call.id, text, ToolState::Error, limits, None);
    } else if outcome.end == WalkEnd::Budget {
        let mut text = format!(
            "Error: Glob scan stopped after {}s (incomplete).",
            WALK_BUDGET.as_secs()
        );
        if !hits.is_empty() {
            text.push('\n');
            text.push_str(&hits.join("\n"));
        }
        if skipped > 0 {
            text.push_str("\n… some directories were unreadable.");
        }
        return folded_response(&call.id, text, ToolState::Error, limits, None);
    }
    if skipped > 0 {
        text.push_str("\n… some directories were unreadable.");
    }
    folded_response(&call.id, text, ToolState::Success, limits, None)
}

pub fn grep_files(
    ws: &Workspace,
    call: &ToolCall,
    limits: ToolLimits,
    blobs: Option<&BlobStore>,
) -> ToolResponse {
    let Some(pattern) = arg_str(&call.arguments, "pattern")
        .or_else(|| arg_str(&call.arguments, "query"))
        .filter(|s| !s.trim().is_empty())
    else {
        return ToolResponse::text(&call.id, "Error: No `pattern` provided.", ToolState::Error);
    };
    let root = match grep_root(ws, &call.arguments) {
        Ok(p) => p,
        Err(e) => return ToolResponse::text(&call.id, e, ToolState::Error),
    };
    if let Some(raw) = arg_path(&call.arguments) {
        if !root.exists() {
            return ToolResponse::text(
                &call.id,
                format!("Error: The path {} does not exist.", ws.shown(&raw)),
                ToolState::Error,
            );
        }
        if !root.is_file() && !root.is_dir() {
            return ToolResponse::text(
                &call.id,
                format!("Error: {} is not a regular file.", ws.shown(&raw)),
                ToolState::Error,
            );
        }
    }
    if root.is_dir() {
        if let Err(e) = fs::read_dir(&root) {
            let shown = arg_path(&call.arguments)
                .map(|raw| ws.shown(&raw))
                .unwrap_or_else(|| ".".into());
            return ToolResponse::text(
                &call.id,
                format!("Error: {shown}: {}", super::path::io_user_msg(&e)),
                ToolState::Error,
            );
        }
    }
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
        if file_has_nul(&root) {
            return ToolResponse::text(
                &call.id,
                format!("Error: {rel} appears to be binary."),
                ToolState::Error,
            );
        }
    }
    if let Some((text, state)) = grep_ripgrep(ws, &root, &pattern, &call.arguments) {
        if state == ToolState::Error {
            return ToolResponse::text(&call.id, text, state);
        }
        return folded_response(&call.id, text, state, limits, blobs);
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
        .filter(|&n| n > 0)
        .unwrap_or(GREP_CAP as u32)
        .min(2000) as usize
}

fn clip_grep_line(line: &str) -> String {
    const MAX: usize = 400;
    if line.len() <= MAX {
        return line.to_string();
    }
    let mut end = MAX;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

fn grep_output_mode(args: &Value) -> &str {
    match arg_str(args, "output_mode").as_deref() {
        Some("files_with_matches") => "files_with_matches",
        Some("count") => "count",
        _ => "content",
    }
}

fn grep_ripgrep(
    ws: &Workspace,
    root: &Path,
    pattern: &str,
    args: &Value,
) -> Option<(String, ToolState)> {
    let mut cmd = Command::new("rg");
    let path = rg_search_path();
    crate::proc_spawn::hide_window(&mut cmd);
    cmd.current_dir(ws.root())
        .env("TERM", "dumb")
        .env("PATH", &path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    apply_rg_skip_globs(&mut cmd, ws, root);
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
    // Directory scans skip giant dumps. Cap matches the 8MiB text slurp so a
    // 2MB source file is not silently "No matches".
    if !root.is_file() {
        cmd.arg("--max-filesize").arg("8M");
    } else {
        // Explicit file: search even if a later NUL would make rg skip it.
        cmd.arg("-a");
    }
    // A 9MB single-line file would otherwise print megabyte match rows.
    cmd.arg("--max-columns").arg("400");
    cmd.arg("--max-columns-preview");
    cmd.arg("--").arg(pattern).arg(root);
    let mut child = cmd.spawn().ok()?;
    // Drain pipes on helper threads so a huge match set cannot fill the OS
    // pipe (64KiB) and deadlock `wait` → `read_to_end`. Separate threads so
    // stdout and stderr cannot stall each other.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_h = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut out) = stdout {
            crate::proc_spawn::drain_capped(&mut out, &mut buf, MAX_RG_PIPE);
        }
        buf
    });
    let err_h = std::thread::spawn(move || {
        let mut err = Vec::new();
        if let Some(mut e) = stderr {
            crate::proc_spawn::drain_capped(&mut e, &mut err, MAX_RG_PIPE);
        }
        err
    });
    let started = Instant::now();
    let timeout = Duration::from_secs(20);
    let st = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_h.join();
                let _ = err_h.join();
                return Some((
                    format!(
                        "Error: ripgrep timed out after {}s.",
                        timeout.as_secs()
                    ),
                    ToolState::Error,
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                let _ = out_h.join();
                let _ = err_h.join();
                return Some((
                    "Error: ripgrep failed (wait).".into(),
                    ToolState::Error,
                ));
            }
        }
    };
    let buf = out_h.join().unwrap_or_default();
    let err = err_h.join().unwrap_or_default();
    let pipe_capped = buf.len() >= MAX_RG_PIPE;
    // rg: 0 matches, 1 no matches, 2+ error. Swallowed stderr used
    // to turn a missing path / bad regex into "No matches". Mixed
    // trees (one locked dir + hits elsewhere) still exit 2 with
    // stdout; dropping those hits is dishonest.
    let code = st.code().unwrap_or(2);
    if buf.contains(&0) {
        return Some((
            "Error: ripgrep output appears to be binary.".into(),
            ToolState::Error,
        ));
    }
    let raw = String::from_utf8_lossy(&buf);
    if code >= 2 {
        let msg = String::from_utf8_lossy(&err);
        let msg = super::path::strip_os_error(msg.trim());
        let hits = format_rg_output(ws, pattern, raw.as_ref(), args, true);
        if !raw.trim().is_empty() && !hits.starts_with("No matches") {
            let note = if msg.is_empty() {
                format!("… ripgrep also failed (exit {code}).")
            } else {
                format!("… also: {msg}")
            };
            return Some(finish_rg(
                format!("{hits}\n{note}"),
                ToolState::Success,
                pipe_capped,
            ));
        }
        let text = if msg.is_empty() {
            format!("Error: ripgrep failed (exit {code}).")
        } else {
            format!("Error: {msg}")
        };
        return Some((text, ToolState::Error));
    }
    let mut text = format_rg_output(ws, pattern, raw.as_ref(), args, st.success());
    let huge = oversized_dir_count(root);
    if huge > 0 {
        if text.starts_with("No matches") {
            return Some((
                format!(
                    "Error: Grep skipped {huge} oversized file(s) (max {MAX_FILE_BYTES} bytes). Point `path` at a smaller file."
                ),
                ToolState::Error,
            ));
        }
        return Some(finish_rg(
            format!(
                "Error: Grep skipped {huge} oversized file(s) (max {MAX_FILE_BYTES} bytes; incomplete).\n{text}"
            ),
            ToolState::Error,
            pipe_capped,
        ));
    }
    let special = special_dir_count(root);
    if special > 0 {
        if text.starts_with("No matches") {
            return Some((
                format!("Error: Grep skipped {special} special file(s) (not regular)."),
                ToolState::Error,
            ));
        }
        return Some(finish_rg(
            format!(
                "Error: Grep skipped {special} special file(s) (not regular; incomplete).\n{text}"
            ),
            ToolState::Error,
            pipe_capped,
        ));
    }
    if text.starts_with("No matches") {
        if let Some(note) = binary_dir_note(root) {
            text.push_str(&note);
        }
    }
    Some(finish_rg(text, ToolState::Success, pipe_capped))
}

fn finish_rg(text: String, state: ToolState, pipe_capped: bool) -> (String, ToolState) {
    let text = pipe_note(text, pipe_capped);
    if pipe_capped && state != ToolState::Error {
        let text = if text.starts_with("Error:") {
            text
        } else {
            format!("Error: Grep output truncated at 512KiB (incomplete).\n{text}")
        };
        (text, ToolState::Error)
    } else {
        (text, state)
    }
}

/// Keep reading so the child can exit; only the first `cap` bytes are kept.
const MAX_RG_PIPE: usize = 512 * 1024;

fn pipe_note(text: String, capped: bool) -> String {
    if capped {
        format!("{text}\n… truncated: ripgrep output exceeded 512KiB.")
    } else {
        text
    }
}

/// rg skips binaries silently. If the grep root's immediate files are all
/// binary, say so instead of a bare "No matches".
fn binary_dir_note(root: &Path) -> Option<String> {
    if !root.is_dir() {
        return None;
    }
    let rd = fs::read_dir(root).ok()?;
    let mut binary = 0usize;
    let mut textish = 0usize;
    let mut special = 0usize;
    for e in rd.flatten().take(32) {
        let p = e.path();
        if p.is_dir() {
            continue;
        }
        if super::path::is_special_file(&p) || (!p.is_file() && !p.is_dir()) {
            special += 1;
            continue;
        }
        if !p.is_file() {
            continue;
        }
        if file_has_nul(&p) {
            binary += 1;
        } else {
            textish += 1;
        }
    }
    if special > 0 && textish == 0 && binary == 0 {
        Some(format!("\n… skipped {special} special file(s)."))
    } else if binary > 0 && textish == 0 {
        Some(format!("\n… skipped {binary} binary file(s)."))
    } else {
        None
    }
}

/// Directory rg uses `--max-filesize 8M`. A hit only in a 9MiB dump would
/// otherwise be Success "No matches".
fn oversized_dir_count(root: &Path) -> usize {
    if root.is_file() {
        return usize::from(super::path::is_oversized_text(root));
    }
    let mut n = 0usize;
    let mut visits = 0usize;
    let mut skipped = 0usize;
    let _ = walk_files(
        root,
        root,
        &mut |_, abs| {
            visits += 1;
            if super::path::is_oversized_text(abs) {
                n += 1;
            }
            visits < 64 && n < 8
        },
        &mut skipped,
    );
    n
}

fn special_dir_count(root: &Path) -> usize {
    if root.is_file() {
        return usize::from(super::path::is_special_file(root));
    }
    let mut n = 0usize;
    let mut visits = 0usize;
    let mut skipped = 0usize;
    let _ = walk_files(
        root,
        root,
        &mut |_, abs| {
            visits += 1;
            if super::path::is_special_file(abs) {
                n += 1;
            }
            visits < 64 && n < 8
        },
        &mut skipped,
    );
    n
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
        .map(|l| clip_grep_line(&l))
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
        let skipped_files = std::cell::Cell::new(0usize);
        let skipped_binary = std::cell::Cell::new(0usize);
        let skipped_special = std::cell::Cell::new(0usize);
        let skipped_huge = std::cell::Cell::new(0usize);
        let mut skipped_dirs = 0usize;
        let outcome = walk_files(
            root,
            ws.root(),
            &mut |rel, abs| {
                if let Some(g) = &file_glob {
                    if !g.matches(rel) {
                        return true;
                    }
                }
                if super::path::is_special_file(abs) || (!abs.is_file() && !abs.is_dir()) {
                    skipped_special.set(skipped_special.get() + 1);
                    return true;
                }
                if !abs.is_file() {
                    return true;
                }
                if let Ok(meta) = abs.metadata() {
                    if meta.len() > MAX_FILE_BYTES {
                        skipped_huge.set(skipped_huge.get() + 1);
                        return true;
                    }
                }
                let Ok(bytes) = super::path::read_bytes_regular(abs) else {
                    skipped_files.set(skipped_files.get() + 1);
                    return true;
                };
                if bytes.contains(&0) {
                    skipped_binary.set(skipped_binary.get() + 1);
                    return true;
                }
                let Ok(body) = String::from_utf8(bytes) else {
                    skipped_binary.set(skipped_binary.get() + 1);
                    return true;
                };
                for (i, line) in body.lines().enumerate() {
                    if re.is_match(line) {
                        lines.push(format!("{rel}:{}:{}", i + 1, clip_grep_line(line)));
                        if lines.len() >= cap {
                            return false;
                        }
                    }
                }
                true
            },
            &mut skipped_dirs,
        );
        let skipped = skipped_dirs + skipped_files.get();
        if lines.is_empty() && skipped_special.get() > 0 && skipped == 0 {
            return ToolResponse::text(
                id,
                format!(
                    "Error: skipped {} special file(s) (not a regular file).",
                    skipped_special.get()
                ),
                ToolState::Error,
            );
        }
        if lines.is_empty() && skipped_huge.get() > 0 && skipped == 0 {
            return ToolResponse::text(
                id,
                format!(
                    "Error: skipped {} oversized file(s) (max {} bytes).",
                    skipped_huge.get(),
                    MAX_FILE_BYTES
                ),
                ToolState::Error,
            );
        }
        if lines.is_empty() && skipped > 0 && outcome.end != WalkEnd::Budget {
            return ToolResponse::text(
                id,
                "Error: some files or directories were unreadable.",
                ToolState::Error,
            );
        }
        if outcome.end == WalkEnd::Budget && lines.len() < cap {
            let mut text = format!(
                "Error: Grep scan stopped after {}s (incomplete).",
                WALK_BUDGET.as_secs()
            );
            if lines.is_empty() {
                text.push_str(&format!(" No matches yet for `{pattern}`."));
            } else {
                text.push('\n');
                text.push_str(&lines.join("\n"));
            }
            if skipped > 0 {
                text.push_str("\n… some files or directories were unreadable.");
            }
            if skipped_binary.get() > 0 {
                text.push_str(&format!(
                    "\n… skipped {} binary file(s).",
                    skipped_binary.get()
                ));
            }
            if skipped_special.get() > 0 {
                text.push_str(&format!(
                    "\n… skipped {} special file(s).",
                    skipped_special.get()
                ));
            }
            return folded_response(id, text, ToolState::Error, limits, blobs);
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
        if skipped > 0 {
            text.push_str("\n… some files or directories were unreadable.");
        }
        if skipped_binary.get() > 0 {
            text.push_str(&format!(
                "\n… skipped {} binary file(s).",
                skipped_binary.get()
            ));
        }
        if skipped_special.get() > 0 {
            text = format!(
                "Error: Grep skipped {} special file(s) (not regular; incomplete).\n{text}",
                skipped_special.get()
            );
            return folded_response(id, text, ToolState::Error, limits, blobs);
        }
        if skipped_huge.get() > 0 {
            text = format!(
                "Error: Grep skipped {} oversized file(s) (max {MAX_FILE_BYTES} bytes; incomplete).\n{text}",
                skipped_huge.get()
            );
            return folded_response(id, text, ToolState::Error, limits, blobs);
        }
        return folded_response(id, text, ToolState::Success, limits, blobs);
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

/// First 64KiB NUL means rg would skip the file and lie with "No matches".
fn file_has_nul(path: &Path) -> bool {
    use std::io::Read;
    let mut f = match super::path::open_read_nonblock(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = [0u8; 64 * 1024];
    match f.read(&mut buf) {
        Ok(n) => buf[..n].contains(&0),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(_) => false,
    }
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
    if super::path::is_special_file(abs) {
        return Err(format!("Error: {rel} is not a regular file."));
    }
    if super::path::is_oversized_text(abs) {
        return Err(format!(
            "Error: {rel} is too large to grep without ripgrep (max {} bytes).",
            super::path::MAX_TEXT_SLURP_BYTES
        ));
    }
    let body = match super::path::read_bytes_regular(abs) {
        Ok(bytes) => {
            if bytes.contains(&0) {
                return Err(format!("Error: {rel} appears to be binary."));
            }
            match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => return Err(format!("Error: {rel} is not valid UTF-8 text.")),
            }
        }
        Err(e) => {
            return Err(format!("Error: {rel}: {}", super::path::io_user_msg(&e)));
        }
    };
    let mut lines = Vec::new();
    for (i, line) in body.lines().enumerate() {
        if re.is_match(line) {
            lines.push(format!("{rel}:{}:{}", i + 1, clip_grep_line(line)));
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

fn apply_rg_skip_globs(cmd: &mut Command, ws: &Workspace, root: &Path) {
    for dir in SKIP_DIR {
        if path_under_dir(ws, root, dir) {
            continue;
        }
        // `!release/**` only matches a root-level dir. Nested electron
        // bundles live at `web/desktop/release/` and need `**/`.
        cmd.arg("--glob");
        cmd.arg(format!("!**/{dir}/**"));
    }
}

fn path_under_hyper_overlay(ws: &Workspace, root: &Path) -> bool {
    path_under_dir(ws, root, ".grok-hyper")
}

fn path_under_dir(ws: &Workspace, root: &Path, name: &str) -> bool {
    let rel = match root.strip_prefix(ws.root()) {
        Ok(r) => r.to_path_buf(),
        Err(_) => root.to_path_buf(),
    };
    rel.components().any(|c| c.as_os_str() == name)
}

fn skip_walk_dir(name: &str, parent_rel: &str) -> bool {
    if SKIP_DIR.contains(&name) {
        return true;
    }
    let under_hyper = parent_rel == ".grok-hyper" || parent_rel.starts_with(".grok-hyper/");
    under_hyper && HYPER_SKIP_DIR.contains(&name)
}

fn listing_is_ws_root(ws: &Workspace, root: &Path) -> bool {
    if root == ws.root() {
        return true;
    }
    match (root.canonicalize(), ws.root().canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Immediate children of `dir`, skipping packed/vendor names. Dirs keep a
/// trailing slash so the sample reads as a tree, not a file list.
pub(crate) fn shallow_listing(dir: &Path, ws_root: &Path, cap: usize) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let parent_rel = dir
        .strip_prefix(ws_root)
        .unwrap_or(dir)
        .to_string_lossy()
        .replace('\\', "/");
    let mut hits: Vec<(bool, String)> = Vec::new();
    for entry in entries.flatten() {
        let name_s = entry.file_name().to_string_lossy().into_owned();
        if skip_walk_dir(&name_s, &parent_rel) {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(ws_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            hits.push((true, format!("{rel}/")));
        } else {
            hits.push((false, rel));
        }
    }
    hits.sort_by(|a, b| a.1.cmp(&b.1));
    hits.into_iter().map(|(_, s)| s).take(cap).collect()
}

fn walk_files(
    dir: &Path,
    root: &Path,
    visit: &mut dyn FnMut(&str, &Path) -> bool,
    skipped: &mut usize,
) -> WalkOutcome {
    let started = Instant::now();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        if started.elapsed() >= WALK_BUDGET {
            return WalkOutcome {
                end: WalkEnd::Budget,
            };
        }
        let Ok(entries) = fs::read_dir(&cur) else {
            *skipped += 1;
            continue;
        };
        for entry in entries.flatten() {
            if started.elapsed() >= WALK_BUDGET {
                return WalkOutcome {
                    end: WalkEnd::Budget,
                };
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
            // DirEntry `is_file` is false for file symlinks, FIFOs, sockets,
            // and devices. Cursor Glob lists those names and does not walk
            // directory symlinks (skipped above).
            if ft.is_symlink() && path.is_dir() {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if !visit(&rel, &path) {
                return WalkOutcome {
                    end: WalkEnd::Capped,
                };
            }
        }
    }
    WalkOutcome {
        end: WalkEnd::Done,
    }
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
    fn pipe_note_marks_capped_rg_output() {
        let t = pipe_note("hit".into(), true);
        assert!(t.contains("truncated"), "{t}");
        assert_eq!(pipe_note("hit".into(), false), "hit");
    }

    #[test]
    fn grep_rg_pipe_cap_is_error() {
        let (ws, dir) = scratch();
        let line = format!("HIT {}\n", "x".repeat(320));
        let mut body = String::with_capacity(line.len() * 1800);
        for _ in 0..1800 {
            body.push_str(&line);
        }
        std::fs::write(dir.join("wide.txt"), body).unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({
                    "pattern": "HIT",
                    "path": "wide.txt",
                    "head_limit": 2000
                }),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Error, "{t}");
        assert!(t.starts_with("Error:"), "{t}");
        assert!(t.contains("truncated") || t.contains("512"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_mixed_oversized_neighbor_is_error() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("hit.txt"), "NEEDLE_MIXED_HUGE\n").unwrap();
        std::fs::write(dir.join("huge.bin"), vec![b'x'; 8 * 1024 * 1024 + 1]).unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "NEEDLE_MIXED_HUGE"}),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Error, "{t}");
        assert!(t.starts_with("Error:"), "{t}");
        assert!(t.contains("oversized") || t.contains("incomplete"), "{t}");
        assert!(t.contains("NEEDLE_MIXED_HUGE"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn grep_mixed_fifo_neighbor_is_error() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("hit.txt"), "NEEDLE_MIXED_FIFO\n").unwrap();
        let fifo = dir.join("pipe.txt");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "NEEDLE_MIXED_FIFO"}),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Error, "{t}");
        assert!(t.starts_with("Error:"), "{t}");
        assert!(t.contains("special") || t.contains("incomplete"), "{t}");
        assert!(t.contains("NEEDLE_MIXED_FIFO"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_walk_directory_of_binaries_notes_skip() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("a.bin"), [0u8, 1, 2, 0]).unwrap();
        std::fs::write(dir.join("b.bin"), [0u8; 32]).unwrap();
        let r = grep_walk(
            &ws,
            &dir,
            "UNIQUE_NOPE",
            &json!({}),
            ToolLimits::default(),
            None,
            "t1",
        );
        let t = r.joined_text();
        assert!(t.contains("No matches"), "{t}");
        assert!(t.contains("binary"), "{t}");
        assert!(t.contains("skipped"), "{t}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn grep_walk_directory_of_fifos_is_error_not_empty() {
        let (ws, dir) = scratch();
        let fifo = dir.join("pipe.txt");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let r = grep_walk(
            &ws,
            &dir,
            "UNIQUE_NOPE",
            &json!({}),
            ToolLimits::default(),
            None,
            "t1",
        );
        assert_eq!(r.state, ToolState::Error, "{}", r.joined_text());
        let t = r.joined_text();
        assert!(t.starts_with("Error:"), "{t}");
        assert!(t.contains("special") || t.contains("regular"), "{t}");
        let _ = std::fs::remove_file(&fifo);
        let _ = std::fs::remove_dir_all(dir);
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
    fn glob_truncated_at_cap_is_error() {
        let (ws, dir) = scratch();
        for i in 0..(GLOB_CAP + 1) {
            std::fs::write(dir.join(format!("f{i:03}.txt")), "x\n").unwrap();
        }
        let r = glob_files(
            &ws,
            &call(json!({"glob_pattern": "*.txt"})),
            ToolLimits::default(),
        );
        let t = r.joined_text();
        assert_eq!(r.state, ToolState::Error, "{t}");
        assert!(t.starts_with("Error:"), "{t}");
        assert!(t.contains("truncated") || t.contains("incomplete"), "{t}");
        assert!(!t.contains("No files matching"), "{t}");
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

    #[test]
    fn glob_skips_electron_release_and_out() {
        let (ws, dir) = scratch();
        std::fs::create_dir_all(dir.join("crates")).unwrap();
        std::fs::create_dir_all(dir.join("web/desktop/release/mac")).unwrap();
        std::fs::create_dir_all(dir.join("plugins/vscode-hyper/out")).unwrap();
        std::fs::write(dir.join("crates/lib.rs"), "fn x() {}\n").unwrap();
        std::fs::write(
            dir.join("web/desktop/release/mac/bundle.js"),
            "function tools() {}\n",
        )
        .unwrap();
        std::fs::write(dir.join("plugins/vscode-hyper/out/ext.js"), "export {}\n").unwrap();
        let t = glob_files(
            &ws,
            &call(json!({"glob_pattern": "**/*.{rs,js}"})),
            ToolLimits::default(),
        )
        .joined_text();
        assert!(t.contains("crates/lib.rs"), "{t}");
        assert!(
            !t.contains("release"),
            "electron release must not fill the glob cap: {t}"
        );
        assert!(!t.contains("/out/"), "tsc out must not fill glob: {t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_root_star_returns_shallow_sample() {
        let (ws, dir) = scratch();
        std::fs::create_dir_all(dir.join("crates")).unwrap();
        std::fs::write(dir.join("crates/lib.rs"), "fn x() {}\n").unwrap();
        std::fs::write(dir.join("README.md"), "# h\n").unwrap();
        let r = glob_files(
            &ws,
            &call(json!({"glob_pattern": "**/*"})),
            ToolLimits::default(),
        );
        assert_eq!(r.state, ToolState::Error, "{}", r.joined_text());
        let t = r.joined_text();
        assert!(t.starts_with("Error:"), "{t}");
        assert!(t.contains("crates/"), "{t}");
        assert!(t.contains("README.md"), "{t}");
        assert!(t.contains(GLOB_TREE_MSG), "{t}");
        assert!(!t.contains("crates/lib.rs"), "must not recurse: {t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_skips_electron_release() {
        let (ws, dir) = scratch();
        std::fs::create_dir_all(dir.join("crates")).unwrap();
        std::fs::create_dir_all(dir.join("web/desktop/release")).unwrap();
        std::fs::write(dir.join("crates/lib.rs"), "fn release_mark_omega() {}\n").unwrap();
        std::fs::write(
            dir.join("web/desktop/release/bundle.js"),
            "function release_mark_omega() {}\n",
        )
        .unwrap();
        let t = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "release_mark_omega"}),
            },
            ToolLimits::default(),
            None,
        )
        .joined_text();
        assert!(t.contains("crates/lib.rs"), "{t}");
        assert!(
            !t.contains("bundle.js") && !t.contains("web/desktop/release"),
            "rg must not dump electron release: {t}"
        );
        let explicit = grep_files(
            &ws,
            &ToolCall {
                id: "t2".into(),
                name: "Grep".into(),
                arguments: json!({
                    "pattern": "release_mark_omega",
                    "path": "web/desktop/release",
                }),
            },
            ToolLimits::default(),
            None,
        )
        .joined_text();
        assert!(
            explicit.contains("bundle.js"),
            "explicit path into release must still search: {explicit}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_file_target_directory_errors() {
        let (ws, dir) = scratch();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "fn x() {}\n").unwrap();
        let t = glob_files(
            &ws,
            &call(json!({
                "glob_pattern": "**/*.rs",
                "target_directory": "src/lib.rs",
            })),
            ToolLimits::default(),
        )
        .joined_text();
        assert!(t.contains("is a file"), "{t}");
        assert!(t.contains("src/lib.rs"), "{t}");
        assert!(!t.starts_with("No files matching"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn glob_unreadable_directory_is_error_not_empty() {
        use std::os::unix::fs::PermissionsExt;
        let (ws, dir) = scratch();
        let locked = dir.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("hit.rs"), "fn x() {}\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let hit = glob_files(
            &ws,
            &call(json!({
                "glob_pattern": "**/*.rs",
                "target_directory": "locked",
            })),
            ToolLimits::default(),
        );
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Error, "{t}");
        assert!(
            t.to_ascii_lowercase().contains("permission") || t.contains("denied"),
            "{t}"
        );
        assert!(!t.contains("os error"), "{t}");
        assert!(!t.starts_with("No files matching"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn glob_mixed_unreadable_subdir_keeps_other_hits() {
        use std::os::unix::fs::PermissionsExt;
        let (ws, dir) = scratch();
        std::fs::create_dir_all(dir.join("ok")).unwrap();
        std::fs::create_dir_all(dir.join("locked")).unwrap();
        std::fs::write(dir.join("ok/hit.rs"), "fn x() {}\n").unwrap();
        std::fs::write(dir.join("locked/secret.rs"), "fn y() {}\n").unwrap();
        std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let hit = glob_files(
            &ws,
            &call(json!({"glob_pattern": "**/*.rs"})),
            ToolLimits::default(),
        );
        let _ = std::fs::set_permissions(
            dir.join("locked"),
            std::fs::Permissions::from_mode(0o755),
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Success, "{t}");
        assert!(t.contains("ok/hit.rs"), "{t}");
        assert!(t.contains("unreadable"), "{t}");
        assert!(!t.contains("os error"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn glob_only_unreadable_subdir_is_error_not_empty() {
        use std::os::unix::fs::PermissionsExt;
        let (ws, dir) = scratch();
        std::fs::create_dir_all(dir.join("locked")).unwrap();
        std::fs::write(dir.join("locked/secret.rs"), "fn y() {}\n").unwrap();
        std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let hit = glob_files(
            &ws,
            &call(json!({"glob_pattern": "**/*.rs"})),
            ToolLimits::default(),
        );
        let _ = std::fs::set_permissions(
            dir.join("locked"),
            std::fs::Permissions::from_mode(0o755),
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Error, "{t}");
        assert!(t.contains("unreadable"), "{t}");
        assert!(!t.contains("No files matching"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn grep_unreadable_directory_is_error_not_no_matches() {
        use std::os::unix::fs::PermissionsExt;
        let (ws, dir) = scratch();
        let locked = dir.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("hit.rs"), "UNIQUE_GREP_LOCKED_DIR\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "UNIQUE_GREP_LOCKED_DIR", "path": "locked"}),
            },
            ToolLimits::default(),
            None,
        );
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Error, "{t}");
        assert!(
            t.to_ascii_lowercase().contains("permission") || t.contains("denied"),
            "{t}"
        );
        assert!(!t.contains("os error"), "{t}");
        assert!(!t.contains("No matches"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn grep_mixed_unreadable_subdir_keeps_other_hits() {
        use std::os::unix::fs::PermissionsExt;
        let (ws, dir) = scratch();
        std::fs::create_dir_all(dir.join("ok")).unwrap();
        std::fs::create_dir_all(dir.join("locked")).unwrap();
        std::fs::write(dir.join("ok/hit.rs"), "UNIQUE_GREP_MIX_HIT\n").unwrap();
        std::fs::write(dir.join("locked/secret.rs"), "UNIQUE_GREP_MIX_HIT\n").unwrap();
        std::fs::set_permissions(dir.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "UNIQUE_GREP_MIX_HIT"}),
            },
            ToolLimits::default(),
            None,
        );
        let _ = std::fs::set_permissions(
            dir.join("locked"),
            std::fs::Permissions::from_mode(0o755),
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Success, "{t}");
        assert!(t.contains("ok/hit.rs"), "{t}");
        assert!(t.contains("UNIQUE_GREP_MIX_HIT"), "{t}");
        assert!(!t.contains("os error"), "{t}");
        assert!(
            t.to_ascii_lowercase().contains("permission")
                || t.contains("denied")
                || t.contains("also:"),
            "{t}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_missing_path_is_error() {
        let (ws, dir) = scratch();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "foo", "path": "missing.rs"}),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Error, "{t}");
        assert!(t.contains("does not exist"), "{t}");
        assert!(!t.contains("No matches"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_missing_directory_is_error() {
        let (ws, dir) = scratch();
        let t = glob_files(
            &ws,
            &call(json!({
                "glob_pattern": "**/*.rs",
                "target_directory": "nosuch",
            })),
            ToolLimits::default(),
        );
        let body = t.joined_text();
        assert_eq!(t.state, ToolState::Error, "{body}");
        assert!(body.contains("does not exist"), "{body}");
        assert!(!body.contains("No files matching"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_invalid_regex_is_error() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("a.rs"), "fn x() {}\n").unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "(", "path": "a.rs"}),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Error, "{t}");
        assert!(
            t.contains("regex") || t.contains("pattern") || t.contains("Error:"),
            "{t}"
        );
        assert!(!t.contains("No matches"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_existing_file_no_match_is_success() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("a.rs"), "fn x() {}\n").unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "zzznotfound", "path": "a.rs"}),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Success, "{t}");
        assert!(t.contains("No matches"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_head_limit_zero_does_not_pretend_no_matches() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("a.rs"), "UNIQUE_GREP_HEAD_ZERO\n").unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({
                    "pattern": "UNIQUE_GREP_HEAD_ZERO",
                    "path": "a.rs",
                    "head_limit": 0,
                }),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Success, "{t}");
        assert!(t.contains("UNIQUE_GREP_HEAD_ZERO"), "{t}");
        assert!(!t.contains("No matches"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_rg_binary_dir_notes_skip() {
        let (ws, dir) = scratch();
        std::fs::create_dir_all(dir.join("bins")).unwrap();
        std::fs::write(dir.join("bins/a.bin"), [0u8, 1, 2, 0]).unwrap();
        std::fs::write(dir.join("bins/b.bin"), [0u8; 32]).unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "UNIQUE_NOPE_RG", "path": "bins"}),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Success, "{t}");
        assert!(t.contains("No matches"), "{t}");
        assert!(t.contains("binary"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn grep_fifo_dir_is_error_not_empty() {
        let (ws, dir) = scratch();
        let pipes = dir.join("pipes");
        std::fs::create_dir_all(&pipes).unwrap();
        let fifo = pipes.join("pipe.txt");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "UNIQUE_NOPE_FIFO", "path": "pipes"}),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Error, "{t}");
        assert!(t.starts_with("Error:"), "{t}");
        assert!(t.contains("special") || t.contains("regular"), "{t}");
        assert!(!t.contains("No matches"), "{t}");
        let _ = std::fs::remove_file(&fifo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_oversized_dir_is_error_not_empty() {
        let (ws, dir) = scratch();
        let mut body = vec![b'x'; (super::super::path::MAX_TEXT_SLURP_BYTES as usize) + 1];
        body.extend_from_slice(b"\nUNIQUE_GREP_OVERSIZE_HIT\n");
        std::fs::write(dir.join("huge.txt"), &body).unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "UNIQUE_GREP_OVERSIZE_HIT"}),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Error, "{t}");
        assert!(t.starts_with("Error:"), "{t}");
        assert!(t.contains("oversized") || t.contains("max"), "{t}");
        assert!(!t.contains("No matches"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_one_file_rejects_binary() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("pic.bin"), b"hello\0world").unwrap();
        let re = Regex::new("hello").unwrap();
        let err = grep_one_file(&ws, "pic.bin", &dir.join("pic.bin"), &re, 20).unwrap_err();
        assert!(err.contains("binary"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_explicit_binary_file_is_error() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("pic.bin"), b"hello\0world").unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "hello", "path": "pic.bin"}),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Error, "{t}");
        assert!(t.contains("binary"), "{t}");
        assert!(!t.contains("No matches"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn grep_unreadable_file_is_error_not_no_matches() {
        use std::os::unix::fs::PermissionsExt;
        let (ws, dir) = scratch();
        let path = dir.join("secret.txt");
        std::fs::write(&path, "UNIQUE_PERM_DENIED_MARKER\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "UNIQUE_PERM_DENIED_MARKER", "path": "secret.txt"}),
            },
            ToolLimits::default(),
            None,
        );
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Error, "{t}");
        assert!(
            t.to_ascii_lowercase().contains("permission") || t.contains("denied"),
            "{t}"
        );
        assert!(!t.contains("os error"), "{t}");
        assert!(!t.contains("No matches"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_explicit_file_nul_after_64k_is_error() {
        let (ws, dir) = scratch();
        // Peek is 64KiB; put the NUL on a later matching line so rg -a
        // would otherwise fold `\0` into the next hop.
        let mut body = b"a\n".repeat(35_000);
        body.extend(b"UNIQUE_LATE_NUL_MARKER\0tail\n");
        std::fs::write(dir.join("late.bin"), &body).unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "UNIQUE_LATE_NUL_MARKER", "path": "late.bin"}),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Error, "{t}");
        assert!(t.contains("binary"), "{t}");
        assert!(!t.contains("No matches"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_directory_finds_file_over_one_megabyte() {
        let (ws, dir) = scratch();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let mut body = "x".repeat(1_500_000);
        body.push_str("\nUNIQUE_GREP_MARKER_OMEGA\n");
        std::fs::write(dir.join("src/big.rs"), body).unwrap();
        std::fs::write(dir.join("src/small.rs"), "fn tiny() {}\n").unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({
                    "pattern": "UNIQUE_GREP_MARKER_OMEGA",
                    "path": "src",
                }),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Success, "{t}");
        assert!(t.contains("UNIQUE_GREP_MARKER_OMEGA"), "{t}");
        assert!(!t.contains("No matches"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_empty_pattern_is_error() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("a.rs"), "fn x() {}\n").unwrap();
        let hit = grep_files(
            &ws,
            &ToolCall {
                id: "t1".into(),
                name: "Grep".into(),
                arguments: json!({"pattern": "   ", "path": "a.rs"}),
            },
            ToolLimits::default(),
            None,
        );
        let t = hit.joined_text();
        assert_eq!(hit.state, ToolState::Error, "{t}");
        assert!(t.contains("No `pattern`"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_empty_pattern_is_error() {
        let (ws, dir) = scratch();
        let t = glob_files(
            &ws,
            &call(json!({"glob_pattern": ""})),
            ToolLimits::default(),
        );
        let body = t.joined_text();
        assert_eq!(t.state, ToolState::Error, "{body}");
        assert!(body.contains("No `glob_pattern`"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn glob_lists_file_symlink() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("real.txt"), "hi\n").unwrap();
        std::os::unix::fs::symlink("real.txt", dir.join("alias.txt")).unwrap();
        let t = glob_files(
            &ws,
            &call(json!({"glob_pattern": "*.txt"})),
            ToolLimits::default(),
        )
        .joined_text();
        assert!(t.contains("real.txt"), "{t}");
        assert!(t.contains("alias.txt"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn glob_lists_fifo() {
        let (ws, dir) = scratch();
        let fifo = dir.join("pipe.fifo");
        let st = std::process::Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(st.success());
        let t = glob_files(
            &ws,
            &call(json!({"glob_pattern": "*.fifo"})),
            ToolLimits::default(),
        )
        .joined_text();
        assert!(t.contains("pipe.fifo"), "{t}");
        assert!(!t.contains("No files matching"), "{t}");
        let _ = std::fs::remove_file(&fifo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn glob_skips_directory_symlink() {
        let (ws, dir) = scratch();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/real.txt"), "hi\n").unwrap();
        std::os::unix::fs::symlink("sub", dir.join("linkdir")).unwrap();
        let t = glob_files(
            &ws,
            &call(json!({"glob_pattern": "**/*.txt"})),
            ToolLimits::default(),
        )
        .joined_text();
        assert!(t.contains("sub/real.txt"), "{t}");
        assert!(
            !t.contains("linkdir/real.txt"),
            "Glob must not walk directory symlinks:\n{t}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn glob_lists_broken_file_symlink() {
        let (ws, dir) = scratch();
        std::os::unix::fs::symlink("missing.txt", dir.join("broken.txt")).unwrap();
        let t = glob_files(
            &ws,
            &call(json!({"glob_pattern": "*.txt"})),
            ToolLimits::default(),
        )
        .joined_text();
        assert!(t.contains("broken.txt"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn grep_fifo_is_error_not_hang() {
        let (ws, dir) = scratch();
        let fifo = dir.join("pipe");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let t = grep_files(
            &ws,
            &call(json!({"pattern": ".", "path": "pipe"})),
            ToolLimits::default(),
            None,
        );
        let body = t.joined_text();
        assert_eq!(t.state, ToolState::Error, "{body}");
        assert!(body.contains("not a regular file"), "{body}");
        let g = glob_files(
            &ws,
            &call(json!({"glob_pattern": "*", "target_directory": "pipe"})),
            ToolLimits::default(),
        );
        let gb = g.joined_text();
        assert_eq!(g.state, ToolState::Error, "{gb}");
        assert!(
            gb.contains("not a directory") || gb.contains("is a file"),
            "{gb}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_explicit_file_over_one_meg_still_hits() {
        let (ws, dir) = scratch();
        let path = dir.join("mid.txt");
        let chunk = vec![b'a'; 1024 * 1024];
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&chunk).unwrap();
            f.write_all(b"\nNEEDLE_UNIQUE_9f3a\n").unwrap();
        }
        let r = grep_files(
            &ws,
            &call(json!({"pattern": "NEEDLE_UNIQUE_9f3a", "path": "mid.txt"})),
            ToolLimits::default(),
            None,
        );
        let t = r.joined_text();
        assert_eq!(r.state, ToolState::Success, "{t}");
        assert!(t.contains("NEEDLE_UNIQUE_9f3a"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_explicit_long_line_file_returns_preview() {
        let (ws, dir) = scratch();
        let path = dir.join("long.txt");
        let mut body = String::from("NEEDLE_LONG_LINE ");
        body.extend(std::iter::repeat('x').take(1_200_000));
        std::fs::write(&path, body).unwrap();
        let started = std::time::Instant::now();
        let r = grep_files(
            &ws,
            &call(json!({"pattern": "NEEDLE_LONG_LINE", "path": "long.txt"})),
            ToolLimits::default(),
            None,
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let t = r.joined_text();
        assert_eq!(r.state, ToolState::Success, "{t}");
        assert!(t.contains("NEEDLE_LONG_LINE"), "{t}");
        assert!(
            t.len() < 50_000,
            "must not dump the whole line: {}",
            t.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
