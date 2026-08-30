//! Hidden user notes stored after the live query so Qwen Jinja
//! `last_query_index` stays on the request. Grok Responses hoists kept
//! cards back in front of that user (see `hoist_hidden_notes_before_query`).
//! Attach 0 or 1 skill, at most one MEMORY card, and at most one MCP card,
//! then stub after two later real user turns.

use crate::family::Family;
use crate::template::{is_hidden_user_text, wrap_tool_response, ChatMessage};
use crate::tokenize::count_tokens;

/// Real user turns after a note before it becomes `applied`.
pub const STUB_AFTER_USERS: usize = 2;

/// Interactive narration. Re-injected each real user turn once the old copy
/// stubs, so the instruction stays one live card (~30 tokens) at any time.
pub const STYLE_CARD: &str =
    "[style] Narrate as you work: before each tool batch, one short sentence (≤20 words) in the user's language on what comes next. End with the result only.";
pub const SKILL_BODY_MAX_TOKENS: u32 = 400;
pub const AGENTS_MD_MAX_TOKENS: u32 = 400;
pub const MEMORY_HOT_MAX_LINES: usize = 12;
pub const MEMORY_FULL_MAX_LINES: usize = 40;

pub fn tokens(text: &str) -> u32 {
    count_tokens(Family::Grok46, text)
        .unwrap_or_else(|_| text.chars().count().div_ceil(2).max(1) as u32)
}

pub fn clip_to_tokens(text: &str, max: u32) -> String {
    if tokens(text) <= max {
        return text.trim_end().to_string();
    }
    let mut out = String::new();
    for line in text.lines() {
        let cand = if out.is_empty() {
            line.to_string()
        } else {
            format!("{out}\n{line}")
        };
        if tokens(&cand) > max {
            break;
        }
        out = cand;
    }
    out
}

/// `/pdf args` becomes a TurnStart prompt the loop can split.
pub fn skill_turn_prompt(name: &str, args: &str) -> String {
    let args = args.trim();
    if args.is_empty() {
        format!("[skill:{name}]")
    } else {
        format!("[skill:{name}]\n{args}")
    }
}

pub fn mcp_turn_prompt(name: &str, args: &str) -> String {
    let args = args.trim();
    if args.is_empty() {
        format!("[mcp:{name}]")
    } else {
        format!("[mcp:{name}]\n{args}")
    }
}

pub fn split_skill_prefix(text: &str) -> (Option<String>, String) {
    split_tagged_prefix(text, "[skill:")
}

pub fn split_mcp_prefix(text: &str) -> (Option<String>, String) {
    split_tagged_prefix(text, "[mcp:")
}

fn split_tagged_prefix(text: &str, tag: &str) -> (Option<String>, String) {
    let t = text.trim();
    let Some(rest) = t.strip_prefix(tag) else {
        return (None, text.to_string());
    };
    let (name, tail) = match rest.split_once(']') {
        Some((n, r)) => (n.trim(), r.trim_start_matches('\n').to_string()),
        None => return (None, text.to_string()),
    };
    if name.is_empty() {
        return (None, text.to_string());
    }
    (Some(name.to_string()), tail)
}

pub fn is_sticky_note(text: &str) -> bool {
    let inner = unwrap_hidden(text);
    inner.starts_with("[skill:")
        || inner.starts_with("[mcp:")
        || inner.starts_with("[mcp]")
        || inner.starts_with("[style]")
        || inner.starts_with("[out]")
        || inner.starts_with("[workset]")
        || inner.starts_with("[rules]")
        || inner.starts_with("[im]")
        || inner.starts_with("[history]")
        || inner.starts_with("MEMORY hot")
        || inner.starts_with("MEMORY hosts")
        || inner.starts_with("MEMORY.md")
}

