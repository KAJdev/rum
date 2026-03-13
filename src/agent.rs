use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::api::{ApiClient, ContentBlock, Message, MessageContent, StreamEvent};
use crate::config::Config;
use crate::tools::{self, ToolResult};

// shared flag for cooperative cancellation.
// set to true when the user hits escape during a running operation.
#[derive(Clone)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn reset(&self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

// events sent from the agent to the TUI
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum AgentEvent {
    Thinking(String),
    Text(String),
    ToolStart { id: String, name: String },
    ToolInputDelta(String),
    ToolComplete {
        id: String,
        name: String,
        result: ToolResult,
    },
    // incremental stdout/stderr from a running bash command
    ToolOutputDelta {
        id: String,
        text: String,
    },
    TokenUsage {
        input_tokens: u32,
        output_tokens: u32,
    },
    // status messages (retries, etc.) rendered distinctly from model output
    Status(String),
    TurnComplete,
    Error(String),
}

// control messages sent from the TUI to the agent between turns
pub enum ControlMessage {
    ChangeModel(String),
    ChangeThinking(String),
    UpdateAuth(String),
    ClearHistory,
}

pub struct Agent {
    client: ApiClient,
    messages: Vec<Message>,
    system_prompt: String,
    thinking_level: String,
    cwd: PathBuf,
    cancel: CancelToken,
}

impl Agent {
    pub fn new(config: &Config, client: ApiClient, cwd: PathBuf, cancel: CancelToken) -> Self {
        let mut system = config.system_prompt.clone();
        for ctx in &config.context_files {
            system.push_str("\n\n");
            system.push_str(ctx);
        }

        system.push_str(&format!(
            "\n\nCurrent working directory: {}",
            cwd.display()
        ));

        let messages = crate::persistence::load_history(&cwd);

        Self {
            client,
            messages,
            system_prompt: system,
            thinking_level: config.thinking_level.clone(),
            cwd,
            cancel,
        }
    }

    // number of messages loaded from the persisted history on startup
    pub fn loaded_history_len(&self) -> usize {
        self.messages.len()
    }

    pub fn set_model(&mut self, model: &str) {
        self.client.set_model(model);
    }

    pub fn set_auth_token(&mut self, token: String) {
        self.client.set_bearer(token);
    }

    pub fn set_thinking(&mut self, level: &str) {
        self.thinking_level = level.to_string();
    }

    pub fn clear_history(&mut self) {
        self.messages.clear();
        let _ = crate::persistence::clear_history(&self.cwd);
    }

