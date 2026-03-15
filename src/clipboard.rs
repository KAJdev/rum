// clipboard image detection and pasted path resolution

// tries to save clipboard image data to a temp file and returns the path.
// on macOS uses pngpaste (if available) then osascript.
// on linux uses wl-paste (wayland) then xclip (x11).
pub fn try_read_clipboard_image() -> Option<String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let path = format!("/tmp/rum_img_{ts}.png");

    if read_clipboard_image_to_path(&path) {
        Some(path)
    } else {
        None
    }
}

// returns true if the text appears to be binary data passed through a lossy UTF-8 decode.
// terminals that forward raw clipboard bytes produce replacement chars for non-UTF-8 sequences.
pub fn paste_looks_like_binary(text: &str) -> bool {
    let sample: Vec<char> = text.chars().take(200).collect();
    if sample.len() < 4 {
        return false;
    }
    let replacement_count = sample.iter().filter(|&&c| c == '\u{FFFD}').count();
    replacement_count * 4 > sample.len()
}

// if the pasted text is a file path (possibly as a file:// URL from drag-and-drop),
// return the resolved filesystem path. handles file:// URLs, percent-encoding,
// shell-style quote wrapping, and backslash-escaped spaces.
pub fn resolve_pasted_path(text: &str, cwd: &std::path::Path) -> Option<String> {
    let text = text.trim();

    if text.contains('\n') {
        return None;
    }

    // strip surrounding shell quotes that some terminals add
    let text = if (text.starts_with('\'') && text.ends_with('\''))
        || (text.starts_with('"') && text.ends_with('"'))
    {
        &text[1..text.len() - 1]
    } else {
        text
    };

    // strip file:// URL prefix (file:///path or file://hostname/path)
    let path_str = if let Some(rest) = text.strip_prefix("file://") {
        if let Some(stripped) = rest.strip_prefix('/') {
            format!("/{stripped}")
        } else {
            rest.split_once('/')
                .map(|(_, p)| format!("/{p}"))?
                .to_string()
        }
    } else {
        text.to_string()
    };

    let path_str = percent_decode(&path_str);
    let path_str = unescape_backslashes(&path_str);
    let path_str = path_str.trim_end_matches('/');
    if path_str.is_empty() {
        return None;
    }

    // expand leading ~ to home directory
    let path_str: std::borrow::Cow<str> = if let Some(rest) = path_str.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(rest).to_string_lossy().into_owned().into())
            .unwrap_or(path_str.into())
    } else {
        path_str.into()
    };

    let path = std::path::Path::new(path_str.as_ref());
    if path.is_absolute() && path.exists() {
        return Some(path_str.into_owned());
    }

    let full = cwd.join(path);
    if full.exists() {
        return Some(full.to_string_lossy().into_owned());
    }

    None
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte as char);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn unescape_backslashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                out.push(next);
                chars.next();
                continue;
            }
        }
        out.push(c);
    }
    out
}

#[cfg(target_os = "macos")]
fn read_clipboard_image_to_path(path: &str) -> bool {
    // try pngpaste first (handles PNG, TIFF, JPEG, etc.)
    if let Ok(status) = std::process::Command::new("pngpaste")
        .arg(path)
        .stderr(std::process::Stdio::null())
        .status()
    {
        if status.success() {
            return true;
        }
    }

    // try PNG directly via osascript
    let script = format!(
        "set d to the clipboard as «class PNGf»\n\
         set f to open for access POSIX file \"{path}\" with write permission\n\
         write d to f\n\
         close access f"
    );
    if matches!(
        std::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .stderr(std::process::Stdio::null())
            .status(),
        Ok(s) if s.success()
    ) {
        return true;
    }

    // screenshots are stored as TIFF on the clipboard; write TIFF then convert with sips
    let tiff_path = format!("{path}.tiff");
    let script = format!(
        "set d to the clipboard as «class TIFF»\n\
         set f to open for access POSIX file \"{tiff_path}\" with write permission\n\
         write d to f\n\
         close access f"
    );
    if matches!(
        std::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .stderr(std::process::Stdio::null())
            .status(),
        Ok(s) if s.success()
    ) {
        let converted = std::process::Command::new("sips")
            .args(["-s", "format", "png", &tiff_path, "--out", path])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let _ = std::fs::remove_file(&tiff_path);
        return converted;
    }

    false
}

#[cfg(target_os = "linux")]
fn read_clipboard_image_to_path(path: &str) -> bool {
    // try wl-paste (wayland)
    if let Ok(out) = std::process::Command::new("wl-paste")
        .args(["--type", "image/png", "--no-newline"])
        .stderr(std::process::Stdio::null())
        .output()
    {
        if out.status.success() && !out.stdout.is_empty() {
            if std::fs::write(path, &out.stdout).is_ok() {
                return true;
            }
        }
    }

    // fall back to xclip (x11)
    if let Ok(out) = std::process::Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "image/png", "-o"])
        .stderr(std::process::Stdio::null())
        .output()
    {
        if out.status.success() && !out.stdout.is_empty() {
            if std::fs::write(path, &out.stdout).is_ok() {
                return true;
            }
        }
    }

    false
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read_clipboard_image_to_path(_path: &str) -> bool {
    false
}
