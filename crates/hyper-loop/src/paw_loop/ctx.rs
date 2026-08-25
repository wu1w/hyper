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
}

impl ToolFingerprint {
    pub fn new(name: impl Into<String>, args: &str) -> Self {
        Self {
            name: name.into(),
            args_hash: hash_args(args),
            path: None,
        }
    }

    pub fn with_path(mut self, path: Option<String>) -> Self {
        self.path = path.filter(|s| !s.is_empty());
        self
    }
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
}