    pub async fn send_message(
        &mut self,
        user_message: &str,
        event_tx: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<()> {
        let pre_len = self.messages.len();

        self.messages.push(Message {
            role: "user".to_string(),
            content: MessageContent::Text(user_message.to_string()),
        });

        let result = self.run_turn(event_tx).await;

        // if cancelled before any response was produced, remove the
        // dangling user message to maintain valid alternation.
        // completed work from partial turns is preserved by run_turn.
        if self.cancel.is_cancelled() && self.messages.len() == pre_len + 1 {
            self.messages.truncate(pre_len);
        }

        let _ = crate::persistence::save_history(&self.cwd, &self.messages);

        result
    }

    async fn run_turn(
        &mut self,
        event_tx: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<()> {
        let mut retries = 0u32;

        loop {
            let mut stream_errors: Vec<String> = Vec::new();
            let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<StreamEvent>();

            let messages = self.messages.clone();
            let system = self.system_prompt.clone();
            let thinking = self.thinking_level.clone();

            let client_model = self.client.model_clone();
            let client_auth = self.client.auth_clone();
            let client_base_url = self.client.base_url_clone();
            let tools_json = self.client.build_tools_json();

            let stream_handle = tokio::spawn(async move {
                let client = reqwest::Client::new();
                if let Err(e) = stream_request(
                    &client,
                    &client_auth,
                    &client_base_url,
                    &client_model,
                    &messages,
                    &system,
                    &thinking,
                    &tools_json,
                    stream_tx.clone(),
                )
                .await
                {
                    let _ = stream_tx.send(StreamEvent::Error(
                        format!("stream error: {}", e),
                    ));
                }
            });

            let mut response_blocks: Vec<ContentBlock> = Vec::new();
            let mut current_text = String::new();
            let mut current_thinking = String::new();
            let mut current_tool_id = String::new();
            let mut current_tool_name = String::new();
            let mut current_tool_input_json = String::new();
            let mut current_thinking_signature = String::new();
            let mut input_tokens = 0u32;
            let mut output_tokens = 0u32;
            let mut stop_reason: Option<String> = None;
            let mut in_thinking = false;
            let mut in_text = false;
            let mut in_tool = false;

            // map tool_use_id -> ToolResult for reuse
            let mut tool_results: HashMap<String, ToolResult> = HashMap::new();

            while let Some(evt) = stream_rx.recv().await {
                if self.cancel.is_cancelled() {
                    stream_handle.abort();
                    break;
                }
                match evt {
                    StreamEvent::MessageStart { input_tokens: it } => {
                        input_tokens = it;
                    }
                    StreamEvent::Thinking(t) => {
                        if !in_thinking {
                            in_thinking = true;
                            current_thinking.clear();
                            current_thinking_signature.clear();
                        }
                        current_thinking.push_str(&t);
                        let _ = event_tx.send(AgentEvent::Thinking(t));
                    }
                    StreamEvent::ThinkingSignature(s) => {
                        current_thinking_signature.push_str(&s);
                    }
                    StreamEvent::Text(t) => {
                        if !in_text && !in_tool {
                            in_text = true;
                            current_text.clear();
                        }
                        if in_text {
                            current_text.push_str(&t);
                            let _ = event_tx.send(AgentEvent::Text(t));
                        }
                    }
                    StreamEvent::ToolUseStart { id, name } => {
                        // map claude code tool names back to our lowercase names
                        let name = from_cc_name(&name).to_string();
                        in_tool = true;
                        in_text = false;
                        current_tool_id = id.clone();
                        current_tool_name = name.clone();
                        current_tool_input_json.clear();
                        let _ = event_tx.send(AgentEvent::ToolStart { id, name });
                    }
                    StreamEvent::ToolUseInput(json) => {
                        current_tool_input_json.push_str(&json);
                        let _ = event_tx.send(AgentEvent::ToolInputDelta(json));
                    }
                    StreamEvent::ContentBlockStop => {
                        if in_thinking {
                            let sig = if current_thinking_signature.is_empty() {
                                None
                            } else {
                                Some(current_thinking_signature.clone())
                            };
                            response_blocks.push(ContentBlock::Thinking {
                                thinking: current_thinking.clone(),
                                signature: sig,
                            });
                            in_thinking = false;
                        } else if in_tool {
                            let input: serde_json::Value =
                                serde_json::from_str(&current_tool_input_json)
                                    .unwrap_or(serde_json::Value::Object(Default::default()));

                            response_blocks.push(ContentBlock::ToolUse {
                                id: current_tool_id.clone(),
                                name: current_tool_name.clone(),
                                input: input.clone(),
                            });

                            // skip tool execution if cancelled
                            if self.cancel.is_cancelled() {
                                in_tool = false;
                                continue;
                            }

                            // spawn a task that forwards raw bash output chunks to
                            // ToolOutputDelta events; for non-bash tools this is a
                            // no-op since stream_tx is never written to.
                            let (stream_tx, mut stream_rx) =
                                mpsc::unbounded_channel::<String>();
                            let event_tx_fwd = event_tx.clone();
                            let fwd_tool_id = current_tool_id.clone();
                            let forward_handle = tokio::spawn(async move {
                                while let Some(text) = stream_rx.recv().await {
                                    let _ = event_tx_fwd.send(AgentEvent::ToolOutputDelta {
                                        id: fwd_tool_id.clone(),
                                        text,
                                    });
                                }
                            });

                            let result = tools::execute_tool(
                                &current_tool_name,
                                &input,
                                &self.cwd,
                                Some(stream_tx),
                            )
                            .await;

                            // wait for all deltas to be forwarded before ToolComplete
                            forward_handle.await.ok();

                            tool_results.insert(current_tool_id.clone(), result.clone());

                            let _ = event_tx.send(AgentEvent::ToolComplete {
                                id: current_tool_id.clone(),
                                name: current_tool_name.clone(),
                                result,
                            });

                            in_tool = false;
                        } else if in_text {
                            response_blocks.push(ContentBlock::Text {
                                text: current_text.clone(),
                            });
                            in_text = false;
                        }
                    }
                    StreamEvent::MessageDelta { stop_reason: sr, output_tokens: ot } => {
                        stop_reason = sr;
                        output_tokens = ot;
                    }
                    StreamEvent::MessageDone => {}
                    StreamEvent::Error(e) => {
                        stream_errors.push(e);
                    }
                }
            }

            let _ = event_tx.send(AgentEvent::TokenUsage {
                input_tokens,
                output_tokens,
            });

            // bail out if cancelled, preserving any completed work
            if self.cancel.is_cancelled() {
                // finalize any in-progress content blocks from the stream
                if in_thinking && !current_thinking.is_empty() {
                    let sig = if current_thinking_signature.is_empty() {
                        None
                    } else {
                        Some(current_thinking_signature.clone())
                    };
                    response_blocks.push(ContentBlock::Thinking {
                        thinking: current_thinking.clone(),
                        signature: sig,
                    });
                }
                if in_text && !current_text.is_empty() {
                    response_blocks.push(ContentBlock::Text {
                        text: current_text.clone(),
                    });
                }

                // save the partial assistant response and provide cancel
                // results for any tool_use blocks missing results
                if !response_blocks.is_empty() {
                    let tool_use_ids: Vec<String> = response_blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                            _ => None,
                        })
                        .collect();

                    self.messages.push(Message {
                        role: "assistant".to_string(),
                        content: MessageContent::Blocks(response_blocks),
                    });

                    if !tool_use_ids.is_empty() {
                        let mut result_blocks = Vec::new();
                        for id in &tool_use_ids {
                            if let Some(result) = tool_results.remove(id.as_str()) {
                                let (content, is_error) = match result {
                                    ToolResult::Success { output, .. } => (output, None),
                                    ToolResult::Error(e) => (e, Some(true)),
                                };
                                result_blocks.push(ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content,
                                    is_error,
                                });
                            } else {
                                result_blocks.push(ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: "cancelled by user".to_string(),
                                    is_error: Some(true),
                                });
                            }
                        }
                        self.messages.push(Message {
                            role: "user".to_string(),
                            content: MessageContent::Blocks(result_blocks),
                        });
                    }
                }

                let _ = event_tx.send(AgentEvent::TurnComplete);
                break;
            }

