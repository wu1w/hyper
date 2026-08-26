//! Role + boundary. Workspace AGENT.md still wins over the builtin.

use std::fs;
use std::path::Path;

pub const AGENT_MD_NAME: &str = "AGENT.md";

/// Short Cursor-shaped identity. Tool how-to lives in `tools[]`, not here.
/// Office chunk-read is a hidden `[doc-read]` card, not this loop prefix.
/// Workspace / home AGENT.md still override this.
pub const DEFAULT_AGENT_MD: &str = "\
You are grok-hyper, an agent in this workspace. Follow the user's request. \
If they named files, work in those files. Do not expand scope or add extras \
they did not ask for.

Lead with the answer, then supporting detail, for a reader who has not seen \
your tool calls. Define project terms on first use. Complete sentences. \
Backticks for files, functions, and commands. Bold only the few words that \
matter. Match the user's language.

When citing code, use ```startLine:endLine:filepath with the snippet inside. \
That is the only citation format.

Prefer editing existing files. Match local style. Do not commit, push, or \
open a PR unless they ask.

Use the tools provided. Prefer Read, Glob, and Grep over Shell cat, ls, or \
rg. Independent read-only calls belong in one turn. Do not parallel writes \
to the same path. Paths are workspace-relative unless absolute. Write \
complete files; no placeholder ellipses. Independent multi-step work can go \
to Task; do not spawn one for a single Read.
";

/// Back-compat alias. Builtin no longer splits office vs coding.
pub const CODING_AGENT_MD: &str = DEFAULT_AGENT_MD;

/// Back-compat alias for tests that still name the frozen system blob.
pub const CODING_SYSTEM_PROMPT: &str = DEFAULT_AGENT_MD;

pub fn builtin_role_boundary(_coding: bool) -> &'static str {
    DEFAULT_AGENT_MD
}

pub fn load_role_boundary(
    workspace: &Path,
    home: Option<&Path>,
    file: &str,
    coding: bool,
) -> String {
    let name = if file.trim().is_empty() {
        AGENT_MD_NAME
    } else {
        file.trim()
    };
    for root in [Some(workspace), home] {
        let Some(root) = root else { continue };
        let path = root.join(name);
        if let Ok(raw) = fs::read_to_string(&path) {
            let t = raw.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    builtin_role_boundary(coding).trim().to_string()
}

pub fn session_prompt(workspace: &Path, home: Option<&Path>, file: &str, coding: bool) -> String {
    let role = load_role_boundary(workspace, home, file, coding);
    with_workspace(&role, &workspace.display().to_string())
}

pub fn with_workspace(role_boundary: &str, workspace: &str) -> String {
    format!("{}\nWorkspace:\n    {workspace}\n", role_boundary.trim())
}

/// Previous builtin office snapshot — not a user-written AGENT.md.
pub fn is_stale_office_builtin(text: &str) -> bool {
    text.contains("You are grok-hyper, a local office assistant")
        && text.contains("not a senior software engineer")
}

/// Previous builtin coding snapshot — not a user-written AGENT.md.
pub fn is_stale_coding_builtin(text: &str) -> bool {
    text.contains("You are grok-hyper, a coding agent in this workspace")
        && text.contains("Cursor's tool contract")
}

/// Previous Cursor-roster snapshot that restated `tools[]` in system.
pub fn is_stale_roster_builtin(text: &str) -> bool {
    text.contains("You are grok-hyper, an agent in this workspace")
        && text.contains("Tool names and schemas are in tools[]")
}

/// Previous builtin that lectured office chunk-read in every turn.
pub fn is_stale_doc_read_builtin(text: &str) -> bool {
    text.contains("You are grok-hyper, an agent in this workspace")
        && text.contains("Office files: Read with no offset returns an outline")
}

/// Previous builtin that never mentioned Task.
pub fn is_stale_task_builtin(text: &str) -> bool {
    text.contains("You are grok-hyper, an agent in this workspace")
        && text.contains("Independent read-only calls belong in one turn")
        && text.contains("no placeholder ellipses")
        && !text.contains("Independent multi-step work can go to Task")
}

pub fn is_stale_builtin(text: &str) -> bool {
    is_stale_office_builtin(text)
        || is_stale_coding_builtin(text)
        || is_stale_roster_builtin(text)
        || is_stale_doc_read_builtin(text)
        || is_stale_task_builtin(text)
}

/// Rewrite home AGENT.md when it is still a previous builtin snapshot.
pub fn migrate_stale_home_agent_md(path: &Path, _coding: bool) {
    let Ok(raw) = fs::read_to_string(path) else {
        return;
    };
    if !is_stale_builtin(&raw) {
        return;
    }
    let _ = fs::write(path, DEFAULT_AGENT_MD.trim());
}

/// Tests / callers that only have a display path (no AGENT.md search).
pub fn coding_prompt(workspace: &str) -> String {
    with_workspace(DEFAULT_AGENT_MD, workspace)
}

/// Skill / MCP name lists only. Empty catalogs emit nothing.
pub fn periphery_section(skills_catalog: &str, mcp_catalog: &str) -> String {
    let mut s = String::new();
    if !skills_catalog.is_empty() {
        s.push('\n');
        s.push_str(skills_catalog.trim_end());
        s.push('\n');
    }
    if !mcp_catalog.is_empty() {
        s.push('\n');
        s.push_str(mcp_catalog.trim_end());
        s.push('\n');
    }
    s
}

#[cfg(test)]
fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_is_cursor_agent_contract() {
        let n = word_count(DEFAULT_AGENT_MD);
        assert!(n >= 80, "Cursor prompt too short: {n} words");
        assert!(n <= 210, "Cursor prompt too long: {n} words");
        assert!(DEFAULT_AGENT_MD.contains("an agent in this workspace"));
        assert!(!DEFAULT_AGENT_MD.contains("coding agent"));
        assert!(!DEFAULT_AGENT_MD.contains("office assistant"));
        assert!(!DEFAULT_AGENT_MD.contains("Tool names and schemas"));
        assert!(DEFAULT_AGENT_MD.contains("Read"));
        assert!(DEFAULT_AGENT_MD.contains("Grep"));
        assert!(!DEFAULT_AGENT_MD.contains("1-based chunk"));
        assert!(!DEFAULT_AGENT_MD.contains("Office files"));
        assert!(DEFAULT_AGENT_MD.contains("startLine:endLine:filepath"));
        assert!(!DEFAULT_AGENT_MD.contains("AskQuestion"));
        assert!(!DEFAULT_AGENT_MD.contains("TodoWrite"));
        assert!(!DEFAULT_AGENT_MD.contains("工作区助手"));
        assert!(!DEFAULT_AGENT_MD.contains("TODO.md"));
        assert!(!DEFAULT_AGENT_MD.contains("Think first"));
        assert!(!DEFAULT_AGENT_MD.contains("云端"));
        assert!(!DEFAULT_AGENT_MD.contains("coding overlay"));
    }

    #[test]
    fn coding_flag_does_not_change_builtin() {
        assert_eq!(builtin_role_boundary(false), builtin_role_boundary(true));
        assert_eq!(builtin_role_boundary(false), DEFAULT_AGENT_MD);
        assert!(!builtin_role_boundary(true).contains("coding overlay"));
    }

    #[test]
    fn diy_file_wins_over_coding_flag() {
        let dir =
            std::env::temp_dir().join(format!("grok-hyper-md-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENT.md"), "家里猫。别出家门。\n").unwrap();
        let s = load_role_boundary(&dir, None, "AGENT.md", true);
        assert_eq!(s, "家里猫。别出家门。");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn periphery_empty_when_nothing_to_say() {
        assert!(periphery_section("", "").is_empty());
        let s = periphery_section("Skills: pdf\n", "");
        assert!(s.contains("pdf"));
        assert!(!s.contains("MEMORY.md"));
        assert!(!s.contains("do not guess"));
    }

    #[test]
    fn prompt_includes_workspace_boundary() {
        let s = coding_prompt("/tmp/ws");
        assert!(s.contains("an agent in this workspace"));
        assert!(s.contains("/tmp/ws"));
    }

    #[test]
    fn stale_office_builtin_is_detected() {
        assert!(is_stale_office_builtin(
            "You are grok-hyper, a local office assistant in this workspace.\nnot a senior software engineer"
        ));
        assert!(!is_stale_office_builtin(DEFAULT_AGENT_MD));
    }

    #[test]
    fn stale_coding_builtin_is_detected() {
        assert!(is_stale_coding_builtin(
            "You are grok-hyper, a coding agent in this workspace. You follow Cursor's tool contract on grok-4.6."
        ));
        assert!(!is_stale_coding_builtin(DEFAULT_AGENT_MD));
        assert!(is_stale_builtin(
            "You are grok-hyper, a coding agent in this workspace. You follow Cursor's tool contract."
        ));
        assert!(
            !is_stale_coding_builtin(
                "I am a coding agent in this workspace. You follow Cursor's tool contract on grok-4.6."
            ),
            "handwritten copy without the grok-hyper fingerprint must stay"
        );
        assert!(!is_stale_office_builtin(
            "I am a local office assistant in this workspace.\nnot a senior software engineer"
        ));
        assert!(is_stale_roster_builtin(
            "You are grok-hyper, an agent in this workspace.\nTool names and schemas are in tools[]."
        ));
        assert!(!is_stale_roster_builtin(DEFAULT_AGENT_MD));
        assert!(is_stale_doc_read_builtin(
            "You are grok-hyper, an agent in this workspace.\nOffice files: Read with no offset returns an outline; then offset is a 1-based chunk.\n"
        ));
        assert!(!is_stale_doc_read_builtin(DEFAULT_AGENT_MD));
        assert!(is_stale_task_builtin(
            "You are grok-hyper, an agent in this workspace. Independent read-only calls belong in one turn. no placeholder ellipses"
        ));
        assert!(!is_stale_task_builtin(DEFAULT_AGENT_MD));
    }

    #[test]
    fn migrate_rewrites_stale_builtin_files() {
        let dir =
            std::env::temp_dir().join(format!("grok-hyper-md-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("AGENT.md");
        std::fs::write(
            &path,
            "You are grok-hyper, a local office assistant in this workspace.\nYou are not an IDE, not a review bot, and not a senior software engineer.\n",
        )
        .unwrap();
        migrate_stale_home_agent_md(&path, false);
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.contains("an agent in this workspace"), "{s}");
        assert!(!s.contains("office assistant"), "{s}");
        assert!(!s.contains("coding agent"), "{s}");
        std::fs::write(
            &path,
            "You are grok-hyper, a coding agent in this workspace. You follow Cursor's tool contract on grok-4.6.\n",
        )
        .unwrap();
        migrate_stale_home_agent_md(&path, true);
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.contains("an agent in this workspace"), "{s}");
        assert!(!s.contains("coding agent"), "{s}");
        std::fs::write(&path, "家里猫。别出家门。\n").unwrap();
        migrate_stale_home_agent_md(&path, false);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "家里猫。别出家门。\n"
        );
        std::fs::write(
            &path,
            "I am a coding agent in this workspace. You follow Cursor's tool contract.\n",
        )
        .unwrap();
        migrate_stale_home_agent_md(&path, true);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "I am a coding agent in this workspace. You follow Cursor's tool contract.\n"
        );
        std::fs::write(
            &path,
            "You are grok-hyper, an agent in this workspace. You follow Cursor's tool contract on grok-4.6.\nTool names and schemas are in tools[]. Cursor names: Read, Write\n",
        )
        .unwrap();
        migrate_stale_home_agent_md(&path, false);
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.contains("an agent in this workspace"), "{s}");
        assert!(!s.contains("Tool names and schemas"), "{s}");
        std::fs::write(
            &path,
            "You are grok-hyper, an agent in this workspace. Follow the user's request.\nOffice files: Read with no offset returns an outline; then offset is a 1-based chunk.\n",
        )
        .unwrap();
        migrate_stale_home_agent_md(&path, false);
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.contains("an agent in this workspace"), "{s}");
        assert!(!s.contains("Office files"), "{s}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
