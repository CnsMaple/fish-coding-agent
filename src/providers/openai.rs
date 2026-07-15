use super::common;
use super::{ChatEvent, ChatRequest, Provider, ProviderError, ToolCall, Usage};
use crate::config::ProviderKind;
use crate::function::notifications::ModelInfo;
use crate::net::stream::{drive_sse_stream, SseControl, STREAM_IDLE_TIMEOUT};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::mpsc;

pub struct OpenAiProvider;

#[async_trait]
impl Provider for OpenAiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Openai
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
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let mut body = serde_json::json!({
            "model": req.model,
            "stream": true,
            "stream_options": { "include_usage": true },
            "messages": req.messages.iter().map(openai_message).collect::<Vec<_>>(),
            "tools": req.tools.unwrap_or_else(crate::tools::openai_tool_specs),
            "tool_choice": "auto",
        });
        if let Some(sys) = &req.system {
            if let Some(arr) = body["messages"].as_array_mut() {
                arr.insert(0, serde_json::json!({"role": "system", "content": sys}));
            }
        }
        if let Some(effort) = req.thinking.openai_effort() {
            body["reasoning_effort"] = serde_json::Value::String(effort.to_string());
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
                return Err(ProviderError::RateLimited(text).into());
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
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        // Tracks the kind of the most recent block we emitted a
        // delta for ("thinking" or "text") so we can fire a
        // `ContentBlockStart` event when the upstream switches
        // between reasoning and final-answer deltas. Without this
        // the session would merge every reasoning segment that
        // shares the same content offset into a single block.
        let mut last_block_kind: Option<&'static str> = None;

        let stream_result = drive_sse_stream(resp, STREAM_IDLE_TIMEOUT, |ev| {
            if ev.data == "[DONE]" {
                return Ok(SseControl::Stop);
            }
            if ev.data.is_empty() {
                return Ok(SseControl::Continue);
            }
            let v: serde_json::Value = match common::parse_sse_json(&ev, "openai", &tx) {
                Some(v) => v,
                None => return Ok(SseControl::Continue),
            };
            if let Some(delta) = v.pointer("/choices/0/delta/content") {
                if let Some(s) = delta.as_str() {
                    if !s.is_empty() {
                        // Transition: reasoning → text. Close off
                        // the in-flight thinking segment so the
                        // session starts a fresh one on the next
                        // reasoning delta.
                        if last_block_kind == Some("thinking") {
                            let _ = tx.send(ChatEvent::ContentBlockStart("text".to_string()));
                        }
                        last_block_kind = Some("text");
                        let _ = tx.send(ChatEvent::Delta(s.to_string()));
                    }
                }
            }
            if let Some(reasoning) = v.pointer("/choices/0/delta/reasoning_content") {
                if let Some(s) = reasoning.as_str() {
                    if !s.is_empty() {
                        if last_block_kind == Some("text") {
                            let _ = tx.send(ChatEvent::ContentBlockStart("thinking".to_string()));
                        }
                        last_block_kind = Some("thinking");
                        let _ = tx.send(ChatEvent::ThinkingDelta(s.to_string()));
                    }
                }
            }
            if let Some(calls) = v
                .pointer("/choices/0/delta/tool_calls")
                .and_then(|v| v.as_array())
            {
                if last_block_kind == Some("thinking") {
                    let _ = tx.send(ChatEvent::ContentBlockStart("tool_use".to_string()));
                }
                last_block_kind = Some("tool_use");
                let changed = merge_tool_call_deltas(&mut tool_calls, calls);
                // Emit streaming tool arg deltas so the UI can show
                // command/code text as it arrives from the LLM. Only
                // emit for the slots that actually changed in this
                // delta (not every slot on every delta — otherwise
                // parallel tool calls cause the session to create
                // duplicate placeholder blocks).
                for idx in changed {
                    if idx < tool_calls.len() && !tool_calls[idx].name.is_empty() {
                        let _ = tx.send(ChatEvent::ToolArgDelta {
                            index: idx,
                            call_id: tool_calls[idx].id.clone(),
                            name: tool_calls[idx].name.clone(),
                            args: tool_calls[idx].arguments.clone(),
                        });
                    }
                }
            }
            if let Some(u) = v.get("usage") {
                if let Some(parsed) = parse_openai_usage(u) {
                    final_usage = Some(parsed);
                }
            }
            Ok(SseControl::Continue)
        })
        .await;

        stream_result?;
        if let Some(u) = final_usage {
            let _ = tx.send(ChatEvent::Usage(u));
        }
        if !tool_calls.is_empty() {
            let _ = tx.send(ChatEvent::ToolCalls(tool_calls));
        }
        let _ = tx.send(ChatEvent::Done);
        Ok(())
    }
}

