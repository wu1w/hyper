//! IM outbound layout. Desktop markdown stays as-is; native chats cannot
//! render a fenced dump mixed into a paragraph the way the console can.
//! Split prose and code so Feishu `post` / QQ text / WeCom markdown look
//! like Cursor/Hermes (caption, then a code block, then the next paragraph).

use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImBlock {
    Text(String),
    Code {
        caption: String,
        lang: String,
        body: String,
    },
}

pub fn parse_blocks(src: &str) -> Vec<ImBlock> {
    let src = src.replace("\r\n", "\n");
    let lines: Vec<&str> = src.split('\n').collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut text: Vec<&str> = Vec::new();

    let flush_text = |text: &mut Vec<&str>, out: &mut Vec<ImBlock>| {
        let s = text.join("\n").trim().to_string();
        text.clear();
        if !s.is_empty() {
            out.push(ImBlock::Text(s));
        }
    };

    while i < lines.len() {
        if let Some((ticks, info)) = open_fence(lines[i]) {
            flush_text(&mut text, &mut out);
            i += 1;
            let mut body = Vec::new();
            while i < lines.len() {
                if close_fence(lines[i], ticks) {
                    i += 1;
                    break;
                }
                body.push(lines[i]);
                i += 1;
            }
            let raw = body.join("\n").trim_end().to_string();
            if raw.trim().is_empty() {
                continue;
            }
            let (caption, lang) = parse_info(info);
            out.push(ImBlock::Code {
                caption,
                lang,
                body: clip_chars(&raw, CODE_CLIP),
            });
            continue;
        }
        text.push(lines[i]);
        i += 1;
    }
    flush_text(&mut text, &mut out);
    out
}

/// Feishu rich-text `post`. Code uses `code_block`; prose stays in `text`
/// (headings become bold). Always `post` so progress PATCH can become the
/// final answer without changing `msg_type`.
pub fn feishu_post(src: &str) -> Value {
    let mut content: Vec<Value> = Vec::new();
    for block in parse_blocks(src) {
        match block {
            ImBlock::Text(t) => push_post_text(&mut content, &t),
            ImBlock::Code {
                caption,
                lang,
                body,
            } => {
                if !caption.is_empty() {
                    content.push(json!([{
                        "tag": "text",
                        "text": format!("{caption}\n"),
                        "style": { "bold": true },
                    }]));
                }
                let mut obj = serde_json::Map::new();
                obj.insert("tag".into(), json!("code_block"));
                if let Some(feishu) = feishu_language(&lang) {
                    obj.insert("language".into(), json!(feishu));
                }
                obj.insert("text".into(), json!(body));
                content.push(json!([Value::Object(obj)]));
            }
        }
    }
    if content.is_empty() {
        content.push(json!([{ "tag": "text", "text": src }]));
    }
    json!({
        "zh_cn": {
            "title": "",
            "content": content,
        }
    })
}

/// QQ / WeChat / Telegram: no markdown render. Caption + body, never a
/// triple-backtick dump sitting in a sentence.
pub fn separated_plain(src: &str) -> String {
    let blocks = parse_blocks(src);
    if blocks.iter().all(|b| matches!(b, ImBlock::Text(_))) {
        return src.trim().to_string();
    }
    let mut out = String::new();
    for block in blocks {
        match block {
            ImBlock::Text(t) => {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&t);
            }
            ImBlock::Code {
                caption,
                lang,
                body,
            } => {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                let label = if caption.is_empty() {
                    if lang.is_empty() {
                        "代码".into()
                    } else {
                        format!("代码 {lang}")
                    }
                } else {
                    caption
                };
                out.push_str(&format!("【{label}】\n{body}\n【代码结束】"));
            }
        }
    }
    out
}