            // retry on transient errors when no content was produced
            if response_blocks.is_empty() && stop_reason.is_none() {
                let has_retryable = stream_errors.iter().any(|e| is_retryable_error(e));
                let has_non_retryable = stream_errors.iter().any(|e| !is_retryable_error(e));
                let silent_failure = stream_errors.is_empty();

                if retries < 3 && !has_non_retryable && (has_retryable || silent_failure) {
                    retries += 1;
                    let delay = 1u64 << retries.min(4);
                    let msg = stream_errors.first()
                        .map(|e| e.chars().take(80).collect::<String>())
                        .unwrap_or_else(|| "no response".to_string());
                    let _ = event_tx.send(AgentEvent::Status(
                        format!("retrying in {delay}s ({retries}/3): {msg}"),
                    ));
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    continue;
                }

                // retries exhausted or non-retryable error
                for e in &stream_errors {
                    let _ = event_tx.send(AgentEvent::Error(e.clone()));
                }
                if stream_errors.is_empty() {
                    let _ = event_tx.send(AgentEvent::Error(
                        "stream ended without a response".to_string(),
                    ));
                }
                let _ = event_tx.send(AgentEvent::TurnComplete);
                break;
            }

            // forward any errors that occurred alongside a partial response
            for e in &stream_errors {
                let _ = event_tx.send(AgentEvent::Error(e.clone()));
            }

            // successful response resets retry counter
            retries = 0;

            // add assistant message
            self.messages.push(Message {
                role: "assistant".to_string(),
                content: MessageContent::Blocks(response_blocks.clone()),
            });

