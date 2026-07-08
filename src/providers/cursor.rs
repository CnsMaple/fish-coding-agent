use super::{ChatEvent, ChatRequest, Provider, ProviderError, Usage};
use crate::config::ProviderKind;
use crate::function::notifications::ModelInfo;
use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use futures_util::StreamExt;
use prost::Message;
use reqwest::StatusCode;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

const CURSOR_LOGIN_URL: &str = "https://cursor.com/loginDeepControl";
const CURSOR_POLL_URL: &str = "https://api2.cursor.sh/auth/poll";
const CURSOR_REFRESH_URL: &str = "https://api2.cursor.sh/auth/exchange_user_api_key";
const CURSOR_CLIENT_VERSION: &str = "cli-2026.01.09-231024f";
const CONNECT_END_STREAM_FLAG: u8 = 0b0000_0010;
const CURSOR_MODELS_PATH: &str = "/agent.v1.AgentService/GetUsableModels";
const CURSOR_STREAM_TIMEOUT_SECS: u64 = 120;

pub struct CursorProvider;

#[derive(Debug, Clone)]
pub struct CursorAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Clone)]
pub struct CursorAuthParams {
    pub verifier: String,
    pub uuid: String,
    pub login_url: String,
}

pub fn generate_auth_params() -> CursorAuthParams {
    let mut seed = Vec::new();
    seed.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    seed.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    seed.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(seed);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    let uuid = uuid::Uuid::new_v4().to_string();
    let login_url = format!(
        "{CURSOR_LOGIN_URL}?challenge={challenge}&uuid={uuid}&mode=login&redirectTarget=cli"
    );
    CursorAuthParams {
        verifier,
        uuid,
        login_url,
    }
}

pub fn open_browser(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};

    #[cfg(target_os = "windows")]
    {
        // Avoid `cmd /C start`: Cursor OAuth URLs contain `&`, which cmd treats
        // as command separators unless every layer quotes perfectly. rundll32
        // receives the URL directly and keeps accidental shell output out of
        // the TUI.
        Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    Ok(())
}

pub async fn poll_auth(
    client: &reqwest::Client,
    uuid: &str,
    verifier: &str,
) -> Result<CursorAuthTokens> {
    let mut delay_ms: u64 = 1000;
    let mut consecutive_errors = 0;
    for _ in 0..150 {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        let url = format!("{CURSOR_POLL_URL}?uuid={uuid}&verifier={verifier}");
        match client.get(&url).send().await {
            Ok(resp) if resp.status() == StatusCode::NOT_FOUND => {
                consecutive_errors = 0;
                delay_ms = ((delay_ms as f64 * 1.2) as u64).min(10_000);
            }
            Ok(resp) if resp.status().is_success() => {
                let body: CursorAuthResp = resp.json().await.map_err(ProviderError::Http)?;
                return Ok(CursorAuthTokens {
                    access_token: body.access_token,
                    refresh_token: body.refresh_token,
                });
            }
            Ok(resp) => {
                return Err(ProviderError::Other(format!(
                    "Cursor auth poll status {}",
                    resp.status()
                ))
                .into())
            }
            Err(_) => {
                consecutive_errors += 1;
                if consecutive_errors >= 3 {
                    return Err(ProviderError::Other(
                        "too many Cursor auth polling errors".to_string(),
                    )
                    .into());
                }
            }
        }
    }
    Err(ProviderError::Other("Cursor authentication polling timeout".to_string()).into())
}

