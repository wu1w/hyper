//! Frozen OpenAI tool JSON. Byte-stable: parsed with `preserve_order` and never rebuilt by hash maps.
//!
//! Model-facing names are Cursor's (`Read`, `Shell`, `AskQuestion`, …). Executors
//! still accept the Qwen four-pack aliases (`read` / `write` / `edit` / `bash`).
//! Office outline/chunk how-to is the hidden `[doc-read]` card, not these blobs.

use serde_json::Value;

/// Dummy system blob for template tests (not injected by the agent loop).
pub const HARNESS_SYSTEM: &str = "Workspace assistant. Paths are relative.";

const READ: &str = r#"{"type":"function","function":{"name":"Read","description":"Read a file. Path is workspace-relative unless absolute. For text, page with offset (1-based line) and limit. Prefer this over Shell cat. Parallel-safe with Glob and Grep.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative or absolute path."},"offset":{"type":"integer","description":"1-based start line. Omit to read from the start."},"limit":{"type":"integer","description":"Max lines."}},"required":["path"]}}}"#;
const WRITE: &str = r#"{"type":"function","function":{"name":"Write","description":"Create or overwrite a file. Pass contents (legacy content is accepted). Read first if it exists. No placeholder ellipses. Not parallel-safe with other writes to the same path.","parameters":{"type":"object","properties":{"path":{"type":"string"},"contents":{"type":"string","description":"Full file body."},"content":{"type":"string","description":"Legacy alias for contents."}},"required":["path"]}}}"#;
const STR_REPLACE: &str = r#"{"type":"function","function":{"name":"StrReplace","description":"Replace one unique old_string with new_string. Widen old_string until it matches once, unless replace_all. Not parallel-safe with Write/Delete on the same path.","parameters":{"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string","description":"Exact text to find. Must be unique unless replace_all."},"new_string":{"type":"string","description":"Replacement text."},"replace_all":{"type":"boolean","description":"Replace every match. Default false."}},"required":["path","old_string","new_string"]}}}"#;
const DELETE: &str = r#"{"type":"function","function":{"name":"Delete","description":"Delete a file. Prefer StrReplace or Write to change contents. Not parallel-safe with other mutations on the same path.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}"#;
const GLOB: &str = r#"{"type":"function","function":{"name":"Glob","description":"Find paths matching glob_pattern (for example **/*.md). Optional target_directory. Returns paths, not contents. Parallel-safe with Read and Grep.","parameters":{"type":"object","properties":{"glob_pattern":{"type":"string"},"target_directory":{"type":"string","description":"Directory to search under. Default is the workspace root."}},"required":["glob_pattern"]}}}"#;
const GREP: &str = r#"{"type":"function","function":{"name":"Grep","description":"Search file contents with a regex. Optional path, glob, -i, head_limit. Prefer Grep over Shell rg. Parallel-safe with Read, Glob, and WebSearch.","parameters":{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"glob":{"type":"string"},"head_limit":{"type":"integer"},"-i":{"type":"boolean"}},"required":["pattern"]}}}"#;
const SHELL: &str = r#"{"type":"function","function":{"name":"Shell","description":"Run a command in the workspace. Optional working_directory, block_until_ms (timeout is a legacy alias), background. Prefer Read/Grep/Glob over cat/ls/rg. Do not parallel Shell calls that touch the same path.","parameters":{"type":"object","properties":{"command":{"type":"string"},"working_directory":{"type":"string"},"block_until_ms":{"type":"integer","description":"How long to wait before backgrounding. Default is the tool timeout."},"timeout":{"type":"integer","description":"Legacy alias for block_until_ms."},"background":{"type":"boolean","description":"Return immediately with this call id; wait with AwaitShell."}},"required":["command"]}}}"#;
const WEB_SEARCH: &str = r#"{"type":"function","function":{"name":"WebSearch","description":"Search the public web. Pass search_term or query. Not for files in this workspace. Parallel-safe with Read, Glob, Grep, and WebFetch.","parameters":{"type":"object","properties":{"search_term":{"type":"string"},"query":{"type":"string","description":"Legacy alias for search_term."}}}}}"#;
const WEB_FETCH: &str = r#"{"type":"function","function":{"name":"WebFetch","description":"Fetch a URL and extract readable text. Parallel-safe with WebSearch and Read.","parameters":{"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}}}"#;
const TODO_WRITE: &str = r#"{"type":"function","function":{"name":"TodoWrite","description":"Short todo list for multi-step work. Each item needs content and status (pending, in_progress, completed, cancelled). Optional id; merge true updates by id.","parameters":{"type":"object","properties":{"todos":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"content":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed","cancelled"]}},"required":["content","status"]}},"merge":{"type":"boolean","description":"If true, merge by id instead of replacing the list."}},"required":["todos"]}}}"#;
const ASK_QUESTION: &str = r#"{"type":"function","function":{"name":"AskQuestion","description":"Ask one multiple-choice question when a choice would change the work. Do not ask in prose. Blocks until the user answers.","parameters":{"type":"object","properties":{"title":{"type":"string"},"questions":{"type":"array"},"prompt":{"type":"string","description":"Legacy flat prompt when questions is omitted."},"options":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"label":{"type":"string"}},"required":["label"]},"description":"Legacy flat options when questions is omitted."}},"required":["questions"]}}}"#;
const TASK: &str = r#"{"type":"function","function":{"name":"Task","description":"Launch a scoped subagent (depth 1). Required: description (3-5 word UI title) and prompt. subagent_type: explore (read-only, low effort), plan (read-only except plan.md), generalPurpose, office. Omit it or pass inherit to use generalPurpose. Cursor aliases shell, cursor-guide, ci-investigator, bugbot, security-review are accepted. model inherit/fast/composer-* use the parent model. Multiple Task calls in one turn run in parallel. run_in_background true returns the child id immediately; wait with AwaitShell. resume is a previous Task id. isolation none/auto share the parent cwd; worktree is a git checkout that keeps that directory after exit. Do not use for a single Read.","parameters":{"type":"object","properties":{"description":{"type":"string","description":"3-5 word title shown on the Task card."},"prompt":{"type":"string","description":"The subagent's task."},"subagent_type":{"type":"string","description":"explore | plan | generalPurpose | office. inherit and unknown names map to generalPurpose."},"model":{"type":"string","description":"Optional model id. inherit, fast, and composer-* keep the parent model. Does not change effort routing."},"run_in_background":{"type":"boolean","description":"Return immediately with the child id."},"background":{"type":"boolean","description":"Legacy alias for run_in_background."},"resume":{"type":"string","description":"Previous Task id to continue. self starts a new child."},"isolation":{"type":"string","enum":["none","worktree","auto"],"description":"none and auto share the parent cwd. worktree is an explicit git worktree kept after the child finishes."}},"required":["description","prompt"]}}}"#;
const AWAIT_SHELL: &str = r#"{"type":"function","function":{"name":"AwaitShell","description":"Wait on a background Shell or Task. Pass task_id or shell_id and optional block_until_ms.","parameters":{"type":"object","properties":{"task_id":{"type":"string"},"shell_id":{"type":"string"},"block_until_ms":{"type":"integer"}}}}}"#;
const RUN_CODE: &str = r#"{"type":"function","function":{"name":"run_code","description":"Run Python.","parameters":{"type":"object","properties":{"code":{"type":"string"}},"required":["code"]}}}"#;
const RECALL: &str = r#"{"type":"function","function":{"name":"recall","description":"Search session archive.","parameters":{"type":"object","properties":{"query":{"type":"string"},"seq":{"type":"integer"},"blob":{"type":"string"}}}}}"#;
const MEMORY_SEARCH: &str = r#"{"type":"function","function":{"name":"memory_search","description":"Search notes.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}}"#;
const SKILL: &str = r#"{"type":"function","function":{"name":"skill","description":"Load a skill by name.","parameters":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}}}"#;
const MCP: &str = r#"{"type":"function","function":{"name":"mcp","description":"Call an MCP server.","parameters":{"type":"object","properties":{"server":{"type":"string"},"method":{"type":"string"},"args":{"type":"object"}},"required":["method"]}}}"#;
const VIEW: &str = r#"{"type":"function","function":{"name":"view","description":"Load image, video stills, or audio.","parameters":{"type":"object","properties":{"path":{"type":"string"},"kind":{"type":"string","enum":["image","audio","video"]}},"required":["path"]}}}"#;
const SEARCH: &str = r#"{"type":"function","function":{"name":"search","description":"Find code.","parameters":{"type":"object","properties":{"query":{"type":"string"},"path":{"type":"string"}},"required":["query"]}}}"#;
const WEB: &str = r#"{"type":"function","function":{"name":"web","description":"Web search (query) or fetch a page (url).","parameters":{"type":"object","properties":{"query":{"type":"string"},"url":{"type":"string"}}}}}"#;
const ASK: &str = r#"{"type":"function","function":{"name":"ask","description":"Ask the user one multiple-choice question (2-4 options). First option is recommended. Blocks until they pick, skip, or type Other.","parameters":{"type":"object","properties":{"title":{"type":"string"},"prompt":{"type":"string"},"options":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"label":{"type":"string"}},"required":["label"]}}},"required":["prompt","options"]}}}"#;

