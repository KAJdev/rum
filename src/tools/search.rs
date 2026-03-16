use super::types::ToolResult;

pub(super) async fn exec_web_search(input: &serde_json::Value) -> ToolResult {
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
