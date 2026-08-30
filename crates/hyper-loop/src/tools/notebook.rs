//! Jupyter `.ipynb` edits. Cursor `EditNotebook` shape: cell index + source
//! strings, never the notebook JSON wrapper.

use serde_json::{json, Value};

use super::{arg_str, arg_str_any, arg_u32, fs, Workspace};
use crate::tool_calls::{ToolCall, ToolResponse, ToolState};

pub fn edit_notebook(ws: &Workspace, call: &ToolCall) -> ToolResponse {
    let Some(raw) = arg_str_any(&call.arguments, &["target_notebook", "path"]) else {
        return ToolResponse::text(
            &call.id,
            "Error: EditNotebook needs `target_notebook`.",
            ToolState::Error,
        );
    };
    if !raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&raw)
        .ends_with(".ipynb")
    {
        return ToolResponse::text(
            &call.id,
            format!("Error: {raw} is not a .ipynb notebook."),
            ToolState::Error,
        );
    }
    let path = match ws.resolve(&raw) {
        Ok(p) => p,
        Err(e) => return ToolResponse::text(&call.id, e, ToolState::Error),
    };
    let new_cell = arg_bool(&call.arguments, "is_new_cell").unwrap_or(false);
    let Some(idx) = arg_u32(&call.arguments, "cell_idx").map(|n| n as usize) else {
        return ToolResponse::text(
            &call.id,
            "Error: EditNotebook needs `cell_idx`.",
            ToolState::Error,
        );
    };
    let language = arg_str(&call.arguments, "cell_language").unwrap_or_else(|| "python".into());
    let old = arg_str(&call.arguments, "old_string").unwrap_or_default();
    let new = arg_str(&call.arguments, "new_string").unwrap_or_default();

    let mut nb = if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str::<Value>(&s) {
                Ok(v) => v,
                Err(e) => {
                    return ToolResponse::text(
                        &call.id,
                        format!("Error: notebook JSON: {e}"),
                        ToolState::Error,
                    );
                }
            },
            Err(e) => {
                return ToolResponse::text(&call.id, format!("Error: {e}"), ToolState::Error);
            }
        }
    } else if new_cell {
        empty_notebook(&language)
    } else {
        return ToolResponse::text(
            &call.id,
            format!("Error: The file {} does not exist.", ws.shown(&raw)),
            ToolState::Error,
        );
    };

    let Some(cells) = nb.get_mut("cells").and_then(Value::as_array_mut) else {
        return ToolResponse::text(
            &call.id,
            "Error: notebook is missing a cells array.",
            ToolState::Error,
        );
    };
    if new_cell {
        if idx > cells.len() {
            return ToolResponse::text(
                &call.id,
                format!(
                    "Error: cell_idx {idx} is past the end ({} cells).",
                    cells.len()
                ),
                ToolState::Error,
            );
        }
        cells.insert(idx, new_cell_value(&language, &new));
    } else {
        if idx >= cells.len() {
            return ToolResponse::text(
                &call.id,
                format!(
                    "Error: cell_idx {idx} is past the end ({} cells).",
                    cells.len()
                ),
                ToolState::Error,
            );
        }
        let cell = &mut cells[idx];
        let current = cell_source(cell);
        if old.is_empty() {
            return ToolResponse::text(
                &call.id,
                "Error: `old_string` must be non-empty unless is_new_cell is true.",
                ToolState::Error,
            );
        }
        let matches = current.matches(&old).count();
        if matches == 0 {
            return ToolResponse::text(
                &call.id,
                "Error: old_string not found in that cell.",
                ToolState::Error,
            );
        }
        if matches > 1 {
            return ToolResponse::text(
                &call.id,
                "Error: old_string matches more than once in that cell; widen it.",
                ToolState::Error,
            );
        }
        let updated = current.replacen(&old, &new, 1);
        set_cell_source(cell, &updated);
        if let Some(lang) = cell.get_mut("metadata") {
            if lang.is_object() {
                lang["language_id"] = json!(language);
            }
        }
    }

    let pretty = serde_json::to_string_pretty(&nb).unwrap_or_else(|_| nb.to_string());
    match fs::write_atomic(&path, &pretty) {
        Ok(()) => ToolResponse::text(
            &call.id,
            format!(
                "{} cell {idx} in {}.",
                if new_cell { "Inserted" } else { "Updated" },
                ws.shown(&raw)
            ),
            ToolState::Success,
        ),
        Err(e) => ToolResponse::text(&call.id, format!("Error: {e}"), ToolState::Error),
    }
}

