use super::types::{ApiContext, ToolResult};
use crate::api::{AuthMethod, StreamEvent};

const FETCH_MODEL: &str = "claude-haiku-4-5";

const FETCH_SYSTEM: &str = "\
You extract specific information from web page content. The user will give you the \
full text of a page and describe what they are looking for. Return only the relevant \
data — no commentary, no filler. If the page does not contain what they are looking \
for, say so briefly.\
";

// strip html tags, script/style blocks, and common entities from raw html.
// returns plain text suitable for passing to a language model.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // skip script and style blocks entirely
        if bytes[i] == b'<' {
            let rest = &html[i..];
            let lower = rest.to_ascii_lowercase();
            if lower.starts_with("<script") || lower.starts_with("<style")
                || lower.starts_with("<head") || lower.starts_with("<nav")
                || lower.starts_with("<footer") || lower.starts_with("<header")
            {
                // find the matching close tag
                let close = if lower.starts_with("<script") {
                    "</script>"
                } else if lower.starts_with("<style") {
                    "</style>"
                } else if lower.starts_with("<head") {
                    "</head>"
                } else if lower.starts_with("<nav") {
                    "</nav>"
                } else if lower.starts_with("<footer") {
                    "</footer>"
                } else {
                    "</header>"
                };
                if let Some(end) = lower.find(close) {
                    i += end + close.len();
                    out.push('\n');
                    continue;
                }
            }
            // skip html comments
            if rest.starts_with("<!--") {
                if let Some(end) = rest.find("-->") {
                    i += end + 3;
                    continue;
                }
            }
            // skip all other tags, inserting a space so words don't run together
            while i < len && bytes[i] != b'>' {
                i += 1;
            }
            i += 1; // consume '>'
            out.push(' ');
            continue;
        }

        // decode common html entities
        if bytes[i] == b'&' {
            let rest = &html[i..];
            if let Some(semi) = rest.find(';') {
                let entity = &rest[1..semi];
                let replacement = match entity {
                    "amp" => Some("&"),
                    "lt" => Some("<"),
                    "gt" => Some(">"),
                    "quot" => Some("\""),
                    "apos" | "#39" => Some("'"),
                    "nbsp" => Some(" "),
                    "mdash" | "#8212" => Some("—"),
                    "ndash" | "#8211" => Some("–"),
                    "hellip" | "#8230" => Some("…"),
                    "ldquo" | "#8220" => Some("\u{201C}"),
                    "rdquo" | "#8221" => Some("\u{201D}"),
                    "lsquo" | "#8216" => Some("\u{2018}"),
                    "rsquo" | "#8217" => Some("\u{2019}"),
                    _ => None,
                };
                if let Some(r) = replacement {
                    out.push_str(r);
                    i += semi + 1;
                    continue;
                }
            }
        }

        // pass through regular characters
        // SAFETY: we only advance byte-by-byte on ascii boundaries or push full chars below
        let ch = html[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }

    // collapse runs of whitespace into single newlines/spaces
    let mut result = String::with_capacity(out.len());
    let mut last_was_newline = false;
    let mut last_was_space = false;
    for ch in out.chars() {
        if ch == '\n' || ch == '\r' {
            if !last_was_newline {
                result.push('\n');
            }
            last_was_newline = true;
            last_was_space = false;
        } else if ch == ' ' || ch == '\t' {
            if !last_was_space && !last_was_newline {
                result.push(' ');
            }
            last_was_space = true;
        } else {
            result.push(ch);
            last_was_newline = false;
            last_was_space = false;
        }
    }

    result.trim().to_string()
}

pub(super) async fn exec_url_fetch(
    input: &serde_json::Value,
    api_ctx: &ApiContext,
) -> ToolResult {
    use futures::StreamExt;

    let url = match input.get("url").and_then(|v| v.as_str()) {
        Some(u) => u.to_string(),
        None => return ToolResult::Error("missing 'url' parameter".to_string()),
    };
    let prompt = match input.get("prompt").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => return ToolResult::Error("missing 'prompt' parameter".to_string()),
    };

    // fetch the page
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("Mozilla/5.0 (compatible; rum-agent)")
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap_or_default();

    let page_text = match http
        .get(&url)
        .header("Accept", "text/html,application/xhtml+xml,*/*")
        .send()
        .await
    {
        Err(e) => return ToolResult::Error(format!("url_search: fetch failed: {}", e)),
        Ok(resp) => {
            if !resp.status().is_success() {
                return ToolResult::Error(format!(
                    "url_search: server returned {}",
                    resp.status()
                ));
            }
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            let body = match resp.text().await {
                Ok(t) => t,
                Err(e) => return ToolResult::Error(format!("url_search: failed to read body: {}", e)),
            };

            // if it looks like html, strip it; otherwise use raw text
            if content_type.contains("html") || body.trim_start().starts_with('<') {
                strip_html(&body)
            } else {
                body
            }
        }
    };

    if page_text.is_empty() {
        return ToolResult::Error("url_search: page had no readable content".to_string());
    }

    // build headers for the haiku call
    let mut headers = reqwest::header::HeaderMap::new();
    match &api_ctx.auth {
        AuthMethod::ApiKey(key) => {
            let Ok(v) = key.parse() else {
                return ToolResult::Error("url_search: invalid api key".to_string());
            };
            headers.insert("x-api-key", v);
        }
        AuthMethod::Bearer(token) => {
            let Ok(v) = format!("Bearer {}", token).parse() else {
                return ToolResult::Error("url_search: invalid bearer token".to_string());
            };
            headers.insert(reqwest::header::AUTHORIZATION, v);
        }
        AuthMethod::None => {
            return ToolResult::Error("url_search: no credentials available".to_string());
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

    let system_value = if api_ctx.is_oauth {
        serde_json::json!([
            {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
            {"type": "text", "text": FETCH_SYSTEM}
        ])
    } else {
        serde_json::Value::String(FETCH_SYSTEM.to_string())
    };

    let user_message = format!(
        "Page content from {url}:\n\n{page_text}\n\n---\n\nWhat I'm looking for: {prompt}"
    );

    let body = serde_json::json!({
        "model": FETCH_MODEL,
        "max_tokens": 4096,
        "system": system_value,
        "messages": [
            {"role": "user", "content": user_message}
        ],
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
        Err(e) => return ToolResult::Error(format!("url_search: api request failed: {}", e)),
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        return ToolResult::Error(format!("url_search: api error ({}): {}", status, body_text));
    }

    // stream the response and collect text
    let mut byte_stream = resp.bytes_stream();
    let mut sse_buf = String::new();
    let mut result_text = String::new();

    loop {
        if api_ctx.is_cancelled() {
            return ToolResult::Error("cancelled".to_string());
        }
        let chunk = match byte_stream.next().await {
            Some(Ok(c)) => c,
            Some(Err(e)) => return ToolResult::Error(format!("url_search: stream error: {}", e)),
            None => break,
        };
        sse_buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(pos) = sse_buf.find("\n\n") {
            let event_text = sse_buf[..pos].to_string();
            sse_buf = sse_buf[pos + 2..].to_string();

            let Some(evt) = crate::api::parse_sse_event(&event_text) else {
                continue;
            };
            if let StreamEvent::Text(t) = evt {
                result_text.push_str(&t);
            }
        }
    }

    let result_text = result_text.trim().to_string();
    if result_text.is_empty() {
        return ToolResult::Error("url_search: model returned no content".to_string());
    }

    ToolResult::Success {
        output: result_text,
        diff: None,
        read: None,
    }
}
