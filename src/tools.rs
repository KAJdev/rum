use crate::api::{AuthMethod, ContentBlock, Message, MessageContent, StreamEvent};
use serde::Serialize;
use similar::{ChangeTag, TextDiff};
use std::path::{Path, PathBuf};

// passed down from the agent so tools can make api calls and send background job events
pub struct ApiContext {
    pub auth: AuthMethod,
    pub base_url: String,
    pub is_oauth: bool,
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub job_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::tui::JobEvent>>,
    pub next_job_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub lsp: Option<std::sync::Arc<tokio::sync::Mutex<crate::lsp::LspManager>>>,
}

impl ApiContext {
    fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct DiffStat {
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Debug, Clone)]
pub enum ToolResult {
    Success {
        output: String,
        diff: Option<DiffInfo>,
        // set by the read tool to indicate which file/line was viewed
        read: Option<ReadInfo>,
    },
    // image file read result -- base64-encoded data sent to the model as a content block
    Image {
        text: String,
        data: String,
        media_type: String,
    },
    Error(String),
}

#[derive(Debug, Clone)]
pub struct ReadInfo {
    pub path: String,
    // 1-indexed line offset
    pub offset: usize,
}

#[derive(Debug, Clone)]
pub struct DiffInfo {
    pub path: String,
    pub stat: DiffStat,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone)]
pub struct DiffHunk {
    pub new_start: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub tag: DiffLineTag,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiffLineTag {
    Equal,
    Insert,
    Delete,
}

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
            description: "Spawn a focused sub-agent that uses read-only tools (read, bash, web_search) to thoroughly investigate a topic, then returns a detailed structured writeup. Use this when a task requires substantial exploration before you can act — inspecting an unfamiliar codebase section, tracing how something works across many files, or researching a problem space.".to_string(),
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

fn resolve_path(path: &str, cwd: &Path) -> PathBuf {
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
            "read" => exec_read(input, cwd).await,
            "bash" => {
                exec_bash(
                    input,
                    cwd,
                    stream_tx,
                    api_ctx,
                )
                .await
            }
            "edit" => exec_edit(input, cwd).await,
            "write" => exec_write(input, cwd).await,
            "web_search" => exec_web_search(input).await,
            "view_file" => exec_view_file(input, cwd).await,
            "explore" => match api_ctx {
                Some(ctx) => exec_explore(input, cwd, stream_tx, ctx).await,
                None => ToolResult::Error("explore is not available in this context".to_string()),
            },
            "goto_definition" => match api_ctx {
                Some(ctx) => exec_goto_definition(input, cwd, ctx).await,
                None => ToolResult::Error("goto_definition requires LSP".to_string()),
            },
            "diagnostics" => match api_ctx {
                Some(ctx) => exec_diagnostics(input, cwd, ctx).await,
                None => ToolResult::Error("diagnostics requires LSP".to_string()),
            },
            _ => ToolResult::Error(format!("unknown tool: {}", name)),
        }
    })
}

async fn exec_read(input: &serde_json::Value, cwd: &Path) -> ToolResult {
    let raw_path = match input.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolResult::Error("missing 'path' parameter".to_string()),
    };
    let path = resolve_path(raw_path, cwd);

    let offset = input
        .get("offset")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let limit = input
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);

    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            let start = offset.unwrap_or(1).saturating_sub(1);
            let end = match limit {
                Some(l) => (start + l).min(lines.len()),
                None => lines.len().min(start + 2000),
            };

            let slice = &lines[start.min(lines.len())..end.min(lines.len())];
            let result = slice.join("\n");

            // truncate to ~50KB
            let output = if result.len() > 50_000 {
                format!("{}...\n[truncated]", &result[..50_000])
            } else {
                result
            };

            let display_path = path.strip_prefix(cwd)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();

            ToolResult::Success {
                output,
                diff: None,
                read: Some(ReadInfo {
                    path: display_path,
                    offset: offset.unwrap_or(1),
                }),
            }
        }
        Err(e) => ToolResult::Error(format!("failed to read {}: {}", path.display(), e)),
    }
}

