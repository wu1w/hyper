//! Workspace upload, tree, and download. Paths stay under the session root.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{bail, Result};
use axum::http::HeaderValue;
use serde::Serialize;
use serde_json::{json, Value};

use hyper_loop::config::user_home;
use hyper_loop::media::{MediaKind, MediaPart, MAX_INLINE_MEDIA_BYTES};
use hyper_loop::out_dir::OUT_DIR;
use hyper_loop::Workspace;

pub const DEFAULT_UPLOAD_CAP: u64 = 10 * 1024 * 1024;
pub const FILE_PUT_CAP: usize = 96 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct Uploaded {
    pub name: String,
    pub path: String,
    pub url: String,
    pub mime: String,
    pub bytes: u64,
    pub kind: String,
    pub content_part: Value,
}

pub fn max_upload(media_max: u64) -> u64 {
    media_max.max(DEFAULT_UPLOAD_CAP)
}

pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let ws = Workspace::open(root, true).map_err(|e| anyhow::anyhow!("{e}"))?;
    // The file manager stays confined even though agent reads were unfenced:
    // `resolve` is the read path and allows `..`; the web file manager's
    // reads AND writes must both go through the confined write check.
    ws.resolve_write(rel).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Overwrite a workspace-relative file (preview save). Atomic tmp + rename.
