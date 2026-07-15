pub mod anthropic;
pub mod common;
pub mod cursor;
pub mod openai;
pub mod volcengine;

use crate::config::ProviderKind;
use crate::function::notifications::ModelInfo;
use crate::model_data;
use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
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
    /// Incremental tool-call arguments delta. Emitted during the LLM
    /// stream as the provider accumulates partial JSON fragments for
    /// a tool call. `index` identifies the tool call slot,
    /// `call_id` is the stable tool-call id (available from the first
    /// delta), `name` is the tool name, `args` is the full accumulated
    /// arguments string so far (may be partial/invalid JSON).
    ToolArgDelta {
        index: usize,
        call_id: String,
        name: String,
        args: String,
    },
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
            ChatEvent::ContentBlockStart(kind) => crate::event::AppMsg::ChatContentBlockStart(kind),
            ChatEvent::ToolArgDelta {
                index,
                call_id,
                name,
                args,
                ..
            } => crate::event::AppMsg::ToolInputDelta {
                index,
                call_id,
                name,
                args,
            },
            ChatEvent::Debug(s) => crate::event::AppMsg::ChatDebug(s),
            ChatEvent::ToolResult {
                name,
                title,
                content,
            } => crate::event::AppMsg::ChatToolResult {
                name,
                title,
                content,
                metadata: String::new(),
                call_id: String::new(),
                failed: false,
            },
            ChatEvent::ToolCalls(_) => crate::event::AppMsg::ChatDone { seq },
            ChatEvent::Usage(u) => crate::event::AppMsg::ChatUsage { seq, usage: u },
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
    /// Optional custom tool specs. When `None`, the provider uses the
    /// global `openai_tool_specs()` / `anthropic_tool_specs()`. When
    /// `Some`, the provider uses these instead. This is used by the
    /// sub-agent tool to pass filtered tool specs.
    pub tools: Option<Vec<serde_json::Value>>,
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
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sse error: {0}")]
    Sse(String),
    #[error("other: {0}")]
    Other(String),
}

pub struct ListModelsArgs<'a> {
    pub client: &'a reqwest::Client,
    pub kind: ProviderKind,
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub cache_path: &'a Path,
    pub provider_name: &'a str,
}

pub async fn list_models(args: ListModelsArgs<'_>) -> Result<Vec<ModelInfo>> {
    let p = make_list_provider(args.kind);
    let mut models = p
        .list_models(
            args.client,
            args.base_url,
            args.api_key,
            args.access_key,
            args.secret_key,
        )
        .await?;
    fill_context_windows(
        args.client,
        args.provider_name,
        &mut models,
        args.cache_path,
    )
    .await;
    Ok(models)
}

/// Fill context_window_tokens in models using models.dev data.
/// If `cache_path` is None, uses a default path in the config directory.
/// Models that are not found in models.dev keep their existing
/// context_window_tokens (which may be None).
pub async fn fill_context_windows(
    client: &reqwest::Client,
    provider_name: &str,
    models: &mut [ModelInfo],
    cache_path: &Path,
) {
    let clean_name = provider_name.to_lowercase();

    let model_data_path = cache_path.join("model-data.json");
    let custom_cache_path = cache_path.join("context-cache.json");

    let model_data = match model_data::fetch_models_dev(client, &model_data_path).await {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!("models.dev fetch failed: {e}");
            // Fall back to stale cache.
            model_data::ModelData::load(&model_data_path).unwrap_or_else(|| model_data::ModelData {
                models: std::collections::HashMap::new(),
                fetched_at: chrono::Utc::now(),
            })
        }
    };

    let custom_cache = model_data::CustomContextCache::load(&custom_cache_path);

    for model in models.iter_mut() {
        if model.context_window_tokens.is_some() {
            continue;
        }
        // Try custom cache first
        if let Some(ctx) = custom_cache.get(&model.id) {
            model.context_window_tokens = Some(ctx);
            continue;
        }
        // Try models.dev
        if let Some(ctx) = model_data.lookup(&clean_name, &model.id) {
            model.context_window_tokens = Some(ctx);
        }
    }
}

pub fn provider(kind: ProviderKind) -> Box<dyn Provider> {
    make_chat_provider(kind)
}

/// Construct a boxed `Provider` for the given `ProviderKind` for chat
/// streaming. Shared by `provider()`.
///
/// Note: `Volcengine` maps to `OpenAiProvider` here because chat uses
/// the OpenAI-compatible `/chat/completions` endpoint. Model listing
/// (`list_models`) uses `VolcengineProvider` which needs V4 signing.
fn make_chat_provider(kind: ProviderKind) -> Box<dyn Provider> {
    match kind {
        ProviderKind::Openai
        | ProviderKind::DeepSeek
        | ProviderKind::MiniMax
        | ProviderKind::Volcengine => Box::new(openai::OpenAiProvider),
        ProviderKind::Anthropic => Box::new(anthropic::AnthropicProvider),
        ProviderKind::Cursor => Box::new(cursor::CursorProvider),
    }
}

/// Construct a boxed `Provider` for the given `ProviderKind` for model
/// listing. `Volcengine` maps to `VolcengineProvider` (needs V4 signing)
/// unlike `make_chat_provider` which maps it to `OpenAiProvider`.
fn make_list_provider(kind: ProviderKind) -> Box<dyn Provider> {
    match kind {
        ProviderKind::Openai | ProviderKind::DeepSeek | ProviderKind::MiniMax => {
            Box::new(openai::OpenAiProvider)
        }
        ProviderKind::Anthropic => Box::new(anthropic::AnthropicProvider),
        ProviderKind::Cursor => Box::new(cursor::CursorProvider),
        ProviderKind::Volcengine => Box::new(volcengine::VolcengineProvider),
    }
}