// strip ANSI escape sequences and normalize carriage returns.
// handles CSI (colors, cursor, erase), OSC/DCS/APC string sequences,
// character set designations (e.g. ESC ( B), and other escape sequences.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => {
                match chars.peek() {
                    Some(&'[') => {
                        // CSI: ESC [ <params> <final>  where final is 0x40-0x7E
                        chars.next();
                        for c in chars.by_ref() {
                            if c as u32 >= 0x40 && c as u32 <= 0x7E {
                                break;
                            }
                        }
                    }
                    Some(&']') | Some(&'P') | Some(&'X') | Some(&'^') | Some(&'_') => {
                        // OSC and other string sequences terminated by BEL or ST (ESC \)
                        chars.next();
                        let mut prev = '\0';
                        for c in chars.by_ref() {
                            if c == '\x07' {
                                break;
                            }
                            if prev == '\x1b' && c == '\\' {
                                break;
                            }
                            prev = c;
                        }
                    }
                    Some(&'(' | &')' | &'*' | &'+') => {
                        // character set designation: ESC ( F, ESC ) F, etc.
                        // three bytes total, skip the intermediate and final
                        chars.next();
                        chars.next();
                    }
                    Some(_) => {
                        // two-character sequence (e.g. ESC M reverse index)
                        chars.next();
                    }
                    None => {}
                }
            }
            '\r' => {
                // \r\n -> keep as \n (consumed on the next iteration)
                // bare \r -> newline so overwritten content stays readable
                if chars.peek() != Some(&'\n') {
                    out.push('\n');
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

async fn exec_bash(
    input: &serde_json::Value,
    cwd: &Path,
    stream_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    api_ctx: Option<&ApiContext>,
) -> ToolResult {
    let command = match input.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return ToolResult::Error("missing 'command' parameter".to_string()),
    };

    let background = input
        .get("background")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let timeout_secs = input.get("timeout").and_then(|v| v.as_u64()).unwrap_or(600);

    let cancel = api_ctx.and_then(|c| c.cancel.clone());

    if background {
        return exec_bash_background(command, cwd, timeout_secs, api_ctx);
    }

    use tokio::io::AsyncReadExt;

    let mut child = match tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ToolResult::Error(format!("failed to execute: {}", e)),
    };

    let stdout = child.stdout.take().expect("stdout should be piped");
    let stderr = child.stderr.take().expect("stderr should be piped");

    // funnel both stdout and stderr into a single byte channel
    let (merge_tx, mut merge_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    let tx1 = merge_tx.clone();
    let stdout_task = tokio::spawn(async move {
        let mut rdr = stdout;
        let mut buf = vec![0u8; 4096];
        loop {
            match rdr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx1.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let tx2 = merge_tx.clone();
    let stderr_task = tokio::spawn(async move {
        let mut rdr = stderr;
        let mut buf = vec![0u8; 4096];
        loop {
            match rdr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx2.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // dropping the original sender so the channel closes when both reader tasks finish
    drop(merge_tx);

    let mut collected;
    let mut raw_collected = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut timed_out = false;

    // poll for cancellation every 100 ms alongside the output stream
    let cancel_poll = async {
        loop {
            if cancel
                .as_ref()
                .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(false)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    tokio::pin!(cancel_poll);

    loop {
        tokio::select! {
            biased;
            chunk = tokio::time::timeout_at(deadline, merge_rx.recv()) => {
                match chunk {
                    Ok(Some(bytes)) => {
                        let raw = String::from_utf8_lossy(&bytes);
                        raw_collected.push_str(&raw);
                        // per-chunk strip for streaming display (may have minor
                        // artifacts from sequences split across chunks, but the
                        // final result is stripped from the full raw buffer below)
                        if let Some(ref tx) = stream_tx {
                            let clean = strip_ansi(&raw);
                            if !clean.is_empty() {
                                let _ = tx.send(clean);
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        timed_out = true;
                        break;
                    }
                }
            }
            _ = &mut cancel_poll => {
                stdout_task.abort();
                stderr_task.abort();
                child.kill().await.ok();
                return ToolResult::Error("cancelled".to_string());
            }
        }
    }

    stdout_task.abort();
    stderr_task.abort();

    if timed_out {
        child.kill().await.ok();
        return ToolResult::Error(format!("command timed out after {}s", timeout_secs));
    }

    let exit_status = child.wait().await.ok();
    let exit_code = exit_status.and_then(|s| s.code()).unwrap_or(-1);

    // strip the full raw buffer so escape sequences split across chunks are
    // handled correctly (streaming display may have had minor artifacts but
    // this final result is what the agent and history see)
    collected = strip_ansi(&raw_collected);

    if collected.is_empty() {
        collected = "(no output)".to_string();
    }

    if collected.len() > 50_000 {
        collected = format!("{}...\n[truncated]", &collected[..50_000]);
    }

    let output = if exit_code != 0 {
        format!("[exit code: {}]\n{}", exit_code, collected)
    } else {
        collected
    };

    ToolResult::Success { output, diff: None, read: None }
}

fn exec_bash_background(
    command: &str,
    cwd: &Path,
    timeout_secs: u64,
    api_ctx: Option<&ApiContext>,
) -> ToolResult {
    let job_tx = match api_ctx.and_then(|c| c.job_tx.clone()) {
        Some(tx) => tx,
        None => return ToolResult::Error("background jobs not available in this context".to_string()),
    };

    let job_id = api_ctx
        .map(|c| c.next_job_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(0);

    let cmd_label: String = command.chars().take(40).collect();
    let return_label = cmd_label.clone();
    let command = command.to_string();
    let cwd = cwd.to_path_buf();

    let _ = job_tx.send(crate::tui::JobEvent::Show { id: job_id });

    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;

        let mut child = match tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&command)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = job_tx.send(crate::tui::JobEvent::Complete {
                    id: job_id,
                    status: crate::tui::JobStatus::Failed("error".to_string()),
                    summary: format!("background `{}`: {}", cmd_label, e),
                });
                return;
            }
        };

        let stdout = child.stdout.take().expect("stdout should be piped");
        let stderr = child.stderr.take().expect("stderr should be piped");

        let (merge_tx, mut merge_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        let tx1 = merge_tx.clone();
        let stdout_task = tokio::spawn(async move {
            let mut rdr = stdout;
            let mut buf = vec![0u8; 4096];
            loop {
                match rdr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx1.send(buf[..n].to_vec()).is_err() { break; }
                    }
                }
            }
        });

        let tx2 = merge_tx.clone();
        let stderr_task = tokio::spawn(async move {
            let mut rdr = stderr;
            let mut buf = vec![0u8; 4096];
            loop {
                match rdr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx2.send(buf[..n].to_vec()).is_err() { break; }
                    }
                }
            }
        });

        drop(merge_tx);

        let mut raw_collected = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let mut timed_out = false;

        loop {
            tokio::select! {
                biased;
                chunk = tokio::time::timeout_at(deadline, merge_rx.recv()) => {
                    match chunk {
                        Ok(Some(bytes)) => {
                            let raw = String::from_utf8_lossy(&bytes);
                            raw_collected.push_str(&raw);
                        }
                        Ok(None) => break,
                        Err(_) => {
                            timed_out = true;
                            break;
                        }
                    }
                }
            }
        }

        stdout_task.abort();
        stderr_task.abort();

        if timed_out {
            child.kill().await.ok();
            let _ = job_tx.send(crate::tui::JobEvent::Complete {
                id: job_id,
                status: crate::tui::JobStatus::Failed("timed out".to_string()),
                summary: format!("background `{}`: timed out after {}s", cmd_label, timeout_secs),
            });
            return;
        }

        let exit_status = child.wait().await.ok();
        let exit_code = exit_status.and_then(|s| s.code()).unwrap_or(-1);

        let mut collected = strip_ansi(&raw_collected);
        if collected.is_empty() {
            collected = "(no output)".to_string();
        }
        if collected.len() > 50_000 {
            collected = format!("{}...\n[truncated]", &collected[..50_000]);
        }

        let output = if exit_code != 0 {
            format!("[exit code: {}]\n{}", exit_code, collected)
        } else {
            collected
        };

        if exit_code == 0 {
            let _ = job_tx.send(crate::tui::JobEvent::Complete {
                id: job_id,
                status: crate::tui::JobStatus::Passed,
                summary: format!("background `{}`:\n{}", cmd_label, output),
            });
        } else {
            let _ = job_tx.send(crate::tui::JobEvent::Complete {
                id: job_id,
                status: crate::tui::JobStatus::Failed(format!("exit {}", exit_code)),
                summary: format!("background `{}`:\n{}", cmd_label, output),
            });
        }
    });

    ToolResult::Success {
        output: format!("started in background: {}", return_label),
        diff: None,
        read: None,
    }
}
async fn exec_view_file(input: &serde_json::Value, cwd: &Path) -> ToolResult {
    use base64::{engine::general_purpose::STANDARD, Engine};

    let path_str = match input.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolResult::Error("missing 'path' parameter".to_string()),
    };

    let path = resolve_path(path_str, cwd);

    let media_type = match detect_image_media_type(&path) {
        Some(t) => t,
        None => {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("unknown");
            return ToolResult::Error(format!(
                "'{}' is not a supported image type (jpeg, png, gif, webp) — got extension '{}'",
                path_str, ext
            ));
        }
    };

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return ToolResult::Error(format!("failed to read '{}': {}", path_str, e)),
    };

    // 5 MB limit — matches the anthropic api's per-image size cap
    const MAX_BYTES: usize = 5 * 1024 * 1024;
    if bytes.len() > MAX_BYTES {
        return ToolResult::Error(format!(
            "'{}' is {:.1} MB — images must be under 5 MB",
            path_str,
            bytes.len() as f64 / 1_048_576.0
        ));
    }

    let data = STANDARD.encode(&bytes);
    let kb = bytes.len() as f64 / 1024.0;
    let text = format!("{} [{}, {:.1} KB]", path.display(), media_type, kb);

    ToolResult::Image {
        text,
        data,
        media_type: media_type.to_string(),
    }
}

// detect an image's mime type from magic bytes first, then file extension
fn detect_image_media_type(path: &Path) -> Option<&'static str> {
    // try magic bytes from the first 12 bytes of the file
    if let Ok(bytes) = std::fs::read(path) {
        if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
            return Some("image/png");
        }
        if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return Some("image/jpeg");
        }
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            return Some("image/gif");
        }
        if bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
            return Some("image/webp");
        }
    }

    // fall back to extension
    match path.extension()?.to_str()?.to_lowercase().as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

