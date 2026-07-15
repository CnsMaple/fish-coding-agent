use super::{ChatEvent, ChatMessage, ContentPart, ProviderError, ToolCall};
use base64::Engine;
use std::path::Path;
use tokio::sync::mpsc;

/// Read an image file from disk and return its base64-encoded content.
/// Returns an empty string on failure (the provider will skip the image).
pub(crate) fn image_to_base64(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => base64::engine::general_purpose::STANDARD.encode(&bytes),
        Err(_) => String::new(),
    }
}

/// Ensure `tool_calls` has at least `idx + 1` slots, pushing empty
/// `ToolCall` placeholders as needed. Used by both OpenAI and Anthropic
/// streaming merge functions.
pub(crate) fn ensure_tool_slot(tool_calls: &mut Vec<ToolCall>, idx: usize) {
    while tool_calls.len() <= idx {
        tool_calls.push(ToolCall {
            id: String::new(),
            name: String::new(),
            arguments: String::new(),
        });
    }
}

/// Attempt to parse an SSE `data:` payload as JSON. On failure, emits a
/// `ChatEvent::Debug` message tagged with `tag` (e.g. `"openai"`,
/// `"anthropic"`) and returns `None`. The caller should treat `None`
/// as "skip this event" (`return Ok(SseControl::Continue)`).
pub(crate) fn parse_sse_json(
    ev: &crate::net::stream::SseEvent<'_>,
    tag: &str,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> Option<serde_json::Value> {
    match serde_json::from_str(ev.data) {
        Ok(v) => Some(v),
        Err(e) => {
            let _ = tx.send(ChatEvent::Debug(format!(
                "{}: malformed SSE json ({}): {}",
                e,
                tag,
                &ev.data[..ev.data.len().min(120)]
            )));
            None
        }
    }
}

/// Check the HTTP status code returned by a model-listing endpoint.
/// Maps 401/403 → `AuthFailed`, 404/405 → `NoModelsEndpoint`, other
/// non-success → `Other`. Used by both OpenAI and Anthropic providers.
pub(crate) fn check_list_models_status(status: reqwest::StatusCode) -> Result<(), ProviderError> {
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(ProviderError::AuthFailed(status.as_u16()));
    }
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
    {
        return Err(ProviderError::NoModelsEndpoint);
    }
    if !status.is_success() {
        return Err(ProviderError::Other(format!("status {status}")));
    }
    Ok(())
}

/// Extract `(status, content_type)` from a `reqwest::Response` without
/// consuming the body. Both fields are needed by the error-formatting
/// helpers below.
pub(crate) fn response_meta(resp: &reqwest::Response) -> (reqwest::StatusCode, String) {
    let status = resp.status();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    (status, ct)
}

/// Format a non-success chat response into a `ProviderError`. Consumes
/// the response body text. The body is included verbatim so the caller
/// sees the full upstream error payload without truncation.
pub(crate) fn chat_response_error(
    status: reqwest::StatusCode,
    ct: &str,
    body: String,
) -> ProviderError {
    ProviderError::Other(format!("status {status} ct={ct} body={body}"))
}

/// Format a rate-limited response into a `ProviderError`. The full body
/// is preserved in the error message so the user can see the upstream
/// quota details.
pub(crate) fn rate_limited_error(body: String) -> ProviderError {
    ProviderError::RateLimited(body)
}

/// Check whether a chat response error looks like a rate/quota limit
/// (HTTP 429 or body containing `insufficient_quota`) so the caller can
/// apply a longer backoff before retrying.
pub(crate) fn is_rate_limited_error(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || body.contains("\"type\":\"insufficient_quota\"")
        || body.contains("'type':'insufficient_quota'")
        || body.contains("insufficient_quota")
}

/// Build a multimodal content array from a message's `content_parts`,
/// flushing accumulated text segments between image blocks. The caller
/// provides a closure that turns an `ImageAttachment` + base64 string
/// into the provider-specific image JSON block.
///
/// Shared by `openai_message` and `anthropic_message`.
pub(crate) fn build_multimodal_content(
    m: &ChatMessage,
    image_block: impl Fn(&crate::session::ImageAttachment, &str) -> serde_json::Value,
) -> Vec<serde_json::Value> {
    let mut content = Vec::new();
    let mut text_buf = String::new();
    for part in &m.content_parts {
        match part {
            ContentPart::Text(t) => text_buf.push_str(t),
            ContentPart::Image(att) => {
                if !text_buf.is_empty() {
                    content.push(serde_json::json!({"type": "text", "text": text_buf}));
                    text_buf.clear();
                }
                let b64 = image_to_base64(&att.asset_path);
                if b64.is_empty() {
                    content
                        .push(serde_json::json!({"type": "text", "text": "[image load failed]"}));
                    continue;
                }
                content.push(image_block(att, &b64));
            }
        }
    }
    if !text_buf.is_empty() {
        content.push(serde_json::json!({"type": "text", "text": text_buf}));
    }
    if content.is_empty() {
        content.push(serde_json::json!({"type": "text", "text": m.content}));
    }
    content
}
