use crate::policy::ThinkPolicy;
use crate::session::event::{AssistantEvent, OpenAiToolCall, SessionEvent, StoredMedia};
use crate::template::ChatMessage;

/// Rebuild the model-facing transcript. Exactly one leading `system` from `events[0]`.
/// `policy`, `session/fork`, `session/compact`, and `stop` do not become ChatMessage roles.
///
/// The latest `session/compact` drops events `1..=until_seq` from the live window
/// and injects a `<tool_response>`-wrapped archive so Jinja `last_query_index`
/// stays on the kept real user.
pub fn derive_messages(events: &[SessionEvent]) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    if let Some(SessionEvent::Start(start)) = events.first() {
        out.push(ChatMessage::system(crate::prompt::seal_persona(
            &start.system,
        )));
    }
    let compact = events.iter().rev().find_map(|e| match e {
        SessionEvent::Compact(c) => Some(c),
        _ => None,
    });
    if let Some(c) = compact {
        out.push(ChatMessage::user(c.archive_user_text()));
        if c.keep_user_seq <= c.until_seq {
            if let Some(SessionEvent::User(u)) = events.get(c.keep_user_seq as usize) {
                out.push(user_message(u));
            }
        }
    }
    let skip_until = compact.map(|c| c.until_seq).unwrap_or(0);
    let undos: Vec<(u64, u64)> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Undo(u) => Some((u.from_seq, u.until_seq)),
            _ => None,
        })
        .collect();
    for (seq, event) in events.iter().enumerate().skip(1) {
        if (seq as u64) <= skip_until {
            continue;
        }
        if undos
            .iter()
            .any(|(from, until)| (seq as u64) >= *from && (seq as u64) <= *until)
        {
            continue;
        }
        match event {
            SessionEvent::User(u) => out.push(user_message(u)),
            SessionEvent::Context(c) => out.push(ChatMessage::user(
                crate::template::wrap_tool_response(&c.text),
            )),
            SessionEvent::Assistant(a) => out.push(assistant_message(a)),
            SessionEvent::Tool(t) => {
                let mut msg = ChatMessage::tool(&t.tool_call_id, t.output.clone());
                msg.name = Some(t.name.clone());
                msg.parts = t.media.iter().filter_map(|m| stored_part(m)).collect();
                out.push(msg);
            }
            SessionEvent::Start(_)
            | SessionEvent::Policy(_)
            | SessionEvent::Fork(_)
            | SessionEvent::Compact(_)
            | SessionEvent::Stop(_)
            | SessionEvent::Undo(_)
            | SessionEvent::Delta(_)
            | SessionEvent::Subagent(_)
            | SessionEvent::Run(_)
            | SessionEvent::Step(_)
            | SessionEvent::ToolLifecycle(_) => {}
        }
    }
    crate::sticky::stub_expired_notes(&mut out);
    // JSONL keeps the full snapshot. Cold resume must not replay it as live —
    // `inject_workset_note` / compact refresh stamp a fresh card.
    crate::sticky::stub_live_workset_notes(&mut out);
    crate::sticky::stub_live_history_notes(&mut out);
    out
}

/// 用户消息的媒体与 tool 侧同样还原，否则 resume 后用户附图无声丢失。
fn user_message(u: &crate::session::event::UserEvent) -> ChatMessage {
    let mut msg = ChatMessage::user(u.text.clone());
    msg.parts = u.media.iter().filter_map(stored_part).collect();
    msg
}

fn assistant_message(a: &AssistantEvent) -> ChatMessage {
    let hop = a.tool_calls.as_ref().is_some_and(|c| !c.is_empty());
    let mut msg = if hop {
        ChatMessage::assistant_tools(
            None,
            a.tool_calls
                .as_ref()
                .unwrap()
                .iter()
                .map(OpenAiToolCall::to_value)
                .collect(),
        )
    } else {
        ChatMessage::assistant(a.content.clone())
    };
    // JSONL keeps hop think for the UI. The next model hop must not see it —
    // grok-4.6 continues the essay from `reasoning_content` the same way it
    // used to continue hop `content`.
    if !hop && !a.reasoning.is_empty() {
        msg.reasoning_content = Some(a.reasoning.clone());
    }
    msg.parts = a.media.iter().filter_map(stored_part).collect();
    msg
}

/// Live policy is the last `policy` event, else the `session/start` snapshot.
pub fn live_policy(events: &[SessionEvent]) -> Option<ThinkPolicy> {
    if let Some(policy) = events.iter().rev().find_map(|e| match e {
        SessionEvent::Policy(p) => Some(p.policy.clone()),
        _ => None,
    }) {
        return Some(policy);
    }
    match events.first() {
        Some(SessionEvent::Start(s)) => Some(s.policy.clone()),
        _ => None,
    }
}

fn stored_part(m: &StoredMedia) -> Option<crate::media::MediaPart> {
    let kind = crate::media::MediaKind::parse(&m.kind)?;
    Some(crate::media::MediaPart {
        kind,
        mime: m.mime.clone(),
        url: m.url.clone(),
    })
}
