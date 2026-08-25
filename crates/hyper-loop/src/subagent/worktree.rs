//! Git worktree isolation for mutating child agents.
//!
//! `isolation=none` and `auto` share the parent cwd so uncommitted drafts and
//! child writes stay where the user is looking. `worktree` requires git, checks
//! out HEAD into `~/.grok-hyper/worktrees/<id>`, and **keeps** that directory
//! after the child finishes (not merged back). Resume reuses the same dest.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::policy::SubagentType;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isolation {
    None,
    Worktree,
    Auto,
}

impl Isolation {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Self::Auto),
            "none" => Ok(Self::None),
            "worktree" => Ok(Self::Worktree),
            other => Err(format!(
                "Error: unknown isolation `{other}` (none|worktree|auto)."
            )),
        }
    }

    pub fn wants_worktree(self, _kind: SubagentType) -> bool {
        matches!(self, Self::Worktree)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Worktree => "worktree",
            Self::Auto => "auto",
        }
    }
}

/// Written after a worktree child returns so [`prune_stale`] will not delete it.
/// Crash leftovers never get this file.
pub(crate) const KEEP_MARK: &str = ".grok-hyper-keep";

pub struct Worktree {
    pub path: PathBuf,
    repo: PathBuf,
}

impl Worktree {
    /// Open an existing dest (resume) or `git worktree add --detach HEAD`.
    /// Returns `(worktree, created)` — `created` is false when the dest was reused.
    pub fn add(repo_hint: &Path, id: &str, home: Option<&Path>) -> Result<(Self, bool), String> {
        let repo = toplevel(repo_hint)?;
        let dest = dest_dir(id, home)?;
        if dest.exists() && git_common_dir(&dest).is_ok() {
            return Ok((Self { path: dest, repo }, false));
        }
        if dest.exists() {
            let _ = remove_at(&repo, &dest);
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("worktree dir: {e}"))?;
        }
        let dest_s = dest.to_string_lossy().into_owned();
        let out = git(&repo, &["worktree", "add", "--detach", &dest_s, "HEAD"])?;
        if !out.status.success() {
            return Err(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok((Self { path: dest, repo }, true))
    }

    pub fn remove(self) {
        let _ = remove_at(&self.repo, &self.path);
    }
}

pub fn mark_keep(path: &Path) {
    let _ = std::fs::write(path.join(KEEP_MARK), b"");
}

fn has_keep(path: &Path) -> bool {
    path.join(KEEP_MARK).is_file()
}

fn dest_dir(id: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    Ok(worktrees_root(home)?.join(safe_segment(id)))
}

fn worktrees_root(home: Option<&Path>) -> Result<PathBuf, String> {
    let root = match home {
        Some(h) => h.to_path_buf(),
        None => crate::config::Config::home_dir().map_err(|e| e.to_string())?,
    };
    Ok(root.join("worktrees"))
}

fn safe_segment(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Drop leftover `~/.grok-hyper/worktrees/<id>` dirs from **crashed** children
/// (no [`KEEP_MARK`]). Finished worktrees are kept so writes and resume survive.
/// `keep` is running child ids (same sanitizing as [`dest_dir`]).
///
/// `home` must be the grok-hyper home. `None` is a no-op so tests and
/// `DispatchCtx::from_workspace` cannot wipe the real `~/.grok-hyper/worktrees`.
pub fn prune_stale(home: Option<&Path>, keep: &[String]) {
    let Some(home) = home else {
        return;
    };
    let Ok(root) = worktrees_root(Some(home)) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let keep: std::collections::HashSet<String> = keep.iter().map(|id| safe_segment(id)).collect();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if keep.contains(name.as_ref()) {
            continue;
        }
        if has_keep(&path) {
            continue;
        }
        match git_common_dir(&path) {
            Ok(common) => {
                let repo = repo_from_git_common(&common);
                let _ = remove_at(&repo, &path);
            }
            Err(_) => {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
}

fn git_common_dir(dir: &Path) -> Result<PathBuf, String> {
    let dir_s = dir
        .to_str()
        .ok_or_else(|| "worktree path is not utf-8".to_string())?;
    let mut cmd = Command::new("git");
    crate::proc_spawn::hide_window(&mut cmd);
    let out = cmd
        .args(["-C", dir_s, "rev-parse", "--git-common-dir"])
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err("not a git worktree".into());
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return Err("not a git worktree".into());
    }
    let p = PathBuf::from(&raw);
    Ok(if p.is_absolute() { p } else { dir.join(p) })
}

fn repo_from_git_common(common: &Path) -> PathBuf {
    if common.file_name().is_some_and(|n| n == ".git") {
        common.parent().unwrap_or(common).to_path_buf()
    } else {
        common.to_path_buf()
    }
}

fn toplevel(hint: &Path) -> Result<PathBuf, String> {
    let hint_s = hint
        .to_str()
        .ok_or_else(|| "workspace path is not utf-8".to_string())?;
    let mut cmd = Command::new("git");
    crate::proc_spawn::hide_window(&mut cmd);
    let out = cmd
        .args(["-C", hint_s, "rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err("not a git repository".into());
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        return Err("not a git repository".into());
    }
    Ok(PathBuf::from(path))
}

fn git(repo: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    let mut cmd = Command::new("git");
    crate::proc_spawn::hide_window(&mut cmd);
    cmd.arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))
}

fn remove_at(repo: &Path, dest: &Path) -> Result<(), String> {
    let dest_s = dest.to_string_lossy().into_owned();
    let out = git(repo, &["worktree", "remove", "--force", &dest_s])?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(dest);
        let _ = git(repo, &["worktree", "prune"]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_and_none_share_cwd() {
        assert!(!Isolation::Auto.wants_worktree(SubagentType::Explore));
        assert!(!Isolation::Auto.wants_worktree(SubagentType::Plan));
        assert!(!Isolation::Auto.wants_worktree(SubagentType::GeneralPurpose));
        assert!(!Isolation::Auto.wants_worktree(SubagentType::Office));
        assert!(!Isolation::None.wants_worktree(SubagentType::GeneralPurpose));
        assert!(Isolation::Worktree.wants_worktree(SubagentType::Explore));
        assert!(Isolation::Worktree.wants_worktree(SubagentType::Office));
    }

    #[test]
    fn parse_accepts_schema_and_rejects_typos() {
        assert_eq!(Isolation::parse("auto").unwrap(), Isolation::Auto);
        assert_eq!(Isolation::parse("").unwrap(), Isolation::Auto);
        assert_eq!(Isolation::parse("none").unwrap(), Isolation::None);
        assert_eq!(Isolation::parse("worktree").unwrap(), Isolation::Worktree);
        assert_eq!(Isolation::Worktree.as_str(), "worktree");
        assert_eq!(Isolation::Auto.as_str(), "auto");
        assert!(Isolation::parse("shared").is_err());
        assert!(Isolation::parse("isolate").is_err());
        assert!(Isolation::parse("wortree").is_err());
        let err = Isolation::parse("git_worktree").unwrap_err();
        assert!(err.contains("none|worktree|auto"), "{err}");
    }

    #[test]
    fn prune_stale_drops_orphans_keeps_running() {
        let home = std::env::temp_dir().join(format!("hyper-wt-{}", uuid::Uuid::new_v4().simple()));
        let orphan = home.join("worktrees").join("dead-id");
        let keep = home.join("worktrees").join("live-id");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::write(orphan.join("x"), "y").unwrap();
        prune_stale(Some(&home), &["live-id".into()]);
        assert!(!orphan.exists());
        assert!(keep.exists());
        std::fs::create_dir_all(&orphan).unwrap();
        mark_keep(&orphan);
        prune_stale(Some(&home), &[]);
        assert!(orphan.exists(), "finished worktrees with KEEP_MARK stay");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn prune_stale_without_home_is_noop() {
        prune_stale(None, &[]);
    }

    #[test]
    fn add_reuses_existing_dest_instead_of_resetting_head() {
        let dir =
            std::env::temp_dir().join(format!("hyper-wt-reuse-{}", uuid::Uuid::new_v4().simple()));
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        assert!(Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
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
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        let (wt, created) = Worktree::add(&dir, "child-1", Some(&home)).unwrap();
        assert!(created);
        std::fs::write(wt.path.join("draft.txt"), "uncommitted in tree").unwrap();
        mark_keep(&wt.path);
        let dest = wt.path.clone();
        let (wt2, created2) = Worktree::add(&dir, "child-1", Some(&home)).unwrap();
        assert!(!created2);
        assert_eq!(wt2.path, dest);
        assert_eq!(
            std::fs::read_to_string(dest.join("draft.txt")).unwrap(),
            "uncommitted in tree"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
