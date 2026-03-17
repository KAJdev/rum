use super::dispatch::{execute_tool, tool_definitions};
use super::types::{ApiContext, ToolResult};
use crate::api::{AuthMethod, ContentBlock, Message, MessageContent, StreamEvent};
use std::path::Path;

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
2. Be thorough \u{2014} explore file trees, read key files, follow imports and references
3. When you have a complete picture, write your final response as a detailed \
structured report with no further tool calls
4. Your final message becomes the output returned to the caller \u{2014} make it comprehensive\
";

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

// map oauth-cased tool names back to local names for execute_tool dispatch
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
                format!("{}...", crate::util::truncate_str(cmd, 57))
            } else {
                cmd.to_string()
            }
        }
        "web_search" => {
            let q = input.get("query").and_then(|v| v.as_str()).unwrap_or("?");
            if q.len() > 60 {
                format!("{}...", crate::util::truncate_str(q, 57))
            } else {
                q.to_string()
            }
        }
        _ => String::new(),
    }
}

fn explore_view_file_preview(input: &serde_json::Value) -> String {
    input
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string()
}

pub(super) async fn exec_explore(
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
                    let _ = tx.send(format!("\u{2192} {}  {}\n", local, preview));
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
