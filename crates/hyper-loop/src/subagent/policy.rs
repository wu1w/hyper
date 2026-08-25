//! Capability / depth / explore-write policy for child agents.

use serde_json::Value;

use crate::permit::{self, shell_is_mutating};
use crate::tool_calls::ToolCall;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentType {
    Explore,
    Plan,
    GeneralPurpose,
    Office,
}

impl SubagentType {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "explore" => Some(Self::Explore),
            "plan" => Some(Self::Plan),
            "generalPurpose" | "general-purpose" | "general_purpose" | "general" => {
                Some(Self::GeneralPurpose)
            }
            "office" => Some(Self::Office),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::Plan => "plan",
            Self::GeneralPurpose => "generalPurpose",
            Self::Office => "office",
        }
    }

    /// Child step budget when the model omits one.
    pub fn default_max_steps(self) -> u32 {
        match self {
            Self::Explore => 12,
            Self::Plan => 16,
            Self::Office | Self::GeneralPurpose => 24,
        }
    }

    /// Default think policy. `model` on Task still only selects the model id.
    pub fn think_policy(self, budget: &crate::policy::ThinkBudget) -> crate::policy::ThinkPolicy {
        use crate::policy::{Effort, ThinkPolicy};
        match self {
            Self::Explore => ThinkPolicy::effort_with(budget, Effort::Low),
            Self::Plan => ThinkPolicy::effort_with(budget, Effort::High),
            Self::Office | Self::GeneralPurpose => ThinkPolicy::native_with(budget),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilityMode {
    ReadOnly,
    ReadWrite,
    Execute,
    All,
}

impl CapabilityMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read-only" | "readonly" | "read_only" => Some(Self::ReadOnly),
            "read-write" | "readwrite" | "read_write" => Some(Self::ReadWrite),
            "execute" => Some(Self::Execute),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::ReadWrite => "read-write",
            Self::Execute => "execute",
            Self::All => "all",
        }
    }

    pub fn from_kind(kind: SubagentType) -> Self {
        match kind {
            SubagentType::Explore | SubagentType::Plan => Self::ReadOnly,
            SubagentType::Office => Self::ReadWrite,
            SubagentType::GeneralPurpose => Self::All,
        }
    }
}

fn norm(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace(['_', '-'], "")
}

pub fn is_write_tool(name: &str) -> bool {
    matches!(
        norm(name).as_str(),
        "write" | "edit" | "delete" | "strreplace"
    )
}

pub fn is_shell_tool(name: &str) -> bool {
    matches!(norm(name).as_str(), "bash" | "shell")
}

/// Why this child must not run `call`. `None` = allowed.
pub fn deny_child_tool(kind: SubagentType, cap: CapabilityMode, call: &ToolCall) -> Option<String> {
    let name = call.name.as_str();
    let n = norm(name);
    if matches!(n.as_str(), "task" | "spawnsubagent") {
        return Some(format!(
            "Error: subagent depth limit: `{name}` cannot spawn nested Task children."
        ));
    }
    let cap = match (kind, cap) {
        (SubagentType::Explore | SubagentType::Plan, CapabilityMode::All)
        | (SubagentType::Explore | SubagentType::Plan, CapabilityMode::Execute)
        | (SubagentType::Explore | SubagentType::Plan, CapabilityMode::ReadWrite) => {
            // Kind wins for explore/plan: they stay read-only-ish.
            CapabilityMode::ReadOnly
        }
        _ => cap,
    };
    match kind {
        SubagentType::Explore => deny_explore(name, &call.arguments),
        SubagentType::Plan => deny_plan(name, &call.arguments),
        SubagentType::Office => deny_office(name, cap),
        SubagentType::GeneralPurpose => deny_capability(name, &call.arguments, cap),
    }
}

