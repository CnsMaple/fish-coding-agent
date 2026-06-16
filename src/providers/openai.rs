use super::{ChatEvent, ChatRequest, Provider, ProviderError, ToolCall, Usage};
use crate::config::ProviderKind;
use crate::function::notifications::ModelInfo;
use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;
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
    ) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/models", base_url.trim_end_matches('/'));
        let resp = client
            .get(&url)
            .bearer_auth(api_key)
            .send()
            .await
            .map_err(ProviderError::Http)?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ProviderError::AuthFailed(status.as_u16()).into());
        }
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Err(ProviderError::NoModelsEndpoint.into());
        }
        if !status.is_success() {
            return Err(ProviderError::Other(format!("status {}", status)).into());
        }
        let body: ModelsResp = resp.json().await.map_err(ProviderError::Http)?;
        Ok(body
            .data
            .into_iter()
            .map(|m| ModelInfo {
                id: m.id.clone(),
                display: m.id,
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
            "tools": crate::tools::openai_tool_specs(),
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
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Other(format!("status {}: {}", status, text)).into());
        }
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut done = false;
        let mut final_usage: Option<Usage> = None;
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(ProviderError::Http)?;
            buf.extend_from_slice(&chunk);
            // process complete SSE lines
            while let Some(pos) = find_sse_boundary(&buf) {
                let raw: Vec<u8> = buf.drain(..pos + 1).collect();
                if let Ok(text) = std::str::from_utf8(&raw) {
                    for line in text.lines() {
                        let line = line.trim_end_matches('\r');
                        if let Some(rest) = line.strip_prefix("data:") {
                            let data = rest.trim();
                            if data == "[DONE]" {
                                done = true;
                            } else if !data.is_empty() {
                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                                    if let Some(delta) = v.pointer("/choices/0/delta/content") {
                                        if let Some(s) = delta.as_str() {
                                            if !s.is_empty() {
                                                let _ = tx.send(ChatEvent::Delta(s.to_string()));
                                            }
                                        }
                                    }
                                    if let Some(calls) = v.pointer("/choices/0/delta/tool_calls").and_then(|v| v.as_array()) {
                                        merge_tool_call_deltas(&mut tool_calls, calls);
                                    }
                                    if let Some(u) = v.get("usage") {
                                        if let Some(parsed) = parse_openai_usage(u) {
                                            final_usage = Some(parsed);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(u) = final_usage {
            let _ = tx.send(ChatEvent::Usage(u));
        }
        if !tool_calls.is_empty() {
            let _ = tx.send(ChatEvent::ToolCalls(tool_calls));
        }
        if !done {
            // stream closed without [DONE] marker; still treat as done
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
    serde_json::json!({"role": m.role, "content": m.content})
}

fn merge_tool_call_deltas(tool_calls: &mut Vec<ToolCall>, deltas: &[serde_json::Value]) {
    for delta in deltas {
        let idx = delta.get("index").and_then(|v| v.as_u64()).unwrap_or(tool_calls.len() as u64) as usize;
        while tool_calls.len() <= idx {
            tool_calls.push(ToolCall {
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
        }
        let call = &mut tool_calls[idx];
        if let Some(id) = delta.get("id").and_then(|v| v.as_str()) {
            call.id = id.to_string();
        }
        if let Some(name) = delta.pointer("/function/name").and_then(|v| v.as_str()) {
            call.name.push_str(name);
        }
        if let Some(args) = delta.pointer("/function/arguments").and_then(|v| v.as_str()) {
            call.arguments.push_str(args);
        }
    }
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

fn find_sse_boundary(buf: &[u8]) -> Option<usize> {
    // SSE messages are separated by a blank line: \n\n
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i);
        }
    }
    // also accept \r\n\r\n
    for i in 0..buf.len().saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 2);
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct ModelsResp {
    data: Vec<ModelEntry>,
}
#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}
