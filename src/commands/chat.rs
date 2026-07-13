use crate::app::App;
use crate::config::{parse_id, ProviderKind};
use crate::function::notifications::ToastLevel;
use crate::function::SidebarTab;
use crate::providers::{ChatMessage, ChatRequest};
use crate::session::{Message, Role};
use serde_json::json;
use std::time::Duration;

use super::utils::{is_doom_loop, parse_tool_result_display, tool_result_title, parse_text_tool_calls};
use super::{open_settings, build_agents_content, system_prompt, compact_now};
use super::{MSG_REQUEST_IN_FLIGHT, MSG_PROVIDER_INVALID};
pub fn send_chat(app: &mut App, user_text: String, image_parts: Vec<crate::session::ContentPart>) {
    let mut msg = Message::new(Role::User, user_text);
    // Extract ImageAttachment from ContentPart to store on the Message.
    for part in &image_parts {
        if let crate::session::ContentPart::Image(att) = part {
            msg.attachments.push(att.clone());
        }
    }
    send_message(app, msg);
}

/// Dispatch a pre-built user message to the active provider. Used by
/// `/skill:<name>` to send the skill's body (rather than a literal
/// `[skill]` marker) as the user prompt, while the chat UI still
/// renders the marker block via `Message::skill_ref`.
pub fn send_message(app: &mut App, user_msg: Message) {
    if app.inflight.is_some() {
        app.notify(ToastLevel::Warn, MSG_REQUEST_IN_FLIGHT);
        return;
    }
    let Some(active_id) = app.config.active.clone() else {
        app.notify(
            ToastLevel::Fail,
            "no active provider; configure one via /settings",
        );
        open_settings(app);
        return;
    };
    if let Err(e) = app.config.validate_provider(&active_id) {
        app.notify(ToastLevel::Fail, e.clone());
        app.session.push(Message::new(
            Role::System,
            format!("[config error] {e} - open /settings to fix"),
        ));
        if !app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::Settings(_)))
        {
            open_settings(app);
        }
        return;
    }
    let (provider, _mode) = match parse_id(&active_id) {
        Some(p) => p,
        None => {
            app.notify(ToastLevel::Fail, MSG_PROVIDER_INVALID);
            return;
        }
    };
    let base = app
        .config
        .entry(&active_id)
        .map(|c| c.base_url.clone())
        .unwrap_or_default();
    let key: String = match app.config.effective_api_key(&active_id) {
        Some(k) if !k.is_empty() => k,
        _ => {
            let env_name = app
                .config
                .entry(&active_id)
                .map(|c| c.api_key_env.clone())
                .unwrap_or_default();
            app.session.push(Message::new(
                Role::System,
                format!("[no api key for {active_id}: set it via /settings or env {env_name}]"),
            ));
            app.notify(ToastLevel::Fail, format!("missing api key for {active_id}"));
            return;
        }
    };
    let model = app.config.active_model().to_string();
    let thinking = app.config.thinking;

    let user_text = user_msg.content.clone();
    app.maybe_title_from_first_prompt(&user_msg.content);
    app.session.push(user_msg);
    let assistant = Message {
        role: Role::Assistant,
        content: String::new(),
        thinking: String::new(),
        thinking_segments: Vec::new(),
        thinking_visible: false,
        tool_results: Vec::new(),
        tool_calls: Vec::new(),
        attachments: Vec::new(),
        display_cursor: 0,
        line_count: 0,
        cached_content_line_count: None,
        ts: chrono::Utc::now(),
        streaming: true,
        skill_ref: None,
        content_version: 0,
    };
    let id = app.session.push(assistant);
    app.session.streaming_id = Some(id);
    app.response_started_at = None;
    app.response_output_chars = 0;
    app.response_output_tokens = None;

    if app.config.auto_compact
        && app.status.context_window_known
        && !app.compacting
        && app.inflight.is_none()
    {
        let agents = build_agents_content(app);
        let sp = system_prompt(app.active_agent, &agents);
        let msg_texts: Vec<String> = app
            .session
            .messages
            .iter()
            .filter(|m| !matches!(m.role, Role::System))
            .filter(|m| !(matches!(m.role, Role::Assistant) && m.content.is_empty()))
            .map(|m| m.content.clone())
            .collect();
        let inp = crate::compaction::CompactionInputs {
            auto_enabled: app.config.auto_compact,
            ctx_window: app.status.context_window_tokens,
            max_output_tokens: app.status.max_output_tokens,
            reserved_override: app.config.compact_reserved,
        };
        if crate::compaction::compact_if_needed(&msg_texts, &sp, inp) {
            app.notify(ToastLevel::Info, "compacting session before sending...");
            app.pending_post_compaction_prompt = Some(user_text);
            // Remove the user message and assistant placeholder from the
            // session — they will be re-sent by drain_post_compaction_prompt.
            app.session.messages.pop();
            app.session.messages.pop();
            app.session.streaming_id = None;
            app.session.invalidate_layout_cache();
            compact_now(app, "");
            return;
        }
    }

    // Prune pass: clear AI-facing content of old tool results whose
    // cumulative size exceeds the protected budget. The TUI still
    // shows the original content; only the LLM-bound value is swapped.
    crate::compaction::prune(&mut app.session.messages);

    let messages: Vec<ChatMessage> = app
        .session
        .messages
        .iter()
        .filter(|m| !matches!(m.role, Role::System))
        .filter(|m| {
            // Keep assistant messages that have tool_calls
            // (e.g. after plan/ask interaction), even if
            // content is empty. Otherwise filter out empty
            // assistant messages.
            !(matches!(m.role, Role::Assistant) && m.content.is_empty() && m.tool_calls.is_empty())
        })
        .flat_map(|m| {
            let mut msgs: Vec<ChatMessage> = Vec::new();
            let role = match m.role {
                Role::User => "user".to_string(),
                Role::Assistant => "assistant".to_string(),
                Role::System => "user".to_string(),
            };
            if !m.tool_calls.is_empty() {
                // Emit the assistant message with its tool_calls.
                msgs.push(ChatMessage {
                    role: role.clone(),
                    content: m.content.clone(),
                    content_parts: Vec::new(),
                    tool_call_id: None,
                    tool_calls: m.tool_calls.iter().map(|tc| crate::providers::ToolCall {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                    }).collect(),
                });
                // Emit a tool result message for each tool result.
                for tr in &m.tool_results {
                    // Pruned tool outputs are swapped for a compact
                    // placeholder so old read/command results don't
                    // keep consuming context budget in long sessions.
                    let content = if tr.pruned {
                        "[Old tool result content cleared]".to_string()
                    } else {
                        tr.content.clone()
                    };
                    msgs.push(ChatMessage {
                        role: "tool".to_string(),
                        content,
                        content_parts: Vec::new(),
                        tool_call_id: Some(tr.call_id.clone()),
                        tool_calls: Vec::new(),
                    });
                }
            } else {
                // Preserve image attachments on user messages by converting
                // Message::attachments into ChatMessage content_parts so the
                // provider can send them as multimodal content.
                let mut content_parts = Vec::new();
                if !m.content.is_empty() {
                    content_parts.push(crate::session::ContentPart::Text(m.content.clone()));
                }
                for att in &m.attachments {
                    content_parts.push(crate::session::ContentPart::Image(att.clone()));
                }
                msgs.push(ChatMessage {
                    role,
                    content: m.content.clone(),
                    content_parts,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                });
            }
            msgs
        })
        .collect();
    // Bump the request generation. Anything stale from a previous
    // request that slips through after we re-enter (an old chat
    // task still draining, a queued `ChatDone`/`ChatError` from
    // before Esc cleared the inflight, etc.) carries the OLD seq
    // and is filtered out in `handle_msg`.
    app.current_request_seq = app.current_request_seq.wrapping_add(1);
    let seq = app.current_request_seq;

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    app.inflight = Some(crate::app::InflightHandle {
        cancel: cancel_tx,
        label: format!("chat:{active_id}:{model}"),
        seq,
        started_at: std::time::Instant::now(),
    });
    app.cancel_state = crate::function::CancelState::Idle;

    let agents = build_agents_content(app);
    let sp = system_prompt(app.active_agent, &agents);

    let req = ChatRequest {
        model,
        messages,
        thinking,
        system: Some(sp),
        tools: None,
    };

    if let Some(tx) = app.msg_tx.clone() {
let client = app.stream_client.clone();
    let cwd = app.cwd.clone();
    let agent = app.active_agent;
    // Defer the actual `tokio::spawn` until after the next
    // `terminal.draw(...)` returns, so the freshly-pushed user
    // message is on screen before the HTTP request goes out. The
    // main event loop pulls this in `flush_pending_request`.
        app.pending_request = Some(crate::function::PendingRequest::Chat(
            crate::function::ChatPending {
                client,
                base,
                key,
                req,
                provider,
                cwd,
                agent,
                cancel_rx,
                tx,
                seq,
            },
        ));
    }
}

