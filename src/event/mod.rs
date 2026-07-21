use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::backend::Backend;
use ratatui::Terminal;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::interval;

use crate::app::App;
use crate::function::CancelState;
use crate::ui::screen_y_to_doc_line;

mod mcp;
mod paste;
mod pickers;

use mcp::*;
use paste::*;
use pickers::*;

#[cfg(test)]
mod tests;

/// Async -> main loop messages.
pub enum AppMsg {
    /// A piece of streamed chat delta arrived.
    ChatDelta(String),
    /// A piece of thinking delta (Anthropic "thinking_delta") arrived.
    ChatThinkingDelta(String),
    /// Provider signals a new content block has started in the upstream
    /// stream (Anthropic `content_block_start` for thinking/text/
    /// tool_use, or a reasoning→text transition in OpenAI / Cursor).
    /// The session closes off the in-flight thinking segment so the
    /// next thinking delta lands in a fresh block. The string is the
    /// block kind ("thinking", "text", "tool_use", ...).
    ChatContentBlockStart(String),
    /// Provider-level debug event, shown only in notifications.
    ChatDebug(String),
    /// Stream-level warning event (e.g. retry), shown in notifications
    /// at Warn level so it is more visible than Info-level debug events.
    ChatWarn(String),
    /// Remove the most recent notification whose text contains the
    /// given substring. Used to clean up rate-limit retry warnings once
    /// the retry succeeds.
    ChatWarnClear(String),
    /// A structured tool result arrived, to be rendered as a collapsible block.
    ChatToolResult {
        name: String,
        title: String,
        content: String,
        /// UI-only structured payload (e.g. `edit_diff` JSON for
        /// edit/write). Empty for tools without metadata. Never sent
        /// to the AI — only stored on `ToolResultBlock.metadata` for
        /// the TUI renderer.
        metadata: String,
        /// The tool call id that produced this result.
        call_id: String,
        /// `true` when the tool returned `{"ok": false, ...}`.
        failed: bool,
    },
    /// Tool calls emitted by the assistant. Stored in the session so
    /// the conversation context can be reconstructed for follow-up
    /// turns (e.g. after plan/ask interaction tools pause the loop).
    AssistantToolCalls(Vec<crate::session::SessionToolCall>),
    LocalToolResult {
        name: String,
        title: String,
        content: String,
        metadata: String,
        context: Option<String>,
        failed: bool,
    },
    /// Final usage arrived for a completed stream.
    ChatUsage {
        seq: u64,
        usage: crate::providers::Usage,
    },
    /// Stream finished successfully. `seq` matches
    /// `App::current_request_seq` at the time the request started;
    /// the handler drops stale events from previous requests so a
    /// slow-finishing background task can't clobber the new inflight.
    ChatDone {
        seq: u64,
    },
    /// Stream errored. See `ChatDone` for the `seq` semantics.
    ChatError {
        seq: u64,
        error: String,
    },
    /// Models list fetched successfully.
    ModelsFetched {
        provider: crate::config::ProviderKind,
        base_url: String,
        api_key: String,
        models: Vec<crate::function::notifications::ModelInfo>,
    },
    /// Model fetch failed.
    ModelsFetchFailed {
        provider: crate::config::ProviderKind,
        error: String,
        no_endpoint: bool,
    },
    CursorAuthSucceeded {
        access_token: String,
        refresh_token: String,
    },
    CursorAuthFailed(String),
    /// Pause the model-output timer (during tool calls).
    ChatTimerPause,
    /// Resume the model-output timer (after tool calls).
    ChatTimerResume,
    /// A tool has started executing (creates a placeholder block).
    /// `call_id` is the stable identity matching the LLM tool call
    /// so parallel tool execution routes results to the correct block.
    ToolStarted {
        call_id: String,
        name: String,
        title: String,
    },
    /// Incremental output from a running tool. `call_id` routes the
    /// delta to the correct block during parallel execution.
    ToolDelta {
        call_id: String,
        content: String,
    },
    /// Streaming tool-call arguments from the LLM. `args` is the
    /// full accumulated JSON arguments string so far. The session
    /// stores this on `ToolResultBlock.streaming_input` so the
    /// renderer can show the command/code/edit text as it arrives.
    /// `index` is the tool-call slot (OpenAI tool_call index /
    /// Anthropic content_block index); `call_id` is the stable id.
    ToolInputDelta {
        index: usize,
        call_id: String,
        name: String,
        args: String,
    },
    /// MCP tool list changed for a single server (added, removed,
    /// or server went up/down). Triggers re-aggregation of the
    /// `openai_tool_specs` / `anthropic_tool_specs` view and an
    /// immediate redraw of the status bar.
    McpToolsChanged {
        server: String,
    },
    /// MCP status changed for a server. Used to refresh the
    /// `/mcp` picker without forcing a re-aggregation of tools.
    McpStatusChanged {
        name: String,
        status: crate::mcp::McpStatus,
    },
    /// An MCP server needs user authentication. `url` is the
    /// authorization URL the app should surface in a toast (and
    /// open in the browser if possible).
    McpAuthRequired {
        server: String,
        url: String,
        error: String,
    },
    /// The browser failed to open for an MCP auth URL; the TUI
    /// already showed the URL in a toast, so the user can copy it.
    McpBrowserOpenFailed {
        server: String,
        url: String,
    },
    /// A connected MCP server's client closed unexpectedly. The
    /// service has already marked the server as `Failed`; the
    /// TUI uses this to surface a toast and update the picker.
    McpClientClosed {
        server: String,
    },
    /// Manual request to start the OAuth dance for a remote MCP
    /// server. Issued by `/mcp-auth <name>`.
    McpStartAuth {
        server: String,
    },
    /// Auto or `/compact` finished: the LLM returned a summary for
    /// the slice `Session::messages[start..keep_start]`. The handler
    /// calls `Session::apply_compaction` with the kept window
    /// `keep_start..end` (real messages that follow the summary).
    CompactionSummaryReady {
        start: usize,
        end: usize,
        keep_start: usize,
        summary: String,
    },
    /// The compaction stream errored out. The session is left
    /// untouched. Surfaces as a `Fail` toast.
    CompactionFailed {
        error: String,
    },
}

pub struct EventChannels {
    pub tx: UnboundedSender<AppMsg>,
    pub rx: UnboundedReceiver<AppMsg>,
}

impl EventChannels {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self { tx, rx }
    }
}