pub fn write_workspace_file(root: &Path, rel: &str, bytes: &[u8]) -> Result<(String, u64, String)> {
    if bytes.len() > FILE_PUT_CAP {
        bail!("file too large");
    }
    let n = rel.replace('\\', "/");
    let n = n.trim().trim_start_matches("./");
    if n.is_empty() {
        bail!("path required");
    }
    if n.split('/').any(|s| {
        matches!(
            s,
            ".git" | "node_modules" | "target" | "__pycache__" | ".DS_Store"
        )
    }) {
        bail!("refused path");
    }
    let path = safe_join(root, n)?;
    if path.is_dir() {
        bail!("is a directory");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("hyper");
    let tmp = dir.join(format!(
        "{stem}.hypertmp.{}.{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    let sha = hyper_loop::vendor::sha256_hex(bytes);
    Ok((n.to_string(), bytes.len() as u64, sha))
}

pub fn write_upload(workspace: &Path, file_name: &str, bytes: &[u8], cap: u64) -> Result<Uploaded> {
    if bytes.len() as u64 > cap {
        bail!("file exceeds {} bytes", cap);
    }
    let name = sanitize_name(file_name);
    if name.is_empty() {
        bail!("empty filename");
    }
    let rel_dir = PathBuf::from(".grok-hyper/uploads");
    let dest_dir = safe_join(workspace, rel_dir.to_str().unwrap())?;
    fs::create_dir_all(&dest_dir)?;
    let stem = Path::new(&name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = Path::new(&name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let uniq = format!(
        "{}-{}{}",
        stem,
        &uuid::Uuid::new_v4().to_string()[..8],
        if ext.is_empty() {
            String::new()
        } else {
            format!(".{ext}")
        }
    );
    let dest = dest_dir.join(&uniq);
    let mut f = fs::File::create(&dest)?;
    f.write_all(bytes)?;
    let rel = format!(".grok-hyper/uploads/{uniq}").replace('\\', "/");
    let mime = mime_of(&name);
    let kind = kind_of(mime, &name);
    let content_part = content_part(&rel, mime, kind, bytes);
    Ok(Uploaded {
        name,
        path: rel.clone(),
        url: format!("/api/files?path={}", urlencode_rfc3986(&rel)),
        mime: mime.into(),
        bytes: bytes.len() as u64,
        kind: kind.into(),
        content_part,
    })
}

fn content_part(rel: &str, mime: &str, kind: &str, bytes: &[u8]) -> Value {
    match kind {
        "image" | "video" | "audio" if bytes.len() <= MAX_INLINE_MEDIA_BYTES => {
            let part = MediaPart::data_uri(
                match kind {
                    "video" => MediaKind::Video,
                    "audio" => MediaKind::Audio,
                    _ => MediaKind::Image,
                },
                mime,
                bytes,
            );
            json!({
                "type": kind,
                "url": part.url,
                "mime": mime,
            })
        }
        "image" => json!({"type":"image","url": rel, "mime": mime}),
        "video" => json!({"type":"video","url": rel, "mime": mime}),
        "audio" => json!({"type":"audio","url": rel, "mime": mime}),
        _ => json!({"type":"file","file_url": rel, "name": rel}),
    }
}

pub fn list_tree(root: &Path, max_entries: usize) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    walk(root, root, 0, max_entries, &mut out)?;
    Ok(out)
}

/// Files in workspace `out/`. Missing folder → empty list (Q&A has no deliverable).
pub fn list_out_files(root: &Path, max_files: usize) -> Result<Vec<Value>> {
    let dir = root.join(OUT_DIR);
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    let mut files = Vec::new();
    walk_out_files(root, &dir, 0, max_files, &mut files)?;
    files.sort_by(|a, b| {
        let ma = a.get("mtime").and_then(Value::as_u64).unwrap_or(0);
        let mb = b.get("mtime").and_then(Value::as_u64).unwrap_or(0);
        mb.cmp(&ma).then_with(|| {
            let pa = a.get("path").and_then(Value::as_str).unwrap_or("");
            let pb = b.get("path").and_then(Value::as_str).unwrap_or("");
            pa.cmp(pb)
        })
    });
    Ok(files)
}

fn walk_out_files(
    root: &Path,
    dir: &Path,
    depth: usize,
    max_files: usize,
    out: &mut Vec<Value>,
) -> Result<()> {
    if depth > 8 || out.len() >= max_files {
        return Ok(());
    }
    let mut entries: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return Ok(()),
    };
    entries.sort_by_key(|e| e.file_name().to_string_lossy().to_lowercase());
    for ent in entries {
        if out.len() >= max_files {
            break;
        }
        let name = ent.file_name().to_string_lossy().to_string();
        if skip_name(&name) {
            continue;
        }
        let path = ent.path();
        if path.is_dir() {
            walk_out_files(root, &path, depth + 1, max_files, out)?;
            continue;
        }
        let meta = match ent.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        out.push(json!({
            "path": rel,
            "name": name,
            "bytes": meta.len(),
            "mtime": file_mtime_ms(&meta),
        }));
    }
    Ok(())
}

fn file_mtime_ms(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Expand `~` / `~/…`. Other paths are unchanged (relative stays relative).
pub fn expand_user_path(raw: &str) -> PathBuf {
    let t = raw.trim();
    if t.is_empty() {
        return PathBuf::new();
    }
    if t == "~" {
        return user_home().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = t.strip_prefix("~/") {
        if let Some(home) = user_home() {
            return home.join(rest);
        }
    }
    PathBuf::from(t)
}

/// Resolve a typed/pasted folder to an existing directory.
/// Relative paths are joined to `current` (the live workspace) when given.
pub fn resolve_workspace_dir(raw: &str, current: Option<&Path>) -> Result<PathBuf> {
    let expanded = expand_user_path(raw);
    if expanded.as_os_str().is_empty() {
        bail!("empty path");
    }
    let candidate = if expanded.is_absolute() {
        expanded
    } else if let Some(cur) = current {
        cur.join(expanded)
    } else {
        std::env::current_dir()?.join(expanded)
    };
    if !candidate.exists() {
        bail!("文件夹不存在: {}", candidate.display());
    }
    if !candidate.is_dir() {
        bail!("不是文件夹: {}", candidate.display());
    }
    Ok(fs::canonicalize(&candidate).unwrap_or(candidate))
}

/// `hyper web` start root: CLI `--workspace` wins; else saved `[console] workspace`; else cwd.
pub fn resolve_web_workspace(cli: &Path, saved: &str) -> Result<PathBuf> {
    if !cli.as_os_str().is_empty() {
        return Ok(fs::canonicalize(cli).unwrap_or_else(|_| cli.to_path_buf()));
    }
    let saved = saved.trim();
    if !saved.is_empty() {
        match resolve_workspace_dir(saved, None) {
            Ok(p) => return Ok(p),
            Err(_) => {
                eprintln!("hyper web: 已保存的工作区不存在，改用当前目录: {saved}");
            }
        }
    }
    std::env::current_dir().map_err(|e| anyhow::anyhow!("cwd: {e}"))
}

pub fn parent_dir(path: &Path) -> Option<String> {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.display().to_string())
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceShortcut {
    pub id: String,
    pub label: String,
    pub path: String,
}

pub fn workspace_shortcuts() -> Vec<WorkspaceShortcut> {
    let mut out = Vec::new();
    let mut push = |id: &str, label: &str, p: PathBuf| {
        if !p.is_dir() {
            return;
        }
        let path = fs::canonicalize(&p).unwrap_or(p);
        let s = path.display().to_string();
        if out.iter().any(|x: &WorkspaceShortcut| x.path == s) {
            return;
        }
        out.push(WorkspaceShortcut {
            id: id.into(),
            label: label.into(),
            path: s,
        });
    };
    if let Some(home) = user_home() {
        push("home", "主目录", home.clone());
        push("desktop", "桌面", home.join("Desktop"));
        push("desktop-zh", "桌面", home.join("桌面"));
        push("documents", "文稿", home.join("Documents"));
        push("documents-zh", "文档", home.join("文档"));
        push("downloads", "下载", home.join("Downloads"));
        push("downloads-zh", "下载", home.join("下载"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        push("cwd", "启动目录", cwd);
    }
    out
}

/// Immediate child directories of `raw` (or home when empty). For in-console picking.
pub fn list_child_dirs(raw: &str, max: usize) -> Result<(PathBuf, Option<String>, Vec<Value>)> {
    let path = if raw.trim().is_empty() {
        user_home().ok_or_else(|| anyhow::anyhow!("找不到主目录"))?
    } else {
        resolve_workspace_dir(raw, None)?
    };
    let parent = parent_dir(&path);
    let mut entries: Vec<_> = match fs::read_dir(&path) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(e) => bail!("无法读取 {}: {e}", path.display()),
    };
    entries.sort_by_key(|e| e.file_name().to_string_lossy().to_lowercase());
    let mut dirs = Vec::new();
    for ent in entries {
        if dirs.len() >= max {
            break;
        }
        let name = ent.file_name().to_string_lossy().to_string();
        if skip_name(&name) || name.starts_with('.') {
            continue;
        }
        let child = ent.path();
        if !child.is_dir() {
            continue;
        }
        dirs.push(json!({
            "name": name,
            "path": child.display().to_string(),
        }));
    }
    Ok((path, parent, dirs))
}

/// Native OS folder dialog. `Ok(None)` = user cancelled. `Err` = no picker / failed.
///
/// Desktop Electron uses its own UTF-16 dialog and POSTs `/workspace`. This
/// sidecar path is for `hyper web` in a browser. Windows PowerShell 5.1 writes
/// native stdout as ASCII/`$OutputEncoding` unless we emit UTF-8 bytes; a
/// Chinese folder would otherwise arrive as `?` and fail `is_dir()`.
pub fn pick_folder_native() -> Result<Option<PathBuf>> {
    let output = if cfg!(target_os = "macos") {
        Command::new("osascript")
            .args([
                "-e",
                r#"try
  POSIX path of (choose folder with prompt "选择 hyper 工作区文件夹")
on error number -128
  return ""
end try"#,
            ])
            .output()
    } else if cfg!(target_os = "windows") {
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-STA",
                "-NonInteractive",
                "-Command",
                r#"
$ErrorActionPreference = 'Stop'
$utf8 = New-Object System.Text.UTF8Encoding $false
[Console]::OutputEncoding = $utf8
$OutputEncoding = $utf8
Add-Type -AssemblyName System.Windows.Forms
$d = New-Object System.Windows.Forms.FolderBrowserDialog
$d.Description = '选择工作区文件夹'
$d.ShowNewFolderButton = $true
try { $d.SelectedPath = [Environment]::GetFolderPath('UserProfile') } catch {}
if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK -and $d.SelectedPath) {
  $bytes = $utf8.GetBytes($d.SelectedPath)
  [Console]::OpenStandardOutput().Write($bytes, 0, $bytes.Length)
}
"#,
            ])
            .output()
    } else {
        let zenity = Command::new("zenity")
            .args([
                "--file-selection",
                "--directory",
                "--title=选择工作区文件夹",
            ])
            .output();
        match zenity {
            Ok(o) if o.status.success() || o.status.code() == Some(1) => Ok(o),
            _ => Command::new("kdialog")
                .args(["--getexistingdirectory", ".", "选择工作区文件夹"])
                .output(),
        }
    };
    let output = match output {
        Ok(o) => o,
        Err(_) => bail!("这台机器没有系统文件夹对话框，请把路径贴到输入框再点打开"),
    };
    let path = picker_stdout_path(&output.stdout);
    if path.is_empty() {
        if output.status.success() || output.status.code() == Some(1) {
            return Ok(None);
        }
        let err = String::from_utf8_lossy(&output.stderr);
        bail!("文件夹对话框失败: {}", err.trim());
    }
    let p = PathBuf::from(path);
    if !p.is_dir() {
        bail!("选中的路径不是文件夹: {}", p.display());
    }
    Ok(Some(fs::canonicalize(&p).unwrap_or(p)))
}

/// First non-empty stdout line, UTF-8 with an optional BOM. PowerShell used to
/// emit GBK/ASCII here; callers now write UTF-8, but a BOM still appears if
/// `[UTF8Encoding]::new($true)` sneaks in.
pub(crate) fn picker_stdout_path(stdout: &[u8]) -> String {
    let bytes = stdout.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(stdout);
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| l.trim().trim_matches('"'))
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .trim_end_matches(['/', '\\'])
        .to_string()
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    max_entries: usize,
    out: &mut Vec<Value>,
) -> Result<()> {
    if depth > 8 || out.len() >= max_entries {
        return Ok(());
    }
    let mut entries: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return Ok(()),
    };
    entries.sort_by_key(|e| {
        (
            !e.path().is_dir(),
            e.file_name().to_string_lossy().to_lowercase(),
        )
    });
    for ent in entries {
        if out.len() >= max_entries {
            break;
        }
        let name = ent.file_name().to_string_lossy().to_string();
        if skip_name(&name) {
            continue;
        }
        let path = ent.path();
        if hyper_loop::is_reparse_or_symlink(&path) {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let is_dir = path.is_dir();
        out.push(json!({
            "path": rel,
            "name": name,
            "dir": is_dir,
            "bytes": if is_dir { Value::Null } else { json!(ent.metadata().map(|m| m.len()).unwrap_or(0)) },
        }));
        if is_dir {
            walk(root, &path, depth + 1, max_entries, out)?;
        }
    }
    Ok(())
}

pub fn read_preview(root: &Path, rel: &str, cap: usize) -> Result<(String, Vec<u8>, bool)> {
    let path = safe_join(root, rel)?;
    if !path.exists() {
        bail!("not found");
    }
    if path.is_dir() {
        bail!("is a directory");
    }
    // 有界读:最多 cap+1 字节,多出的 1 字节只用来判断截断,
    // 几 GB 的文件不再整个读进内存
    let f = fs::File::open(&path)?;
    let mut body = Vec::new();
    f.take(cap as u64 + 1).read_to_end(&mut body)?;
    let truncated = body.len() > cap;
    if truncated {
        body.truncate(cap);
    }
    let mime = mime_of(path.file_name().and_then(|s| s.to_str()).unwrap_or(""));
    Ok((mime.into(), body, truncated))
}

fn skip_name(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "target"
            | "node_modules"
            | "__pycache__"
            | ".DS_Store"
            | ".venv"
            | "venv"
            | ".pptx-venv"
    )
}

fn sanitize_name(raw: &str) -> String {
    Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .chars()
        .filter(|c| *c != '\0')
        .take(180)
        .collect()
}

fn mime_of(name: &str) -> &'static str {
    let ext = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "text/javascript",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "json" | "jsonl" => "application/json",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "doc" => "application/msword",
        "docx" | "docm" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        }
        "xls" => "application/vnd.ms-excel",
        "xlsx" | "xlsm" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" | "ppsx" | "pptm" => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        }
        "ppt" => "application/vnd.ms-powerpoint",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "pdf" => "application/pdf",
        "vsd" => "application/vnd.visio",
        "vsdx" => "application/vnd.visio",
        "rs" | "ts" | "tsx" | "txt" | "toml" | "py" | "sh" | "bash" | "zsh" | "yaml" | "yml"
        | "log" | "ini" | "cfg" | "sql" | "go" | "java" | "c" | "h" | "hpp" | "cc" | "rb"
        | "env" => "text/plain",
        _ => "application/octet-stream",
    }
}