            // if the model used tools, add cached results and continue the loop
            let tool_uses: Vec<&ContentBlock> = response_blocks
                .iter()
                .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
                .collect();

            if tool_uses.is_empty() || stop_reason.as_deref() != Some("tool_use") {
                let _ = event_tx.send(AgentEvent::TurnComplete);
                break;
            }

            let mut result_blocks = Vec::new();
            for block in &tool_uses {
                if let ContentBlock::ToolUse { id, .. } = block {
                    let cached = tool_results.remove(id.as_str());
                    let (content, is_error) = match cached {
                        Some(ToolResult::Success { output, .. }) => (output, None),
                        Some(ToolResult::Error(e)) => (e, Some(true)),
                        None => ("tool result missing".to_string(), Some(true)),
                    };
                    result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content,
                        is_error,
                    });
                }
            }

            self.messages.push(Message {
                role: "user".to_string(),
                content: MessageContent::Blocks(result_blocks),
            });
        }

        Ok(())
    }
}

// standalone streaming function that owns all its data
// claude code tool name casing for oauth stealth mode
fn to_cc_name(name: &str) -> &str {
    match name {
        "read" => "Read",
        "write" => "Write",
        "edit" => "Edit",
        "bash" => "Bash",
        "web_search" => "WebSearch",
        _ => name,
    }
}

fn from_cc_name(name: &str) -> &str {
    match name {
        "Read" => "read",
        "Write" => "write",
        "Edit" => "edit",
        "Bash" => "bash",
        "WebSearch" => "web_search",
        _ => name,
    }
}

fn clean_thinking_blocks(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .map(|m| {
            let content = match m.content {
                MessageContent::Blocks(blocks) => {
                    let filtered: Vec<ContentBlock> = blocks
                        .into_iter()
                        .filter(|b| match b {
                            ContentBlock::Thinking { signature, .. } => signature.is_some(),
                            _ => true,
                        })
                        .collect();
                    // if all blocks were unsigned thinking, keep an empty text block
                    // so the message content array is not empty
                    if filtered.is_empty() {
                        MessageContent::Blocks(vec![ContentBlock::Text {
                            text: String::new(),
                        }])
                    } else {
                        MessageContent::Blocks(filtered)
                    }
                }
                other => other,
            };
            Message { role: m.role, content }
        })
        .collect()
}

fn is_retryable_error(e: &str) -> bool {
    e.contains("429")
        || e.contains("rate_limit")
        || e.contains("overloaded")
        || e.contains("529")
        || e.contains("stream error")
        || e.contains("stream read error")
        || e.contains("connection")
        || e.contains("timed out")
}

fn is_adaptive_model(model: &str) -> bool {
    model.contains("opus-4-5") || model.contains("opus-4-6")
        || model.contains("sonnet-4-5") || model.contains("sonnet-4-6")
        || model.contains("haiku-4-5")
}