fn unwrap_hidden(text: &str) -> &str {
    let t = text.trim();
    t.strip_prefix("<tool_response>")
        .and_then(|s| s.strip_suffix("</tool_response>"))
        .map(str::trim)
        .unwrap_or(t)
}

fn stub_body(inner: &str) -> String {
    if let Some(rest) = inner.strip_prefix("[skill:") {
        if let Some(name) = rest.split(']').next() {
            return format!("[skill: {}] applied", name.trim());
        }
    }
    if inner.starts_with("MEMORY hot") {
        return "MEMORY hot applied".into();
    }
    if inner.starts_with("MEMORY hosts") {
        return "MEMORY hosts applied".into();
    }
    if inner.starts_with("MEMORY.md") {
        return "MEMORY.md applied".into();
    }
    if let Some(rest) = inner.strip_prefix("[mcp:") {
        if let Some(name) = rest.split(']').next() {
            return format!("[mcp: {}] applied", name.trim());
        }
    }
    if inner.starts_with("[mcp]") {
        return "[mcp] applied".into();
    }
    if inner.starts_with("[style]") {
        return "[style] applied".into();
    }
    if inner.starts_with("[out]") {
        return "[out] applied".into();
    }
    if inner.starts_with("[workset]") {
        return "[workset] applied".into();
    }
    if inner.starts_with("[rules]") {
        return "[rules] applied".into();
    }
    if inner.starts_with("[im]") {
        return "[im] applied".into();
    }
    if inner.starts_with("[history]") {
        return "[history] applied".into();
    }
    "applied".into()
}

/// Replace expired skill/memory notes in the live window. JSONL is unchanged.
/// 返回实际替换条数：原位改写会击穿前缀缓存，调用侧据此打观测 note。
pub fn stub_expired_notes(messages: &mut [ChatMessage]) -> usize {
    stub_notes(messages, StubWhen::AfterUsers(STUB_AFTER_USERS), |_| true)
}

/// User explicitly switched (`[skill:…]` / FAILED testhook). Don't wait two turns.
pub fn stub_live_skill_notes(messages: &mut [ChatMessage]) -> usize {
    stub_notes(messages, StubWhen::Now, |inner| {
        inner.starts_with("[skill:")
    })
}

/// Fresh `[out]` card each user turn.
pub fn stub_live_out_notes(messages: &mut [ChatMessage]) -> usize {
    stub_notes(messages, StubWhen::Now, |inner| inner.starts_with("[out]"))
}

/// Fresh `[workset]` / `[rules]` cards each user turn.
pub fn stub_live_workset_notes(messages: &mut [ChatMessage]) -> usize {
    stub_notes(messages, StubWhen::Now, |inner| {
        inner.starts_with("[workset]") || inner.starts_with("[rules]")
    })
}

/// Fresh `[history]` card each user turn.
pub fn stub_live_history_notes(messages: &mut [ChatMessage]) -> usize {
    stub_notes(messages, StubWhen::Now, |inner| {
        inner.starts_with("[history]")
    })
}

/// Recency closer on IM `instructions`. Language lock belongs here, not in
/// `DEFAULT_AGENT_MD` (desktop CoT can stay English). Chinese last so grok
/// actually reasons in CJK; the English lines still cover English inbound.
pub const IM_SYSTEM_LOCK_ZH: &str = "用户写中文时，思考过程必须是中文，禁止英文推理。";
pub const IM_SYSTEM_LOCK: &str = "\
This is a messaging channel. Think and reply in the user's language. \
If they wrote Chinese, reasoning must be Chinese, not English. \
用户写中文时，思考过程必须是中文，禁止英文推理。";

/// Resume uses frozen `session/start.system`. Feishu chats keep one JSONL for
/// days; without this, a lock added after the first message never reaches grok.
pub fn ensure_im_system_lock(system: &mut String) {
    if system.contains(IM_SYSTEM_LOCK_ZH) {
        return;
    }
    if !system.is_empty() && !system.ends_with('\n') {
        system.push('\n');
    }
    if system.contains("This is a messaging channel. Think and reply in the user's language.") {
        system.push_str(IM_SYSTEM_LOCK_ZH);
        return;
    }
    system.push_str(IM_SYSTEM_LOCK);
}