async fn exec_edit(input: &serde_json::Value, cwd: &Path) -> ToolResult {
    let path = match input.get("path").and_then(|v| v.as_str()) {
        Some(p) => resolve_path(p, cwd),
        None => return ToolResult::Error("missing 'path' parameter".to_string()),
    };
    let old_text = match input.get("oldText").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return ToolResult::Error("missing 'oldText' parameter".to_string()),
    };
    let new_text = match input.get("newText").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return ToolResult::Error("missing 'newText' parameter".to_string()),
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return ToolResult::Error(format!("failed to read {}: {}", path.display(), e)),
    };

    let count = content.matches(old_text).count();
    if count == 0 {
        return ToolResult::Error(format!(
            "oldText not found in {}. make sure it matches exactly.",
            path.display()
        ));
    }
    if count > 1 {
        return ToolResult::Error(format!(
            "oldText found {} times in {}. provide a more unique match.",
            count,
            path.display()
        ));
    }

    let new_content = content.replacen(old_text, new_text, 1);

    let display_path = path
        .strip_prefix(cwd)
        .unwrap_or(&path)
        .to_string_lossy()
        .to_string();

    let diff_info = compute_diff(&display_path, &content, &new_content);

    if let Err(e) = std::fs::write(&path, &new_content) {
        return ToolResult::Error(format!("failed to write {}: {}", path.display(), e));
    }

    ToolResult::Success {
        output: format!("edited {}", display_path),
        diff: Some(diff_info),
        read: None,
    }
}

