//! Cross-session (and compacted in-session) recap card.
//!
//! Frozen Cursor `tools[]` must not gain `recall` / `memory_search`. This is a
//! hidden `[history]` user card, same shape as `[workset]`.

use std::fs;
use std::path::Path;

use crate::memory::MemoryStore;
use crate::session::catalog;
use crate::session::event::{SessionEvent, SessionStart};
use crate::session::index::HistoryIndex;
use crate::template::is_hidden_user_text;

const CARD_MAX: usize = 2000;
const SIBLINGS: usize = 4;
const CLIP: usize = 280;
const ARCHIVED: usize = 3;
const MIN_SPOKEN: usize = 40;
const TAIL_BYTES: usize = 512 * 1024;

/// Reads of Hyper's own session store are other chats, not this turn's state.
pub(crate) fn is_session_file_noise(hay: &str) -> bool {
    let l = hay.to_ascii_lowercase();
    l.contains(".grok-hyper/sessions")
        || l.contains("sessions/.current")
        || l.contains("overnight-audit")
        || l.contains("hyper-self-audit")
}

pub(crate) fn is_probe_session(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    [
        "overnight-",
        "overnight_audit",
        "hyper-self-audit",
        "wake-",
        "fold-",
        "write-r",
        "close-r",
        "im-hb",
        "im-live",
        "im-queue",
        "soak-",
    ]
    .iter()
    .any(|p| id.starts_with(p) || id.contains(p))
}

pub fn card(
    session_dir: &Path,
    current_id: &str,
    workspace: &str,
    query: &str,
    recaps: Option<&MemoryStore>,
    current_events: Option<&[SessionEvent]>,
) -> Option<String> {
    let archived = current_events
        .map(|ev| archived_spoken(ev, ARCHIVED))
        .unwrap_or_default();
    let siblings = sibling_rows(session_dir, current_id, workspace, recaps);
    let hits = if wants_recall_search(query) {
        fts_rows(session_dir, current_id, workspace, query, &siblings)
    } else {
        Vec::new()
    };
    if archived.is_empty() && siblings.is_empty() && hits.is_empty() {
        return None;
    }

    let mut out = String::from("[history]\n");
    if query.chars().any(is_cjk) {
        out.push_str(
            "同一工作区里其他会话的摘要，以及本场已压缩的结论。不是当前这条用户消息。\
不要 Read ~/.grok-hyper/sessions 下的 JSONL，也不要读 sessions/.current。\n",
        );
    } else {
        out.push_str(
            "Other chats in this workspace, plus this chat's archived conclusions. \
Not the live user turn. Do not Read ~/.grok-hyper/sessions JSONL or sessions/.current.\n",
        );
    }

    if !archived.is_empty() {
        out.push_str("this chat:\n");
        for line in &archived {
            out.push_str("  ");
            out.push_str(&clip_text(line, CLIP));
            out.push('\n');
        }
    }
    for row in &siblings {
        out.push_str(&row.channel);
        out.push_str(" · ");
        out.push_str(&row.title);
        out.push('\n');
        if !row.clip.is_empty() {
            out.push_str("  ");
            out.push_str(&row.clip);
            out.push('\n');
        }
    }
    for row in &hits {
        if siblings.iter().any(|s| s.id == row.id) {
            continue;
        }
        out.push_str("hit · ");
        out.push_str(&row.channel);
        out.push_str(" · ");
        out.push_str(&row.title);
        out.push('\n');
        if !row.clip.is_empty() {
            out.push_str("  ");
            out.push_str(&row.clip);
            out.push('\n');
        }
    }

    if out.chars().count() > CARD_MAX {
        let mut s: String = out.chars().take(CARD_MAX.saturating_sub(1)).collect();
        while !s.is_char_boundary(s.len()) {
            s.pop();
        }
        s.push('…');
        out = s;
    }
    Some(out)
}

struct Row {
    id: String,
    channel: String,
    title: String,
    clip: String,
}

