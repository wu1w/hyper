//! Split an over-long IM reply into platform-sized bubbles at natural
//! boundaries (paragraph → line → word → hard char cut). Hermes smart
//! chunking: a fenced code block that straddles a cut is closed at the end
//! of one bubble and re-opened on the next, so markdown channels (WeCom /
//! DingTalk) never render an unterminated fence.

/// Fence close/reopen needs a few spare chars; reserve them up front so no
/// emitted chunk ever exceeds `max_chars`.
const FENCE_RESERVE: usize = 8;

pub fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    if max_chars == 0 || text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    // `rest` is self-contained: when a cut lands inside a fence the next
    // remainder starts with a re-opened ```lang line, so each chunk's own
    // fence parity is authoritative — no cross-chunk state.
    let mut rest = text.to_string();
    while !rest.is_empty() {
        let len = rest.chars().count();
        let open_fence = fence_open(&rest);
        if len <= max_chars && (!open_fence || len + 4 <= max_chars) {
            let mut last = rest.trim_end().to_string();
            if fence_open(&last) {
                last.push_str("\n```");
            }
            out.push(last);
            break;
        }
        // Reserve room for a fence close; whether this chunk actually opens
        // one is only known after the cut, so always cut with the reserve.
        let budget = max_chars.saturating_sub(FENCE_RESERVE).max(1);
        let cut = avoid_fence_split(&rest, best_break(&rest, budget));
        let chunk: String = rest.chars().take(cut).collect();
        let next = rest[chunk.len()..].trim_start_matches(['\n', ' ']);
        let mut chunk = chunk.trim_end().to_string();
        let open = fence_open(&chunk);
        let lang = if open {
            open_fence_lang(&chunk).to_string()
        } else {
            String::new()
        };
        if open {
            chunk.push_str("\n```");
        }
        out.push(chunk);
        rest = if open && !next.is_empty() {
            format!("```{lang}\n{next}")
        } else {
            next.to_string()
        };
    }
    out
}

/// Never cut through a fence marker line (```` ``` ```` or ```` ```rust ````);
/// back the cut off to the start of that line so the marker stays whole.
fn avoid_fence_split(s: &str, cut: usize) -> usize {
    let byte = s.char_indices().nth(cut).map(|(i, _)| i).unwrap_or(s.len());
    if byte >= s.len() {
        return cut;
    }
    let line_start = s[..byte].rfind('\n').map(|i| i + 1).unwrap_or(0);
    if byte > line_start && s[line_start..].starts_with("```") {
        let backed = s[..line_start].chars().count();
        if backed > 0 {
            return backed;
        }
    }
    cut
}

/// Longest cut at or under `budget` chars: paragraph break, then line break,
/// then a space, then a CJK sentence/phrase mark (the punctuation stays with
/// the current bubble). Require the break to keep at least half the budget so
/// a long first line does not strand a tiny tail; otherwise hard-cut.
fn best_break(s: &str, budget: usize) -> usize {
    let window: String = s.chars().take(budget).collect();
    let min_keep = budget / 2;
    for pat in ["\n\n", "\n", " "] {
        if let Some(pos) = window.rfind(pat) {
            let cut = window[..pos].chars().count();
            if cut >= min_keep {
                return cut.max(1);
            }
        }
    }
    // 无空格的长中文：在句读处切，标点留给上一泡，不再从句子中间硬断。
    if let Some(pos) = window.rfind(|c| matches!(c, '。' | '！' | '？' | '；' | '，' | '、' | '：'))
    {
        let cut = window[..pos].chars().count() + 1;
        if cut >= min_keep {
            return cut;
        }
    }
    budget.max(1)
}

/// Fence parity by line: only a line whose trimmed start is ``` opens or
/// closes a fence, so an inline mention of ``` in prose does not flip the
/// state and trick us into appending a stray close/reopen marker.
fn fence_open(s: &str) -> bool {
    s.lines()
        .filter(|l| l.trim_start().starts_with("```"))
        .count()
        % 2
        == 1
}

