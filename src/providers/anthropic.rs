use super::common;
use super::{ChatEvent, ChatRequest, Provider, ProviderError, ToolCall, Usage};
use crate::config::ProviderKind;
use crate::function::notifications::ModelInfo;
use crate::net::stream::{drive_sse_stream, SseControl, STREAM_IDLE_TIMEOUT};
use anyhow::Result;
use async_trait::async_trait;

use serde::Deserialize;
use tokio::sync::mpsc;

pub struct AnthropicProvider;

#[async_trait]
impl Provider for AnthropicProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    async fn list_models(
        &self,
        client: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        _access_key: &str,
        _secret_key: &str,
    ) -> Result<Vec<ModelInfo>> {
        // Try the /v1/models endpoint (works for some Anthropic-compatible proxies).
        // For the official Anthropic base URL this returns 404 -> we surface NoModelsEndpoint.
        let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
        let resp = client
            .get(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(ProviderError::Http)?;
        let status = resp.status();
        common::check_list_models_status(status)?;
        let body: AnthropicModelsResp = resp.json().await.map_err(ProviderError::Http)?;
        Ok(body
            .data
            .into_iter()
            .map(|m| ModelInfo {
                id: m.id,
                display: m.display_name.unwrap_or_else(|| "?".to_string()),
                request_id: None,
                context_window_tokens: None,
                context_needs_pick: false,
            })
            .collect())
    }

    async fn chat_stream(
        &self,
        client: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        req: ChatRequest,
        tx: mpsc::UnboundedSender<ChatEvent>,
    ) -> Result<()> {
        let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
        let mut body = serde_json::json!({
            "model": req.model,
            "max_tokens": 8192,
            "stream": true,
            "messages": req.messages.iter().map(anthropic_message).collect::<Vec<_>>(),
            "tools": req.tools.unwrap_or_else(crate::tools::anthropic_tool_specs),
        });
        if let Some(sys) = &req.system {
            body["system"] = serde_json::Value::String(sys.clone());
        }
        if let Some(thinking_type) = req.thinking.anthropic_thinking_type() {
            let mut thinking = serde_json::Map::new();
            thinking.insert("type".into(), thinking_type.into());
            if let Some(budget) = req.thinking.anthropic_budget() {
                thinking.insert("budget_tokens".into(), budget.into());
            }
            body["thinking"] = serde_json::Value::Object(thinking);
        }

        let resp = client
            .post(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(ProviderError::Http)?;
        let (resp_status, resp_ct) = common::response_meta(&resp);
        if !resp_status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            if common::is_rate_limited_error(resp_status, &text) {
                return Err(common::rate_limited_error(text).into());
            }
            return Err(common::chat_response_error(resp_status, &resp_ct, text).into());
        }

        let mut final_usage: Option<Usage> = None;
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        let stream_result = drive_sse_stream(resp, STREAM_IDLE_TIMEOUT, |ev| {
            if ev.data.is_empty() {
                return Ok(SseControl::Continue);
            }
            let v: serde_json::Value = match common::parse_sse_json(&ev, "anthropic", &tx) {
                Some(v) => v,
                None => return Ok(SseControl::Continue),
            };
            let kind = if !ev.event.is_empty() {
                ev.event
            } else {
                v.get("type").and_then(|t| t.as_str()).unwrap_or("")
            };
            match kind {
                "content_block_start" => {
                    let block_type = v
                        .pointer("/content_block/type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    if block_type == "tool_use" {
                        merge_tool_use_start(&mut tool_calls, &v);
                        // Emit initial ToolArgDelta so the tool block
                        // appears immediately in the UI.
                        let idx = v
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(tool_calls.len().saturating_sub(1) as u64)
                            as usize;
                        if idx < tool_calls.len() {
                            let _ = tx.send(ChatEvent::ToolArgDelta {
                                index: idx,
                                call_id: tool_calls[idx].id.clone(),
                                name: tool_calls[idx].name.clone(),
                                args: tool_calls[idx].arguments.clone(),
                            });
                        }
                    }
                    // Notify the session so it can close off the
                    // in-flight thinking segment. This is the
                    // signal that makes per-tool-call thinking
                    // blocks render as separate boxes rather than
                    // collapsing into a single block.
                    if !block_type.is_empty() {
                        let _ = tx.send(ChatEvent::ContentBlockStart(block_type.to_string()));
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = v.pointer("/delta/text") {
                        if let Some(s) = delta.as_str() {
                            if !s.is_empty() {
                                let _ = tx.send(ChatEvent::Delta(s.to_string()));
                            }
                        }
                    }
                    if let Some(delta) = v.pointer("/delta/thinking") {
                        if let Some(s) = delta.as_str() {
                            if !s.is_empty() {
                                let _ = tx.send(ChatEvent::ThinkingDelta(s.to_string()));
                            }
                        }
                    }
                    if let Some(partial) = v.pointer("/delta/partial_json").and_then(|p| p.as_str())
                    {
                        merge_tool_use_delta(&mut tool_calls, &v, partial);
                        // Emit streaming tool arg delta so the UI can show
                        // the command/code text as it arrives.
                        let idx = v.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        if idx < tool_calls.len() {
                            let _ = tx.send(ChatEvent::ToolArgDelta {
                                index: idx,
                                call_id: tool_calls[idx].id.clone(),
                                name: tool_calls[idx].name.clone(),
                                args: tool_calls[idx].arguments.clone(),
                            });
                        }
                    }
                }
                "message_delta" => {
                    if let Some(u) = v.get("usage") {
                        if let Some(parsed) = parse_anthropic_usage(u) {
                            final_usage = Some(parsed);
                        }
                    }
                }
                "message_stop" => {
                    if let Some(u) = final_usage.take() {
                        let _ = tx.send(ChatEvent::Usage(u));
                    }
                    let calls = valid_tool_calls(&tool_calls);
                    if !calls.is_empty() {
                        let _ = tx.send(ChatEvent::ToolCalls(calls));
                    }
                    let _ = tx.send(ChatEvent::Done);
                    return Ok(SseControl::Stop);
                }
                _ => {}
            }
            Ok(SseControl::Continue)
        })
        .await;

        stream_result?;
        if let Some(u) = final_usage {
            let _ = tx.send(ChatEvent::Usage(u));
        }
        let calls = valid_tool_calls(&tool_calls);
        if !calls.is_empty() {
            let _ = tx.send(ChatEvent::ToolCalls(calls));
        }
        let _ = tx.send(ChatEvent::Done);
        Ok(())
    }
}

fn anthropic_message(m: &super::ChatMessage) -> serde_json::Value {
    if m.role == "tool" {
        return serde_json::json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": m.tool_call_id,
                "content": m.content,
            }]
        });
    }

    if !m.tool_calls.is_empty() {
        let mut content = Vec::new();
        if !m.content.is_empty() {
            content.push(serde_json::json!({ "type": "text", "text": m.content }));
        }
        for call in &m.tool_calls {
            let input = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .unwrap_or_else(|_| serde_json::json!({}));
            content.push(serde_json::json!({
                "type": "tool_use",
                "id": call.id,
                "name": call.name,
                "input": input,
            }));
        }
        return serde_json::json!({
            "role": "assistant",
            "content": content,
        });
    }

    // If the message has image content parts, produce a content array
    // with text + image blocks instead of a plain string.
    if !m.content_parts.is_empty() {
        let mut content = Vec::new();
        let mut text_buf = String::new();
        for part in &m.content_parts {
            match part {
                super::ContentPart::Text(t) => text_buf.push_str(t),
                super::ContentPart::Image(att) => {
                    if !text_buf.is_empty() {
                        content.push(serde_json::json!({"type": "text", "text": text_buf}));
                        text_buf.clear();
                    }
                    let b64 = common::image_to_base64(&att.asset_path);
                    if b64.is_empty() {
                        content.push(
                            serde_json::json!({"type": "text", "text": "[image load failed]"}),
                        );
                        continue;
                    }
                    content.push(serde_json::json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": att.media_type,
                            "data": b64,
                        }
                    }));
                }
            }
        }
        if !text_buf.is_empty() {
            content.push(serde_json::json!({"type": "text", "text": text_buf}));
        }
        if content.is_empty() {
            content.push(serde_json::json!({"type": "text", "text": m.content}));
        }
        return serde_json::json!({"role": m.role, "content": content});
    }

    serde_json::json!({"role": m.role, "content": m.content})
}