pub async fn refresh_token(client: &reqwest::Client, refresh: &str) -> Result<CursorAuthTokens> {
    let resp = client
        .post(CURSOR_REFRESH_URL)
        .bearer_auth(refresh)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .map_err(ProviderError::Http)?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(
            ProviderError::Other(format!("Cursor token refresh status {status}: {text}")).into(),
        );
    }
    let body: CursorAuthResp = resp.json().await.map_err(ProviderError::Http)?;
    Ok(CursorAuthTokens {
        access_token: body.access_token,
        refresh_token: body.refresh_token,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorAuthResp {
    access_token: String,
    refresh_token: String,
}

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

fn cursor_debug(tx: &mpsc::UnboundedSender<ChatEvent>, message: impl Into<String>) {
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

fn create_blob_id(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorServerOutcome {
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

async fn handle_server_message(
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

async fn handle_exec_server_message(
    exec: ExecServerMessage,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<CursorServerOutcome> {
    match exec.message {
        Some(exec_server_message::Message::RequestContextArgs(_)) => {
            cursor_debug(
                tx,
                format!(
                    "exec request_context id={} exec_id={}",
                    exec.id, exec.exec_id
                ),
            );
            let reply = AgentClientMessage {
                message: Some(agent_client_message::Message::ExecClientMessage(
                    ExecClientMessage {
                        id: exec.id,
                        exec_id: exec.exec_id,
                        message: Some(exec_client_message::Message::RequestContextResult(
                            RequestContextResult {
                                result: Some(request_context_result::Result::Success(
                                    RequestContextSuccess {
                                        request_context: Some(RequestContext::default()),
                                    },
                                )),
                            },
                        )),
                    },
                )),
            };
            send_cursor_client_message(body_tx, reply).await?;
            Ok(CursorServerOutcome::Meaningful)
        }
        Some(exec_server_message::Message::ShellArgs(args)) => {
            cursor_debug(
                tx,
                format!(
                    "exec shell_args id={} command={}",
                    exec.id,
                    args.command.trim()
                ),
            );
            handle_shell_exec(exec.id, exec.exec_id, args, false, tx, body_tx).await?;
            Ok(CursorServerOutcome::ToolOutput)
        }
        Some(exec_server_message::Message::ShellStreamArgs(args)) => {
            cursor_debug(
                tx,
                format!(
                    "exec shell_stream_args id={} command={}",
                    exec.id,
                    args.command.trim()
                ),
            );
            handle_shell_exec(exec.id, exec.exec_id, args, true, tx, body_tx).await?;
            Ok(CursorServerOutcome::ToolOutput)
        }
        Some(exec_server_message::Message::ReadArgs(args)) => {
            cursor_debug(
                tx,
                format!("exec read_args id={} path={}", exec.id, args.path),
            );
            handle_read_exec(exec.id, exec.exec_id, args, tx, body_tx).await?;
            Ok(CursorServerOutcome::ToolOutput)
        }
        Some(exec_server_message::Message::LsArgs(args)) => {
            cursor_debug(
                tx,
                format!("exec ls_args id={} path={}", exec.id, args.path),
            );
            handle_ls_exec(exec.id, exec.exec_id, args, tx, body_tx).await?;
            Ok(CursorServerOutcome::ToolOutput)
        }
        Some(exec_server_message::Message::GrepArgs(args)) => {
            cursor_debug(
                tx,
                format!("exec grep_args id={} pattern={}", exec.id, args.pattern),
            );
            handle_grep_exec(exec.id, exec.exec_id, args, tx, body_tx).await?;
            Ok(CursorServerOutcome::ToolOutput)
        }
        None => {
            cursor_debug(
                tx,
                format!(
                    "exec unsupported_unknown id={} exec_id={} (ignored)",
                    exec.id, exec.exec_id
                ),
            );
            Ok(CursorServerOutcome::Continue)
        }
    }
}

async fn handle_read_exec(
    id: u32,
    exec_id: String,
    args: ReadArgs,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let path = resolve_cursor_path(&cwd, &args.path);
    let result = match tokio::fs::read_to_string(&path).await {
        Ok(content) => {
            let total_lines = content.lines().count() as i32;
            let file_size = content.len() as i64;
            read_result::Result::Success(ReadSuccess {
                path: args.path.clone(),
                total_lines,
                file_size,
                truncated: false,
                output_blob_id: None,
                output: Some(read_success::Output::Content(content)),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            read_result::Result::FileNotFound(ReadFileNotFound {
                path: args.path.clone(),
            })
        }
        Err(e) => read_result::Result::Error(ReadError {
            path: args.path.clone(),
            error: e.to_string(),
        }),
    };
    let display = match &result {
        read_result::Result::Success(s) => match &s.output {
            Some(read_success::Output::Content(c)) => c.clone(),
            Some(read_success::Output::Data(d)) => format!("[binary data: {} bytes]", d.len()),
            None => String::new(),
        },
        read_result::Result::Error(e) => format!("[read error] {}", e.error),
        read_result::Result::FileNotFound(_) => "[file not found]".to_string(),
        read_result::Result::Rejected(e) => format!("[read rejected] {}", e.reason),
        read_result::Result::PermissionDenied(_) => "[permission denied]".to_string(),
        read_result::Result::InvalidFile(e) => format!("[invalid file] {}", e.reason),
    };
    let _ = tx.send(ChatEvent::ToolResult {
        name: "read".to_string(),
        title: format!("[read] {}", args.path),
        content: display,
    });
    send_cursor_client_message(
        body_tx,
        AgentClientMessage {
            message: Some(agent_client_message::Message::ExecClientMessage(
                ExecClientMessage {
                    id,
                    exec_id,
                    message: Some(exec_client_message::Message::ReadResult(ReadResult {
                        result: Some(result),
                    })),
                },
            )),
        },
    )
    .await
}

async fn handle_ls_exec(
    id: u32,
    exec_id: String,
    args: LsArgs,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let target = if args.path.trim().is_empty() {
        "."
    } else {
        args.path.trim()
    };
    let path = resolve_cursor_path(&cwd, target);
    let (result, display) = match build_ls_tree(&path, 2) {
        Ok(root) => {
            let display = format_ls_tree(&root, 0);
            (
                ls_result::Result::Success(LsSuccess {
                    directory_tree_root: Some(root),
                }),
                display,
            )
        }
        Err(e) => (
            ls_result::Result::Error(LsError {
                path: target.to_string(),
                error: e.to_string(),
            }),
            format!("[ls error] {e}"),
        ),
    };
    let _ = tx.send(ChatEvent::ToolResult {
        name: "list".to_string(),
        title: format!("[list] {}", target),
        content: display,
    });
    send_cursor_client_message(
        body_tx,
        AgentClientMessage {
            message: Some(agent_client_message::Message::ExecClientMessage(
                ExecClientMessage {
                    id,
                    exec_id,
                    message: Some(exec_client_message::Message::LsResult(LsResult {
                        result: Some(result),
                    })),
                },
            )),
        },
    )
    .await
}

async fn handle_grep_exec(
    id: u32,
    exec_id: String,
    args: GrepArgs,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let tool_args = serde_json::json!({
        "pattern": args.pattern,
        "path": args.path.unwrap_or_else(|| ".".to_string()),
    })
    .to_string();
    let content = crate::tools::execute_tool("grep", &tool_args, &cwd).await;
    let _ = tx.send(ChatEvent::ToolResult {
        name: "grep".to_string(),
        title: "[grep]".to_string(),
        content: content.clone(),
    });
    let result = grep_result::Result::Error(GrepError { error: content });
    send_cursor_client_message(
        body_tx,
        AgentClientMessage {
            message: Some(agent_client_message::Message::ExecClientMessage(
                ExecClientMessage {
                    id,
                    exec_id,
                    message: Some(exec_client_message::Message::GrepResult(GrepResult {
                        result: Some(result),
                    })),
                },
            )),
        },
    )
    .await
}

fn resolve_cursor_path(cwd: &std::path::Path, path: &str) -> PathBuf {
    let p = PathBuf::from(path.trim());
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}

fn build_ls_tree(path: &std::path::Path, depth: usize) -> std::io::Result<LsDirectoryTreeNode> {
    let abs_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut node = LsDirectoryTreeNode {
        abs_path: abs_path.display().to_string(),
        children_dirs: Vec::new(),
        children_files: Vec::new(),
        children_were_processed: false,
        full_subtree_extension_counts: std::collections::HashMap::new(),
        num_files: 0,
    };
    if depth == 0 || !path.is_dir() {
        return Ok(node);
    }
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        if file_name == ".git" || file_name == "target" {
            continue;
        }
        let meta = entry.metadata()?;
        if meta.is_dir() {
            dirs.push(entry.path());
        } else if meta.is_file() {
            files.push(file_name);
        }
    }
    dirs.sort();
    files.sort();
    for dir in dirs.into_iter().take(64) {
        if let Ok(child) = build_ls_tree(&dir, depth.saturating_sub(1)) {
            node.num_files += child.num_files;
            node.children_dirs.push(child);
        }
    }
    for file in files.into_iter().take(256) {
        if let Some(ext) = std::path::Path::new(&file)
            .extension()
            .and_then(|e| e.to_str())
        {
            *node
                .full_subtree_extension_counts
                .entry(ext.to_string())
                .or_insert(0) += 1;
        }
        node.num_files += 1;
        node.children_files.push(LsDirectoryTreeNodeFile {
            name: file,
            terminal_metadata: None,
        });
    }
    node.children_were_processed = true;
    Ok(node)
}

fn format_ls_tree(node: &LsDirectoryTreeNode, indent: usize) -> String {
    let mut out = String::new();
    let name = std::path::Path::new(&node.abs_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&node.abs_path);
    out.push_str(&format!("{}{}\n", "  ".repeat(indent), name));
    for dir in &node.children_dirs {
        out.push_str(&format_ls_tree(dir, indent + 1));
    }
    for file in &node.children_files {
        out.push_str(&format!("{}{}\n", "  ".repeat(indent + 1), file.name));
    }
    out
}

#[cfg(windows)]
fn normalize_cursor_shell_command(command: &str) -> String {
    match command.trim() {
        "ls -la" | "ls -al" | "ls --all -l" | "ls -l -a" => "Get-ChildItem -Force".to_string(),
        "ls -a" | "ls --all" => "Get-ChildItem -Force".to_string(),
        "ls -l" => "Get-ChildItem".to_string(),
        other => other.to_string(),
    }
}

#[cfg(not(windows))]
fn normalize_cursor_shell_command(command: &str) -> String {
    command.trim().to_string()
}

async fn handle_shell_exec(
    id: u32,
    exec_id: String,
    args: ShellArgs,
    stream: bool,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let command = normalize_cursor_shell_command(args.command.trim());
    let cwd = if args.working_directory.trim().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(args.working_directory.trim())
    };
    let tool_args = serde_json::json!({ "command": command }).to_string();
    let content = crate::tools::execute_tool("shell_command", &tool_args, &cwd).await;
    let shell_content = crate::session::unwrap_tool_result_content(&content);
    let _ = tx.send(ChatEvent::ToolResult {
        name: "shell_command".to_string(),
        title: format!("$ {}", command),
        content: content.clone(),
    });

    let parsed = ParsedShellOutput::parse(&shell_content);
    cursor_debug(
        tx,
        format!(
            "exec shell_result exit={} stdout={} stderr={} stream={}",
            parsed.exit_code,
            parsed.stdout.len(),
            parsed.stderr.len(),
            stream
        ),
    );
    if stream {
        send_shell_stream_event(
            id,
            exec_id.clone(),
            body_tx,
            shell_stream::Event::Start(ShellStreamStart {}),
        )
        .await?;
        if !parsed.stdout.is_empty() {
            send_shell_stream_event(
                id,
                exec_id.clone(),
                body_tx,
                shell_stream::Event::Stdout(ShellStreamStdout {
                    data: parsed.stdout.clone(),
                }),
            )
            .await?;
        }
        if !parsed.stderr.is_empty() {
            send_shell_stream_event(
                id,
                exec_id.clone(),
                body_tx,
                shell_stream::Event::Stderr(ShellStreamStderr {
                    data: parsed.stderr.clone(),
                }),
            )
            .await?;
        }
        send_shell_stream_event(
            id,
            exec_id.clone(),
            body_tx,
            shell_stream::Event::Exit(ShellStreamExit {
                code: parsed.exit_code.max(0) as u32,
                cwd: cwd.display().to_string(),
                aborted: false,
            }),
        )
        .await?;
    }

    let result = if parsed.exit_code == 0 && !content.starts_with("[Tool Error]") {
        shell_result::Result::Success(ShellSuccess {
            command,
            working_directory: cwd.display().to_string(),
            exit_code: parsed.exit_code,
            signal: String::new(),
            stdout: parsed.stdout,
            stderr: parsed.stderr,
            execution_time: parsed.execution_time_ms,
        })
    } else {
        shell_result::Result::Failure(ShellFailure {
            command,
            working_directory: cwd.display().to_string(),
            exit_code: parsed.exit_code,
            signal: String::new(),
            stdout: parsed.stdout,
            stderr: parsed.stderr,
            execution_time: parsed.execution_time_ms,
            aborted: false,
        })
    };
    send_cursor_client_message(
        body_tx,
        AgentClientMessage {
            message: Some(agent_client_message::Message::ExecClientMessage(
                ExecClientMessage {
                    id,
                    exec_id: exec_id.clone(),
                    message: Some(exec_client_message::Message::ShellResult(ShellResult {
                        result: Some(result),
                    })),
                },
            )),
        },
    )
    .await?;
    if stream {
        cursor_debug(tx, format!("exec stream_close id={id}"));
        send_exec_stream_close(id, body_tx).await?;
    }
    Ok(())
}

async fn send_shell_stream_event(
    id: u32,
    exec_id: String,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    event: shell_stream::Event,
) -> Result<()> {
    send_cursor_client_message(
        body_tx,
        AgentClientMessage {
            message: Some(agent_client_message::Message::ExecClientMessage(
                ExecClientMessage {
                    id,
                    exec_id,
                    message: Some(exec_client_message::Message::ShellStream(ShellStream {
                        event: Some(event),
                    })),
                },
            )),
        },
    )
    .await
}

async fn send_exec_stream_close(
    id: u32,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    send_cursor_client_message(
        body_tx,
        AgentClientMessage {
            message: Some(agent_client_message::Message::ExecClientControlMessage(
                ExecClientControlMessage {
                    message: Some(exec_client_control_message::Message::StreamClose(
                        ExecClientStreamClose { id },
                    )),
                },
            )),
        },
    )
    .await
}

async fn send_cursor_client_message(
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    msg: AgentClientMessage,
) -> Result<()> {
    body_tx
        .send(frame_connect_message(&msg.encode_to_vec(), 0))
        .await
        .map_err(|_| ProviderError::Other("Cursor request stream closed".to_string()).into())
}

struct ParsedShellOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
    execution_time_ms: i32,
}

impl ParsedShellOutput {
    fn parse(content: &str) -> Self {
        let exit_code = extract_header_value(content, "exit_code:")
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(-1);
        let execution_time_ms = extract_header_value(content, "wall_secs:")
            .and_then(|s| s.parse::<f64>().ok())
            .map(|s| (s * 1000.0).round() as i32)
            .unwrap_or(0);
        Self {
            exit_code,
            stdout: extract_section(
                content,
                "stdout:
",
                "
stderr:
",
            )
            .unwrap_or_default(),
            stderr: extract_after(
                content,
                "
stderr:
",
            )
            .unwrap_or_default(),
            execution_time_ms,
        }
    }
}

fn extract_header_value<'a>(content: &'a str, key: &str) -> Option<&'a str> {
    content
        .lines()
        .find_map(|line| line.strip_prefix(key).map(str::trim))
}

fn extract_section(content: &str, start: &str, end: &str) -> Option<String> {
    let rest = content.split_once(start)?.1;
    let value = rest.split_once(end).map(|(v, _)| v).unwrap_or(rest);
    Some(value.to_string())
}

fn extract_after(content: &str, start: &str) -> Option<String> {
    Some(content.split_once(start)?.1.to_string())
}

fn frame_connect_message(data: &[u8], flags: u8) -> Vec<u8> {
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

#[derive(Clone, PartialEq, Message)]
struct GetUsableModelsRequest {
    #[prost(string, repeated, tag = "1")]
    custom_model_ids: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
struct GetUsableModelsResponse {
    #[prost(message, repeated, tag = "1")]
    models: Vec<ModelDetails>,
}

#[derive(Clone, PartialEq, Message)]
struct AgentClientMessage {
    #[prost(oneof = "agent_client_message::Message", tags = "1, 2, 3, 5, 7")]
    message: Option<agent_client_message::Message>,
}
mod agent_client_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        RunRequest(super::AgentRunRequest),
        #[prost(message, tag = "2")]
        ExecClientMessage(super::ExecClientMessage),
        #[prost(message, tag = "3")]
        KvClientMessage(super::KvClientMessage),
        #[prost(message, tag = "5")]
        ExecClientControlMessage(super::ExecClientControlMessage),
        #[prost(message, tag = "7")]
        ClientHeartbeat(super::ClientHeartbeat),
    }
}
#[derive(Clone, PartialEq, Message)]
struct ClientHeartbeat {}

#[derive(Clone, PartialEq, Message)]
struct AgentRunRequest {
    #[prost(message, optional, tag = "1")]
    conversation_state: Option<ConversationStateStructure>,
    #[prost(message, optional, tag = "2")]
    action: Option<ConversationAction>,
    #[prost(message, optional, tag = "3")]
    model_details: Option<ModelDetails>,
    #[prost(string, optional, tag = "5")]
    conversation_id: Option<String>,
    #[prost(string, optional, tag = "8")]
    custom_system_prompt: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ConversationStateStructure {
    #[prost(bytes = "vec", repeated, tag = "1")]
    root_prompt_messages_json: Vec<Vec<u8>>,
    #[prost(message, optional, tag = "5")]
    token_details: Option<ConversationTokenDetails>,
}
#[derive(Clone, PartialEq, Message)]
struct ConversationTokenDetails {
    #[prost(uint32, tag = "1")]
    used_tokens: u32,
    #[prost(uint32, tag = "2")]
    max_tokens: u32,
}

#[derive(Clone, PartialEq, Message)]
struct ConversationAction {
    #[prost(oneof = "conversation_action::Action", tags = "1")]
    action: Option<conversation_action::Action>,
}
mod conversation_action {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Action {
        #[prost(message, tag = "1")]
        UserMessageAction(super::UserMessageAction),
    }
}
#[derive(Clone, PartialEq, Message)]
struct UserMessageAction {
    #[prost(message, optional, tag = "1")]
    user_message: Option<UserMessage>,
}
#[derive(Clone, PartialEq, Message)]
struct UserMessage {
    #[prost(string, tag = "1")]
    text: String,
    #[prost(string, tag = "2")]
    message_id: String,
    #[prost(int32, tag = "4")]
    mode: i32,
}
#[derive(Clone, PartialEq, Message)]
struct ModelDetails {
    #[prost(string, tag = "1")]
    model_id: String,
    #[prost(string, tag = "3")]
    display_model_id: String,
    #[prost(string, tag = "4")]
    display_name: String,
}

#[derive(Clone, PartialEq, Message)]
struct ExecServerMessage {
    #[prost(uint32, tag = "1")]
    id: u32,
    #[prost(string, tag = "15")]
    exec_id: String,
    #[prost(oneof = "exec_server_message::Message", tags = "2, 5, 7, 8, 10, 14")]
    message: Option<exec_server_message::Message>,
}
mod exec_server_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    #[allow(clippy::enum_variant_names)]
    pub enum Message {
        #[prost(message, tag = "2")]
        ShellArgs(super::ShellArgs),
        #[prost(message, tag = "5")]
        GrepArgs(super::GrepArgs),
        #[prost(message, tag = "7")]
        ReadArgs(super::ReadArgs),
        #[prost(message, tag = "8")]
        LsArgs(super::LsArgs),
        #[prost(message, tag = "10")]
        RequestContextArgs(super::RequestContextArgs),
        #[prost(message, tag = "14")]
        ShellStreamArgs(super::ShellArgs),
    }
}
#[derive(Clone, PartialEq, Message)]
struct ShellArgs {
    #[prost(string, tag = "1")]
    command: String,
    #[prost(string, tag = "2")]
    working_directory: String,
    #[prost(int32, tag = "3")]
    timeout: i32,
    #[prost(string, tag = "4")]
    tool_call_id: String,
}

#[derive(Clone, PartialEq, Message)]
struct ShellResult {
    #[prost(oneof = "shell_result::Result", tags = "1, 2")]
    result: Option<shell_result::Result>,
}
mod shell_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::ShellSuccess),
        #[prost(message, tag = "2")]
        Failure(super::ShellFailure),
    }
}

#[derive(Clone, PartialEq, Message)]
struct ShellSuccess {
    #[prost(string, tag = "1")]
    command: String,
    #[prost(string, tag = "2")]
    working_directory: String,
    #[prost(int32, tag = "3")]
    exit_code: i32,
    #[prost(string, tag = "4")]
    signal: String,
    #[prost(string, tag = "5")]
    stdout: String,
    #[prost(string, tag = "6")]
    stderr: String,
    #[prost(int32, tag = "7")]
    execution_time: i32,
}

#[derive(Clone, PartialEq, Message)]
struct ShellFailure {
    #[prost(string, tag = "1")]
    command: String,
    #[prost(string, tag = "2")]
    working_directory: String,
    #[prost(int32, tag = "3")]
    exit_code: i32,
    #[prost(string, tag = "4")]
    signal: String,
    #[prost(string, tag = "5")]
    stdout: String,
    #[prost(string, tag = "6")]
    stderr: String,
    #[prost(int32, tag = "7")]
    execution_time: i32,
    #[prost(bool, tag = "11")]
    aborted: bool,
}

#[derive(Clone, PartialEq, Message)]
struct ShellStream {
    #[prost(oneof = "shell_stream::Event", tags = "1, 2, 3, 4")]
    event: Option<shell_stream::Event>,
}
mod shell_stream {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Event {
        #[prost(message, tag = "1")]
        Stdout(super::ShellStreamStdout),
        #[prost(message, tag = "2")]
        Stderr(super::ShellStreamStderr),
        #[prost(message, tag = "3")]
        Exit(super::ShellStreamExit),
        #[prost(message, tag = "4")]
        Start(super::ShellStreamStart),
    }
}
#[derive(Clone, PartialEq, Message)]
struct ShellStreamStart {}
#[derive(Clone, PartialEq, Message)]
struct ShellStreamStdout {
    #[prost(string, tag = "1")]
    data: String,
}
#[derive(Clone, PartialEq, Message)]
struct ShellStreamStderr {
    #[prost(string, tag = "1")]
    data: String,
}
#[derive(Clone, PartialEq, Message)]
struct ShellStreamExit {
    #[prost(uint32, tag = "1")]
    code: u32,
    #[prost(string, tag = "2")]
    cwd: String,
    #[prost(bool, tag = "4")]
    aborted: bool,
}

#[derive(Clone, PartialEq, Message)]
struct ExecClientControlMessage {
    #[prost(oneof = "exec_client_control_message::Message", tags = "1")]
    message: Option<exec_client_control_message::Message>,
}
mod exec_client_control_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        StreamClose(super::ExecClientStreamClose),
    }
}
#[derive(Clone, PartialEq, Message)]
struct ExecClientStreamClose {
    #[prost(uint32, tag = "1")]
    id: u32,
}