async fn exec_write(input: &serde_json::Value, cwd: &Path) -> ToolResult {
    let path = match input.get("path").and_then(|v| v.as_str()) {
        Some(p) => resolve_path(p, cwd),
        None => return ToolResult::Error("missing 'path' parameter".to_string()),
    };
    let content = match input.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return ToolResult::Error("missing 'content' parameter".to_string()),
    };

    // create parent dirs
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return ToolResult::Error(format!(
                "failed to create directories for {}: {}",
                path.display(),
                e
            ));
        }
    }

    let existed = path.exists();
    let old_content = if existed {
        std::fs::read_to_string(&path).unwrap_or_default()
    } else {
        String::new()
    };

    if let Err(e) = std::fs::write(&path, content) {
        return ToolResult::Error(format!("failed to write {}: {}", path.display(), e));
    }

    let display_path = path
        .strip_prefix(cwd)
        .unwrap_or(&path)
        .to_string_lossy()
        .to_string();

    let diff_info = compute_diff(&display_path, &old_content, content);

    let action = if existed { "wrote" } else { "created" };
    ToolResult::Success {
        output: format!("{} {} ({} bytes)", action, display_path, content.len()),
        diff: Some(diff_info),
        read: None,
    }
}

pub fn compute_diff(path: &str, old: &str, new: &str) -> DiffInfo {
    let text_diff = TextDiff::from_lines(old, new);
    let mut additions = 0;
    let mut deletions = 0;
    let mut hunks = Vec::new();

    for group in text_diff.grouped_ops(3) {
        // the first op's new range start gives us the line number in the new file
        let new_start = group.first().map(|op| op.new_range().start).unwrap_or(0);
        let mut hunk = DiffHunk { new_start, lines: Vec::new() };
        for op in &group {
            for change in text_diff.iter_changes(op) {
                let tag = match change.tag() {
                    ChangeTag::Equal => DiffLineTag::Equal,
                    ChangeTag::Insert => {
                        additions += 1;
                        DiffLineTag::Insert
                    }
                    ChangeTag::Delete => {
                        deletions += 1;
                        DiffLineTag::Delete
                    }
                };
                hunk.lines.push(DiffLine {
                    tag,
                    content: change.value().to_string(),
                });
            }
        }
        if !hunk.lines.is_empty() {
            hunks.push(hunk);
        }
    }

    DiffInfo {
        path: path.to_string(),
        stat: DiffStat {
            additions,
            deletions,
        },
        hunks,
    }
}