impl Default for EventChannels {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn run<B>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()>
where
    B: Backend,
{
    let mut channels = EventChannels::new();
    // We need to put the sender into the App so spawned tasks can use it.
    app.msg_tx = Some(channels.tx.clone());
    app.check_config();

    // Wire the MCP service (if installed) into the app's event
    // channel so tool-list changes surface as `AppMsg`s.
    if let Some(svc) = crate::mcp::McpRegistry::current() {
        let tx_for_mcp = channels.tx.clone();
        let sink = crate::mcp::AppMsgEventSink::new(tx_for_mcp);
        svc.bind_event_sink(std::sync::Arc::new(sink)).await;
    }
    refresh_mcp_summary(app);

    // Eagerly fetch models.dev data in the background so the
    // provider list in /settings is populated when the user opens it.
    {
        let model_data_path = app
            .model_cache_path
            .parent()
            .unwrap_or(&app.model_cache_path)
            .join("model-data.json");
        let client = app.reqwest.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::model_data::fetch_models_dev(&client, &model_data_path).await {
                tracing::debug!("background models.dev fetch: {e}");
            }
        });
    }

    let mut events = EventStream::new();
    let mut tick = interval(Duration::from_millis(100));
    // Faster tick dedicated to scrolling momentum. ~60fps so the
    // motion looks smooth.
    let mut scroll_tick = interval(Duration::from_millis(SCROLL_ANIM_TICK_MS));
    let mut last_status_refresh = std::time::Instant::now();
    let mut needs_draw = true;
    let mut prev_scroll: Option<u32> = None;
    let mut last_draw = Instant::now();
    // Minimum interval between draws (~60 fps).
    const DRAW_INTERVAL: Duration = Duration::from_millis(16);

    loop {
        // Throttled draw: cap at ~60 fps via DRAW_INTERVAL. During
        // inflight we previously skipped the throttle, but SSE delta
        // chunks arrive far faster than 60 fps and each draw re-parses
        // the growing streaming message through Markdown twice — that
        // was the dominant CPU + stdout-write hotspot. Discrete
        // events (parallel tool results) are now coalesced into the
        // next 16 ms frame, which is imperceptible. The 100 ms tick
        // keeps the spinner animating even when no deltas arrive.
        if needs_draw && last_draw.elapsed() >= DRAW_INTERVAL {
            if let Err(e) = terminal.draw(|f| crate::ui::render(f, app)) {
                let _ = e;
            }
            // The CursorTrackingBackend wrapper de-duplicates cursor
            // visibility commands, so the terminal's native blink timer
            // is not reset on every frame.
            last_draw = Instant::now();
            needs_draw = false;

            // The freshly-pushed user message and (for tools) the
            // pending tool block are now on screen. Kick off the
            // deferred request — see `submit_input` /
            // `commands::send_message` for the producer side.
            flush_pending_request(app);
            drain_post_compaction_prompt(app);

            prev_scroll = Some(app.session.scroll);
        }

        tokio::select! {
                    biased;
                    evt = events.next() => {
                        needs_draw = true;
                        let Some(evt) = evt else { break; };
                        match evt? {
                            Event::Key(k) if k.kind == KeyEventKind::Press => {
                                // Intercept Alt+V to open paste preview.
                                let is_alt_v = k.modifiers.contains(KeyModifiers::ALT)
                                    && matches!(k.code, KeyCode::Char('v') | KeyCode::Char('V'));
                                if is_alt_v {
                                    open_paste_preview(app);
                                    continue;
                                }

                                // All other key events go through handle_key directly.
                                // Burst paste detection is removed in favor of the
                                // explicit paste preview panel.
                                for k in try_consume_burst(k, &mut events).await {
                                    handle_key(k, app).await;
                                }
                            }
                            Event::Mouse(m) => {
                                handle_mouse(m, app);
                            }
                            Event::Paste(text) => {
                                handle_paste(text, app).await;
                            }
        Event::Resize(_, _) => {}
                            _ => {}
                        }
                    }
                    msg = channels.rx.recv() => {
                        needs_draw = true;
                        if let Some(m) = msg { handle_msg(m, app); }
                    }
                    _ = scroll_tick.tick() => {
                        // Clear the 1-frame gating window set by the most
                        // recent wheel event. In instant-scroll mode this is
                        // a no-op for the view (the view already jumped on
                        // the event frame) — it only re-arms `animating` to
                        // `false` so the next wheel event can start a new
                        // gesture.
                        if app.session_scroll.animating {
                            let _ = app.session_scroll.step(Instant::now());
                        }
                        if app.input_scroll.animating {
                            let _ = app.input_scroll.step(Instant::now());
                        }
                    }
                    _ = tick.tick() => {
                        // Always render on tick while inflight so the spinner
                        // animates smoothly and the display cursor advances
                        // even when no new data arrives between API chunks.
                        if app.inflight.is_some() {
                            needs_draw = true;
                            // 2s timeout: revert "esc again" back to
                            // "esc to interrupt" if user doesn't follow through.
                            if let CancelState::Confirming(since) = app.cancel_state {
                                if since.elapsed() >= Duration::from_secs(2) {
                                    app.cancel_state = CancelState::Idle;
                                }
                            }
                        }
                        // display_cursor is kept up-to-date in append_to_last,
                        // so content is immediately visible during streaming.
                        // The tick handler still triggers re-renders for the
                        // spinner animation above.
                        if last_status_refresh.elapsed() >= Duration::from_millis(500) {
                            app.status.update_hit(&app.hit_rate);
                            last_status_refresh = std::time::Instant::now();
                            needs_draw = true;
                        }
                    }
                }

        // Detect scroll changes to force a full repaint (every cell
        // marked AlwaysUpdate) on the next draw. This works around
        // ratatui's BufferDiff skipping CJK trailing cells, which
        // leaves 1-cell bg-color streaks when wide characters scroll
        // over previously-colored regions.
        if let Some(prev) = prev_scroll {
            if prev != app.session.scroll {
                app.force_full_repaint = true;
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

/// Spawn any request that was prepared by `submit_input` /
/// `commands::send_message` but held back so the user message could
/// render first. Called from the main event loop right after
/// `terminal.draw(...)` returns.
///
/// While the request sits in `app.pending_request`, `inflight` is
/// already set so the spinner / pending tool block is visible; only
/// the actual network / tool execution is deferred.
fn flush_pending_request(app: &mut App) {
    let Some(pending) = app.pending_request.take() else {
        return;
    };
    match pending {
        crate::function::PendingRequest::Chat(p) => {
            tokio::spawn(crate::commands::run_chat_stream(
                p.client,
                p.base,
                p.key,
                p.req,
                p.provider,
                p.agent,
                p.cwd,
                p.cancel_rx,
                p.tx,
                p.seq,
            ));
        }
        crate::function::PendingRequest::Tool(p) => {
            tokio::spawn(run_tool_execution(
                p.name,
                p.title,
                p.args,
                p.include_context,
                p.cwd,
                p.cancel_rx,
                p.tx,
                p.seq,
            ));
        }
    }
}

/// Drain a queued post-compaction follow-up prompt, if any. The
/// main loop calls this right after a frame is rendered so the
/// synthetic message lands on screen before the actual chat stream
/// is fired. No-op when the session is not idle (a fresh
/// user-driven request is already pending) or when nothing is
/// queued.
fn drain_post_compaction_prompt(app: &mut App) {
    let Some(text) = app.pending_post_compaction_prompt.take() else {
        return;
    };
    if app.inflight.is_some() || app.compacting || app.pending_request.is_some() {
        // Re-queue and try again next frame.
        app.pending_post_compaction_prompt = Some(text);
        return;
    }
    crate::commands::send_chat(app, text, Vec::new());
}

/// Heuristic: treat text as a paste if it spans multiple lines or is long enough.
/// Path-like text needs more characters to avoid fragmenting long file paths
/// that arrive as individual key events from legacy Windows terminals.
/// Try to aggregate rapid-fire key events into a single batch so
/// terminal-level buffering doesn't starve the main loop.
/// IME commits characters in burst-like fashion but does NOT modify the
/// clipboard, so a clipboard mismatch reliably rules out a paste.
async fn try_consume_burst(
    first_key: crossterm::event::KeyEvent,
    events: &mut EventStream,
) -> Vec<crossterm::event::KeyEvent> {
    use crossterm::event::{Event, KeyCode, KeyEventKind};

    let mut keys = vec![first_key];
    let mut text = String::new();

    // Only start burst detection on a plain printable char (no modifiers).
    match first_key.code {
        KeyCode::Char(c) if first_key.modifiers.is_empty() => text.push(c),
        _ => return keys,
    }

    // Adaptive timeout: start short, grow as we see more chars.
    // This lets paste bursts (which are fast) collect before the timeout
    // fires, while single keystrokes return almost instantly.
    let mut timeout = Duration::from_millis(10);
    const MAX_COLLECTED: usize = 4096;

    loop {
        if text.len() >= MAX_COLLECTED {
            break;
        }
        match tokio::time::timeout(timeout, events.next()).await {
            Ok(Some(Ok(Event::Key(k)))) if k.kind == KeyEventKind::Press => {
                match k.code {
                    KeyCode::Char(c) if k.modifiers.is_empty() => {
                        text.push(c);
                        keys.push(k);
                        // Adaptive timeout: longer bursts (image paths, URLs)
                        // need more time between components to avoid splitting.
                        timeout = if keys.len() >= 30 {
                            Duration::from_millis(1000)
                        } else if keys.len() >= 10 {
                            Duration::from_millis(500)
                        } else if keys.len() >= 3 {
                            Duration::from_millis(200)
                        } else {
                            Duration::from_millis(10)
                        };
                    }
                    KeyCode::Enter if k.modifiers.is_empty() => {
                        text.push('\n');
                        keys.push(k);
                    }
                    KeyCode::Tab if k.modifiers.is_empty() => {
                        text.push('\t');
                        keys.push(k);
                    }
                    // Any other key breaks the burst.
                    _ => break,
                }
            }
            // Timeout or any other event ends the burst.
            _ => break,
        }
    }

    keys
}

fn note_model_output(app: &mut App, chunk: &str) {
    if chunk.is_empty() {
        return;
    }
    if app.response_started_at.is_none() {
        app.response_started_at = Some(Instant::now());
        app.response_output_chars = 0;
    }
    app.response_output_chars += chunk.chars().count();
}

fn finish_model_output_rate(app: &mut App) {
    let Some(started_at) = app.response_started_at.take() else {
        app.response_output_chars = 0;
        app.response_output_tokens = None;
        return;
    };
    let elapsed_self = started_at.elapsed();
    let total_elapsed = app.response_accumulated + elapsed_self;
    app.response_accumulated = std::time::Duration::ZERO;
    let elapsed = total_elapsed.as_secs_f64();
    if elapsed <= 0.0 {
        app.response_output_chars = 0;
        app.response_output_tokens = None;
        return;
    }

    let tokens = app
        .response_output_tokens
        .take()
        .unwrap_or_else(|| estimate_output_tokens(app.response_output_chars));
    app.response_output_chars = 0;
    if tokens == 0 {
        return;
    }

    app.token_rate.record(tokens as f64 / elapsed);
    app.status.update_token_rate(&app.token_rate);
    app.status.total_output_tokens += tokens;
    app.status.total_elapsed_secs += elapsed;
}

fn estimate_output_tokens(chars: usize) -> u64 {
    // Coarse fallback for providers that do not stream final usage.
    // CJK-heavy output is closer to one token per character, Latin text
    // closer to one per four characters; two chars is a conservative middle.
    ((chars as f64) / 2.0).ceil() as u64
}

fn open_tool_function_panel(app: &mut App, name: &str, content: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return;
    };
    let kind = value.get("kind").and_then(|v| v.as_str()).unwrap_or(name);
    if kind == "plan" {
        let title = value
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Plan")
            .to_string();
        let content = value
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !content.trim().is_empty() {
            app.open_plan(title, content);
        }
        return;
    }
    if kind == "ask" {
        let question = value
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if question.is_empty() {
            return;
        }
        let options: Vec<String> = value
            .get("options")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        app.open_ask(question, options);
    }
}

fn handle_todowrite_result(app: &mut App, content: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return;
    };
    let kind = value.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    if kind != "todowrite" {
        return;
    }
    let action = value.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let Some(todos) = value.get("todos").and_then(|v| v.as_array()) else {
        return;
    };
    let new_items: Vec<crate::session::TodoItem> = todos
        .iter()
        .filter_map(|v| {
            let content = v.get("content").and_then(|c| c.as_str())?.to_string();
            let status = v
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("pending")
                .to_string();
            Some(crate::session::TodoItem { content, status })
        })
        .collect();
    app.session.todo_items = new_items;
    app.session.invalidate_layout_cache();
    if app.session.todo_items.is_empty() || action == "clear" {
        app.close_todo_tab();
    } else {
        app.open_todo_tab();
    }
}

/// Refresh the MCP status summary displayed in the status bar.
/// Reads the live snapshot from the MCP service and aggregates
/// per-server statuses into a compact string like `"2✓ 1✗"`.
fn refresh_mcp_summary(app: &mut App) {
    let snap = crate::mcp::try_snapshot_or_empty();
    let mut connected = 0u32;
    let mut failed = 0u32;
    let mut other = 0u32;

    for status in snap.status.values() {
        match status {
            crate::mcp::McpStatus::Connected => connected += 1,
            crate::mcp::McpStatus::Failed { .. } => failed += 1,
            crate::mcp::McpStatus::Disabled => {}
            _ => other += 1,
        }
    }

    let active = connected + failed + other;
    if active == 0 {
        app.status.set_mcp_summary(None);
        return;
    }

    let mut parts = Vec::new();
    if connected > 0 {
        parts.push(format!("{connected}✓"));
    }
    if failed > 0 {
        parts.push(format!("{failed}✗"));
    }
    if other > 0 {
        parts.push(format!("{other}⚠"));
    }
    app.status.set_mcp_summary(Some(parts.join(" ")));
}

fn handle_msg(msg: AppMsg, app: &mut App) {
    match msg {
        AppMsg::ChatDelta(s) => {
            note_model_output(app, &s);
            app.session.append_to_last(&s);
        }
        AppMsg::ChatThinkingDelta(s) => {
            note_model_output(app, &s);
            app.session.append_thinking_to_last(&s);
        }
        AppMsg::ChatContentBlockStart(_) => {
            // A new content block has begun in the upstream stream;
            // close off the in-flight thinking segment so the
            // renderer treats it as a complete block and the next
            // thinking delta lands in a fresh one.
            app.session.begin_thinking_segment();
        }
        AppMsg::ChatDebug(s) => {
            app.notify(crate::function::notifications::ToastLevel::Info, s);
        }
        AppMsg::ChatWarn(s) => {
            app.notify(crate::function::notifications::ToastLevel::Warn, s);
        }
        AppMsg::ChatWarnClear(s) => {
            app.notifications.remove_last_containing(&s);
        }
        AppMsg::ChatToolResult {
            name,
            title,
            content,
            metadata,
            call_id,
            failed,
        } => {
            if name == "todowrite" {
                handle_todowrite_result(app, &content);
            }
            open_tool_function_panel(app, &name, &content);
            app.session
                .update_last_tool_content(name, title, content, call_id, metadata, failed);
        }
        AppMsg::AssistantToolCalls(tool_calls) => {
            if let Some(id) = app.session.streaming_id {
                if let Some(m) = app.session.messages.get_mut(id) {
                    m.tool_calls.extend(tool_calls);
                }
            }
        }
        AppMsg::LocalToolResult {
            name,
            title,
            content,
            metadata,
            context,
            failed,
        } => {
            if name == "todowrite" {
                handle_todowrite_result(app, &content);
            }
            open_tool_function_panel(app, &name, &content);
            app.session
                .push_tool_result_message(name, title, content, metadata, failed);
            if let Some(context) = context {
                app.session.push(crate::session::Message::new(
                    crate::session::Role::User,
                    context,
                ));
            }
            app.save_current_session();
        }
        AppMsg::ChatUsage { seq, usage: u } => {
            if seq != app.current_request_seq {
                return;
            }
            let rate = if u.input_tokens == 0 {
                0.0
            } else {
                u.cache_read_tokens as f64 / u.input_tokens as f64
            };
            app.hit_rate.record(rate);
            app.status.update_hit(&app.hit_rate);
            if let Some(context_window_tokens) = u.context_window_tokens {
                app.status.set_context_window_tokens(context_window_tokens);
            }
            let ctx_tokens = u.input_tokens;
            if ctx_tokens > 0 {
                app.status.update_token_usage(ctx_tokens);
            }
            // Accumulate for overall hit rate display.
            app.status.total_input_tokens += u.input_tokens;
            app.status.total_cache_read += u.cache_read_tokens;
            if u.output_tokens > 0 {
                *app.response_output_tokens.get_or_insert(0) += u.output_tokens;
            }
        }
        AppMsg::ChatDone { seq } => {
            if seq != app.current_request_seq {
                // Stale event from a request we already cancelled
                // (e.g. user hit Esc mid-stream, then started a
                // fresh request before the OLD chat task finished
                // draining). The OLD task no longer owns the inflight
                // — drop the event so it doesn't clobber the new one.
                return;
            }
            finish_model_output_rate(app);
            app.flush_ask_snapshot();
            app.session.finish_streaming();
            app.save_current_session();
            app.inflight = None;
            app.cancel_state = CancelState::Idle;
            use crate::function::notifications::ToastLevel;
            app.notify(ToastLevel::Ok, "response complete");
            maybe_trigger_auto_compact(app);
        }
        AppMsg::ChatError { seq, error } => {
            if seq != app.current_request_seq {
                // See `ChatDone` — stale event from a previous
                // request, ignore it.
                return;
            }
            finish_model_output_rate(app);
            app.flush_ask_snapshot();
            app.session.finish_streaming();
            app.save_current_session();
            app.inflight = None;
            app.cancel_state = CancelState::Idle;
            use crate::function::notifications::ToastLevel;
            app.notify(ToastLevel::Fail, error.clone());
            // If this is a context-overflow error, try to compact
            // and recover. Remove the error message and the
            // streaming assistant placeholder, then trigger
            // compaction. The auto-continue mechanism will resume
            // the conversation after compaction succeeds.
            if crate::compaction::is_context_overflow_error(&error)
                && app.config.auto_compact
                && !app.compacting
            {
                app.notify(
                    ToastLevel::Info,
                    "context overflow — compacting and retrying...",
                );
                // Pop the error message we just pushed
                app.session.messages.pop();
                // Remove the streaming assistant placeholder
                if let Some(last) = app.session.messages.last() {
                    if matches!(last.role, crate::session::Role::Assistant) && last.streaming {
                        app.session.messages.pop();
                    }
                }
                app.session.streaming_id = None;
                maybe_trigger_auto_compact(app);
                return;
            }
            app.session.push(crate::session::Message::new(
                crate::session::Role::System,
                format!("[request failed: {error}]"),
            ));
        }
        AppMsg::ModelsFetched {
            provider,
            base_url,
            api_key,
            models,
        } => {
            // Update any open picker
            if let Some(crate::function::SidebarTab::ModelPicker(s)) = app
                .function
                .tabs
                .iter_mut()
                .find(|t| matches!(t, crate::function::SidebarTab::ModelPicker(_)))
            {
                s.fetching = false;
                s.fetch_error = None;
                s.no_endpoint = false;
                s.models = models.clone();
                s.rebuild_filter();
            }
            app.model_cache
                .put(provider, base_url, api_key, models.clone());
            if app.config.active_kind() == Some(provider) {
                let active_model = app.config.active_model().to_string();
                let selected_model = models.iter().find(|m| {
                    m.id == active_model || m.request_id.as_deref() == Some(active_model.as_str())
                });
                if let Some(model) = selected_model {
                    if let Some(active_id) = app.config.active.clone() {
                        if let Some(entry) = app.config.entry_mut(&active_id) {
                            if entry.model == model.id {
                                entry.model =
                                    model.request_id.clone().unwrap_or_else(|| model.id.clone());
                            }
                            entry.model_display = model.display.clone();
                        }
                    }
                    app.status.set_model(&app.config.active_model_display());
                    app.refresh_status_model_context();
                    if let Some(tokens) = model.context_window_tokens {
                        app.status.set_context_window_tokens(tokens);
                    }
                    app.save_config();
                }
            }
            app.model_cache.save(&app.model_cache_path);
            let ctx_count = models
                .iter()
                .filter(|m| m.context_window_tokens.is_some())
                .count();
            let missing_count = models
                .iter()
                .filter(|m| m.context_window_tokens.is_none())
                .count();
            use crate::function::notifications::ToastLevel;
            let mut msg = format!("fetched {} models for {}", models.len(), provider.as_str());
            if ctx_count > 0 {
                msg.push_str(&format!(" ({} with context)", ctx_count));
            } else if missing_count > 0 {
                msg.push_str(" (no context data)");
            }
            app.notify(ToastLevel::Ok, msg);
        }
        AppMsg::ModelsFetchFailed {
            provider,
            error,
            no_endpoint,
        } => {
            let models_path = match provider {
                crate::config::ProviderKind::Openai => "/models",
                crate::config::ProviderKind::Anthropic => "/v1/models",
                crate::config::ProviderKind::Cursor => "",
                crate::config::ProviderKind::Volcengine => "/models",
            };
            if let Some(crate::function::SidebarTab::ModelPicker(s)) = app
                .function
                .tabs
                .iter_mut()
                .find(|t| matches!(t, crate::function::SidebarTab::ModelPicker(_)))
            {
                s.fetching = false;
                s.fetch_error = Some(if no_endpoint {
                    format!("[no {models_path} endpoint at this base_url]")
                } else {
                    error.clone()
                });
                s.no_endpoint = no_endpoint;
            }
            use crate::function::notifications::ToastLevel;
            app.notify(
                ToastLevel::Fail,
                if no_endpoint {
                    format!("base_url has no {models_path}; use Manual id")
                } else {
                    format!("fetch models for {}: {}", provider.as_str(), error)
                },
            );
        }
        AppMsg::CursorAuthSucceeded {
            access_token,
            refresh_token,
        } => {
            use crate::config::{make_id, ProviderKind, ProviderMode};
            use crate::function::notifications::ToastLevel;
            let id = make_id(ProviderKind::Cursor, ProviderMode::Oauth);
            if let Some(entry) = app.config.entry_mut(&id) {
                entry.api_key = access_token;
                entry.api_key_env = refresh_token;
                if entry.model.trim().eq_ignore_ascii_case("auto") {
                    entry.model.clear();
                }
            }
            app.config.active = Some(id.clone());
            app.save_config();
            app.status.set_provider_name(&app.config.active_name());
            app.status.set_model(&app.config.active_model_display());
            app.refresh_status_model_context();
            app.notify(ToastLevel::Ok, "Cursor OAuth authorized");
            crate::commands::open_model_picker_for_entry(app, &id);
        }
        AppMsg::CursorAuthFailed(e) => {
            use crate::function::notifications::ToastLevel;
            app.notify(ToastLevel::Fail, format!("Cursor OAuth: {e}"));
        }
        AppMsg::ChatTimerPause => {
            if let Some(started_at) = &app.response_started_at {
                app.response_accumulated += started_at.elapsed();
                app.response_started_at = None;
            }
        }
        AppMsg::ChatTimerResume => {
            app.response_started_at = Some(std::time::Instant::now());
        }
        AppMsg::ToolStarted {
            call_id,
            name,
            title,
        } => {
            app.session.start_tool_in_last(call_id, name, title);
        }
        AppMsg::ToolDelta { call_id, content } => {
            app.session.append_tool_delta_to_last(&call_id, &content);
        }
        AppMsg::ToolInputDelta {
            index,
            call_id,
            name,
            args,
        } => {
            app.session
                .update_tool_input_delta(index, &call_id, &name, &args);
        }
        AppMsg::McpToolsChanged { server } => {
            // The aggregated tool set changed; nudge the next
            // request to re-read `openai_tool_specs` /
            // `anthropic_tool_specs`. The picker / status bar
            // already consume the live snapshot.
            tracing::debug!(server = %server, "mcp tools changed");
            app.invalidate_tool_specs();
            app.notify(
                crate::function::notifications::ToastLevel::Info,
                format!("mcp `{server}` tools updated"),
            );
        }
        AppMsg::McpStatusChanged { name, status } => {
            tracing::debug!(server = %name, status = %status.label(), "mcp status changed");
            refresh_mcp_summary(app);
        }
        AppMsg::McpAuthRequired { server, url, error } => {
            if !url.is_empty() {
                app.notify(
                    crate::function::notifications::ToastLevel::Warn,
                    format!("mcp `{server}` needs auth: {url}"),
                );
            } else {
                app.notify(
                    crate::function::notifications::ToastLevel::Warn,
                    format!("mcp `{server}` needs auth: {error}"),
                );
            }
        }
        AppMsg::McpBrowserOpenFailed { server: _, url: _ } => {
            // The toast already surfaced the URL; nothing else to do.
        }
        AppMsg::McpClientClosed { server } => {
            app.notify(
                crate::function::notifications::ToastLevel::Fail,
                format!("mcp `{server}` connection closed"),
            );
        }
        AppMsg::McpStartAuth { server } => {
            // Drive the OAuth flow asynchronously. The handler is
            // synchronous, so we spawn a background task that does
            // the async work and sends result AppMsgs back.
            let server_clone = server.clone();
            let tx = app.msg_tx.clone();
            app.notify(
                crate::function::notifications::ToastLevel::Info,
                format!("mcp `{server}`: starting OAuth..."),
            );
            tokio::spawn(async move {
                let Some(tx) = tx else { return };
                if let Err(e) = run_mcp_oauth(&server_clone, &tx).await {
                    let _ = tx.send(crate::event::AppMsg::McpAuthRequired {
                        server: server_clone,
                        url: String::new(),
                        error: format!("OAuth failed: {e}"),
                    });
                }
            });
        }
        AppMsg::CompactionSummaryReady {
            start,
            end,
            keep_start,
            summary,
        } => {
            use crate::function::notifications::ToastLevel;
            app.inflight = None;
            app.cancel_state = CancelState::Idle;
            app.compacting = false;
            if let Some(idx) = app
                .session
                .apply_compaction(start, end, keep_start, summary)
            {
                app.notify(ToastLevel::Ok, "session compacted");
                app.save_current_session();
                if app.pending_post_compaction_prompt.is_none() {
                    app.pending_post_compaction_prompt = Some(continue_prompt_text().to_string());
                }
                let _ = idx;
            } else {
                app.notify(ToastLevel::Warn, "compaction range was empty");
            }
            // Reset the "triggered" status indicator so the bar
            // returns to showing the new headroom percentage.
            if let Some(total) = app.status.token_total {
                app.status.update_token_usage(total);
            } else {
                app.status.compact_triggered = false;
            }
        }
        AppMsg::CompactionFailed { error } => {
            use crate::function::notifications::ToastLevel;
            app.inflight = None;
            app.cancel_state = CancelState::Idle;
            app.compacting = false;
            app.notify(ToastLevel::Fail, format!("compact failed: {error}"));
            if let Some(total) = app.status.token_total {
                app.status.update_token_usage(total);
            } else {
                app.status.compact_triggered = false;
            }
        }
    }
}

/// Text used as the synthetic follow-up message after a successful
/// compaction. Matches opencode's
/// `experimental.compaction.autocontinue` message, with a small
/// prefix to clarify to the user that it was auto-injected.
fn continue_prompt_text() -> &'static str {
    "Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed."
}