fn sibling_rows(
    dir: &Path,
    current_id: &str,
    workspace: &str,
    recaps: Option<&MemoryStore>,
) -> Vec<Row> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut cands: Vec<(u64, String, std::path::PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if id == current_id || is_probe_session(id) {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        cands.push((mtime, id.to_string(), path));
    }
    cands.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = Vec::new();
    for (_, id, path) in cands {
        if out.len() >= SIBLINGS {
            break;
        }
        let Some(start) = peek_start(&path) else {
            continue;
        };
        if start.channel == "subagent" || !same_workspace(&start.workspace, workspace) {
            continue;
        }
        let tail = tail_plain(&path);
        let spoken = recap_clip(recaps, &id)
            .or_else(|| tail.as_ref().and_then(|t| t.assistant.clone()))
            .unwrap_or_default();
        let spoken = clip_text(spoken.trim(), CLIP);
        let title_src = tail
            .as_ref()
            .and_then(|t| t.user.clone())
            .unwrap_or_default();
        if spoken.chars().count() < 12 && title_src.chars().count() < 8 {
            continue;
        }
        let titled = catalog::title_from_text(&title_src);
        let title = if titled.is_empty() {
            clip_text(&title_src, 48)
        } else {
            titled
        };
        let channel = if start.channel.is_empty() {
            "cli".into()
        } else {
            start.channel
        };
        out.push(Row {
            id,
            channel,
            title,
            clip: spoken,
        });
    }
    out
}

fn fts_rows(
    dir: &Path,
    current_id: &str,
    workspace: &str,
    query: &str,
    siblings: &[Row],
) -> Vec<Row> {
    let Ok(index) = HistoryIndex::open(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for q in recall_queries(query) {
        let Ok(hits) = index.search(q, None, 8) else {
            continue;
        };
        for h in hits {
            if h.session_id == current_id || is_probe_session(&h.session_id) {
                continue;
            }
            if h.kind != "assistant" && h.kind != "user" {
                continue;
            }
            if is_session_file_noise(&h.snippet) {
                continue;
            }
            if siblings.iter().any(|s| s.id == h.session_id) {
                continue;
            }
            if out.iter().any(|r: &Row| r.id == h.session_id) {
                continue;
            }
            let path = dir.join(format!("{}.jsonl", h.session_id));
            let start = peek_start(&path);
            if let Some(s) = &start {
                if !same_workspace(&s.workspace, workspace) {
                    continue;
                }
            }
            let channel = start
                .as_ref()
                .map(|s| {
                    if s.channel.is_empty() {
                        "cli".into()
                    } else {
                        s.channel.clone()
                    }
                })
                .unwrap_or_else(|| "cli".into());
            out.push(Row {
                id: h.session_id.clone(),
                channel,
                title: clip_text(&h.snippet, 48),
                clip: clip_text(h.snippet.trim(), CLIP),
            });
            if out.len() >= 3 {
                return out;
            }
        }
    }
    out
}

fn same_workspace(a: &str, b: &str) -> bool {
    let a = a.trim().trim_end_matches('/');
    let b = b.trim().trim_end_matches('/');
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(pa), Ok(pb)) => pa == pb,
        _ => false,
    }
}

fn recap_clip(store: Option<&MemoryStore>, session_id: &str) -> Option<String> {
    let store = store?;
    let path = store
        .root()
        .join("memory/chats")
        .join(format!("{session_id}.md"));
    let text = fs::read_to_string(path).ok()?;
    let body = text
        .split("## Assistant\n")
        .nth(1)
        .unwrap_or(text.trim())
        .trim();
    if body.chars().count() < 12 {
        None
    } else {
        Some(body.to_string())
    }
}

struct TailPlain {
    user: Option<String>,
    assistant: Option<String>,
}

fn tail_plain(path: &Path) -> Option<TailPlain> {
    let data = fs::read(path).ok()?;
    let start = data.len().saturating_sub(TAIL_BYTES);
    let slice = &data[start..];
    let text = std::str::from_utf8(slice).ok()?;
    let text = if start > 0 {
        text.find('\n').map(|i| &text[i + 1..]).unwrap_or(text)
    } else {
        text
    };
    let mut user = None;
    let mut assistant = None;
    for line in text.lines() {
        let Ok(ev) = serde_json::from_str::<SessionEvent>(line) else {
            continue;
        };
        match ev {
            SessionEvent::User(u) if !is_hidden_user_text(&u.text) && !u.text.trim().is_empty() => {
                user = Some(u.text);
            }
            SessionEvent::Assistant(a) => {
                let spoken = a.content.trim();
                if spoken.is_empty() || a.tool_calls.is_some() || is_session_file_noise(spoken) {
                    continue;
                }
                assistant = Some(spoken.to_string());
            }
            _ => {}
        }
    }
    Some(TailPlain { user, assistant })
}