#[derive(Clone, PartialEq, Message)]
struct ReadArgs {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    tool_call_id: String,
}
#[derive(Clone, PartialEq, Message)]
struct ReadResult {
    #[prost(oneof = "read_result::Result", tags = "1, 2, 3, 4, 5, 6")]
    result: Option<read_result::Result>,
}
mod read_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::ReadSuccess),
        #[prost(message, tag = "2")]
        Error(super::ReadError),
        #[prost(message, tag = "3")]
        Rejected(super::ReadRejected),
        #[prost(message, tag = "4")]
        FileNotFound(super::ReadFileNotFound),
        #[prost(message, tag = "5")]
        PermissionDenied(super::ReadPermissionDenied),
        #[prost(message, tag = "6")]
        InvalidFile(super::ReadInvalidFile),
    }
}
#[derive(Clone, PartialEq, Message)]
struct ReadSuccess {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(int32, tag = "3")]
    total_lines: i32,
    #[prost(int64, tag = "4")]
    file_size: i64,
    #[prost(bool, tag = "6")]
    truncated: bool,
    #[prost(bytes = "vec", optional, tag = "7")]
    output_blob_id: Option<Vec<u8>>,
    #[prost(oneof = "read_success::Output", tags = "2, 5")]
    output: Option<read_success::Output>,
}
mod read_success {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Output {
        #[prost(string, tag = "2")]
        Content(String),
        #[prost(bytes, tag = "5")]
        Data(Vec<u8>),
    }
}
#[derive(Clone, PartialEq, Message)]
struct ReadError {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    error: String,
}
#[derive(Clone, PartialEq, Message)]
struct ReadRejected {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    reason: String,
}
#[derive(Clone, PartialEq, Message)]
struct ReadFileNotFound {
    #[prost(string, tag = "1")]
    path: String,
}
#[derive(Clone, PartialEq, Message)]
struct ReadPermissionDenied {
    #[prost(string, tag = "1")]
    path: String,
}
#[derive(Clone, PartialEq, Message)]
struct ReadInvalidFile {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    reason: String,
}