/// Decide whether the current usage warrants an auto-compaction,
/// and if so, schedule one. No-op when:
/// - the toggle is off
/// - another compaction is already running
/// - the token usage / context window is not known
/// - the active provider is missing or unconfigured
fn maybe_trigger_auto_compact(app: &mut App) {
    use crate::function::notifications::ToastLevel;
    if !app.config.auto_compact {
        return;
    }
    if app.compacting || app.inflight.is_some() {
        return;
    }
    let Some(used) = app.status.token_total else {
        return;
    };
    if !app.status.context_window_known {
        return;
    }
    let inp = crate::compaction::CompactionInputs {
        auto_enabled: app.config.auto_compact,
        ctx_window: app.status.context_window_tokens,
        max_output_tokens: app.status.max_output_tokens,
        reserved_override: app.config.compact_reserved,
    };
    if !crate::compaction::should_auto_compact(used, inp) {
        return;
    }
    if crate::compaction::plan_cutoff(&app.session.messages, crate::compaction::DEFAULT_TAIL_TURNS)
        .is_none()
    {
        return;
    }
    // Verify that the compaction range is not empty after trimming
    // to the API input limit. If it is, skip this cycle — the
    // prompt is too large to compact in one shot.
    let Some((start, end)) = crate::compaction::plan_cutoff(
        &app.session.messages,
        crate::compaction::DEFAULT_TAIL_TURNS,
    ) else {
        return;
    };
    let adjusted = crate::compaction::trim_to_size(
        &app.session.messages,
        start,
        end,
        crate::compaction::MAX_COMPACTION_PROMPT_CHARS,
    );
    if adjusted >= end {
        app.notify(
            ToastLevel::Warn,
            "compaction prompt too large — skipping auto-compaction",
        );
        return;
    }
    app.notify(ToastLevel::Info, "auto compacting session...");
    crate::commands::compact_now(app, "");
    // compact_now will only fail silently if the inflight / provider
    // gate tripped; if it bailed, the user already saw a toast.
}

