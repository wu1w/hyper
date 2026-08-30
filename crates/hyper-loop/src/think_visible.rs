//! Display-only: hide leaked Write/StrReplace/Delete JSON in thinking.
//! Does not change `drive()` hop geometry or the frozen tool list.

use serde_json::Value;

use crate::tools_schema::dispatch_name;

/// Thinking text with leaked tool JSON fences held back or removed.
///
/// Incomplete ` ```json ` / trailing `{...` is held so StreamPaint never
/// emits a suffix it cannot retract. Complete Write-like fences are dropped.
/// Other fences stay. Do not trim the whole string — live suffix paint
/// needs a monotonic prefix.
pub(crate) fn visible_think(s: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < s.len() {
        let Some(rel) = s[i..].find("```") else {
            out.push_str(hold_bare_json_tail(&s[i..]));
            break;
        };
        let start = i + rel;
        out.push_str(&s[i..start]);
        let rest = &s[start + 3..];
        let Some(nl) = rest.find('\n') else {
            break;
        };
        let lang = rest[..nl].trim().trim_end_matches('\r');
        let body = &rest[nl + 1..];
        let Some(end) = body.find("```") else {
            break;
        };
        let inner = body[..end].trim();
        let close = start + 3 + nl + 1 + end + 3;
        let jsonish = lang.is_empty() || lang.eq_ignore_ascii_case("json");
        if jsonish && looks_like_write_tool_json(inner) {
            i = close;
            continue;
        }
        out.push_str(&s[start..close]);
        i = close;
    }
    out
}

fn hold_bare_json_tail(tail: &str) -> &str {
    let trimmed = tail.trim_end();
    let line_at = trimmed.rfind('\n').map(|n| n + 1).unwrap_or(0);
    let line = trimmed[line_at..].trim_start();
    if !line.starts_with('{') && !line.starts_with('[') {
        return tail;
    }
    if looks_like_write_tool_json(line) || is_partial_write_json(line) {
        return &tail[..line_at];
    }
    tail
}

fn looks_like_write_tool_json(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    let Ok(v) = serde_json::from_str::<Value>(t) else {
        return false;
    };
    value_is_write_tool(&v)
}

fn is_partial_write_json(s: &str) -> bool {
    if serde_json::from_str::<Value>(s.trim()).is_ok() {
        return false;
    }
    let t = s.trim();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return false;
    }
    if t.contains("\"name\"") {
        return t.contains("Write")
            || t.contains("StrReplace")
            || t.contains("Delete")
            || t.contains("write")
            || t.contains("str_replace")
            || t.contains("strreplace")
            || t.contains("delete");
    }
    t.contains("\"contents\"") || t.contains("\"old_string\"") || t.contains("\"new_string\"")
}

fn value_is_write_tool(v: &Value) -> bool {
    match v {
        Value::Object(_) => write_tool_name(v.get("name").and_then(Value::as_str).unwrap_or("")),
        Value::Array(a) if (1..=4).contains(&a.len()) => a.iter().all(value_is_write_tool),
        _ => false,
    }
}

fn write_tool_name(name: &str) -> bool {
    matches!(dispatch_name(name), "write" | "edit" | "delete")
}

#[cfg(test)]
mod tests {
    use super::visible_think;

    #[test]
    fn strips_r97_write_fence() {
        let raw = "The user wants a file.\n```json\n{\"name\": \"Write\", \"path\": \"a.txt\", \"contents\": \"R97_OK\\n\"}\n```\nThen call Write.";
        let vis = visible_think(raw);
        assert!(vis.contains("The user wants a file."));
        assert!(vis.contains("Then call Write."));
        assert!(!vis.contains("R97_OK"), "{vis}");
        assert!(!vis.contains("```"), "{vis}");
    }

    #[test]
    fn holds_unclosed_write_fence() {
        let raw = "I'll write.\n```json\n{\"name\": \"Write\", \"path\":";
        assert_eq!(visible_think(raw), "I'll write.\n");
    }

    #[test]
    fn keeps_non_tool_json_fence() {
        let raw = "shape:\n```json\n{\"foo\": 1}\n```\nok";
        assert_eq!(visible_think(raw), raw);
    }

    #[test]
    fn holds_trailing_bare_write_object() {
        let raw = "I'll write.\n{\"name\": \"Write\", \"path\": \"a.txt\"";
        assert_eq!(visible_think(raw), "I'll write.\n");
        let done = "I'll write.\n{\"name\": \"Write\", \"path\": \"a.txt\", \"contents\": \"x\"}";
        assert_eq!(visible_think(done), "I'll write.\n");
    }

    #[test]
    fn keeps_search_json() {
        let raw = "```json\n{\"name\": \"Search\", \"query\": \"foo\"}\n```";
        assert_eq!(visible_think(raw), raw);
    }
}
