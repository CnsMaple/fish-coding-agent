use super::common;
use super::{ChatEvent, ChatRequest, Provider, ProviderError, ToolCall, Usage};
use crate::config::ProviderKind;
use crate::function::notifications::ModelInfo;
use crate::net::stream::{drive_sse_stream, SseControl, STREAM_IDLE_TIMEOUT};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;
use tokio::sync::mpsc;

/// OpenAI-compatible `Responses` API provider (e.g. DeepSeek's
/// `/responses` endpoint). Uses the semantic SSE event stream
/// (`event:` named events) rather than the chat/completions
/// `/choices/0/delta/...` shape.
pub struct ResponsesProvider;

#[async_trait]
impl Provider for ResponsesProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenaiResponses
    }

    async fn list_models(
        &self,
        client: &reqwest::Client,
        base_url: &str,
        api_key: &str,
        _access_key: &str,
        _secret_key: &str,
    ) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/models", base_url.trim_end_matches('/'));
        let resp = client
            .get(&url)
            .bearer_auth(api_key)
            .send()
            .await
            .map_err(ProviderError::Http)?;
        let status = resp.status();
        common::check_list_models_status(status)?;
        let body: ModelsResp = resp.json().await.map_err(ProviderError::Http)?;
        Ok(body
            .data
            .into_iter()
            .map(|m| ModelInfo {
                id: m.id.clone(),
                display: m.id,
                request_id: None,
                context_window_tokens: None,
                context_needs_pick: false,
                modalities: Vec::new(),
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
        let url = format!("{}/responses", base_url.trim_end_matches('/'));
        let mut input: Vec<serde_json::Value> = Vec::new();

        // Prefix messages form the stable cache prefix.
        if !req.prefix_messages.is_empty() {
            for pm in &req.prefix_messages {
                if let Some(item) = responses_input_item(pm) {
                    input.push(item);
                }
            }
            input.push(serde_json::json!({
                "type": "message",
                "role": "user",
                "content": "[End of cached context. Continue below.]",
            }));
        }

        for m in &req.messages {
            if let Some(item) = responses_input_item(m) {
                input.push(item);
            }
        }

        let mut body = serde_json::json!({
            "model": req.model,
            "stream": true,
            "input": input,
            "tools": req.tools.unwrap_or_else(crate::tools::responses_tool_specs),
            "tool_choice": "auto",
        });
        if let Some(sys) = &req.system {
            body["instructions"] = serde_json::Value::String(sys.clone());
        }
        if let Some(effort) = req.thinking.openai_effort() {
            body["reasoning"] = serde_json::json!({ "effort": effort.to_string() });
        }

        let resp = client
            .post(&url)
            .bearer_auth(api_key)
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
        if !resp_ct.is_empty()
            && !resp_ct.contains("text/event-stream")
            && !resp_ct.contains("application/json")
        {
            let text = resp.text().await.unwrap_or_default();
            return Err(
                ProviderError::Other(format!("unexpected ct={} body={}", resp_ct, text)).into(),
            );
        }

        let mut final_usage: Option<Usage> = None;
        // Track in-flight tool calls keyed by their `output_index` so
        // parallel calls stay distinct. `call_id`/`name` are filled in
        // by `response.output_item.added`; arguments accumulate via
        // `response.function_call_arguments.delta`.
        let mut tool_calls: HashMap<usize, ToolCall> = HashMap::new();
        let mut last_block_kind: Option<&'static str> = None;
        let mut saw_terminal = false;

        let stream_result = drive_sse_stream(resp, STREAM_IDLE_TIMEOUT, |ev| {
            if ev.data.is_empty() {
                return Ok(SseControl::Continue);
            }
            let v: serde_json::Value = match common::parse_sse_json(&ev, "responses", &tx) {
                Some(v) => v,
                None => return Ok(SseControl::Continue),
            };
            let kind = if !ev.event.is_empty() {
                ev.event
            } else {
                v.get("type").and_then(|t| t.as_str()).unwrap_or("")
            };
            match kind {
                "response.output_item.added" => {
                    let item_type = v
                        .pointer("/item/type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    if item_type == "function_call" {
                        if last_block_kind == Some("thinking") {
                            let _ = tx.send(ChatEvent::ContentBlockStart("tool_use".to_string()));
                        }
                        last_block_kind = Some("tool_use");
                        let idx =
                            v.get("output_index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                        let call = tool_calls.entry(idx).or_insert_with(|| ToolCall {
                            id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                        });
                        if let Some(id) = v.pointer("/item/id").and_then(|x| x.as_str()) {
                            call.id = id.to_string();
                        }
                        if let Some(name) = v.pointer("/item/name").and_then(|x| x.as_str()) {
                            call.name = name.to_string();
                        }
                        // Emit initial ToolArgDelta so the tool block
                        // appears immediately.
                        if !call.name.is_empty() {
                            let _ = tx.send(ChatEvent::ToolArgDelta {
                                index: idx,
                                call_id: call.id.clone(),
                                name: call.name.clone(),
                                args: call.arguments.clone(),
                            });
                        }
                    }
                }
                "response.reasoning_text.delta" => {
                    if let Some(s) = v.get("delta").and_then(|x| x.as_str()) {
                        if !s.is_empty() {
                            if last_block_kind == Some("text")
                                || last_block_kind == Some("tool_use")
                            {
                                let _ =
                                    tx.send(ChatEvent::ContentBlockStart("thinking".to_string()));
                            }
                            last_block_kind = Some("thinking");
                            let _ = tx.send(ChatEvent::ThinkingDelta(s.to_string()));
                        }
                    }
                }
                "response.output_text.delta" => {
                    if let Some(s) = v.get("delta").and_then(|x| x.as_str()) {
                        if !s.is_empty() {
                            if last_block_kind == Some("thinking") {
                                let _ = tx.send(ChatEvent::ContentBlockStart("text".to_string()));
                            }
                            last_block_kind = Some("text");
                            let _ = tx.send(ChatEvent::Delta(s.to_string()));
                        }
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let Some(delta) = v.get("delta").and_then(|x| x.as_str()) {
                        if last_block_kind == Some("thinking") {
                            let _ = tx.send(ChatEvent::ContentBlockStart("tool_use".to_string()));
                        }
                        last_block_kind = Some("tool_use");
                        let idx =
                            v.get("output_index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                        let call = tool_calls.entry(idx).or_insert_with(|| ToolCall {
                            id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                        });
                        if let Some(id) = v.get("item_id").and_then(|x| x.as_str()) {
                            if call.id.is_empty() {
                                call.id = id.to_string();
                            }
                        }
                        call.arguments.push_str(delta);
                        if !call.name.is_empty() {
                            let _ = tx.send(ChatEvent::ToolArgDelta {
                                index: idx,
                                call_id: call.id.clone(),
                                name: call.name.clone(),
                                args: call.arguments.clone(),
                            });
                        }
                    }
                }
                "response.completed" => {
                    if let Some(u) = v.pointer("/response/usage") {
                        if let Some(parsed) = parse_usage(u) {
                            final_usage = Some(parsed);
                        }
                    }
                    emit_done(&tx, &tool_calls, &mut final_usage);
                    saw_terminal = true;
                    return Ok(SseControl::Stop);
                }
                "response.incomplete" => {
                    if let Some(u) = v.pointer("/response/usage") {
                        if let Some(parsed) = parse_usage(u) {
                            final_usage = Some(parsed);
                        }
                    }
                    emit_done(&tx, &tool_calls, &mut final_usage);
                    saw_terminal = true;
                    return Ok(SseControl::Stop);
                }
                "response.failed" => {
                    let msg = v
                        .pointer("/response/error/message")
                        .and_then(|x| x.as_str())
                        .unwrap_or("response failed");
                    let _ = tx.send(ChatEvent::Error(msg.to_string()));
                    saw_terminal = true;
                    return Ok(SseControl::Stop);
                }
                _ => {}
            }
            Ok(SseControl::Continue)
        })
        .await;

        stream_result?;
        if !saw_terminal {
            emit_done(&tx, &tool_calls, &mut final_usage);
        }
        Ok(())
    }
}