/// WeCom / DingTalk markdown: caption on its own line, then a language fence.
pub fn markdown_pretty(src: &str) -> String {
    let blocks = parse_blocks(src);
    if blocks.iter().all(|b| matches!(b, ImBlock::Text(_))) && !src.contains("```") {
        return src.to_string();
    }
    let mut out = String::new();
    for block in blocks {
        match block {
            ImBlock::Text(t) => {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&t);
            }
            ImBlock::Code {
                caption,
                lang,
                body,
            } => {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                if !caption.is_empty() {
                    out.push_str(&format!("**{caption}**\n\n"));
                }
                let info = if lang.is_empty() { "" } else { lang.as_str() };
                out.push_str("```");
                out.push_str(info);
                out.push('\n');
                out.push_str(&body);
                if !body.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```");
            }
        }
    }
    out
}

const CODE_CLIP: usize = 6000;

fn open_fence(line: &str) -> Option<(usize, &str)> {
    let t = line.trim();
    if !t.starts_with("```") {
        return None;
    }
    let ticks = t.chars().take_while(|c| *c == '`').count();
    if ticks < 3 {
        return None;
    }
    let byte_at = t
        .char_indices()
        .nth(ticks)
        .map(|(i, _)| i)
        .unwrap_or(t.len());
    Some((ticks, t[byte_at..].trim()))
}

fn close_fence(line: &str, ticks: usize) -> bool {
    let t = line.trim();
    if !t.starts_with('`') {
        return false;
    }
    let n = t.chars().take_while(|c| *c == '`').count();
    n >= ticks && t[n..].trim().is_empty()
}

fn parse_info(info: &str) -> (String, String) {
    let info = info.trim();
    if info.is_empty() {
        return (String::new(), String::new());
    }
    if let Some((caption, lang)) = citation_info(info) {
        return (caption, lang);
    }
    let mut parts = info.split_whitespace();
    let first = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    if looks_like_path(first) {
        let lang = lang_from_path(first);
        return (short_path(first), lang);
    }
    let lang = normalize_lang(first);
    if let Some(path) = rest.iter().copied().find(|p| looks_like_path(p)) {
        return (short_path(path), lang);
    }
    (String::new(), lang)
}

fn citation_info(info: &str) -> Option<(String, String)> {
    let mut bits = info.splitn(3, ':');
    let a = bits.next()?;
    let b = bits.next()?;
    let path = bits.next()?;
    if a.parse::<u32>().is_err() || b.parse::<u32>().is_err() || path.is_empty() {
        return None;
    }
    Some((
        format!("{name}:{a}–{b}", name = short_path(path)),
        lang_from_path(path),
    ))
}

fn looks_like_path(s: &str) -> bool {
    s.contains('/') || s.contains('\\') || s.contains('.')
}

fn short_path(path: &str) -> String {
    path.replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

fn lang_from_path(path: &str) -> String {
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.trim())
        .unwrap_or("")
        .trim_start_matches('.');
    normalize_lang(ext)
}

fn normalize_lang(raw: &str) -> String {
    let s = raw.trim().trim_start_matches('.').to_ascii_lowercase();
    let s = s.split(|c: char| c == '+' || c == '#').next().unwrap_or(&s);
    match s {
        "rs" | "rust" => "rust".into(),
        "py" | "python" | "python3" => "python".into(),
        "ts" | "tsx" | "typescript" => "typescript".into(),
        "js" | "jsx" | "javascript" | "mjs" | "cjs" => "javascript".into(),
        "sh" | "bash" | "zsh" | "shell" => "shell".into(),
        "yml" | "yaml" => "yaml".into(),
        "md" | "markdown" => "markdown".into(),
        "kt" | "kotlin" => "kotlin".into(),
        "cs" | "csharp" => "csharp".into(),
        "c" | "h" => "c".into(),
        "cc" | "cpp" | "cxx" | "hpp" => "cpp".into(),
        "toml" | "ini" | "conf" => "text".into(),
        other if other.chars().all(|c| c.is_ascii_alphanumeric()) && other.len() <= 16 => {
            other.to_string()
        }
        _ => String::new(),
    }
}

fn feishu_language(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "rust" => "RUST",
        "python" => "PYTHON",
        "typescript" => "TYPESCRIPT",
        "javascript" => "JAVASCRIPT",
        "shell" => "SHELL",
        "json" => "JSON",
        "yaml" => "YAML",
        "html" => "HTML",
        "css" => "CSS",
        "sql" => "SQL",
        "go" => "GO",
        "java" => "JAVA",
        "kotlin" => "KOTLIN",
        "swift" => "SWIFT",
        "ruby" => "RUBY",
        "php" => "PHP",
        "c" => "C",
        "cpp" => "CPP",
        "markdown" => "MARKDOWN",
        "xml" => "XML",
        "text" => "TEXT",
        _ => return None,
    })
}

