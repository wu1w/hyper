//! `Glob` / `Grep`. Workspace walk with skip dirs; no extra indexer.

use std::fs;
use std::path::{Path, PathBuf};

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
    "__pycache__",
    ".venv",
    "venv",
    ".grok-hyper",
    "blobs",
    "AppData",
    "Application Data",
    "Local Settings",
    "Library",
    "Caches",
    "OneDrive",
];

const GLOB_CAP: usize = 200;
const GREP_CAP: usize = 80;
const MAX_FILE_BYTES: u64 = 1_048_576;

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
    walk_files(&root, ws.root(), &mut |rel, _abs| {
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
        text.push_str(&format!("\n… truncated at {GLOB_CAP} paths."));
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
    let ignore_case = matches!(
        call.arguments.get("-i").or_else(|| call.arguments.get("i")),
        Some(Value::Bool(true))
    ) || arg_str(&call.arguments, "-i")
        .or_else(|| arg_str(&call.arguments, "i"))
        .is_some_and(|s| s == "true" || s == "1");
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
    let file_glob = arg_str(&call.arguments, "glob")
        .or_else(|| arg_str(&call.arguments, "glob_pattern"))
        .and_then(|p| GlobMatcher::new(&p).ok());
    let root = match grep_root(ws, &call.arguments) {
        Ok(p) => p,
        Err(e) => return ToolResponse::text(&call.id, e, ToolState::Error),
    };
    let cap = arg_u32(&call.arguments, "head_limit")
        .unwrap_or(GREP_CAP as u32)
        .min(200) as usize;
    let mut lines = Vec::new();
    if root.is_file() {
        let rel = arg_path(&call.arguments)
            .map(|raw| ws.shown(&raw))
            .unwrap_or_else(|| {
                root.strip_prefix(ws.root())
                    .unwrap_or(&root)
                    .to_string_lossy()
                    .replace('\\', "/")
            });
        match grep_one_file(ws, &rel, &root, &re, cap) {
            Ok(hits) => lines = hits,
            Err(e) => return ToolResponse::text(&call.id, e, ToolState::Error),
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
        return folded_response(&call.id, text, ToolState::Success, limits, blobs);
    }
    walk_files(&root, ws.root(), &mut |rel, abs| {
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
    let mut out = String::from("^");
    let chars: Vec<char> = pat.chars().collect();
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
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' => {
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
    out.push('$');
    out
}

fn walk_files(dir: &Path, root: &Path, visit: &mut dyn FnMut(&str, &Path) -> bool) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let Ok(entries) = fs::read_dir(&cur) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                if SKIP_DIR.contains(&name_s.as_ref()) {
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
                return;
            }
        }
    }
}
