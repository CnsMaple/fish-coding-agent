use super::{ChatEvent, ChatRequest, Provider, ProviderError, ToolCall, Usage};
use crate::config::ProviderKind;
use crate::function::notifications::ModelInfo;
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
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ProviderError::AuthFailed(status.as_u16()).into());
        }
        if status == reqwest::StatusCode::NOT_FOUND
            || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
        {
            return Err(ProviderError::NoModelsEndpoint.into());
        }
        if !status.is_success() {
            return Err(ProviderError::Other(format!("status {}", status)).into());
        }
        let body: AnthropicModelsResp = resp.json().await.map_err(ProviderError::Http)?;
        Ok(body
            .data
            .into_iter()
            .map(|m| ModelInfo {
                id: m.id,
                display: m.display_name.unwrap_or_else(|| "?".to_string()),
                request_id: None,
                context_window_tokens: None,
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
            "tools": crate::tools::anthropic_tool_specs(),
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

let body_bytes: Vec<u8> = {
    let mut last_err = String::new();
    let mut body_result: Option<Vec<u8>> = None;
    for attempt in 0..3 {
        if attempt > 0 {
            let _ = tx.send(ChatEvent::Debug(format!("retry {attempt}/3 after: {last_err}")));
        }
        let resp = match client
            .post(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => { last_err = format!("{e}"); continue; }
        };
        let resp_status = resp.status();
        let resp_ct = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let resp_cl = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).unwrap_or("?").to_string();
        if !resp_status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            last_err = format!("status {} ct={} cl={} body={}", resp_status, resp_ct, resp_cl, text);
            continue;
        }
        match resp.bytes().await {
            Ok(b) => { body_result = Some(b.to_vec()); break; }
            Err(e) => { last_err = format!("bytes fail status={} ct={} cl={}: {}", resp_status, resp_ct, resp_cl, e); continue; }
        }
    }
    match body_result {
        Some(b) => b,
        None => return Err(ProviderError::Other(format!("request failed after 3 retries: {last_err}")).into()),
    }
};
let mut buf = body_bytes;
if buf.is_empty() {
    let _ = tx.send(ChatEvent::Debug("empty response body from server".to_string()));
}
let mut final_usage: Option<Usage> = None;
let mut tool_calls: Vec<ToolCall> = Vec::new();
                    while let Some(pos) = find_sse_boundary(&buf) {
                let raw: Vec<u8> = buf.drain(..pos + 1).collect();
                if let Ok(text) = std::str::from_utf8(&raw) {
                    for line in text.lines() {
                        let line = line.trim_end_matches('\r');
                        if let Some(rest) = line.strip_prefix("data:") {
                            let data = rest.trim();
                            if data.is_empty() {
                                continue;
                            }
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                                match v.get("type").and_then(|t| t.as_str()) {
                                    Some("content_block_start") => {
                                        if v.pointer("/content_block/type").and_then(|t| t.as_str())
                                            == Some("tool_use")
                                        {
                                            merge_tool_use_start(&mut tool_calls, &v);
                                        }
                                    }
                                    Some("content_block_delta") => {
                                        if let Some(delta) = v.pointer("/delta/text") {
                                            if let Some(s) = delta.as_str() {
                                                if !s.is_empty() {
                                                    let _ =
                                                        tx.send(ChatEvent::Delta(s.to_string()));
                                                }
                                            }
                                        }
                                        if let Some(delta) = v.pointer("/delta/thinking") {
                                            if let Some(s) = delta.as_str() {
                                                if !s.is_empty() {
                                                    let _ = tx.send(ChatEvent::ThinkingDelta(
                                                        s.to_string(),
                                                    ));
                                                }
                                            }
                                        }
                                        if let Some(partial) = v
                                            .pointer("/delta/partial_json")
                                            .and_then(|p| p.as_str())
                                        {
                                            merge_tool_use_delta(&mut tool_calls, &v, partial);
                                        }
                                    }
                                    Some("message_delta") => {
                                        if let Some(u) = v.get("usage") {
                                            if let Some(parsed) = parse_anthropic_usage(u) {
                                                final_usage = Some(parsed);
                                            }
                                        }
                                    }
                                    Some("message_stop") => {
                                        if let Some(u) = final_usage.take() {
                                            let _ = tx.send(ChatEvent::Usage(u));
                                        }
                                        let calls = valid_tool_calls(&tool_calls);
                                        if !calls.is_empty() {
                                            let _ = tx.send(ChatEvent::ToolCalls(calls));
                                        }
                                        let _ = tx.send(ChatEvent::Done);
                                        return Ok(());
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
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

    serde_json::json!({"role": m.role, "content": m.content})
}

fn merge_tool_use_start(tool_calls: &mut Vec<ToolCall>, event: &serde_json::Value) {
    let idx = event
        .get("index")
        .and_then(|v| v.as_u64())
        .unwrap_or(tool_calls.len() as u64) as usize;
    while tool_calls.len() <= idx {
        tool_calls.push(ToolCall {
            id: String::new(),
            name: String::new(),
            arguments: String::new(),
        });
    }
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
    while tool_calls.len() <= idx {
        tool_calls.push(ToolCall {
            id: String::new(),
            name: String::new(),
            arguments: String::new(),
        });
    }
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

fn find_sse_boundary(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i);
        }
    }
    for i in 0..buf.len().saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 2);
        }
    }
    None
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