async fn handle_key(k: crossterm::event::KeyEvent, app: &mut App) {
    // Post-paste quota: suppress characters the terminal re-sends as raw keys.
    if app.paste_key_quota > 0 {
        use crossterm::event::KeyCode;
        // 5000ms window covers slowly-arriving legacy paste chars on conhost.
        let expired = app
            .last_paste_at
            .map(|t| t.elapsed() >= Duration::from_millis(5000))
            .unwrap_or(true);
        if expired {
            app.paste_key_quota = 0;
        } else if matches!(k.code, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab) {
            app.paste_key_quota -= 1;
            return;
        }
    }

    use crossterm::event::{KeyCode, KeyModifiers};

    // Non-text keys break any ongoing paste-burst tracking.
    if !matches!(k.code, KeyCode::Char(_) | KeyCode::Enter) {
        app.burst_buf.clear();
        app.burst_snapshot = None;
    }

    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

    // Ctrl+Q: quit (and cancel inflight)
    if ctrl && matches!(k.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
        if let Some(h) = app.inflight.take() {
            let _ = h.cancel.send(true);
        }
        app.should_quit = true;
        return;
    }

    // Ctrl+C: priority is full-TUI selection > input prompt selection >
    // clear-input. The TUI selection is the one the user usually wants
    // when they grabbed text with the mouse.
    if ctrl && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('C')) {
        if let Some(text) = app.selected_text.take() {
            if !text.is_empty() {
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    let _ = cb.set_text(text.clone());
                    use crate::function::notifications::ToastLevel;
                    app.notify(
                        ToastLevel::Ok,
                        format!("copied {} chars to clipboard", text.chars().count()),
                    );
                }
            }
            app.tui_selection = None;
            app.input.clear_selection();
            return;
        }
        if app.input.has_selection() {
            if let Some(text) = app.input.selected_text() {
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    let _ = cb.set_text(text);
                    use crate::function::notifications::ToastLevel;
                    app.notify(ToastLevel::Ok, "copied to clipboard");
                }
            }
            app.input.clear_selection();
        } else if !app.input.buffer.is_empty() {
            app.input.buffer.clear();
            app.input.cursor = 0;
            app.input.clear_selection();
            app.paste_blocks.clear();
        }
        return;
    }

    // Ctrl+I: focus input. Closes any active sidebar tab (returns to chat).
    if ctrl && matches!(k.code, KeyCode::Char('i') | KeyCode::Char('I')) {
        app.function
            .tabs
            .retain(|t| matches!(t, crate::function::SidebarTab::Notifications));
        app.function.active = 0;
        return;
    }

    // Ctrl+L clears session
    if ctrl && matches!(k.code, KeyCode::Char('l') | KeyCode::Char('L')) {
        app.start_new_session();
        use crate::function::notifications::ToastLevel;
        app.notify(ToastLevel::Info, "session cleared");
        return;
    }

    //   - panel hidden  -> show it and focus Notifications
    //   - panel showing and Notifications is active -> hide
    //   - panel showing but another tab is active -> switch to Notifications
    if ctrl && matches!(k.code, KeyCode::Char('n') | KeyCode::Char('N')) {
        handle_ctrl_n(app);
        return;
    }

    // Alt+L: cycle focus between Input -> FunctionPanel -> AgentsCheckbox -> Input.
    if k.modifiers.contains(KeyModifiers::ALT)
        && matches!(k.code, KeyCode::Char('l') | KeyCode::Char('L'))
    {
        match app.focus_target {
            crate::function::FocusTarget::Input => {
                if app.function_visible {
                    app.focus_target = crate::function::FocusTarget::FunctionPanel;
                } else if app.agents_visible {
                    app.focus_target = crate::function::FocusTarget::AgentsCheckbox;
                }
            }
            crate::function::FocusTarget::FunctionPanel => {
                if app.agents_visible {
                    app.focus_target = crate::function::FocusTarget::AgentsCheckbox;
                } else {
                    app.focus_target = crate::function::FocusTarget::Input;
                }
            }
            crate::function::FocusTarget::AgentsCheckbox => {
                app.focus_target = crate::function::FocusTarget::Input;
            }
        }
        return;
    }

    // Handle Up/Down for agents checkbox navigation
    if app.focus_target == crate::function::FocusTarget::AgentsCheckbox {
        if matches!(k.code, KeyCode::Up) {
            if app.agents_cursor > 0 {
                app.agents_cursor -= 1;
            }
            return;
        }
        if matches!(k.code, KeyCode::Down) {
            let count = app.config.agents.entries.len();
            if count > 0 && app.agents_cursor + 1 < count {
                app.agents_cursor += 1;
            }
            return;
        }
    }

    if app.focus_target == crate::function::FocusTarget::FunctionPanel
        && dispatch_to_active_tab(k, app).await
    {
        return;
    }

    // Handle Enter/Space for agents checkbox toggle
    if app.focus_target == crate::function::FocusTarget::AgentsCheckbox
        && (matches!(k.code, KeyCode::Enter) || matches!(k.code, KeyCode::Char(' ')))
    {
        let keys: Vec<String> = app.config.agents.entries.keys().cloned().collect();
        if app.agents_cursor < keys.len() {
            if let Some(entry) = app.config.agents.entries.get_mut(&keys[app.agents_cursor]) {
                *entry = !*entry;
                app.save_config();
            }
        }
        return;
    }

    app.input_scroll_decoupled = false;

    match k.code {
        KeyCode::Esc => {
            // If a request has been prepared but not yet dispatched
            // (e.g. the user just hit Enter and the deferred spawn in
            // `flush_pending_request` hasn't run yet), silently drop
            // it. The user message + empty assistant already in
            // `session.messages` are kept, matching the existing
            // cancel semantics ("the message was sent, but I'm not
            // waiting for the answer").
            if app.pending_request.is_some() {
                app.pending_request = None;
                app.inflight = None;
                app.session.streaming_id = None;
                return;
            }
            // Progressive cancellation: first Esc → "esc again" hint,
            // second Esc → actually cancel. Falls back to Idle after
            // 2s of no input (checked in the tick handler).
            // Only applies when focus is on Input — when focus is on
            // the FunctionPanel or AgentsCheckbox, Esc closes the
            // active tab / returns focus to Input instead.
            if app.focus_target == crate::function::FocusTarget::Input && app.inflight.is_some() {
                match app.cancel_state {
                    CancelState::Idle => {
                        app.cancel_state = CancelState::Confirming(Instant::now());
                        app.save_current_session();
                        return;
                    }
                    CancelState::Confirming(_) => {
                        app.cancel_state = CancelState::Idle;
                        if let Some(inflight) = app.inflight.take() {
                            let _ = inflight.cancel.send(true);
                        }
                        app.compacting = false;
                        app.save_current_session();
                        app.session.streaming_id = None;
                        return;
                    }
                }
            }
            // If a selection is active, just clear it.
            if app.input.has_selection() {
                app.input.clear_selection();
                return;
            }
            // close current sidebar tab, or clear input
            if !app.function.close_active() {
                if !app.input.buffer.is_empty() {
                    app.input.buffer.clear();
                    app.input.cursor = 0;
                    app.paste_blocks.clear();
                }
            } else {
                // A function tab was closed. If it was the last non-
                // Notification tab, hide the panel so we return to the
                // default state.
                app.maybe_hide_panel();
            }
        }
        KeyCode::Tab => {
            if complete_focused_candidate(app) {
                return;
            }
            // Tab jumps to the Plan tab (or creates one).
            app.jump_to_plan();
        }
        KeyCode::BackTab => {
            // Shift+Tab cycles forward through tabs (wrap last→first).
            cycle_sidebar_forward(app);
        }
        KeyCode::Enter => {
            // If the completion tab is showing for a partial command, complete
            // the buffer with the focused candidate, then submit.
            if completion_is_focused(app) {
                complete_focused_candidate(app);
                submit_input(app);
                return;
            }

            // If the active sidebar tab is a Plan and the input buffer is
            // empty, Enter approves the plan directly from the input box
            // (no need to Alt+L into the panel first). A non-empty buffer
            // falls through to the normal submit path so the user can still
            // type additional args/instructions; handle_plan_key already
            // appends them when approving from the panel.
            if app.input.buffer.trim().is_empty()
                && k.modifiers.is_empty()
                && matches!(
                    app.function.tabs.get(app.function.active),
                    Some(crate::function::SidebarTab::Plan(_))
                )
                && dispatch_to_active_tab(k, app).await
            {
                return;
            }

            // Treat any modifier (Shift, Ctrl, Alt, Meta) as the "modified
            // variant". This matters because some Windows consoles drop
            // the SHIFT bit for Enter specifically — without this fallback
            // the user could only ever get the plain-Enter behavior. With
            // it, Ctrl+Enter (or Alt+Enter) is a reliable send trigger in
            // the EnterNewline mode, even when Shift+Enter is not.
            let modified = k.modifiers.intersects(
                KeyModifiers::SHIFT
                    | KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::META,
            );
            let newline = matches!(
                enter_action(app.config.enter_behavior, modified),
                EnterAction::Newline
            );
            if newline {
                if app.input.has_selection() {
                    app.input.delete_selection();
                    app.burst_buf.clear();
                    app.burst_snapshot = None;
                }
                // Track Enter in burst for legacy paste detection
                let now = Instant::now();
                let expired = app
                    .burst_snapshot
                    .map(|(t, _, _)| now.duration_since(t) > Duration::from_millis(100))
                    .unwrap_or(true);
                if expired {
                    app.burst_buf.clear();
                    app.burst_buf.push('\n');
                    app.burst_snapshot = Some((now, app.input.cursor, app.input.buffer.len()));
                } else {
                    app.burst_buf.push('\n');
                    if let Some((_, sc, sl)) = app.burst_snapshot {
                        app.burst_snapshot = Some((now, sc, sl));
                    }
                }
                app.input.insert_newline();
                app.sync_completion();
            } else {
                // During a burst (legacy paste without bracketed-paste
                // support), multi-line text may contain Enter keys. Force
                // newline instead of submitting so the full pasted text
                // arrives intact.
                let now = Instant::now();
                let in_burst = app
                    .burst_snapshot
                    .map(|(t, _, _)| now.duration_since(t) <= Duration::from_millis(100))
                    .unwrap_or(false);
                if in_burst {
                    if app.input.has_selection() {
                        app.input.delete_selection();
                        app.burst_buf.clear();
                        app.burst_snapshot = None;
                    }
                    app.burst_buf.push('\n');
                    if let Some((_, sc, sl)) = app.burst_snapshot {
                        app.burst_snapshot = Some((now, sc, sl));
                    }
                    app.input.insert_newline();
                    app.sync_completion();
                } else {
                    submit_input(app);
                }
            }
        }
        KeyCode::Backspace => {
            if !app.input.delete_selection()
                && !try_remove_paste_marker(app)
                && !try_remove_image_marker(app)
            {
                app.input.backspace();
            }
            app.sync_completion();
        }
        KeyCode::Delete => {
            if !app.input.delete_selection() {
                app.input.delete_forward();
            }
            app.sync_completion();
        }
        KeyCode::Char(c) => {
            if k.modifiers.contains(KeyModifiers::CONTROL) {
                match c {
                    'w' | 'W' => {
                        app.push_input_undo();
                        app.input.delete_word_back();
                        app.sync_completion();
                    }
                    'z' | 'Z' => {
                        app.undo_input();
                        app.sync_completion();
                    }
                    'y' | 'Y' => {
                        app.redo_input();
                        app.sync_completion();
                    }
                    'a' | 'A' => app.input.move_home(),
                    'e' | 'E' => app.input.move_end(),
                    'u' | 'U' => {
                        if !app.input.buffer.is_empty() {
                            app.push_input_undo();
                        }
                        app.input.buffer.clear();
                        app.input.cursor = 0;
                        app.input.clear_selection();
                        app.paste_blocks.clear();
                        app.image_blocks.clear();
                        app.sync_completion();
                    }
                    'k' | 'K' => {
                        if app.input.cursor < app.input.buffer.len() {
                            app.push_input_undo();
                            app.input.buffer.truncate(app.input.cursor);
                        }
                        app.sync_completion();
                    }
                    _ => {}
                }
            } else {
                if app.input.has_selection() {
                    app.input.delete_selection();
                    app.burst_buf.clear();
                    app.burst_snapshot = None;
                }
                // Track burst for legacy-paste detection (conhost etc.).
                let now = Instant::now();
                let expired = app
                    .burst_snapshot
                    .map(|(t, _, _)| now.duration_since(t) > Duration::from_millis(100))
                    .unwrap_or(true);
                if expired {
                    app.burst_buf.clear();
                    app.burst_buf.push(c);
                    app.burst_snapshot = Some((now, app.input.cursor, app.input.buffer.len()));
                } else {
                    app.burst_buf.push(c);
                    if let Some((_, sc, sl)) = app.burst_snapshot {
                        app.burst_snapshot = Some((now, sc, sl));
                    }
                }
                app.input.insert_char(c);
                app.sync_completion();
            }
        }
        KeyCode::Left => {
            if k.modifiers.contains(KeyModifiers::SHIFT) {
                app.input.extend_selection_left();
            } else {
                app.input.move_left();
            }
        }
        KeyCode::Right => {
            if k.modifiers.contains(KeyModifiers::SHIFT) {
                app.input.extend_selection_right();
            } else {
                app.input.move_right();
            }
        }
        KeyCode::Home => {
            scroll_session_to_top(app);
        }
        KeyCode::End => {
            app.set_scroll_anchored(0);
        }
        KeyCode::PageUp => {
            scroll_session_page(app, true);
        }
        KeyCode::PageDown => {
            scroll_session_page(app, false);
        }
        KeyCode::Up => {
            if completion_is_focused(app) {
                if let Some(idx) = completion_idx(app) {
                    if let crate::function::SidebarTab::Completion(s) = &mut app.function.tabs[idx]
                    {
                        s.move_up();
                    }
                }
            } else if !app.input.move_up_line() {
                app.input.history_prev();
            }
        }
        KeyCode::Down => {
            if completion_is_focused(app) {
                if let Some(idx) = completion_idx(app) {
                    if let crate::function::SidebarTab::Completion(s) = &mut app.function.tabs[idx]
                    {
                        s.move_down();
                    }
                }
            } else if !app.input.move_down_line() {
                app.input.history_next();
            }
        }
        _ => {}
    }
}

