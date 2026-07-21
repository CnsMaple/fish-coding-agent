use self::exec::handle_exec_server_message;
use self::proto::*;
use super::{ChatEvent, ChatRequest, Provider, ProviderError, Usage};
use crate::config::ProviderKind;
use crate::function::notifications::ModelInfo;
use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;
use prost::Message;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;

mod auth;
mod exec;
mod proto;

pub use auth::{
    generate_auth_params, open_browser, poll_auth, refresh_token, CursorAuthParams,
    CursorAuthTokens,
};

const CURSOR_CLIENT_VERSION: &str = "cli-2026.01.09-231024f";
const CONNECT_END_STREAM_FLAG: u8 = 0b0000_0010;
const CURSOR_MODELS_PATH: &str = "/agent.v1.AgentService/GetUsableModels";
const CURSOR_STREAM_TIMEOUT_SECS: u64 = 120;

pub struct CursorProvider;

#[async_trait]
impl Provider for CursorProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Cursor
    }

    async fn list_models(
        &self,
        _client: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        _access_key: &str,
        _secret_key: &str,
    ) -> Result<Vec<ModelInfo>> {
        let client = cursor_http2_client()?;
        let url = format!("{}{}", base_url.trim_end_matches('/'), CURSOR_MODELS_PATH);
        let req = GetUsableModelsRequest::default();
        let resp = client
            .post(&url)
            .bearer_auth(api_key)
            .header("content-type", "application/proto")
            .header("te", "trailers")
            .header("user-agent", "connect-es/1.6.1")
            .header("x-ghost-mode", "true")
            .header("x-cursor-client-version", CURSOR_CLIENT_VERSION)
            .header("x-cursor-client-type", "cli")
            .header("connect-protocol-version", "1")
            .body(req.encode_to_vec())
            .send()
            .await
            .map_err(ProviderError::Http)?;
        if !resp.status().is_success() {
            return Err(cursor_status_error(resp).await.into());
        }
        let bytes = resp.bytes().await.map_err(ProviderError::Http)?;
        let decoded = decode_get_usable_models_response(bytes.as_ref())?;
        let mut models = Vec::new();
        for m in decoded.models {
            if let Some(info) = model_info_from_cursor_details(m) {
                models.push(info);
            }
        }
        models.sort_by(|a, b| a.id.cmp(&b.id));
        if models.is_empty() {
            return Err(
                ProviderError::Other("Cursor returned no usable models".to_string()).into(),
            );
        }
        Ok(models)
    }

    async fn chat_stream(
        &self,
        _client: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        req: ChatRequest,
        tx: mpsc::UnboundedSender<ChatEvent>,
    ) -> Result<()> {
        if req.model.trim().is_empty() || req.model.trim().eq_ignore_ascii_case("auto") {
            return Err(ProviderError::Other(
                "Cursor model is not selected. Open /model and choose a Cursor model after OAuth."
                    .to_string(),
            )
            .into());
        }
        let url = format!(
            "{}/agent.v1.AgentService/Run",
            base_url.trim_end_matches('/')
        );
        cursor_debug(
            &tx,
            format!("request start model={} url={}", req.model, url),
        );
        // Check for image attachments that Cursor's protocol does not support.
        let image_count: usize = req
            .messages
            .iter()
            .flat_map(|m| m.content_parts.iter())
            .filter(|p| matches!(p, super::ContentPart::Image(_)))
            .count();
        if image_count > 0 {
            cursor_debug(
                &tx,
                format!(
                    "dropping {image_count} image(s): cursor protocol has no image attachment field"
                ),
            );
        }
        let (request, mut blob_store) = build_request(req);
        let bytes = request.encode_to_vec();
        let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        body_tx
            .send(frame_connect_message(&bytes, 0))
            .await
            .map_err(|_| ProviderError::Other("Cursor request stream closed".to_string()))?;
        let body_stream = futures_util::stream::unfold(body_rx, |mut rx| async {
            rx.recv()
                .await
                .map(|chunk| (Ok::<_, std::io::Error>(chunk), rx))
        });
        let heartbeat_tx = body_tx.clone();
        let heartbeat = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(5));
            loop {
                ticker.tick().await;
                let heartbeat = AgentClientMessage {
                    message: Some(agent_client_message::Message::ClientHeartbeat(
                        ClientHeartbeat::default(),
                    )),
                };
                if heartbeat_tx
                    .send(frame_connect_message(&heartbeat.encode_to_vec(), 0))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let client = cursor_http2_client()?;
        let request = client
            .post(&url)
            .bearer_auth(api_key)
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("te", "trailers")
            .header("x-ghost-mode", "true")
            .header("x-cursor-client-version", CURSOR_CLIENT_VERSION)
            .header("x-cursor-client-type", "cli")
            .header("x-request-id", uuid::Uuid::new_v4().to_string())
            .body(reqwest::Body::wrap_stream(body_stream));
        let resp = match tokio::time::timeout(Duration::from_secs(30), request.send()).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                heartbeat.abort();
                return Err(ProviderError::Http(e).into());
            }
            Err(_) => {
                heartbeat.abort();
                return Err(ProviderError::Other(
                    "Cursor request timed out waiting for response headers".to_string(),
                )
                .into());
            }
        };
        cursor_debug(&tx, format!("response headers status={}", resp.status()));
        if !resp.status().is_success() {
            heartbeat.abort();
            return Err(cursor_status_error(resp).await.into());
        }
        let mut stream = resp.bytes_stream();
        let mut pending = Vec::<u8>::new();
        let mut usage = Usage::default();
        // Tracks the kind of the most recent block we emitted a
        // delta for so the provider can fire a `ContentBlockStart`
        // event when the upstream switches between thinking and
        // text deltas. Without this the session would merge every
        // reasoning segment that shares the same content offset
        // into a single block.
        let mut last_block_kind: Option<&'static str> = None;
        loop {
            let Some(chunk) = (match tokio::time::timeout(
                Duration::from_secs(CURSOR_STREAM_TIMEOUT_SECS),
                stream.next(),
            )
            .await
            {
                Ok(next) => next,
                Err(_) => {
                    heartbeat.abort();
                    return Err(ProviderError::Other(
                        "Cursor stream timed out waiting for turnEnded".to_string(),
                    )
                    .into());
                }
            }) else {
                break;
            };
            let chunk = chunk.map_err(|e| {
                let preview = if pending.len() > 5 {
                    let len = u32::from_be_bytes([pending[1], pending[2], pending[3], pending[4]])
                        as usize;
                    format!(" pending_frame_len={} pending_bytes={}", len, pending.len())
                } else {
                    String::new()
                };
                ProviderError::Other(format!("Cursor response body decode: {e}{preview}"))
            })?;
            pending.extend_from_slice(&chunk);
            while pending.len() >= 5 {
                let flags = pending[0];
                let len =
                    u32::from_be_bytes([pending[1], pending[2], pending[3], pending[4]]) as usize;
                if pending.len() < 5 + len {
                    break;
                }
                let msg = pending[5..5 + len].to_vec();
                pending.drain(..5 + len);
                if flags & CONNECT_END_STREAM_FLAG != 0 {
                    cursor_debug(&tx, format!("connect end-stream frame len={len}"));
                    continue;
                }
                if let Ok(server) = AgentServerMessage::decode(msg.as_slice()) {
                    match handle_server_message(
                        server,
                        &tx,
                        &mut usage,
                        &body_tx,
                        &mut blob_store,
                        &mut last_block_kind,
                    )
                    .await?
                    {
                        CursorServerOutcome::Done => {
                            cursor_debug(&tx, "finish: done event");
                            finish_cursor_stream(&tx, &usage, &heartbeat);
                            return Ok(());
                        }
                        CursorServerOutcome::TextOutput
                        | CursorServerOutcome::ToolOutput
                        | CursorServerOutcome::Meaningful
                        | CursorServerOutcome::StepCompleted
                        | CursorServerOutcome::Checkpoint
                        | CursorServerOutcome::KvServerMessage
                        | CursorServerOutcome::Heartbeat
                        | CursorServerOutcome::Continue => {}
                    }
                }
            }
        }
        cursor_debug(&tx, "finish: response body ended");
        finish_cursor_stream(&tx, &usage, &heartbeat);
        Ok(())
    }
}

