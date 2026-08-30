//! Compact working-set card: date, OS, git, open editors, files already in
//! the live window. Rule *bodies* are a sibling `[rules]` card (Cursor injects
//! alwaysApply + glob-matched `.cursor/rules`, not filenames).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::sidecar::EditorFile;

const CARD_MAX: usize = 2500;
const RULES_MAX: usize = 12_000;
const RULE_BODY_MAX: usize = 2500;
const SELECTION_MAX: usize = 800;
const GIT_TIMEOUT: Duration = Duration::from_secs(2);
const GIT_LINES: usize = 20;
const WINDOW_PATHS: usize = 12;
const OPEN_FILES: usize = 12;

pub fn card(
    root: &Path,
    window: &[String],
    editor: &[EditorFile],
    compact_git: bool,
) -> Option<String> {
    let mut out = String::from("[workset]");
    out.push('\n');
    out.push_str("when: ");
    out.push_str(&today());
    out.push('\n');
    out.push_str("os: ");
    out.push_str(std::env::consts::OS);
    if let Some(open) = format_open(editor) {
        out.push('\n');
        out.push_str(&open);
    }
    if let Some(g) = git_status(root, compact_git) {
        out.push('\n');
        out.push_str("git:\n");
        out.push_str(&g);
    }
    if let Some(w) = format_window(window) {
        out.push('\n');
        out.push_str("window: ");
        out.push_str(&w);
    }
    if out.len() > CARD_MAX {
        out.truncate(CARD_MAX);
        while !out.is_char_boundary(out.len()) {
            out.pop();
        }
    }
    Some(out)
}

/// alwaysApply + glob-matched Cursor rule bodies. Unmatched files stay a name list.
pub fn rules_card(root: &Path, home: Option<&Path>, window: &[String]) -> Option<String> {
    let rules = load_rules(root, home);
    if rules.is_empty() {
        return None;
    }
    let mut applied = Vec::new();
    let mut available = Vec::new();
    for rule in &rules {
        if rule.always || matches_any(&rule.globs, window) {
            applied.push(rule);
        } else {
            available.push(rule.name.as_str());
        }
    }
    if applied.is_empty() && available.is_empty() {
        return None;
    }
    let mut out = String::from("[rules]");
    for rule in applied {
        let body = clip_chars(rule.body.trim(), RULE_BODY_MAX);
        if body.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str("# ");
        out.push_str(&rule.name);
        out.push('\n');
        out.push_str(&body);
        out.push('\n');
        if out.len() > RULES_MAX {
            break;
        }
    }
    if !available.is_empty() {
        out.push_str("available: ");
        out.push_str(&available.join(", "));
        out.push('\n');
    }
    if out.trim() == "[rules]" {
        return None;
    }
    if out.len() > RULES_MAX {
        out.truncate(RULES_MAX);
        while !out.is_char_boundary(out.len()) {
            out.pop();
        }
    }
    Some(out)
}

struct RuleFile {
    name: String,
    always: bool,
    globs: Vec<String>,
    body: String,
}