/// Run the chat stream retry loop. Extracted from `send_message` so
/// the same body can be invoked both inline (legacy path) and from
/// `event::flush_pending_request` after the user message has been
/// rendered.
///
/// `req` is consumed and may be mutated across retries (the assistant
/// turn is appended after a successful tool-call round, then popped
/// on a retry so the new request starts clean).
///
/// `seq` is `App::current_request_seq` at the time the request was
/// prepared. It's stamped onto the final `ChatDone`/`ChatError` so
/// `handle_msg` can tell a freshly-completed request from a stale
/// `ChatDone` left over from a previously cancelled request. While
/// the request is running we also gate every `tx.send(...)` on
/// `cancel_rx`: once the user hits Esc we no longer want any of
/// these events to mutate the new state (a partial `Delta`
/// landing in the new assistant message, or worse, a `ChatDone`
/// clearing the freshly-armed inflight during `/continue`).
#[allow(clippy::too_many_arguments)]
pub async fn run_chat_stream(
    client: reqwest::Client,
    base: String,
    key: String,
    mut req: ChatRequest,
    provider: ProviderKind,
    agent: crate::permission::Agent,
    cwd: std::path::PathBuf,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    tx: tokio::sync::mpsc::UnboundedSender<crate::event::AppMsg>,
    seq: u64,
) {
    // Wrap every outbound AppMsg in a cancel check so a stale chat
    // task cannot race with a follow-up request. See the field-level
    // comments in `run_chat_stream` above and `App::current_request_seq`.
    let send_msg = |msg: crate::event::AppMsg| {
        if !*cancel_rx.borrow() {
            let _ = tx.send(msg);
        }
    };
    let mut stream_retries = 0u32;
    let retry_delays = [3u64, 10, 60];
    // Rolling record of recent tool calls (name, arguments) used by
    // the doom-loop detector: when the same tool is invoked 3 times
    // in a row with identical arguments, the loop is broken and the
    // user is asked to intervene. Matches opencode's
    // `DOOM_LOOP_THRESHOLD`.
    let mut doom_history: Vec<(String, String)> = Vec::new();
    loop {
        if *cancel_rx.borrow() {
            // Silent exit. We do NOT send `ChatDone` / `ChatError`
            // here — the Esc handler already cleared local state and
            // `seq` will reject any leftover event from this task if
            // a new request takes over.
            return;
        }
        let (chat_tx, mut chat_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::providers::ChatEvent>();
        let p = crate::providers::provider(provider);
        let client_for_call = client.clone();
        let base_for_call = base.clone();
        let key_for_call = key.clone();
        let req_for_call = crate::providers::ChatRequest {
            model: req.model.clone(),
            messages: req.messages.clone(),
            thinking: req.thinking,
            system: req.system.clone(),
            tools: req.tools.clone(),
        };
        let call = tokio::spawn(async move {
            p.chat_stream(
                &client_for_call,
                &base_for_call,
                &key_for_call,
                req_for_call,
                chat_tx,
            )
            .await
        });

        let mut assistant_content = String::new();
        let mut tool_calls: Vec<crate::providers::ToolCall> = Vec::new();
        let mut stream_done = false;
        while let Some(ev) = chat_rx.recv().await {
            if *cancel_rx.borrow() {
                // Drop the event we just received and exit. The next
                // chat_rx.recv() would block forever on the dead
                // http stream anyway; returning here lets the
                // background `call` task finish on its own.
                return;
            }
            match ev {
                crate::providers::ChatEvent::Delta(s) => {
                    assistant_content.push_str(&s);
                    send_msg(crate::event::AppMsg::ChatDelta(s));
                }
                crate::providers::ChatEvent::ThinkingDelta(s) => {
                    send_msg(crate::event::AppMsg::ChatThinkingDelta(s));
                }
                crate::providers::ChatEvent::Debug(s) => {
                    send_msg(crate::event::AppMsg::ChatDebug(s));
                }
                crate::providers::ChatEvent::Usage(u) => {
                    send_msg(crate::event::AppMsg::ChatUsage { seq, usage: u });
                }
                crate::providers::ChatEvent::ToolResult {
                    name,
                    title,
                    content,
                } => {
                    send_msg(crate::event::AppMsg::ChatToolResult {
                        name,
                        title,
                        content,
                        metadata: String::new(),
                        call_id: String::new(),
                        failed: false,
                    });
                }
                crate::providers::ChatEvent::ToolCalls(calls) => {
                    tool_calls = calls;
                }
                crate::providers::ChatEvent::Done => {
                    stream_done = true;
                    break;
                }
                crate::providers::ChatEvent::Error(e) => {
                    send_msg(crate::event::AppMsg::ChatError { seq, error: e });
                    return;
                }
                crate::providers::ChatEvent::ContentBlockStart(kind) => {
                    send_msg(crate::event::AppMsg::ChatContentBlockStart(kind));
                }
                crate::providers::ChatEvent::ToolArgDelta {
                    index,
                    call_id,
                    name,
                    args,
                } => {
                    send_msg(crate::event::AppMsg::ToolInputDelta {
                        index,
                        call_id,
                        name,
                        args,
                    });
                }
            }
        }

        if !stream_done {
            let err = match call.await {
                Ok(Ok(())) => None,
                // {e:#} shows the full anyhow error chain (surface
                // message + underlying cause like reqwest transport
                // errors) so retry/failure notifications carry enough
                // context to diagnose API or network issues.
                Ok(Err(e)) => Some(format!("{e:#}")),
                Err(e) => Some(format!("chat task failed: {e:#}")),
            };
            if let Some(e) = err {
                let is_rate_limit = e.contains("status 429") || e.contains("insufficient_quota");
                stream_retries += 1;
                if stream_retries >= 3 {
                    // Show the full error chain in the final failure so
                    // the user sees both the surface message and its
                    // underlying cause (e.g. reqwest transport errors).
                    send_msg(crate::event::AppMsg::ChatError { seq, error: e });
                    return;
                }
                let delay = retry_delays[(stream_retries - 1) as usize];
                // Use ChatWarn (Warn level) instead of ChatDebug
                // (Info level) so retry notifications are more visible
                // in the notification list. Use {e:#} to show the full
                // error chain including the underlying cause.
                let warn = if is_rate_limit {
                    format!("rate limit hit, stream retry {stream_retries}/3 (wait {delay}s): {e:#}")
                } else {
                    format!("stream retry {stream_retries}/3 ({delay}s): {e:#}")
                };
                send_msg(crate::event::AppMsg::ChatWarn(warn));
                // If an assistant message was pushed to req (we got tool calls),
                // pop it so the retry starts clean.
                if !tool_calls.is_empty() {
                    req.messages.pop(); // assistant
                }
                tokio::time::sleep(Duration::from_secs(delay)).await;
                continue;
            }
        }
        // Stream completed (either via Done event or graceful EOF
        // without error). Reset the retry counter so a subsequent
        // failure starts from 1/3, not from the stale count.
        stream_retries = 0;

        if tool_calls.is_empty() && !assistant_content.is_empty() {
            let parsed = parse_text_tool_calls(&assistant_content);
            if !parsed.is_empty() {
                tool_calls = parsed;
            }
        }

        if *cancel_rx.borrow() {
            // User cancelled between the inner loop draining the
            // provider's `Done` event and us trying to close it out
            // here. Stay silent.
            return;
        }

        if tool_calls.is_empty() {
            send_msg(crate::event::AppMsg::ChatDone { seq });
            return;
        }

        req.messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: assistant_content,
            content_parts: Vec::new(),
            tool_call_id: None,
            tool_calls: tool_calls.clone(),
        });

        send_msg(crate::event::AppMsg::ChatTimerPause);

        // Doom-loop preflight: detect a tool invoked 3x in a row with
        // identical args across the whole batch (parallel safety) so a
        // stuck repetition does not burn tokens on parallel retries.
        for call in &tool_calls {
            if is_doom_loop(&doom_history, &call.name, &call.arguments) {
                send_msg(crate::event::AppMsg::ChatWarn(format!(
                    "doom loop detected: `{}` called 3x with identical args. Pausing for user review.",
                    call.name
                )));
                send_msg(crate::event::AppMsg::ChatDone { seq });
                return;
            }
        }
        for call in &tool_calls {
            doom_history.push((call.name.clone(), call.arguments.clone()));
        }

        // Split tool calls into parallelizable (everything except the
        // recursive `sub_agent` and the interaction tools `plan`/`ask`)
        // and serial ones. Parallel tools run concurrently; their
        // results are collected and pushed in declaration order so the
        // AI-facing tool messages stay deterministic. `sub_agent` runs
        // serially (it is itself a nested LLM loop). `plan`/`ask` are
        // interaction tools: after they run, the loop yields to the user.
        let is_serial = |name: &str| name == "sub_agent" || name == "plan" || name == "ask";

        // Spawn a task per parallel tool call. Each task owns its own
        // `tx`/`cancel_rx` clones and routes `ToolStarted`/`ToolDelta`/
        // `ChatToolResult` to the correct block via `call_id`.
        let mut parallel_handles: Vec<tokio::task::JoinHandle<(usize, String, String)>> = Vec::new();
        for (i, call) in tool_calls.iter().enumerate() {
            if is_serial(&call.name) {
                continue;
            }
            let call = call.clone();
            let tx = tx.clone();
            let cancel_rx = cancel_rx.clone();
            let cwd = cwd.clone();
            let handle = tokio::spawn(async move {
                let send = |msg: crate::event::AppMsg| {
                    if !*cancel_rx.borrow() {
                        let _ = tx.send(msg);
                    }
                };
                let title = tool_result_title(&call);
                send(crate::event::AppMsg::ToolStarted {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    title: title.clone(),
                });
                let result = crate::tools::execute_tool_streaming_with_agent(
                    agent,
                    &call.name,
                    &call.arguments,
                    &cwd,
                    &call.id,
                    tx.clone(),
                )
                .await;
                let ai_result = crate::tools::strip_metadata(&result);
                let (display, failed) = parse_tool_result_display(&result);
                let metadata = crate::tools::extract_metadata(&result);
                send(crate::event::AppMsg::ChatToolResult {
                    name: call.name.clone(),
                    title,
                    content: display,
                    metadata,
                    call_id: call.id.clone(),
                    failed,
                });
                (i, call.id.clone(), ai_result)
            });
            parallel_handles.push(handle);
        }

        // Await all parallel tasks. Collect (index, call_id, ai_result)
        // then push tool messages in declaration order.
        let mut parallel_results: Vec<(usize, String, String)> = Vec::new();
        for h in parallel_handles {
            if *cancel_rx.borrow() {
                break;
            }
            match h.await {
                Ok(r) => parallel_results.push(r),
                Err(e) => {
                    send_msg(crate::event::AppMsg::ChatWarn(format!(
                        "parallel tool task panicked: {e:#}"
                    )));
                }
            }
        }
        parallel_results.sort_by_key(|r| r.0);
        for (_, call_id, ai_result) in parallel_results {
            req.messages.push(ChatMessage {
                role: "tool".to_string(),
                content: ai_result,
                content_parts: Vec::new(),
                tool_call_id: Some(call_id),
                tool_calls: Vec::new(),
            });
        }

        // Serial tools (sub_agent / plan / ask) run one at a time after
        // the parallel batch. Interaction tools (`plan`/`ask`) stop the
        // auto-continue loop and hand control to the user.
        let mut interaction_tool = false;
        for call in &tool_calls {
            if !is_serial(&call.name) {
                continue;
            }
            let title = tool_result_title(call);
            send_msg(crate::event::AppMsg::ToolStarted {
                call_id: call.id.clone(),
                name: call.name.clone(),
                title: title.clone(),
            });
            let result = if call.name == "sub_agent" {
                run_sub_agent(
                    &client,
                    &base,
                    &key,
                    provider,
                    &req.model,
                    &call.arguments,
                    &cwd,
                    &cancel_rx,
                    &tx,
                )
                .await
            } else {
                crate::tools::execute_tool_streaming_with_agent(
                    agent,
                    &call.name,
                    &call.arguments,
                    &cwd,
                    &call.id,
                    tx.clone(),
                )
                .await
            };
            let ai_result = crate::tools::strip_metadata(&result);
            req.messages.push(ChatMessage {
                role: "tool".to_string(),
                content: ai_result,
                content_parts: Vec::new(),
                tool_call_id: Some(call.id.clone()),
                tool_calls: Vec::new(),
            });
            let (display, failed) = parse_tool_result_display(&result);
            let metadata = crate::tools::extract_metadata(&result);
            send_msg(crate::event::AppMsg::ChatToolResult {
                name: call.name.clone(),
                title,
                content: display,
                metadata,
                call_id: call.id.clone(),
                failed,
            });
            if call.name == "plan" || call.name == "ask" {
                interaction_tool = true;
            }
        }

        // Always persist tool calls to the session so the
        // conversation context (assistant tool_calls + tool
        // results) can be reconstructed for follow-up turns.
        let session_calls: Vec<crate::session::SessionToolCall> = tool_calls
            .iter()
            .map(|c| crate::session::SessionToolCall {
                id: c.id.clone(),
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            })
            .collect();
        send_msg(crate::event::AppMsg::AssistantToolCalls(session_calls));

        // If the model emitted an interaction tool (plan or
        // ask), stop the auto-continue loop and let the user
        // respond. The plan agent surfaces the question in the
        // session; the user types the answer in the input
        // prompt and the conversation resumes.
        if interaction_tool {
            send_msg(crate::event::AppMsg::ChatDone { seq });
            return;
        }
        send_msg(crate::event::AppMsg::ChatTimerResume);
    }
}

