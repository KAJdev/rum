// remove ansi escape sequences and other terminal control codes.
// covers CSI sequences (\x1b[...X), OSC sequences (\x1b]...ST), character set
// designations (\x1b(F), and bare \x1b.
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
