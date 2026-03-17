// true if the char is a problematic control or invisible unicode codepoint
// that should be stripped before rendering. preserves \n.
fn is_control_or_invisible(c: char) -> bool {
    match c {
        // C0 controls except \n (\x0A)
        '\x00'..='\x09' | '\x0B'..='\x1F' => true,
        // DEL
        '\x7F' => true,
        // C1 controls
        '\u{0080}'..='\u{009F}' => true,
        // unicode directional overrides
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => true,
        // zero-width chars and BOM
        '\u{200B}'..='\u{200D}' | '\u{FEFF}' => true,
        _ => false,
    }
}

// strip control characters and invisible unicode from text.
// tabs are replaced with spaces (ratatui cannot handle tab stops).
// preserves \n. does not handle ANSI escape sequences or
// carriage return line-overwrite semantics (use strip_ansi for that).
pub fn sanitize_text(s: &str) -> String {
    s.chars()
        .filter_map(|c| {
            if c == '\n' {
                Some(c)
            } else if c == '\t' {
                Some(' ')
            } else if is_control_or_invisible(c) {
                None
            } else {
                Some(c)
            }
        })
        .collect()
}

// truncate a string to at most `max_bytes`, backing up to the nearest
// char boundary so we never slice inside a multi-byte codepoint.
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// remove ansi escape sequences, terminal control codes, and invisible unicode.
// covers CSI sequences (\x1b[...X), OSC sequences (\x1b]...ST), character set
// designations (\x1b(F), carriage return line-overwrite, and bare \x1b.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch as u32 >= 0x40 && ch as u32 <= 0x7E {
                            break;
                        }
                    }
                }
                Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                    chars.next();
                    let mut prev = '\0';
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch == '\x07' {
                            break;
                        }
                        if prev == '\x1b' && ch == '\\' {
                            break;
                        }
                        prev = ch;
                    }
                }
                Some('(' | ')' | '*' | '+') => {
                    chars.next();
                    chars.next();
                }
                Some(_) => {
                    chars.next();
                }
                _ => {}
            }
        } else if c == '\r' {
            // carriage return: overwrite the current line
            if let Some(pos) = out.rfind('\n') {
                out.truncate(pos + 1);
            } else {
                out.clear();
            }
        } else if c == '\t' {
            out.push(' ');
        } else if is_control_or_invisible(c) {
            // skip
        } else {
            out.push(c);
        }
    }
    out
}

pub async fn check_for_update() -> Option<String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("rum/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;

    let resp = client
        .get("https://api.github.com/repos/KAJdev/rum/releases/latest")
        .send()
        .await
        .ok()?;

    let json: serde_json::Value = resp.json().await.ok()?;
    let tag = json.get("tag_name")?.as_str()?;
    let latest = tag.trim_start_matches('v');
    let current = env!("CARGO_PKG_VERSION");

    if parse_semver(latest) > parse_semver(current) {
        Some(tag.to_string())
    } else {
        None
    }
}

fn parse_semver(v: &str) -> (u32, u32, u32) {
    let parts: Vec<u32> = v.split('.').filter_map(|p| p.parse().ok()).collect();
    (
        parts.first().copied().unwrap_or(0),
        parts.get(1).copied().unwrap_or(0),
        parts.get(2).copied().unwrap_or(0),
    )
}
