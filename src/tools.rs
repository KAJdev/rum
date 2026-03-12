use serde::Serialize;
use similar::{ChangeTag, TextDiff};
use std::path::{Path, PathBuf};

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
    },
    Error(String),
}

#[derive(Debug, Clone)]
pub struct DiffInfo {
    pub path: String,
    pub stat: DiffStat,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone)]
pub struct DiffHunk {
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

pub async fn execute_tool(
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
) -> ToolResult {
    match name {
        "read" => exec_read(input, cwd).await,
        "bash" => exec_bash(input, cwd).await,
        "edit" => exec_edit(input, cwd).await,
        "write" => exec_write(input, cwd).await,
        "web_search" => exec_web_search(input).await,
        _ => ToolResult::Error(format!("unknown tool: {}", name)),
    }
}

async fn exec_read(input: &serde_json::Value, cwd: &Path) -> ToolResult {
    let path = match input.get("path").and_then(|v| v.as_str()) {
        Some(p) => resolve_path(p, cwd),
        None => return ToolResult::Error("missing 'path' parameter".to_string()),
    };

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

            ToolResult::Success {
                output,
                diff: None,
            }
        }
        Err(e) => ToolResult::Error(format!("failed to read {}: {}", path.display(), e)),
    }
}

async fn exec_bash(input: &serde_json::Value, cwd: &Path) -> ToolResult {
    let command = match input.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return ToolResult::Error("missing 'command' parameter".to_string()),
    };

    let timeout_secs = input
        .get("timeout")
        .and_then(|v| v.as_u64())
        .unwrap_or(120);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        tokio::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut text = String::new();
            if !stdout.is_empty() {
                text.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&stderr);
            }
            if text.is_empty() {
                text = "(no output)".to_string();
            }

            // truncate
            if text.len() > 50_000 {
                text = format!("{}...\n[truncated]", &text[..50_000]);
            }

            let exit = output.status.code().unwrap_or(-1);
            if exit != 0 {
                text = format!("[exit code: {}]\n{}", exit, text);
            }

            ToolResult::Success {
                output: text,
                diff: None,
            }
        }
        Ok(Err(e)) => ToolResult::Error(format!("failed to execute: {}", e)),
        Err(_) => ToolResult::Error(format!("command timed out after {}s", timeout_secs)),
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

    let diff_info = compute_diff(
        &display_path,
        old_text,
        new_text,
    );

    if let Err(e) = std::fs::write(&path, &new_content) {
        return ToolResult::Error(format!("failed to write {}: {}", path.display(), e));
    }

    ToolResult::Success {
        output: format!("edited {}", display_path),
        diff: Some(diff_info),
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
    }
}

pub fn compute_diff(path: &str, old: &str, new: &str) -> DiffInfo {
    let text_diff = TextDiff::from_lines(old, new);
    let mut additions = 0;
    let mut deletions = 0;
    let mut hunks = Vec::new();

    for group in text_diff.grouped_ops(3) {
        let mut hunk = DiffHunk { lines: Vec::new() };
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

async fn exec_web_search(input: &serde_json::Value) -> ToolResult {
    let query = match input.get("query").and_then(|v| v.as_str()) {
        Some(q) => q,
        None => return ToolResult::Error("missing 'query' parameter".to_string()),
    };

    let limit = input
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(5) as usize;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default();

    let resp = client
        .post("https://html.duckduckgo.com/html/")
        .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("q={}", url_encode(query)))
        .send()
        .await;

    match resp {
        Ok(response) if response.status().is_success() => {
            let body = response.text().await.unwrap_or_default();
            let output = parse_ddg_results(&body, limit);
            ToolResult::Success { output, diff: None }
        }
        Ok(response) => {
            ToolResult::Error(format!("search returned status {}", response.status()))
        }
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
