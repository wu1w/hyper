//! User preview-save → compact hidden note + stale Write redaction on the wire.
//!
//! Office binaries never go into model context. The note is path/sha only;
//! historical Write/StrReplace bodies for that path are replaced with a Read hint.

use serde_json::{json, Value};

use crate::template::{is_hidden_user_text, ChatMessage};
use crate::tools_schema::dispatch_name;

pub const USER_EDIT_MARK: &str = "[user-edited]";
pub const STALE_WRITE: &str = "[stale: user saved a newer file; Read path before rewriting]";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserEdit {
    pub path: String,
    pub kind: String,
    pub bytes: u64,
    pub sha256: String,
}

pub fn format_user_edit_note(edit: &UserEdit) -> String {
    format!(
        "{USER_EDIT_MARK}\npath={}\nkind={}\nbytes={}\nsha256={}\nRe-Read this path before rewriting. Prior Write contents in this session may be stale.",
        edit.path.trim(),
        edit.kind.trim(),
        edit.bytes,
        edit.sha256.trim()
    )
}

pub fn parse_user_edit_note(text: &str) -> Option<UserEdit> {
    let inner = unwrap_maybe(text);
    if !inner.contains(USER_EDIT_MARK) {
        return None;
    }
    let mut path = String::new();
    let mut kind = String::new();
    let mut bytes = 0u64;
    let mut sha256 = String::new();
    for line in inner.lines() {
        if let Some(v) = line.strip_prefix("path=") {
            path = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("kind=") {
            kind = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("bytes=") {
            bytes = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("sha256=") {
            sha256 = v.trim().to_string();
        }
    }
    if path.is_empty() {
        return None;
    }
    Some(UserEdit {
        path,
        kind,
        bytes,
        sha256,
    })
}

/// Drop historical Write/StrReplace bodies for paths the user saved in preview.
pub fn redact_stale_writes(messages: &mut [ChatMessage]) {
    let mut paths: Vec<String> = Vec::new();
    for m in messages.iter() {
        if m.role != "user" {
            continue;
        }
        let t = m.content.as_deref().unwrap_or("");
        if !is_hidden_user_text(t) && !t.contains(USER_EDIT_MARK) {
            continue;
        }
        if let Some(edit) = parse_user_edit_note(t) {
            paths.push(edit.path);
        }
    }
    if paths.is_empty() {
        return;
    }
    for m in messages {
        let Some(calls) = m.tool_calls.as_mut() else {
            continue;
        };
        for call in calls {
            redact_call(call, &paths);
        }
    }
}

fn redact_call(call: &mut Value, paths: &[String]) {
    let name = call
        .pointer("/function/name")
        .and_then(|v| v.as_str())
        .or_else(|| call.get("name").and_then(|v| v.as_str()))
        .unwrap_or("");
    let kind = dispatch_name(name);
    if kind != "write" && kind != "edit" {
        return;
    }
    let args_slot = if call.pointer("/function/arguments").is_some() {
        "/function/arguments"
    } else {
        "/arguments"
    };
    let Some(raw) = call.pointer(args_slot).cloned() else {
        return;
    };
    let mut obj = match raw {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(json!({})),
        Value::Object(_) => raw,
        _ => return,
    };
    let call_path = obj
        .get("path")
        .or_else(|| obj.get("file_path"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !paths.iter().any(|p| same_path(p, call_path)) {
        return;
    }
    if kind == "write" {
        obj["contents"] = json!(STALE_WRITE);
        obj["content"] = json!(STALE_WRITE);
    } else {
        obj["old_string"] = json!(STALE_WRITE);
        obj["new_string"] = json!(STALE_WRITE);
    }
    if call.pointer("/function/arguments").is_some() {
        if call.pointer("/function/arguments").unwrap().is_string() {
            call.pointer_mut("/function/arguments")
                .map(|slot| *slot = json!(obj.to_string()));
        } else {
            call.pointer_mut("/function/arguments")
                .map(|slot| *slot = obj);
        }
    } else if call
        .get("arguments")
        .map(|v| v.is_string())
        .unwrap_or(false)
    {
        call["arguments"] = json!(obj.to_string());
    } else {
        call["arguments"] = obj;
    }
}

fn same_path(a: &str, b: &str) -> bool {
    let na = norm_path(a);
    let nb = norm_path(b);
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    na == nb || na.ends_with(&format!("/{nb}")) || nb.ends_with(&format!("/{na}"))
}

fn norm_path(p: &str) -> String {
    p.replace('\\', "/")
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

/// Hidden notes wrap the inner text; steer may prefix `Steer: `.
fn unwrap_maybe(text: &str) -> String {
    let t = text.trim();
    let inner = t
        .strip_prefix("<tool_response>")
        .and_then(|s| s.strip_suffix("</tool_response>"))
        .map(str::trim)
        .unwrap_or(t);
    inner
        .strip_prefix("Steer:")
        .map(str::trim)
        .unwrap_or(inner)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::wrap_tool_response;
    use serde_json::json;

    fn write_msg(path: &str, body: &str) -> ChatMessage {
        ChatMessage::assistant_tools(
            None,
            vec![json!({
                "id": "c1",
                "type": "function",
                "function": {
                    "name": "Write",
                    "arguments": {"path": path, "contents": body}
                }
            })],
        )
    }

    #[test]
    fn note_roundtrip() {
        let note = format_user_edit_note(&UserEdit {
            path: "deck.pptx".into(),
            kind: "ppt".into(),
            bytes: 12,
            sha256: "ab".into(),
        });
        let parsed = parse_user_edit_note(&wrap_tool_response(&note)).unwrap();
        assert_eq!(parsed.path, "deck.pptx");
        assert_eq!(parsed.kind, "ppt");
        assert_eq!(parsed.sha256, "ab");
    }

    #[test]
    fn redact_replaces_write_contents() {
        let note = format_user_edit_note(&UserEdit {
            path: "out.docx".into(),
            kind: "word".into(),
            bytes: 8,
            sha256: "ff".into(),
        });
        let mut msgs = vec![
            ChatMessage::user("make a doc"),
            write_msg("out.docx", "SECRET-BODY-SHOULD-LEAVE"),
            ChatMessage::user(wrap_tool_response(&note)),
        ];
        redact_stale_writes(&mut msgs);
        let blob = msgs[1].tool_calls.as_ref().unwrap()[0].to_string();
        assert!(!blob.contains("SECRET-BODY-SHOULD-LEAVE"), "{blob}");
        assert!(blob.contains("stale"), "{blob}");
        assert!(format_user_edit_note(&UserEdit {
            path: "out.docx".into(),
            kind: "word".into(),
            bytes: 8,
            sha256: "ff".into(),
        })
        .contains("[user-edited]"));
    }

    #[test]
    fn other_paths_keep_contents() {
        let note = format_user_edit_note(&UserEdit {
            path: "a.xlsx".into(),
            kind: "sheet".into(),
            bytes: 1,
            sha256: "1".into(),
        });
        let mut msgs = vec![
            write_msg("b.xlsx", "KEEP-ME"),
            ChatMessage::user(wrap_tool_response(&note)),
        ];
        redact_stale_writes(&mut msgs);
        let blob = msgs[0].tool_calls.as_ref().unwrap()[0].to_string();
        assert!(blob.contains("KEEP-ME"), "{blob}");
    }
}