/// Returns the index of the Completion sidebar tab, if any.
fn completion_idx(app: &App) -> Option<usize> {
    app.function
        .tabs
        .iter()
        .position(|t| matches!(t, crate::function::SidebarTab::Completion(_)))
}

fn complete_focused_candidate(app: &mut App) -> bool {
    let Some(idx) = completion_idx(app) else {
        return false;
    };
    let Some(cand) = (match &app.function.tabs[idx] {
        crate::function::SidebarTab::Completion(s) => s.candidates.get(s.cursor).cloned(),
        _ => None,
    }) else {
        return false;
    };
    app.input.buffer = cand;
    app.input.cursor = app.input.buffer.len();
    app.input.clear_selection();
    app.sync_completion();
    true
}

/// True if the Completion tab is present and has at least one candidate.
/// In this state Up/Down navigate candidates, Tab completes, and Enter
/// executes the focused candidate.
fn completion_is_focused(app: &App) -> bool {
    let Some(idx) = completion_idx(app) else {
        return false;
    };
    matches!(
        &app.function.tabs[idx],
        crate::function::SidebarTab::Completion(s) if !s.candidates.is_empty()
    )
}

/// Compute the byte index in the input buffer corresponding to a screen column,
/// given the prompt prefix width and the buffer text.
fn screen_col_to_byte(buffer: &str, screen_col: u16, prefix_width: u16) -> usize {
    if screen_col < prefix_width {
        return 0;
    }
    let target = (screen_col - prefix_width) as usize;
    let mut acc = 0usize;
    for (i, c) in buffer.char_indices() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if acc + w > target {
            return i;
        }
        acc += w;
        if acc > target {
            return i + c.len_utf8();
        }
    }
    buffer.len()
}