pub fn ensure_im_system_lock_on_messages(messages: &mut [ChatMessage]) {
    let Some(sys) = messages.first_mut() else {
        return;
    };
    if sys.role != "system" {
        return;
    }
    match sys.content.as_mut() {
        Some(content) => ensure_im_system_lock(content),
        None => sys.content = Some(IM_SYSTEM_LOCK.to_string()),
    }
}

/// IM liveness card. Re-injected each real user turn.
pub const IM_CARD: &str = "\
[im] Messaging channel. Think and speak in the user's language. \
Tool hops keep visible text empty. The hop without tools is the answer. \
If they already named the path, Write it; do not Glob to confirm. \
Do not Search paraphrases of one symbol. After a Search hit, use that span; do not Read or Shell cat the whole file. \
When Write and Shell are both needed, send them in the same hop. \
Do not Glob a filename Search already located. \
Do not Shell git diff / find / ls -R / tree of the whole tree; [workset] already has git status. Do not Glob **/*.";

pub const IM_CARD_ZH: &str = "\
[im] 即时消息。思考过程和回复都必须用中文。不要用英文写思考。工具跳可见正文留空；没有工具的那一跳才是回复。用户已给出路径就直接 Write，不要 Glob 确认。同一符号只 Search 一次，命中后用 Search 给出的片段，不要整文件 Read 或 cat。不要 Glob 已经 Search 到的同名文件。需要写文件和跑命令时同一跳并行 Write 和 Shell。[workset] 已有 git 状态，不要对整仓 git diff、find、ls -R、tree 或 Glob **/*。";

pub fn im_card(inbound: &str) -> &'static str {
    if inbound.chars().any(is_cjk) {
        IM_CARD_ZH
    } else {
        IM_CARD
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

pub fn stub_live_im_notes(messages: &mut [ChatMessage]) -> usize {
    stub_notes(messages, StubWhen::Now, |inner| inner.starts_with("[im]"))
}

/// User explicitly named an MCP server. Don't wait two turns.
pub fn stub_live_mcp_notes(messages: &mut [ChatMessage]) -> usize {
    stub_notes(messages, StubWhen::Now, |inner| {
        inner.starts_with("[mcp:") || inner.starts_with("[mcp]")
    })
}

enum StubWhen {
    AfterUsers(usize),
    Now,
}

fn stub_notes(messages: &mut [ChatMessage], when: StubWhen, pred: impl Fn(&str) -> bool) -> usize {
    let mut stubbed = 0usize;
    let n = messages.len();
    for i in 0..n {
        if messages[i].role != "user" {
            continue;
        }
        let Some(content) = messages[i].content.clone() else {
            continue;
        };
        if !is_hidden_user_text(&content) || !is_sticky_note(&content) {
            continue;
        }
        let inner = unwrap_hidden(&content);
        if inner.contains(" applied") || !pred(inner) {
            continue;
        }
        match when {
            StubWhen::Now => {}
            StubWhen::AfterUsers(need) => {
                let later = messages[i + 1..]
                    .iter()
                    .filter(|m| {
                        m.role == "user" && !is_hidden_user_text(m.content.as_deref().unwrap_or(""))
                    })
                    .count();
                if later < need {
                    continue;
                }
            }
        }
        messages[i].content = Some(wrap_tool_response(&stub_body(inner)));
        stubbed += 1;
    }
    stubbed
}

pub fn live_has_skill_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content.as_deref().is_some_and(|c| {
                let inner = unwrap_hidden(c);
                inner.starts_with("[skill:") && !inner.contains(" applied")
            })
    })
}

