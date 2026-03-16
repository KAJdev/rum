use crate::api::AuthMethod;
use crate::diff::DiffInfo;
use serde::Serialize;

// passed down from the agent so tools can make api calls and send background job events
pub struct ApiContext {
    pub auth: AuthMethod,
    pub base_url: String,
    pub is_oauth: bool,
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub job_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::tui::JobEvent>>,
    pub next_job_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub lsp: Option<std::sync::Arc<tokio::sync::Mutex<crate::lsp::LspManager>>>,
}

impl ApiContext {
    pub(super) fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone)]
pub enum ToolResult {
    Success {
        output: String,
        diff: Option<DiffInfo>,
        // set by the read tool to indicate which file/line was viewed
        read: Option<ReadInfo>,
    },
    // image file read result - base64-encoded data sent to the model as a content block
    Image {
        text: String,
        data: String,
        media_type: String,
    },
    Error(String),
}

#[derive(Debug, Clone)]
pub struct ReadInfo {
    pub path: String,
    // 1-indexed line offset
    pub offset: usize,
}
