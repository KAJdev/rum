use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::api::{ApiClient, ContentBlock, Message, MessageContent, StreamEvent};
use crate::config::Config;
use crate::tools::{self, ToolResult};

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
    TokenUsage {
        input_tokens: u32,
        output_tokens: u32,
    },
    TurnComplete,
    Error(String),
}

pub struct Agent {
    client: ApiClient,
    messages: Vec<Message>,
    system_prompt: String,
    thinking_level: String,
    cwd: PathBuf,
}

impl Agent {
    pub fn new(config: &Config, client: ApiClient, cwd: PathBuf) -> Self {
        let mut system = config.system_prompt.clone();
        for ctx in &config.context_files {
            system.push_str("\n\n");
            system.push_str(ctx);
        }

        system.push_str(&format!(
            "\n\nCurrent working directory: {}",
            cwd.display()
        ));

        Self {
            client,
            messages: Vec::new(),
            system_prompt: system,
            thinking_level: config.thinking_level.clone(),
            cwd,
        }
    }

    pub async fn send_message(
        &mut self,
        user_message: &str,
        event_tx: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<()> {
        self.messages.push(Message {
            role: "user".to_string(),
            content: MessageContent::Text(user_message.to_string()),
        });

        self.run_turn(event_tx).await
    }

    async fn run_turn(
        &mut self,
        event_tx: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<()> {
        loop {
            let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<StreamEvent>();

            let messages = self.messages.clone();
            let system = self.system_prompt.clone();
            let thinking = self.thinking_level.clone();

            // spawn streaming in a separate task
            let client_model = self.client.model_clone();
            let client_api_key = self.client.api_key_clone();
            let client_base_url = self.client.base_url_clone();
            let tools_json = self.client.build_tools_json();

            tokio::spawn(async move {
                let client = reqwest::Client::new();
                let _ = stream_request(
                    &client,
                    &client_api_key,
                    &client_base_url,
                    &client_model,
                    &messages,
                    &system,
                    &thinking,
                    &tools_json,
                    stream_tx,
                )
                .await;
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

                            // execute the tool once, cache result
                            let result = tools::execute_tool(
                                &current_tool_name,
                                &input,
                                &self.cwd,
                            )
                            .await;

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
                        let _ = event_tx.send(AgentEvent::Error(e));
                    }
                }
            }

            let _ = event_tx.send(AgentEvent::TokenUsage {
                input_tokens,
                output_tokens,
            });

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
async fn stream_request(
    client: &reqwest::Client,
    api_key: &str,
    base_url: &str,
    model: &str,
    messages: &[Message],
    system: &str,
    thinking_level: &str,
    tools_json: &[serde_json::Value],
    tx: mpsc::UnboundedSender<StreamEvent>,
) -> Result<()> {
    use crate::api::{MessagesRequest, ThinkingConfig};
    use futures::StreamExt;

    let thinking_budget = match thinking_level {
        "minimal" => Some(1024u32),
        "low" => Some(4096),
        "medium" => Some(10240),
        "high" => Some(32768),
        "xhigh" => Some(65536),
        _ => None,
    };

    let thinking = thinking_budget.map(|budget| ThinkingConfig {
        thinking_type: "enabled".to_string(),
        budget_tokens: budget,
    });

    // max_tokens must be strictly greater than the thinking budget
    let max_tokens = match thinking_budget {
        Some(budget) => budget + 16384,
        None => 8192,
    };

    let request = MessagesRequest {
        model: model.to_string(),
        max_tokens,
        system: system.to_string(),
        thinking,
        tools: tools_json.to_vec(),
        messages: messages.to_vec(),
        stream: true,
    };

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-api-key", api_key.parse()?);
    headers.insert("anthropic-version", "2023-06-01".parse()?);
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        "application/json".parse()?,
    );

    if request.thinking.is_some() {
        headers.insert(
            "anthropic-beta",
            "interleaved-thinking-2025-05-14".parse()?,
        );
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
        let chunk = chunk?;
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