fn peek_start(path: &Path) -> Option<SessionStart> {
    let data = fs::read(path).ok()?;
    let text = std::str::from_utf8(&data[..data.len().min(8 * 1024)]).ok()?;
    for line in text.lines() {
        if let Ok(SessionEvent::Start(s)) = serde_json::from_str(line) {
            return Some(s);
        }
    }
    None
}

/// Finals compacted out of the live window. Skip a turn whose tools were
/// other-session JSONL so an overnight recap cannot displace this chat.
fn archived_spoken(events: &[SessionEvent], limit: usize) -> Vec<String> {
    let until = events.iter().rev().find_map(|e| match e {
        SessionEvent::Compact(c) => Some(c.until_seq as usize),
        _ => None,
    });
    let Some(until) = until else {
        return Vec::new();
    };
    let mut turn_noise = false;
    let mut spoken = Vec::new();
    for event in events.iter().take(until.saturating_add(1)) {
        match event {
            SessionEvent::User(u) if !is_hidden_user_text(&u.text) => {
                turn_noise = false;
            }
            SessionEvent::Assistant(a) => {
                if let Some(calls) = &a.tool_calls {
                    for c in calls {
                        if is_session_file_noise(&c.function.arguments) {
                            turn_noise = true;
                        }
                    }
                }
                let spoken_text = a.content.trim();
                if spoken_text.chars().count() >= MIN_SPOKEN
                    && a.tool_calls.is_none()
                    && !turn_noise
                    && !is_session_file_noise(spoken_text)
                {
                    spoken.push(spoken_text.to_string());
                }
                if a.tool_calls.is_none() {
                    turn_noise = false;
                }
            }
            SessionEvent::Tool(t) => {
                if is_session_file_noise(&t.output) {
                    turn_noise = true;
                }
            }
            _ => {}
        }
    }
    let skip = spoken.len().saturating_sub(limit);
    spoken.into_iter().skip(skip).collect()
}

fn wants_recall_search(user: &str) -> bool {
    const MARKS: &[&str] = &[
        "上次",
        "前面",
        "记得",
        "历史",
        "审计",
        "复查",
        "修好",
        "修没",
        "结果",
        "还没",
        "做过",
        "last time",
        "remember",
        "earlier",
        "previous",
        "what did we",
    ];
    let lower = user.to_ascii_lowercase();
    MARKS.iter().any(|m| user.contains(m) || lower.contains(m))
}

fn recall_queries(user: &str) -> Vec<&str> {
    const MARKS: &[&str] = &[
        "审计", "建议", "复查", "沙箱", "结论", "修好", "permit", "compact",
    ];
    let mut out: Vec<&str> = MARKS
        .iter()
        .copied()
        .filter(|m| user.contains(m) || user.to_ascii_lowercase().contains(m))
        .collect();
    if out.is_empty() {
        out.push(user);
    }
    out
}

fn clip_text(s: &str, n: usize) -> String {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= n {
        t
    } else {
        format!(
            "{}…",
            t.chars().take(n.saturating_sub(1)).collect::<String>()
        )
    }
}

fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{4E00}'..='\u{9FFF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{3000}'..='\u{303F}'
            | '\u{3040}'..='\u{30FF}'
            | '\u{FF00}'..='\u{FFEF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::ThinkPolicy;
    use crate::session::event::{CompactEvent, SessionMode};
    use crate::session::log::SessionLog;
    use crate::session::tools_hash;
    use std::path::PathBuf;

    fn tmp() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("hyper-hist-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn start(id: &str, workspace: &str, channel: &str) -> crate::session::event::SessionStart {
        let mut s = crate::session::event::SessionStart::new(
            id,
            workspace,
            SessionMode::Agent,
            "sys",
            tools_hash(&[]),
            ThinkPolicy::agent_default(),
        );
        s.channel = channel.into();
        s
    }

    #[test]
    fn probe_ids_are_skipped() {
        assert!(is_probe_session("hyper-self-audit-r2-1"));
        assert!(is_probe_session("overnight-audit-9"));
        assert!(!is_probe_session("54dd2860edbd47deb59aabd9b05a2d49"));
    }

    #[test]
    fn session_jsonl_paths_are_noise() {
        assert!(is_session_file_noise(
            "read /Users/william/.grok-hyper/sessions/hyper-self-audit-r2.jsonl"
        ));
        assert!(is_session_file_noise("read sessions/.current → 4e658c85"));
        assert!(!is_session_file_noise(
            "read crates/hyper-loop/src/session/mod.rs"
        ));
    }

    #[test]
    fn card_shows_sibling_not_overnight() {
        let dir = tmp();
        let ws = dir.to_string_lossy().to_string();
        let mut sib = SessionLog::create_in(&dir, start("sib-audit", &ws, "feishu")).unwrap();
        sib.append(SessionEvent::user("前面的审计做完了吗？结果发我"))
            .unwrap();
        sib.append(SessionEvent::assistant(
            "做完了。主审计：Shell 不是沙箱，workspace_write_only 管不住命令。",
            "",
            None,
        ))
        .unwrap();
        let mut probe =
            SessionLog::create_in(&dir, start("hyper-self-audit-r2-9", &ws, "console")).unwrap();
        probe
            .append(SessionEvent::user("只做运行时边界测试"))
            .unwrap();
        probe
            .append(SessionEvent::assistant(
                "steer skip-unstarted-tools 已落地。",
                "",
                None,
            ))
            .unwrap();

        let card =
            card(&dir, "fresh", &ws, "你复查一下，看修没修好", None, None).expect("sibling card");
        assert!(card.starts_with("[history]"), "{card}");
        assert!(card.contains("Shell 不是沙箱"), "{card}");
        assert!(card.contains("feishu"), "{card}");
        assert!(
            !card.contains("skip-unstarted") && !card.contains("hyper-self-audit"),
            "probe session leaked: {card}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn archived_finals_skip_overnight_read_turn() {
        let events = vec![
            SessionEvent::Start(start("cur", "/ws", "feishu")),
            SessionEvent::user("审计一遍"),
            SessionEvent::assistant(
                "审计完了。结论：Shell 不是沙箱，workspace_write_only 管不住命令。",
                "",
                None,
            ),
            SessionEvent::user("前面的审计做完了吗？结果发我"),
            SessionEvent::assistant(
                "",
                "",
                Some(vec![crate::session::event::OpenAiToolCall::function(
                    "r",
                    "Read",
                    r#"{"path":"/Users/w/.grok-hyper/sessions/hyper-self-audit-r2.jsonl"}"#,
                )]),
            ),
            SessionEvent::tool("r", "read", "steer skip-unstarted-tools"),
            SessionEvent::assistant(
                "总判：循环大体对上。投机只读。这是运行时边界测试。",
                "",
                None,
            ),
            SessionEvent::Compact(CompactEvent {
                until_seq: 6,
                keep_user_seq: 7,
                summary: "x".into(),
                index: String::new(),
            }),
            SessionEvent::user("你复查一下，看修没修好"),
        ];
        // Compact event is last in the real JSONL; archived_spoken reads until_seq.
        let spoken = archived_spoken(&events, 3);
        assert!(
            spoken.iter().any(|s| s.contains("Shell 不是沙箱")),
            "{spoken:?}"
        );
        assert!(
            !spoken.iter().any(|s| s.contains("投机只读")),
            "overnight recap must not displace this chat: {spoken:?}"
        );
        let empty = tmp();
        let card =
            card(&empty, "cur", "/ws", "看修没修好", None, Some(&events)).expect("this-chat card");
        assert!(card.contains("this chat:"), "{card}");
        assert!(card.contains("Shell 不是沙箱"), "{card}");
        assert!(!card.contains("投机只读"), "{card}");
        let _ = std::fs::remove_dir_all(empty);
    }
}