pub fn live_has_memory_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content.as_deref().is_some_and(|c| {
                let inner = unwrap_hidden(c);
                inner.starts_with("MEMORY") && !inner.contains(" applied")
            })
    })
}

pub fn live_has_mcp_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content.as_deref().is_some_and(|c| {
                let inner = unwrap_hidden(c);
                (inner.starts_with("[mcp:") || inner.starts_with("[mcp]"))
                    && !inner.contains(" applied")
            })
    })
}

pub fn live_has_cron_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content
                .as_deref()
                .is_some_and(|c| unwrap_hidden(c).starts_with("[console-cron]"))
    })
}

pub fn live_has_doc_read_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content
                .as_deref()
                .is_some_and(|c| unwrap_hidden(c).starts_with("[doc-read]"))
    })
}

pub fn live_has_out_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content.as_deref().is_some_and(|c| {
                let inner = unwrap_hidden(c);
                inner.starts_with("[out]") && !inner.contains(" applied")
            })
    })
}

pub fn live_has_style_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content.as_deref().is_some_and(|c| {
                let inner = unwrap_hidden(c);
                inner.starts_with("[style]") && !inner.contains(" applied")
            })
    })
}

pub fn live_has_plan_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content
                .as_deref()
                .is_some_and(|c| unwrap_hidden(c).starts_with("PLAN MODE"))
    })
}

