//! Frozen OpenAI tool JSON. Byte-stable: parsed with `preserve_order` and never rebuilt by hash maps.
//!
//! Model-facing names are Cursor's (`Read`, `Shell`, `AskQuestion`, …).
//! Executors still accept a few aliases for old transcripts.

use serde_json::Value;

/// Dummy system blob for template tests (not injected by the agent loop).
pub const HARNESS_SYSTEM: &str = "Workspace assistant. Paths are relative.";

const READ: &str = r#"{"type":"function","function":{"name":"Read","description":"Read a file. Path is workspace-relative unless absolute. For text, page with offset (1-based line) and limit. Prefer this over Shell cat. Parallel-safe with Glob and Grep.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative or absolute path."},"offset":{"type":"integer","description":"1-based start line. Omit to read from the start."},"limit":{"type":"integer","description":"Max lines."}},"required":["path"]}}}"#;
const WRITE: &str = r#"{"type":"function","function":{"name":"Write","description":"Create or overwrite a file. Pass contents. Read first if it exists. No placeholder ellipses. Not parallel-safe with other writes to the same path.","parameters":{"type":"object","properties":{"path":{"type":"string"},"contents":{"type":"string","description":"Full file body."}},"required":["path"]}}}"#;
const STR_REPLACE: &str = r#"{"type":"function","function":{"name":"StrReplace","description":"Replace one unique old_string with new_string. Widen old_string until it matches once, unless replace_all. Not parallel-safe with Write/Delete on the same path.","parameters":{"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string","description":"Exact text to find. Must be unique unless replace_all."},"new_string":{"type":"string","description":"Replacement text."},"replace_all":{"type":"boolean","description":"Replace every match. Default false."}},"required":["path","old_string","new_string"]}}}"#;
const DELETE: &str = r#"{"type":"function","function":{"name":"Delete","description":"Delete a file. Prefer StrReplace or Write to change contents. Not parallel-safe with other mutations on the same path.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}"#;
const GLOB: &str = r#"{"type":"function","function":{"name":"Glob","description":"Find paths matching glob_pattern (for example **/*.md). Optional target_directory. Returns paths, not contents. Parallel-safe with Read and Grep.","parameters":{"type":"object","properties":{"glob_pattern":{"type":"string"},"target_directory":{"type":"string","description":"Directory to search under. Default is the workspace root."}},"required":["glob_pattern"]}}}"#;
const GREP: &str = r#"{"type":"function","function":{"name":"Grep","description":"ripgrep over the workspace. Prefer this over Shell rg. Optional path, glob, type, head_limit, -i, -A/-B/-C, output_mode (content | files_with_matches | count), multiline, offset. Parallel-safe with Read, Glob, and WebSearch.","parameters":{"type":"object","properties":{"pattern":{"type":"string","description":"Regex pattern."},"path":{"type":"string","description":"File or directory. Default is the workspace root."},"glob":{"type":"string","description":"Glob to filter files, e.g. *.rs."},"output_mode":{"type":"string","description":"content (default), files_with_matches, or count."},"-B":{"type":"integer","description":"Lines before each match."},"-A":{"type":"integer","description":"Lines after each match."},"-C":{"type":"integer","description":"Lines before and after."},"-i":{"type":"boolean"},"type":{"type":"string","description":"ripgrep --type, e.g. rust."},"head_limit":{"type":"integer"},"multiline":{"type":"boolean"},"offset":{"type":"integer","description":"Skip first N matches."}},"required":["pattern"]}}}"#;
const READ_LINTS: &str = r#"{"type":"function","function":{"name":"ReadLints","description":"Read current compiler or linter errors for code files. Use after edits when diagnostics are needed explicitly; successful Write/StrReplace calls also attach diagnostics automatically.","parameters":{"type":"object","properties":{"paths":{"type":"array","items":{"type":"string"},"description":"Workspace-relative code files to check."},"path":{"type":"string","description":"Single-file compatibility alias."}}}}}"#;
const EDIT_NOTEBOOK: &str = r#"{"type":"function","function":{"name":"EditNotebook","description":"Edit a Jupyter notebook (.ipynb). cell_idx is 0-based. is_new_cell true inserts a cell at that index. old_string and new_string are cell source, not notebook JSON. Prefer this over Write for .ipynb.","parameters":{"type":"object","properties":{"target_notebook":{"type":"string"},"path":{"type":"string","description":"Alias for target_notebook."},"cell_idx":{"type":"integer"},"is_new_cell":{"type":"boolean"},"cell_language":{"type":"string","description":"python, markdown, javascript, typescript, r, sql, shell, raw, or other."},"old_string":{"type":"string"},"new_string":{"type":"string"}},"required":["cell_idx","is_new_cell","cell_language","old_string","new_string"]}}}"#;
const SHELL: &str = r#"{"type":"function","function":{"name":"Shell","description":"Run a command in the workspace. Optional working_directory, block_until_ms, background. Prefer Read/Grep/Glob over cat/ls/rg. Do not parallel Shell calls that touch the same path.","parameters":{"type":"object","properties":{"command":{"type":"string"},"working_directory":{"type":"string"},"block_until_ms":{"type":"integer","description":"How long to wait before backgrounding. Default is the tool timeout."},"background":{"type":"boolean","description":"Return immediately with this call id; wait with AwaitShell."}},"required":["command"]}}}"#;
const WEB_SEARCH: &str = r#"{"type":"function","function":{"name":"WebSearch","description":"Search the public web. Not for files in this workspace. Parallel-safe with Read, Glob, Grep, and WebFetch.","parameters":{"type":"object","properties":{"search_term":{"type":"string"}},"required":["search_term"]}}}"#;
const WEB_FETCH: &str = r#"{"type":"function","function":{"name":"WebFetch","description":"Fetch a URL and extract readable text. Parallel-safe with WebSearch and Read.","parameters":{"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}}}"#;
const GENERATE_IMAGE: &str = r#"{"type":"function","function":{"name":"GenerateImage","description":"Generate an image from a text description and save it in the workspace. Prefer this over inventing image bytes.","parameters":{"type":"object","properties":{"description":{"type":"string"},"prompt":{"type":"string","description":"Alias for description."},"filename":{"type":"string","description":"Optional workspace-relative png/jpg name."}}}}}"#;
const TODO_WRITE: &str = r#"{"type":"function","function":{"name":"TodoWrite","description":"Short todo list for multi-step work. Each item needs content and status (pending, in_progress, completed, cancelled). Optional id; merge true updates by id.","parameters":{"type":"object","properties":{"todos":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"content":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed","cancelled"]}},"required":["content","status"]}},"merge":{"type":"boolean","description":"If true, merge by id instead of replacing the list."}},"required":["todos"]}}}"#;
const ASK_QUESTION: &str = r#"{"type":"function","function":{"name":"AskQuestion","description":"Ask one multiple-choice question when a choice would change the work. Do not ask in prose. Blocks until the user answers.","parameters":{"type":"object","properties":{"title":{"type":"string"},"questions":{"type":"array"},"prompt":{"type":"string","description":"Legacy flat prompt when questions is omitted."},"options":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"label":{"type":"string"}},"required":["label"]},"description":"Legacy flat options when questions is omitted."}},"required":["questions"]}}}"#;
const SWITCH_MODE: &str = r#"{"type":"function","function":{"name":"SwitchMode","description":"Switch how the current agent run operates. plan inspects and writes only plan.md; agent performs the work; ask gathers a blocking user choice before proceeding. Use this tool instead of asking the user to type a slash command.","parameters":{"type":"object","properties":{"mode":{"type":"string","enum":["agent","plan","ask"]},"target_mode_id":{"type":"string","enum":["agent","plan","ask"],"description":"Cursor alias for mode."}}}}}"#;
const TASK: &str = r#"{"type":"function","function":{"name":"Task","description":"Launch a scoped subagent (depth 1). Required: description (3-5 word UI title) and prompt. subagent_type: explore (read-only, low effort), plan (read-only except plan.md), generalPurpose, office. Omit it or pass inherit to use generalPurpose. Cursor aliases shell, cursor-guide, ci-investigator, bugbot, security-review are accepted. model inherit/fast/composer-* use the parent model. Multiple Task calls in one turn run in parallel. run_in_background true returns the child id immediately; wait with AwaitShell. resume is a previous Task id. isolation none/auto share the parent cwd; worktree is a git checkout that keeps that directory after exit. Do not use for a single Read.","parameters":{"type":"object","properties":{"description":{"type":"string","description":"3-5 word title shown on the Task card."},"prompt":{"type":"string","description":"The subagent's task."},"subagent_type":{"type":"string","description":"explore | plan | generalPurpose | office. inherit and unknown names map to generalPurpose."},"model":{"type":"string","description":"Optional model id. inherit, fast, and composer-* keep the parent model. Does not change effort routing."},"run_in_background":{"type":"boolean","description":"Return immediately with the child id."},"background":{"type":"boolean","description":"Legacy alias for run_in_background."},"resume":{"type":"string","description":"Previous Task id to continue. self starts a new child."},"isolation":{"type":"string","enum":["none","worktree","auto"],"description":"none and auto share the parent cwd. worktree is an explicit git worktree kept after the child finishes."}},"required":["description","prompt"]}}}"#;
const AWAIT_SHELL: &str = r#"{"type":"function","function":{"name":"AwaitShell","description":"Wait on a background Shell or Task. Pass task_id or shell_id and optional block_until_ms.","parameters":{"type":"object","properties":{"task_id":{"type":"string"},"shell_id":{"type":"string"},"block_until_ms":{"type":"integer"}}}}}"#;
const RUN_CODE: &str = r#"{"type":"function","function":{"name":"run_code","description":"Run Python.","parameters":{"type":"object","properties":{"code":{"type":"string"}},"required":["code"]}}}"#;
const RECALL: &str = r#"{"type":"function","function":{"name":"recall","description":"Search the compacted session archive. query is keyword search. seq expands one JSONL event including screenshot paths on disk. blob is the full SHA-256 of an offloaded tool dump, not a 12-character prefix.","parameters":{"type":"object","properties":{"query":{"type":"string","description":"Keyword search over the archive index."},"seq":{"type":"integer","description":"JSONL event index to expand."},"blob":{"type":"string","description":"Full SHA-256 of an offloaded tool dump."}}}}}"#;
const MEMORY_SEARCH: &str = r#"{"type":"function","function":{"name":"memory_search","description":"Search notes.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}}"#;
const SKILL: &str = r#"{"type":"function","function":{"name":"skill","description":"Load a skill by name.","parameters":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}}}"#;
const MCP: &str = r#"{"type":"function","function":{"name":"mcp","description":"Call an MCP server.","parameters":{"type":"object","properties":{"server":{"type":"string"},"method":{"type":"string"},"args":{"type":"object"}},"required":["method"]}}}"#;
const GET_DYNAMIC_TOOLS: &str = r#"{"type":"function","function":{"name":"GetDynamicTools","description":"Discover the live tools exposed by configured MCP servers. Call this before using an unfamiliar server or tool. Omit server to inspect every mounted server.","parameters":{"type":"object","properties":{"server":{"type":"string","description":"Optional configured MCP server name."},"namespace":{"type":"string","description":"Cursor alias for server."},"query":{"type":"string","description":"Optional case-insensitive filter over tool names and descriptions."},"pattern":{"type":"string","description":"Cursor alias for query."},"toolName":{"type":"string","description":"Optional exact tool name filter."}}}}}"#;
const CALL_DYNAMIC_TOOL: &str = r#"{"type":"function","function":{"name":"CallDynamicTool","description":"Call one MCP tool discovered with GetDynamicTools. The MCP tool is a first-class dynamic tool; pass its server, exact name, and arguments.","parameters":{"type":"object","properties":{"server":{"type":"string"},"namespace":{"type":"string","description":"Cursor alias for server."},"name":{"type":"string"},"toolName":{"type":"string","description":"Cursor alias for name."},"arguments":{"type":"object"}}}}}"#;
const FETCH_MCP_RESOURCE: &str = r#"{"type":"function","function":{"name":"FetchMcpResource","description":"Read a resource from a configured MCP server. Omit uri to list resources. Optional downloadPath writes the bytes into the workspace.","parameters":{"type":"object","properties":{"server":{"type":"string"},"namespace":{"type":"string","description":"Cursor alias for server."},"uri":{"type":"string"},"downloadPath":{"type":"string"}}}}}"#;
const VIEW: &str = r#"{"type":"function","function":{"name":"view","description":"Load image, video stills, or audio.","parameters":{"type":"object","properties":{"path":{"type":"string"},"kind":{"type":"string","enum":["image","audio","video"]}},"required":["path"]}}}"#;
const SEARCH: &str = r#"{"type":"function","function":{"name":"Search","description":"Find code in this workspace. Prefer this over Grep for a symbol or when you do not have a regex. Exact identifiers return definition spans first. Grep is exact regex; Glob is paths; Read is file contents. Parallel-safe with Read, Glob, and Grep.","parameters":{"type":"object","properties":{"query":{"type":"string"},"path":{"type":"string"}},"required":["query"]}}}"#;
const WEB: &str = r#"{"type":"function","function":{"name":"web","description":"Web search (query) or fetch a page (url).","parameters":{"type":"object","properties":{"query":{"type":"string"},"url":{"type":"string"}}}}}"#;
const ASK: &str = r#"{"type":"function","function":{"name":"ask","description":"Ask the user one multiple-choice question (2-4 options). First option is recommended. Blocks until they pick, skip, or type Other.","parameters":{"type":"object","properties":{"title":{"type":"string"},"prompt":{"type":"string"},"options":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"label":{"type":"string"}},"required":["label"]}}},"required":["prompt","options"]}}}"#;
const COMPUTER_USE: &str = r#"{"type":"function","function":{"name":"ComputerUse","description":"See and control this Windows or macOS desktop. Coordinates are in the last screenshot image, origin top-left. Screenshot first, then click or type. macOS: Screen Recording + Accessibility. Windows: interactive desktop. action: screenshot | list_displays | click | double_click | right_click | move | drag | scroll | type | key | wait. key: enter, tab, cmd+c, ctrl+v, mod+c (cmd on Mac, ctrl on Windows). Not parallel-safe.","parameters":{"type":"object","properties":{"action":{"type":"string","description":"screenshot | list_displays | click | double_click | right_click | move | drag | scroll | type | key | wait"},"x":{"type":"number","description":"Image-space X from the last screenshot."},"y":{"type":"number"},"x2":{"type":"number","description":"Drag end X."},"y2":{"type":"number"},"text":{"type":"string","description":"Unicode text for type."},"keys":{"type":"string","description":"Chord for key: enter, cmd+c, ctrl+shift+t, mod+v."},"scroll_y":{"type":"integer","description":"Positive is down."},"display":{"type":"integer","description":"0-based display index. Default primary."},"ms":{"type":"integer","description":"wait milliseconds, max 8000."}},"required":["action"]}}}"#;

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
        "ReadLints" | "read_lints" | "readlints" => "readlints",
        "EditNotebook" | "edit_notebook" | "editnotebook" => "editnotebook",
        "search" | "Search" => "search",
        "Shell" | "bash" | "run_terminal_command" | "Bash" => "bash",
        "WebSearch" | "WebFetch" | "web" | "web_search" | "web_fetch" => "web",
        "GenerateImage" | "generate_image" | "generateimage" => "generateimage",
        "AskQuestion" | "ask" | "ask_user_question" => "ask",
        "SwitchMode" | "switch_mode" | "switchmode" => "switchmode",
        "GetDynamicTools" | "get_dynamic_tools" | "getdynamictools" => "getdynamictools",
        "CallDynamicTool" | "call_dynamic_tool" | "calldynamictool" => "calldynamictool",
        "FetchMcpResource" | "fetch_mcp_resource" | "fetchmcpresource" => "fetchmcpresource",
        "TodoWrite" | "todowrite" | "todo_write" => "todowrite",
        "Task" | "task" | "spawn_subagent" => "task",
        "AwaitShell"
        | "awaitshell"
        | "get_command_or_subagent_output"
        | "wait_commands_or_subagents" => "awaitshell",
        "ComputerUse" | "computer_use" | "computeruse" | "computer" => "computeruse",
        other => other,
    }
}

