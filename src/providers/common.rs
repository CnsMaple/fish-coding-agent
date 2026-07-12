use super::{ChatEvent, ProviderError, ToolCall};
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
/// the response body text.
pub(crate) fn chat_response_error(
    status: reqwest::StatusCode,
    ct: &str,
    body: String,
) -> ProviderError {
    ProviderError::Other(format!("status {status} ct={ct} body={body}"))
}