#[derive(Clone, PartialEq, Message)]
struct LsArgs {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, repeated, tag = "2")]
    ignore: Vec<String>,
    #[prost(string, tag = "3")]
    tool_call_id: String,
    #[prost(uint32, optional, tag = "5")]
    timeout_ms: Option<u32>,
}
#[derive(Clone, PartialEq, Message)]
struct LsResult {
    #[prost(oneof = "ls_result::Result", tags = "1, 2, 3, 4")]
    result: Option<ls_result::Result>,
}
mod ls_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::LsSuccess),
        #[prost(message, tag = "2")]
        Error(super::LsError),
        #[prost(message, tag = "3")]
        Rejected(super::LsRejected),
        #[prost(message, tag = "4")]
        Timeout(super::LsTimeout),
    }
}
#[derive(Clone, PartialEq, Message)]
struct LsSuccess {
    #[prost(message, optional, tag = "1")]
    directory_tree_root: Option<LsDirectoryTreeNode>,
}
#[derive(Clone, PartialEq, Message)]
struct LsDirectoryTreeNode {
    #[prost(string, tag = "1")]
    abs_path: String,
    #[prost(message, repeated, tag = "2")]
    children_dirs: Vec<LsDirectoryTreeNode>,
    #[prost(message, repeated, tag = "3")]
    children_files: Vec<LsDirectoryTreeNodeFile>,
    #[prost(bool, tag = "4")]
    children_were_processed: bool,
    #[prost(map = "string, int32", tag = "5")]
    full_subtree_extension_counts: std::collections::HashMap<String, i32>,
    #[prost(int32, tag = "6")]
    num_files: i32,
}
#[derive(Clone, PartialEq, Message)]
struct LsDirectoryTreeNodeFile {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(message, optional, tag = "2")]
    terminal_metadata: Option<TerminalMetadata>,
}
#[derive(Clone, PartialEq, Message)]
struct TerminalMetadata {}
#[derive(Clone, PartialEq, Message)]
struct LsError {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    error: String,
}
#[derive(Clone, PartialEq, Message)]
struct LsRejected {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    reason: String,
}
#[derive(Clone, PartialEq, Message)]
struct LsTimeout {
    #[prost(message, optional, tag = "1")]
    directory_tree_root: Option<LsDirectoryTreeNode>,
}