async fn stream_request(
    client: &reqwest::Client,
    auth: &crate::api::AuthMethod,
    base_url: &str,
    model: &str,
    messages: &[Message],
    system: &str,
    thinking_level: &str,
    tools_json: &[serde_json::Value],
    tx: mpsc::UnboundedSender<StreamEvent>,
) -> Result<()> {
    use crate::api::{AuthMethod, MessagesRequest, OutputConfig, ThinkingConfig};
    use futures::StreamExt;

    if matches!(auth, AuthMethod::None) {
        let _ = tx.send(StreamEvent::Error(
            "no credentials found. use /login to authenticate.".to_string(),
        ));
        return Ok(());
    }

    let is_oauth = matches!(auth, AuthMethod::Bearer(_));

    // adaptive thinking for opus 4.6+, budget-based for older models
    let (thinking, output_config, max_tokens) = if is_adaptive_model(model) && thinking_level != "off" {
        let effort = match thinking_level {
            "minimal" | "low" => "low",
            "medium" => "medium",
            "high" => "high",
            "xhigh" => "max",
            _ => "high",
        };
        (
            Some(ThinkingConfig::Adaptive {
                thinking_type: "adaptive".to_string(),
            }),
            Some(OutputConfig { effort: effort.to_string() }),
            16384,
        )
    } else {
        let thinking_budget = match thinking_level {
            "minimal" => Some(1024u32),
            "low" => Some(4096),
            "medium" => Some(10240),
            "high" => Some(32768),
            "xhigh" => Some(65536),
            _ => None,
        };

        let thinking = thinking_budget.map(|budget| ThinkingConfig::Budget {
            thinking_type: "enabled".to_string(),
            budget_tokens: budget,
        });

        // max_tokens must be strictly greater than the thinking budget
        let max_tokens = match thinking_budget {
            Some(budget) => budget + 16384,
            None => 8192,
        };

        (thinking, None, max_tokens)
    };

    // oauth requires claude code identity in system prompt
    let system_value = if is_oauth {
        serde_json::json!([
            { "type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude." },
            { "type": "text", "text": system }
        ])
    } else {
        serde_json::Value::String(system.to_string())
    };

    // oauth requires claude code tool name casing
    let tools = if is_oauth {
        tools_json.iter().map(|t| {
            let mut t = t.clone();
            if let Some(name) = t.get("name").and_then(|n| n.as_str()) {
                t["name"] = serde_json::Value::String(to_cc_name(name).to_string());
            }
            t
        }).collect()
    } else {
        tools_json.to_vec()
    };

    // remap tool names in conversation history for oauth
    let messages = if is_oauth {
        messages.iter().map(|m| {
            let content = match &m.content {
                MessageContent::Blocks(blocks) => {
                    MessageContent::Blocks(blocks.iter().map(|b| match b {
                        ContentBlock::ToolUse { id, name, input } => ContentBlock::ToolUse {
                            id: id.clone(),
                            name: to_cc_name(name).to_string(),
                            input: input.clone(),
                        },
                        other => other.clone(),
                    }).collect())
                }
                other => other.clone(),
            };
            Message { role: m.role.clone(), content }
        }).collect::<Vec<_>>()
    } else {
        messages.to_vec()
    };

    // the api requires a signature on every thinking block in conversation
    // history. blocks without signatures came from cancelled streams and
    // must be stripped before sending.
    let messages = clean_thinking_blocks(messages);

    let request = MessagesRequest {
        model: model.to_string(),
        max_tokens,
        system: system_value,
        thinking,
        output_config,
        tools,
        messages,
        stream: true,
    };

    let mut headers = reqwest::header::HeaderMap::new();
    match auth {
        AuthMethod::ApiKey(key) => {
            headers.insert("x-api-key", key.parse()?);
        }
        AuthMethod::Bearer(token) => {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", token).parse()?,
            );
        }
        AuthMethod::None => unreachable!("none auth filtered above"),
    }
    headers.insert("anthropic-version", "2023-06-01".parse()?);
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        "application/json".parse()?,
    );

    // build beta features list
    let mut beta_features = Vec::new();
    if is_oauth {
        beta_features.push("claude-code-20250219");
        beta_features.push("oauth-2025-04-20");
        headers.insert(
            reqwest::header::USER_AGENT,
            "claude-cli/2.1.2 (external, cli)".parse()?,
        );
        headers.insert("x-app", "cli".parse()?);
    }
    if request.thinking.is_some() {
        beta_features.push("interleaved-thinking-2025-05-14");
    }
    if !beta_features.is_empty() {
        headers.insert("anthropic-beta", beta_features.join(",").parse()?);
    }

    let response = client
        .post(format!("{}/v1/messages", base_url))
        .headers(headers)
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let _ = tx.send(StreamEvent::Error(format!("api error ({}): {}", status, body)));
        return Ok(());
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(StreamEvent::Error(
                    format!("stream read error: {}", e),
                ));
                return Ok(());
            }
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(pos) = buffer.find("\n\n") {
            let event_text = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();

            if let Some(evt) = crate::api::parse_sse_event(&event_text) {
                if tx.send(evt).is_err() {
                    return Ok(());
                }
            }
        }
    }

    if !buffer.trim().is_empty() {
        if let Some(evt) = crate::api::parse_sse_event(&buffer) {
            let _ = tx.send(evt);
        }
    }

    Ok(())
}