fn textual_mime(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime == "application/json"
        || mime == "application/xml"
        || mime == "application/javascript"
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
}

/// Browsers default `text/*` without charset to Latin-1 / windows-1252, which
/// turns UTF-8 CJK into mojibake. Always label textual previews as UTF-8.
pub fn file_content_type(mime: &str) -> HeaderValue {
    let value = if textual_mime(mime) {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_string()
    };
    value
        .parse()
        .unwrap_or(HeaderValue::from_static("application/octet-stream"))
}

/// `filename=` is Latin-1-only in HTTP. Non-ASCII names go in `filename*`.
pub fn file_disposition(path: &str) -> HeaderValue {
    file_disposition_kind(path, false)
}

pub fn file_disposition_kind(path: &str, download: bool) -> HeaderValue {
    let name = path.rsplit(['/', '\\']).next().unwrap_or("file");
    let encoded = percent_encode_rfc5987(name);
    let ascii_ok =
        name.is_ascii() && !name.contains(['"', '\\', '\r', '\n', ';']) && !name.is_empty();
    let kind = if download { "attachment" } else { "inline" };
    let value = if ascii_ok {
        format!("{kind}; filename=\"{name}\"; filename*=UTF-8''{encoded}")
    } else {
        format!("{kind}; filename=\"download\"; filename*=UTF-8''{encoded}")
    };
    value
        .parse()
        .unwrap_or(HeaderValue::from_static(if download {
            "attachment"
        } else {
            "inline"
        }))
}