#[derive(Clone, PartialEq, Message)]
struct GrepArgs {
    #[prost(string, tag = "1")]
    pattern: String,
    #[prost(string, optional, tag = "2")]
    path: Option<String>,
    #[prost(string, optional, tag = "3")]
    glob: Option<String>,
    #[prost(string, optional, tag = "4")]
    output_mode: Option<String>,
}
#[derive(Clone, PartialEq, Message)]
struct GrepResult {
    #[prost(oneof = "grep_result::Result", tags = "1, 2")]
    result: Option<grep_result::Result>,
}
mod grep_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::GrepSuccess),
        #[prost(message, tag = "2")]
        Error(super::GrepError),
    }
}
#[derive(Clone, PartialEq, Message)]
struct GrepSuccess {
    #[prost(string, tag = "1")]
    pattern: String,
    #[prost(string, tag = "2")]
    path: String,
    #[prost(string, tag = "3")]
    output_mode: String,
}
#[derive(Clone, PartialEq, Message)]
struct GrepError {
    #[prost(string, tag = "1")]
    error: String,
}

#[derive(Clone, PartialEq, Message)]
struct RequestContextArgs {}

#[derive(Clone, PartialEq, Message)]
struct ExecClientMessage {
    #[prost(uint32, tag = "1")]
    id: u32,
    #[prost(string, tag = "15")]
    exec_id: String,
    #[prost(oneof = "exec_client_message::Message", tags = "2, 5, 7, 8, 10, 14")]
    message: Option<exec_client_message::Message>,
}
mod exec_client_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        ShellResult(super::ShellResult),
        #[prost(message, tag = "5")]
        GrepResult(super::GrepResult),
        #[prost(message, tag = "7")]
        ReadResult(super::ReadResult),
        #[prost(message, tag = "8")]
        LsResult(super::LsResult),
        #[prost(message, tag = "10")]
        RequestContextResult(super::RequestContextResult),
        #[prost(message, tag = "14")]
        ShellStream(super::ShellStream),
    }
}
#[derive(Clone, PartialEq, Message)]
struct RequestContextResult {
    #[prost(oneof = "request_context_result::Result", tags = "1")]
    result: Option<request_context_result::Result>,
}
mod request_context_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::RequestContextSuccess),
    }
}
#[derive(Clone, PartialEq, Message)]
struct RequestContextSuccess {
    #[prost(message, optional, tag = "1")]
    request_context: Option<RequestContext>,
}
#[derive(Clone, PartialEq, Message)]
struct RequestContext {}

