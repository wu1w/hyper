//! Console scheduled jobs. Not an OpenAI tool — the agent writes
//! `{workspace}/.grok-hyper/cron.json`, and `hyper web` fires `turn.start`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const WORKSPACE_REL: &str = ".grok-hyper/cron.json";

/// Former always-on system line. Kept for tests / slash help; live turns
/// inject [`CRON_CARD`] only when the user query matches [`wants_cron_card`].
pub const CRON_SYSTEM_LINE: &str = "\
Scheduled jobs: write `.grok-hyper/cron.json` (jobs: id, name, interval_s, \
prompt, enabled; interval_s is seconds). They appear on the console Scheduled \
jobs page. Do not use system crontab.";

/// Hidden user card when the live query is about scheduling.
pub const CRON_CARD: &str = "\
[console-cron]
Write workspace `.grok-hyper/cron.json`; do not use system crontab.
Shape: {\"jobs\":[{\"id\":\"name\",\"name\":\"name\",\"interval_s\":3600,\"prompt\":\"what to do\",\"enabled\":true}]}
interval_s is seconds, not a five-field cron expression. After writing, jobs appear on the console Scheduled jobs page.";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub prompt: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub last_run: Option<u64>,
}

fn default_interval() -> u64 {
    3600
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FileShape {
    #[serde(default)]
    jobs: Vec<CronJob>,
}

pub fn workspace_path(workspace: impl AsRef<Path>) -> PathBuf {
    workspace.as_ref().join(WORKSPACE_REL)
}

pub fn wants_cron_card(user: &str) -> bool {
    let t = user.to_ascii_lowercase();
    [
        "cron",
        "crontab",
        "interval_s",
        "定时",
        "定时任务",
        "每隔",
        "每小时",
        "web-cron",
        ".grok-hyper/cron",
    ]
    .iter()
    .any(|k| t.contains(k))
}

pub fn load_jobs(path: impl AsRef<Path>) -> Vec<CronJob> {
    let Ok(raw) = fs::read_to_string(path.as_ref()) else {
        return Vec::new();
    };
    parse_jobs_json(&raw)
}

pub fn parse_jobs_json(raw: &str) -> Vec<CronJob> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    if let Ok(file) = serde_json::from_str::<FileShape>(raw) {
        return file.jobs;
    }
    serde_json::from_str::<Vec<CronJob>>(raw).unwrap_or_default()
}

pub fn save_jobs(path: impl AsRef<Path>, jobs: &[CronJob]) -> Result<()> {
    let path = path.as_ref();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = FileShape {
        jobs: jobs.to_vec(),
    };
    fs::write(
        path,
        serde_json::to_string_pretty(&file).map_err(Error::msg)?,
    )?;
    Ok(())
}

pub fn upsert(list: &mut Vec<CronJob>, mut add: CronJob) {
    if add.id.trim().is_empty() {
        add.id = slug(&add.name);
    }
    if add.name.trim().is_empty() {
        add.name = add.id.clone();
    }
    if add.interval_s == 0 {
        add.interval_s = default_interval();
    }
    if let Some(prev) = list.iter().find(|p| p.id == add.id) {
        add.last_run = match (prev.last_run, add.last_run) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, b) => b,
        };
    }
    if let Some(i) = list.iter().position(|p| p.id == add.id) {
        list[i] = add;
    } else {
        list.push(add);
    }
}

pub fn remove(list: &mut Vec<CronJob>, id: &str) {
    list.retain(|j| j.id != id && j.name != id);
}

/// `/cron` and `/cron add name interval prompt…` / `/cron rm id`.
pub fn apply_slash(workspace: impl AsRef<Path>, args: &str) -> String {
    let path = workspace_path(workspace);
    let mut jobs = load_jobs(&path);
    let args = args.trim();
    if args.is_empty() || args.eq_ignore_ascii_case("list") {
        return list_text(&path, &jobs);
    }
    let (cmd, rest) = match args.split_once(char::is_whitespace) {
        Some((c, r)) => (c.to_ascii_lowercase(), r.trim().to_string()),
        None => (args.to_ascii_lowercase(), String::new()),
    };
    match cmd.as_str() {
        "add" => match parse_add(&rest) {
            Ok(job) => {
                let id = job.id.clone();
                upsert(&mut jobs, job);
                if let Err(e) = save_jobs(&path, &jobs) {
                    return format!("cron: write failed ({e})");
                }
                format!("cron: saved `{id}` → {}", path.display())
            }
            Err(e) => e,
        },
        "rm" | "remove" | "delete" => {
            if rest.is_empty() {
                return "cron: /cron rm <id>".into();
            }
            let before = jobs.len();
            remove(&mut jobs, &rest);
            if jobs.len() == before {
                return format!("cron: no job `{rest}`");
            }
            if let Err(e) = save_jobs(&path, &jobs) {
                return format!("cron: write failed ({e})");
            }
            format!("cron: removed `{rest}`")
        }
        _ => list_text(&path, &jobs),
    }
}