/// Spawn a sub-agent conversation loop. The sub-agent runs with filtered
/// tool permissions (e.g. `explore` is read-only, `general` has full
/// access but cannot recurse). Returns a JSON-formatted result string
/// compatible with the tool-result envelope.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_sub_agent(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    provider: ProviderKind,
    model: &str,
    args: &str,
    cwd: &std::path::Path,
    cancel_rx: &tokio::sync::watch::Receiver<bool>,
    tx: &tokio::sync::mpsc::UnboundedSender<crate::event::AppMsg>,
) -> String {
    let args: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": format!("invalid sub_agent args: {e}")}).to_string(),
    };
    let sub_type = args
        .get("subagent_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let Some(sub) = crate::permission::SubAgent::parse(sub_type) else {
        return json!({"ok": false, "error": format!("unknown subagent_type: {sub_type}")}).to_string();
    };
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if prompt.trim().is_empty() {
        return json!({"ok": false, "error": "prompt is empty"}).to_string();
    }

    let _ = tx.send(crate::event::AppMsg::ToolDelta {
        call_id: String::new(),
        content: format!("[sub_agent:{}] starting…\n", sub.as_str()),
    });

    let system_prompt = sub_agent_system_prompt(sub);
    let tools = match provider {
        ProviderKind::Anthropic => crate::tools::anthropic_tool_specs_for_sub_agent(sub),
        _ => crate::tools::openai_tool_specs_for_sub_agent(sub),
    };

    let mut req = crate::providers::ChatRequest {
        model: model.to_string(),
        messages: vec![crate::providers::ChatMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
            content_parts: Vec::new(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }],
        thinking: crate::config::ReasoningMode::Off,
        system: Some(system_prompt),
        tools: Some(tools),
    };

    const MAX_STEPS: usize = 15;
    let retry_delays: [u64; 3] = [3, 10, 60];
    for step in 0..MAX_STEPS {
        if *cancel_rx.borrow() {
            return json!({"ok": false, "error": "sub-agent cancelled"}).to_string();
        }

        let mut text = String::new();
        let mut tool_calls: Vec<crate::providers::ToolCall> = Vec::new();
        let mut stream_retries = 0u32;

        loop {
            if *cancel_rx.borrow() {
                return json!({"ok": false, "error": "sub-agent cancelled"}).to_string();
            }

            let (chat_tx, mut chat_rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::providers::ChatEvent>();
            let p = crate::providers::provider(provider);
            let client_c = client.clone();
            let base_c = base.to_string();
            let key_c = key.to_string();
            let req_c = crate::providers::ChatRequest {
                model: req.model.clone(),
                messages: req.messages.clone(),
                thinking: req.thinking,
                system: req.system.clone(),
                tools: req.tools.clone(),
            };
            let call = tokio::spawn(async move {
                p.chat_stream(&client_c, &base_c, &key_c, req_c, chat_tx).await
            });

            text.clear();
            tool_calls.clear();
            let mut stream_done = false;
            let mut stream_err: Option<String> = None;

            while let Some(ev) = chat_rx.recv().await {
                if *cancel_rx.borrow() {
                    return json!({"ok": false, "error": "sub-agent cancelled"}).to_string();
                }
                match ev {
                    crate::providers::ChatEvent::Delta(s) => text.push_str(&s),
                    crate::providers::ChatEvent::ToolCalls(calls) => tool_calls = calls,
                    crate::providers::ChatEvent::Done => {
                        stream_done = true;
                        break;
                    }
                    crate::providers::ChatEvent::Error(e) => {
                        stream_err = Some(e);
                        break;
                    }
                    _ => {}
                }
            }

            if let Some(e) = stream_err {
                let is_rate_limit = e.contains("status 429") || e.contains("insufficient_quota");
                stream_retries += 1;
                if stream_retries >= 3 {
                    return json!({"ok": false, "error": format!("sub-agent stream error after {stream_retries} retries: {e}")})
                        .to_string();
                }
                let delay = retry_delays[(stream_retries - 1) as usize];
                let label = if is_rate_limit { "rate limit hit" } else { "stream retry" };
                let _ = tx.send(crate::event::AppMsg::ToolDelta {
                    call_id: String::new(),
                    content: format!(
                        "[sub_agent:{}] {label} {stream_retries}/3 ({delay}s): {e:#}\n",
                        sub.as_str()
                    ),
                });
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                continue;
            }

            if !stream_done {
                match call.await {
                    Ok(Ok(())) => {
                        break;
                    }
                    Ok(Err(e)) => {
                        let e_str = format!("{e:#}");
                        let is_rate_limit = e_str.contains("status 429") || e_str.contains("insufficient_quota");
                        stream_retries += 1;
                        if stream_retries >= 3 {
                            return json!({"ok": false, "error": format!("sub-agent stream failed after {stream_retries} retries: {e:#}")})
                                .to_string();
                        }
                        let delay = retry_delays[(stream_retries - 1) as usize];
                        let label = if is_rate_limit { "rate limit hit" } else { "stream retry" };
                        let _ = tx.send(crate::event::AppMsg::ToolDelta {
                            call_id: String::new(),
                            content: format!(
                                "[sub_agent:{}] {label} {stream_retries}/3 ({delay}s): {e:#}\n",
                                sub.as_str()
                            ),
                        });
                        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                        continue;
                    }
                    Err(e) => {
                        let e_str = format!("{e:#}");
                        let is_rate_limit = e_str.contains("status 429") || e_str.contains("insufficient_quota");
                        stream_retries += 1;
                        if stream_retries >= 3 {
                            return json!({"ok": false, "error": format!("sub-agent task failed after {stream_retries} retries: {e:#}")})
                                .to_string();
                        }
                        let delay = retry_delays[(stream_retries - 1) as usize];
                        let label = if is_rate_limit { "rate limit hit" } else { "stream retry" };
                        let _ = tx.send(crate::event::AppMsg::ToolDelta {
                            call_id: String::new(),
                            content: format!(
                                "[sub_agent:{}] {label} {stream_retries}/3 ({delay}s): {e:#}\n",
                                sub.as_str()
                            ),
                        });
                        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                        continue;
                    }
                }
            }

            break;
            }

        if tool_calls.is_empty() {
            let _ = tx.send(crate::event::AppMsg::ToolDelta {
                call_id: String::new(),
                content: format!("[sub_agent:{}] done ({steps} steps)\n", sub.as_str(), steps = step + 1),
            });
            return json!({"ok": true, "result": text}).to_string();
        }

        req.messages.push(crate::providers::ChatMessage {
            role: "assistant".to_string(),
            content: text,
            content_parts: Vec::new(),
            tool_call_id: None,
            tool_calls: tool_calls.clone(),
        });

        for tc in &tool_calls {
            if matches!(
                crate::permission::check_sub_agent(sub, &tc.name),
                crate::permission::Action::Deny
            ) {
                req.messages.push(crate::providers::ChatMessage {
                    role: "tool".to_string(),
                    content: json!({"ok": false, "error": format!("tool `{}` is not allowed for sub-agent `{}`", tc.name, sub.as_str())}).to_string(),
                    content_parts: Vec::new(),
                    tool_call_id: Some(tc.id.clone()),
                    tool_calls: Vec::new(),
                });
                continue;
            }

            let _ = tx.send(crate::event::AppMsg::ToolDelta {
                call_id: String::new(),
                content: format!("[sub_agent:{}] step {step}: {tool}\n", sub.as_str(), step = step + 1, tool = tool_result_title(tc)),
            });

            let result = crate::tools::execute_tool_with_agent(
                crate::permission::Agent::Build,
                &tc.name,
                &tc.arguments,
                cwd,
            )
            .await;

            req.messages.push(crate::providers::ChatMessage {
                role: "tool".to_string(),
                content: result,
                content_parts: Vec::new(),
                tool_call_id: Some(tc.id.clone()),
                tool_calls: Vec::new(),
            });
        }
    }

    json!({"ok": false, "error": format!("sub-agent exceeded max steps ({MAX_STEPS})")}).to_string()
}