fn percent_encode_rfc5987(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn kind_of(mime: &str, name: &str) -> &'static str {
    if mime.starts_with("image/") {
        "image"
    } else if mime.starts_with("video/") {
        "video"
    } else if mime.starts_with("audio/") {
        "audio"
    } else if MediaKind::parse(
        Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or(""),
    ) == Some(MediaKind::Image)
    {
        "image"
    } else {
        "file"
    }
}

/// RFC 3986:unreserved(字母数字 `-._~`)之外的字节统一百分号编码。
/// 文件名含 `#?%&+` 或空格时链接才不会断;`+` 必须编码,否则
/// urlencoded 解析会把它还原成空格。
fn urlencode_rfc3986(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_parent_escape() {
        let dir = std::env::temp_dir().join(format!("hyper-web-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        assert!(safe_join(&dir, "../etc/passwd").is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_workspace_file_overwrites_and_rejects_dotdot() {
        let dir = std::env::temp_dir().join(format!("hyper-put-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let (p, n, sha) = write_workspace_file(&dir, "out.txt", b"hello").unwrap();
        assert_eq!(p, "out.txt");
        assert_eq!(n, 5);
        assert_eq!(sha.len(), 64);
        assert_eq!(fs::read(dir.join("out.txt")).unwrap(), b"hello");
        write_workspace_file(&dir, "out.txt", b"world").unwrap();
        assert_eq!(fs::read(dir.join("out.txt")).unwrap(), b"world");
        assert!(write_workspace_file(&dir, "../escape.txt", b"x").is_err());
        assert!(write_workspace_file(&dir, ".git/config", b"x").is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn text_preview_declares_utf8() {
        let ct = file_content_type("text/markdown");
        assert_eq!(ct.to_str().unwrap(), "text/markdown; charset=utf-8");
        let json = file_content_type("application/json");
        assert_eq!(json.to_str().unwrap(), "application/json; charset=utf-8");
        let png = file_content_type("image/png");
        assert_eq!(png.to_str().unwrap(), "image/png");
    }

    #[test]
    fn python_and_yaml_are_text() {
        assert_eq!(mime_of("probe.py"), "text/plain");
        assert_eq!(mime_of("note.yaml"), "text/plain");
    }

    #[test]
    fn web_assets_use_browser_mimes() {
        assert_eq!(mime_of("styles.css"), "text/css");
        assert_eq!(mime_of("app.js"), "text/javascript");
        assert_eq!(mime_of("index.html"), "text/html");
        assert_eq!(
            file_content_type("text/css").to_str().unwrap(),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            file_content_type("text/javascript").to_str().unwrap(),
            "text/javascript; charset=utf-8"
        );
    }

    #[test]
    fn urlencode_covers_reserved_bytes() {
        assert_eq!(urlencode_rfc3986("a b"), "a%20b");
        assert_eq!(urlencode_rfc3986("a#b?c%d&e+f"), "a%23b%3Fc%25d%26e%2Bf");
        assert_eq!(urlencode_rfc3986("dir/file"), "dir%2Ffile");
        assert_eq!(urlencode_rfc3986("ok-._~09AZaz"), "ok-._~09AZaz");
        assert_eq!(urlencode_rfc3986("审"), "%E5%AE%A1");
    }

    #[test]
    fn read_preview_is_bounded() {
        let dir = std::env::temp_dir().join(format!("hyper-web-cap-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("big.txt"), vec![b'x'; 100]).unwrap();
        let (_, body, truncated) = read_preview(&dir, "big.txt", 10).unwrap();
        assert_eq!(body.len(), 10);
        assert!(truncated);
        let (_, body, truncated) = read_preview(&dir, "big.txt", 100).unwrap();
        assert_eq!(body.len(), 100);
        assert!(!truncated);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn chinese_filename_uses_rfc5987() {
        let d = file_disposition("reports/审计.md");
        let s = d.to_str().unwrap();
        assert!(s.contains("filename*=UTF-8''"), "{s}");
        assert!(s.contains("%E5%AE%A1%E8%AE%A1.md"), "{s}");
        assert!(!s.contains("审计"), "{s}");
        assert!(s.starts_with("inline;"), "{s}");
        let att = file_disposition_kind("shot.png", true);
        let a = att.to_str().unwrap();
        assert!(a.starts_with("attachment;"), "{a}");
        assert!(a.contains("filename=\"shot.png\""), "{a}");
    }

    #[test]
    fn expand_home_tilde() {
        let home = hyper_loop::config::user_home().expect("home");
        assert_eq!(expand_user_path("~"), home);
        assert_eq!(expand_user_path("~/notes"), home.join("notes"));
        assert_eq!(expand_user_path("/abs/path"), PathBuf::from("/abs/path"));
    }

    #[test]
    fn resolve_workspace_rejects_file_and_missing() {
        let dir = std::env::temp_dir().join(format!("hyper-ws-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("only.txt"), b"x").unwrap();
        assert!(resolve_workspace_dir(&dir.join("only.txt").display().to_string(), None).is_err());
        assert!(resolve_workspace_dir(&dir.join("nope").display().to_string(), None).is_err());
        let got = resolve_workspace_dir(&dir.display().to_string(), None).unwrap();
        assert_eq!(got, fs::canonicalize(&dir).unwrap());
        let rel = resolve_workspace_dir(".", Some(&dir)).unwrap();
        assert_eq!(rel, fs::canonicalize(&dir).unwrap());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_child_dirs_skips_files() {
        let dir = std::env::temp_dir().join(format!("hyper-ls-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("file.txt"), b"x").unwrap();
        let (path, _parent, kids) = list_child_dirs(&dir.display().to_string(), 50).unwrap();
        assert_eq!(path, fs::canonicalize(&dir).unwrap());
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0]["name"], "sub");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_out_files_empty_without_folder() {
        let dir = std::env::temp_dir().join(format!("hyper-out-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        assert!(list_out_files(&dir, 50).unwrap().is_empty());
        fs::create_dir_all(dir.join("out/demo")).unwrap();
        fs::write(dir.join("out/demo/index.html"), b"<p>ok</p>").unwrap();
        fs::write(dir.join("out/skip.py"), b"print(1)\n").unwrap();
        let rows = list_out_files(&dir, 50).unwrap();
        let paths: Vec<_> = rows
            .iter()
            .filter_map(|r| r.get("path").and_then(|v| v.as_str()))
            .collect();
        assert!(paths.contains(&"out/demo/index.html"), "{paths:?}");
        assert!(paths.contains(&"out/skip.py"), "{paths:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn picker_stdout_keeps_chinese_and_strips_bom() {
        let raw = "C:\\Users\\张三\\文档\r\n".as_bytes();
        assert_eq!(picker_stdout_path(raw), "C:\\Users\\张三\\文档");
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice("/Users/张三/文稿/\n".as_bytes());
        assert_eq!(picker_stdout_path(&bom), "/Users/张三/文稿");
        assert_eq!(picker_stdout_path(b"\n\n"), "");
        assert_eq!(
            picker_stdout_path("\"/tmp/项目\"\n".as_bytes()),
            "/tmp/项目"
        );
    }

    #[test]
    fn resolve_and_list_chinese_workspace() {
        let dir = std::env::temp_dir().join(format!("hyper-中文-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("子文件夹")).unwrap();
        fs::write(dir.join("说明.txt"), b"ok").unwrap();
        let got = resolve_workspace_dir(&dir.display().to_string(), None).unwrap();
        assert_eq!(got, fs::canonicalize(&dir).unwrap());
        let (_path, _parent, kids) = list_child_dirs(&dir.display().to_string(), 50).unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0]["name"], "子文件夹");
        assert!(kids[0]["path"].as_str().unwrap().contains("子文件夹"));
        let up = write_upload(&dir, "季度报告.docx", b"PK", 10_000).unwrap();
        assert_eq!(up.name, "季度报告.docx");
        assert!(up.path.contains("季度报告"));
        fs::remove_dir_all(&dir).ok();
    }
}
