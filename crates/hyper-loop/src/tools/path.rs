//! Workspace path resolution. Relative paths join the root; `..` is lexical.
//!
//! Confinement is symlink-aware: for confined workspaces the resolved path is
//! canonicalized (existing parts, following symlinks) before the within-root
//! check, so an in-repo symlink pointing out of the repo cannot smuggle a
//! read/write past the root.

use std::path::{Component, Path, PathBuf};

use crate::error::Result;

/// Tool-facing I/O text: kinds we know, then strip ` (os error N)`.
pub(crate) fn io_user_msg(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "the path does not exist".into(),
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        std::io::ErrorKind::AlreadyExists => "file exists".into(),
        std::io::ErrorKind::IsADirectory => "is a directory".into(),
        _ => strip_os_error(&e.to_string()),
    }
}

/// ripgrep / OS stderr still says `(os error N)`; strip it for the model.
pub(crate) fn strip_os_error(s: &str) -> String {
    match s.find(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// True for symlinks and Windows directory junctions. Walkers must skip these
/// or a junction under the workspace (or a user profile) can recurse into
/// AppData / the whole volume and freeze the machine.
pub fn is_reparse_or_symlink(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// FIFOs / sockets / devices exist but `read`/`write` block until a peer
/// opens the other end. Regular files, directories, and missing paths are
/// not special.
pub fn is_special_file(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() || meta.is_dir() => false,
        Ok(_) => true,
        Err(_) => false,
    }
}

/// Quoted `'…'` / `"…"` bits that look like file paths, for `python3 -c`
/// / `run_code` preflight. Identifiers like `os` are skipped.
pub fn quoted_path_literals(code: &str) -> Vec<String> {
    let bytes = code.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\'' || c == b'"' {
            let q = c;
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != q {
                if bytes[i] == b'\\' {
                    i = i.saturating_add(2);
                    continue;
                }
                i += 1;
            }
            let end = i.min(code.len());
            if start <= end {
                let s = &code[start..end];
                if looks_like_rel_path(s) {
                    out.push(s.to_string());
                }
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    out
}

fn looks_like_rel_path(s: &str) -> bool {
    if s.is_empty() || s.len() > 512 || s.contains('\n') || s.contains('\0') {
        return false;
    }
    if s.starts_with('-') {
        return false;
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return false;
    }
    s.contains('/') || s.contains('\\') || s.contains('.')
}

/// First quoted path in `text` that exists as a FIFO/socket/device under cwd.
pub fn first_special_quoted_path(cwd: &Path, text: &str) -> Option<String> {
    for rel in quoted_path_literals(text) {
        let candidate = joins_cwd(cwd, &rel);
        if is_special_file(&candidate) {
            return Some(rel);
        }
    }
    None
}

/// First quoted path in `text` that exists as a regular file over the slurp cap.
pub fn first_oversized_quoted_path(cwd: &Path, text: &str) -> Option<String> {
    for rel in quoted_path_literals(text) {
        let candidate = joins_cwd(cwd, &rel);
        if is_oversized_text(&candidate) {
            return Some(rel);
        }
    }
    None
}

fn looks_like_file_open_api(text: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "open(",
        "readFile(",
        "readFileSync(",
        "file_get_contents(",
        ".read_text(",
        ".read_bytes(",
        "File.read(",
        "File.binread(",
        "File.open(",
        "IO.binread(",
        "io.open(",
        "os.open(",
        "np.load(",
        "numpy.load(",
        "cv2.imread(",
        "sqlite3.connect(",
        "ZipFile(",
        "pickle.load(",
        "createReadStream(",
        "createWriteStream(",
        "fs.readFile(",
        "fs.writeFile(",
        "fs.appendFile(",
        "fs.open(",
        "IO.read(",
        "Deno.readFile(",
        "Bun.file(",
        "shutil.copy(",
        "shutil.copyfile(",
        "shutil.copy2(",
        "shutil.move(",
        ".write_text(",
        ".write_bytes(",
        "pd.read_",
        "pandas.read_",
        "tarfile.open(",
        "cv2.VideoCapture(",
        "h5py.File(",
    ];
    NEEDLES.iter().any(|n| text.contains(n)) || looks_like_dynamic_spawn(text)
}

/// `os.system('cat '+os.listdir('.')[0])` never writes `open(` but still hangs
/// on a leftover FIFO. `os.system('echo hi')` beside a pipe must still run.
fn looks_like_dynamic_spawn(text: &str) -> bool {
    const SPAWN: &[&str] = &[
        "os.system(",
        "os.popen(",
        "os.exec",
        "os.spawn",
        "subprocess.",
        "Popen(",
    ];
    const DYN: &[&str] = &["listdir", "scandir", "glob(", "os.walk", "[0]"];
    SPAWN.iter().any(|s| text.contains(s)) && DYN.iter().any(|d| text.contains(d))
}

fn joins_cwd(cwd: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// First FIFO/socket/device in `dir` (not recursive). Overnight leftover
/// pipes sitting next to `open(p)` / `os.listdir` would otherwise hang.
pub fn first_special_in_dir(dir: &Path) -> Option<String> {
    let rd = std::fs::read_dir(dir).ok()?;
    for (i, ent) in rd.enumerate() {
        if i > 256 {
            break;
        }
        let Ok(ent) = ent else {
            continue;
        };
        if is_special_file(&ent.path()) {
            return ent.file_name().to_str().map(str::to_string);
        }
    }
    None
}

/// `open(p)` / `open(os.listdir(...)[0])` with a FIFO in cwd and no quoted
/// regular file to open. Quoted `open('notes.md')` still runs when notes.md
/// is a real file, even if a leftover pipe sits beside it.
pub fn first_special_unquoted_open(cwd: &Path, text: &str) -> Option<String> {
    if !looks_like_file_open_api(text) {
        return None;
    }
    let opens_named_regular = quoted_path_literals(text).iter().any(|rel| {
        let p = joins_cwd(cwd, rel);
        p.is_file() && !is_special_file(&p)
    });
    if opens_named_regular {
        return None;
    }
    first_special_in_dir(cwd)
}

/// Full UTF-8 slurp cap for Read/StrReplace/Write-prior. Bigger files Error
/// (Read) or are skipped (snapshot) instead of loading gigabytes into RAM.
pub const MAX_TEXT_SLURP_BYTES: u64 = 8 * 1024 * 1024;

pub fn is_oversized_text(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() > MAX_TEXT_SLURP_BYTES => true,
        _ => false,
    }
}

/// Read a regular file without blocking on FIFO/socket (unix `O_NONBLOCK`).
/// `max` is a hard byte cap; bigger files Error instead of slurping.
pub fn read_bytes_capped(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    if is_special_file(path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.is_file() && meta.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "too large",
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            match f.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    if (buf.len() + n) as u64 > max {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "too large",
                        ));
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if is_special_file(path) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "not a regular file",
                        ));
                    }
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        return Ok(buf);
    }
    #[cfg(not(unix))]
    {
        use std::io::Read;
        let mut f = std::fs::File::open(path)?;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            match f.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    if (buf.len() + n) as u64 > max {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "too large",
                        ));
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(buf)
    }
}