pub fn is_parallel_safe(name: &str) -> bool {
    matches!(
        dispatch_name(name),
        "read"
            | "view"
            | "grep"
            | "glob"
            | "search"
            | "web"
            | "task"
            | "getdynamictools"
            | "fetchmcpresource"
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
        parse(READ_LINTS),
        parse(EDIT_NOTEBOOK),
        parse(SHELL),
        parse(WEB_SEARCH),
        parse(WEB_FETCH),
        parse(GENERATE_IMAGE),
        parse(TODO_WRITE),
        parse(ASK_QUESTION),
        parse(SWITCH_MODE),
        parse(TASK),
        parse(AWAIT_SHELL),
    ]
}

pub fn agent_tool_names() -> [&'static str; 17] {
    [
        "Read",
        "Write",
        "StrReplace",
        "Delete",
        "Glob",
        "Grep",
        "ReadLints",
        "EditNotebook",
        "Shell",
        "WebSearch",
        "WebFetch",
        "GenerateImage",
        "TodoWrite",
        "AskQuestion",
        "SwitchMode",
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

/// Separate blob. Not in the frozen Cursor set; live tests may still mount it.
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

/// Cursor-shaped MCP surface. Schemas stay stable while the live server
/// catalog is discovered at call time.
pub fn dynamic_mcp_tools() -> [Value; 3] {
    [
        parse(GET_DYNAMIC_TOOLS),
        parse(CALL_DYNAMIC_TOOL),
        parse(FETCH_MCP_RESOURCE),
    ]
}

pub fn view_tool() -> Value {
    parse(VIEW)
}

/// Extra blob. Do not splice into [`agent_tools`].
pub fn computer_use_tool() -> Value {
    parse(COMPUTER_USE)
}

/// Workspace code-index tool. Appended after the core set; executor accepts `Search` / `search`.
pub fn search_tool() -> Value {
    parse(SEARCH)
}

/// Legacy combined web tool. Frozen Cursor set uses WebSearch / WebFetch instead.
pub fn web_tool() -> Value {
    parse(WEB)
}

/// xAI / grok-cli server-side tools. Not function schemas — the host runs
/// them and returns results. Kept for compact/archive parsing of old hops.
/// Responses wire sends the Cursor client set instead (`WebSearch` / `WebFetch`
/// / `GenerateImage`); do not prepend these or grok-4.6 calls both.
pub fn xai_server_search_tools() -> Vec<Value> {
    vec![
        serde_json::json!({"type": "web_search"}),
        serde_json::json!({"type": "x_search"}),
        serde_json::json!({"type": "image_generation"}),
    ]
}

/// Client `WebSearch` / legacy `web`. Distinct from host `web_search`.
/// `WebFetch` is always a client function: xAI has no server `web_fetch`.
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

/// Drop `recall` if a previous Hyper version appended it. Cursor compact
/// continues from the archive card; it does not expose a search-archive tool.
pub fn strip_recall(tools: &mut Vec<Value>) -> bool {
    let before = tools.len();
    tools.retain(|t| {
        t["function"]["name"]
            .as_str()
            .map(|n| dispatch_name(n) != "recall")
            .unwrap_or(true)
    });
    tools.len() != before
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
        let task_d = tools[15]["function"]["description"].as_str().unwrap();
        assert!(task_d.contains("explore"));
        assert!(task_d.contains("effort"));
        assert!(task_d.contains("share the parent cwd"), "{task_d}");
        assert!(task_d.contains("keeps that directory"), "{task_d}");
        let web_search = &tools[9]["function"]["parameters"];
        assert_eq!(web_search["required"], serde_json::json!(["search_term"]));
        assert!(web_search["properties"]["search_term"].is_object());
        assert!(web_search["properties"].get("query").is_none());
        assert_eq!(
            tools[8]["function"]["parameters"]["properties"]["background"]["type"].as_str(),
            Some("boolean")
        );
        assert!(tools[7]["function"]["parameters"]["properties"]
            .get("target_notebook")
            .is_some());
        assert!(tools[11]["function"]["parameters"]["properties"]
            .get("description")
            .is_some());
        assert!(tools[14]["function"]["parameters"]["properties"]
            .get("target_mode_id")
            .is_some());
    }

    #[test]
    fn dispatch_name_cli_aliases() {
        assert_eq!(dispatch_name("read_file"), "read");
        assert_eq!(dispatch_name("write_file"), "write");
        assert_eq!(dispatch_name("search_replace"), "edit");
        assert_eq!(dispatch_name("list_dir"), "glob");
        assert_eq!(dispatch_name("run_terminal_command"), "bash");
        assert_eq!(dispatch_name("wait_commands_or_subagents"), "awaitshell");
        assert_eq!(dispatch_name("ComputerUse"), "computeruse");
        assert_eq!(dispatch_name("computer_use"), "computeruse");
        assert_eq!(dispatch_name("computer"), "computeruse");
        assert_eq!(dispatch_name("EditNotebook"), "editnotebook");
        assert_eq!(dispatch_name("GenerateImage"), "generateimage");
        assert_eq!(dispatch_name("FetchMcpResource"), "fetchmcpresource");
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
        let mut with = vec![recall_tool()];
        assert!(has_recall(&with));
        assert!(strip_recall(&mut with));
        assert!(!has_recall(&with));
        assert!(!strip_recall(&mut with));
        assert_eq!(
            serde_json::to_string(&memory_search_tool()).unwrap(),
            MEMORY_SEARCH
        );
        assert_eq!(serde_json::to_string(&skill_tool()).unwrap(), SKILL);
        assert_eq!(serde_json::to_string(&mcp_tool()).unwrap(), MCP);
        let dynamic = dynamic_mcp_tools();
        assert_eq!(dynamic[0]["function"]["name"], "GetDynamicTools");
        assert_eq!(dynamic[1]["function"]["name"], "CallDynamicTool");
        assert_eq!(dynamic[2]["function"]["name"], "FetchMcpResource");
        assert!(is_parallel_safe("GetDynamicTools"));
        assert!(is_parallel_safe("FetchMcpResource"));
        assert!(!is_parallel_safe("CallDynamicTool"));
        assert_eq!(serde_json::to_string(&view_tool()).unwrap(), VIEW);
        assert_eq!(
            serde_json::to_string(&computer_use_tool()).unwrap(),
            COMPUTER_USE
        );
        assert!(!has_tool(&tools, "ComputerUse"));
        assert_eq!(serde_json::to_string(&search_tool()).unwrap(), SEARCH);
        assert_eq!(search_tool()["function"]["name"].as_str(), Some("Search"));
        assert!(search_tool()["function"]["description"]
            .as_str()
            .unwrap()
            .contains("Prefer this over Grep"));
        assert_eq!(serde_json::to_string(&web_tool()).unwrap(), WEB);
        assert_eq!(serde_json::to_string(&ask_tool()).unwrap(), ASK);
        assert!(has_tool(&tools, "AskQuestion"));
        assert!(has_tool(&tools, "ask"));
        assert!(has_tool(&tools, "EditNotebook"));
        assert!(has_tool(&tools, "GenerateImage"));
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