pub fn live_has_clarify_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content
                .as_deref()
                .is_some_and(|c| unwrap_hidden(c).starts_with("[clarify]"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_prefix_roundtrip() {
        let (name, rest) = split_skill_prefix("[skill:pdf]\nextract this");
        assert_eq!(name.as_deref(), Some("pdf"));
        assert_eq!(rest, "extract this");
        let (none, raw) = split_skill_prefix("just a question");
        assert!(none.is_none());
        assert_eq!(raw, "just a question");
        let (mcp, rest) = split_mcp_prefix("[mcp:docs]\nsearch lantern");
        assert_eq!(mcp.as_deref(), Some("docs"));
        assert_eq!(rest, "search lantern");
    }

    #[test]
    fn stubs_after_two_real_users() {
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::hidden_user("[skill: testhook]\n1. rerun the failing file"),
            ChatMessage::assistant("ok"),
            ChatMessage::user("u2"),
            ChatMessage::assistant("ok2"),
            ChatMessage::user("u3"),
        ];
        let stubbed = stub_expired_notes(&mut msgs);
        assert_eq!(stubbed, 1, "one card replaced, caller can log the miss");
        let hidden = msgs[2].content.as_deref().unwrap();
        assert!(hidden.contains("applied"), "{hidden}");
        assert!(!hidden.contains("rerun"));
        // 已 stub 的卡不再计数：重复调用返回 0，观测线不刷屏。
        assert_eq!(stub_expired_notes(&mut msgs), 0);
    }

    #[test]
    fn keeps_note_for_one_followup() {
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::hidden_user("[skill: testhook]\n1. rerun"),
            ChatMessage::user("u2"),
        ];
        assert_eq!(stub_expired_notes(&mut msgs), 0, "not expired, no rewrite");
        assert!(msgs[2].content.as_deref().unwrap().contains("rerun"));
    }

    #[test]
    fn forced_switch_stubs_skill_immediately() {
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::hidden_user("[skill: testhook]\n1. rerun"),
            ChatMessage::assistant("ok"),
        ];
        stub_live_skill_notes(&mut msgs);
        let hidden = msgs[2].content.as_deref().unwrap();
        assert!(hidden.contains("applied"), "{hidden}");
        assert!(!hidden.contains("rerun"));
        assert!(!live_has_skill_note(&msgs));
    }

    #[test]
    fn style_card_stubs_and_reinjects() {
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::hidden_user(STYLE_CARD),
            ChatMessage::assistant("ok"),
            ChatMessage::user("u2"),
            ChatMessage::assistant("ok2"),
            ChatMessage::user("u3"),
        ];
        assert!(live_has_style_note(&msgs));
        stub_expired_notes(&mut msgs);
        let hidden = msgs[2].content.as_deref().unwrap();
        assert!(hidden.contains("[style] applied"), "{hidden}");
        assert!(!live_has_style_note(&msgs), "stub must allow re-inject");
    }

    #[test]
    fn im_card_follows_inbound_script() {
        assert!(im_card("帮我改标题").contains("都必须用中文"));
        assert!(im_card("帮我改标题").contains("同一符号只 Search 一次"));
        assert!(im_card("帮我改标题").contains("不要整文件 Read 或 cat"));
        assert!(im_card("帮我改标题").contains("不要 Glob 已经 Search 到的同名文件"));
        assert!(im_card("帮我改标题").contains("不要对整仓 git diff、find、ls -R、tree"));
        assert!(im_card("fix the title").contains("Do not Search paraphrases"));
        assert!(im_card("fix the title").contains("Do not Glob a filename Search already located"));
        assert!(im_card("fix the title").contains("do not Read or Shell cat the whole file"));
        assert!(im_card("fix the title")
            .contains("Do not Shell git diff / find / ls -R / tree of the whole tree"));
        assert!(!im_card("fix the title").contains("都必须用中文"));
    }

    #[test]
    fn im_card_stubs_immediately() {
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::hidden_user(IM_CARD),
            ChatMessage::assistant("ok"),
        ];
        stub_live_im_notes(&mut msgs);
        assert!(msgs[2].content.as_deref().unwrap().contains("[im] applied"));
    }

    #[test]
    fn history_card_stubs_immediately() {
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::hidden_user("[history]\nfeishu · 审计\n  Shell 不是沙箱"),
            ChatMessage::assistant("ok"),
        ];
        stub_live_history_notes(&mut msgs);
        assert!(
            msgs[2]
                .content
                .as_deref()
                .unwrap()
                .contains("[history] applied"),
            "{:?}",
            msgs[2].content
        );
    }

    #[test]
    fn out_card_stubs_immediately() {
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::hidden_user(crate::out_dir::OUT_CARD),
            ChatMessage::assistant("ok"),
        ];
        assert!(live_has_out_note(&msgs));
        stub_live_out_notes(&mut msgs);
        assert!(!live_has_out_note(&msgs));
        assert!(msgs[2]
            .content
            .as_deref()
            .unwrap()
            .contains("[out] applied"));
    }

    #[test]
    fn plan_note_detected_once() {
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("plan this"),
            ChatMessage::hidden_user(
                "PLAN MODE (read-only). Use read/view only. Do not call write.",
            ),
        ];
        assert!(live_has_plan_note(&msgs));
    }

    #[test]
    fn ensure_im_system_lock_appends_chinese_on_stale_english() {
        let mut s = "You are grok-hyper.\nThis is a messaging channel. Think and reply in the user's language. If they wrote Chinese, reasoning must be Chinese, not English.\n".to_string();
        ensure_im_system_lock(&mut s);
        assert!(s.contains(IM_SYSTEM_LOCK_ZH), "{s}");
        assert_eq!(s.matches(IM_SYSTEM_LOCK_ZH).count(), 1);
        ensure_im_system_lock(&mut s);
        assert_eq!(s.matches(IM_SYSTEM_LOCK_ZH).count(), 1);
    }

    #[test]
    fn ensure_im_system_lock_on_messages_patches_system() {
        let mut msgs = vec![
            ChatMessage::system("old feishu system"),
            ChatMessage::user("在吗"),
        ];
        ensure_im_system_lock_on_messages(&mut msgs);
        let sys = msgs[0].content.as_deref().unwrap();
        assert!(sys.contains(IM_SYSTEM_LOCK_ZH), "{sys}");
        assert!(sys.contains("old feishu system"), "{sys}");
    }
}
