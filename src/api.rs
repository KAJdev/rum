use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::config::{AuthEntry, Config};
use crate::tools;

#[derive(Debug, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub system: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    pub tools: Vec<serde_json::Value>,
    pub messages: Vec<Message>,
    pub stream: bool,
}

#[derive(Debug, Serialize)]
pub struct ThinkingConfig {
    #[serde(rename = "type")]
    pub thinking_type: String,
    pub budget_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Thinking(String),
    ThinkingSignature(String),
    Text(String),
    ToolUseStart { id: String, name: String },
    ToolUseInput(String),
    ContentBlockStop,
    MessageDelta { stop_reason: Option<String> },
    MessageStart { input_tokens: u32 },
    MessageDone { output_tokens: u32 },
    Error(String),
}

pub struct ApiClient {
    api_key: String,
    model: String,
    base_url: String,
}

impl ApiClient {
    pub fn new(config: &Config) -> Result<Self> {
        let api_key = if let Some(ref key) = config.api_key {
            key.clone()
        } else if let Some(AuthEntry::OAuth { ref access, .. }) = config.auth_entry {
            access.clone()
        } else if let Some(AuthEntry::ApiKey { ref key }) = config.auth_entry {
            key.clone()
        } else {
            bail!(
                "no api key found for provider '{}'. set the appropriate env var or run `pi` and `/login`.",
                config.provider
            );
        };

        let base_url = match config.provider.as_str() {
            "anthropic" => "https://api.anthropic.com".to_string(),
            other => bail!("provider '{}' not yet supported in rum", other),
        };

        Ok(Self {
            api_key,
            model: config.model.clone(),
            base_url,
        })
    }

    pub fn model_clone(&self) -> String {
        self.model.clone()
    }

    pub fn api_key_clone(&self) -> String {
        self.api_key.clone()
    }

    pub fn base_url_clone(&self) -> String {
        self.base_url.clone()
    }

    pub fn build_tools_json(&self) -> Vec<serde_json::Value> {
        tools::tool_definitions()
            .into_iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect()
    }
}

pub fn parse_sse_event(text: &str) -> Option<StreamEvent> {
    let mut event_type = String::new();
    let mut data = String::new();

    for line in text.lines() {
        if let Some(val) = line.strip_prefix("event: ") {
            event_type = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("data: ") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(val);
        }
    }

    if data.is_empty() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_str(&data).ok()?;

    match event_type.as_str() {
        "message_start" => {
            let input_tokens = json
                .pointer("/message/usage/input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            Some(StreamEvent::MessageStart { input_tokens })
        }
        "content_block_start" => {
            let block = json.get("content_block")?;
            let block_type = block.get("type")?.as_str()?;
            match block_type {
                "tool_use" => {
                    let id = block.get("id")?.as_str()?.to_string();
                    let name = block.get("name")?.as_str()?.to_string();
                    Some(StreamEvent::ToolUseStart { id, name })
                }
                "thinking" => Some(StreamEvent::Thinking(String::new())),
                "text" => Some(StreamEvent::Text(String::new())),
                _ => None,
            }
        }
        "content_block_delta" => {
            let delta = json.get("delta")?;
            let delta_type = delta.get("type")?.as_str()?;
            match delta_type {
                "text_delta" => {
                    let text = delta.get("text")?.as_str()?.to_string();
                    Some(StreamEvent::Text(text))
                }
                "thinking_delta" => {
                    let thinking = delta.get("thinking")?.as_str()?.to_string();
                    Some(StreamEvent::Thinking(thinking))
                }
                "signature_delta" => {
                    let sig = delta.get("signature")?.as_str()?.to_string();
                    Some(StreamEvent::ThinkingSignature(sig))
                }
                "input_json_delta" => {
                    let partial = delta.get("partial_json")?.as_str()?.to_string();
                    Some(StreamEvent::ToolUseInput(partial))
                }
                _ => None,
            }
        }
        "content_block_stop" => Some(StreamEvent::ContentBlockStop),
        "message_delta" => {
            let stop_reason = json
                .pointer("/delta/stop_reason")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(StreamEvent::MessageDelta { stop_reason })
        }
        "message_stop" => {
            let output_tokens = json
                .pointer("/usage/output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            Some(StreamEvent::MessageDone { output_tokens })
        }
        "error" => {
            let msg = json
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string();
            Some(StreamEvent::Error(msg))
        }
        "ping" => None,
        _ => None,
    }
}