fn parse(s: &'static str) -> Value {
    serde_json::from_str(s).expect("frozen tool JSON")
}

/// Map a model-facing or legacy name onto the internal dispatch key.
pub fn dispatch_name(name: &str) -> &str {
    match name {
        "Read" | "read" | "read_file" => "read",
        "Write" | "write" | "write_file" => "write",
        "StrReplace" | "edit" | "search_replace" | "MultiEdit" | "Edit" => "edit",
        "Delete" | "delete" => "delete",
        "Glob" | "glob" | "list_dir" => "glob",
        "Grep" | "grep" => "grep",
        "search" => "search",
        "Shell" | "bash" | "run_terminal_command" | "Bash" => "bash",
        "WebSearch" | "WebFetch" | "web" | "web_search" | "web_fetch" => "web",
        "AskQuestion" | "ask" | "ask_user_question" => "ask",
        "TodoWrite" | "todowrite" | "todo_write" => "todowrite",
        "Task" | "task" | "spawn_subagent" => "task",
        "AwaitShell"
        | "awaitshell"
        | "get_command_or_subagent_output"
        | "wait_commands_or_subagents" => "awaitshell",
        other => other,
    }
}

pub fn is_parallel_safe(name: &str) -> bool {
    matches!(
        dispatch_name(name),
        "read" | "view" | "grep" | "glob" | "search" | "web" | "task"
    )
}

