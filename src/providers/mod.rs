pub mod anthropic;
pub mod cursor;
pub mod openai;
pub mod volcengine;

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
    /// Fired by the provider when a new content block begins in the
    /// upstream stream (Anthropic `content_block_start`,
    /// OpenAI/Cursor reasoning→text transitions, etc.). The session
    /// uses this to close off the in-flight thinking segment so the
    /// next `ThinkingDelta` lands in a fresh block. The string is
    /// the block kind ("thinking", "text", "tool_use", ...).
    ContentBlockStart(String),
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
    /// Convert a ChatEvent into the corresponding AppMsg for the
    /// main loop. `seq` stamps the request generation onto terminal
    /// events (`ChatDone` / `ChatError`) so stale events from a
    /// previous request can be filtered out by `handle_msg`.
    #[allow(dead_code)]
    pub fn into_app_msg(self, seq: u64) -> crate::event::AppMsg {
        match self {
            ChatEvent::Delta(s) => crate::event::AppMsg::ChatDelta(s),
            ChatEvent::ThinkingDelta(s) => crate::event::AppMsg::ChatThinkingDelta(s),
            ChatEvent::ContentBlockStart(kind) => {
                crate::event::AppMsg::ChatContentBlockStart(kind)
            }
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
            ChatEvent::ToolCalls(_) => crate::event::AppMsg::ChatDone { seq },
            ChatEvent::Usage(u) => crate::event::AppMsg::ChatUsage(u),
            ChatEvent::Done => crate::event::AppMsg::ChatDone { seq },
            ChatEvent::Error(e) => crate::event::AppMsg::ChatError { seq, error: e },
        }
    }
}

pub use crate::session::{ContentPart, ImageAttachment};

#[derive(Debug)]
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
    pub content_parts: Vec<ContentPart>,
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
        access_key: &str,
        secret_key: &str,
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
    access_key: &str,
    secret_key: &str,
) -> Result<Vec<ModelInfo>> {
    let p: Box<dyn Provider> = match kind {
        ProviderKind::Openai => Box::new(openai::OpenAiProvider),
        ProviderKind::Anthropic => Box::new(anthropic::AnthropicProvider),
        ProviderKind::Cursor => Box::new(cursor::CursorProvider),
        ProviderKind::DeepSeek => Box::new(openai::OpenAiProvider),
        ProviderKind::MiniMax => Box::new(openai::OpenAiProvider),
        ProviderKind::Volcengine => Box::new(volcengine::VolcengineProvider),
    };
    p.list_models(client, base_url, api_key, access_key, secret_key)
        .await
}

pub fn provider(kind: ProviderKind) -> Box<dyn Provider> {
    match kind {
        ProviderKind::Openai => Box::new(openai::OpenAiProvider),
        ProviderKind::Anthropic => Box::new(anthropic::AnthropicProvider),
        ProviderKind::Cursor => Box::new(cursor::CursorProvider),
        ProviderKind::DeepSeek => Box::new(openai::OpenAiProvider),
        ProviderKind::MiniMax => Box::new(openai::OpenAiProvider),
        ProviderKind::Volcengine => Box::new(openai::OpenAiProvider),
    }
}