#[derive(Clone, PartialEq, Message)]
struct AgentServerMessage {
    #[prost(oneof = "agent_server_message::Message", tags = "1, 2, 3, 4")]
    message: Option<agent_server_message::Message>,
}
mod agent_server_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        InteractionUpdate(super::InteractionUpdate),
        #[prost(message, tag = "2")]
        ExecServerMessage(super::ExecServerMessage),
        #[prost(message, tag = "3")]
        ConversationCheckpointUpdate(super::ConversationStateStructure),
        #[prost(message, tag = "4")]
        KvServerMessage(super::KvServerMessage),
    }
}
#[derive(Clone, PartialEq, Message)]
struct InteractionUpdate {
    #[prost(
        oneof = "interaction_update::Message",
        tags = "1, 4, 5, 8, 13, 14, 16, 17"
    )]
    message: Option<interaction_update::Message>,
}
mod interaction_update {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        TextDelta(super::TextDeltaUpdate),
        #[prost(message, tag = "4")]
        ThinkingDelta(super::ThinkingDeltaUpdate),
        #[prost(message, tag = "5")]
        ThinkingCompleted(super::ThinkingCompletedUpdate),
        #[prost(message, tag = "8")]
        TokenDelta(super::TokenDeltaUpdate),
        #[prost(message, tag = "13")]
        Heartbeat(super::HeartbeatUpdate),
        #[prost(message, tag = "14")]
        TurnEnded(super::TurnEndedUpdate),
        #[prost(message, tag = "16")]
        StepStarted(super::StepStartedUpdate),
        #[prost(message, tag = "17")]
        StepCompleted(super::StepCompletedUpdate),
    }
}
#[derive(Clone, PartialEq, Message)]
struct TextDeltaUpdate {
    #[prost(string, tag = "1")]
    text: String,
}
#[derive(Clone, PartialEq, Message)]
struct ThinkingDeltaUpdate {
    #[prost(string, tag = "1")]
    text: String,
}
#[derive(Clone, PartialEq, Message)]
struct ThinkingCompletedUpdate {
    #[prost(int32, tag = "1")]
    thinking_duration_ms: i32,
}
#[derive(Clone, PartialEq, Message)]
struct TokenDeltaUpdate {
    #[prost(int32, tag = "1")]
    tokens: i32,
}
#[derive(Clone, PartialEq, Message)]
struct HeartbeatUpdate {}
#[derive(Clone, PartialEq, Message)]
struct TurnEndedUpdate {}
#[derive(Clone, PartialEq, Message)]
struct StepStartedUpdate {}
#[derive(Clone, PartialEq, Message)]
struct StepCompletedUpdate {}
#[derive(Clone, PartialEq, Message)]
struct KvServerMessage {
    #[prost(uint32, tag = "1")]
    id: u32,
    #[prost(oneof = "kv_server_message::Message", tags = "2, 3")]
    message: Option<kv_server_message::Message>,
}
mod kv_server_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        GetBlobArgs(super::GetBlobArgs),
        #[prost(message, tag = "3")]
        SetBlobArgs(super::SetBlobArgs),
    }
}
#[derive(Clone, PartialEq, Message)]
struct GetBlobArgs {
    #[prost(bytes = "vec", tag = "1")]
    blob_id: Vec<u8>,
}
#[derive(Clone, PartialEq, Message)]
struct SetBlobArgs {
    #[prost(bytes = "vec", tag = "1")]
    blob_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    blob_data: Vec<u8>,
}
#[derive(Clone, PartialEq, Message)]
struct KvClientMessage {
    #[prost(uint32, tag = "1")]
    id: u32,
    #[prost(oneof = "kv_client_message::Message", tags = "2, 3")]
    message: Option<kv_client_message::Message>,
}
mod kv_client_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        GetBlobResult(super::GetBlobResult),
        #[prost(message, tag = "3")]
        SetBlobResult(super::SetBlobResult),
    }
}
#[derive(Clone, PartialEq, Message)]
struct GetBlobResult {
    #[prost(bytes = "vec", optional, tag = "1")]
    blob_data: Option<Vec<u8>>,
}
#[derive(Clone, PartialEq, Message)]
struct SetBlobResult {}