pub fn agent_tools() -> Vec<Value> {
    vec![
        parse(READ),
        parse(WRITE),
        parse(STR_REPLACE),
        parse(DELETE),
        parse(GLOB),
        parse(GREP),
        parse(SHELL),
        parse(WEB_SEARCH),
        parse(WEB_FETCH),
        parse(TODO_WRITE),
        parse(ASK_QUESTION),
        parse(TASK),
        parse(AWAIT_SHELL),
    ]
}

pub fn agent_tool_names() -> [&'static str; 13] {
    [
        "Read",
        "Write",
        "StrReplace",
        "Delete",
        "Glob",
        "Grep",
        "Shell",
        "WebSearch",
        "WebFetch",
        "TodoWrite",
        "AskQuestion",
        "Task",
        "AwaitShell",
    ]
}

pub fn code_tools() -> Vec<Value> {
    vec![parse(RUN_CODE), parse(READ), parse(SHELL)]
}

pub fn code_tool_names() -> [&'static str; 3] {
    ["run_code", "Read", "Shell"]
}

/// Separate blob. Do not splice into [`agent_tools`] — append after compact.
pub fn recall_tool() -> Value {
    parse(RECALL)
}

pub fn memory_search_tool() -> Value {
    parse(MEMORY_SEARCH)
}

pub fn skill_tool() -> Value {
    parse(SKILL)
}

pub fn mcp_tool() -> Value {
    parse(MCP)
}

pub fn view_tool() -> Value {
    parse(VIEW)
}