pub(super) fn sub_agent_system_prompt(sub: crate::permission::SubAgent) -> String {
    let now = chrono::Local::now();
    let date = now.format("%Y-%m-%d %A").to_string();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let os = crate::tools::os_name();
    let shell = crate::tools::shell_description();
    let shell_details = crate::tools::shell_guidance();

    let base_ctx = format!(
        "Current date: {date}\nOS: {os}\nShell: {shell} ({shell_details})\nWorkspace: {workspace}\nAll file paths are relative to the workspace.",
        date = date,
        os = os,
        shell = shell,
        shell_details = shell_details,
        workspace = cwd,
    );

    match sub {
        crate::permission::SubAgent::General => format!(
            "\
You are a sub-agent handling a delegated task. Work autonomously and return a single \
concise result. Do not ask questions or present plans — just complete the task and \
report back.

{base_ctx}

## Guidelines
- Use the tools available to you to gather information and complete the task.
- Be thorough but efficient. Do not repeat work you have already done.
- When you need to read files, use `read` to get the full content first.
- For large codebases, use `grep` and `glob` to narrow your search before reading.
- Return your findings clearly and concisely. Include file paths and line numbers.
- Do not call the sub_agent tool — you cannot spawn further sub-agents.
- If a tool call fails, try an alternative approach before giving up.",
            base_ctx = base_ctx,
        ),
        crate::permission::SubAgent::Explore => format!(
            "\
You are a fast codebase exploration sub-agent. Your job is to search, read, and \
analyze code. Use grep, glob, read, list, webfetch, and websearch to find information.

{base_ctx}

## Guidelines
- Do not modify files or run commands — you are read-only.
- Start broad with `grep` and `glob`, then narrow down with `read`.
- Be thorough but concise. Return clear, structured findings.
- When searching, try multiple patterns and approaches to be comprehensive.
- Include file paths and line numbers in your findings.
- Do not call the sub_agent tool — you cannot spawn further sub-agents.
- If a search returns no results, try alternative patterns before giving up.",
            base_ctx = base_ctx,
        ),
    }
}

