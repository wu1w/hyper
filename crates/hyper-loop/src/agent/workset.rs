//! Compact working-set card: git status, files already in the live window,
//! and workspace rules. Re-injected each user turn; facts only.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CARD_MAX: usize = 1500;
const GIT_TIMEOUT: Duration = Duration::from_secs(2);
const GIT_LINES: usize = 20;
const WINDOW_PATHS: usize = 12;

pub fn card(root: &Path, window: &[String]) -> Option<String> {
    let git = git_status(root);
    let rules = list_rules(root);
    let window_line = format_window(window);
    if git.is_none() && rules.is_empty() && window_line.is_none() {
        return None;
    }
    let mut out = String::from("[workset]");
    if let Some(g) = git {
        out.push('\n');
        out.push_str("git:\n");
        out.push_str(&g);
    }
    if let Some(w) = window_line {
        out.push('\n');
        out.push_str("window: ");
        out.push_str(&w);
    }
    if !rules.is_empty() {
        out.push('\n');
        out.push_str("rules: ");
        out.push_str(&rules.join(", "));
    }
    if out.len() > CARD_MAX {
        out.truncate(CARD_MAX);
        while !out.is_char_boundary(out.len()) {
            out.pop();
        }
    }
    Some(out)
}

fn format_window(window: &[String]) -> Option<String> {
    if window.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = window
        .iter()
        .map(|p| p.trim().replace('\\', "/"))
        .filter(|p| !p.is_empty())
        .collect();
    parts.sort();
    parts.dedup();
    parts.truncate(WINDOW_PATHS);
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

fn list_rules(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if root.join(".cursorrules").is_file() {
        names.push(".cursorrules".into());
    }
    let dir = root.join(".cursor/rules");
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut files: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                let lower = n.to_ascii_lowercase();
                if lower.ends_with(".mdc") || lower.ends_with(".md") {
                    Some(n)
                } else {
                    None
                }
            })
            .collect();
        files.sort();
        names.extend(files.into_iter().take(8));
    }
    names
}

fn git_status(root: &Path) -> Option<String> {
    let stdout = git_at(root, &["status", "--porcelain=v1", "-b"])?;
    let text = String::from_utf8_lossy(&stdout);
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return None;
    }
    lines.truncate(GIT_LINES);
    let body = lines.join("\n");
    if body.trim().is_empty() {
        None
    } else {
        Some(body)
    }
}

fn git_at(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut cmd = Command::new("git");
    crate::proc_spawn::hide_window(&mut cmd);
    cmd.args(["-C"])
        .arg(root)
        .args(["-c", "safe.directory=*"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => {
                let mut buf = Vec::new();
                let _ = child.stdout.as_mut()?.read_to_end(&mut buf);
                return Some(buf);
            }
            Ok(Some(_)) => return None,
            Ok(None) if started.elapsed() > GIT_TIMEOUT => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_ok() -> bool {
        Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[test]
    fn empty_workspace_has_no_card() {
        let dir =
            std::env::temp_dir().join(format!("hyper-ws-empty-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(card(&dir, &[]).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn window_and_rules_without_git() {
        let dir =
            std::env::temp_dir().join(format!("hyper-ws-win-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join(".cursor/rules")).unwrap();
        std::fs::write(dir.join(".cursor/rules/rust.mdc"), "globs: **/*.rs\n").unwrap();
        let c = card(&dir, &["src/a.rs".into(), "src/b.rs".into()]).unwrap();
        assert!(c.starts_with("[workset]"), "{c}");
        assert!(c.contains("window: src/a.rs, src/b.rs"), "{c}");
        assert!(c.contains("rust.mdc"), "{c}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn git_porcelain_in_card() {
        if !git_ok() {
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("hyper-ws-git-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(["-C"])
                .arg(&dir)
                .args(args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .status()
                .unwrap()
                .success()
        };
        assert!(run(&["init"]));
        assert!(run(&["config", "user.email", "t@t"]));
        assert!(run(&["config", "user.name", "t"]));
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        assert!(run(&["add", "a.rs"]));
        assert!(run(&["commit", "-m", "init"]));
        std::fs::write(dir.join("a.rs"), "fn a() { 1 }\n").unwrap();
        let c = card(&dir, &["a.rs".into()]).unwrap();
        assert!(c.contains("git:"), "{c}");
        assert!(c.contains("a.rs"), "{c}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
