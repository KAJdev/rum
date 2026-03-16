use super::dispatch::resolve_path;
use super::types::{ReadInfo, ToolResult};
use crate::diff;
use std::path::Path;

pub(super) async fn exec_read(input: &serde_json::Value, cwd: &Path) -> ToolResult {
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

pub(super) async fn exec_view_file(input: &serde_json::Value, cwd: &Path) -> ToolResult {
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
                "'{}' is not a supported image type (jpeg, png, gif, webp) -- got extension '{}'",
                path_str, ext
            ));
        }
    };

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return ToolResult::Error(format!("failed to read '{}': {}", path_str, e)),
    };

    // 5 MB limit - matches the anthropic api's per-image size cap
    const MAX_BYTES: usize = 5 * 1024 * 1024;
    if bytes.len() > MAX_BYTES {
        return ToolResult::Error(format!(
            "'{}' is {:.1} MB -- images must be under 5 MB",
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

pub(super) async fn exec_edit(input: &serde_json::Value, cwd: &Path) -> ToolResult {
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

    let diff_info = diff::compute_diff(&display_path, &content, &new_content);

    if let Err(e) = std::fs::write(&path, &new_content) {
        return ToolResult::Error(format!("failed to write {}: {}", path.display(), e));
    }

    ToolResult::Success {
        output: format!("edited {}", display_path),
        diff: Some(diff_info),
        read: None,
    }
}

pub(super) async fn exec_write(input: &serde_json::Value, cwd: &Path) -> ToolResult {
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

    let diff_info = diff::compute_diff(&display_path, &old_content, content);

    let action = if existed { "wrote" } else { "created" };
    ToolResult::Success {
        output: format!("{} {} ({} bytes)", action, display_path, content.len()),
        diff: Some(diff_info),
        read: None,
    }
}