/// Legacy code-index tool. Not in the frozen Cursor set; executor still accepts `search`.
pub fn search_tool() -> Value {
    parse(SEARCH)
}

/// Legacy combined web tool. Frozen Cursor set uses WebSearch / WebFetch instead.
pub fn web_tool() -> Value {
    parse(WEB)
}

/// xAI / grok-cli server-side tools. Not function schemas — the host runs
/// them and returns results. Responses wire only; never send on Chat Completions.
pub fn xai_server_search_tools() -> Vec<Value> {
    vec![
        serde_json::json!({"type": "web_search"}),
        serde_json::json!({"type": "x_search"}),
        serde_json::json!({"type": "image_generation"}),
    ]
}

/// Client `WebSearch` / legacy `web` duplicate the host `web_search` tool.
/// `WebFetch` stays: xAI has no server `web_fetch`.
pub fn is_client_web_search_name(name: &str) -> bool {
    matches!(name, "WebSearch" | "web_search" | "web")
}

/// Legacy ask blob. Frozen Cursor set already includes AskQuestion.
pub fn ask_tool() -> Value {
    parse(ASK)
}

/// AskQuestion is frozen in [`agent_tools`]. `/plan` and `/clarify` only arm the hub.
pub fn sync_ask_tool(_tools: &mut Vec<Value>, _armed: bool) -> bool {
    false
}

pub fn has_tool(tools: &[Value], name: &str) -> bool {
    let want = dispatch_name(name);
    tools.iter().any(|t| {
        t["function"]["name"]
            .as_str()
            .map(|n| dispatch_name(n) == want)
            .unwrap_or(false)
    })
}

