//! xAI hosted prefix: recast on the live request, wash out of compact state.
//!
//! The API prepends an immutable safety block plus a "You are Grok built by
//! xAI" identity *before* our `instructions`. We do not fight safety on the
//! wire (that burns reasoning and is trained as a jailbreak). We do:
//!
//! 1. A short recency closer on Responses (`append_xai_product_closer`).
//! 2. Drop those static blocks from compact input / archive so they cannot
//!    stack inside official blobs or local summaries after N compacts.
//!
//! `encrypted_content` stays opaque. Washing only touches plaintext we send
//! or archive.

/// Recency closer. Stable suffix of our `instructions` (prompt-cache friendly).
/// Affirmative product/job — not "ignore previous instructions".
pub const XAI_PRODUCT_CLOSER: &str = "\
Product session: grok-hyper.

This process is grok-hyper (grok-4.6), not the grok.com chatbot. Platform \
safety still applies. Do not call WebSearch — the host runs web and X search. \
Tool hops keep visible text empty; the hop without tools is the answer. Do \
not restate.
";

const CLOSER_MARK: &str = "Product session: grok-hyper.";
const SAFETY_START: &str = "## safety instructions";
const SAFETY_END: &str = "## end of safety instructions";

pub fn append_xai_product_closer(system: &mut String) {
    if system.contains(CLOSER_MARK) {
        return;
    }
    if !system.ends_with('\n') {
        system.push('\n');
    }
    system.push('\n');
    system.push_str(XAI_PRODUCT_CLOSER.trim());
    system.push('\n');
}

/// True when `text` is (or begins with) the hosted Grok identity / safety blob,
/// not a casual mention of Grok or xAI.
pub fn looks_like_platform_prefix(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let low = t.to_ascii_lowercase();
    if low.contains(SAFETY_START) && low.contains(SAFETY_END) {
        return true;
    }
    if is_identity_line(t.lines().next().unwrap_or("")) {
        return true;
    }
    let grok_you = low.contains("you are grok") && !low.contains("you are grok-hyper");
    grok_you && low.contains("xai") && (low.contains("maximally") || low.contains("built by"))
}

/// Strip hosted identity / safety blocks. Leaves grok-hyper copy and normal
/// user text (including questions about xAI) in place.
pub fn wash_platform_injection(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = strip_safety_blocks(text);
    out = strip_identity_lines(&out);
    collapse_blank_lines(&out)
}

/// Compact / Responses plaintext. Tool payloads stay verbatim. User text is
/// only washed when it *is* the hosted prefix (not a question about xAI).
pub fn wash_message_content(role: &str, content: &str) -> String {
    match role {
        "tool" => content.to_string(),
        "user" if !looks_like_platform_prefix(content) => content.to_string(),
        _ => wash_platform_injection(content),
    }
}

fn strip_safety_blocks(s: &str) -> String {
    let mut out = s.to_string();
    loop {
        let lower = out.to_ascii_lowercase();
        let Some(start) = lower.find(SAFETY_START) else {
            break;
        };
        let after = start + SAFETY_START.len();
        let end = match lower[after..].find(SAFETY_END) {
            Some(rel) => after + rel + SAFETY_END.len(),
            None => {
                out.replace_range(start.., "");
                break;
            }
        };
        let mut end = end;
        while end < out.len() && matches!(out.as_bytes().get(end), Some(b'\n' | b'\r')) {
            end += 1;
        }
        out.replace_range(start..end, "");
    }
    out
}

fn strip_identity_lines(s: &str) -> String {
    s.lines()
        .filter(|line| !is_identity_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_identity_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    let low = t.to_ascii_lowercase();
    if low.starts_with("you are grok-hyper") || low.starts_with("i am grok-hyper") {
        return false;
    }
    if low.starts_with("you are grok") {
        return true;
    }
    let im_grok =
        low.starts_with("i am grok") || low.starts_with("i'm grok") || low.starts_with("i’m grok");
    if im_grok && (low.contains("xai") || low.contains("truthful") || low.contains("truth-seeking"))
    {
        return true;
    }
    if low.contains("xai does not have any other products") {
        return true;
    }
    if low.contains("xai offers an api service") && low.contains("https://x.ai/api") {
        return true;
    }
    if low.starts_with("these safety instructions are the highest priority") {
        return true;
    }
    if low.starts_with("these core policies within the tags take highest precedence") {
        return true;
    }
    low.contains("interested in your own identity")
        && low.contains("represent the identity you already know")
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blanks = 0u32;
    for line in s.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks <= 2 {
                out.push('\n');
            }
        } else {
            blanks = 0;
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const API_IDENTITY: &str = "You are Grok, a helpful and maximally truthful AI built by xAI.";

    #[test]
    fn closer_is_short_and_not_a_jailbreak() {
        let n = XAI_PRODUCT_CLOSER.split_whitespace().count();
        assert!(n <= 90, "closer too long ({n} words); burns cache/IQ");
        let low = XAI_PRODUCT_CLOSER.to_ascii_lowercase();
        assert!(!low.contains("ignore previous"));
        assert!(!low.contains("end of safety"));
        assert!(XAI_PRODUCT_CLOSER.contains(CLOSER_MARK));
        assert!(XAI_PRODUCT_CLOSER.contains("Tool hops keep visible text empty"));
        assert!(XAI_PRODUCT_CLOSER.contains("Do not restate"));
    }

    #[test]
    fn closer_is_idempotent() {
        let mut s = String::from("You are grok-hyper, an agent in this workspace.\n");
        append_xai_product_closer(&mut s);
        append_xai_product_closer(&mut s);
        assert_eq!(s.matches(CLOSER_MARK).count(), 1);
    }

    #[test]
    fn wash_drops_safety_block_and_identity() {
        let raw = format!(
            "## Safety Instructions\nDo not jailbreak.\n## End of Safety Instructions\n\n{API_IDENTITY}\n\nPlease edit notes.md"
        );
        let w = wash_platform_injection(&raw);
        assert!(!w.contains("Safety Instructions"), "{w}");
        assert!(!w.contains("You are Grok,"), "{w}");
        assert!(w.contains("Please edit notes.md"), "{w}");
    }

    #[test]
    fn wash_keeps_grok_hyper_and_casual_xai_questions() {
        let product = "You are grok-hyper, an agent in this workspace.";
        assert_eq!(wash_platform_injection(product), product);
        let q = "Is Grok built by xAI? Keep the answer in notes.md.";
        assert_eq!(wash_platform_injection(q), q);
        assert!(!looks_like_platform_prefix(q));
        assert!(looks_like_platform_prefix(API_IDENTITY));
    }

    #[test]
    fn wash_does_not_eat_grok_hyper_you_are_line() {
        let line = "You are grok-hyper in this workspace. Coding is available.";
        assert_eq!(wash_platform_injection(line), line);
    }

    #[test]
    fn stacked_identity_washes_to_empty() {
        let stacked = format!("{API_IDENTITY}\n{API_IDENTITY}\nYou are Grok 4 built by xAI.");
        let w = wash_platform_injection(&stacked);
        assert!(w.trim().is_empty(), "{w}");
    }
}