const EXPLORE_MODEL: &str = "claude-haiku-4-5";

const EXPLORE_SYSTEM: &str = "\
You are a focused research and exploration assistant. Your job is to thoroughly \
investigate a topic using the provided tools, then write a complete, structured \
report of everything you found.

Available tools:
- read: read a file's contents
- bash: run read-only shell commands (ls, find, grep, cat, rg, etc.)
- web_search: search the web
- view_file: view an image file (jpg, png, gif, webp)

Process:
1. Use tools to gather all relevant information about the prompt
2. Be thorough — explore file trees, read key files, follow imports and references
3. When you have a complete picture, write your final response as a detailed \
structured report with no further tool calls
4. Your final message becomes the output returned to the caller — make it comprehensive

Report guidelines:
- Use clear sections with headers
- Include specific file paths, function names, line numbers, and code snippets
- Cover all aspects relevant to the original prompt
- Be direct and information-dense — no filler";

// tool definitions sent to the explore sub-agent (read-only subset, with oauth name casing if needed)
fn explore_tools_json(is_oauth: bool) -> Vec<serde_json::Value> {
    let allowed = ["read", "bash", "web_search", "view_file"];
    tool_definitions()
        .into_iter()
        .filter(|t| allowed.contains(&t.name.as_str()))
        .map(|t| {
            let name = if is_oauth {
                match t.name.as_str() {
                    "read" => "Read",
                    "bash" => "Bash",
                    "web_search" => "WebSearch",
                    _ => t.name.as_str(),
                }
                .to_string()
            } else {
                t.name.clone()
            };
            serde_json::json!({
                "name": name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        })
        .collect()
}

// map oauth-cased tool names back to local names for execute_tool dispatch
fn explore_is_retryable(e: &str) -> bool {
    e.contains("429")
        || e.contains("rate_limit")
        || e.contains("overloaded")
        || e.contains("529")
        || e.contains("stream error")
        || e.contains("stream read error")
        || e.contains("connection")
        || e.contains("timed out")
}

fn explore_local_name(name: &str) -> &str {
    match name {
        "Read" => "read",
        "Bash" => "bash",
        "WebSearch" => "web_search",
        other => other,
    }
}

// short display string for a sub-tool call shown in the mini stream
fn explore_arg_preview(name: &str, input: &serde_json::Value) -> String {
    match name {
        "read" => input
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        "view_file" => explore_view_file_preview(input),
        "bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("?");
            if cmd.len() > 60 {
                format!("{}...", &cmd[..57])
            } else {
                cmd.to_string()
            }
        }
        "web_search" => {
            let q = input.get("query").and_then(|v| v.as_str()).unwrap_or("?");
            if q.len() > 60 {
                format!("{}...", &q[..57])
            } else {
                q.to_string()
            }
        }
        _ => String::new(),
    }
}

// short preview of a view_file path arg
fn explore_view_file_preview(input: &serde_json::Value) -> String {
    input
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string()
}

async fn exec_explore(
    input: &serde_json::Value,
    cwd: &Path,
    stream_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    api_ctx: &ApiContext,
) -> ToolResult {
    use futures::StreamExt;

    let prompt = match input.get("prompt").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => return ToolResult::Error("missing 'prompt' parameter".to_string()),
    };

    let http = reqwest::Client::new();
    let tools_json = explore_tools_json(api_ctx.is_oauth);

    let system_value = if api_ctx.is_oauth {
        serde_json::json!([
            {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
            {"type": "text", "text": EXPLORE_SYSTEM}
        ])
    } else {
        serde_json::Value::String(EXPLORE_SYSTEM.to_string())
    };

    let mut messages: Vec<Message> = vec![Message {
        role: "user".to_string(),
        content: MessageContent::Text(prompt),
    }];

    let mut retries = 0u32;
    loop {
        if api_ctx.is_cancelled() {
            return ToolResult::Error("cancelled".to_string());
        }
        // build request headers
        let mut headers = reqwest::header::HeaderMap::new();
        match &api_ctx.auth {
            AuthMethod::ApiKey(key) => {
                let Ok(v) = key.parse() else {
                    return ToolResult::Error("explore: invalid api key header".to_string());
                };
                headers.insert("x-api-key", v);
            }
            AuthMethod::Bearer(token) => {
                let Ok(v) = format!("Bearer {}", token).parse() else {
                    return ToolResult::Error("explore: invalid bearer header".to_string());
                };
                headers.insert(reqwest::header::AUTHORIZATION, v);
            }
            AuthMethod::None => {
                return ToolResult::Error("explore: no credentials available".to_string());
            }
        }
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        if api_ctx.is_oauth {
            headers.insert(
                "anthropic-beta",
                "claude-code-20250219,oauth-2025-04-20".parse().unwrap(),
            );
            headers.insert(
                reqwest::header::USER_AGENT,
                "claude-cli/2.1.2 (external, cli)".parse().unwrap(),
            );
            headers.insert("x-app", "cli".parse().unwrap());
        }

        let messages_json = match serde_json::to_value(&messages) {
            Ok(v) => v,
            Err(e) => {
                return ToolResult::Error(format!("explore: failed to serialize messages: {}", e))
            }
        };

        let body = serde_json::json!({
            "model": EXPLORE_MODEL,
            "max_tokens": 8192,
            "system": system_value,
            "cache_control": {"type": "ephemeral"},
            "tools": tools_json,
            "messages": messages_json,
            "stream": true,
        });

        let resp = match http
            .post(format!("{}/v1/messages", api_ctx.base_url))
            .headers(headers)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                if retries < 3 {
                    retries += 1;
                    tokio::time::sleep(std::time::Duration::from_secs(1u64 << retries.min(4)))
                        .await;
                    continue;
                }
                return ToolResult::Error(format!("explore: request failed: {}", e));
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            if retries < 3 && matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529) {
                retries += 1;
                tokio::time::sleep(std::time::Duration::from_secs(1u64 << retries.min(4))).await;
                continue;
            }
            return ToolResult::Error(format!("explore: api error ({}): {}", status, body_text));
        }

        // process the SSE stream for this turn
        let mut byte_stream = resp.bytes_stream();
        let mut sse_buf = String::new();

        let mut response_blocks: Vec<ContentBlock> = Vec::new();
        let mut current_text = String::new();
        let mut current_tool_id = String::new();
        let mut current_tool_name = String::new();
        let mut current_tool_input = String::new();
        let mut stop_reason: Option<String> = None;
        let mut in_tool = false;
        let mut in_text = false;
        let mut stream_errors: Vec<String> = Vec::new();

        loop {
            if api_ctx.is_cancelled() {
                return ToolResult::Error("cancelled".to_string());
            }
            let chunk = match byte_stream.next().await {
                Some(Ok(c)) => c,
                Some(Err(e)) => {
                    stream_errors.push(format!("stream read error: {}", e));
                    break;
                }
                None => break,
            };
            sse_buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = sse_buf.find("\n\n") {
                let event_text = sse_buf[..pos].to_string();
                sse_buf = sse_buf[pos + 2..].to_string();

                let Some(evt) = crate::api::parse_sse_event(&event_text) else {
                    continue;
                };

                match evt {
                    StreamEvent::Text(t) => {
                        if !in_tool {
                            in_text = true;
                            current_text.push_str(&t);
                        }
                    }
                    StreamEvent::ToolUseStart { id, name } => {
                        in_tool = true;
                        in_text = false;
                        current_tool_id = id;
                        current_tool_name = name;
                        current_tool_input.clear();
                    }
                    StreamEvent::ToolUseInput(json) => {
                        current_tool_input.push_str(&json);
                    }
                    StreamEvent::ContentBlockStop => {
                        if in_tool {
                            let input_val: serde_json::Value =
                                serde_json::from_str(&current_tool_input)
                                    .unwrap_or(serde_json::Value::Object(Default::default()));
                            response_blocks.push(ContentBlock::ToolUse {
                                id: current_tool_id.clone(),
                                name: current_tool_name.clone(),
                                input: input_val,
                            });
                            in_tool = false;
                        } else if in_text {
                            if !current_text.is_empty() {
                                response_blocks.push(ContentBlock::Text {
                                    text: current_text.clone(),
                                });
                            }
                            in_text = false;
                        }
                    }
                    StreamEvent::MessageDelta {
                        stop_reason: sr, ..
                    } => {
                        stop_reason = sr;
                    }
                    StreamEvent::Error(e) => {
                        stream_errors.push(e);
                    }
                    _ => {}
                }
            }
        }

        // retry on empty response (same logic as the top-level agent)
        if response_blocks.is_empty() && stop_reason.is_none() {
            let has_retryable = stream_errors.iter().any(|e| explore_is_retryable(e));
            let has_non_retryable = stream_errors.iter().any(|e| !explore_is_retryable(e));
            let silent = stream_errors.is_empty();
            if retries < 3 && !has_non_retryable && (has_retryable || silent) {
                retries += 1;
                tokio::time::sleep(std::time::Duration::from_secs(1u64 << retries.min(4))).await;
                continue;
            }
            let err = stream_errors
                .first()
                .cloned()
                .unwrap_or_else(|| "stream ended without a response".to_string());
            return ToolResult::Error(format!("explore: {}", err));
        }
        retries = 0;

        // push assistant turn
        if !response_blocks.is_empty() {
            messages.push(Message {
                role: "assistant".to_string(),
                content: MessageContent::Blocks(response_blocks.clone()),
            });
        }

        // if no more tool calls, the final text is the writeup
        if stop_reason.as_deref() != Some("tool_use") {
            let writeup = current_text.trim().to_string();
            if writeup.is_empty() {
                return ToolResult::Error("explore: sub-agent produced no writeup".to_string());
            }
            return ToolResult::Success {
                output: writeup,
                diff: None,
                read: None,
            };
        }

        // execute each tool use and collect results
        let tool_uses: Vec<&ContentBlock> = response_blocks
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .collect();

        let mut result_blocks = Vec::new();
        for block in tool_uses {
            if let ContentBlock::ToolUse { id, name, input } = block {
                let local = explore_local_name(name);
                let preview = explore_arg_preview(local, input);

                // send the tool call to the parent's mini stream
                if let Some(ref tx) = stream_tx {
                    let _ = tx.send(format!("→ {}  {}\n", local, preview));
                }

                // sub-tools get no stream_tx and no api_ctx (explore can't recurse)
                let result = execute_tool(local, input, cwd, None, None).await;

                let (content, is_error) = match result {
                    ToolResult::Success { output, .. } => (serde_json::Value::String(output), None),
                    ToolResult::Error(e) => (serde_json::Value::String(e), Some(true)),
                    ToolResult::Image {
                        text,
                        data,
                        media_type,
                    } => {
                        let val = serde_json::json!([
                            {"type": "text", "text": text},
                            {"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}}
                        ]);
                        (val, None)
                    }
                };

                result_blocks.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content,
                    is_error,
                });
            }
        }

        messages.push(Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(result_blocks),
        });
    }
}