/// Track the start of an in-progress drag selection.
#[derive(Default)]
struct DragState {
    active: bool,
    prefix_width: u16,
    start_byte: usize,
    prompt_row: i32, // -1 = unknown
}

static DRAG: std::sync::Mutex<DragState> = std::sync::Mutex::new(DragState {
    active: false,
    prefix_width: 0,
    start_byte: 0,
    prompt_row: -1,
});

/// Scroll state: (last_event_time, current_step_size).
/// Step accelerates when events arrive quickly and resets when slow.
static SCROLL_STATE: std::sync::Mutex<(Option<std::time::Instant>, u32)> =
    std::sync::Mutex::new((None, 3));

// ---- Instant scroll + 1-frame gating window ------------------------------
//
// Each wheel event lands `current` on `target` in a single step —
// no visible animation, the view jumps by the OS step amount
// immediately. The `animating` flag stays set for one frame
// (~16ms) so the original "while the session is still scrolling,
// ignore new scroll events" rule is preserved as a brief gating
// window: events that arrive within the same render frame as a
// previous event are dropped. After the next 16ms tick, the
// window opens again and new gestures are accepted.

/// Tick period (ms) for clearing the gating window. Drives the
/// `step` call in the main loop.
const SCROLL_ANIM_TICK_MS: u64 = 16;

#[derive(Debug, Clone, Copy)]
pub struct ScrollAnimator {
    /// Currently displayed offset (lines from bottom). For instant
    /// scroll this is always equal to `target` while `animating` is
    /// set; kept as `f32` for the field's symmetry with the public
    /// API and to make `snap(f32)` a no-op type-wise.
    pub current: f32,
    /// Where the user is scrolling toward. After a wheel event
    /// `target == current` (the view is already at the target);
    /// retained as a separate field so the integration in `step`
    /// is conceptually symmetric with a future smooth-scroll mode.
    pub target: f32,
    /// Last-frame velocity, kept for API symmetry. Always 0 in
    /// instant-scroll mode.
    pub velocity: f32,
    /// Timestamp of the last `step` call.
    pub last_tick: Option<Instant>,
    /// `true` for the one-frame window after a wheel event. New
    /// wheel events are dropped while this is set.
    pub animating: bool,
}

impl Default for ScrollAnimator {
    fn default() -> Self {
        Self {
            current: 0.0,
            target: 0.0,
            velocity: 0.0,
            last_tick: None,
            animating: false,
        }
    }
}

impl ScrollAnimator {
    /// Begin a scroll gesture. `delta_lines` is positive for
    /// "scroll up / see older content" and negative for "scroll
    /// down / see newer content". `step` is the adaptive step size
    /// computed by the existing wheel handler (3..=10); kept as a
    /// parameter for the same symmetry reason as `target`/`velocity`.
    ///
    /// The view lands on `target` immediately (no visible motion).
    /// `animating` is set to `true` for a one-frame gating window
    /// so additional wheel events arriving in the same render frame
    /// are dropped. The window is cleared by the next `step` call.
    ///
    /// Callers MUST check `self.animating` first and only call this
    /// when no gesture is in flight.
    pub fn begin_gesture(&mut self, delta_lines: f32, _step: u32, now: Instant) {
        debug_assert!(!self.animating, "begin_gesture called while animating");
        let new_target = (self.target + delta_lines).max(0.0);
        // Floor guard: when the gesture would push past 0 (tail)
        // and we're already at rest there, do nothing. Avoids a
        // no-op jump and keeps `current` clamped at 0.
        if delta_lines < 0.0 && self.target == 0.0 {
            self.snap(0.0);
            return;
        }
        self.target = new_target;
        // Instant: place `current` on `target` right now. The view
        // jumps to the new position on the next draw.
        self.current = new_target;
        self.velocity = 0.0;
        self.last_tick = Some(now);
        self.animating = true;
    }

    /// Pin to a known value, cancelling any in-flight gating window.
    /// Used by programmatic scrolls (submit, jump, clear, etc.)
    /// that should land immediately.
    pub fn snap(&mut self, value: f32) {
        self.current = value;
        self.target = value;
        self.velocity = 0.0;
        self.last_tick = None;
        self.animating = false;
    }

    /// Advance the gating window by one tick. In instant-scroll mode
    /// there is no integration to perform — `current` is already at
    /// `target` (set in `begin_gesture`) — so this simply clears the
    /// `animating` flag so the next wheel event can start a new
    /// gesture.
    ///
    /// Returns `(session.scroll, settled)`. `session.scroll` is the
    /// integer-rounded `current` (callers write it into
    /// `session.scroll`); `settled` is `true` once the gating window
    /// has been cleared.
    pub fn step(&mut self, now: Instant) -> (u32, bool) {
        self.last_tick = Some(now);
        if !self.animating {
            return (self.current.round() as u32, true);
        }
        self.animating = false;
        (self.current.round() as u32, true)
    }
}

enum ToggleTarget {
    Thinking(usize),
    Tool(usize, usize),
}

/// Scroll the session to the very top (oldest content).
/// `session.scroll` is an offset from the bottom, so "top" = max_scroll.
fn scroll_session_to_top(app: &mut App) {
    let max_scroll = session_max_scroll(app);
    app.set_scroll_anchored(max_scroll);
}