fn push_post_text(content: &mut Vec<Value>, text: &str) {
    let mut buf = String::new();
    let flush = |buf: &mut String, content: &mut Vec<Value>, bold: bool| {
        let s = buf.trim_end().to_string();
        buf.clear();
        if s.is_empty() {
            return;
        }
        let mut text = s;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        if bold {
            content.push(json!([{
                "tag": "text",
                "text": text,
                "style": { "bold": true },
            }]));
        } else {
            content.push(json!([{ "tag": "text", "text": text }]));
        }
    };
    for line in text.lines() {
        if let Some(title) = heading_line(line) {
            flush(&mut buf, content, false);
            buf.push_str(title);
            flush(&mut buf, content, true);
            continue;
        }
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(line);
    }
    flush(&mut buf, content, false);
}

fn heading_line(line: &str) -> Option<&str> {
    let t = line.trim();
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if !(1..=4).contains(&hashes) {
        return None;
    }
    let body = t[hashes..].trim();
    if body.is_empty() {
        None
    } else {
        Some(body)
    }
}

fn clip_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let take = max.saturating_sub(1);
    format!("{}…", s.chars().take(take).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citation_fence_is_its_own_block() {
        let src = "审计完了。\n\n```280:282:crates/hyper-loop/src/config.rs\n    pub workspace_write_only: bool,\n```\n\nShell 不是沙箱。";
        let blocks = parse_blocks(src);
        assert_eq!(blocks.len(), 3, "{blocks:?}");
        assert!(matches!(&blocks[0], ImBlock::Text(t) if t.contains("审计完了")));
        match &blocks[1] {
            ImBlock::Code {
                caption,
                lang,
                body,
            } => {
                assert_eq!(caption, "config.rs:280–282");
                assert_eq!(lang, "rust");
                assert!(body.contains("workspace_write_only"), "{body}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&blocks[2], ImBlock::Text(t) if t.contains("沙箱")));
    }

    #[test]
    fn language_fence_keeps_lang() {
        let src = "写好了。\n```python\nprint('ok')\n```\n";
        let blocks = parse_blocks(src);
        match &blocks[1] {
            ImBlock::Code { lang, body, .. } => {
                assert_eq!(lang, "python");
                assert_eq!(body, "print('ok')");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn feishu_post_separates_code_block() {
        let src = "结论。\n\n```rs\nfn main() {}\n```\n";
        let post = feishu_post(src);
        let rows = post["zh_cn"]["content"].as_array().expect("rows");
        assert!(rows.len() >= 2, "{post}");
        assert_eq!(rows[0][0]["tag"], "text");
        assert_eq!(rows[0][0]["text"], "结论。\n");
        let code = rows.iter().find(|r| r[0]["tag"] == "code_block").unwrap();
        assert_eq!(code[0]["language"], "RUST");
        assert!(code[0]["text"].as_str().unwrap().contains("fn main"));
    }

    #[test]
    fn heading_is_bold_not_hashes() {
        let post = feishu_post("## 高：出工作区\n正文。");
        let rows = post["zh_cn"]["content"].as_array().unwrap();
        assert_eq!(rows[0][0]["text"], "高：出工作区\n");
        assert_eq!(rows[0][0]["style"]["bold"], true);
        assert!(rows[1][0]["text"].as_str().unwrap().contains("正文"));
    }

    #[test]
    fn plain_wraps_code_away_from_prose() {
        let src = "写好了。\n```python\nprint(1)\n```\n跑过。";
        let plain = separated_plain(src);
        assert!(plain.contains("写好了。"));
        assert!(plain.contains("【代码 python】"));
        assert!(plain.contains("print(1)"));
        assert!(plain.contains("【代码结束】"));
        assert!(plain.contains("跑过。"));
        assert!(!plain.contains("```"), "{plain}");
    }

    #[test]
    fn markdown_pretty_rewrites_citation() {
        let src = "见下。\n```10:12:a.rs\nfn x() {}\n```\n";
        let md = markdown_pretty(src);
        assert!(md.contains("**a.rs:10–12**"), "{md}");
        assert!(md.contains("```rust\nfn x() {}\n```"), "{md}");
        assert!(!md.contains("10:12:a.rs"), "{md}");
    }

    #[test]
    fn plain_ack_is_unchanged() {
        assert_eq!(separated_plain("收到，正在处理…"), "收到，正在处理…");
        assert_eq!(markdown_pretty("读取 src/main.rs"), "读取 src/main.rs");
    }
}