fn list_text(path: &Path, jobs: &[CronJob]) -> String {
    let mut s = format!(
        "定时任务 {}\n/cron add <name> <seconds|30m|1h|1d> <prompt>\n/cron rm <id>\n\n",
        path.display()
    );
    if jobs.is_empty() {
        s.push_str("(empty — write this file or use /cron add)\n");
        return s;
    }
    for j in jobs {
        s.push_str(&format!(
            "- {}  {}s  {}  {}\n",
            j.id,
            j.interval_s,
            if j.enabled { "on" } else { "off" },
            if j.prompt.is_empty() {
                "(no prompt)"
            } else {
                j.prompt.as_str()
            }
        ));
    }
    s
}

fn parse_add(rest: &str) -> std::result::Result<CronJob, String> {
    let mut parts = rest.splitn(3, char::is_whitespace);
    let name = parts.next().unwrap_or("").trim();
    let interval = parts.next().unwrap_or("").trim();
    let prompt = parts.next().unwrap_or("").trim();
    if name.is_empty() || interval.is_empty() || prompt.is_empty() {
        return Err("cron: /cron add <name> <seconds|30m|1h|1d> <prompt>".into());
    }
    let interval_s = parse_interval(interval)
        .ok_or_else(|| format!("cron: bad interval `{interval}` (use seconds, 30m, 1h, 1d)"))?;
    Ok(CronJob {
        id: slug(name),
        name: name.to_string(),
        interval_s,
        prompt: prompt.to_string(),
        enabled: true,
        last_run: None,
    })
}

pub fn parse_interval(raw: &str) -> Option<u64> {
    let t = raw.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    let (n, mult) = if let Some(n) = t.strip_suffix('s') {
        (n, 1u64)
    } else if let Some(n) = t.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = t.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = t.strip_suffix('d') {
        (n, 86400)
    } else {
        (t.as_str(), 1)
    };
    n.parse::<u64>().ok().filter(|v| *v > 0).map(|v| v * mult)
}

fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "job".into()
    } else {
        s
    }
}

/// Cheap workspace sensor for host heartbeat. No model call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspacePulse {
    pub fingerprint: String,
    pub dirty: bool,
    /// HEARTBEAT.md has body. Prompt material only — a standing file must not
    /// wake the model every interval ([`heartbeat_tick`] uses the fingerprint).
    pub scripted: bool,
    pub summary: String,
}

/// Host heartbeat after a due timer. Keep overnight jobs from empty-spinning
/// on a standing HEARTBEAT.md, without silencing an explicit `/loop` prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeartbeatTick {
    /// First fingerprint only. No model call.
    Prime,
    /// Tree unchanged and no standing `/loop` prompt.
    SkipQuiet,
    /// Fire a heartbeat turn (tree changed, or the user set a `/loop` prompt).
    Fire,
}

pub fn heartbeat_tick(last_fp: &str, pulse_fp: &str, custom_prompt: bool) -> HeartbeatTick {
    let primed = !last_fp.is_empty();
    let same = primed && last_fp == pulse_fp;
    if !primed {
        if custom_prompt {
            HeartbeatTick::Fire
        } else {
            HeartbeatTick::Prime
        }
    } else if same && !custom_prompt {
        HeartbeatTick::SkipQuiet
    } else {
        HeartbeatTick::Fire
    }
}

fn git_out(root: &Path, args: &[&str]) -> String {
    use std::io::Read;
    let mut child = match std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            return String::new();
        }
    };
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(_) => break,
        }
    }
    let buf = reader.join().unwrap_or_default();
    String::from_utf8_lossy(&buf).trim().to_string()
}

fn tree_stamp(root: &Path) -> String {
    let rd = match fs::read_dir(root) {
        Ok(rd) => rd,
        Err(_) => return String::new(),
    };
    let mut entries: Vec<String> = Vec::new();
    for e in rd.flatten().take(64) {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if let Some(secs) = path_mtime_secs(&e.path()) {
            entries.push(format!("{name}:{secs}"));
        }
    }
    entries.sort();
    entries.join("\n")
}

