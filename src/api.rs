use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::tools;

#[derive(Debug, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub system: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<ContextManagement>,
    pub tools: Vec<serde_json::Value>,
    pub messages: Vec<Message>,
    pub stream: bool,
}

#[derive(Debug, Serialize)]
pub struct ContextManagement {
    pub edits: Vec<CompactEdit>,
}

#[derive(Debug, Serialize)]
pub struct CompactEdit {
    #[serde(rename = "type")]
    pub edit_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<TriggerConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause_after_compaction: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TriggerConfig {
    #[serde(rename = "type")]
    pub trigger_type: String,
    pub value: u32,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ThinkingConfig {
    Budget {
        #[serde(rename = "type")]
        thinking_type: String,
        budget_tokens: u32,
    },
    Adaptive {
        #[serde(rename = "type")]
        thinking_type: String,
    },
}

#[derive(Debug, Serialize)]
pub struct OutputConfig {
    pub effort: String,
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
        // string for plain text results; array of {type,text}/{type,image} blocks
        // for results that include image content
        content: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    // summary block produced by server-side context compaction.
    // must be passed back on subsequent requests so the api can ignore
    // the messages that preceded it.
    #[serde(rename = "compaction")]
    Compaction { content: String },
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Thinking(String),
    ThinkingSignature(String),
    Text(String),
    ToolUseStart {
        id: String,
        name: String,
    },
    ToolUseInput(String),
    ContentBlockStop,
    CompactionStart,
    CompactionDelta(String),
    MessageDelta {
        stop_reason: Option<String>,
        output_tokens: u32,
    },
    MessageStart {
        input_tokens: u32,
        cache_read_tokens: u32,
        cache_creation_tokens: u32,
    },
    MessageDone,
    Error(String),
}

// whether the credential is an oauth bearer token or a raw api key.
// oauth tokens use Authorization: Bearer, api keys use x-api-key.
#[derive(Debug, Clone)]
pub enum AuthMethod {
    ApiKey(String),
    Bearer(String),
    None,
}

pub struct ApiClient {
    pub auth: AuthMethod,
    model: String,
    base_url: String,
}

impl ApiClient {
    pub fn new(config: &Config) -> Result<Self> {
        let auth = if let Some(ref key) = config.api_key {
            // env var api keys always use x-api-key
            AuthMethod::ApiKey(key.clone())
        } else if let Some(ref creds) = config.oauth {
            AuthMethod::Bearer(creds.access.clone())
        } else {
            AuthMethod::None
        };

        Ok(Self {
            auth,
            model: config.model.clone(),
            base_url: "https://api.anthropic.com".to_string(),
        })
    }

    pub fn set_bearer(&mut self, token: String) {
        self.auth = AuthMethod::Bearer(token);
    }

    pub fn set_model(&mut self, model: &str) {
        self.model = model.to_string();
    }

    pub fn model_clone(&self) -> String {
        self.model.clone()
    }

    pub fn auth_clone(&self) -> AuthMethod {
        self.auth.clone()
    }

    pub fn set_auth(&mut self, auth: AuthMethod) {
        self.auth = auth;
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
            let cache_read_tokens = json
                .pointer("/message/usage/cache_read_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let cache_creation_tokens = json
                .pointer("/message/usage/cache_creation_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            Some(StreamEvent::MessageStart {
                input_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            })
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
                "compaction" => Some(StreamEvent::CompactionStart),
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
                "compaction_delta" => {
                    let content = delta.get("content")?.as_str()?.to_string();
                    Some(StreamEvent::CompactionDelta(content))
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
            let output_tokens = json
                .pointer("/usage/output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            Some(StreamEvent::MessageDelta {
                stop_reason,
                output_tokens,
            })
        }
        "message_stop" => Some(StreamEvent::MessageDone),
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