pub(super) fn cursor_debug(tx: &mpsc::UnboundedSender<ChatEvent>, message: impl Into<String>) {
    let _ = tx.send(ChatEvent::Debug(format!("cursor: {}", message.into())));
}

fn finish_cursor_stream(
    tx: &mpsc::UnboundedSender<ChatEvent>,
    usage: &Usage,
    heartbeat: &tokio::task::JoinHandle<()>,
) {
    if usage.input_tokens
        + usage.output_tokens
        + usage.cache_read_tokens
        + usage.cache_creation_tokens
        > 0
    {
        let _ = tx.send(ChatEvent::Usage(usage.clone()));
    }
    heartbeat.abort();
    let _ = tx.send(ChatEvent::Done);
}

fn cursor_http2_client() -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(ProviderError::Http)
}

async fn cursor_status_error(resp: reqwest::Response) -> ProviderError {
    let status = resp.status();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| format!("{}={}", k.as_str(), v.to_str().unwrap_or("<binary>")))
        .collect::<Vec<_>>()
        .join(", ");
    let text = resp.text().await.unwrap_or_default();
    ProviderError::Other(format!(
        "Cursor status {status}; headers: [{headers}]; body: {text}"
    ))
}

fn build_request(req: ChatRequest) -> (AgentClientMessage, HashMap<Vec<u8>, Vec<u8>>) {
    let mut blob_store = HashMap::new();
    let mut root_prompt_messages_json = Vec::new();

    // System prompt blob
    let system = req
        .system
        .clone()
        .unwrap_or_else(|| "You are a helpful assistant.".to_string());
    let system_blob = serde_json::json!({
        "role": "system",
        "content": system,
    })
    .to_string()
    .into_bytes();
    let system_blob_id = create_blob_id(&system_blob);
    blob_store.insert(system_blob_id.clone(), system_blob);
    root_prompt_messages_json.push(system_blob_id);

    // Find the index of the last user message — that message is sent
    // as the `UserMessageAction` (the trigger), so it must NOT also
    // appear in `root_prompt_messages_json` (otherwise the server
    // would see it duplicated).
    let last_user_idx = req.messages.iter().rposition(|m| m.role == "user");

    // Add all prior conversation messages as blobs so the Cursor
    // server has the full context (previous user/assistant turns,
    // tool calls and tool results). Without this, each request looks
    // like a brand-new conversation with only the system prompt.
    for (i, m) in req.messages.iter().enumerate() {
        if Some(i) == last_user_idx {
            continue;
        }
        let blob = message_to_blob_bytes(m);
        let blob_id = create_blob_id(&blob);
        blob_store.insert(blob_id.clone(), blob);
        root_prompt_messages_json.push(blob_id);
    }

    // The last user message becomes the action that triggers the run.
    let current_user = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let user_message = UserMessage {
        text: current_user,
        message_id: uuid::Uuid::new_v4().to_string(),
        ..Default::default()
    };
    let action = ConversationAction {
        action: Some(conversation_action::Action::UserMessageAction(
            UserMessageAction {
                user_message: Some(user_message),
            },
        )),
    };
    let state = ConversationStateStructure {
        root_prompt_messages_json,
        ..Default::default()
    };
    let model = ModelDetails {
        model_id: req.model.clone(),
        display_model_id: req.model.clone(),
        display_name: req.model,
    };
    let run = AgentRunRequest {
        conversation_state: Some(state),
        action: Some(action),
        model_details: Some(model),
        conversation_id: Some(uuid::Uuid::new_v4().to_string()),
        ..Default::default()
    };
    (
        AgentClientMessage {
            message: Some(agent_client_message::Message::RunRequest(run)),
        },
        blob_store,
    )
}

