//! `TodoWrite`. Persists a short list under `.grok-hyper/todos.json`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{arg_str, Workspace};
use crate::tool_calls::{ToolCall, ToolResponse, ToolState};

const STATUSES: &[&str] = &["pending", "in_progress", "completed", "cancelled"];

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Todo {
    id: String,
    content: String,
    status: String,
}

pub fn todo_write(ws: &Workspace, call: &ToolCall) -> ToolResponse {
    let Some(items) = call.arguments.get("todos").and_then(|v| v.as_array()) else {
        return ToolResponse::text(
            &call.id,
            "Error: TodoWrite needs a `todos` array.",
            ToolState::Error,
        );
    };
    let merge = call
        .arguments
        .get("merge")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut incoming = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let content = arg_str(item, "content").unwrap_or_default();
        if content.trim().is_empty() {
            return ToolResponse::text(
                &call.id,
                format!("Error: todos[{i}] needs `content`."),
                ToolState::Error,
            );
        }
        let status = arg_str(item, "status").unwrap_or_else(|| "pending".into());
        if !STATUSES.contains(&status.as_str()) {
            return ToolResponse::text(
                &call.id,
                format!("Error: todos[{i}] has unknown status `{status}`."),
                ToolState::Error,
            );
        }
        let id = arg_str(item, "id").unwrap_or_else(|| format!("{}", i + 1));
        incoming.push(Todo {
            id,
            content,
            status,
        });
    }
    let path = todo_path(ws);
    let mut list = if merge { load_todos(&path) } else { Vec::new() };
    for todo in incoming {
        if let Some(existing) = list.iter_mut().find(|t| t.id == todo.id) {
            *existing = todo;
        } else {
            list.push(todo);
        }
    }
    if let Err(e) = save_todos(&path, &list) {
        return ToolResponse::text(
            &call.id,
            format!("Error: could not save todos: {e}"),
            ToolState::Error,
        );
    }
    let body = render(&list);
    ToolResponse::text(&call.id, body, ToolState::Success)
}

fn todo_path(ws: &Workspace) -> std::path::PathBuf {
    ws.root().join(".grok-hyper").join("todos.json")
}

fn load_todos(path: &std::path::Path) -> Vec<Todo> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_todos(path: &std::path::Path, list: &[Todo]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let v = Value::Array(
        list.iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "content": t.content,
                    "status": t.status,
                })
            })
            .collect(),
    );
    std::fs::write(path, serde_json::to_vec_pretty(&v)?)
}

fn render(list: &[Todo]) -> String {
    if list.is_empty() {
        return "Todos: (empty)".into();
    }
    let mut s = String::from("Todos:\n");
    for t in list {
        s.push_str(&format!("- [{}] {} ({})\n", t.status, t.content, t.id));
    }
    s
}