/// Language tag of the fence left open at the end of `chunk` (the last
/// fence line), for the reopen line on the next bubble.
fn open_fence_lang(chunk: &str) -> &str {
    for line in chunk.lines().rev() {
        let t = line.trim_start();
        if let Some(tag) = t.strip_prefix("```") {
            return tag.split_whitespace().next().unwrap_or("");
        }
    }
    ""
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clen(s: &str) -> usize {
        s.chars().count()
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(chunk_text("你好", 2000), vec!["你好"]);
        assert!(chunk_text("", 2000).is_empty());
        assert!(chunk_text("   ", 2000).is_empty());
    }

    #[test]
    fn paragraphs_split_before_hard_cut() {
        let a = "第一段内容。".repeat(40);
        let b = "第二段内容。".repeat(40);
        let text = format!("{a}\n\n{b}");
        let chunks = chunk_text(&text, 260);
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        assert!(chunks[0].starts_with("第一段"));
        assert!(!chunks[0].contains("第二段"));
        assert!(chunks[1].starts_with("第二段"));
        for c in &chunks {
            assert!(clen(c) <= 260, "{} chars", clen(c));
        }
    }

    #[test]
    fn lines_split_when_no_paragraph_break() {
        let lines: Vec<String> = (0..50).map(|i| format!("第 {i} 行内容")).collect();
        let chunks = chunk_text(&lines.join("\n"), 60);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(clen(c) <= 60, "{} chars: {c}", clen(c));
        }
        let rejoined = chunks.join("\n");
        for i in [0, 17, 49] {
            assert!(rejoined.contains(&format!("第 {i} 行")), "missing line {i}");
        }
    }

    #[test]
    fn unbreakable_text_hard_cuts_at_char_boundary() {
        let blob = "字".repeat(5000);
        let chunks = chunk_text(&blob, 2000);
        assert_eq!(chunks.len(), 3, "{:?}", chunks.iter().map(|c| clen(c)));
        assert!(chunks.iter().all(|c| clen(c) <= 2000));
        assert_eq!(chunks.concat().chars().count(), 5000);
    }

    #[test]
    fn fence_is_closed_and_reopened_across_chunks() {
        let code = "println!(\"hi\");\n".repeat(60);
        let text = format!("说明。\n\n```rust\n{code}```\n\n收尾。");
        let chunks = chunk_text(&text, 200);
        assert!(chunks.len() > 1);
        for (i, c) in chunks.iter().enumerate() {
            let fences = c.matches("```").count();
            assert_eq!(fences % 2, 0, "chunk {i} has an unterminated fence: {c}");
            assert!(clen(c) <= 200, "chunk {i} too long: {} chars", clen(c));
        }
        assert!(chunks[1].starts_with("```rust\n"), "{}", chunks[1]);
        assert!(chunks.last().unwrap().contains("收尾。"));
    }

    #[test]
    fn fence_tag_falls_back_when_language_missing() {
        let code = "x = 1\n".repeat(80);
        let text = format!("```\n{code}```");
        let chunks = chunk_text(&text, 100);
        for c in &chunks {
            assert_eq!(c.matches("```").count() % 2, 0, "{c}");
            assert!(clen(c) <= 100);
        }
        assert!(chunks[1].starts_with("```\n"), "{}", chunks[1]);
    }

    #[test]
    fn cjk_counts_chars_not_bytes() {
        let text = "汉".repeat(3901);
        let chunks = chunk_text(&text, 3900);
        assert_eq!(chunks.len(), 2, "{:?}", chunks.iter().map(|c| clen(c)));
        assert!(chunks.iter().all(|c| clen(c) <= 3900));
        assert_eq!(chunks.concat().chars().count(), 3901);
    }

    #[test]
    fn cjk_punctuation_breaks_before_hard_cut() {
        // 无空格长中文：按句号切，标点留在上一泡末尾。
        let sentence = "这一段没有空格也没有换行只是不断往后写";
        let text = format!("{sentence}。{sentence}。{sentence}。");
        let chunks = chunk_text(&text, 40);
        assert!(chunks.len() > 1, "{chunks:?}");
        for c in &chunks {
            assert!(clen(c) <= 40, "{} chars: {c}", clen(c));
            assert!(c.ends_with('。'), "cut should land after the mark: {c}");
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn inline_fence_mention_does_not_flip_parity() {
        // 正文里行内提到 ``` 不算开闭围栏，不该补出 stray 闭合标记。
        let text = format!("用 ``` 包代码即可。\n\n{}", "填充内容。".repeat(120));
        let chunks = chunk_text(&text, 200);
        assert!(chunks.len() > 1, "{chunks:?}");
        for c in &chunks {
            assert!(!c.ends_with("```"), "stray fence close appended: {c}");
            assert!(!c.starts_with("```"), "stray fence reopen prepended: {c}");
        }
    }
}