fn openai_message(m: &super::ChatMessage) -> serde_json::Value {
    if m.role == "tool" {
        return serde_json::json!({
            "role": "tool",
            "tool_call_id": m.tool_call_id,
            "content": m.content,
        });
    }
    if !m.tool_calls.is_empty() {
        return serde_json::json!({
            "role": m.role,
            "content": if m.content.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(m.content.clone()) },
            "tool_calls": m.tool_calls.iter().map(|call| serde_json::json!({
                "id": call.id,
                "type": "function",
                "function": {
                    "name": call.name,
                    "arguments": call.arguments,
                }
            })).collect::<Vec<_>>(),
        });
    }
    // If the message has image content parts, produce a content array
    // with text + image_url blocks instead of a plain string.
    if !m.content_parts.is_empty() {
        let mut content = Vec::new();
        let mut text_buf = String::new();
        for part in &m.content_parts {
            match part {
                super::ContentPart::Text(t) => text_buf.push_str(t),
                super::ContentPart::Image(att) => {
                    // Flush accumulated text first.
                    if !text_buf.is_empty() {
                        content.push(serde_json::json!({"type": "text", "text": text_buf}));
                        text_buf.clear();
                    }
                    // Read the image file and base64-encode it.
                    let b64 = common::image_to_base64(&att.asset_path);
                    if b64.is_empty() {
                        content.push(
                            serde_json::json!({"type": "text", "text": "[image load failed]"}),
                        );
                    } else {
                        let url = format!("data:{};base64,{}", att.media_type, b64);
                        content.push(serde_json::json!({
                            "type": "image_url",
                            "image_url": { "url": url }
                        }));
                    }
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

/// Merge OpenAI streaming `tool_calls` deltas into the accumulated
/// `tool_calls` vector. Returns the indices of slots that were created
/// or updated in this delta, so callers can emit `ToolArgDelta` only
/// for changed slots (emitting for every slot on every delta would
/// make the session create duplicate placeholder blocks for parallel
/// tool calls).
fn merge_tool_call_deltas(
    tool_calls: &mut Vec<ToolCall>,
    deltas: &[serde_json::Value],
) -> Vec<usize> {
    let mut changed = Vec::new();
    for delta in deltas {
        let idx = delta
            .get("index")
            .and_then(|v| v.as_u64())
            .unwrap_or(tool_calls.len() as u64) as usize;
        common::ensure_tool_slot(tool_calls, idx);
        let call = &mut tool_calls[idx];
        if let Some(id) = delta.get("id").and_then(|v| v.as_str()) {
            call.id = id.to_string();
        }
        if let Some(name) = delta.pointer("/function/name").and_then(|v| v.as_str()) {
            call.name.push_str(name);
        }
        if let Some(args) = delta
            .pointer("/function/arguments")
            .and_then(|v| v.as_str())
        {
            call.arguments.push_str(args);
        }
        changed.push(idx);
    }
    changed
}

fn parse_openai_usage(v: &serde_json::Value) -> Option<Usage> {
    let mut u = Usage::default();
    if let Some(n) = v.get("prompt_tokens").and_then(|x| x.as_u64()) {
        u.input_tokens = n;
    }
    if let Some(n) = v.get("completion_tokens").and_then(|x| x.as_u64()) {
        u.output_tokens = n;
    }
    if let Some(n) = v
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(|x| x.as_u64())
    {
        u.cache_read_tokens = n;
    }
    Some(u)
}

#[derive(Debug, Deserialize)]
struct ModelsResp {
    data: Vec<ModelEntry>,
}
#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}
