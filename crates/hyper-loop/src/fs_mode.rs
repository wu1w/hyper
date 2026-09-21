//! Restrict Hyper home files that can hold secrets to owner-only modes.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        match fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
        {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                tighten_dir(dir);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

pub fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    if crate::tools::is_special_file(path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            ensure_private_dir(dir)?;
        }
    }
    #[cfg(unix)]
    {
        let mut opts = OpenOptions::new();
        opts.create(true).write(true).truncate(true).mode(0o600);
        let mut file = opts.open(path)?;
        file.write_all(contents.as_ref())?;
        tighten_file(path);
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, contents)
    }
}

pub fn tighten_file(path: &Path) {
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

pub fn tighten_dir(path: &Path) {
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Owner-only mode on known secret-bearing files under `~/.grok-hyper`.
pub fn tighten_hyper_home(root: &Path) {
    tighten_dir(root);
    for name in [
        "config.toml",
        "MEMORY.md",
        "memory.sqlite",
        "desktop.log",
        "cron.json",
        "AGENT.md",
    ] {
        let p = root.join(name);
        if p.is_file() {
            tighten_file(&p);
        }
    }
    tighten_tree_files(&root.join("memory"));
    tighten_tree_files(&root.join("digest"));
    let hist = root.join("sessions").join("history.sqlite");
    if hist.is_file() {
        tighten_file(&hist);
    }
}

fn tighten_tree_files(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            tighten_dir(&path);
            tighten_tree_files(&path);
        } else if path.is_file() {
            tighten_file(&path);
        }
    }
}

/// Rewrite markdown whose body still contains recognizable secrets.
pub fn scrub_markdown_tree(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scrub_markdown_tree(&path);
            continue;
        }
        let is_md = path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("md"));
        if !is_md {
            continue;
        }
        let Some(text) = crate::tools::read_text_if_regular(&path) else {
            continue;
        };
        let redacted = crate::secrets::redact(&text);
        if redacted != text {
            let _ = write_private(&path, redacted);
        } else {
            tighten_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn write_private_mode_is_0600() {
        let dir =
            std::env::temp_dir().join(format!("hyper-mode-{}", uuid::Uuid::new_v4().simple()));
        let path = dir.join("secret.txt");
        write_private(&path, "x").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_private_fifo_is_error_not_hang() {
        let dir =
            std::env::temp_dir().join(format!("hyper-mode-fifo-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.txt");
        let st = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(st.success());
        let started = std::time::Instant::now();
        let err = write_private(&path, "x").unwrap_err();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "FIFO write_private must not block: {:?}",
            started.elapsed()
        );
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let _ = fs::remove_dir_all(dir);
    }
}
