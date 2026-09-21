//! Scrub secrets before they land in compact summaries, memory notes, or logs.
//!
//! Session JSONL stays the evidence (already 0600). Compact / memory markdown
//! used to copy `Prior User` verbatim, including passwords typed in chat.

use std::sync::OnceLock;

use regex::Regex;

const REDACTED: &str = "[redacted]";

pub fn redact(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut out = input.to_string();
    for re in labeled_res() {
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                format!("{}{REDACTED}", &caps[1])
            })
            .into_owned();
    }
    out = pem_re()
        .replace_all(&out, "[redacted-private-key]")
        .into_owned();
    out = basic_auth_re()
        .replace_all(&out, |caps: &regex::Captures| {
            format!("{}:{REDACTED}@", &caps[1])
        })
        .into_owned();
    for re in token_res() {
        out = re.replace_all(&out, REDACTED).into_owned();
    }
    out = sshpass_re()
        .replace_all(&out, |caps: &regex::Captures| {
            format!("{}{REDACTED}", &caps[1])
        })
        .into_owned();
    out
}

fn labeled_res() -> &'static [Regex] {
    static LOCK: OnceLock<Vec<Regex>> = OnceLock::new();
    LOCK.get_or_init(|| {
        vec![
            Regex::new(
                r"(?i)(\b(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|auth(?:orization)?|jwt|credential)\s*[:=]\s*)(\S+)",
            )
            .expect("labeled secret"),
            Regex::new(r"((?:密码|口令|密钥|令牌)\s*[：:=]\s*)(\S+)").expect("zh labeled secret"),
        ]
    })
}

fn token_res() -> &'static [Regex] {
    static LOCK: OnceLock<Vec<Regex>> = OnceLock::new();
    LOCK.get_or_init(|| {
        vec![
            Regex::new(r"\bghp_[A-Za-z0-9]{20,}").expect("ghp"),
            Regex::new(r"\bgithub_pat_[A-Za-z0-9_]{20,}").expect("github_pat"),
            Regex::new(r"\bxox[baprs]-[A-Za-z0-9-]{10,}").expect("slack"),
            Regex::new(r"\bAKIA[0-9A-Z]{16}\b").expect("aws"),
            Regex::new(r"\bsk-(?:proj-)?[A-Za-z0-9_-]{20,}").expect("sk"),
            Regex::new(r"\bxai-[A-Za-z0-9_-]{20,}").expect("xai"),
            Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9\-._~+/]+=*").expect("bearer"),
        ]
    })
}

fn pem_re() -> &'static Regex {
    static LOCK: OnceLock<Regex> = OnceLock::new();
    LOCK.get_or_init(|| {
        Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----")
            .expect("pem")
    })
}

fn basic_auth_re() -> &'static Regex {
    static LOCK: OnceLock<Regex> = OnceLock::new();
    LOCK.get_or_init(|| Regex::new(r"(https?://[^:\s/]+):([^\s@/]+)@").expect("basic-auth"))
}

fn sshpass_re() -> &'static Regex {
    static LOCK: OnceLock<Regex> = OnceLock::new();
    LOCK.get_or_init(|| Regex::new(r"(?i)(sshpass\s+-p\s*)(\S+)").expect("sshpass"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labeled_password_is_stripped() {
        let s = redact("VPS password: hunter2-not-real please use it");
        assert!(!s.contains("hunter2-not-real"), "{s}");
        assert!(s.contains("[redacted]"), "{s}");
        assert!(s.contains("VPS password:"), "{s}");
    }

    #[test]
    fn chinese_password_label() {
        let s = redact("服务器密码：abcDEF123 keep-this");
        assert!(!s.contains("abcDEF123"), "{s}");
        assert!(s.contains("keep-this"), "{s}");
    }

    #[test]
    fn password_as_a_word_is_kept() {
        let s = redact("use password hashing, not plaintext");
        assert_eq!(s, "use password hashing, not plaintext");
    }

    #[test]
    fn github_pat_and_url_userinfo() {
        let s = redact(
            "token ghp_abcdefghijklmnopqrstuvwxyz012345 and https://root:s3cret@vps.example/",
        );
        assert!(!s.contains("ghp_abcdefghijklmnopqrstuvwxyz012345"), "{s}");
        assert!(!s.contains("s3cret"), "{s}");
        assert!(s.contains("https://root:[redacted]@vps.example/"), "{s}");
    }

    #[test]
    fn pem_block() {
        let raw = "-----BEGIN PRIVATE KEY-----\nMIIHideMe\n-----END PRIVATE KEY-----";
        let s = redact(raw);
        assert!(!s.contains("MIIHideMe"), "{s}");
        assert_eq!(s, "[redacted-private-key]");
    }

    #[test]
    fn idempotent() {
        let once = redact("password: abc123");
        assert_eq!(redact(&once), once);
    }

    #[test]
    fn xai_live_key_is_stripped() {
        let s = redact("key xai-abcdefghijklmnopqrstuvwxyz012345 keep");
        assert!(!s.contains("abcdefghijklmnopqrstuvwxyz012345"), "{s}");
        assert!(s.contains("[redacted]"), "{s}");
        assert!(s.contains("keep"), "{s}");
        assert_eq!(redact("header xai-grok-cli"), "header xai-grok-cli");
    }
}