fn load_rules(root: &Path, home: Option<&Path>) -> Vec<RuleFile> {
    let mut files = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in rule_dirs(root, home) {
        if !dir.is_dir() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        names.sort();
        for path in names {
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let lower = name.to_ascii_lowercase();
            if !(lower.ends_with(".mdc") || lower.ends_with(".md")) {
                continue;
            }
            if !seen.insert(name.to_string()) {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            files.push(parse_rule(name, &raw));
        }
    }
    let cursorrules = root.join(".cursorrules");
    if cursorrules.is_file() && seen.insert(".cursorrules".into()) {
        if let Ok(raw) = std::fs::read_to_string(&cursorrules) {
            let mut rule = parse_rule(".cursorrules", &raw);
            rule.always = true;
            files.push(rule);
        }
    }
    files
}

fn rule_dirs(root: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(h) = home {
        if let Some(user) = h.parent() {
            dirs.push(user.join(".cursor").join("rules"));
        }
    }
    dirs.push(root.join(".cursor").join("rules"));
    dirs.sort();
    dirs.dedup();
    dirs
}

fn parse_rule(name: &str, raw: &str) -> RuleFile {
    let t = raw.trim_start();
    let Some(rest) = t.strip_prefix("---") else {
        return RuleFile {
            name: name.into(),
            always: false,
            globs: Vec::new(),
            body: raw.to_string(),
        };
    };
    let rest = rest.trim_start_matches(['\r', '\n']);
    let Some((fm, body)) = rest.split_once("\n---") else {
        return RuleFile {
            name: name.into(),
            always: false,
            globs: Vec::new(),
            body: raw.to_string(),
        };
    };
    let mut always = false;
    let mut globs = Vec::new();
    for line in fm.lines() {
        let line = line.trim();
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let val = v.trim().trim_matches('"').trim_matches('\'');
        if key == "alwaysapply" {
            always = val.eq_ignore_ascii_case("true") || val == "1";
        }
        if key == "globs" || key == "glob" {
            let cleaned = val.trim_start_matches('[').trim_end_matches(']');
            for g in cleaned.split(',') {
                let g = g.trim().trim_matches('"').trim_matches('\'').to_string();
                if !g.is_empty() {
                    globs.push(g);
                }
            }
        }
    }
    RuleFile {
        name: name.into(),
        always,
        globs,
        body: body.trim_start_matches(['\r', '\n']).to_string(),
    }
}

fn matches_any(globs: &[String], window: &[String]) -> bool {
    if globs.is_empty() {
        return false;
    }
    window
        .iter()
        .any(|p| globs.iter().any(|g| path_matches(g, p)))
}

fn path_matches(glob: &str, path: &str) -> bool {
    let path = path.replace('\\', "/").trim_start_matches("./").to_string();
    let glob = glob.trim().replace('\\', "/");
    if glob.is_empty() {
        return false;
    }
    let Ok(re) = regex::Regex::new(&glob_to_regex(&glob)) else {
        return path == glob;
    };
    re.is_match(&path)
}

fn glob_to_regex(pattern: &str) -> String {
    let pat = pattern.trim_start_matches("./").to_string();
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

fn format_open(editor: &[EditorFile]) -> Option<String> {
    if editor.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    for (i, f) in editor.iter().take(OPEN_FILES).enumerate() {
        let path = f.path.trim().replace('\\', "/");
        if path.is_empty() {
            continue;
        }
        let mut row = if i == 0 {
            format!("open: {path}")
        } else {
            format!("      {path}")
        };
        if let Some(line) = f.line.filter(|n| *n > 0) {
            row.push(':');
            row.push_str(&line.to_string());
        }
        lines.push(row);
        if let Some(sel) = f.selection.as_deref() {
            let clip = clip_chars(sel.trim(), SELECTION_MAX);
            if !clip.is_empty() {
                lines.push(format!("selected:\n{clip}"));
            }
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
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

fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

fn today() -> String {
    #[cfg(unix)]
    {
        today_unix().unwrap_or_else(today_utc)
    }
    #[cfg(not(unix))]
    {
        today_utc()
    }
}

#[cfg(unix)]
fn today_unix() -> Option<String> {
    unsafe {
        let mut t: libc::time_t = 0;
        if libc::time(&mut t) < 0 {
            return None;
        }
        let mut tm = std::mem::zeroed::<libc::tm>();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return None;
        }
        Some(format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        ))
    }
}

fn today_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    civil_ymd(days)
}

fn civil_ymd(unix_days: u64) -> String {
    let z = unix_days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn git_status(root: &Path, compact: bool) -> Option<String> {
    let stdout = git_at(root, &["status", "--porcelain=v1", "-b"])?;
    let text = String::from_utf8_lossy(&stdout);
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return None;
    }
    if compact {
        let branch = lines[0];
        let dirty = lines.len().saturating_sub(1);
        let mut body = branch.to_string();
        if dirty > 0 {
            body.push_str(&format!(
                "\n{dirty} dirty files. Do not git diff the whole tree."
            ));
        }
        return Some(body);
    }
    let total = lines.len();
    lines.truncate(GIT_LINES);
    let mut body = lines.join("\n");
    if body.trim().is_empty() {
        return None;
    }
    // A 20-line porcelain list still invites `git diff HEAD` (megabytes of
    // dist/vendor). Say it on the card; Shell also skips whole-tree diffs.
    if total > 1 {
        if total > GIT_LINES {
            body.push_str(&format!("\n… {} more.", total - GIT_LINES));
        }
        body.push_str("\nDo not git diff the whole tree.");
    }
    Some(body)
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

    fn git_ok() -> bool {
        Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[test]
    fn civil_epoch_day_is_1970() {
        assert_eq!(civil_ymd(0), "1970-01-01");
    }

    #[test]
    fn empty_workspace_still_has_when() {
        let dir =
            std::env::temp_dir().join(format!("hyper-ws-empty-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = card(&dir, &[], &[], false).unwrap();
        assert!(c.starts_with("[workset]"), "{c}");
        assert!(c.contains("when: "), "{c}");
        assert!(c.contains("os: "), "{c}");
        assert!(!c.contains("git:"), "{c}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn window_and_open_without_git() {
        let dir =
            std::env::temp_dir().join(format!("hyper-ws-win-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = card(
            &dir,
            &["src/a.rs".into(), "src/b.rs".into()],
            &[EditorFile {
                path: "src/a.rs".into(),
                line: Some(12),
                selection: Some("fn a() {}".into()),
            }],
            false,
        )
        .unwrap();
        assert!(c.contains("window: src/a.rs, src/b.rs"), "{c}");
        assert!(c.contains("open: src/a.rs:12"), "{c}");
        assert!(c.contains("fn a() {}"), "{c}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn always_apply_rule_body_is_injected() {
        let dir =
            std::env::temp_dir().join(format!("hyper-ws-rule-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join(".cursor/rules")).unwrap();
        std::fs::write(
            dir.join(".cursor/rules/house.mdc"),
            "---\nalwaysApply: true\n---\n家里猫。别出家门。\n",
        )
        .unwrap();
        let c = rules_card(&dir, None, &[]).unwrap();
        assert!(c.contains("家里猫。别出家门。"), "{c}");
        assert!(c.contains("house.mdc"), "{c}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn glob_rule_waits_for_matching_window() {
        let dir =
            std::env::temp_dir().join(format!("hyper-ws-glob-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join(".cursor/rules")).unwrap();
        std::fs::write(
            dir.join(".cursor/rules/rust.mdc"),
            "---\nglobs: **/*.rs\n---\nuse rustfmt.\n",
        )
        .unwrap();
        let listed = rules_card(&dir, None, &["src/a.ts".into()]).unwrap();
        assert!(listed.contains("available: rust.mdc"), "{listed}");
        assert!(!listed.contains("use rustfmt"), "{listed}");
        let hit = rules_card(&dir, None, &["src/a.rs".into()]).unwrap();
        assert!(hit.contains("use rustfmt."), "{hit}");
        assert!(!hit.contains("available:"), "{hit}");
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
        let c = card(&dir, &["a.rs".into()], &[], false).unwrap();
        assert!(c.contains("git:"), "{c}");
        assert!(c.contains("a.rs"), "{c}");
        assert!(c.contains("Do not git diff the whole tree."), "{c}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn many_dirty_files_count_the_rest() {
        if !git_ok() {
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("hyper-ws-many-{}", uuid::Uuid::new_v4().simple()));
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
        std::fs::write(dir.join("keep.rs"), "fn k() {}\n").unwrap();
        assert!(run(&["add", "keep.rs"]));
        assert!(run(&["commit", "-m", "init"]));
        for i in 0..25 {
            std::fs::write(dir.join(format!("f{i}.rs")), "fn x() {}\n").unwrap();
        }
        let c = card(&dir, &[], &[], false).unwrap();
        assert!(c.contains("… 6 more."), "{c}");
        assert!(c.contains("Do not git diff the whole tree."), "{c}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn im_git_is_a_count_not_a_file_list() {
        if !git_ok() {
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("hyper-ws-im-{}", uuid::Uuid::new_v4().simple()));
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
        std::fs::write(dir.join("keep.rs"), "fn k() {}\n").unwrap();
        assert!(run(&["add", "keep.rs"]));
        assert!(run(&["commit", "-m", "init"]));
        std::fs::write(dir.join("keep.rs"), "fn k() { 1 }\n").unwrap();
        std::fs::write(dir.join("extra.rs"), "fn e() {}\n").unwrap();
        let c = card(&dir, &[], &[], true).unwrap();
        assert!(c.contains("2 dirty files."), "{c}");
        assert!(c.contains("Do not git diff the whole tree."), "{c}");
        assert!(!c.contains("keep.rs"), "IM must not list dirty paths: {c}");
        assert!(!c.contains("extra.rs"), "{c}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