/// Open for read without blocking on FIFO/socket (unix `O_NONBLOCK`).
/// Callers that already checked `is_special_file` still need this: the
/// path can become a FIFO between the stat and the open.
pub fn open_read_nonblock(path: &Path) -> std::io::Result<std::fs::File> {
    if is_special_file(path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::File::open(path)
    }
}

/// Text-tool slurp: regular file, ≤8MiB, unix non-blocking.
pub fn read_bytes_regular(path: &Path) -> std::io::Result<Vec<u8>> {
    read_bytes_capped(path, MAX_TEXT_SLURP_BYTES)
}

/// UTF-8 slurp that never blocks on FIFO/socket/device or loads >8MiB.
pub fn read_text_if_regular(path: &Path) -> Option<String> {
    let bytes = read_bytes_regular(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Skip FIFO/socket/device instead of blocking overnight pulse writers.
/// Unix open is `O_NONBLOCK` so a FIFO swapped in after the stat fails fast.
pub fn write_if_regular(path: &Path, bytes: impl AsRef<[u8]>) -> std::io::Result<()> {
    if is_special_file(path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?;
        f.write_all(bytes.as_ref())?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

/// `rename(tmp, symlink)` replaces the link. Follow an existing file symlink
/// so Write/StrReplace update the target, matching Cursor.
fn follow_existing_file(path: PathBuf) -> PathBuf {
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(&path).unwrap_or(path),
        _ => path,
    }
}

fn is_not_a_directory_err(e: &std::io::Error) -> bool {
    e.to_string()
        .to_ascii_lowercase()
        .contains("not a directory")
        || e.raw_os_error() == Some(20)
}

#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
    confined: bool,
}

impl Workspace {
    pub fn open(root: impl AsRef<Path>, confined: bool) -> Result<Self> {
        let root = root.as_ref();
        std::fs::create_dir_all(root)?;
        Ok(Self {
            root: root.canonicalize()?,
            confined,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn display(&self) -> String {
        self.root.display().to_string()
    }

    pub fn resolve(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        if raw.is_empty() {
            return Err("Error: No `path` provided.".into());
        }
        let joined = {
            let p = Path::new(raw);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                self.root.join(p)
            }
        };
        let normalized = lexical_normalize(&joined);
        Ok(normalized)
    }

    /// Writes stay inside the workspace when `confined` (`workspace_write_only`).
    /// Reads/Glob/Grep/Shell cwd use [`Self::resolve`] so absolute paths work
    /// like Hermes.
    pub fn resolve_write(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        let normalized = self.resolve(raw)?;
        if self.confined {
            self.check_confined(&normalized)
        } else {
            Ok(follow_existing_file(normalized))
        }
    }

    /// Path `Delete` unlinks. Cursor `rm` removes the named link, not the
    /// target. Confined: the parent must canonicalize inside the workspace so
    /// `hole/secret` through an outbound dir symlink is still rejected, while
    /// `leak.txt -> /outside` can be removed without touching `/outside`.
    pub fn resolve_unlink(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        let normalized = self.resolve(raw)?;
        if !self.confined {
            return Ok(normalized);
        }
        let Some(name) = normalized.file_name() else {
            return Err(format!("Error: {raw} is a directory. Delete only files."));
        };
        let parent = match normalized.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => {
                return Err(format!(
                    "Error: path `{}` is outside the workspace.",
                    normalized.display()
                ));
            }
        };
        match std::fs::canonicalize(parent) {
            Ok(real_parent) => {
                if !is_within(&real_parent, &self.root) {
                    return Err(format!(
                        "Error: path `{}` is outside the workspace (resolves to {}).",
                        normalized.display(),
                        real_parent.join(name).display()
                    ));
                }
                Ok(real_parent.join(name))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !is_within(&normalized, &self.root) {
                    return Err(format!(
                        "Error: path `{}` is outside the workspace.",
                        normalized.display()
                    ));
                }
                Ok(normalized)
            }
            Err(e) => Err(format!(
                "Error: cannot resolve `{}`: {}",
                normalized.display(),
                e
            )),
        }
    }

    /// Symlink-aware confinement check. Returns the path read/write must use.
    ///
    /// `canonicalize` only works on paths that exist, so:
    /// - existing path → canonicalize it (follows symlinks at every level)
    ///   and require the result to sit under the canonical root;
    /// - missing path (fresh write) → canonicalize the deepest *existing*
    ///   ancestor and re-append the absent tail lexically. This still catches
    ///   a symlinked directory: `hole -> /outside` with `hole/new.txt`
    ///   canonicalizes `hole` to `/outside` and fails the check, even though
    ///   `hole/new.txt` does not exist yet.
    fn check_confined(&self, normalized: &Path) -> std::result::Result<PathBuf, String> {
        let mut probe = normalized.to_path_buf();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        loop {
            match std::fs::canonicalize(&probe) {
                Ok(real) => {
                    let mut full = real;
                    for name in tail.iter().rev() {
                        full.push(name);
                    }
                    return if is_within(&full, &self.root) {
                        Ok(full)
                    } else {
                        Err(format!(
                            "Error: path `{}` is outside the workspace (resolves to {}).",
                            normalized.display(),
                            full.display()
                        ))
                    };
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    match (probe.file_name(), probe.parent()) {
                        (Some(name), Some(parent)) => {
                            tail.push(name.to_os_string());
                            probe = parent.to_path_buf();
                        }
                        _ => return Ok(probe),
                    }
                }
                Err(e) if is_not_a_directory_err(&e) => {
                    let shown = probe
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .unwrap_or("path");
                    return Err(format!("Error: {shown} is not a directory."));
                }
                Err(e) => {
                    return Err(format!(
                        "Error: cannot resolve `{}`: {}",
                        normalized.display(),
                        e
                    ));
                }
            }
        }
    }

    /// Path the model should see: the argument it sent, not a host absolute.
    pub fn shown(&self, raw: &str) -> String {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return trimmed.to_string();
        }
        let p = Path::new(trimmed);
        if !p.is_absolute() {
            return trimmed.to_string();
        }
        match self.resolve(trimmed) {
            Ok(resolved) => resolved
                .strip_prefix(&self.root)
                .ok()
                .map(|rel| {
                    let s = rel.to_string_lossy();
                    if s.is_empty() {
                        ".".to_string()
                    } else {
                        s.replace('\\', "/")
                    }
                })
                .unwrap_or_else(|| trimmed.to_string()),
            Err(_) => trimmed.to_string(),
        }
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Component-wise containment, *not* string prefix.
/// `Path::strip_prefix` compares whole path components, so
/// `/workspace-evil` is NOT within `/workspace` even though the byte string
/// shares the prefix. Never reimplement this with `str::starts_with`.
fn is_within(path: &Path, root: &Path) -> bool {
    path == root || path.strip_prefix(root).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hyper-path-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn io_user_msg_strips_os_error() {
        let denied = std::io::Error::from_raw_os_error(13);
        let s = io_user_msg(&denied);
        assert!(!s.contains("os error"), "{s}");
        let missing = std::io::Error::from_raw_os_error(2);
        assert_eq!(io_user_msg(&missing), "the path does not exist");
        assert_eq!(
            strip_os_error("rg: /tmp/x: Permission denied (os error 13)"),
            "rg: /tmp/x: Permission denied"
        );
    }

    #[test]
    fn is_within_is_component_based_not_string_prefix() {
        assert!(is_within(Path::new("/workspace"), Path::new("/workspace")));
        assert!(is_within(
            Path::new("/workspace/a.txt"),
            Path::new("/workspace")
        ));
        assert!(!is_within(
            Path::new("/workspace-evil"),
            Path::new("/workspace")
        ));
        assert!(!is_within(
            Path::new("/workspace-evil/a"),
            Path::new("/workspace")
        ));
        assert!(!is_within(Path::new("/other"), Path::new("/workspace")));
    }

    #[test]
    fn missing_paths_stay_lexical_new_writes_allowed() {
        let d = temp_dir("missing");
        let ws = Workspace::open(&d, true).unwrap();
        let p = ws.resolve("brand/new/file.txt").unwrap();
        assert!(p.starts_with(ws.root()));
        assert!(p.ends_with("brand/new/file.txt"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn lexical_dotdot_escape_is_rejected() {
        let d = temp_dir("dotdot");
        let ws = Workspace::open(&d, true).unwrap();
        let err = ws.resolve_write("a/../../outside.txt").unwrap_err();
        assert!(err.contains("outside the workspace"), "{err}");
        assert!(ws.resolve("a/../../outside.txt").is_ok());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn confined_reads_allow_absolute_outside() {
        let d = temp_dir("read-out");
        let outside =
            std::env::temp_dir().join(format!("hyper-path-read-out-{}", uuid::Uuid::new_v4()));
        std::fs::write(&outside, "ok").unwrap();
        let ws = Workspace::open(&d, true).unwrap();
        let p = ws.resolve(outside.to_str().unwrap()).unwrap();
        assert_eq!(p, outside);
        let err = ws.resolve_write(outside.to_str().unwrap()).unwrap_err();
        assert!(err.contains("outside the workspace"), "{err}");
        std::fs::remove_dir_all(&d).ok();
        std::fs::remove_file(&outside).ok();
    }

    #[cfg(unix)]
    #[test]
    fn in_repo_symlink_pointing_out_is_rejected() {
        use std::os::unix::fs::symlink;

        let d = temp_dir("sym");
        let outside =
            std::env::temp_dir().join(format!("hyper-path-outside-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("secret.txt");
        std::fs::write(&outside_file, "top secret").unwrap();

        let ws = Workspace::open(&d, true).unwrap();

        let leak = d.join("leak.txt");
        symlink(&outside_file, &leak).unwrap();
        let err = ws.resolve_write("leak.txt").unwrap_err();
        assert!(err.contains("outside the workspace"), "{err}");
        assert!(ws.resolve("leak.txt").is_ok());

        let hole = d.join("hole");
        symlink(&outside, &hole).unwrap();
        let err = ws.resolve_write("hole/new.txt").unwrap_err();
        assert!(err.contains("outside the workspace"), "{err}");

        let inner = d.join("inner.txt");
        std::fs::write(&inner, "ok").unwrap();
        let good = d.join("good.txt");
        symlink(&inner, &good).unwrap();
        let p = ws.resolve("good.txt").unwrap();
        assert!(is_within(&p, ws.root()));

        let ws_open = Workspace::open(&d, false).unwrap();
        assert!(ws_open.resolve("leak.txt").is_ok());
        let followed = ws_open.resolve_write("good.txt").unwrap();
        assert_eq!(followed, std::fs::canonicalize(&inner).unwrap());

        let unlinked = ws.resolve_unlink("leak.txt").unwrap();
        assert_eq!(
            unlinked,
            std::fs::canonicalize(&d).unwrap().join("leak.txt")
        );
        assert_ne!(unlinked, std::fs::canonicalize(&outside_file).unwrap());
        let through_hole = ws.resolve_unlink("hole/secret.txt").unwrap_err();
        assert!(
            through_hole.contains("outside the workspace"),
            "{through_hole}"
        );

        std::fs::remove_dir_all(&d).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn reparse_is_false_for_regular_paths() {
        let d = temp_dir("reparse-reg");
        let f = d.join("a.txt");
        std::fs::write(&f, "x").unwrap();
        assert!(!is_reparse_or_symlink(&f));
        assert!(!is_reparse_or_symlink(&d));
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn reparse_detects_symlink() {
        let d = temp_dir("reparse-link");
        let f = d.join("a.txt");
        std::fs::write(&f, "x").unwrap();
        let link = d.join("l");
        std::os::unix::fs::symlink(&f, &link).unwrap();
        assert!(is_reparse_or_symlink(&link));
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn special_file_detects_fifo_not_regular() {
        let d = temp_dir("special-fifo");
        let fifo = d.join("pipe");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        assert!(is_special_file(&fifo));
        assert!(!is_special_file(&d));
        let regular = d.join("a.txt");
        std::fs::write(&regular, "x").unwrap();
        assert!(!is_special_file(&regular));
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn read_text_if_regular_skips_fifo() {
        let d = temp_dir("read-text-fifo");
        let fifo = d.join("pipe");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let started = std::time::Instant::now();
        assert!(read_text_if_regular(&fifo).is_none());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "FIFO slurp must not block: {:?}",
            started.elapsed()
        );
        let regular = d.join("a.txt");
        std::fs::write(&regular, "hi").unwrap();
        assert_eq!(read_text_if_regular(&regular).as_deref(), Some("hi"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn read_bytes_regular_fifo_does_not_hang() {
        let d = temp_dir("read-bytes-fifo");
        let fifo = d.join("pipe");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let started = std::time::Instant::now();
        let err = read_bytes_regular(&fifo).unwrap_err();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "FIFO read must not block: {:?}",
            started.elapsed()
        );
        assert!(err.to_string().contains("not a regular file"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn open_read_nonblock_fifo_does_not_hang() {
        let d = temp_dir("open-read-fifo");
        let fifo = d.join("pipe");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let started = std::time::Instant::now();
        let err = open_read_nonblock(&fifo).unwrap_err();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "FIFO open must not block: {:?}",
            started.elapsed()
        );
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let regular = d.join("a.txt");
        std::fs::write(&regular, "hello").unwrap();
        let mut f = open_read_nonblock(&regular).unwrap();
        let mut buf = String::new();
        use std::io::Read;
        f.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "hello");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn read_bytes_capped_respects_custom_max() {
        let d = temp_dir("read-bytes-cap");
        let p = d.join("nine.bin");
        std::fs::write(&p, b"123456789").unwrap();
        let err = read_bytes_capped(&p, 8).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
        assert_eq!(read_bytes_capped(&p, 9).unwrap(), b"123456789");
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn write_if_regular_fifo_is_error_not_hang() {
        let d = temp_dir("write-if-fifo");
        let fifo = d.join("pipe");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let started = std::time::Instant::now();
        let err = write_if_regular(&fifo, b"x").unwrap_err();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "FIFO write must not block: {:?}",
            started.elapsed()
        );
        assert!(err.to_string().contains("not a regular file"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn quoted_path_literals_skip_identifiers() {
        assert_eq!(
            quoted_path_literals(r#"open("pipe.txt").read()"#),
            vec!["pipe.txt".to_string()]
        );
        assert!(quoted_path_literals("print('os')").is_empty());
        assert_eq!(
            quoted_path_literals(r#"open("/tmp/x")"#),
            vec!["/tmp/x".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unquoted_open_errors_when_cwd_has_fifo() {
        let d = temp_dir("unquoted-open-fifo");
        let fifo = d.join("pipe.txt");
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        assert_eq!(
            first_special_unquoted_open(&d, "open(p)").as_deref(),
            Some("pipe.txt")
        );
        assert_eq!(
            first_special_unquoted_open(&d, "open(os.listdir('.')[0])").as_deref(),
            Some("pipe.txt")
        );
        std::fs::write(d.join("notes.md"), "ok").unwrap();
        assert!(first_special_unquoted_open(&d, "open('notes.md')").is_none());
        assert!(first_special_unquoted_open(&d, "print(1)").is_none());
        assert!(first_special_unquoted_open(&d, "os.system('echo hi')").is_none());
        assert_eq!(
            first_special_unquoted_open(&d, "os.system('cat '+os.listdir('.')[0])").as_deref(),
            Some("pipe.txt")
        );
        assert_eq!(
            first_special_unquoted_open(&d, "require('fs').createReadStream(p)").as_deref(),
            Some("pipe.txt")
        );
        assert_eq!(
            first_special_unquoted_open(&d, "pd.read_csv(p)").as_deref(),
            Some("pipe.txt")
        );
        assert_eq!(
            first_special_unquoted_open(&d, "shutil.copy(p, 'x')").as_deref(),
            Some("pipe.txt")
        );
        assert_eq!(
            first_special_unquoted_open(&d, "tarfile.open(p)").as_deref(),
            Some("pipe.txt")
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn first_oversized_quoted_path_detects_slurp() {
        let d = temp_dir("oversized-quoted");
        let huge = d.join("huge.txt");
        std::fs::write(&huge, vec![b'x'; (MAX_TEXT_SLURP_BYTES as usize) + 1]).unwrap();
        std::fs::write(d.join("notes.md"), "ok").unwrap();
        assert_eq!(
            first_oversized_quoted_path(&d, "open('huge.txt').read()").as_deref(),
            Some("huge.txt")
        );
        assert!(first_oversized_quoted_path(&d, "open('notes.md').read()").is_none());
        std::fs::remove_dir_all(&d).ok();
    }
}
