//! Typed gate input. Replaces Python's untyped `ctx: Any` dict.

#[derive(Clone, Debug)]
pub struct GateCtx<'a> {
    pub session_id: &'a str,
    pub iteration: u32,
    pub tokens_used: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub last_tool: Option<&'a ToolFingerprint>,
    pub tool_names: &'a [String],
    pub fingerprints: &'a [ToolFingerprint],
}

impl<'a> GateCtx<'a> {
    pub fn new(session_id: &'a str) -> Self {
        Self {
            session_id,
            iteration: 0,
            tokens_used: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            last_tool: None,
            tool_names: &[],
            fingerprints: &[],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolFingerprint {
    pub name: String,
    pub args_hash: String,
    pub path: Option<String>,
    /// Same-argv poll (status/diff/log/tail/AwaitShell). Doom must not halt these.
    pub poll: bool,
}

impl ToolFingerprint {
    pub fn new(name: impl Into<String>, args: &str) -> Self {
        let name = name.into();
        let poll = looks_like_poll(&name, args);
        Self {
            name,
            args_hash: hash_args(args),
            path: None,
            poll,
        }
    }

    pub fn with_path(mut self, path: Option<String>) -> Self {
        self.path = path.filter(|s| !s.is_empty());
        self
    }
}

pub fn looks_like_poll(name: &str, args: &str) -> bool {
    let dispatch = crate::tools_schema::dispatch_name(name);
    if dispatch == "awaitshell" {
        return true;
    }
    if dispatch != "bash" {
        return false;
    }
    let cmd = poll_command(args);
    let mut tokens = cmd.split_whitespace();
    let Some(bin) = tokens.next() else {
        return false;
    };
    let bin = bin.rsplit('/').next().unwrap_or(bin);
    match bin {
        "tail" | "watch" | "journalctl" => true,
        "git" => tokens
            .next()
            .is_some_and(|s| matches!(s, "status" | "diff" | "log" | "show")),
        _ => false,
    }
}

fn poll_command(args: &str) -> String {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| {
            v.get("command")
                .and_then(|c| c.as_str())
                .map(|s| s.to_ascii_lowercase())
        })
        .unwrap_or_else(|| args.to_ascii_lowercase())
}

/// SHA-256 of the full argument bytes, truncated to 16 hex chars.
pub fn hash_args(raw: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(raw.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_args_is_16_hex_and_uses_full_bytes() {
        assert_eq!(hash_args("hi").len(), 16);
        assert!(hash_args("hi").chars().all(|c| c.is_ascii_hexdigit()));
        let a = format!("{}a", "x".repeat(2048));
        let b = format!("{}b", "x".repeat(2048));
        assert_ne!(hash_args(&a), hash_args(&b));
        assert_eq!(hash_args("same"), hash_args("same"));
    }

    #[test]
    fn poll_is_token_not_substring() {
        assert!(looks_like_poll("bash", r#"{"command":"git status"}"#));
        assert!(looks_like_poll("Shell", r#"{"command":"git diff --stat"}"#));
        assert!(looks_like_poll("AwaitShell", r#"{"task_id":"s1"}"#));
        assert!(!looks_like_poll("bash", r#"{"command":"echo changelog"}"#));
        assert!(!looks_like_poll("bash", r#"{"command":"ls"}"#));
        assert!(!looks_like_poll("bash", r#"{"command":"cargo test"}"#));
        assert!(!looks_like_poll("bash", r#"{"command":"sleep 1"}"#));
        assert!(!looks_like_poll("Read", r#"{"path":"a.rs"}"#));
    }
}