fn path_mtime_secs(p: &Path) -> Option<u64> {
    let m = fs::metadata(p).ok()?;
    m.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

pub fn workspace_pulse(root: &Path) -> WorkspacePulse {
    let porcelain = git_out(root, &["status", "--porcelain", "-b"]);
    let head = git_out(root, &["rev-parse", "HEAD"]);
    let mut watch = String::new();
    for rel in [
        "HEARTBEAT.md",
        ".grok-hyper/HEARTBEAT.md",
        "AGENT.md",
        "USER.md",
        ".grok-hyper/inbox",
    ] {
        if let Some(secs) = path_mtime_secs(&root.join(rel)) {
            watch.push_str(&format!("{rel}:{secs}\n"));
        }
    }
    let git = !head.is_empty();
    let stamp = if git {
        String::new()
    } else {
        tree_stamp(root)
    };
    let raw = format!("{porcelain}\n{head}\n{watch}\n{stamp}");
    let fingerprint = crate::vendor::sha256_hex(raw.as_bytes());
    let git_dirty = porcelain
        .lines()
        .any(|l| !l.starts_with("##") && !l.trim().is_empty());
    let has_heartbeat_file = ["HEARTBEAT.md", ".grok-hyper/HEARTBEAT.md"]
        .iter()
        .any(|rel| {
            fs::read_to_string(root.join(rel))
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        });
    let dirty = git_dirty;
    let summary = if dirty {
        let lines: Vec<_> = porcelain
            .lines()
            .filter(|l| !l.starts_with("##"))
            .take(20)
            .collect();
        format!("workspace changed:\n{}", lines.join("\n"))
    } else {
        "workspace unchanged".into()
    };
    WorkspacePulse {
        fingerprint,
        dirty,
        scripted: has_heartbeat_file,
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_array_or_object() {
        let a = parse_jobs_json(
            r#"[{"id":"n","name":"n","interval_s":60,"prompt":"x","enabled":true}]"#,
        );
        assert_eq!(a[0].id, "n");
        let b = parse_jobs_json(r#"{"jobs":[{"id":"m","prompt":"y","enabled":true}]}"#);
        assert_eq!(b[0].id, "m");
        assert_eq!(b[0].interval_s, 3600);
    }

    #[test]
    fn interval_suffixes() {
        assert_eq!(parse_interval("60"), Some(60));
        assert_eq!(parse_interval("30m"), Some(1800));
        assert_eq!(parse_interval("1h"), Some(3600));
        assert_eq!(parse_interval("1d"), Some(86400));
    }

    #[test]
    fn workspace_pulse_nongit_tracks_file_mtime() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-pulse-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        let a = workspace_pulse(&dir);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(dir.join("a.txt"), "two").unwrap();
        let b = workspace_pulse(&dir);
        assert_ne!(a.fingerprint, b.fingerprint, "non-git edit must change pulse");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn interval_zero_is_none() {
        assert_eq!(parse_interval("0"), None);
    }

    #[test]
    fn slash_add_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("hyper-cron-{}", crate::session::new_session_id()));
        fs::create_dir_all(&dir).unwrap();
        let msg = apply_slash(&dir, "add nightly 1h 跑 clippy");
        assert!(msg.contains("nightly"), "{msg}");
        let jobs = load_jobs(workspace_path(&dir));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].interval_s, 3600);
        assert!(jobs[0].enabled);
        assert_eq!(jobs[0].prompt, "跑 clippy");
        apply_slash(&dir, "rm nightly");
        assert!(load_jobs(workspace_path(&dir)).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn card_keywords() {
        assert!(wants_cron_card("帮我写个 cron"));
        assert!(wants_cron_card("加一个定时任务"));
        assert!(!wants_cron_card("修一下编译错误"));
    }

    #[test]
    fn heartbeat_tick_primes_then_skips_quiet_tree() {
        let fp = "abc";
        assert_eq!(
            heartbeat_tick("", fp, false),
            HeartbeatTick::Prime,
            "first sample must not spend a model hop"
        );
        assert_eq!(
            heartbeat_tick(fp, fp, false),
            HeartbeatTick::SkipQuiet,
            "standing HEARTBEAT.md must not empty-spin"
        );
        assert_eq!(
            heartbeat_tick(fp, "def", false),
            HeartbeatTick::Fire,
            "tree change must wake"
        );
    }

    #[test]
    fn heartbeat_tick_custom_loop_fires_on_quiet_tree() {
        let fp = "abc";
        assert_eq!(heartbeat_tick("", fp, true), HeartbeatTick::Fire);
        assert_eq!(
            heartbeat_tick(fp, fp, true),
            HeartbeatTick::Fire,
            "/loop prompt is a standing overnight job, not an empty wake"
        );
    }

    #[test]
    fn pulse_ignores_session_sidecar_mtime() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-pulse-sess-{}",
            crate::session::new_session_id()
        ));
        fs::create_dir_all(dir.join(".grok-hyper/sessions")).unwrap();
        let a = workspace_pulse(&dir);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(dir.join(".grok-hyper/sessions/s1.jsonl"), "x\n").unwrap();
        let b = workspace_pulse(&dir);
        assert_eq!(
            a.fingerprint, b.fingerprint,
            "session jsonl must not retrigger heartbeat"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pulse_non_git_is_stable_until_watch_file_changes() {
        let dir = std::env::temp_dir().join(format!(
            "hyper-pulse-{}",
            crate::session::new_session_id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let a = workspace_pulse(&dir);
        let b = workspace_pulse(&dir);
        assert_eq!(a.fingerprint, b.fingerprint);
        assert!(!a.dirty);
        assert!(!a.scripted);
        fs::write(dir.join("HEARTBEAT.md"), "check inbox\n").unwrap();
        let c = workspace_pulse(&dir);
        assert_ne!(a.fingerprint, c.fingerprint);
        assert!(c.scripted);
        fs::create_dir_all(dir.join(".grok-hyper")).unwrap();
        fs::write(dir.join(".grok-hyper/scratch"), "agent log\n").unwrap();
        let d = workspace_pulse(&dir);
        assert_eq!(
            c.fingerprint, d.fingerprint,
            "session dir mtime must not wake the heartbeat"
        );
        let _ = fs::remove_dir_all(dir);
    }
}
