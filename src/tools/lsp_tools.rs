use super::dispatch::resolve_path;
use super::types::{ApiContext, ToolResult};
use std::path::Path;

pub(super) async fn exec_goto_definition(
    input: &serde_json::Value,
    cwd: &Path,
    ctx: &ApiContext,
) -> ToolResult {
    let path_str = match input.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolResult::Error("missing 'path' parameter".to_string()),
    };
    let line = match input.get("line").and_then(|v| v.as_u64()) {
        Some(l) => l as u32,
        None => return ToolResult::Error("missing 'line' parameter".to_string()),
    };
    let character = match input.get("character").and_then(|v| v.as_u64()) {
        Some(c) => c as u32,
        None => return ToolResult::Error("missing 'character' parameter".to_string()),
    };

    let lsp = match &ctx.lsp {
        Some(l) => l,
        None => return ToolResult::Error("no LSP server available".to_string()),
    };

    let path = resolve_path(path_str, cwd);

    // ensure the file is open in the language server
    if let Ok(text) = std::fs::read_to_string(&path) {
        let mgr = lsp.lock().await;
        mgr.notify_open(&path, &text).await;
    }

    let mgr = lsp.lock().await;
    // line is 1-indexed from the tool, LSP uses 0-indexed
    let lsp_line = line.saturating_sub(1);
    match mgr.goto_definition(&path, lsp_line, character).await {
        Some(locations) if !locations.is_empty() => {
            let mut results = Vec::new();
            for loc in &locations {
                let uri_str = loc.uri.as_str();
                let display_path = crate::lsp::uri_to_path(uri_str)
                    .map(|p| {
                        p.strip_prefix(cwd)
                            .unwrap_or(&p)
                            .to_string_lossy()
                            .to_string()
                    })
                    .unwrap_or_else(|| uri_str.to_string());
                results.push(format!(
                    "{}:{}:{}",
                    display_path,
                    loc.range.start.line + 1,
                    loc.range.start.character + 1,
                ));
            }
            // also read a few lines of context from the definition site
            let mut output = results.join("\n");
            if let Some(loc) = locations.first() {
                if let Some(def_path) = crate::lsp::uri_to_path(loc.uri.as_str()) {
                    if let Ok(content) = std::fs::read_to_string(&def_path) {
                        let lines: Vec<&str> = content.lines().collect();
                        let start = loc.range.start.line as usize;
                        let end = (start + 10).min(lines.len());
                        if start < lines.len() {
                            output.push_str("\n\n");
                            for (i, line) in lines[start..end].iter().enumerate() {
                                output.push_str(&format!("{:>4} | {}\n", start + i + 1, line));
                            }
                        }
                    }
                }
            }
            ToolResult::Success { output, diff: None, read: None }
        }
        Some(_) => ToolResult::Success {
            output: "no definition found".to_string(),
            diff: None,
            read: None,
        },
        None => ToolResult::Error("LSP goto_definition request failed (no server for this file type?)".to_string()),
    }
}

pub(super) async fn exec_diagnostics(
    input: &serde_json::Value,
    cwd: &Path,
    ctx: &ApiContext,
) -> ToolResult {
    let lsp = match &ctx.lsp {
        Some(l) => l,
        None => return ToolResult::Error("no LSP server available".to_string()),
    };

    let mgr = lsp.lock().await;

    if let Some(path_str) = input.get("path").and_then(|v| v.as_str()) {
        let path = resolve_path(path_str, cwd);
        let diags = mgr.diagnostics_for(&path).await;
        if diags.is_empty() {
            return ToolResult::Success {
                output: format!("no diagnostics for {}", path_str),
                diff: None,
                read: None,
            };
        }
        let display_path = path.strip_prefix(cwd)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let mut lines = Vec::new();
        for d in &diags {
            let sev = match d.severity {
                crate::lsp::DiagSeverity::Error => "error",
                crate::lsp::DiagSeverity::Warning => "warning",
                crate::lsp::DiagSeverity::Info => "info",
                crate::lsp::DiagSeverity::Hint => "hint",
            };
            lines.push(format!(
                "{}:{}:{}: {}: {}",
                display_path,
                d.line + 1,
                d.col + 1,
                sev,
                d.message
            ));
        }
        ToolResult::Success {
            output: lines.join("\n"),
            diff: None,
            read: None,
        }
    } else {
        match mgr.diagnostics_summary().await {
            Some(summary) => ToolResult::Success {
                output: summary,
                diff: None,
                read: None,
            },
            None => ToolResult::Success {
                output: "no diagnostics".to_string(),
                diff: None,
                read: None,
            },
        }
    }
}