pub fn has_recall(tools: &[Value]) -> bool {
    has_tool(tools, "recall")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_order_and_names() {
        let tools = agent_tools();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, agent_tool_names());
        assert_eq!(serde_json::to_string(&tools[0]).unwrap(), READ);
        assert!(!names.contains(&"read"));
        assert!(!names.contains(&"bash"));
        assert!(!names.contains(&"spawn_subagent"));
        let read_d = tools[0]["function"]["description"].as_str().unwrap();
        assert!(read_d.len() > 80, "{read_d}");
        assert!(read_d.contains("offset"));
        assert!(!read_d.contains("office"), "{read_d}");
        assert!(!read_d.contains("chunk"), "{read_d}");
        let grep_d = tools[5]["function"]["description"].as_str().unwrap();
        assert!(!grep_d.contains("office"), "{grep_d}");
        assert!(!grep_d.contains("chunk"), "{grep_d}");
        let task_d = tools[11]["function"]["description"].as_str().unwrap();
        assert!(task_d.contains("explore"));
        assert!(task_d.contains("effort"));
        assert!(task_d.contains("share the parent cwd"), "{task_d}");
        assert!(task_d.contains("keeps that directory"), "{task_d}");
        let web_search = &tools[7]["function"]["parameters"];
        assert!(web_search["required"].is_null(), "{web_search}");
        assert!(web_search["properties"]["query"].is_object());
        assert!(web_search["properties"]["search_term"].is_object());
        assert_eq!(
            tools[6]["function"]["parameters"]["properties"]["background"]["type"].as_str(),
            Some("boolean")
        );
    }

    #[test]
    fn dispatch_name_cli_aliases() {
        assert_eq!(dispatch_name("read_file"), "read");
        assert_eq!(dispatch_name("write_file"), "write");
        assert_eq!(dispatch_name("search_replace"), "edit");
        assert_eq!(dispatch_name("list_dir"), "glob");
        assert_eq!(dispatch_name("run_terminal_command"), "bash");
        assert_eq!(dispatch_name("wait_commands_or_subagents"), "awaitshell");
    }

    #[test]
    fn aliases_share_dispatch_keys() {
        assert_eq!(dispatch_name("Read"), "read");
        assert_eq!(dispatch_name("read"), "read");
        assert_eq!(dispatch_name("StrReplace"), "edit");
        assert_eq!(dispatch_name("Shell"), "bash");
        assert_eq!(dispatch_name("AskQuestion"), "ask");
        assert_eq!(dispatch_name("WebSearch"), "web");
        assert_eq!(dispatch_name("WebFetch"), "web");
        assert_eq!(dispatch_name("Task"), "task");
        assert_eq!(dispatch_name("read_file"), "read");
        assert_eq!(dispatch_name("search_replace"), "edit");
        assert_eq!(dispatch_name("MultiEdit"), "edit");
        assert_eq!(dispatch_name("Edit"), "edit");
        assert_eq!(dispatch_name("write_file"), "write");
        assert_eq!(dispatch_name("list_dir"), "glob");
        assert_eq!(dispatch_name("run_terminal_command"), "bash");
        assert_eq!(dispatch_name("Bash"), "bash");
        assert_eq!(dispatch_name("web_search"), "web");
        assert_eq!(dispatch_name("web_fetch"), "web");
        assert_eq!(dispatch_name("todo_write"), "todowrite");
        assert_eq!(dispatch_name("ask_user_question"), "ask");
        assert_eq!(dispatch_name("wait_commands_or_subagents"), "awaitshell");
        assert!(is_parallel_safe("Read"));
        assert!(is_parallel_safe("Grep"));
        assert!(is_parallel_safe("WebSearch"));
        assert!(is_parallel_safe("Task"));
        assert!(is_parallel_safe("task"));
        assert!(!is_parallel_safe("Write"));
        assert!(!is_parallel_safe("Shell"));
        assert!(!is_parallel_safe("AskQuestion"));
        assert!(!is_parallel_safe("ask"));
    }

    #[test]
    fn xai_server_search_is_host_type_not_function() {
        let tools = xai_server_search_tools();
        assert_eq!(tools[0]["type"], "web_search");
        assert_eq!(tools[1]["type"], "x_search");
        assert_eq!(tools[2]["type"], "image_generation");
        assert!(tools[0].get("function").is_none());
        assert!(is_client_web_search_name("WebSearch"));
        assert!(is_client_web_search_name("web"));
        assert!(!is_client_web_search_name("WebFetch"));
    }

    #[test]
    fn code_tools_order_and_frozen() {
        let tools = code_tools();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, code_tool_names());
        assert_eq!(names.join(", "), "run_code, Read, Shell");
        assert_eq!(serde_json::to_string(&code_tools()[0]).unwrap(), RUN_CODE);
    }

    #[test]
    fn extras_are_not_in_frozen_agent_tools() {
        let tools = agent_tools();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"recall"));
        assert_eq!(serde_json::to_string(&recall_tool()).unwrap(), RECALL);
        assert!(!has_recall(&tools));
        assert!(has_recall(&[recall_tool()]));
        assert_eq!(
            serde_json::to_string(&memory_search_tool()).unwrap(),
            MEMORY_SEARCH
        );
        assert_eq!(serde_json::to_string(&skill_tool()).unwrap(), SKILL);
        assert_eq!(serde_json::to_string(&mcp_tool()).unwrap(), MCP);
        assert_eq!(serde_json::to_string(&view_tool()).unwrap(), VIEW);
        assert_eq!(serde_json::to_string(&search_tool()).unwrap(), SEARCH);
        assert_eq!(serde_json::to_string(&web_tool()).unwrap(), WEB);
        assert_eq!(serde_json::to_string(&ask_tool()).unwrap(), ASK);
        assert!(has_tool(&tools, "AskQuestion"));
        assert!(has_tool(&tools, "ask"));
        assert!(has_tool(&tools, "web"));
        assert!(has_tool(&tools, "Grep"));
        assert!(!has_tool(&tools, "memory_search"));
        assert!(!has_tool(&tools, "skill"));
        assert!(!has_tool(&tools, "mcp"));
        assert!(!has_tool(&tools, "view"));
        assert!(!has_tool(&tools, "search"));
        let mut extra = tools.clone();
        assert!(!sync_ask_tool(&mut extra, true));
        assert!(has_tool(&extra, "AskQuestion"));
        assert!(!sync_ask_tool(&mut extra, false));
        assert!(has_tool(&extra, "AskQuestion"));
    }
}