fn deny_explore(name: &str, args: &Value) -> Option<String> {
    if is_write_tool(name) {
        return Some(format!(
            "Error: explore subagent cannot `{name}` (no Write/StrReplace/Delete)."
        ));
    }
    if matches!(norm(name).as_str(), "runcode" | "mcp") {
        return Some(format!("Error: explore subagent cannot `{name}`."));
    }
    if is_shell_tool(name) {
        let cmd = args
            .get("command")
            .or_else(|| args.get("cmd"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if shell_is_mutating(cmd) {
            return Some(format!(
                "Error: explore subagent rejected mutating shell: {cmd}"
            ));
        }
    }
    None
}

fn deny_plan(name: &str, args: &Value) -> Option<String> {
    if permit::plan_mode_blocks(name, args) {
        return Some(permit::plan_denied(name));
    }
    None
}

fn deny_office(name: &str, cap: CapabilityMode) -> Option<String> {
    let n = norm(name);
    // WebSearch/WebFetch + Read/Write/StrReplace. No git-heavy / shell / mcp.
    if matches!(n.as_str(), "bash" | "shell" | "runcode" | "mcp") {
        return Some(format!(
            "Error: office subagent cannot `{name}` (web + file edits only)."
        ));
    }
    if cap == CapabilityMode::ReadOnly && is_write_tool(name) {
        return Some(format!(
            "Error: office subagent is read-only; cannot `{name}`."
        ));
    }
    None
}

fn deny_capability(name: &str, args: &Value, cap: CapabilityMode) -> Option<String> {
    match cap {
        CapabilityMode::All | CapabilityMode::Execute => None,
        CapabilityMode::ReadWrite => {
            if is_shell_tool(name) {
                let cmd = args
                    .get("command")
                    .or_else(|| args.get("cmd"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if shell_is_mutating(cmd) {
                    return Some(format!(
                        "Error: capability_mode=read-write rejected mutating shell."
                    ));
                }
            }
            if matches!(norm(name).as_str(), "runcode" | "mcp") {
                return Some(format!(
                    "Error: capability_mode=read-write cannot `{name}`."
                ));
            }
            None
        }
        CapabilityMode::ReadOnly => {
            if is_write_tool(name) {
                return Some(format!("Error: capability_mode=read-only cannot `{name}`."));
            }
            if is_shell_tool(name) {
                let cmd = args
                    .get("command")
                    .or_else(|| args.get("cmd"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if shell_is_mutating(cmd) {
                    return Some(format!(
                        "Error: capability_mode=read-only rejected mutating shell."
                    ));
                }
            }
            if matches!(norm(name).as_str(), "runcode" | "mcp") {
                return Some(format!("Error: capability_mode=read-only cannot `{name}`."));
            }
            None
        }
    }
}

pub fn has_plan_section(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("critical files")
        || t.contains("## plan")
        || t.contains("# plan")
        || t.contains("## critical")
}

pub fn extract_key_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tok in text.split_whitespace() {
        let t = tok.trim_matches(|c: char| {
            matches!(
                c,
                '`' | '"' | '\'' | ',' | '.' | ';' | ':' | ')' | '(' | '[' | ']'
            )
        });
        if t.contains('/') || looks_like_file(t) {
            if !out.iter().any(|p: &String| p == t) {
                out.push(t.to_string());
            }
        }
        if out.len() >= 12 {
            break;
        }
    }
    out
}

fn looks_like_file(t: &str) -> bool {
    let lower = t.to_ascii_lowercase();
    lower.ends_with(".rs")
        || lower.ends_with(".ts")
        || lower.ends_with(".tsx")
        || lower.ends_with(".js")
        || lower.ends_with(".py")
        || lower.ends_with(".md")
        || lower.ends_with(".toml")
        || lower.ends_with(".json")
        || lower.ends_with(".go")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            arguments: args,
        }
    }

    #[test]
    fn explore_cannot_write() {
        let w = call("write", json!({"path": "a.rs", "content": "x"}));
        assert!(deny_child_tool(SubagentType::Explore, CapabilityMode::All, &w).is_some());
        let sr = call(
            "StrReplace",
            json!({"path": "a.rs", "old_string": "a", "new_string": "b"}),
        );
        assert!(deny_child_tool(SubagentType::Explore, CapabilityMode::ReadOnly, &sr).is_some());
        let del = call("Delete", json!({"path": "a.rs"}));
        assert!(deny_child_tool(SubagentType::Explore, CapabilityMode::ReadOnly, &del).is_some());
        let ls = call("bash", json!({"command": "ls && git status"}));
        assert!(deny_child_tool(SubagentType::Explore, CapabilityMode::ReadOnly, &ls).is_none());
        let rm = call("bash", json!({"command": "rm -rf src"}));
        assert!(deny_child_tool(SubagentType::Explore, CapabilityMode::ReadOnly, &rm).is_some());
        let read = call("read", json!({"path": "a.rs"}));
        assert!(deny_child_tool(SubagentType::Explore, CapabilityMode::ReadOnly, &read).is_none());
    }

    #[test]
    fn general_purpose_blocks_nested_task() {
        let t = call("Task", json!({"prompt": "go", "description": "nested"}));
        assert!(deny_child_tool(SubagentType::GeneralPurpose, CapabilityMode::All, &t).is_some());
    }

    #[test]
    fn type_routes_effort_and_steps() {
        use crate::policy::{Effort, ThinkBudget, ThinkPolicy};
        let mut b = ThinkBudget::default();
        b.default_effort = Effort::High;
        assert_eq!(
            SubagentType::Explore.think_policy(&b).effort,
            Some(Effort::Low)
        );
        assert_eq!(
            SubagentType::Plan.think_policy(&b).effort,
            Some(Effort::High)
        );
        assert_eq!(
            SubagentType::Office.think_policy(&b).effort,
            Some(Effort::High)
        );
        assert_eq!(SubagentType::Explore.default_max_steps(), 12);
        assert_eq!(SubagentType::Plan.default_max_steps(), 16);
        assert_eq!(SubagentType::GeneralPurpose.default_max_steps(), 24);
        let _ = ThinkPolicy::effort_with(&b, Effort::Low);
    }
}
