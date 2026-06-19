pub mod anthropic;
pub mod cursor;
pub mod openai;

use crate::config::ProviderKind;
use crate::function::notifications::ModelInfo;
use anyhow::Result;
use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub context_window_tokens: Option<u64>,
}

pub enum ChatEvent {
    Delta(String),
    ThinkingDelta(String),
    Debug(String),
    ToolResult {
        name: String,
        title: String,
        content: String,
    },
    ToolCalls(Vec<ToolCall>),
    Usage(Usage),
    Done,
    Error(String),
}

impl ChatEvent {
    /// Convert a ChatEvent into the corresponding AppMsg for the main loop.
    pub fn into_app_msg(self) -> crate::event::AppMsg {
        match self {
            ChatEvent::Delta(s) => crate::event::AppMsg::ChatDelta(s),
            ChatEvent::ThinkingDelta(s) => crate::event::AppMsg::ChatThinkingDelta(s),
            ChatEvent::Debug(s) => crate::event::AppMsg::ChatDebug(s),
            ChatEvent::ToolResult {
                name,
                title,
                content,
            } => crate::event::AppMsg::ChatToolResult {
                name,
                title,
                content,
            },
            ChatEvent::ToolCalls(_) => crate::event::AppMsg::ChatDone,
            ChatEvent::Usage(u) => crate::event::AppMsg::ChatUsage(u),
            ChatEvent::Done => crate::event::AppMsg::ChatDone,
            ChatEvent::Error(e) => crate::event::AppMsg::ChatError(e),
        }
    }
}

pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub thinking: crate::config::ReasoningMode,
    pub system: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    pub tool_call_id: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    async fn list_models(
        &self,
        client: &reqwest::Client,
        base_url: &str,
        api_key: &str,
    ) -> Result<Vec<ModelInfo>>;
    async fn chat_stream(
        &self,
        client: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        req: ChatRequest,
        tx: mpsc::UnboundedSender<ChatEvent>,
    ) -> Result<()>;
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("base_url has no /v1/models endpoint")]
    NoModelsEndpoint,
    #[error("auth failed (status {0})")]
    AuthFailed(u16),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sse error: {0}")]
    Sse(String),
    #[error("other: {0}")]
    Other(String),
}

pub async fn list_models(
    client: &reqwest::Client,
    kind: ProviderKind,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>> {
    let p: Box<dyn Provider> = match kind {
        ProviderKind::Openai => Box::new(openai::OpenAiProvider),
        ProviderKind::Anthropic => Box::new(anthropic::AnthropicProvider),
        ProviderKind::Cursor => Box::new(cursor::CursorProvider),
    };
    p.list_models(client, base_url, api_key).await
}

pub fn provider(kind: ProviderKind) -> Box<dyn Provider> {
    match kind {
        ProviderKind::Openai => Box::new(openai::OpenAiProvider),
        ProviderKind::Anthropic => Box::new(anthropic::AnthropicProvider),
        ProviderKind::Cursor => Box::new(cursor::CursorProvider),
    }
}