/// Convert a `ChatMessage` into a Responses API input item. Returns
/// `None` for roles that cannot be represented (handled by caller).
fn responses_input_item(m: &super::ChatMessage) -> Option<serde_json::Value> {
    if m.role == "tool" {
        // Tool results are `function_call_output` items.
        return Some(serde_json::json!({
            "type": "function_call_output",
            "call_id": m.tool_call_id,
            "output": m.content,
        }));
    }
    if !m.tool_calls.is_empty() {
        // Assistant messages with tool calls become a message item plus
        // adjacent `function_call` items.
        let mut content = Vec::new();
        if !m.content.is_empty() {
            content.push(serde_json::json!({ "type": "output_text", "text": m.content }));
        }
        let mut items = Vec::new();
        if !content.is_empty() {
            items.push(serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": content,
            }));
        }
        for call in &m.tool_calls {
            items.push(serde_json::json!({
                "type": "function_call",
                "call_id": call.id,
                "name": call.name,
                "arguments": call.arguments,
            }));
        }
        return if items.is_empty() {
            None
        } else if items.len() == 1 {
            Some(items.pop().unwrap())
        } else {
            Some(serde_json::json!({ "type": "items", "items": items }))
        };
    }
    Some(serde_json::json!({
        "type": "message",
        "role": m.role,
        "content": m.content,
    }))
}

fn parse_usage(v: &serde_json::Value) -> Option<Usage> {
    let mut u = Usage::default();
    if let Some(n) = v.get("input_tokens").and_then(|x| x.as_u64()) {
        u.input_tokens = n;
    }
    if let Some(n) = v.get("output_tokens").and_then(|x| x.as_u64()) {
        u.output_tokens = n;
    }
    if let Some(n) = v
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(|x| x.as_u64())
    {
        u.cache_read_tokens = n;
    }
    Some(u)
}

fn emit_done(
    tx: &mpsc::UnboundedSender<ChatEvent>,
    tool_calls: &HashMap<usize, ToolCall>,
    final_usage: &mut Option<Usage>,
) {
    if let Some(u) = final_usage.take() {
        let _ = tx.send(ChatEvent::Usage(u));
    }
    let calls: Vec<ToolCall> = tool_calls
        .values()
        .filter(|c| !c.id.is_empty() && !c.name.is_empty())
        .cloned()
        .collect();
    if !calls.is_empty() {
        let _ = tx.send(ChatEvent::ToolCalls(calls));
    }
    let _ = tx.send(ChatEvent::Done);
}

#[derive(Debug, Deserialize)]
struct ModelsResp {
    data: Vec<ModelEntry>,
}
#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}