/// Scroll the session up or down by one viewport page.
/// `up = true` means towards older content (increase scroll).
fn scroll_session_page(app: &mut App, up: bool) {
    let viewport_h = session_viewport_height(app);
    if viewport_h == 0 {
        return;
    }
    let max_scroll = session_max_scroll(app);
    let new_scroll = if up {
        app.session
            .scroll
            .saturating_add(viewport_h)
            .min(max_scroll)
    } else {
        app.session.scroll.saturating_sub(viewport_h)
    };
    app.set_scroll_anchored(new_scroll);
}

/// The number of visible lines in the session viewport.
fn session_viewport_height(app: &App) -> u32 {
    app.session_area
        .map(|area| area.height.saturating_sub(2) as u32)
        .unwrap_or(0)
}

/// The maximum scroll value (= total - viewport_height).
fn session_max_scroll(app: &mut App) -> u32 {
    if let Some(area) = app.session_area {
        let inner_h = area.height.saturating_sub(2) as u32;
        let total = app.session.count_all_lines_with_width(area.width as usize);
        total.saturating_sub(inner_h)
    } else {
        0
    }
}

fn handle_mouse(m: MouseEvent, app: &mut App) {
    let prompt = app.input_prompt_area;
    let prefix_width = unicode_width::UnicodeWidthStr::width(" > ") as u16;
    let in_prompt_row = prompt.map(|r| m.row == r.y).unwrap_or(false);

    // Mouse wheel scroll — instant jump by the OS step.
    //
    // scroll = offset from bottom.  ScrollUp = see older content
    // (increase offset).  ScrollDown = see newer content (decrease
    // offset).
    //
    // The view lands on the new position in a single frame. The
    // `animating` flag is held for one 16ms tick so the original
    // gating rule ("while the session is still scrolling, ignore
    // new scroll events") is preserved: events arriving within the
    // same render frame as a previous event are dropped.
    let is_wheel = matches!(
        m.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    );
    if is_wheel {
        let now = std::time::Instant::now();
        let step: u32 = if let Ok(mut state) = SCROLL_STATE.lock() {
            let (last, mut step) = *state;
            if let Some(t) = last {
                let ms = now.duration_since(t).as_millis();
                if ms < 15 {
                    step = (step + 1).min(10);
                } else if ms > 80 {
                    step = 3;
                }
            }
            *state = (Some(now), step);
            step
        } else {
            3
        };
        let delta: f32 = match m.kind {
            MouseEventKind::ScrollUp => step as f32,
            MouseEventKind::ScrollDown => -(step as f32),
            _ => unreachable!(),
        };

        // Route wheel to input area when the mouse is over it.
        if let Some(input_area) = app.input_prompt_area {
            if m.row >= input_area.y && m.row < input_area.y + input_area.height {
                if app.input_scroll.animating {
                    return;
                }
                let inner_h = (input_area.height.saturating_sub(2)).max(1) as usize;
                let visible_count = (app.input.buffer.split('\n').count() as u16)
                    .min(inner_h as u16)
                    .max(1) as usize;
                let max_scroll = app
                    .input
                    .buffer
                    .split('\n')
                    .count()
                    .saturating_sub(visible_count) as f32;
                // Input scroll: offset from top. ScrollUp → see older
                // lines → decrease offset (negative delta).
                // ScrollDown → see newer lines → increase offset.
                let input_delta: f32 = match m.kind {
                    MouseEventKind::ScrollUp => -(step as f32),
                    MouseEventKind::ScrollDown => step as f32,
                    _ => unreachable!(),
                };
                app.input_scroll.begin_gesture(input_delta, step, now);
                if app.input_scroll.target > max_scroll {
                    app.input_scroll.target = max_scroll;
                    app.input_scroll.current = max_scroll;
                }
                app.input_scroll_decoupled = true;
                return;
            }
        }

        if app.session_scroll.animating {
            // Gating window active — drop this event.
            return;
        }
        // Clamp the gesture against the real viewport max so a
        // scroll-up at the top of the session doesn't aim past the
        // ceiling.
        let max_scroll_f = if let Some(area) = app.session_area {
            let inner_h = area.height.saturating_sub(2) as u32;
            let total = app.session.count_all_lines_with_width(area.width as usize);
            total.saturating_sub(inner_h) as f32
        } else {
            u32::MAX as f32
        };
        app.session_scroll.begin_gesture(delta, step, now);
        if app.session_scroll.target > max_scroll_f {
            app.session_scroll.target = max_scroll_f;
            app.session_scroll.current = max_scroll_f;
        }
        // Write the integer anchor. The view jumps on the next draw.
        app.session.scroll = app.session_scroll.current.round() as u32;
        return;
    }

    // Check if the click landed inside a toggle block (thinking or
    // tool). If so, capture the pending toggle for Mouse Up — a drag
    // (text selection) cancels it.
    let mut pending_toggle: Option<ToggleTarget> = None;
    if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
        for &(top, bot, msg_idx) in &app.thinking_toggle_rows {
            if m.row >= top && m.row <= bot {
                pending_toggle = Some(ToggleTarget::Thinking(msg_idx));
                break;
            }
        }
        if pending_toggle.is_none() {
            for &(top, bot, msg_idx, tool_idx) in &app.tool_toggle_rows {
                if m.row >= top && m.row <= bot {
                    pending_toggle = Some(ToggleTarget::Tool(msg_idx, tool_idx));
                    break;
                }
            }
        }
    }

    if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
        if let Some(target) = pending_toggle {
            // Defer the actual toggle to Mouse Up so a drag cancels
            // it. Preserve tui_drag_start so a drag can create a
            // selection inside the block. On Mouse Up we only toggle
            // if the user did not end up with a selection.
            app.pending_tool_toggle = match target {
                ToggleTarget::Thinking(mi) => Some((mi, usize::MAX)),
                ToggleTarget::Tool(mi, ti) => Some((mi, ti)),
            };
            app.tui_selection = None;
            // Don't touch tui_drag_start here; let the standard
            // Down branch below record it so selection still works.
        }
    }

    if matches!(m.kind, MouseEventKind::Drag(MouseButton::Left)) {
        // A drag cancels any pending toggle — the user is selecting
        // text inside the block.
        app.pending_tool_toggle = None;
    }

    if matches!(m.kind, MouseEventKind::Up(MouseButton::Left)) {
        if let Some((msg_idx, tool_idx)) = app.pending_tool_toggle.take() {
            // If the user has an selection, do not toggle — they are
            // selecting text inside the block instead of clicking it.
            if let Some(s) = app.tui_selection.as_mut() {
                s.active = false;
                return;
            }
            if tool_idx == usize::MAX {
                // Thinking toggle.
                let width = app.session_area.map(|a| a.width as usize).unwrap_or(120);
                let preview_lines = app.session.tool_preview_lines;
                let old_delta = if let Some(msg) = app.session.messages.get(msg_idx) {
                    let segments = crate::session::render::get_thinking_segments(msg);
                    let old_vis = msg.thinking_visible;
                    let mut old_h: u32 = 0;
                    let mut new_h: u32 = 0;
                    for seg in &segments {
                        old_h += crate::session::render::thinking_block_line_count(
                            &seg.content,
                            old_vis,
                            preview_lines,
                            width,
                        ) as u32;
                        new_h += crate::session::render::thinking_block_line_count(
                            &seg.content,
                            !old_vis,
                            preview_lines,
                            width,
                        ) as u32;
                    }
                    if new_h != old_h {
                        Some(new_h as i64 - old_h as i64)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(msg) = app.session.messages.get_mut(msg_idx) {
                    msg.thinking_visible = !msg.thinking_visible;
                    msg.bump_version();
                }
                app.session.invalidate_layout_cache();
                if let Some(delta) = old_delta {
                    if delta > 0 {
                        app.session.scroll = app.session.scroll.saturating_add(delta as u32);
                    } else if delta < 0 {
                        app.session.scroll = app.session.scroll.saturating_sub((-delta) as u32);
                    }
                    // Sync last_rendered_total to the new total so
                    // pin_scroll_for_total on the next frame does NOT
                    // re-absorb the same delta (double compensation).
                    let w = app.session_area.map(|a| a.width).unwrap_or(120);
                    let new_total = app.session.count_all_lines_with_width(w as usize);
                    app.session.last_rendered_total = Some((w, new_total));
                }
                app.set_scroll_anchored(app.session.scroll);
            } else {
                // Tool block toggle.
                let width = app.session_area.map(|a| a.width as usize).unwrap_or(120);
                let preview_lines = app.session.tool_preview_lines;
                let old_delta = if let Some(msg) = app.session.messages.get(msg_idx) {
                    if let Some(tool) = msg.tool_results.get(tool_idx) {
                        let old_vis = tool.visible;
                        let old_h = crate::session::render::tool_block_line_count(
                            tool,
                            old_vis,
                            preview_lines,
                            width,
                        ) as u32;
                        let new_h = crate::session::render::tool_block_line_count(
                            tool,
                            !old_vis,
                            preview_lines,
                            width,
                        ) as u32;
                        Some(new_h as i64 - old_h as i64)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(msg) = app.session.messages.get_mut(msg_idx) {
                    if let Some(tool) = msg.tool_results.get_mut(tool_idx) {
                        tool.visible = !tool.visible;
                    }
                    msg.bump_version();
                }
                app.session.invalidate_layout_cache();
                if let Some(delta) = old_delta {
                    if delta > 0 {
                        app.session.scroll = app.session.scroll.saturating_add(delta as u32);
                    } else if delta < 0 {
                        app.session.scroll = app.session.scroll.saturating_sub((-delta) as u32);
                    }
                    // Sync last_rendered_total to the new total so
                    // pin_scroll_for_total on the next frame does NOT
                    // re-absorb the same delta (double compensation).
                    let w = app.session_area.map(|a| a.width).unwrap_or(120);
                    let new_total = app.session.count_all_lines_with_width(w as usize);
                    app.session.last_rendered_total = Some((w, new_total));
                }
                app.set_scroll_anchored(app.session.scroll);
            }
            app.tui_selection = None;
            app.tui_drag_start = None;
            return;
        }
    }

    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // Click-to-focus: determine which area was clicked.
            let in_agents = app
                .agents_area
                .map(|a| m.row >= a.y && m.row < a.y + a.height)
                .unwrap_or(false);
            let in_panel = app
                .function_panel_area
                .map(|a| m.row >= a.y && m.row < a.y + a.height)
                .unwrap_or(false);

            if in_agents {
                app.focus_target = crate::function::FocusTarget::AgentsCheckbox;
            } else if in_panel {
                app.focus_target = crate::function::FocusTarget::FunctionPanel;
            } else if in_prompt_row {
                app.focus_target = crate::function::FocusTarget::Input;
            } else {
                // Click in the session area — focus input
                app.focus_target = crate::function::FocusTarget::Input;
            }

            // Clear any prior selection but DO NOT create a new one yet.
            // We only commit a TUI selection when the user actually drags,
            // so a plain click leaves the screen untouched.
            app.tui_selection = None;
            app.selected_text = None;
            app.tui_drag_start = Some((m.column, m.row));
            app.input.clear_selection();
            if let Ok(mut d) = DRAG.lock() {
                d.active = false;
            }
            if in_prompt_row {
                let byte = screen_col_to_byte(&app.input.buffer, m.column, prefix_width);
                app.input.cursor = byte;
                if let Some(p) = prompt {
                    if let Ok(mut d) = DRAG.lock() {
                        d.active = true;
                        d.prefix_width = prefix_width;
                        d.start_byte = byte;
                        d.prompt_row = p.y as i32;
                    }
                }
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            // The first Drag after a Down materializes the selection. Any
            // prior click that did not move never created one, so this is
            // also how a click-only event stays invisible.
            if let Some(start) = app.tui_drag_start {
                if app.tui_selection.is_none() {
                    if let Some(area) = app.session_area {
                        let width = area.width as usize;
                        let total = app.session.count_all_lines_with_width(width);
                        let doc_start =
                            screen_y_to_doc_line(start.1, &area, app.session.scroll, total);
                        let col_start = start.0.saturating_sub(area.x);
                        app.tui_selection = Some(crate::function::Selection {
                            doc_start,
                            doc_end: doc_start,
                            col_start: Some(col_start),
                            col_end: Some(col_start),
                            active: true,
                        });
                    }
                }
                if let Some(sel) = app.tui_selection.as_mut() {
                    if let Some(area) = app.session_area {
                        let width = area.width as usize;
                        let total = app.session.count_all_lines_with_width(width);
                        sel.doc_end = screen_y_to_doc_line(m.row, &area, app.session.scroll, total);
                        sel.col_end = Some(m.column.saturating_sub(area.x));
                    }
                }
            }
            if in_prompt_row {
                let drag = DRAG.lock().ok().and_then(|d| {
                    if d.active {
                        Some((d.prefix_width, d.start_byte))
                    } else {
                        None
                    }
                });
                if let Some((_pw, start_byte)) = drag {
                    let end_byte = screen_col_to_byte(&app.input.buffer, m.column, prefix_width);
                    app.input.cursor = end_byte;
                    if start_byte == end_byte {
                        app.input.clear_selection();
                    } else {
                        let (s, e) = if start_byte <= end_byte {
                            (start_byte, end_byte)
                        } else {
                            (end_byte, start_byte)
                        };
                        app.input.set_selection(s, e);
                    }
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            // A click (Down + Up with no Drag) ends here: no selection
            // was ever created, so nothing to finalize.
            app.tui_drag_start = None;
            app.pending_tool_toggle = None;
            if let Some(sel) = app.tui_selection.as_mut() {
                sel.active = false;
            }
            if let Ok(mut d) = DRAG.lock() {
                d.active = false;
            }
        }
        MouseEventKind::Moved => {
            // Do not cancel a TUI selection or in-progress drag on
            // Moved. A finalized selection persists; an in-progress
            // drag continues. Only the input-area drag lock is released.
            if let Ok(mut d) = DRAG.lock() {
                d.active = false;
            }
        }
        _ => {}
    }

    app.last_mouse_event = Some(Instant::now());
}

pub fn cycle_sidebar_forward(app: &mut App) {
    if app.function.tabs.is_empty() {
        return;
    }
    app.function.active = (app.function.active + 1) % app.function.tabs.len();
    if app.function_visible {
        app.acknowledge_panel();
    }
}

fn submit_input(app: &mut App) {
    // Hide the agents splash area once the user sends input.
    app.agents_visible = false;
    // Sending a prompt always returns focus to the input — never
    // let the function panel steal focus on submit.
    app.focus_target = crate::function::FocusTarget::Input;
    // Snap the chat viewport to the tail before we push any new
    // messages. If the user scrolled up to look at older content,
    // we want their just-submitted message to be visible (so they
    // can confirm it was sent) — not pushed off the bottom. Also
    // cancels any in-flight momentum so the jump lands immediately.
    app.set_scroll_anchored(0);
    // Snap input view back to cursor.
    app.input_scroll_decoupled = false;
    app.input_scroll.snap(0.0);
    // Expand image markers first (while the input buffer still has
    // the markers), then expand paste blocks.
    let raw = app.input.take();
    let (clean_text, mut image_parts) = expand_image_blocks(&raw, &mut app.image_blocks);
    if image_parts.is_empty()
        && try_extract_image_path_from_input(&clean_text, &mut image_parts, app)
    {
        // Image path was extracted and loaded; text is now empty.
        app.sync_completion();
        return;
    }
    let raw = expand_paste_blocks(clean_text, &mut app.paste_blocks);
    if raw.is_empty() && image_parts.is_empty() {
        return;
    }
    if image_parts.is_empty() && submit_direct_tool_input(app, &raw) {
        app.sync_completion();
        return;
    }
    if let Some(rest) = raw.strip_prefix('/') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let cmd: String = parts.next().unwrap_or("").to_lowercase();
        let arg: String = parts.next().unwrap_or("").trim().to_string();
        // Treat `/skill:foo` and `/mcp:foo` as one-shot top-level
        // commands: split the name off the colon so the dispatch
        // table sees `skill` + `foo` rather than a single unknown
        // token.
        // Treat `/skill:foo` and `/mcp:foo` as one-shot top-level
        // commands: split the name off the colon so the dispatch
        // table sees `skill` + `foo` rather than a single unknown
        // token. The trailing `arg` is also forwarded so the user
        // can write `/skill:<name> 加上一些额外说明` and have the
        // extra text treated as the skill's invocation args.
        if let Some((base, name)) = cmd.split_once(':') {
            if base == "skill" {
                crate::commands::dispatch_skill(app, name.trim(), &arg);
                app.sync_completion();
                return;
            }
            if base == "mcp" {
                crate::commands::dispatch(app, base, name.trim());
                app.sync_completion();
                return;
            }
        }
        crate::commands::dispatch(app, &cmd, &arg);
    } else {
        crate::commands::send_chat(app, raw, image_parts);
    }
    // The buffer is now empty, so the completion tab (if any) should close.
    app.sync_completion();
}

/// What pressing Enter (or Shift+Enter) should do, given the configured
/// `EnterBehavior`. Extracted into its own helper so the contract is
/// unit-testable independently of the surrounding key-event plumbing.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum EnterAction {
    /// Submit the input buffer.
    Send,
    /// Insert a newline at the cursor.
    Newline,
}

pub(super) fn enter_action(behavior: crate::config::EnterBehavior, shift: bool) -> EnterAction {
    use crate::config::EnterBehavior;
    match behavior {
        // "Enter sends / Shift+Enter newline":
        //   plain Enter (no shift) submits, Shift+Enter inserts a newline.
        EnterBehavior::EnterSends => {
            if shift {
                EnterAction::Newline
            } else {
                EnterAction::Send
            }
        }
        // "Enter newline / Shift+Enter sends":
        //   plain Enter inserts a newline, Shift+Enter submits.
        EnterBehavior::EnterNewline => {
            if shift {
                EnterAction::Send
            } else {
                EnterAction::Newline
            }
        }
    }
}

/// Ctrl+N: dedicated shortcut for the Notifications tab.
///   - No Notifications tab: create it, show panel, acknowledge
///   - Panel hidden: show panel, focus Notifications
///   - Panel showing and Notifications is active: remove Notifications
///     tab, clear toasts, hide panel if empty
///   - Panel showing but another tab is active: switch to Notifications
pub(crate) fn handle_ctrl_n(app: &mut App) {
    let notif_idx = app
        .function
        .tabs
        .iter()
        .position(|t| matches!(t, crate::function::SidebarTab::Notifications));
    let notif_active = notif_idx.map(|i| i == app.function.active).unwrap_or(false);

    if !app.function_visible || notif_idx.is_none() {
        // Ensure a Notifications tab exists.
        if let Some(i) = notif_idx {
            app.function.active = i;
        } else {
            app.function
                .push(crate::function::SidebarTab::Notifications);
        }
        app.show_panel();
        app.acknowledge_panel();
    } else if notif_active {
        // Remove the Notifications tab.
        if let Some(i) = notif_idx {
            app.function.tabs.remove(i);
            app.function.active = app.function.active.saturating_sub(1);
        }
        app.notifications.clear();
        app.pending_events = 0;
        app.maybe_hide_panel();
    } else if let Some(i) = notif_idx {
        app.function.active = i;
        app.acknowledge_panel();
    }
}
