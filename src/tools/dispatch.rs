use super::types::{ApiContext, ToolDef, ToolResult};
use std::path::{Path, PathBuf};

pub fn tool_definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read".to_string(),
            description: "Read the contents of a file. Use offset/limit for large files.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read"
                    },
                    "offset": {
                        "type": "number",
                        "description": "Line number to start reading from (1-indexed)"
                    },
                    "limit": {
                        "type": "number",
                        "description": "Maximum number of lines to read"
                    }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "bash".to_string(),
            description: "Execute a bash command. Returns stdout and stderr.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Bash command to execute"
                    },
                    "timeout": {
                        "type": "number",
                        "description": "Timeout in seconds (optional)"
                    },
                    "background": {
                        "type": "boolean",
                        "description": "If true, run the command in the background and return immediately. You will be notified with the output when the command finishes. Useful for long-running commands like builds, tests, or servers."
                    }
                },
                "required": ["command"]
            }),
        },
        ToolDef {
            name: "edit".to_string(),
            description: "Edit a file by replacing exact text. The oldText must match exactly.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to edit"
                    },
                    "oldText": {
                        "type": "string",
                        "description": "Exact text to find and replace"
                    },
                    "newText": {
                        "type": "string",
                        "description": "New text to replace with"
                    }
                },
                "required": ["path", "oldText", "newText"]
            }),
        },
        ToolDef {
            name: "write".to_string(),
            description: "Write content to a file. Creates parent directories automatically.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write"
                    }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDef {
            name: "web_search".to_string(),
            description: "Search the web using DuckDuckGo. Returns titles, URLs, and snippets for the top results.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query"
                    },
                    "limit": {
                        "type": "number",
                        "description": "Max results to return (default 5)"
                    }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "view_file".to_string(),
            description: "View an image file and return its visual contents. Supports JPEG, PNG, GIF, and WebP. Use this to inspect screenshots, diagrams, UI mockups, or any image the user references.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the image file"
                    }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "explore".to_string(),
            description: "Spawn a focused sub-agent that uses read-only tools (read, bash, web_search) to thoroughly investigate a topic, then returns a detailed structured writeup. Use this when a task requires substantial exploration before you can act \u{2014} inspecting an unfamiliar codebase section, tracing how something works across many files, or researching a problem space.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "What to explore, research, or investigate"
                    }
                },
                "required": ["prompt"]
            }),
        },
        ToolDef {
            name: "goto_definition".to_string(),
            description: "Find the definition of a symbol at a specific location in a file using the Language Server Protocol. Returns the file path and line number where the symbol is defined. Requires an LSP server to be running for the file's language.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file containing the symbol"
                    },
                    "line": {
                        "type": "number",
                        "description": "Line number (1-indexed)"
                    },
                    "character": {
                        "type": "number",
                        "description": "Column offset in the line (0-indexed)"
                    }
                },
                "required": ["path", "line", "character"]
            }),
        },
        ToolDef {
            name: "diagnostics".to_string(),
            description: "Get LSP diagnostics (errors, warnings) for a specific file or all files. Returns compiler errors, type errors, and warnings from the language server.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to a specific file to get diagnostics for. If omitted, returns diagnostics for all files."
                    }
                }
            }),
        },
    ]
}

pub(super) fn resolve_path(path: &str, cwd: &Path) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

pub fn execute_tool<'a>(
    name: &'a str,
    input: &'a serde_json::Value,
    cwd: &'a Path,
    stream_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    api_ctx: Option<&'a ApiContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
    Box::pin(async move {
        match name {
            "read" => super::file::exec_read(input, cwd).await,
            "bash" => {
                super::bash::exec_bash(
                    input,
                    cwd,
                    stream_tx,
                    api_ctx,
                )
                .await
            }
            "edit" => super::file::exec_edit(input, cwd).await,
            "write" => super::file::exec_write(input, cwd).await,
            "web_search" => super::search::exec_web_search(input).await,
            "view_file" => super::file::exec_view_file(input, cwd).await,
            "explore" => match api_ctx {
                Some(ctx) => super::explore::exec_explore(input, cwd, stream_tx, ctx).await,
                None => ToolResult::Error("explore is not available in this context".to_string()),
            },
            "goto_definition" => match api_ctx {
                Some(ctx) => super::lsp_tools::exec_goto_definition(input, cwd, ctx).await,
                None => ToolResult::Error("goto_definition requires LSP".to_string()),
            },
            "diagnostics" => match api_ctx {
                Some(ctx) => super::lsp_tools::exec_diagnostics(input, cwd, ctx).await,
                None => ToolResult::Error("diagnostics requires LSP".to_string()),
            },
            _ => ToolResult::Error(format!("unknown tool: {}", name)),
        }
    })
}