fn arg_bool(args: &Value, key: &str) -> Option<bool> {
    match args.get(key) {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn empty_notebook(language: &str) -> Value {
    json!({
        "nbformat": 4,
        "nbformat_minor": 5,
        "metadata": {
            "kernelspec": {
                "display_name": language,
                "language": jupyter_kernel_language(language),
                "name": jupyter_kernel_language(language)
            },
            "language_info": { "name": jupyter_kernel_language(language) }
        },
        "cells": []
    })
}

fn new_cell_value(language: &str, source: &str) -> Value {
    let cell_type = jupyter_cell_type(language);
    let mut cell = json!({
        "cell_type": cell_type,
        "metadata": { "language_id": language },
        "source": source_lines(source),
    });
    if cell_type == "code" {
        cell["outputs"] = json!([]);
        cell["execution_count"] = Value::Null;
    }
    cell
}

fn jupyter_cell_type(language: &str) -> &'static str {
    match language.trim().to_ascii_lowercase().as_str() {
        "markdown" | "md" => "markdown",
        "raw" | "other" => "raw",
        _ => "code",
    }
}

fn jupyter_kernel_language(language: &str) -> &'static str {
    match language.trim().to_ascii_lowercase().as_str() {
        "python" | "py" => "python",
        "javascript" | "js" | "typescript" | "ts" => "javascript",
        "r" => "r",
        "sql" => "python",
        "shell" | "bash" => "python",
        _ => "python",
    }
}

fn cell_source(cell: &Value) -> String {
    match cell.get("source") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn set_cell_source(cell: &mut Value, source: &str) {
    cell["source"] = json!(source_lines(source));
}

fn source_lines(source: &str) -> Vec<String> {
    if source.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = source.split_inclusive('\n').map(str::to_string).collect();
    if source.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::arg_str;
    use serde_json::json;
    use std::path::PathBuf;

    fn scratch() -> (Workspace, PathBuf) {
        let dir = std::env::temp_dir().join(format!("hyper-nb-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        (Workspace::open(&dir, true).unwrap(), dir)
    }

    fn call(args: Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: "EditNotebook".into(),
            arguments: args,
        }
    }

    #[test]
    fn inserts_then_replaces_a_code_cell() {
        let (ws, dir) = scratch();
        let r = edit_notebook(
            &ws,
            &call(json!({
                "target_notebook": "n.ipynb",
                "cell_idx": 0,
                "is_new_cell": true,
                "cell_language": "python",
                "old_string": "",
                "new_string": "print(1)"
            })),
        );
        assert_eq!(r.state, ToolState::Success, "{}", r.joined_text());
        let r = edit_notebook(
            &ws,
            &call(json!({
                "path": "n.ipynb",
                "cell_idx": 0,
                "is_new_cell": false,
                "cell_language": "python",
                "old_string": "print(1)",
                "new_string": "print(2)"
            })),
        );
        assert_eq!(r.state, ToolState::Success, "{}", r.joined_text());
        let raw = std::fs::read_to_string(dir.join("n.ipynb")).unwrap();
        let nb: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(cell_source(&nb["cells"][0]), "print(2)");
        assert_eq!(nb["cells"][0]["cell_type"], "code");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_non_notebook_paths() {
        let (ws, dir) = scratch();
        let r = edit_notebook(
            &ws,
            &call(json!({
                "target_notebook": "n.py",
                "cell_idx": 0,
                "is_new_cell": true,
                "cell_language": "python",
                "old_string": "",
                "new_string": "x"
            })),
        );
        assert_eq!(r.state, ToolState::Error);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn arg_str_any_prefers_first_key() {
        let v = json!({"target_notebook": "a.ipynb", "path": "b.ipynb"});
        assert_eq!(
            arg_str_any(&v, &["target_notebook", "path"]).as_deref(),
            Some("a.ipynb")
        );
        assert_eq!(arg_str(&v, "path").as_deref(), Some("b.ipynb"));
    }
}