async fn exec_web_search(input: &serde_json::Value) -> ToolResult {
    let query = match input.get("query").and_then(|v| v.as_str()) {
        Some(q) => q,
        None => return ToolResult::Error("missing 'query' parameter".to_string()),
    };

    let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default();

    let resp = client
        .post("https://html.duckduckgo.com/html/")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36",
        )
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("q={}", url_encode(query)))
        .send()
        .await;

    match resp {
        Ok(response) if response.status().is_success() => {
            let body = response.text().await.unwrap_or_default();
            let output = parse_ddg_results(&body, limit);
            ToolResult::Success { output, diff: None, read: None }
        }
        Ok(response) => ToolResult::Error(format!("search returned status {}", response.status())),
        Err(e) => ToolResult::Error(format!("search request failed: {}", e)),
    }
}

fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                result.push(byte as char);
            }
            b' ' => result.push('+'),
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

fn parse_ddg_results(html: &str, limit: usize) -> String {
    let mut results: Vec<(String, String, String)> = Vec::new();
    let mut search_pos = 0;

    // each result block contains class="result__a" (title+link)
    // and class="result__snippet" (description)
    while results.len() < limit {
        let title_marker = match html[search_pos..].find("class=\"result__a\"") {
            Some(p) => search_pos + p,
            None => break,
        };

        // extract href from the anchor tag containing result__a
        let tag_start = html[..title_marker].rfind('<').unwrap_or(title_marker);
        let href = extract_href(&html[tag_start..]);

        // extract title text (between > and </a>)
        let title = extract_inner_text(&html[title_marker..]);

        // find the snippet after this title
        let snippet = match html[title_marker..].find("class=\"result__snippet\"") {
            Some(p) => extract_inner_text(&html[title_marker + p..]),
            None => String::new(),
        };

        // extract the actual URL from DDG redirect or the result__url element
        let display_url = match html[title_marker..].find("class=\"result__url\"") {
            Some(p) => {
                let raw = extract_inner_text(&html[title_marker + p..]);
                raw.trim().to_string()
            }
            None => href.clone(),
        };

        if !title.is_empty() {
            results.push((title, display_url, snippet));
        }

        search_pos = title_marker + 1;
    }

    if results.is_empty() {
        return "no results found".to_string();
    }

    let mut output = String::new();
    for (i, (title, url, snippet)) in results.iter().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        output.push_str(&format!("{}. {}\n   {}\n", i + 1, title, url));
        if !snippet.is_empty() {
            output.push_str(&format!("   {}\n", snippet));
        }
    }
    output
}

// extract href="..." value from an HTML tag fragment
fn extract_href(tag: &str) -> String {
    if let Some(start) = tag.find("href=\"") {
        let rest = &tag[start + 6..];
        if let Some(end) = rest.find('"') {
            return rest[..end].to_string();
        }
    }
    String::new()
}

// extract visible text from an HTML fragment starting at a class attribute.
// finds the first '>' after the current position, then collects text until '</'
fn extract_inner_text(html: &str) -> String {
    let start = match html.find('>') {
        Some(p) => p + 1,
        None => return String::new(),
    };
    let rest = &html[start..];
    let end = rest.find("</").unwrap_or(rest.len());
    strip_html_tags(&rest[..end])
}

fn strip_html_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in s.chars() {
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch == '>' {
            in_tag = false;
            continue;
        }
        if !in_tag {
            out.push(ch);
        }
    }
    // collapse runs of whitespace
    let mut result = String::new();
    let mut last_space = false;
    for ch in out.chars() {
        if ch.is_whitespace() {
            if !last_space {
                result.push(' ');
                last_space = true;
            }
        } else {
            result.push(ch);
            last_space = false;
        }
    }
    result.trim().to_string()
}

async fn exec_goto_definition(
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

async fn exec_diagnostics(
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