/// System prompt used by the compaction stream. Asks the model to
/// produce a structured summary following the template in
/// `compaction::SUMMARY_TEMPLATE`.
pub(super) fn compaction_system_prompt() -> String {
    "You are a helpful assistant that summarizes conversations. \
Follow the Markdown template provided in the user message exactly. \
Preserve every decision, identifier, file path, and open question \
from the source. Do not use any tools. Reply with the summary \
only — no preamble, no closing remarks."
        .to_string()
}

/// Spawn a one-shot chat stream that summarizes `history`. Used by
/// both auto-compaction and the `/compact` command. The result is
/// delivered as `AppMsg::CompactionSummaryReady { start, end, summary }`
/// (or `AppMsg::CompactionFailed { error }` on error).
///
/// `history` must already be a clone of `Session::messages[start..end]`
/// — the compactor runs entirely on the snapshot, so the live
/// session can be mutated safely in parallel. The cancel channel is
/// independent from the chat inflight handle so the existing
/// inflight-cancel UI does not interfere.
#[allow(clippy::too_many_arguments)]
pub async fn run_compaction_stream(
    client: reqwest::Client,
    base: String,
    key: String,
    provider: ProviderKind,
    model: String,
    history: Vec<crate::session::Message>,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    tx: tokio::sync::mpsc::UnboundedSender<crate::event::AppMsg>,
    start: usize,
    end: usize,
) {
    let send_msg = |msg: crate::event::AppMsg| {
        if !*cancel_rx.borrow() {
            let _ = tx.send(msg);
        }
    };
    if *cancel_rx.borrow() {
        return;
    }
    let history_text: Vec<String> = history
        .iter()
        .map(crate::compaction::serialize_message)
        .filter(|s| !s.is_empty())
        .collect();
    let prompt = crate::compaction::build_prompt(None, &history_text);
    let req = crate::providers::ChatRequest {
        model,
        messages: vec![crate::providers::ChatMessage {
            role: "user".to_string(),
            content: prompt,
            content_parts: Vec::new(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }],
        thinking: crate::config::ReasoningMode::Off,
        system: Some(compaction_system_prompt()),
        tools: None,
    };

    let (chat_tx, mut chat_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::providers::ChatEvent>();
    let p = crate::providers::provider(provider);
    let client_for_call = client.clone();
    let base_for_call = base.clone();
    let key_for_call = key.clone();
    let call = tokio::spawn(async move {
        p.chat_stream(
            &client_for_call,
            &base_for_call,
            &key_for_call,
            req,
            chat_tx,
        )
        .await
    });

    let mut summary = String::new();
    let mut stream_done = false;
    while let Some(ev) = chat_rx.recv().await {
        if *cancel_rx.borrow() {
            return;
        }
        match ev {
            crate::providers::ChatEvent::Delta(s) => {
                summary.push_str(&s);
            }
            crate::providers::ChatEvent::Done => {
                stream_done = true;
                break;
            }
            crate::providers::ChatEvent::Error(e) => {
                send_msg(crate::event::AppMsg::CompactionFailed { error: e });
                return;
            }
            // We do not care about thinking deltas, tool calls, etc.
            // for a compaction summary — drop them.
            _ => {}
        }
    }

    if !stream_done {
        // The provider's task ended without emitting `Done`. Treat
        // any error as a compaction failure so the user gets a
        // meaningful toast.
        let err = match call.await {
            Ok(Ok(())) => "stream closed without Done".to_string(),
            Ok(Err(e)) => format!("{e}"),
            Err(e) => format!("compaction task failed: {e}"),
        };
        send_msg(crate::event::AppMsg::CompactionFailed { error: err });
        return;
    }
    let _ = call.await;
    if summary.trim().is_empty() {
        send_msg(crate::event::AppMsg::CompactionFailed {
            error: "summary was empty".to_string(),
        });
        return;
    }
    send_msg(crate::event::AppMsg::CompactionSummaryReady {
        start,
        end,
        summary,
    });
}