fn merge_tool_use_start(tool_calls: &mut Vec<ToolCall>, event: &serde_json::Value) {
    let idx = event
        .get("index")
        .and_then(|v| v.as_u64())
        .unwrap_or(tool_calls.len() as u64) as usize;
    common::ensure_tool_slot(tool_calls, idx);
    let call = &mut tool_calls[idx];
    if let Some(id) = event.pointer("/content_block/id").and_then(|v| v.as_str()) {
        call.id = id.to_string();
    }
    if let Some(name) = event
        .pointer("/content_block/name")
        .and_then(|v| v.as_str())
    {
        call.name = name.to_string();
    }
    if let Some(input) = event.pointer("/content_block/input") {
        if input.is_object() && !input.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            call.arguments = input.to_string();
        }
    }
}

fn merge_tool_use_delta(tool_calls: &mut Vec<ToolCall>, event: &serde_json::Value, partial: &str) {
    let idx = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    common::ensure_tool_slot(tool_calls, idx);
    tool_calls[idx].arguments.push_str(partial);
}

fn valid_tool_calls(tool_calls: &[ToolCall]) -> Vec<ToolCall> {
    tool_calls
        .iter()
        .filter(|call| !call.id.is_empty() && !call.name.is_empty())
        .cloned()
        .collect()
}

fn parse_anthropic_usage(v: &serde_json::Value) -> Option<Usage> {
    let mut u = Usage::default();
    if let Some(n) = v.get("input_tokens").and_then(|x| x.as_u64()) {
        u.input_tokens = n;
    }
    if let Some(n) = v.get("output_tokens").and_then(|x| x.as_u64()) {
        u.output_tokens = n;
    }
    if let Some(n) = v.get("cache_read_input_tokens").and_then(|x| x.as_u64()) {
        u.cache_read_tokens = n;
    }
    if let Some(n) = v
        .get("cache_creation_input_tokens")
        .and_then(|x| x.as_u64())
    {
        u.cache_creation_tokens = n;
    }
    Some(u)
}

#[derive(Debug, Deserialize)]
struct AnthropicModelsResp {
    data: Vec<AnthropicModelEntry>,
}
#[derive(Debug, Deserialize)]
struct AnthropicModelEntry {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
}