/// Convert a `ChatMessage` into a JSON byte vector suitable for storage
/// as a Cursor protocol blob. Uses OpenAI-style message format so the
/// Cursor server can reconstruct tool calls and tool results.
fn message_to_blob_bytes(m: &super::ChatMessage) -> Vec<u8> {
    if m.role == "tool" {
        return serde_json::json!({
            "role": "tool",
            "tool_call_id": m.tool_call_id,
            "content": m.content,
        })
        .to_string()
        .into_bytes();
    }
    if !m.tool_calls.is_empty() {
        return serde_json::json!({
            "role": m.role,
            "content": if m.content.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(m.content.clone())
            },
            "tool_calls": m.tool_calls.iter().map(|call| serde_json::json!({
                "id": call.id,
                "type": "function",
                "function": {
                    "name": call.name,
                    "arguments": call.arguments,
                }
            })).collect::<Vec<_>>(),
        })
        .to_string()
        .into_bytes();
    }
    serde_json::json!({
        "role": m.role,
        "content": m.content,
    })
    .to_string()
    .into_bytes()
}

fn create_blob_id(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CursorServerOutcome {
    Continue,
    TextOutput,
    ToolOutput,
    Meaningful,
    Checkpoint,
    Heartbeat,
    StepCompleted,
    KvServerMessage,
    Done,
}

pub(super) async fn handle_server_message(
    msg: AgentServerMessage,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    usage: &mut Usage,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    blob_store: &mut HashMap<Vec<u8>, Vec<u8>>,
    last_block_kind: &mut Option<&'static str>,
) -> Result<CursorServerOutcome> {
    match msg.message {
        Some(agent_server_message::Message::InteractionUpdate(update)) => match update.message {
            Some(interaction_update::Message::TextDelta(v)) => {
                if !v.text.is_empty() {
                    cursor_debug(
                        tx,
                        format!("event text_delta chars={}", v.text.chars().count()),
                    );
                    if *last_block_kind == Some("thinking") {
                        let _ = tx.send(ChatEvent::ContentBlockStart("text".to_string()));
                    }
                    *last_block_kind = Some("text");
                    let _ = tx.send(ChatEvent::Delta(v.text));
                    return Ok(CursorServerOutcome::TextOutput);
                }
                Ok(CursorServerOutcome::Continue)
            }
            Some(interaction_update::Message::ThinkingDelta(v)) => {
                if !v.text.is_empty() {
                    cursor_debug(
                        tx,
                        format!("event thinking_delta chars={}", v.text.chars().count()),
                    );
                    if *last_block_kind == Some("text") {
                        let _ = tx.send(ChatEvent::ContentBlockStart("thinking".to_string()));
                    }
                    *last_block_kind = Some("thinking");
                    let _ = tx.send(ChatEvent::ThinkingDelta(v.text));
                    return Ok(CursorServerOutcome::Meaningful);
                }
                Ok(CursorServerOutcome::Continue)
            }
            Some(interaction_update::Message::TokenDelta(v)) => {
                cursor_debug(tx, format!("event token_delta tokens={}", v.tokens));
                usage.output_tokens = usage.output_tokens.saturating_add(v.tokens.max(0) as u64);
                Ok(CursorServerOutcome::Meaningful)
            }
            Some(interaction_update::Message::ThinkingCompleted(_)) => {
                cursor_debug(tx, "event thinking_completed");
                Ok(CursorServerOutcome::Meaningful)
            }
            Some(interaction_update::Message::Heartbeat(_)) => {
                cursor_debug(tx, "event heartbeat");
                Ok(CursorServerOutcome::Heartbeat)
            }
            Some(interaction_update::Message::StepStarted(_)) => {
                cursor_debug(tx, "event step_started");
                Ok(CursorServerOutcome::Meaningful)
            }
            Some(interaction_update::Message::StepCompleted(_)) => {
                cursor_debug(tx, "event step_completed");
                Ok(CursorServerOutcome::StepCompleted)
            }
            Some(interaction_update::Message::TurnEnded(_)) => {
                cursor_debug(tx, "event turn_ended");
                Ok(CursorServerOutcome::Done)
            }
            _ => Ok(CursorServerOutcome::Continue),
        },
        Some(agent_server_message::Message::ConversationCheckpointUpdate(c)) => {
            if let Some(t) = c.token_details {
                cursor_debug(
                    tx,
                    format!(
                        "event conversation_checkpoint used_tokens={} max_tokens={}",
                        t.used_tokens, t.max_tokens
                    ),
                );
                if t.used_tokens > 0 {
                    usage.input_tokens = t.used_tokens as u64;
                    usage.output_tokens = 0;
                }
                // Cursor often reports 200k as the default conversation budget.
                // Treat that as an unknown/default placeholder so the status bar
                // does not imply we learned the model's real maximum context.
                if t.max_tokens > 0 && t.max_tokens != 200_000 {
                    usage.context_window_tokens = Some(t.max_tokens as u64);
                }
            } else {
                cursor_debug(tx, "event conversation_checkpoint");
            }
            Ok(CursorServerOutcome::Checkpoint)
        }
        Some(agent_server_message::Message::KvServerMessage(kv)) => {
            cursor_debug(tx, format!("event kv_server_message id={}", kv.id));
            match kv.message {
                Some(kv_server_message::Message::GetBlobArgs(args)) => {
                    let found = blob_store.contains_key(&args.blob_id);
                    cursor_debug(
                        tx,
                        format!(
                            "event kv_get_blob bytes={} found={}",
                            args.blob_id.len(),
                            found
                        ),
                    );
                    let reply = AgentClientMessage {
                        message: Some(agent_client_message::Message::KvClientMessage(
                            KvClientMessage {
                                id: kv.id,
                                message: Some(kv_client_message::Message::GetBlobResult(
                                    GetBlobResult {
                                        blob_data: blob_store.get(&args.blob_id).cloned(),
                                    },
                                )),
                            },
                        )),
                    };
                    body_tx
                        .send(frame_connect_message(&reply.encode_to_vec(), 0))
                        .await
                        .map_err(|_| {
                            ProviderError::Other("Cursor request stream closed".to_string())
                        })?;
                }
                Some(kv_server_message::Message::SetBlobArgs(args)) => {
                    cursor_debug(
                        tx,
                        format!(
                            "event kv_set_blob id={} blob={} data={}",
                            kv.id,
                            args.blob_id.len(),
                            args.blob_data.len()
                        ),
                    );
                    blob_store.insert(args.blob_id, args.blob_data);
                    let reply = AgentClientMessage {
                        message: Some(agent_client_message::Message::KvClientMessage(
                            KvClientMessage {
                                id: kv.id,
                                message: Some(kv_client_message::Message::SetBlobResult(
                                    SetBlobResult {},
                                )),
                            },
                        )),
                    };
                    body_tx
                        .send(frame_connect_message(&reply.encode_to_vec(), 0))
                        .await
                        .map_err(|_| {
                            ProviderError::Other("Cursor request stream closed".to_string())
                        })?;
                }
                None => {
                    cursor_debug(tx, format!("event kv_unknown id={}", kv.id));
                }
            }
            Ok(CursorServerOutcome::KvServerMessage)
        }
        Some(agent_server_message::Message::ExecServerMessage(exec)) => {
            cursor_debug(
                tx,
                format!(
                    "event exec_server_message id={} exec_id={}",
                    exec.id, exec.exec_id
                ),
            );
            handle_exec_server_message(exec, tx, body_tx).await
        }
        None => Err(
            ProviderError::Other("Cursor sent an unsupported server message".to_string()).into(),
        ),
    }
}

pub(super) async fn send_cursor_client_message(
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    msg: AgentClientMessage,
) -> Result<()> {
    body_tx
        .send(frame_connect_message(&msg.encode_to_vec(), 0))
        .await
        .map_err(|_| ProviderError::Other("Cursor request stream closed".to_string()).into())
}

pub(super) fn frame_connect_message(data: &[u8], flags: u8) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + data.len());
    frame.push(flags);
    frame.extend_from_slice(&(data.len() as u32).to_be_bytes());
    frame.extend_from_slice(data);
    frame
}

fn decode_get_usable_models_response(bytes: &[u8]) -> Result<GetUsableModelsResponse> {
    match GetUsableModelsResponse::decode(bytes) {
        Ok(decoded) => Ok(decoded),
        Err(first_err) => {
            if bytes.len() >= 5 {
                let len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
                if bytes.len() >= 5 + len {
                    return GetUsableModelsResponse::decode(&bytes[5..5 + len])
                        .map_err(|_| first_err.into());
                }
            }
            Err(first_err.into())
        }
    }
}

fn model_info_from_cursor_details(m: ModelDetails) -> Option<ModelInfo> {
    let id = m.model_id.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let display =
        if !m.display_name.trim().is_empty() && !looks_like_raw_model_name(&m.display_name, &id) {
            m.display_name
        } else if !m.display_model_id.trim().is_empty()
            && !looks_like_raw_model_name(&m.display_model_id, &id)
        {
            m.display_model_id
        } else {
            pretty_cursor_model_name(&id)
        };
    Some(ModelInfo {
        id,
        display,
        request_id: None,
        context_window_tokens: None,
        context_needs_pick: false,
        modalities: Vec::new(),
    })
}

fn looks_like_raw_model_name(name: &str, model_id: &str) -> bool {
    let name = name.trim();
    name.is_empty()
        || name.eq_ignore_ascii_case(model_id.trim())
        || name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

fn pretty_cursor_model_name(model_id: &str) -> String {
    let normalized = model_id.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return model_id.to_string();
    }
    match normalized.as_str() {
        "composer-1" => return "Composer 1".to_string(),
        "composer-1.5" => return "Composer 1.5".to_string(),
        "composer-2" => return "Composer 2".to_string(),
        _ => {}
    }
    let parts: Vec<String> = normalized
        .split('-')
        .filter(|p| !p.is_empty())
        .map(format_model_token)
        .collect();
    parts.join(" ")
}

fn format_model_token(token: &str) -> String {
    if token == "gpt" {
        return "GPT".to_string();
    }
    if token == "xhigh" {
        return "XHigh".to_string();
    }
    if token.ends_with('m')
        && token[..token.len().saturating_sub(1)]
            .chars()
            .all(|c| c.is_ascii_digit())
    {
        return format!("{}M", &token[..token.len() - 1]);
    }
    let mut chars = token.chars();
    match chars.next() {
        Some(first) if !first.is_ascii_digit() => {
            format!(
                "{}{}",
                first.to_ascii_uppercase(),
                chars.collect::<String>()
            )
        }
        _ => token.to_string(),
    }
}
