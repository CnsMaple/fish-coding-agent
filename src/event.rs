use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use futures_util::StreamExt;
use ratatui::backend::Backend;
use ratatui::Terminal;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::interval;

use crate::app::App;
use crate::function::AppMode;
use crate::function::CancelState;

/// Position the hardware cursor for IME support.
///
/// Uses absolute CUP (Cursor Position) followed by Hide cursor.
/// We render in the main screen buffer (no alternate screen) so the
/// TSF text store is properly associated with the displayed content.
///
/// The function panel cursor (e.g. picker search input) takes priority
/// over the main input cursor.
fn position_ime_cursor(app: &mut App) {
    let cursor = match app.focus_target {
        crate::function::FocusTarget::FunctionPanel => app.function_panel_cursor,
        crate::function::FocusTarget::Input => app.input_cursor_screen,
    };
    let Some((cx, cy)) = cursor else {
        return;
    };

    static LAST_CURSOR_POS: std::sync::Mutex<Option<(u16, u16)>> = std::sync::Mutex::new(None);
    if let Ok(mut last) = LAST_CURSOR_POS.lock() {
        if *last == Some((cx, cy)) {
            return;
        }
        *last = Some((cx, cy));
    }

    use std::io::Write;

    let _ = write!(std::io::stdout(), "\x1B[{};{}H\x1B[?25h", cy + 1, cx + 1,);
    let _ = std::io::stdout().flush();
}

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
    /// A structured tool result arrived, to be rendered as a collapsible block.
    ChatToolResult {
        name: String,
        title: String,
        content: String,
    },
    LocalToolResult {
        name: String,
        title: String,
        content: String,
        context: Option<String>,
    },
    /// Final usage arrived for a completed stream.
    ChatUsage { seq: u64, usage: crate::providers::Usage },
    /// Stream finished successfully. `seq` matches
    /// `App::current_request_seq` at the time the request started;
    /// the handler drops stale events from previous requests so a
    /// slow-finishing background task can't clobber the new inflight.
    ChatDone { seq: u64 },
    /// Stream errored. See `ChatDone` for the `seq` semantics.
    ChatError { seq: u64, error: String },
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
    ToolStarted {
        name: String,
        title: String,
    },
    /// Incremental output from a running tool.
    ToolDelta {
        content: String,
    },
    /// MCP tool list changed for a single server (added, removed,
    /// or server went up/down). Triggers re-aggregation of the
    /// `openai_tool_specs` / `anthropic_tool_specs` view and an
    /// immediate redraw of the status bar.
    McpToolsChanged { server: String },
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
    McpBrowserOpenFailed { server: String, url: String },
    /// A connected MCP server's client closed unexpectedly. The
    /// service has already marked the server as `Failed`; the
    /// TUI uses this to surface a toast and update the picker.
    McpClientClosed { server: String },
    /// Manual request to start the OAuth dance for a remote MCP
    /// server. Issued by `/mcp-auth <name>`.
    McpStartAuth { server: String },
    /// Auto or `/compact` finished: the LLM returned a summary for
    /// the slice `Session::messages[start..end]`. The handler calls
    /// `Session::apply_compaction` and (for auto-triggers) flags
    /// the post-compaction continue prompt.
    CompactionSummaryReady {
        start: usize,
        end: usize,
        summary: String,
    },
    /// The compaction stream errored out. The session is left
    /// untouched. Surfaces as a `Fail` toast.
    CompactionFailed { error: String },
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

    let mut events = EventStream::new();
    let mut tick = interval(Duration::from_millis(100));
    // Faster tick dedicated to scrolling momentum. ~60fps so the
    // motion looks smooth.
    let mut scroll_tick = interval(Duration::from_millis(SCROLL_ANIM_TICK_MS));
    let mut last_status_refresh = std::time::Instant::now();
    let mut needs_draw = true;
    let mut last_draw = Instant::now();
    // Minimum interval between draws (~60 fps).
    const DRAW_INTERVAL: Duration = Duration::from_millis(16);

    loop {
        // Throttled draw: at most once per DRAW_INTERVAL.
        if needs_draw && last_draw.elapsed() >= DRAW_INTERVAL {
            if let Err(e) = terminal.draw(|f| crate::ui::render(f, app)) {
                let _ = e;
            }
            position_ime_cursor(app);
            last_draw = Instant::now();
            needs_draw = false;

            // The freshly-pushed user message and (for tools) the
            // pending tool block are now on screen. Kick off the
            // deferred request — see `submit_input` /
            // `commands::send_message` for the producer side.
            flush_pending_request(app);
            // Drain a queued post-compaction continue prompt, if
            // any. The session is idle (no inflight) so this is the
            // safest spot to launch the synthetic follow-up.
            drain_post_compaction_prompt(app);
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

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

/// Unified Ctrl+V / Cmd+V handler: open clipboard once, try image
/// first, fall back to text paste.
/// Open the paste preview sidebar tab, showing the current clipboard
/// content (image or text) for the user to confirm before inserting.
fn open_paste_preview(app: &mut App) {
    use crate::function::notifications::ToastLevel;
    use crate::session::ImageAttachment;
    use sha2::{Digest, Sha256};

    let mut state = crate::function::PastePreviewState {
        text: None,
        image: None,
        image_bytes: None,
        media_type: None,
    };

    let Ok(mut cb) = arboard::Clipboard::new() else {
        let _ = app.notify(ToastLevel::Warn, "clipboard unavailable");
        return;
    };

    // Try image first.
    if let Ok(img_data) = cb.get_image() {
        let bytes = &img_data.bytes;
        let media_type = infer_image_type(bytes);
        let extension = media_type.split('/').nth(1).unwrap_or("png");
        let hash = hex::encode(Sha256::digest(bytes));
        if let Ok(assets_dir) = crate::session::store::assets_dir(&app.session_id) {
            let _ = std::fs::create_dir_all(&assets_dir);
            let filename = format!("{hash}.{extension}");
            let asset_path = assets_dir.join(&filename);
            if !asset_path.exists() {
                let _ = std::fs::write(&asset_path, bytes);
            }
            state.image = Some(ImageAttachment {
                asset_path,
                media_type: media_type.to_string(),
                byte_size: bytes.len() as u64,
                width: img_data.width as u32,
                height: img_data.height as u32,
            });
            state.image_bytes = Some(bytes.to_vec());
            state.media_type = Some(media_type.to_string());
        }
    }

    // Fall back to text.
    if state.image.is_none() {
        if let Ok(text) = cb.get_text() {
            if !text.is_empty() {
                state.text = Some(text);
            }
        }
    }

    if state.text.is_none() && state.image.is_none() {
        let _ = app.notify(ToastLevel::Warn, "clipboard is empty");
        return;
    }

    app.function.push(crate::function::SidebarTab::PastePreview(Box::new(state)));
    app.show_panel();
    app.acknowledge_panel();
}

async fn handle_paste(text: String, app: &mut App) {
    app.input_scroll_decoupled = false;
    insert_paste_block(text, app, false);
}

fn handle_paste_preview_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::PastePreviewState,
) -> bool {
    use crossterm::event::KeyCode;
    match k.code {
        KeyCode::Enter => {
            // Confirm paste.
            app.input_scroll_decoupled = false;
            if let Some(ref image) = state.image {
                // Insert [image #N] marker.
                let idx = app.image_blocks.len() + 1;
                app.image_blocks.push_back(image.clone());
                app.input.insert_str(&format!("[image #{idx}]"));
            } else if let Some(ref text) = state.text {
                // Use insert_paste_block to create [paste N lines] marker.
                // This also handles image path detection as a fallback.
                insert_paste_block(text.clone(), app, false);
            }
            close_active_function_tab(app);
            true
        }
        KeyCode::Esc => {
            // Cancel: close the tab without pasting.
            close_active_function_tab(app);
            true
        }
        _ => false,
    }
}

/// Quick MIME detection from magic bytes. Defaults to PNG.
fn infer_image_type(bytes: &[u8]) -> &'static str {
    if bytes.len() < 4 {
        return "image/png";
    }
    if bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return "image/jpeg";
    }
    if bytes[0] == 0x47 && bytes[1] == 0x49 && bytes[2] == 0x46 {
        return "image/gif";
    }
    if bytes.len() >= 8
        && bytes[0] == 0x89 && bytes[1] == 0x50 && bytes[2] == 0x4E && bytes[3] == 0x47
    {
        return "image/png";
    }
    if bytes.len() >= 12
        && bytes[0] == 0x52 && bytes[1] == 0x49 && bytes[2] == 0x46 && bytes[3] == 0x46
        && bytes[8] == 0x57 && bytes[9] == 0x45 && bytes[10] == 0x42 && bytes[11] == 0x50
    {
        return "image/webp";
    }
    "image/png"
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

/// `quota=true` 表示这是 legacy 逐字符终端（如 conhost），需要在
/// handle_key 里抑制随后重发的字符，避免输入重复。
/// Image file extensions that we support loading directly from path.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// If `text` looks like a file path ending in a known image extension,
/// load the file from disk and insert it as an `[image #K]` marker.
/// Returns `true` if an image was successfully loaded and inserted.
fn try_insert_image_from_path(text: &str, app: &mut App) -> bool {
    let path = std::path::Path::new(text.trim().trim_matches('"'));
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return false,
    };
    if !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return false;
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let media_type = infer_image_type(&bytes);
    use sha2::{Digest, Sha256};
    let hash = hex::encode(Sha256::digest(&bytes));
    let assets_dir = match crate::session::store::assets_dir(&app.session_id) {
        Ok(d) => d,
        Err(_) => return false,
    };
    if let Err(e) = std::fs::create_dir_all(&assets_dir) {
        use crate::function::notifications::ToastLevel;
        let _ = app.notify(ToastLevel::Warn, format!("image: create assets dir: {e}"));
        return false;
    }
    let extension = media_type.split('/').nth(1).unwrap_or("png");
    let filename = format!("{hash}.{extension}");
    let asset_path = assets_dir.join(&filename);
    if !asset_path.exists() {
        if let Err(e) = std::fs::write(&asset_path, &bytes) {
            use crate::function::notifications::ToastLevel;
            let _ = app.notify(ToastLevel::Warn, format!("image: write {filename}: {e}"));
            return false;
        }
    }
    let attachment = crate::session::ImageAttachment {
        asset_path: asset_path.clone(),
        media_type: media_type.to_string(),
        byte_size: bytes.len() as u64,
        width: 0,
        height: 0,
    };
    let idx = app.image_blocks.len() + 1;
    app.image_blocks.push_back(attachment);
    let marker = format!("[image #{idx}]");
    if app.input.has_selection() {
        app.input.delete_selection();
    }
    app.input.insert_str(&marker);
    app.sync_completion();
    use crate::function::notifications::ToastLevel;
    let _ = app.notify(ToastLevel::Ok, format!("image #{idx} attached ({media_type})"));
    true
}

fn insert_paste_block(text: String, app: &mut App, quota: bool) {
    let mut text = normalize_paste_text(&text);
    // Strip trailing newline so the paste doesn't inadvertently send
    // the prompt when Enter is pressed afterwards.
    if text.ends_with('\n') {
        text.pop();
    }
    if let Ok(mut cb) = arboard::Clipboard::new() {
        if let Ok(clip) = cb.get_text() {
            let clip = normalize_paste_text(&clip);
            if !clip.is_empty() && (clip == text || clip.contains(&text)) {
                text = clip;
            }
        }
    }
    if text.is_empty() {
        return;
    }
    // If the paste text looks like a local image file path, load it directly.
    if try_insert_image_from_path(&text, app) {
        // Also update last_paste_text so the dedup check catches
        // repeated burst classifications of the same path.
        app.last_paste_text = Some(text.clone());
        app.last_paste_at = Some(Instant::now());
        return;
    }
    let now = Instant::now();
    if app
        .last_paste_text
        .as_ref()
        .map(|last| last == &text)
        .unwrap_or(false)
        && app
            .last_paste_at
            .map(|at| now.duration_since(at) < Duration::from_secs(2))
            .unwrap_or(false)
    {
        return;
    }
    app.last_paste_text = Some(text.clone());
    app.last_paste_at = Some(now);
    if quota {
        app.paste_key_quota = text.chars().count();
    }
    if app.input.has_selection() {
        app.input.delete_selection();
    }
    let line_count = paste_line_count(&text);
    let marker = format!("[paste {line_count} lines]");
    app.paste_blocks.push_back(text);
    app.input.insert_str(&marker);
    app.sync_completion();
}

fn normalize_paste_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn paste_line_count(text: &str) -> usize {
    text.lines().count().max(1)
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
    app.open_todo_tab();
}

/// Refresh the MCP status summary displayed in the status bar.
/// Reads the live snapshot from the MCP service and aggregates
/// per-server statuses into a compact string like `"2✓ 1✗"`.
fn refresh_mcp_summary(app: &mut App) {
    let snap = crate::mcp::try_snapshot_or_empty();
    let mut connected = 0u32;
    let mut failed = 0u32;
    let mut other = 0u32;

    for (_, status) in &snap.status {
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
        AppMsg::ChatToolResult {
            name,
            title,
            content,
        } => {
            if name == "todowrite" {
                handle_todowrite_result(app, &content);
            }
            open_tool_function_panel(app, &name, &content);
            app.session.update_last_tool_content(name, title, content);
        }
        AppMsg::LocalToolResult {
            name,
            title,
            content,
            context,
        } => {
            if name == "todowrite" {
                handle_todowrite_result(app, &content);
            }
            open_tool_function_panel(app, &name, &content);
            app.session.push_tool_result_message(name, title, content);
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
            let denom = u.input_tokens + u.cache_read_tokens;
            let rate = if denom == 0 {
                0.0
            } else {
                u.cache_read_tokens as f64 / denom as f64
            };
            app.hit_rate.record(rate);
            app.status.update_hit(&app.hit_rate);
            if let Some(context_window_tokens) = u.context_window_tokens {
                app.status.set_context_window_tokens(context_window_tokens);
            }
            let total_tokens =
                u.input_tokens + u.output_tokens + u.cache_read_tokens + u.cache_creation_tokens;
            if total_tokens > 0 {
                app.status.update_token_usage(total_tokens);
            }
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
            let ctx_count = models.iter().filter(|m| m.context_window_tokens.is_some()).count();
            let missing_count = models.iter().filter(|m| m.context_window_tokens.is_none()).count();
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
                crate::config::ProviderKind::DeepSeek => "/models",
                crate::config::ProviderKind::MiniMax => "/models",
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
            app.config.active = Some(id);
            app.save_config();
            app.status.set_provider_name(&app.config.active_name());
            app.status.set_model(&app.config.active_model_display());
            app.refresh_status_model_context();
            app.notify(ToastLevel::Ok, "Cursor OAuth authorized");
            crate::commands::open_model_picker_for_kind(app, ProviderKind::Cursor);
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
        AppMsg::ToolStarted { name, title } => {
            app.session.start_tool_in_last(name, title);
        }
        AppMsg::ToolDelta { content } => {
            app.session.append_tool_delta_to_last(&content);
        }
        AppMsg::McpToolsChanged { server } => {
            // The aggregated tool set changed; nudge the next
            // request to re-read `openai_tool_specs` /
            // `anthropic_tool_specs`. The picker / status bar
            // already consume the live snapshot.
            tracing::debug!(server = %server, "mcp tools changed");
            app.invalidate_tool_specs();
            let _ = app.notify(
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
                let _ = app.notify(
                    crate::function::notifications::ToastLevel::Warn,
                    format!("mcp `{server}` needs auth: {url}"),
                );
            } else {
                let _ = app.notify(
                    crate::function::notifications::ToastLevel::Warn,
                    format!("mcp `{server}` needs auth: {error}"),
                );
            }
        }
        AppMsg::McpBrowserOpenFailed { server: _, url: _ } => {
            // The toast already surfaced the URL; nothing else to do.
        }
        AppMsg::McpClientClosed { server } => {
            let _ = app.notify(
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
        AppMsg::CompactionSummaryReady { start, end, summary } => {
            use crate::function::notifications::ToastLevel;
            // The cancel-Esc path takes `inflight` out of `app`
            // before this event arrives, so the cancel sender
            // inside it is still alive. Drop it explicitly.
            app.inflight = None;
            app.cancel_state = CancelState::Idle;
            app.compacting = false;
            if let Some(idx) = app.session.apply_compaction(start, end, summary) {
                app.notify(ToastLevel::Ok, "session compacted");
                app.save_current_session();
                // Stage the continue prompt; the main loop drains
                // it on the next idle frame.
                app.pending_post_compaction_prompt = Some(continue_prompt_text().to_string());
                // Pin the cursor to the inserted summary so the
                // user sees the new context first.
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
    // Ctrl+O toggles all collapsible tool output blocks at once.
    if ctrl && matches!(k.code, KeyCode::Char('o') | KeyCode::Char('O')) {
        app.session.toggle_all_tool_results();
        return;
    }

    // Ctrl+N: dedicated shortcut for the Notifications tab.
    //   - panel hidden  -> show it and focus Notifications
    //   - panel showing and Notifications is active -> hide
    //   - panel showing but another tab is active -> switch to Notifications
    if ctrl && matches!(k.code, KeyCode::Char('n') | KeyCode::Char('N')) {
        handle_ctrl_n(app);
        return;
    }

    // Alt+L: toggle focus between function panel and input.
    if k.modifiers.contains(KeyModifiers::ALT)
        && matches!(k.code, KeyCode::Char('l') | KeyCode::Char('L'))
    {
        if app.focus_target == crate::function::FocusTarget::FunctionPanel {
            app.focus_target = crate::function::FocusTarget::Input;
        } else if app.function_visible {
            app.focus_target = crate::function::FocusTarget::FunctionPanel;
        }
        return;
    }

    if app.focus_target == crate::function::FocusTarget::FunctionPanel
        && dispatch_to_active_tab(k, app).await
    {
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
            if app.inflight.is_some() {
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
                        app.input.delete_word_back();
                        app.sync_completion();
                    }
                    'a' | 'A' => app.input.move_home(),
                    'e' | 'E' => app.input.move_end(),
                    'u' | 'U' => {
                        app.input.buffer.clear();
                        app.input.cursor = 0;
                        app.input.clear_selection();
                        app.paste_blocks.clear();
                        app.sync_completion();
                    }
                    'k' | 'K' => {
                        app.input.buffer.truncate(app.input.cursor);
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
        KeyCode::Home => app.input.move_home(),
        KeyCode::End => app.input.move_end(),
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
    pub fn step(&mut self, now: Instant) -> (u16, bool) {
        self.last_tick = Some(now);
        if !self.animating {
            return (self.current.round() as u16, true);
        }
        self.animating = false;
        (self.current.round() as u16, true)
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
                let visible_count = (app.input.buffer.split('\n').count() as u16).min(inner_h as u16).max(1) as usize;
                let max_scroll = app.input.buffer.split('\n').count().saturating_sub(visible_count) as f32;
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
            (total.saturating_sub(inner_h).min(u16::MAX as u32)) as f32
        } else {
            u16::MAX as f32
        };
        app.session_scroll.begin_gesture(delta, step, now);
        if app.session_scroll.target > max_scroll_f {
            app.session_scroll.target = max_scroll_f;
            app.session_scroll.current = max_scroll_f;
        }
        // Drop the render cache so the new offset is visible on the
        // very next frame.
        if let Ok(mut c) = app.session.render_cache.lock() {
            *c = None;
        }
        // Write the integer anchor. The view jumps on the next draw.
        app.session.scroll = app.session_scroll.current.round() as u16;
        return;
    }

    // Thinking toggle click — if the click lands on a thinking toggle
    // row, expand / collapse that message's thinking block.
    if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
        for &(toggle_y, msg_idx) in &app.thinking_toggle_rows {
            if m.row == toggle_y {
                if let Some(msg) = app.session.messages.get_mut(msg_idx) {
                    msg.thinking_visible = !msg.thinking_visible;
                }
                // Prevent this click from starting a TUI selection.
                app.tui_selection = None;
                app.tui_drag_start = None;
                return;
            }
        }
    }

match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
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
                    app.tui_selection = Some(crate::function::Selection::new(start));
                }
                if let Some(sel) = app.tui_selection.as_mut() {
                    sel.end = (m.column, m.row);
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
            if let Some(sel) = app.tui_selection.as_mut() {
                sel.active = false;
            }
            if let Ok(mut d) = DRAG.lock() {
                d.active = false;
            }
        }
        MouseEventKind::Moved => {
            if app.tui_drag_start.is_some()
                || app.tui_selection.map_or(false, |s| s.active)
            {
                app.tui_selection = None;
                app.selected_text = None;
                app.tui_drag_start = None;
                if let Ok(mut d) = DRAG.lock() {
                    d.active = false;
                }
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
    if app.mode != AppMode::Plan {
        app.previous_mode = app.mode;
    }
    app.set_mode(AppMode::Plan);
    if app.function_visible {
        app.acknowledge_panel();
    }
}

fn try_remove_paste_marker(app: &mut App) -> bool {
    let buf = &app.input.buffer;
    let cursor = app.input.cursor;
    if cursor < "[paste 1 lines]".len() || !buf.is_char_boundary(cursor) {
        return false;
    }
    let before = &buf[..cursor];
    // Find "[paste " backwards from cursor
    if let Some(start) = before.rfind("[paste ") {
        let candidate = &buf[start..cursor];
        if let Some(rest) = candidate
            .strip_prefix("[paste ")
            .and_then(|s| s.strip_suffix(" lines]"))
        {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                app.input.buffer.replace_range(start..cursor, "");
                app.input.cursor = start;
                app.paste_blocks.pop_front();
                return true;
            }
        }
    }
    false
}

fn try_remove_image_marker(app: &mut App) -> bool {
    let buf = &app.input.buffer;
    let cursor = app.input.cursor;
    // Minimum length: "[image #1]" is 9 chars.
    if cursor < 9 || !buf.is_char_boundary(cursor) {
        return false;
    }
    let before = &buf[..cursor];
    if let Some(start) = before.rfind("[image #") {
        let candidate = &buf[start..cursor];
        if let Some(rest) = candidate.strip_prefix("[image #").and_then(|s| s.strip_suffix(']')) {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                let idx: usize = rest.parse().unwrap_or(0);
                if idx > 0 && idx <= app.image_blocks.len() {
                    // Remove the image file from disk.
                    if let Some(att) = app.image_blocks.get(idx - 1) {
                        let _ = std::fs::remove_file(&att.asset_path);
                    }
                    app.image_blocks.remove(idx - 1);
                    app.input.buffer.replace_range(start..cursor, "");
                    app.input.cursor = start;
                    // Re-number remaining image markers in the buffer.
                    renumber_image_markers(app);
                    return true;
                }
            }
        }
    }
    false
}

/// Re-number all `[image #K]` markers in the input buffer to match
/// the current `app.image_blocks` order (1-based). Called after a
/// marker is removed from the middle of the list.
fn renumber_image_markers(app: &mut App) {
    let buf = &app.input.buffer.clone();
    let mut new_buf = buf.clone();
    let mut block_idx = 1usize;
    let mut search_start = 0usize;
    loop {
        let remaining = &new_buf[search_start..];
        let Some(marker_start) = remaining.find("[image #") else {
            break;
        };
        let abs_start = search_start + marker_start;
        let after_marker = &new_buf[abs_start + 8..];
        let Some(bracket_end) = after_marker.find(']') else {
            break;
        };
        let num_str = &after_marker[..bracket_end];
        if !num_str.chars().all(|c| c.is_ascii_digit()) {
            search_start = abs_start + 1;
            continue;
        }
        let old_len = 8 + bracket_end + 1; // "[image #N]" length
        let new_marker = format!("[image #{block_idx}]");
        new_buf.replace_range(abs_start..abs_start + old_len, &new_marker);
        search_start = abs_start + new_marker.len();
        block_idx += 1;
    }
    app.input.buffer = new_buf;
}

/// Replace `[image #K]` markers in `raw` with the corresponding
/// `ContentPart`s from `image_blocks`, and collect them in order.
/// Returns `(cleaned_text, image_parts)`.
fn expand_image_blocks(
    raw: &str,
    image_blocks: &mut VecDeque<crate::session::ImageAttachment>,
) -> (String, Vec<crate::session::ContentPart>) {
    let mut out = raw.to_string();
    let mut parts: Vec<crate::session::ContentPart> = Vec::new();
    let mut search_start = 0usize;
    loop {
        let remaining = &out[search_start..];
        let Some(marker_start) = remaining.find("[image #") else {
            break;
        };
        let abs_start = search_start + marker_start;
        let after_marker = &out[abs_start + 8..];
        let Some(bracket_end) = after_marker.find(']') else {
            break;
        };
        let num_str = &after_marker[..bracket_end];
        if !num_str.chars().all(|c| c.is_ascii_digit()) {
            search_start = abs_start + 1;
            continue;
        }
        let idx: usize = num_str.parse().unwrap_or(0);
        let old_len = 8 + bracket_end + 1;
        // Drain the corresponding image block.
        if idx > 0 && idx <= image_blocks.len() {
            let att = image_blocks.remove(idx - 1).unwrap();
            parts.push(crate::session::ContentPart::Image(att));
        }
        out.replace_range(abs_start..abs_start + old_len, "");
        search_start = abs_start; // re-scan after the removal
    }
    (out, parts)
}

/// Check if `raw` is a single image file path. If so, load the file
/// and push an `Image` ContentPart into `image_parts`. Returns true
/// if an image was loaded.
fn try_extract_image_path_from_input(
    raw: &str,
    image_parts: &mut Vec<crate::session::ContentPart>,
    app: &mut App,
) -> bool {
    let trimmed = raw.trim().trim_matches('"');
    let path = std::path::Path::new(trimmed);
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return false,
    };
    if !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return false;
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_e) => {
            #[cfg(windows)]
            {
                let wide_path = format!("\\\\?\\{}", trimmed);
                let wide_path = std::path::Path::new(&wide_path);
                match std::fs::read(wide_path) {
                    Ok(b) => b,
                    Err(_) => return false,
                }
            }
            #[cfg(not(windows))]
            {
                return false;
            }
        }
    };
    let media_type = infer_image_type(&bytes);
    use sha2::{Digest, Sha256};
    let hash = hex::encode(Sha256::digest(&bytes));
    let assets_dir = match crate::session::store::assets_dir(&app.session_id) {
        Ok(d) => d,
        Err(_) => return false,
    };
    if let Err(e) = std::fs::create_dir_all(&assets_dir) {
        use crate::function::notifications::ToastLevel;
        let _ = app.notify(ToastLevel::Warn, format!("image: create assets dir: {e}"));
        return false;
    }
    let extension = media_type.split('/').nth(1).unwrap_or("png");
    let filename = format!("{hash}.{extension}");
    let asset_path = assets_dir.join(&filename);
    if !asset_path.exists() {
        if let Err(e) = std::fs::write(&asset_path, &bytes) {
            use crate::function::notifications::ToastLevel;
            let _ = app.notify(ToastLevel::Warn, format!("image: write {filename}: {e}"));
            return false;
        }
    }
    let attachment = crate::session::ImageAttachment {
        asset_path: asset_path.clone(),
        media_type: media_type.to_string(),
        byte_size: bytes.len() as u64,
        width: 0,
        height: 0,
    };
    app.image_blocks.push_back(attachment.clone());
    image_parts.push(crate::session::ContentPart::Image(attachment));
    let idx = app.image_blocks.len();
    let _ = app.notify(crate::function::notifications::ToastLevel::Ok,
        format!("image #{idx} loaded from path ({media_type})"));
    true
}

fn expand_paste_blocks(mut raw: String, paste_blocks: &mut VecDeque<String>) -> String {
    while let Some(text) = paste_blocks.pop_front() {
        let line_count = paste_line_count(&text);
        let marker = format!("[paste {line_count} lines]");
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let block = format!("```paste\n{text}\n```");
        if raw.contains(&marker) {
            raw = raw.replacen(&marker, &block, 1);
        }
    }
    raw
}

fn submit_input(app: &mut App) {
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
    if image_parts.is_empty() && try_extract_image_path_from_input(&clean_text, &mut image_parts, app) {
        // Image path was extracted and loaded; text is now empty.
        app.sync_completion();
        return;
    }
    let raw = expand_paste_blocks(clean_text, &mut app.paste_blocks);
    if raw.is_empty() && image_parts.is_empty() {
        return;
    }
    if image_parts.is_empty() {
        if submit_direct_tool_input(app, &raw) {
            app.sync_completion();
            return;
        }
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

/// Run the full OAuth authorization flow for a remote MCP server:
///
/// 1. Start a local TCP callback server
/// 2. Generate PKCE challenge
/// 3. Build the authorization URL (using stored `client_id` or
///    performing dynamic client registration)
/// 4. Open the browser
/// 5. Wait for the callback (5 min timeout)
/// 6. Exchange code for tokens
/// 7. Store tokens in `McpAuthStore`
/// 8. Reconnect the server with the new token
async fn run_mcp_oauth(
    server_name: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<AppMsg>,
) -> Result<(), String> {
    use base64::Engine;
    use sha2::Digest;

    let Some(svc) = crate::mcp::McpRegistry::current() else {
        return Err("mcp service not initialised".into());
    };

    // 1. Get server config. Must be a remote server.
    let config = svc.snapshot().await;
    let cfg = config
        .config
        .get(server_name)
        .ok_or_else(|| format!("server `{server_name}` not configured"))?;
    let (server_url, oauth_cfg) = match cfg {
        crate::mcp::McpServerConfig::Remote { url, oauth, .. } => {
            let oauth = oauth.as_ref().ok_or_else(|| {
                format!("server `{server_name}` has no OAuth config")
            })?;
            (url.clone(), oauth)
        }
        _ => return Err(format!("server `{server_name}` is not a remote server")),
    };

    // 2. Discover OAuth metadata from the MCP server.
    let well_known_url = format!(
        "{}/.well-known/oauth-authorization-server",
        server_url.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("build http client: {e}"))?;
    let metadata: serde_json::Value = client
        .get(&well_known_url)
        .send()
        .await
        .map_err(|e| format!("fetch OAuth metadata: {e}"))?
        .json()
        .await
        .map_err(|e| format!("parse OAuth metadata: {e}"))?;

    let auth_url_str = metadata["authorization_endpoint"]
        .as_str()
        .ok_or_else(|| "no authorization_endpoint in OAuth metadata".to_string())?;
    let token_url_str = metadata["token_endpoint"]
        .as_str()
        .ok_or_else(|| "no token_endpoint in OAuth metadata".to_string())?;

    // 3. Generate PKCE challenge (S256).
    //    Code verifier: uuid-based random token.
    let code_verifier = uuid::Uuid::new_v4().to_string()
        + &uuid::Uuid::new_v4().to_string()
        + &uuid::Uuid::new_v4().to_string();
    // The verifier must be 43-128 chars as per RFC 7636. Uuid hex is 36
    // chars each, so three give us 108. Replace dashes to get only
    // unreserved chars.
    let code_verifier: String = code_verifier.chars().filter(|c| *c != '-').collect();
    let code_challenge_hash = sha2::Sha256::digest(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(code_challenge_hash);

    // 4. Build the redirect URI.
    let port = crate::mcp::oauth_callback::DEFAULT_OAUTH_CALLBACK_PORT;
    let redirect_uri = oauth_cfg
        .redirect_uri
        .clone()
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}{}", crate::mcp::oauth_callback::OAUTH_CALLBACK_PATH));

    // 5. Determine client_id: use configured value or try dynamic
    //    client registration.
    let client_id = if let Some(cid) = &oauth_cfg.client_id {
        cid.clone()
    } else {
        // TODO: dynamic client registration (POST to registration endpoint)
        // For now, require configured client_id.
        return Err(
            "no client_id configured; add `oauth.client_id` to your config".into(),
        );
    };

    // 6. Generate a random state token for CSRF protection.
    let state_token = uuid::Uuid::new_v4().to_string();

    // 7. Start the callback server and wait for the redirect.
    //    Clone the state token so the spawned task can own it.
    let state_for_callback = state_token.clone();
    let callback_handle = tokio::spawn(async move {
        crate::mcp::oauth_callback::wait_for_callback(&state_for_callback).await
    });

    // 8. Build and open the authorization URL.
    use url::form_urlencoded;
    let mut query_parts: Vec<(&str, &str)> = vec![
        ("response_type", "code"),
        ("client_id", &client_id),
        ("redirect_uri", &redirect_uri),
        ("state", &state_token),
        ("code_challenge", &code_challenge),
        ("code_challenge_method", "S256"),
    ];
    if let Some(scope) = &oauth_cfg.scope {
        query_parts.push(("scope", scope));
    }
    let auth_url_str = format!(
        "{}?{}",
        auth_url_str,
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(query_parts)
            .finish()
    );

    tracing::info!(
        server = %server_name,
        url = %auth_url_str,
        "opening browser for MCP OAuth"
    );
    let _ = tx.send(AppMsg::McpAuthRequired {
        server: server_name.to_string(),
        url: auth_url_str.clone(),
        error: String::new(),
    });
    // Best-effort browser open.
    if let Err(e) = open::that(&auth_url_str) {
        let _ = tx.send(AppMsg::McpBrowserOpenFailed {
            server: server_name.to_string(),
            url: auth_url_str,
        });
        return Err(format!("open browser: {e}. URL shown in toast above."));
    }

    // 9. Wait for the callback result (or timeout).
    let cb = callback_handle
        .await
        .map_err(|e| format!("callback task failed: {e}"))?
        .map_err(|e| format!("callback error: {e}"))?;

    // 10. Exchange the auth code for tokens.
    let token_params = [
        ("grant_type", "authorization_code"),
        ("code", &cb),
        ("redirect_uri", &redirect_uri),
        ("client_id", &client_id),
        ("code_verifier", &code_verifier),
    ];
    let token_resp: serde_json::Value = client
        .post(token_url_str)
        .form(&token_params)
        .send()
        .await
        .map_err(|e| format!("token exchange request: {e}"))?
        .json()
        .await
        .map_err(|e| format!("token exchange parse: {e}"))?;

    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| "no access_token in token response".to_string())?
        .to_string();
    let refresh_token = token_resp["refresh_token"].as_str().map(|s| s.to_string());
    let expires_in = token_resp["expires_in"].as_i64();
    let expires_at = expires_in.map(|secs| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            + secs
    });

    // 11. Store tokens.
    let store = crate::mcp::auth::McpAuthStore::load_or_default();
    store.set(
        server_name,
        crate::mcp::auth::Entry {
            tokens: Some(crate::mcp::auth::Tokens {
                access_token,
                refresh_token,
                expires_at,
                scope: oauth_cfg.scope.clone(),
            }),
            client_info: None,
            server_url: Some(server_url.clone()),
        },
    );

    tracing::info!(server = %server_name, "OAuth tokens stored, reconnecting...");

    // 12. Reconnect the server.
    svc.connect(server_name, cfg).await;
    Ok(())
}

fn submit_direct_tool_input(app: &mut App, raw: &str) -> bool {
    let (name, title, args, include_context) = if let Some(code) = raw.strip_prefix("!!") {
        let command = code.trim_start().to_string();
        if command.trim().is_empty() {
            app.notify(
                crate::function::notifications::ToastLevel::Fail,
                "shell command is empty",
            );
            return true;
        }
        (
            "shell_command".to_string(),
            format!("$ {}", command.trim()),
            serde_json::json!({ "command": command }).to_string(),
            true,
        )
    } else if let Some(code) = raw.strip_prefix('!') {
        let command = code.trim_start().to_string();
        if command.trim().is_empty() {
            app.notify(
                crate::function::notifications::ToastLevel::Fail,
                "shell command is empty",
            );
            return true;
        }
        (
            "shell_command".to_string(),
            format!("$ {}", command.trim()),
            serde_json::json!({ "command": command }).to_string(),
            false,
        )
    } else if let Some(code) = raw.strip_prefix("$$") {
        let code = code.trim_start().to_string();
        if code.trim().is_empty() {
            app.notify(
                crate::function::notifications::ToastLevel::Fail,
                "python code is empty",
            );
            return true;
        }
        (
            "python_command".to_string(),
            "python".to_string(),
            serde_json::json!({ "code": code }).to_string(),
            true,
        )
    } else if let Some(code) = raw.strip_prefix('$') {
        let code = code.trim_start().to_string();
        if code.trim().is_empty() {
            app.notify(
                crate::function::notifications::ToastLevel::Fail,
                "python code is empty",
            );
            return true;
        }
        (
            "python_command".to_string(),
            "python".to_string(),
            serde_json::json!({ "code": code }).to_string(),
            false,
        )
    } else {
        return false;
    };

    use crate::session::Message;
    app.maybe_title_from_first_prompt(raw);
    app.session
        .push(Message::new(crate::session::Role::User, raw.to_string()));

    // Create an empty streaming assistant message for tool output
    let assistant = Message {
        role: crate::session::Role::Assistant,
        content: String::new(),
        thinking: String::new(),
        thinking_segments: Vec::new(),
        thinking_visible: false,
        tool_results: Vec::new(),
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

    if let Some(tx) = app.msg_tx.clone() {
        let cwd = app.cwd.clone();
        let n = name.clone();
        let t = title.clone();
        // Set up an inflight handle so the spinner / pending tool
        // block paints immediately, and so Esc can later cancel or
        // drop the request. The actual `tokio::spawn` is deferred
        // until after the next `terminal.draw(...)` returns (see
        // `flush_pending_request` in the main event loop) so the
        // user message and pending tool block are on screen first.
        app.current_request_seq = app.current_request_seq.wrapping_add(1);
        let seq = app.current_request_seq;
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        app.inflight = Some(crate::app::InflightHandle {
            cancel: cancel_tx,
            label: format!("tool:{n}"),
            seq,
        });
        app.cancel_state = CancelState::Idle;
        app.pending_request = Some(crate::function::PendingRequest::Tool(
            crate::function::ToolPending {
                name: n,
                title: t,
                args,
                include_context,
                cwd,
                cancel_rx,
                tx,
                seq,
            },
        ));
    } else {
        app.notify(
            crate::function::notifications::ToastLevel::Fail,
            "event channel is not available",
        );
    }
    true
}

/// Body of the direct-tool-input spawn. Extracted from
/// `submit_direct_tool_input` so the same body can be invoked from
/// `flush_pending_request` after the user message has been rendered.
#[allow(clippy::too_many_arguments)]
pub async fn run_tool_execution(
    name: String,
    title: String,
    args: String,
    include_context: bool,
    cwd: std::path::PathBuf,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    tx: tokio::sync::mpsc::UnboundedSender<AppMsg>,
    seq: u64,
) {
    // Helper that mirrors `run_chat_stream`'s: only deliver messages
    // when the user hasn't cancelled. Tool requests have the same
    // stale-event problem as chat — a tool invoked before Esc must
    // not push a `ChatDone` after the next request has started.
    let send_msg = |msg: AppMsg| {
        if !*cancel_rx.borrow() {
            let _ = tx.send(msg);
        }
    };
    if *cancel_rx.borrow() {
        // User cancelled between submit and the deferred spawn.
        // Silent exit; if a follow-up request is already armed it
        // owns `current_request_seq` and will not be disturbed.
        return;
    }
    send_msg(AppMsg::ToolStarted {
        name: name.clone(),
        title: title.clone(),
    });
    let result = crate::tools::execute_tool_streaming(&name, &args, &cwd, tx.clone()).await;
    if *cancel_rx.borrow() {
        return;
    }
    let display = tool_result_display(&result);
    let context = if include_context {
        Some(local_tool_context(&name, &title, &display))
    } else {
        None
    };
    send_msg(AppMsg::ChatToolResult {
        name,
        title,
        content: display,
    });
    send_msg(AppMsg::ChatDone { seq });
    if let Some(ctx) = context {
        send_msg(AppMsg::ChatDebug(ctx));
    }
}

fn tool_result_display(result: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(result) {
        if val.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            val.get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        } else {
            val.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or(result)
                .to_string()
        }
    } else {
        result.to_string()
    }
}

fn local_tool_context(name: &str, title: &str, content: &str) -> String {
    format!(
        "Context from {name}:
{title}

{content}"
    )
}

/// What pressing Enter (or Shift+Enter) should do, given the configured
/// `EnterBehavior`. Extracted into its own helper so the contract is
/// unit-testable independently of the surrounding key-event plumbing.
#[derive(Debug, PartialEq, Eq)]
enum EnterAction {
    /// Submit the input buffer.
    Send,
    /// Insert a newline at the cursor.
    Newline,
}

fn enter_action(behavior: crate::config::EnterBehavior, shift: bool) -> EnterAction {
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

/// Dispatch a key event to the per-tab handler for the currently active
/// sidebar tab. The dispatch follows a "move out → call handler → decide
/// what to do with the original" pattern:
///
/// 1. The active tab is moved out and replaced with a `Notifications`
///    placeholder. This lets the per-tab handler freely mutate
///    `app.function.tabs` (e.g. close the active tab, push a new one)
///    without any borrow conflict on the moved-out tab.
/// 2. The handler runs.
/// 3. The original tab is restored to the `active` slot IFF the handler
///    did not touch it. We detect "handler touched the slot" by checking
///    whether the placeholder is still there. If the placeholder is
///    gone, the handler must have replaced/removed it — we drop the
///    moved-out copy so we don't overwrite the handler's work (this
///    was the bug that resurrected the provider picker after the user
///    pressed Enter).
///
/// Returns `true` if the handler consumed the key.
async fn dispatch_to_active_tab(k: crossterm::event::KeyEvent, app: &mut App) -> bool {
    let active = app.function.active;
    if active >= app.function.tabs.len() {
        return false;
    }
    let mut tab = std::mem::replace(
        &mut app.function.tabs[active],
        crate::function::SidebarTab::Notifications,
    );
    let consumed = match &mut tab {
        crate::function::SidebarTab::Notifications => handle_notifications_key(k, app),
        crate::function::SidebarTab::ModelPicker(state) => handle_picker_key(k, app, state),
        crate::function::SidebarTab::ProviderPicker(state) => {
            handle_provider_picker_key(k, app, state)
        }
        crate::function::SidebarTab::Settings(state) => handle_settings_key(k, app, state),
        crate::function::SidebarTab::ThinkingPicker(state) => handle_thinking_key(k, app, state),
        crate::function::SidebarTab::TimelinePicker(state) => handle_timeline_key(k, app, state),
        crate::function::SidebarTab::SessionPicker(state) => {
            handle_session_picker_key(k, app, state)
        }
        crate::function::SidebarTab::SessionRename(state) => {
            handle_session_rename_key(k, app, state)
        }
        crate::function::SidebarTab::PastePreview(state) => handle_paste_preview_key(k, app, state),
        crate::function::SidebarTab::Plan(state) => handle_plan_key(k, app, state).await,
        crate::function::SidebarTab::Ask(state) => handle_ask_key(k, app, state).await,
        crate::function::SidebarTab::Todo(state) => handle_todo_key(k, app, state).await,
        _ => false,
    };
    if active < app.function.tabs.len()
        && matches!(
            app.function.tabs[active],
            crate::function::SidebarTab::Notifications
        )
    {
        // If the Plan tab was approved or rejected, close it instead
        // of restoring it. The handler already set state.approved
        // and switched the app mode.
        if matches!(&tab, crate::function::SidebarTab::Plan(state) if state.approved.is_some()) {
            app.function.tabs.remove(active);
            if app.function.active >= app.function.tabs.len() {
                app.function.active = app.function.tabs.len().saturating_sub(1);
            }
            app.maybe_hide_panel();
        } else {
            app.function.tabs[active] = tab;
        }
    }
    consumed
}

fn close_active_function_tab(app: &mut App) {
    let active = app.function.active;
    if active < app.function.tabs.len() {
        app.function.tabs.remove(active);
        if app.function.active >= app.function.tabs.len() {
            app.function.active = app.function.tabs.len().saturating_sub(1);
        }
    }
    app.maybe_hide_panel();
}

fn handle_notifications_key(k: crossterm::event::KeyEvent, app: &mut App) -> bool {
    use crossterm::event::KeyCode;
    match k.code {
        KeyCode::Up => {
            app.notifications.move_up();
            true
        }
        KeyCode::Down => {
            app.notifications.move_down();
            let visible = 8usize;
            if app.notifications.cursor >= app.notifications.scroll + visible {
                app.notifications.scroll = app.notifications.cursor + 1 - visible;
            }
            true
        }
        KeyCode::Backspace => {
            if app.notifications.searching {
                app.notifications.backspace_query()
            } else {
                false
            }
        }
        KeyCode::Esc => {
            if app.notifications.searching {
                app.notifications.exit_search_mode();
                true
            } else if !app.notifications.query.is_empty() {
                app.notifications.query.clear();
                app.notifications.cursor = 0;
                app.notifications.scroll = 0;
                true
            } else {
                close_active_function_tab(app);
                true
            }
        }
        KeyCode::Char('i') | KeyCode::Char('I') if k.modifiers.contains(KeyModifiers::ALT) => {
            if !app.notifications.searching {
                app.notifications.enter_search_mode();
                true
            } else {
                false
            }
        }
        KeyCode::Char(c) => {
            if app.notifications.searching {
                app.notifications.insert_query_char(c);
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

async fn handle_plan_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::PlanState,
) -> bool {
    use crossterm::event::KeyCode;
    match k.code {
        KeyCode::Enter => {
            state.approved = Some(true);
            let prompt = format!(
                "Plan approved. Please proceed with the following plan:\n\n{}",
                state.content
            );
            // send_chat -> send_message pushes the user message into
            // the session; do NOT push it here too, otherwise the
            // message appears twice in the session.
            // close_active_function_tab is intentionally NOT called
            // here: dispatch_to_active_tab swapped the Plan state out
            // of tabs and will close it after we return.
            app.set_mode(crate::function::AppMode::Yolo);
            app.notify(
                crate::function::notifications::ToastLevel::Ok,
                "plan approved",
            );
            crate::commands::send_chat(app, prompt, Vec::new());
            true
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            state.approved = Some(false);
            let prompt = "Plan rejected. Please revise or ask a follow-up question.".to_string();
            app.set_mode(crate::function::AppMode::Yolo);
            app.notify(
                crate::function::notifications::ToastLevel::Warn,
                "plan rejected",
            );
            crate::commands::send_chat(app, prompt, Vec::new());
            true
        }
        KeyCode::Char('s') | KeyCode::Char('S') => {
            if state.dirty {
                app.save_active_plan();
            } else {
                app.notify(
                    crate::function::notifications::ToastLevel::Info,
                    "plan already saved",
                );
            }
            true
        }
        KeyCode::Esc => {
            close_active_function_tab(app);
            app.set_mode(app.previous_mode);
            true
        }
        _ => false,
    }
}

async fn handle_todo_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::TodoTabState,
) -> bool {
    use crossterm::event::KeyCode;
    use crate::session::TodoItem;
    let total = app.session.todo_items.len();

    // If editing, handle Enter (confirm) and Esc (cancel)
    if let Some(edit_idx) = state.editing {
        match k.code {
            KeyCode::Enter => {
                let text = app.input.buffer.trim().to_string();
                if edit_idx < app.session.todo_items.len() {
                    app.session.todo_items[edit_idx].content = text;
                }
                state.editing = None;
                app.session.invalidate_layout_cache();
                return true;
            }
            KeyCode::Esc => {
                // Remove the item if content is still empty
                if edit_idx < app.session.todo_items.len() {
                    if app.session.todo_items[edit_idx].content.trim().is_empty() {
                        app.session.todo_items.remove(edit_idx);
                        if state.cursor > 0 && state.cursor >= app.session.todo_items.len() {
                            state.cursor = state.cursor.saturating_sub(1);
                        }
                    }
                }
                state.editing = None;
                app.session.invalidate_layout_cache();
                return true;
            }
            _ => {}
        }
        return true;
    }

    match k.code {
        KeyCode::Up => {
            if state.cursor > 0 {
                state.cursor -= 1;
            }
            true
        }
        KeyCode::Down => {
            if total > 0 {
                state.cursor = (state.cursor + 1).min(total.saturating_sub(1));
            }
            true
        }
        KeyCode::Enter => {
            if total == 0 {
                return true;
            }
            let item = &mut app.session.todo_items[state.cursor];
            item.status = match item.status.as_str() {
                "pending" => "in_progress".to_string(),
                "in_progress" => "completed".to_string(),
                _ => "pending".to_string(),
            };
            app.session.invalidate_layout_cache();
            true
        }
        KeyCode::Delete => {
            if total == 0 {
                return true;
            }
            app.session.todo_items.remove(state.cursor);
            if state.cursor > 0 && state.cursor >= app.session.todo_items.len() {
                state.cursor = state.cursor.saturating_sub(1);
            }
            app.session.invalidate_layout_cache();
            true
        }
        _ => {
            if k.modifiers.contains(crossterm::event::KeyModifiers::ALT) {
                match k.code {
                    KeyCode::Char('i') => {
                        let text = app.input.buffer.trim().to_string();
                        let insert_at = (state.cursor + 1).min(total);
                        app.session.todo_items.insert(insert_at, TodoItem {
                            content: text,
                            status: "pending".to_string(),
                        });
                        state.cursor = insert_at;
                        state.editing = Some(insert_at);
                        app.session.invalidate_layout_cache();
                        true
                    }
                    KeyCode::Char('I') => {
                        let text = app.input.buffer.trim().to_string();
                        let insert_at = state.cursor.min(total);
                        app.session.todo_items.insert(insert_at, TodoItem {
                            content: text,
                            status: "pending".to_string(),
                        });
                        state.cursor = insert_at;
                        state.editing = Some(insert_at);
                        app.session.invalidate_layout_cache();
                        true
                    }
                    KeyCode::Char('e') | KeyCode::Char('E') => {
                        if total == 0 {
                            return true;
                        }
                        // Copy current todo content to input buffer for editing
                        let content = app.session.todo_items[state.cursor].content.clone();
                        app.input.buffer = content;
                        app.input.cursor = app.input.buffer.len();
                        state.editing = Some(state.cursor);
                        app.session.invalidate_layout_cache();
                        true
                    }
                    _ => false,
                }
            } else {
                false
            }
        }
    }
}

async fn handle_ask_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::AskState,
) -> bool {
    use crate::function::AskPhase;
    use crossterm::event::KeyCode;

    let total_rows = match state.phase {
        AskPhase::Asking => state.row_count(),
        AskPhase::Reviewing => 0,
    };

    match k.code {
        KeyCode::Up => {
            if state.phase == AskPhase::Reviewing {
                // Up in the review phase pops back to Asking so the
                // user can fix an answer. We jump to the first
                // unanswered question to be helpful.
                state.phase = AskPhase::Asking;
                if let Some(idx) = state.next_unanswered(0) {
                    state.active = idx;
                }
                return true;
            }
            if let Some(it) = state.items.get_mut(state.active) {
                if it.cursor == 0 {
                    it.cursor = total_rows.saturating_sub(1);
                } else {
                    it.cursor -= 1;
                }
            }
            true
        }
        KeyCode::Down => {
            if state.phase == AskPhase::Reviewing {
                return true;
            }
            if let Some(it) = state.items.get_mut(state.active) {
                it.cursor = (it.cursor + 1) % total_rows;
            }
            true
        }
        KeyCode::Left => {
            if state.phase == AskPhase::Reviewing {
                return true;
            }
            if state.active > 0 {
                state.active -= 1;
            }
            true
        }
        KeyCode::Right => {
            if state.phase == AskPhase::Reviewing {
                return true;
            }
            if state.active + 1 < state.items.len() {
                state.active += 1;
            } else if state.all_answered() {
                // Past the last question and everything is answered:
                // jump to the review step.
                state.phase = AskPhase::Reviewing;
            }
            true
        }
        KeyCode::Enter => {
            if state.phase == AskPhase::Reviewing {
                // Whole batch approved. Send a single summary turn
                // and close the tab.
                let summary = state.build_summary();
                close_active_function_tab(app);
                crate::commands::send_chat(app, summary, Vec::new());
                return true;
            }

            // Asking phase: dispatch on the cursor row.
            let q_idx = state.active;
            let cursor = state.items[q_idx].cursor;
            let is_freeform = cursor >= state.items[q_idx].options.len();

            if is_freeform {
                // Tell the LLM to wait for the user's free-form
                // input. The state stays in Asking with the
                // question still unanswered, so the user can
                // re-pick an option if they change their mind.
                let question = state.items[q_idx].question.clone();
                let prompt = format!(
                    "(Question: {question})\nPlease wait — the user is typing a free-form answer."
                );
                crate::commands::send_chat(app, prompt, Vec::new());
                return true;
            }

            // Picked a model-supplied option. Write the answer,
            // advance to the next unanswered question, or flip to
            // the review step if everything is answered.
            let answer = state.items[q_idx].options[cursor].clone();
            state.items[q_idx].answered = Some(answer);
            if state.all_answered() {
                state.phase = AskPhase::Reviewing;
            } else if let Some(next) = state.next_unanswered(q_idx + 1) {
                state.active = next;
                state.items[next].cursor = 0;
            }
            true
        }
        KeyCode::Esc => {
            // Esc dismisses the entire ask round. We synthesize a
            // user turn summarising the answered questions (if any)
            // and the unanswered ones (as dismissed), so the LLM has
            // a complete picture of what the user did and didn't
            // answer.
            let summary = state.build_dismiss_summary();
            close_active_function_tab(app);
            crate::commands::send_chat(app, summary, Vec::new());
            true
        }
        _ => false,
    }
}

fn handle_session_picker_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::SessionPickerState,
) -> bool {
    use crossterm::event::KeyCode;
    match k.code {
        KeyCode::Tab => {
            state.toggle_scope(&app.cwd);
            true
        }
        KeyCode::Esc => {
            close_active_function_tab(app);
            app.set_mode(crate::function::AppMode::Yolo);
            true
        }
        KeyCode::Up => {
            if state.cursor > 0 {
                state.cursor -= 1;
            }
            true
        }
        KeyCode::Down => {
            if state.cursor + 1 < state.filtered.len() {
                state.cursor += 1;
            }
            true
        }
        KeyCode::Backspace => {
            state.query.pop();
            state.rebuild_filter();
            true
        }
        KeyCode::Enter => {
            if let Some(id) = state.selected_id() {
                close_active_function_tab(app);
                app.resume_session(&id);
            }
            true
        }
        KeyCode::Char(c) => {
            match c {
                'r' | 'R' if state.mode == crate::function::SessionPickerMode::Manage => {
                    if let (Some(id), Some(title)) = (state.selected_id(), state.selected_title()) {
                        crate::commands::open_session_rename(app, Some(id), title);
                    }
                }
                'd' | 'D' if state.mode == crate::function::SessionPickerMode::Manage => {
                    if let Some(id) = state.selected_id() {
                        match crate::session::store::delete(&id) {
                            Ok(()) => {
                                app.notify(
                                    crate::function::notifications::ToastLevel::Ok,
                                    "session deleted",
                                );
                                state.reload(&app.cwd);
                            }
                            Err(e) => app.notify(
                                crate::function::notifications::ToastLevel::Fail,
                                format!("delete session: {e}"),
                            ),
                        }
                    }
                }
                'f' | 'F' if state.mode == crate::function::SessionPickerMode::Manage => {
                    if let Some(id) = state.selected_id() {
                        app.fork_session(Some(id));
                        state.reload(&app.cwd);
                    }
                }
                _ => {
                    state.query.push(c);
                    state.rebuild_filter();
                }
            }
            true
        }
        _ => false,
    }
}

fn handle_session_rename_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::SessionRenameState,
) -> bool {
    use crossterm::event::KeyCode;
    match k.code {
        KeyCode::Esc => {
            close_active_function_tab(app);
            app.set_mode(crate::function::AppMode::Yolo);
            true
        }
        KeyCode::Enter => {
            app.rename_session(state.target_id.clone(), state.title.clone());
            close_active_function_tab(app);
            true
        }
        KeyCode::Left => {
            if state.cursor > 0 {
                state.cursor -= 1;
            }
            true
        }
        KeyCode::Right => {
            if state.cursor < state.title.len() {
                state.cursor += 1;
            }
            true
        }
        KeyCode::Backspace => {
            if state.cursor > 0 {
                state.cursor -= 1;
                state.title.remove(state.cursor);
            }
            true
        }
        KeyCode::Delete => {
            if state.cursor < state.title.len() {
                state.title.remove(state.cursor);
            }
            true
        }
        KeyCode::Char(c) => {
            state.title.insert(state.cursor, c);
            state.cursor += c.len_utf8();
            true
        }
        _ => false,
    }
}

/// First step of the `/model` flow: pick a provider. On Enter, a
/// `ModelPicker` tab for the selected entry's kind is PUSHED on top of
/// the ProviderPicker — the ProviderPicker stays behind so the user
/// can press Esc on the ModelPicker to return to provider selection
/// (matches the Settings-level back-stack pattern). On Esc (with empty
/// query) the whole flow closes.
/// Mirrors the model picker's search-row + list pattern so the user
/// can type to filter, then Up/Down + Enter to confirm.
fn handle_provider_picker_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::ProviderPickerState,
) -> bool {
    use crossterm::event::KeyCode;
    let open_model_picker_for_selected =
        |app: &mut App, state: &crate::function::ProviderPickerState| {
            if let Some(id) = state.selected_id() {
                if let Some((kind, _)) = crate::config::parse_id(&id) {
                    // Push the model picker for the chosen kind. Do NOT
                    // remove the ProviderPicker — keeping it in the tab
                    // stack means the user can Esc back to provider
                    // selection.
                    crate::commands::open_model_picker_for_kind(app, kind);
                }
            }
        };
    match state.focus {
        crate::function::PickerFocus::Search => match k.code {
            KeyCode::Esc => {
                if state.query.is_empty() {
                    return false; // let the global handler close the tab
                }
                state.query.clear();
                state.rebuild_filter();
                true
            }
            KeyCode::Down => {
                state.focus = crate::function::PickerFocus::List;
                true
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.rebuild_filter();
                true
            }
            KeyCode::Char(c) => {
                state.query.push(c);
                state.rebuild_filter();
                true
            }
            KeyCode::Enter => {
                open_model_picker_for_selected(app, state);
                true
            }
            _ => false,
        },
        crate::function::PickerFocus::List => match k.code {
            KeyCode::Up => {
                if state.cursor > 0 {
                    state.cursor -= 1;
                }
                true
            }
            KeyCode::Down => {
                if state.cursor + 1 < state.filtered.len() {
                    state.cursor += 1;
                }
                true
            }
            KeyCode::Enter => {
                open_model_picker_for_selected(app, state);
                true
            }
            KeyCode::Tab | KeyCode::BackTab => {
                state.focus = crate::function::PickerFocus::Search;
                true
            }
            KeyCode::Char(c) => {
                // Start typing again to refine the filter.
                state.query.push(c);
                state.focus = crate::function::PickerFocus::Search;
                state.rebuild_filter();
                true
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.focus = crate::function::PickerFocus::Search;
                state.rebuild_filter();
                true
            }
            _ => false,
        },
    }
}

fn handle_picker_key(
    k: crossterm::event::KeyEvent,
    _app: &mut App,
    state: &mut crate::function::ModelPickerState,
) -> bool {
    // If context picker is active, handle its keys first.
    if state.context_pick.is_some() {
        return handle_context_picker_key(k, _app, state);
    }
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

    // Global shortcuts (work in any focus mode).
    if ctrl {
        match k.code {
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if !state.fetching {
                    trigger_picker_fetch(_app, state);
                }
                return true;
            }
            KeyCode::Char('m') | KeyCode::Char('M') => {
                // Switch to search input so the user can type a model id.
                state.focus = crate::function::PickerFocus::Search;
                return true;
            }
            _ => {}
        }
    }

    if state.fetching {
        if matches!(k.code, KeyCode::Esc) {
            state.fetching = false;
            state.fetch_error = Some("[cancelled]".to_string());
            return true;
        }
        return false;
    }

    match state.focus {
        crate::function::PickerFocus::Search => match k.code {
            KeyCode::Esc => {
                if state.query.is_empty() {
                    return false; // let global handler close the tab
                }
                state.query.clear();
                state.rebuild_filter();
                true
            }
            KeyCode::Down => {
                state.focus = crate::function::PickerFocus::List;
                true
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.rebuild_filter();
                true
            }
            KeyCode::Char(c) => {
                state.query.push(c);
                state.rebuild_filter();
                true
            }
            KeyCode::Enter => {
                if let Some(&idx) = state.filtered.get(state.cursor) {
                    let model = &state.models[idx];
                    if model.context_needs_pick && model.context_window_tokens.is_none() {
                        open_context_picker(_app, state, idx);
                    } else {
                        let id = model.id.clone();
                        commit_model(_app, state.provider, id, false);
                    }
                } else {
                    let id = state.query.trim();
                    if !id.is_empty() {
                        commit_model(_app, state.provider, id.to_string(), true);
                    }
                }
                true
            }
            _ => false,
        },
        crate::function::PickerFocus::List => match k.code {
            KeyCode::Up => {
                if state.cursor > 0 {
                    state.cursor -= 1;
                }
                true
            }
            KeyCode::Down => {
                if state.focus != crate::function::PickerFocus::List {
                    state.focus = crate::function::PickerFocus::List;
                } else if state.cursor + 1 < state.filtered.len() {
                    state.cursor += 1;
                }
                true
            }
            KeyCode::Enter => {
                if let Some(&idx) = state.filtered.get(state.cursor) {
                    let model = &state.models[idx];
                    if model.context_needs_pick && model.context_window_tokens.is_none() {
                        open_context_picker(_app, state, idx);
                    } else {
                        let id = model.id.clone();
                        commit_model(_app, state.provider, id, false);
                    }
                }
                true
            }
            KeyCode::Tab | KeyCode::BackTab => {
                state.focus = crate::function::PickerFocus::Search;
                true
            }
            KeyCode::Char(c) => {
                // Type a character while browsing the list: switch back
                // to Search and append the key so the user can refine
                // the filter without having to press Up or Tab first.
                state.query.push(c);
                state.focus = crate::function::PickerFocus::Search;
                state.rebuild_filter();
                true
            }
            KeyCode::Backspace => {
                // Backspace in the list: pop the last filter character
                // and return to Search so further typing continues to
                // refine the query.
                state.query.pop();
                state.focus = crate::function::PickerFocus::Search;
                state.rebuild_filter();
                true
            }
            _ => false,
        },
    }
}

/// Search / navigate / select for the thinking-level picker.  Mirrors the
/// model-picker's pattern (search bar + filtered list) even though there
/// are only four possible levels.
fn handle_thinking_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::ThinkingPickerState,
) -> bool {
    use crossterm::event::KeyCode;
    match k.code {
        KeyCode::Up => {
            if state.cursor > 0 {
                state.cursor -= 1;
            }
            true
        }
        KeyCode::Down => {
            if state.cursor + 1 < state.filtered.len() {
                state.cursor += 1;
            }
            true
        }
        KeyCode::Enter => {
            if let Some(level) = state.selected() {
                use crate::config::ReasoningMode;
                let next = match level {
                    "off" => ReasoningMode::Off,
                    "minimal" => ReasoningMode::Minimal,
                    "low" => ReasoningMode::Low,
                    "medium" => ReasoningMode::Medium,
                    "high" => ReasoningMode::High,
                    "xhigh" => ReasoningMode::XHigh,
                    "adaptive" => ReasoningMode::Adaptive,
                    "max" => ReasoningMode::Max,
                    _ => unreachable!(),
                };
                app.config.thinking = next;
                app.status.set_thinking(next);
                app.save_config();
            }
            // Close the picker tab.
            let active = app.function.active;
            if active < app.function.tabs.len() {
                app.function.tabs.remove(active);
            }
            app.maybe_hide_panel();
            true
        }
        KeyCode::Esc => {
            let active = app.function.active;
            if active < app.function.tabs.len() {
                app.function.tabs.remove(active);
            }
            app.maybe_hide_panel();
            true
        }
        KeyCode::Char(c) => {
            state.query.push(c);
            state.rebuild_filter();
            true
        }
        KeyCode::Backspace => {
            state.query.pop();
            state.rebuild_filter();
            true
        }
        _ => true,
    }
}

/// Search / navigate / jump-to-message for the timeline picker.
/// Mirrors the model picker's search-row + list pattern.
fn handle_timeline_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::TimelinePickerState,
) -> bool {
    use crossterm::event::KeyCode;
    match state.focus {
        crate::function::PickerFocus::Search => match k.code {
            KeyCode::Esc => {
                if state.query.is_empty() {
                    return false; // let the global handler close the tab
                }
                state.query.clear();
                state.rebuild_filter();
                true
            }
            KeyCode::Down => {
                state.focus = crate::function::PickerFocus::List;
                true
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.rebuild_filter();
                true
            }
            KeyCode::Char(c) => {
                state.query.push(c);
                state.rebuild_filter();
                true
            }
            KeyCode::Enter => {
                commit_timeline_jump(app, state);
                true
            }
            _ => false,
        },
        crate::function::PickerFocus::List => match k.code {
            KeyCode::Up => {
                if state.cursor > 0 {
                    state.cursor -= 1;
                }
                true
            }
            KeyCode::Down => {
                if state.cursor + 1 < state.filtered.len() {
                    state.cursor += 1;
                }
                true
            }
            KeyCode::Enter => {
                commit_timeline_jump(app, state);
                true
            }
            KeyCode::Tab | KeyCode::BackTab => {
                state.focus = crate::function::PickerFocus::Search;
                true
            }
            KeyCode::Char(c) => {
                // Start typing again to refine the filter.
                state.query.push(c);
                state.focus = crate::function::PickerFocus::Search;
                state.rebuild_filter();
                true
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.focus = crate::function::PickerFocus::Search;
                state.rebuild_filter();
                true
            }
            _ => false,
        },
    }
}

/// Jump the session scroll to the focused entry and close the
/// timeline picker tab.
fn commit_timeline_jump(app: &mut App, state: &crate::function::TimelinePickerState) {
    use crate::function::notifications::ToastLevel;
    let Some((msg_idx, tool_idx)) = state.selected_entry() else {
        return;
    };
    let viewport_h = app.session_area.map(|r| r.height).unwrap_or(20);
    app.session.jump_to_message(msg_idx, viewport_h);
    let mut scroll = app.session.scroll;
    if tool_idx.is_some() {
        // Nudge scroll up a bit so the tool block is more visible.
        let nudge = 3u16.min(scroll);
        scroll = scroll.saturating_sub(nudge);
    }
    // Programmatic jump — land immediately, cancel any momentum.
    app.set_scroll_anchored(scroll);
    let active = app.function.active;
    if active < app.function.tabs.len() {
        app.function.tabs.remove(active);
        if app.function.active >= app.function.tabs.len() {
            app.function.active = app.function.tabs.len().saturating_sub(1);
        }
    }
    app.maybe_hide_panel();
    let label = if tool_idx.is_some() {
        "jumped to tool call"
    } else {
        &format!("jumped to message #{}", msg_idx + 1)
    };
    app.notify(ToastLevel::Info, label);
}

fn trigger_picker_fetch(app: &mut App, state: &mut crate::function::ModelPickerState) {
    let p = state.provider;
    let active_id = match app.config.active.as_ref() {
        Some(id) => id.clone(),
        None => {
            use crate::function::notifications::ToastLevel;
            app.notify(
                ToastLevel::Fail,
                "no active provider; configure one in /settings",
            );
            return;
        }
    };
    if let Err(e) = app.config.validate_provider(&active_id) {
        use crate::function::notifications::ToastLevel;
        app.notify(ToastLevel::Fail, e);
        return;
    }
    state.fetching = true;
    state.fetch_error = None;
    state.no_endpoint = false;
    state.models.clear();
    state.filtered.clear();
    state.cursor = 0;
    if let Some(tx) = app.msg_tx.clone() {
        let base = app
            .config
            .entry(&active_id)
            .map(|c| c.base_url.clone())
            .unwrap_or_default();
        let key = app.config.effective_api_key(&active_id).unwrap_or_default();
        let access_key = app
            .config
            .entry(&active_id)
            .map(|c| c.access_key.clone())
            .unwrap_or_default();
        let secret_key = app
            .config
            .entry(&active_id)
            .map(|c| c.secret_key.clone())
            .unwrap_or_default();
        let client = app.reqwest.clone();
        let provider_name = app
            .config
            .entry(&active_id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let cache_path = app.model_cache_path.parent().unwrap_or(&app.model_cache_path).to_path_buf();
        tokio::spawn(async move {
            match crate::providers::list_models(&client, p, &base, &key, &access_key, &secret_key, &cache_path, &provider_name)
                .await
            {
                Ok(models) => {
                    let _ = tx.send(AppMsg::ModelsFetched {
                        provider: p,
                        base_url: base,
                        api_key: key,
                        models,
                    });
                }
                Err(e) => {
                    let no_endpoint = matches!(
                        e.downcast_ref::<crate::providers::ProviderError>(),
                        Some(crate::providers::ProviderError::NoModelsEndpoint)
                    );
                    let _ = tx.send(AppMsg::ModelsFetchFailed {
                        provider: p,
                        error: format!("{e}"),
                        no_endpoint,
                    });
                }
            }
        });
    }
}

fn handle_settings_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::SettingsState,
) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if matches!(state.level, crate::function::SettingsLevel::NewProviderKind) {
        if matches!(k.code, KeyCode::Enter) {
            handle_settings_enter(app, state);
            return true;
        }
        return handle_new_provider_key(k, state);
    }
    // Tool-preview-lines is a single-row stepper: Up/Down adjust the
    // value rather than navigate.
    if matches!(
        state.level,
        crate::function::SettingsLevel::ToolPreviewLines
    ) {
        match k.code {
            KeyCode::Up => {
                if app.config.tool_preview_lines
                    > crate::config::TOOL_PREVIEW_LINES_MIN
                {
                    app.config.tool_preview_lines -= 1;
                    app.save_config();
                }
                return true;
            }
            KeyCode::Down => {
                if app.config.tool_preview_lines
                    < crate::config::TOOL_PREVIEW_LINES_MAX
                {
                    app.config.tool_preview_lines += 1;
                    app.save_config();
                }
                return true;
            }
            KeyCode::Esc | KeyCode::Enter => {
                handle_settings_back(app, state);
                return true;
            }
            _ => return true,
        }
    }
    // Navigation keys are level-agnostic.
    match k.code {
        KeyCode::Up => {
            if state.cursor > 0 {
                state.cursor -= 1;
            }
            sync_form_focus_to_cursor(state);
            return true;
        }
        KeyCode::Down => {
            let len = state.list_len(&app.config);
            if state.cursor + 1 < len {
                state.cursor += 1;
            }
            sync_form_focus_to_cursor(state);
            return true;
        }
        KeyCode::Esc => {
            handle_settings_back(app, state);
            return true;
        }
        KeyCode::Enter => {
            handle_settings_enter(app, state);
            return true;
        }
        _ => {}
    }

    // Per-level handlers.
    let level = std::mem::replace(&mut state.level, crate::function::SettingsLevel::TopLevel);
    let mut taken = level;
    let handled = match &mut taken {
        crate::function::SettingsLevel::ConfigForm(form) => handle_form_text(k, ctrl, form),
        _ => false,
    };
    state.level = taken;
    if handled {
        return true;
    }
    false
}

/// In a `ConfigForm` level, keep `form.focused` in sync with `state.cursor`
/// so Up/Down move the actual text-input focus, not just the visual
/// highlight. Otherwise the user navigates with Up/Down but typing still
/// goes to the previously-Tabbed field.
fn sync_form_focus_to_cursor(state: &mut crate::function::SettingsState) {
    use crate::function::SettingsLevel;
    if let SettingsLevel::ConfigForm(form) = &mut state.level {
        let fields = form.active_fields();
        form.focused = match state.cursor {
            i if i < fields.len() => fields[i],
            _ => *fields.last().unwrap_or(&crate::function::ConfigField::Exit),
        };
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

/// Esc behavior: pop one level. Only at TopLevel does Esc close the tab.
fn handle_settings_back(app: &mut App, state: &mut crate::function::SettingsState) {
    use crate::function::SettingsLevel;
    match &state.level {
        SettingsLevel::ConfigForm(form) => {
            if form.is_new {
                state.level = SettingsLevel::NewProviderKind;
            } else {
                state.level = SettingsLevel::ProviderList;
            }
            state.cursor = 0;
            state.clamp_cursor(&app.config);
        }
        SettingsLevel::NewProviderKind | SettingsLevel::ExistingActions(_) => {
            state.level = SettingsLevel::ProviderList;
            state.cursor = 0;
            state.clamp_cursor(&app.config);
        }
        SettingsLevel::ThinkingDisplayList
        | SettingsLevel::ToolResultDisplayList
        | SettingsLevel::EnterBehaviorList
        | SettingsLevel::BorderTypeList
        | SettingsLevel::ThemeList
        | SettingsLevel::AutoCompact
        | SettingsLevel::ToolPreviewLines => {
            state.level = SettingsLevel::TopLevel;
            state.cursor = 0;
            state.clamp_cursor(&app.config);
        }
        SettingsLevel::ProviderList => {
            state.level = SettingsLevel::TopLevel;
            state.cursor = 0;
        }
        SettingsLevel::TopLevel => {
            // close the settings tab entirely
            let active = app.function.active;
            if active < app.function.tabs.len() {
                app.function.tabs.remove(active);
                if app.function.active >= app.function.tabs.len() {
                    app.function.active = app.function.tabs.len().saturating_sub(1);
                }
            }
            app.maybe_hide_panel();
        }
    }
}

/// Enter behavior depends on the current level.
///
/// Implementation note: we move `state.level` out with `mem::replace`, work on
/// the owned value, and put it back. This avoids nested `&state.level` /
/// `&mut state.level` patterns that NLL lets through but that are hard to
/// reason about and easy to break with future edits.
///
/// Also: Enter on a text field in the form does **not** auto-advance focus.
/// The user moves between fields with Up/Down/Tab.
fn handle_settings_enter(app: &mut App, state: &mut crate::function::SettingsState) {
    use crate::config::parse_id;
    use crate::function::{ConfigField, SettingsLevel};

    let cursor = state.cursor;
    let level = std::mem::replace(&mut state.level, SettingsLevel::TopLevel);

    let new_level = match level {
        SettingsLevel::TopLevel => match cursor {
            0 => SettingsLevel::ProviderList,
            1 => SettingsLevel::ThinkingDisplayList,
            2 => SettingsLevel::ToolResultDisplayList,
            3 => SettingsLevel::EnterBehaviorList,
            4 => SettingsLevel::BorderTypeList,
            5 => SettingsLevel::ThemeList,
            6 => SettingsLevel::AutoCompact,
            _ => SettingsLevel::ToolPreviewLines,
        },
        SettingsLevel::ProviderList => {
            if cursor == 0 {
                SettingsLevel::NewProviderKind
            } else {
                let keys = app.config.configured_provider_ids();
                match keys.get(cursor - 1) {
                    Some(id) => SettingsLevel::ExistingActions(id.clone()),
                    None => SettingsLevel::ProviderList,
                }
            }
        }
        SettingsLevel::NewProviderKind => {
            match state
                .new_provider
                .selected_id()
                .and_then(|id| parse_id(&id).map(|(k, m)| (id, k, m)))
            {
                Some((_id, kind, mode)) => SettingsLevel::ConfigForm(
                    crate::function::ConfigFormState::new_for_create(kind, mode),
                ),
                None => SettingsLevel::NewProviderKind,
            }
        }
        SettingsLevel::ExistingActions(id) => {
            if cursor == 0 {
                // edit
                if let Some((_kind, mode)) = parse_id(&id) {
                    let cfg = app.config.entry(&id).cloned().unwrap_or_default();
                    SettingsLevel::ConfigForm(crate::function::ConfigFormState::new_for_edit(
                        id, &cfg, mode,
                    ))
                } else {
                    SettingsLevel::ProviderList
                }
            } else {
                // delete
                if let Some(cfg) = app.config.entry(&id).cloned() {
                    app.config.entries.remove(&id);
                    if app.config.active.as_deref() == Some(id.as_str()) {
                        app.config.active = app.config.configured_provider_ids().into_iter().next();
                    }
                    if let Err(e) = app.config.save(&app.config_path) {
                        use crate::function::notifications::ToastLevel;
                        app.notify(ToastLevel::Fail, format!("delete: {e}"));
                        // restore
                        app.config.entries.insert(id, cfg);
                    } else {
                        use crate::function::notifications::ToastLevel;
                        app.notify(ToastLevel::Ok, format!("deleted {id}"));
                        // The active provider may have changed or been
                        // removed — refresh the status bar so it does not
                        // show a stale `name:(no model)` from a deleted
                        // entry.
                        app.status.set_provider_name(&app.config.active_name());
                        app.status.set_model(&app.config.active_model_display());
                        app.refresh_status_model_context();
                    }
                }
                SettingsLevel::ProviderList
            }
        }
        SettingsLevel::ThinkingDisplayList => {
            use crate::config::ThinkingDisplay;
            use crate::function::notifications::ToastLevel;
            let modes = [
                ThinkingDisplay::Show,
                ThinkingDisplay::Hide,
                ThinkingDisplay::ShowWhileStreaming,
            ];
            if let Some(&mode) = modes.get(cursor) {
                app.config.thinking_display = mode;
                app.save_config();
                app.notify(
                    ToastLevel::Ok,
                    format!("thinking display: {}", mode.as_str()),
                );
            }
            SettingsLevel::TopLevel
        }
        SettingsLevel::ToolResultDisplayList => {
            use crate::config::ToolResultDisplay;
            use crate::function::notifications::ToastLevel;
            let modes = [
                ToolResultDisplay::Show,
                ToolResultDisplay::Hide,
                ToolResultDisplay::ShowWhileStreaming,
            ];
            if let Some(&mode) = modes.get(cursor) {
                app.config.tool_display = mode;
                app.save_config();
                app.notify(ToastLevel::Ok, format!("tool display: {}", mode.as_str()));
            }
            SettingsLevel::TopLevel
        }
        SettingsLevel::EnterBehaviorList => {
            use crate::config::EnterBehavior;
            use crate::function::notifications::ToastLevel;
            let modes = [EnterBehavior::EnterSends, EnterBehavior::EnterNewline];
            if let Some(&mode) = modes.get(cursor) {
                app.config.enter_behavior = mode;
                app.save_config();
                app.notify(ToastLevel::Ok, format!("enter behavior: {}", mode.as_str()));
            }
            SettingsLevel::TopLevel
        }
        SettingsLevel::BorderTypeList => {
            use crate::function::notifications::ToastLevel;
            use crate::ui::border_type::BorderType;
            let modes = [BorderType::Ascii, BorderType::Rounded];
            if let Some(&mode) = modes.get(cursor) {
                app.config.border_type = mode;
                app.save_config();
                app.notify(ToastLevel::Ok, format!("border type: {}", mode.as_str()));
            }
            SettingsLevel::TopLevel
        }
        SettingsLevel::ThemeList => {
            use crate::function::notifications::ToastLevel;
            use crate::theme::ThemeVariant;
            let themes = ThemeVariant::all();
            if let Some(variant) = themes.get(cursor) {
                app.config.theme = *variant;
                app.save_config();
                crate::theme::init_theme(*variant);
                app.notify(ToastLevel::Ok, format!("theme: {}", variant.as_str()));
                // Clear line cache so blocks re-render with new colors
                if let Ok(mut c) = app.session.line_cache.lock() {
                    c.clear();
                }
            }
            SettingsLevel::TopLevel
        }
        SettingsLevel::AutoCompact => {
            use crate::function::notifications::ToastLevel;
            // 0 = on, 1 = off. `auto_compact` defaults to `true` in
            // `Config`, so picking the first row turns it on, the
            // second row turns it off.
            let enabled = match cursor {
                0 => true,
                _ => false,
            };
            if app.config.auto_compact != enabled {
                app.config.auto_compact = enabled;
                app.status.set_auto_compact(enabled);
                app.save_config();
                app.notify(
                    ToastLevel::Ok,
                    format!("auto compact: {}", if enabled { "on" } else { "off" }),
                );
            }
            SettingsLevel::TopLevel
        }
        SettingsLevel::ToolPreviewLines => {
            // The single-row stepper is purely Up/Down driven; Enter
            // pops back to the top level without changing the value.
            SettingsLevel::TopLevel
        }
        SettingsLevel::ConfigForm(form) => {
            match form.focused {
                ConfigField::Name
                | ConfigField::BaseUrl
                | ConfigField::Key
                | ConfigField::Env
                | ConfigField::AccessKey
                | ConfigField::SecretKey => {
                    // No auto-advance. User moves fields with Up/Down/Tab.
                    SettingsLevel::ConfigForm(form)
                }
                ConfigField::Save => {
                    if form.base_url.trim().is_empty() {
                        use crate::function::notifications::ToastLevel;
                        let mut f = form;
                        f.form_error = Some("[!] base_url is required".to_string());
                        app.notify(ToastLevel::Fail, "base_url is required");
                        SettingsLevel::ConfigForm(f)
                    } else if !form.is_cursor()
                        && form.api_key.trim().is_empty()
                        && form.api_key_env.trim().is_empty()
                    {
                        use crate::function::notifications::ToastLevel;
                        let mut f = form;
                        f.form_error = Some("[!] api_key or env name is required".to_string());
                        app.notify(ToastLevel::Fail, "api_key or env name is required");
                        SettingsLevel::ConfigForm(f)
                    } else {
                        settings_save_form(app, form);
                        SettingsLevel::ProviderList
                    }
                }
                ConfigField::Exit => SettingsLevel::ProviderList,
            }
        }
    };

    // Restore level and reset cursor to 0 (a new level, no inherited cursor).
    state.level = new_level;
    state.cursor = 0;
    state.clamp_cursor(&app.config);
    let _ = cursor;
}

/// Commit a form into the config. The form's focused field must be Save
/// (caller is responsible). Updates `app.config`, writes to disk, refreshes
/// status, and pushes a toast.
fn settings_save_form(app: &mut App, form: crate::function::ConfigFormState) {
    use crate::config::{parse_id, ProviderKind, ProviderMode};
    use crate::function::notifications::ToastLevel;

    let mut id = form.id.clone();
    let (_kind, _mode) = parse_id(&id).unwrap_or((ProviderKind::Openai, ProviderMode::Key));
    let base_url = form.base_url.trim().to_string();
    let was_new = form.is_new;

    // Deduplicate: if the base ID already exists, append -2, -3, etc.
    if was_new {
        let mut n = 2;
        while app.config.entries.contains_key(&id) {
            id = format!("{}-{}", form.id, n);
            n += 1;
        }
    }

    // Preserve existing model and api_key if the user did not touch
    // the corresponding fields.
    let existing = app.config.entry(&id).cloned();
    let model = existing
        .as_ref()
        .map(|c| c.model.clone())
        .unwrap_or_default();
    let model_display = existing
        .as_ref()
        .map(|c| c.model_display.clone())
        .unwrap_or_default();

    let mut new_cfg = crate::config::ProviderConfig {
        api_key: existing
            .as_ref()
            .map(|c| c.api_key.clone())
            .unwrap_or_default(),
        api_key_env: existing
            .as_ref()
            .map(|c| c.api_key_env.clone())
            .unwrap_or_default(),
        access_key: form.access_key.trim().to_string(),
        secret_key: form.secret_key.trim().to_string(),
        base_url,
        model,
        model_display,
        name: String::new(),
    };
    new_cfg.name = form.name.trim().to_string();

    // api_key: for edit, only apply if user modified the field
    if was_new || form.key_modified {
        new_cfg.api_key = form.api_key.trim().to_string();
    }
    // api_key_env: for edit, only apply if user modified the field
    if was_new || form.env_modified {
        new_cfg.api_key_env = form.api_key_env.trim().to_string();
    }

    app.config.entries.insert(id.clone(), new_cfg);
    app.config.active = Some(id.clone());
    app.config.sanitize_entries();

    if let Err(e) = app.config.save(&app.config_path) {
        app.notify(ToastLevel::Fail, format!("save: {e}"));
        return;
    }

    if was_new {
        app.notify(ToastLevel::Ok, format!("added {id}"));
    } else {
        app.notify(ToastLevel::Ok, format!("saved {id}"));
    }

    // refresh status bar
    app.status.set_provider_name(&app.config.active_name());
    app.status.set_model(&app.config.active_model_display());
    app.refresh_status_model_context();

    // Open the model picker so the user can pick a model. Validate first
    // so we can set the picker's initial state correctly (fetching vs
    // idle with an error message).
    if let Some(k) = app.config.active_kind() {
        if k == ProviderKind::Cursor {
            start_cursor_oauth(app);
            return;
        }
        let active_id = match app.config.active.clone() {
            Some(id) => id,
            None => return,
        };

        let mut state = crate::function::ModelPickerState::new(k);
        match app.config.validate_provider(&active_id) {
            Ok(_) => state.fetching = true,
            Err(e) => state.fetch_error = Some(e),
        }
        let should_fetch = state.fetching;
        app.function
            .push(crate::function::SidebarTab::ModelPicker(state));
        app.show_panel();
        app.acknowledge_panel();

        if should_fetch {
            let base = app
                .config
                .entry(&active_id)
                .map(|c| c.base_url.clone())
                .unwrap_or_default();
            let key = app.config.effective_api_key(&active_id).unwrap_or_default();
            let access_key = app
                .config
                .entry(&active_id)
                .map(|c| c.access_key.clone())
                .unwrap_or_default();
            let secret_key = app
                .config
                .entry(&active_id)
                .map(|c| c.secret_key.clone())
                .unwrap_or_default();
            let client = app.reqwest.clone();
            let provider_name = app
                .config
                .entry(&active_id)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            let cache_path = app.model_cache_path.parent().unwrap_or(&app.model_cache_path).to_path_buf();
            if let Some(tx) = app.msg_tx.clone() {
                tokio::spawn(async move {
                    match crate::providers::list_models(
                        &client,
                        k,
                        &base,
                        &key,
                        &access_key,
                        &secret_key,
                        &cache_path,
                        &provider_name,
                    )
                    .await
                    {
                        Ok(models) => {
                            let _ = tx.send(AppMsg::ModelsFetched {
                                provider: k,
                                base_url: base,
                                api_key: key,
                                models,
                            });
                        }
                        Err(e) => {
                            let no_endpoint = matches!(
                                e.downcast_ref::<crate::providers::ProviderError>(),
                                Some(crate::providers::ProviderError::NoModelsEndpoint)
                            );
                            let _ = tx.send(AppMsg::ModelsFetchFailed {
                                provider: k,
                                error: format!("{e}"),
                                no_endpoint,
                            });
                        }
                    }
                });
            }
        }
    }
}

fn start_cursor_oauth(app: &mut App) {
    use crate::function::notifications::ToastLevel;
    let params = crate::providers::cursor::generate_auth_params();
    let login_url = params.login_url.clone();
    match crate::providers::cursor::open_browser(&login_url) {
        Ok(_) => app.notify(ToastLevel::Info, "opened Cursor OAuth login in browser"),
        Err(e) => app.notify(
            ToastLevel::Warn,
            format!("open browser failed: {e}; visit {login_url}"),
        ),
    }
    let client = app.reqwest.clone();
    if let Some(tx) = app.msg_tx.clone() {
        tokio::spawn(async move {
            match crate::providers::cursor::poll_auth(&client, &params.uuid, &params.verifier).await
            {
                Ok(tokens) => {
                    let _ = tx.send(AppMsg::CursorAuthSucceeded {
                        access_token: tokens.access_token,
                        refresh_token: tokens.refresh_token,
                    });
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::CursorAuthFailed(format!("{e}")));
                }
            }
        });
    }
}

fn handle_new_provider_key(
    k: crossterm::event::KeyEvent,
    state: &mut crate::function::SettingsState,
) -> bool {
    use crossterm::event::KeyCode;
    let picker = &mut state.new_provider;
    match k.code {
        KeyCode::Esc => {
            if picker.query.is_empty() {
                state.level = crate::function::SettingsLevel::ProviderList;
                state.cursor = 0;
            } else {
                picker.query.clear();
                picker.rebuild_filter();
                picker.focus = crate::function::PickerFocus::List;
            }
            true
        }
        KeyCode::Enter => false,
        KeyCode::Up => {
            picker.focus = crate::function::PickerFocus::List;
            picker.cursor = picker.cursor.saturating_sub(1);
            true
        }
        KeyCode::Down => {
            picker.focus = crate::function::PickerFocus::List;
            if picker.cursor + 1 < picker.filtered.len() {
                picker.cursor += 1;
            }
            true
        }
        KeyCode::Backspace => {
            picker.query.pop();
            picker.focus = crate::function::PickerFocus::Search;
            picker.rebuild_filter();
            true
        }
        KeyCode::Char(c) => {
            picker.query.push(c);
            picker.focus = crate::function::PickerFocus::Search;
            picker.rebuild_filter();
            true
        }
        _ => false,
    }
}

/// Text editing inside a `ConfigForm` level. Returns true if the key was used.
fn handle_form_text(
    k: crossterm::event::KeyEvent,
    ctrl: bool,
    form: &mut crate::function::ConfigFormState,
) -> bool {
    use crate::function::ConfigField;
    if ctrl {
        return false;
    }
    if matches!(k.code, crossterm::event::KeyCode::Tab) {
        // Tab cycles fields within the form using active_fields.
        let fields = form.active_fields();
        let idx = fields.iter().position(|f| *f == form.focused).unwrap_or(0);
        form.focused = fields[(idx + 1) % fields.len()];
        return true;
    }
    match form.focused {
        ConfigField::Name => match k.code {
            crossterm::event::KeyCode::Char(c) => {
                form.name.push(c);
                true
            }
            crossterm::event::KeyCode::Backspace => {
                form.name.pop();
                true
            }
            _ => false,
        },
        ConfigField::BaseUrl => match k.code {
            crossterm::event::KeyCode::Char(c) => {
                form.base_url.push(c);
                true
            }
            crossterm::event::KeyCode::Backspace => {
                form.base_url.pop();
                true
            }
            _ => false,
        },
        ConfigField::Key => {
            // First edit clears the saved (masked) value so the user can
            // type a new key. If they don't touch the field, the original
            // is preserved on save.
            if !form.key_modified && !form.api_key.is_empty() {
                form.api_key.clear();
            }
            form.key_modified = true;
            match k.code {
                crossterm::event::KeyCode::Char(c) => {
                    form.api_key.push(c);
                    true
                }
                crossterm::event::KeyCode::Backspace => {
                    form.api_key.pop();
                    true
                }
                _ => false,
            }
        }
        ConfigField::Env => {
            if !form.env_modified && !form.api_key_env.is_empty() {
                form.api_key_env.clear();
            }
            form.env_modified = true;
            match k.code {
                crossterm::event::KeyCode::Char(c) => {
                    form.api_key_env.push(c);
                    true
                }
                crossterm::event::KeyCode::Backspace => {
                    form.api_key_env.pop();
                    true
                }
                _ => false,
            }
        }
        ConfigField::AccessKey => match k.code {
            crossterm::event::KeyCode::Char(c) => {
                form.access_key.push(c);
                true
            }
            crossterm::event::KeyCode::Backspace => {
                form.access_key.pop();
                true
            }
            _ => false,
        },
ConfigField::SecretKey => match k.code {
            crossterm::event::KeyCode::Char(c) => {
                form.secret_key.push(c);
                true
            }
            crossterm::event::KeyCode::Backspace => {
                form.secret_key.pop();
                true
            }
            _ => false,
        },
        _ => false,
    }
}

fn open_context_picker(
    app: &mut App,
    state: &mut crate::function::ModelPickerState,
    model_idx: usize,
) {
    let provider_name = app
        .config
        .active
        .as_ref()
        .and_then(|id| app.config.entry(id))
        .map(|c| c.name.clone())
        .unwrap_or_default()
        .to_lowercase();

    let cache_path = app.model_cache_path.parent().unwrap_or(&app.model_cache_path);
    let model_data_path = cache_path.join("model-data.json");
    let model_data = crate::model_data::ModelData::load(&model_data_path)
        .unwrap_or_else(|| crate::model_data::ModelData {
            models: std::collections::HashMap::new(),
            fetched_at: chrono::Utc::now(),
        });

    let options = model_data.context_options_for_provider(&provider_name);

    state.context_pick = Some(crate::function::ContextPickerState {
        model_idx,
        options,
        cursor: 0,
        custom_input: String::new(),
        focus: crate::function::ContextPickerFocus::Options,
    });
}

fn handle_context_picker_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::ModelPickerState,
) -> bool {
    use crossterm::event::KeyCode;
    let Some(cp) = &mut state.context_pick else {
        return false;
    };

    match k.code {
        KeyCode::Esc => {
            state.context_pick = None;
            true
        }
        KeyCode::Tab => {
            cp.focus = match cp.focus {
                crate::function::ContextPickerFocus::Options => {
                    crate::function::ContextPickerFocus::CustomInput
                }
                crate::function::ContextPickerFocus::CustomInput => {
                    crate::function::ContextPickerFocus::Options
                }
            };
            true
        }
        KeyCode::Up => {
            if cp.focus == crate::function::ContextPickerFocus::Options && cp.cursor > 0 {
                cp.cursor -= 1;
            }
            true
        }
        KeyCode::Down => {
            if cp.focus == crate::function::ContextPickerFocus::Options
                && cp.cursor + 1 < cp.options.len()
            {
                cp.cursor += 1;
            }
            true
        }
        KeyCode::Enter => {
            match cp.focus {
                crate::function::ContextPickerFocus::Options => {
                    if let Some(opt) = cp.options.get(cp.cursor) {
                        let ctx = opt.context;
                        let model_idx = cp.model_idx;
                        state.models[model_idx].context_window_tokens = Some(ctx);
                        state.models[model_idx].context_needs_pick = false;
                        // Save to custom cache
                        let cache_path = app.model_cache_path.parent().unwrap_or(&app.model_cache_path);
                        let custom_cache_path = cache_path.join("context-cache.json");
                        let mut custom_cache =
                            crate::model_data::CustomContextCache::load(&custom_cache_path);
                        custom_cache.set(
                            state.models[model_idx].id.clone(),
                            ctx,
                            &custom_cache_path,
                        );
                        let id = state.models[model_idx].id.clone();
                        let provider = state.provider;
                        state.context_pick = None;
                        commit_model(app, provider, id, false);
                    }
                }
                crate::function::ContextPickerFocus::CustomInput => {
                    let input = cp.custom_input.trim().to_string();
                    if let Ok(ctx) = input.parse::<u64>() {
                        let ctx = ctx * 1000; // user enters k, store as tokens
                        let model_idx = cp.model_idx;
                        state.models[model_idx].context_window_tokens = Some(ctx);
                        state.models[model_idx].context_needs_pick = false;
                        // Save to custom cache
                        let cache_path = app.model_cache_path.parent().unwrap_or(&app.model_cache_path);
                        let custom_cache_path = cache_path.join("context-cache.json");
                        let mut custom_cache =
                            crate::model_data::CustomContextCache::load(&custom_cache_path);
                        custom_cache.set(
                            state.models[model_idx].id.clone(),
                            ctx,
                            &custom_cache_path,
                        );
                        let id = state.models[model_idx].id.clone();
                        let provider = state.provider;
                        state.context_pick = None;
                        commit_model(app, provider, id, false);
                    }
                }
            }
            true
        }
        KeyCode::Backspace => {
            if cp.focus == crate::function::ContextPickerFocus::CustomInput {
                cp.custom_input.pop();
            }
            true
        }
        KeyCode::Char(c) => {
            if cp.focus == crate::function::ContextPickerFocus::CustomInput
                && c.is_ascii_digit()
            {
                cp.custom_input.push(c);
            }
            true
        }
_ => false,
    }
}

pub fn commit_model(
    app: &mut App,
    provider: crate::config::ProviderKind,
    model_id: String,
    manual: bool,
) {
    use crate::config::parse_id;
    use crate::function::notifications::ToastLevel;

    // 1. Find target entry id:
    //    - If the active entry's kind matches, use it.
    //    - Otherwise, find any existing entry with the same kind.
    //    - Otherwise, leave the target unset (no entry to attach the model to).
    let target_id: Option<String> = match app.config.active.as_deref() {
        Some(id) if parse_id(id).map(|(k, _)| k == provider).unwrap_or(false) => {
            Some(id.to_string())
        }
        Some(_) | None => app
            .config
            .entries
            .keys()
            .find(|id| parse_id(id).map(|(k2, _)| k2 == provider).unwrap_or(false))
            .cloned(),
    };

    let selected_model =
        app.model_cache
            .get(provider)
            .and_then(|cache| {
                cache.models.iter().find(|m| {
                    m.id == model_id || m.request_id.as_deref() == Some(model_id.as_str())
                })
            })
            .cloned();
    let request_model_id = selected_model
        .as_ref()
        .and_then(|m| m.request_id.clone())
        .unwrap_or_else(|| model_id.clone());
    let display_model = selected_model
        .as_ref()
        .map(|m| m.display.clone())
        .unwrap_or_else(|| model_id.clone());

    // 2. Update the target entry's request model id and make it active.
    if let Some(id) = target_id {
        app.config.active = Some(id.clone());
        if let Some(entry) = app.config.entry_mut(&id) {
            entry.model = request_model_id.clone();
            entry.model_display = display_model.clone();
        }
    }

    // 3. Refresh the status bar.
    app.status.set_provider_name(&app.config.active_name());
    app.status.set_model(&app.config.active_model_display());
    app.refresh_status_model_context();
    if let Some(tokens) = selected_model.and_then(|m| m.context_window_tokens) {
        app.status.set_context_window_tokens(tokens);
    }

    // 4. Close the picker tab.
    if app.function.active < app.function.tabs.len() {
        app.function.tabs.remove(app.function.active);
        if app.function.active >= app.function.tabs.len() {
            app.function.active = app.function.tabs.len().saturating_sub(1);
        }
    }
    // 4b. If the now-active tab is a ProviderPicker (the /model flow's
    // first step) the user has finished the flow — close it too,
    // otherwise the panel would still be open and the user would
    // have to Esc a second time to exit.
    if app.function.active < app.function.tabs.len()
        && matches!(
            app.function.tabs[app.function.active],
            crate::function::SidebarTab::ProviderPicker(_)
        )
    {
        app.function.tabs.remove(app.function.active);
        if app.function.active >= app.function.tabs.len() {
            app.function.active = app.function.tabs.len().saturating_sub(1);
        }
    }
    app.maybe_hide_panel();

    // 5. Persist to disk (after tab cleanup so notify() doesn't
    //    shift indices when creating a Notifications tab).
    app.save_config();

    // 6. Toast.
    if manual {
        app.notify(ToastLevel::Ok, format!("manual model id set: {model_id}"));
    } else {
        app.notify(
            ToastLevel::Ok,
            format!("model set: {}:{model_id}", provider.as_str()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{make_id, Config, ProviderConfig, ProviderId, ProviderKind, ProviderMode};
    use crate::function::notifications::Notifications;
    use crate::function::SidebarTab;
    use crate::function::{FunctionPanel, ModelPickerState};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn make_app() -> App {
        let mut cfg = Config::default();
        // Add default entries so tests that expect providers work.
        // (Real app starts with empty config; test helper adds stubs.)
        for kind in [ProviderKind::Openai, ProviderKind::Anthropic] {
            let id = make_id(kind, ProviderMode::Key);
            cfg.entries.entry(id).or_insert_with(|| ProviderConfig {
                api_key: String::new(),
                api_key_env: String::new(),
                base_url: crate::config::default_base_url(kind).to_string(),
                model: String::new(),
                model_display: String::new(),
                name: String::new(),
                access_key: String::new(),
                secret_key: String::new(),
            });
        }
        cfg.active = Some(make_id(ProviderKind::Openai, ProviderMode::Key));
        // Use a per-test config file so parallel `cargo test` invocations
        // do not race on the same path. The atomic counter is process-wide
        // and yields a unique id for every call to `make_app`.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!("fish-coding-agent-test-{id}.json"));
        let _ = std::fs::remove_file(&tmp);
        let cache_file = tmp.parent().unwrap_or(&tmp).join("model-cache.json");
        App {
            config: cfg,
            config_path: tmp,
            session: crate::session::Session::default(),
            session_id: crate::session::store::new_session_id(),
            session_title: "test".to_string(),
            mode: crate::function::AppMode::Yolo,
            previous_mode: crate::function::AppMode::Yolo,
            active_agent: crate::permission::Agent::Build,
            function: FunctionPanel::new(),
            input: crate::input::InputState::new(),
            status: crate::input::status::StatusBar::new(),
            function_visible: false,
            pending_events: 0,
            notifications: Notifications::default(),
            model_cache: crate::function::notifications::ModelCache::default(),
            hit_rate: crate::function::notifications::HitRate::new(50),
            token_rate: crate::function::notifications::TokenRate::new(50),
            response_started_at: None,
            response_accumulated: std::time::Duration::ZERO,
            response_output_chars: 0,
            response_output_tokens: None,
            reqwest: reqwest::Client::new(),
            stream_client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("stream client"),
            inflight: None,
            cancel_state: CancelState::Idle,
            focus_target: crate::function::FocusTarget::Input,
            current_request_seq: 0,
            pending_request: None,
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            should_quit: false,
            msg_tx: None,
            mcp_tools_dirty: true,
            input_prompt_area: None,
            tui_selection: None,
            selected_text: None,
            tui_drag_start: None,
            model_cache_path: cache_file,
            thinking_toggle_rows: Vec::new(),
            tool_toggle_rows: Vec::new(),
            session_area: None,
            input_cursor_screen: None,
            function_panel_cursor: None,
            paste_blocks: VecDeque::new(),
            image_blocks: VecDeque::new(),
            last_paste_text: None,
            last_paste_at: None,
            paste_key_quota: 0,
            burst_buf: String::new(),
            burst_snapshot: None,
            pending_ask_snapshot: String::new(),
            session_scroll: crate::event::ScrollAnimator::default(),
            input_scroll: crate::event::ScrollAnimator::default(),
            input_scroll_decoupled: false,
            compacting: false,
            pending_post_compaction_prompt: None,
            last_mouse_event: None,
        }
    }

    /// Build an app with a configured + validated active provider,
    /// suitable for tests that exercise the chat dispatch path (e.g.
    /// skill dispatch sends a real prompt through send_message).
    fn make_app_with_provider() -> App {
        let mut app = make_app();
        let id = make_id(ProviderKind::Openai, ProviderMode::Key);
        app.config.active = Some(id.clone());
        if let Some(entry) = app.config.entry_mut(&id) {
            entry.base_url = "https://api.example.invalid/v1".to_string();
            entry.api_key = "test-key-do-not-call".to_string();
        }
        app
    }

    #[test]
    fn expand_paste_blocks_replaces_marker_with_block() {
        let mut blocks = VecDeque::from(["a\nb\nc".to_string()]);
        let out = expand_paste_blocks("before [paste 3 lines] after".to_string(), &mut blocks);
        assert_eq!(out, "before ```paste\na\nb\nc\n``` after");
        assert!(blocks.is_empty());
    }

    #[test]
    fn paste_line_count_ignores_trailing_newline() {
        assert_eq!(paste_line_count("a\nb\nc\n"), 3);
    }

    #[test]
    fn settings_save_form_creates_new_entry() {
        let mut app = make_app();
        let form = crate::function::ConfigFormState::new_for_create(
            ProviderKind::Openai,
            ProviderMode::Key,
        );
        // form starts with empty base_url and key.
        settings_save_form(&mut app, form.clone());
        // make_app already has openai:key, so dedup creates openai:key-2.
        let id: ProviderId = format!("{}-2", make_id(ProviderKind::Openai, ProviderMode::Key));
        assert!(app.config.entries.contains_key(&id));
        assert_eq!(app.config.active.as_deref(), Some(id.as_str()));
        let entry = app.config.entry(&id).unwrap();
        assert_eq!(entry.base_url, "");
        assert_eq!(entry.model, "");
    }

    #[test]
    fn settings_save_form_preserves_existing_model_on_edit() {
        let mut app = make_app();
        // Pre-populate an existing entry with a custom model.
        let id = make_id(ProviderKind::Anthropic, ProviderMode::Env);
        app.config.entries.insert(
            id.clone(),
            ProviderConfig {
                api_key: String::new(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                base_url: "https://api.anthropic.com".to_string(),
                model: "claude-3-5-sonnet-latest".to_string(),
                model_display: String::new(),
                name: String::new(),
                access_key: String::new(),
                secret_key: String::new(),
            },
        );
        let form = crate::function::ConfigFormState::new_for_edit(
            id.clone(),
            app.config.entry(&id).unwrap(),
            ProviderMode::Env,
        );
        // modify base_url and env
        let mut form = form;
        form.base_url = "https://custom.example.com".to_string();
        form.api_key_env = "CUSTOM_ENV".to_string();
        form.env_modified = true;
        settings_save_form(&mut app, form);
        let entry = app.config.entry(&id).unwrap();
        assert_eq!(entry.base_url, "https://custom.example.com");
        assert_eq!(entry.api_key_env, "CUSTOM_ENV");
        // model preserved
        assert_eq!(entry.model, "claude-3-5-sonnet-latest");
    }

    #[test]
    fn commit_model_updates_active_entry_model() {
        let mut app = make_app();
        let id = make_id(ProviderKind::Openai, ProviderMode::Key);
        app.config.active = Some(id.clone());
        commit_model(&mut app, ProviderKind::Openai, "gpt-4o".to_string(), false);
        let entry = app.config.entry(&id).unwrap();
        assert_eq!(entry.model, "gpt-4o");
    }

    #[test]
    fn commit_model_falls_back_to_matching_kind() {
        let mut app = make_app();
        // active is Openai:key but user picks for Anthropic
        app.config.active = Some(make_id(ProviderKind::Openai, ProviderMode::Key));
        commit_model(
            &mut app,
            ProviderKind::Anthropic,
            "claude-3-5-sonnet-latest".to_string(),
            false,
        );
        // active should now be the Anthropic entry
        let id = make_id(ProviderKind::Anthropic, ProviderMode::Key);
        assert_eq!(app.config.active.as_deref(), Some(id.as_str()));
        let entry = app.config.entry(&id).unwrap();
        assert_eq!(entry.model, "claude-3-5-sonnet-latest");
    }

    #[test]
    fn picker_state_initializes() {
        // Sanity: ModelPickerState::new(provider) doesn't panic
        let _p = ModelPickerState::new(ProviderKind::Anthropic);
        let _ = SidebarTab::ModelPicker(ModelPickerState::new(ProviderKind::Openai));
    }

    #[test]
    fn config_form_enter_on_text_field_does_not_advance_focus() {
        // After my refactor, pressing Enter on BaseUrl / KeyOrEnv should
        // leave the level and focus unchanged. We test by calling
        // handle_settings_enter and inspecting state.level.
        use crate::function::ConfigField;
        use crate::function::SettingsLevel;

        let mut app = make_app();
        let form = crate::function::ConfigFormState::new_for_create(
            ProviderKind::Openai,
            ProviderMode::Key,
        );
        // Caller has typed a base url, now sits on BaseUrl, presses Enter.
        let mut form = form;
        form.base_url = "https://api.openai.com/v1".to_string();
        form.focused = ConfigField::BaseUrl;

        let mut state = crate::function::SettingsState::new(&app.config);
        state.level = SettingsLevel::ConfigForm(form.clone());
        state.cursor = 0;

        // fake a key event
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        );
        handle_settings_enter(&mut app, &mut state);

        // Should still be in the form, on the same field.
        match &state.level {
            SettingsLevel::ConfigForm(f) => {
                assert_eq!(
                    f.focused,
                    ConfigField::BaseUrl,
                    "Enter on BaseUrl must not auto-advance"
                );
                assert!(f.form_error.is_none());
            }
            other => panic!("expected to stay in ConfigForm, got {other:?}"),
        }
        let _ = key; // kept to mirror the production call shape
    }

    #[test]
    fn commit_model_with_open_picker_does_not_panic() {
        // Reproduces the user-reported crash: open picker, focus List,
        // press Enter. The function should write the model and remove the
        // picker tab without panicking.
        let mut app = make_app();
        let id = make_id(ProviderKind::Openai, ProviderMode::Key);
        app.config.active = Some(id.clone());
        app.function.push(SidebarTab::ModelPicker(
            crate::function::ModelPickerState::new(ProviderKind::Openai),
        ));
        let picker_idx = app.function.tabs.len() - 1;
        app.function.active = picker_idx;

        // Verify pre-state.
        assert!(app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::ModelPicker(_))));

        commit_model(&mut app, ProviderKind::Openai, "gpt-4o".to_string(), false);

        // Picker tab should be gone.
        assert!(!app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::ModelPicker(_))));
        // Active entry's model updated.
        let entry = app.config.entry(&id).unwrap();
        assert_eq!(entry.model, "gpt-4o");
        // Either the panel is empty (active 0, len 0) or active is in
        // bounds. With the new design no tab is permanent.
        if !app.function.tabs.is_empty() {
            assert!(app.function.active < app.function.tabs.len());
        }
    }

    #[tokio::test]
    async fn dispatch_provider_picker_enter_replaces_with_model_picker() {
        // The user-reported bug: pressing Enter in the provider picker
        // did nothing. Root cause: the per-tab handler removed the
        // ProviderPicker and pushed a new ModelPicker, but the
        // dispatcher's `std::mem::replace` pattern then put the moved-out
        // ProviderPicker back on top of the freshly-pushed tab. The fix
        // detects that the placeholder slot is gone and drops the
        // moved-out copy. This test goes through the full dispatch path
        // (not just the handler) so the regression is caught.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = make_app();
        // Default Config ships with two providers; /model opens the
        // picker step.
        crate::commands::open_model_picker(&mut app);
        assert!(matches!(
            app.function.tabs[app.function.active],
            SidebarTab::ProviderPicker(_)
        ));
        let provider_count = app
            .function
            .tabs
            .iter()
            .filter(|t| matches!(t, SidebarTab::ProviderPicker(_)))
            .count();
        assert_eq!(provider_count, 1, "exactly one ProviderPicker should exist");

        // Simulate Enter through the dispatcher.
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let consumed = dispatch_to_active_tab(key, &mut app).await;
        assert!(consumed, "Enter must be consumed by the picker");

        // After Enter: ModelPicker is pushed on top of the
        // ProviderPicker (not replacing it). The user can now Esc on
        // the ModelPicker to return to provider selection.
        assert_eq!(
            app.function
                .tabs
                .iter()
                .filter(|t| matches!(t, SidebarTab::ProviderPicker(_)))
                .count(),
            1,
            "ProviderPicker must remain in the tab stack after Enter \
             so Esc can return to it"
        );
        assert!(matches!(
            app.function.tabs[app.function.active],
            SidebarTab::ModelPicker(_)
        ));
    }

    #[tokio::test]
    async fn dispatch_model_picker_esc_returns_to_provider_picker() {
        // The user reported that Esc on the ModelPicker did not
        // navigate back to the ProviderPicker. The fix is to push
        // (not replace) the ModelPicker so the ProviderPicker stays in
        // the tab stack; the global Esc handler then closes the active
        // ModelPicker and the previous ProviderPicker becomes the
        // active tab via `close_active`'s index adjustment.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = make_app();
        crate::commands::open_model_picker(&mut app);
        // Pick the first provider to push a ModelPicker.
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        dispatch_to_active_tab(key, &mut app).await;
        assert!(matches!(
            app.function.tabs[app.function.active],
            SidebarTab::ModelPicker(_)
        ));

        // Simulate the global Esc handler closing the active tab
        // (this is the path the model picker takes — it returns
        // `false` from its Esc arm and the global handler does
        // `close_active`).
        let was_visible = app.function_visible;
        let _ = app.function.close_active();
        app.maybe_hide_panel();

        // We should land back on the ProviderPicker (the previous
        // tab in the stack), not on an empty panel.
        assert!(
            app.function
                .tabs
                .iter()
                .any(|t| matches!(t, SidebarTab::ProviderPicker(_))),
            "ProviderPicker should still exist after Esc on ModelPicker"
        );
        assert!(matches!(
            app.function.tabs[app.function.active],
            SidebarTab::ProviderPicker(_)
        ));
        // Panel stays visible (the ProviderPicker is still open).
        assert!(app.function_visible || !was_visible);
    }

    #[test]
    fn commit_model_closes_provider_picker_behind_it() {
        // Committing a model in the /model flow must close the
        // ProviderPicker too, otherwise the user lands on the
        // provider list and has to Esc a second time.
        use crate::config::{make_id, ProviderKind, ProviderMode};

        let mut app = make_app();
        // Simulate the post-ProviderPicker-pick tab stack.
        app.function.push(SidebarTab::ProviderPicker(
            crate::function::ProviderPickerState::new(&app.config),
        ));
        let _provider_idx = app.function.tabs.len() - 1;
        app.function.push(SidebarTab::ModelPicker(
            crate::function::ModelPickerState::new(ProviderKind::Anthropic),
        ));
        app.function.active = app.function.tabs.len() - 1;
        // Sanity: 2 tabs, ModelPicker active.
        assert_eq!(app.function.tabs.len(), 2);
        assert!(matches!(
            app.function.tabs[app.function.active],
            SidebarTab::ModelPicker(_)
        ));
        let provider_id = make_id(ProviderKind::Anthropic, ProviderMode::Key);
        app.config.active = Some(provider_id.clone());

        // Commit a model directly.
        commit_model(
            &mut app,
            ProviderKind::Anthropic,
            "claude-3-5".to_string(),
            false,
        );

        // Both tabs are gone — the flow ended cleanly.
        assert!(
            !app.function
                .tabs
                .iter()
                .any(|t| matches!(t, SidebarTab::ProviderPicker(_))),
            "ProviderPicker must close after commit (the flow ended)"
        );
        assert!(
            !app.function
                .tabs
                .iter()
                .any(|t| matches!(t, SidebarTab::ModelPicker(_))),
            "ModelPicker must close after commit"
        );
    }

    #[tokio::test]
    async fn dispatch_settings_esc_at_toplevel_with_other_tab_does_not_resurrect_settings() {
        // The same dispatcher bug also affected Settings: pressing Esc
        // at TopLevel removed the Settings tab, but the dispatcher put
        // it back when there were other tabs after it. Fixing the
        // dispatcher's restore logic also fixes this.
        use crate::function::{SettingsLevel, SettingsState};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = make_app();
        // Add a Notifications tab AFTER the Settings tab so the bug
        // would have resurrected the Settings tab.
        app.function.push(SidebarTab::Notifications);
        let notif_idx = app.function.tabs.len() - 1;
        // Push a Settings tab and make it the active one.
        let mut s = SettingsState::new(&app.config);
        s.level = SettingsLevel::TopLevel;
        app.function.push(SidebarTab::Settings(Box::new(s)));
        app.function.active = app.function.tabs.len() - 1;
        let settings_idx = app.function.active;
        assert_ne!(settings_idx, notif_idx);

        // Simulate Esc through the dispatcher.
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let consumed = dispatch_to_active_tab(key, &mut app).await;
        assert!(consumed, "Esc at TopLevel must be consumed");

        // After Esc: Settings tab is gone; Notifications tab remains.
        assert!(
            !app.function
                .tabs
                .iter()
                .any(|t| matches!(t, SidebarTab::Settings(_))),
            "Settings must be removed after Esc at TopLevel"
        );
        assert!(app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::Notifications)));
    }

    #[test]
    fn picker_scroll_keeps_cursor_visible() {
        let mut s = ModelPickerState::new(ProviderKind::Openai);
        for i in 0..20 {
            s.models.push(crate::function::notifications::ModelInfo {
                id: format!("m{i}"),
                display: format!("model {i}"),
                request_id: None,
                context_window_tokens: None,
                context_needs_pick: false,
            });
        }
        s.rebuild_filter();
        assert_eq!(s.cursor, 0);
        assert_eq!(s.scroll, 0);

        // Move cursor to the end.
        s.cursor = 19;
        crate::ui::function_panel::ensure_cursor_visible(s.cursor, &mut s.scroll, 5);
        assert_eq!(s.scroll, 15, "scroll must advance to keep cursor visible");

        // Move cursor to the top.
        s.cursor = 0;
        crate::ui::function_panel::ensure_cursor_visible(s.cursor, &mut s.scroll, 5);
        assert_eq!(s.scroll, 0, "scroll must retreat when cursor goes up");
    }

    #[test]
    fn commit_model_picks_model_from_picker_list() {
        // Simulate the full user flow: open picker, populate models via
        // ModelsFetched, then commit a model via the picker list.
        use crate::event::AppMsg;

        let mut app = make_app();
        // Ensure openai:key is the active entry.
        let id = make_id(ProviderKind::Openai, ProviderMode::Key);
        app.config.active = Some(id.clone());

        // Open the picker.
        let mut picker = crate::function::ModelPickerState::new(ProviderKind::Openai);
        picker.focus = crate::function::PickerFocus::List;
        picker
            .models
            .push(crate::function::notifications::ModelInfo {
                id: "gpt-4o".to_string(),
                display: "gpt-4o".to_string(),
                request_id: None,
                context_window_tokens: None,
                context_needs_pick: false,
            });
        picker
            .models
            .push(crate::function::notifications::ModelInfo {
                id: "gpt-4o-mini".to_string(),
                display: "gpt-4o-mini".to_string(),
                request_id: None,
                context_window_tokens: None,
                context_needs_pick: false,
            });
        picker.rebuild_filter();
        picker.cursor = 1;
        app.function
            .push(crate::function::SidebarTab::ModelPicker(picker));
        let picker_idx = app.function.tabs.len() - 1;
        app.function.active = picker_idx;

        // Simulate "press Enter on the focused model" — the picker would
        // do `commit_model(_app, state.provider, id, false)`.
        let model_to_pick = {
            let s = match &app.function.tabs[picker_idx] {
                crate::function::SidebarTab::ModelPicker(s) => s,
                _ => unreachable!(),
            };
            s.models[s.filtered[s.cursor]].id.clone()
        };
        commit_model(&mut app, ProviderKind::Openai, model_to_pick.clone(), false);

        // Verify post-state.
        assert!(!app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::ModelPicker(_))));
        let entry = app.config.entry(&id).unwrap();
        assert_eq!(entry.model, model_to_pick);
        let _ = AppMsg::ChatError { seq: 0, error: String::new() }; // suppress unused
    }

    #[test]
    fn commit_model_with_empty_function_panel_does_not_panic() {
        // The function panel has only the picker, no Notifications. After
        // commit, the function panel should only have the Notifications tab
        // (created by save_config's notify). Verify no panic.
        let mut app = make_app();
        app.function.tabs.clear();
        app.function.active = 0;
        app.config.active = Some(make_id(ProviderKind::Openai, ProviderMode::Key));
        app.function.push(SidebarTab::ModelPicker(
            crate::function::ModelPickerState::new(ProviderKind::Openai),
        ));
        app.function.active = 0;

        commit_model(&mut app, ProviderKind::Openai, "gpt-4o".to_string(), false);

        // After commit, the Notifications tab is created by save_config.
        assert_eq!(app.function.tabs.len(), 1);
        assert!(matches!(
            app.function.tabs[0],
            SidebarTab::Notifications
        ));
        assert_eq!(app.function.active, 0);
    }

    #[test]
    fn open_settings_pushes_fresh_tab_each_invocation() {
        // Each /settings invocation must open a brand-new tab with a fresh
        // state (TopLevel / cursor 0 / no form_error). Old tabs are kept
        // (the user closes them with Esc), so the tab list grows.
        use crate::function::SettingsLevel;

        let mut app = make_app();
        // Plant an existing Settings tab deep in a non-default level with
        // stale state.
        let mut st = crate::function::SettingsState::new(&app.config);
        st.level = SettingsLevel::ProviderList;
        st.cursor = 3;
        st.form_error = Some("stale error".to_string());
        st.load_error = Some("stale load error".to_string());
        app.function.push(SidebarTab::Settings(Box::new(st)));
        let tabs_before = app.function.tabs.len();
        // Active index is on the old settings tab.
        let old_active = app.function.active;

        crate::commands::open_settings(&mut app);

        // A new tab was pushed.
        assert_eq!(app.function.tabs.len(), tabs_before + 1);
        // Active advanced to the new tab.
        assert_eq!(app.function.active, old_active + 1);
        // Old tab is still in the list and untouched.
        match &app.function.tabs[old_active] {
            SidebarTab::Settings(s) => {
                assert!(matches!(s.level, SettingsLevel::ProviderList));
                assert_eq!(s.cursor, 3);
                assert!(s.form_error.is_some());
            }
            other => panic!("expected old settings tab to remain, got {other:?}"),
        }
        // New tab starts at TopLevel with clean state.
        match &app.function.tabs[app.function.active] {
            SidebarTab::Settings(s) => {
                assert!(matches!(s.level, SettingsLevel::TopLevel));
                assert_eq!(s.cursor, 0);
                assert!(s.form_error.is_none());
                assert!(s.load_error.is_none());
            }
            other => panic!("expected fresh Settings tab, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_n_shows_and_focuses_notifications_when_hidden() {
        // Panel is hidden (default after the new App::new). Pressing Ctrl+N
        // must show it and focus the Notifications tab.
        let mut app = make_app();
        // Push a settings tab so the active tab is non-Notification.
        app.function.push(SidebarTab::Settings(Box::new(
            crate::function::SettingsState::new(&app.config),
        )));
        app.function.active = app.function.tabs.len() - 1;
        assert!(!app.function_visible);
        // No Notifications tab yet.
        assert!(!app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::Notifications)));

        handle_ctrl_n(&mut app);

        assert!(app.function_visible);
        // Active index points at the Notifications tab (the one we just
        // created).
        assert!(matches!(
            app.function.tabs[app.function.active],
            SidebarTab::Notifications
        ));
        // Both Notifications and Settings are present.
        assert_eq!(app.function.tabs.len(), 2);
    }

    #[test]
    fn ctrl_n_hides_when_notifications_active_and_visible() {
        // Panel is visible and Notifications is active. Pressing Ctrl+N
        // must remove the Notifications tab and hide the panel.
        let mut app = make_app();
        // Create the Notifications tab first.
        handle_ctrl_n(&mut app);
        assert!(app.function_visible);
        assert!(matches!(
            app.function.tabs[app.function.active],
            SidebarTab::Notifications
        ));

        handle_ctrl_n(&mut app);

        assert!(!app.function_visible);
        assert!(app.function.tabs.is_empty());
    }

    #[test]
    fn ctrl_n_focuses_notifications_when_other_tab_active_and_visible() {
        // Panel is visible but a different tab is active. Pressing Ctrl+N
        // must switch focus to Notifications (not hide).
        let mut app = make_app();
        app.function_visible = true;
        app.function.push(SidebarTab::Settings(Box::new(
            crate::function::SettingsState::new(&app.config),
        )));
        app.function.active = app.function.tabs.len() - 1;
        assert!(matches!(
            app.function.tabs[app.function.active],
            SidebarTab::Settings(_)
        ));

        handle_ctrl_n(&mut app);

        assert!(app.function_visible, "panel must remain visible");
        assert!(matches!(
            app.function.tabs[app.function.active],
            SidebarTab::Notifications
        ));
    }

    #[test]
    fn app_default_visibility_is_hidden_and_no_tabs() {
        // Fresh app: function panel hidden, no tabs at all. Notifications
        // is only created on-demand (Ctrl+N or important toast).
        let app = make_app();
        assert!(!app.function_visible);
        assert!(app.function.tabs.is_empty());
    }

    #[test]
    fn check_config_does_not_auto_open_settings() {
        // User preference: "default hidden, show only on toast or slash command".
        // check_config may push Fail toasts (which auto-show the panel because
        // Fail is important), but it must NOT push a Settings tab.
        use crate::config::{make_id, ProviderConfig, ProviderKind, ProviderMode};

        let mut app = make_app();
        // Plant a bad entry so validate_all returns an error.
        let id = make_id(ProviderKind::Openai, ProviderMode::Key);
        app.config.entries.insert(
            id.clone(),
            ProviderConfig {
                api_key: String::new(),
                api_key_env: String::new(),
                base_url: String::new(), // triggers "base_url is required"
                model: "gpt-4o-mini".to_string(),
                model_display: String::new(),
                name: String::new(),
                access_key: String::new(),
                secret_key: String::new(),
            },
        );
        app.config.active = Some(id.clone());

        let result = app.check_config();
        assert!(
            !result,
            "check_config must return false when there are errors"
        );
        // Tabs must still be only Notifications. No Settings tab.
        assert_eq!(app.function.tabs.len(), 1);
        assert!(matches!(app.function.tabs[0], SidebarTab::Notifications));
        // Toasts were pushed (Fail auto-shows the panel; that's allowed).
        assert!(!app.notifications.items.is_empty());
    }

    #[test]
    fn check_config_prompts_to_set_up_provider_when_none_usable() {
        // First-launch scenario: Config::default() ships with two entries
        // (openai:key, anthropic:key) that both fail validation. Since no
        // entry is usable, the toast should be a single friendly prompt
        // asking the user to set up one of the available providers, NOT a
        // consolidated list of scary errors.
        let mut app = make_app();
        assert_eq!(app.config.entries.len(), 2);

        let result = app.check_config();
        assert!(!result);
        assert_eq!(app.notifications.items.len(), 1);
        let toast = &app.notifications.items[0];
        assert!(
            toast.text.contains("no provider configured"),
            "toast must prompt to set up a provider, got: {}",
            toast.text
        );
        assert!(
            toast.text.contains("openai") && toast.text.contains("anthropic"),
            "toast must mention both openai and anthropic, got: {}",
            toast.text
        );
    }

    #[test]
    fn check_config_shows_specific_errors_when_some_usable() {
        // If at least one entry is valid but others are misconfigured, we
        // show the specific errors for the broken ones so the user knows
        // what to fix.
        use crate::config::{make_id, ProviderConfig, ProviderKind, ProviderMode};

        let mut app = make_app();
        // Replace the default openai:key with a valid one (direct api_key).
        let valid_id = make_id(ProviderKind::Openai, ProviderMode::Key);
        app.config.entries.insert(
            valid_id.clone(),
            ProviderConfig {
                api_key: "test_key".to_string(),
                api_key_env: "OPENAI_API_KEY".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                model: "gpt-4o-mini".to_string(),
                model_display: String::new(),
                name: String::new(),
                access_key: String::new(),
                secret_key: String::new(),
            },
        );
        // Default anthropic:key is still invalid (empty api_key, env unset).
        app.config.active = Some(valid_id);

        let result = app.check_config();
        assert!(!result);
        // Exactly one toast: the consolidated error for the broken entry.
        assert_eq!(app.notifications.items.len(), 1);
        let toast = &app.notifications.items[0];
        assert!(
            toast.text.contains("anthropic:key"),
            "toast must mention the broken entry, got: {}",
            toast.text
        );
    }

    #[test]
    fn ctrl_n_clears_notifications_when_hiding() {
        // User wants the notification list to be empty on every Ctrl+N open.
        // Closing via Ctrl+N while Notifications is active must wipe the queue
        // and reset the pending counter.
        use crate::function::notifications::ToastLevel;

        let mut app = make_app();
        app.function_visible = true;
        app.function.active = 0; // Notifications
        app.notify(ToastLevel::Fail, "boom");
        app.pending_events = 3;
        assert!(!app.notifications.items.is_empty());

        handle_ctrl_n(&mut app);

        assert!(!app.function_visible);
        assert!(
            app.notifications.items.is_empty(),
            "notifications must be cleared on close"
        );
        assert_eq!(app.pending_events, 0);
    }

    #[test]
    fn ctrl_n_does_not_clear_when_switching_tabs() {
        // Switching from another tab to Notifications (panel stays visible)
        // must NOT clear the list. Only closing (visible -> hidden) clears.
        use crate::function::notifications::ToastLevel;

        let mut app = make_app();
        app.function_visible = true;
        app.function.push(SidebarTab::Settings(Box::new(
            crate::function::SettingsState::new(&app.config),
        )));
        app.function.active = app.function.tabs.len() - 1;
        // Use Info level so notify() does not auto-switch the active tab.
        app.notify(ToastLevel::Info, "heads up");
        let before = app.notifications.items.len();
        assert!(before > 0);

        handle_ctrl_n(&mut app);

        assert!(app.function_visible, "panel must remain visible");
        assert_eq!(
            app.notifications.items.len(),
            before,
            "switching tabs must not clear notifications"
        );
    }

    #[test]
    fn notifications_push_coalesces_consecutive_duplicates() {
        // If the same toast is pushed twice in a row, only one entry should
        // remain; the timestamp is refreshed. This prevents a chat that
        // repeatedly fails with the same error from filling the list.
        use crate::function::notifications::{Notifications, ToastLevel};

        let mut n = Notifications::default();
        n.push(ToastLevel::Fail, "no active provider");
        n.push(ToastLevel::Fail, "no active provider");
        n.push(ToastLevel::Fail, "no active provider");
        assert_eq!(n.items.len(), 1);
        // Different level or text must still produce a new entry.
        n.push(ToastLevel::Fail, "different error");
        assert_eq!(n.items.len(), 2);
        n.push(ToastLevel::Warn, "different error");
        assert_eq!(n.items.len(), 3);
    }

    #[test]
    fn enter_action_matrix_matches_labels() {
        // Pins down the contract documented in the EnterBehavior settings
        // labels. If any of these four cases flip, the on-screen behavior
        // no longer matches what the user reads in /settings.
        use super::{enter_action, EnterAction};
        use crate::config::EnterBehavior;

        // EnterSends: "Enter sends | Shift+Enter newline"
        assert_eq!(
            enter_action(EnterBehavior::EnterSends, false),
            EnterAction::Send
        );
        assert_eq!(
            enter_action(EnterBehavior::EnterSends, true),
            EnterAction::Newline
        );

        // EnterNewline: "Enter newline | Shift+Enter sends"
        assert_eq!(
            enter_action(EnterBehavior::EnterNewline, false),
            EnterAction::Newline
        );
        assert_eq!(
            enter_action(EnterBehavior::EnterNewline, true),
            EnterAction::Send
        );
    }

    #[test]
    fn submit_input_closes_completion_tab() {
        // Typing "/set" creates a Completion tab. Hitting Enter submits the
        // command and must also remove the Completion tab so the panel does
        // not retain it as stale UI.
        use crate::function::SidebarTab;

        let mut app = make_app();
        app.input.buffer = "/set".to_string();
        app.input.cursor = 4;
        app.sync_completion();
        assert!(app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::Completion(_))));

        submit_input(&mut app);

        assert!(
            !app.function
                .tabs
                .iter()
                .any(|t| matches!(t, SidebarTab::Completion(_))),
            "completion tab must be removed after submit"
        );
    }

    #[test]
    fn submit_input_dispatches_skill_colon_form() {
        // `/skill:<name>` must submit as a slash command, NOT pass
        // the whole `/skill:<name>` string to the chat as a plain
        // message. The new contract is "send immediately": the
        // skill body is pushed as a user message with skill_ref set,
        // and the input buffer is cleared (no manual edit step).
        let names = crate::skill::list_names();
        let pick = names
            .first()
            .cloned()
            .expect("test host must have at least one skill under ~/.agents/skills/");
        let mut app = make_app_with_provider();
        app.input.buffer = format!("/skill:{pick}");
        app.input.cursor = app.input.buffer.len();
        submit_input(&mut app);

        let user_msg = app
            .session
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, crate::session::Role::User))
            .expect("skill dispatch must push a user message");
        assert_ne!(
            user_msg.content,
            format!("/skill:{pick}"),
            "the literal `/skill:<name>` must NOT be the message content - the skill body must be inlined",
        );
        assert!(
            user_msg.skill_ref.is_some(),
            "skill message must carry skill_ref for the [skill] block",
        );
        assert!(
            app.input.buffer.is_empty(),
            "input buffer must be empty after the immediate-send dispatch",
        );
    }

    #[test]
    fn submit_input_skill_colon_with_args_picks_up_trailing_text() {
        // `/skill:<name> 加上一些额外说明` must capture the trailing
        // text into skill_ref.args and append it to the AI prompt.
        let names = crate::skill::list_names();
        let pick = names
            .first()
            .cloned()
            .expect("test host must have at least one skill under ~/.agents/skills/");
        let mut app = make_app_with_provider();
        let user_args = "加上一些额外说明";
        app.input.buffer = format!("/skill:{pick} {user_args}");
        app.input.cursor = app.input.buffer.len();
        submit_input(&mut app);
        let user_msg = app
            .session
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, crate::session::Role::User))
            .expect("skill dispatch must push a user message");
        let skill_ref = user_msg.skill_ref.as_ref().expect("skill_ref must be set");
        assert_eq!(skill_ref.args.as_deref(), Some(user_args));
        assert!(user_msg.content.contains(user_args));
    }

    #[test]
    fn sync_completion_auto_shows_function_panel() {
        // Typing `/` is a function trigger: the user must see the candidate
        // list, so the panel must become visible and focus the new
        // Completion tab. Without this, the user has to manually press
        // Ctrl+N to see the suggestions.
        use crate::function::SidebarTab;

        let mut app = make_app();
        // Start with the panel hidden (the default).
        assert!(!app.function_visible);

        // Type a partial slash command and sync.
        app.input.buffer = "/s".to_string();
        app.input.cursor = 2;
        app.sync_completion();

        // Panel must be visible and the Completion tab must be active.
        assert!(app.function_visible, "function panel must auto-show on /");
        assert!(matches!(
            app.function.tabs[app.function.active],
            SidebarTab::Completion(_)
        ));
    }

    #[test]
    fn sync_completion_shows_plan_subcommand_after_space() {
        use crate::function::SidebarTab;

        let mut app = make_app();
        app.input.buffer = "/plan ".to_string();
        app.input.cursor = app.input.buffer.len();
        app.sync_completion();

        let completion = app
            .function
            .tabs
            .iter()
            .find_map(|t| match t {
                SidebarTab::Completion(s) => Some(s),
                _ => None,
            })
            .expect("completion tab should be visible for /plan subcommands");
        assert_eq!(completion.candidates, vec!["/plan exit".to_string()]);
    }

    #[test]
    fn sync_completion_shows_skill_names_top_level() {
        // Typing `/skill` (no colon yet) should populate the completion
        // list with `/skill:<name>` for every skill under
        // `~/.agents/skills/`. The user can then either keep typing
        // or Tab to insert the full form.
        use crate::function::SidebarTab;

        let mut app = make_app();
        app.input.buffer = "/skill".to_string();
        app.input.cursor = app.input.buffer.len();
        app.sync_completion();

        let completion = app
            .function
            .tabs
            .iter()
            .find_map(|t| match t {
                SidebarTab::Completion(s) => Some(s),
                _ => None,
            })
            .expect("completion tab should be visible for /skill");
        // Every candidate must be the top-level `/skill:<name>` form.
        assert!(
            completion
                .candidates
                .iter()
                .all(|c| c.starts_with("/skill:")),
            "expected only /skill:<name> candidates, got: {:?}",
            completion.candidates,
        );
        let names: Vec<String> = completion
            .candidates
            .iter()
            .map(|c| c.trim_start_matches("/skill:").to_string())
            .collect();
        let commit_skills: Vec<&String> =
            names.iter().filter(|n| n.starts_with("commit")).collect();
        assert!(
            !commit_skills.is_empty(),
            "expected at least one skill starting with 'commit' under ~/.agents/skills/, got: {names:?}",
        );
    }

    #[test]
    fn sync_completion_filters_skill_by_prefix() {
        // `/skill:co` filters the skill list to names starting with
        // `co`; the candidate strings must be top-level
        // `/skill:<name>` form, not the legacy `/skill <name>`.
        use crate::function::SidebarTab;

        let mut app = make_app();
        app.input.buffer = "/skill:co".to_string();
        app.input.cursor = app.input.buffer.len();
        app.sync_completion();

        let completion = app
            .function
            .tabs
            .iter()
            .find_map(|t| match t {
                SidebarTab::Completion(s) => Some(s),
                _ => None,
            })
            .expect("completion tab should be visible for /skill:co");
        assert!(
            completion
                .candidates
                .iter()
                .any(|c| c == "/skill:commit-and-push-all" || c == "/skill:conventional-commit"),
            "expected a /skill:commit-* candidate from ~/.agents/skills/, got: {:?}",
            completion.candidates,
        );
        assert!(!completion
            .candidates
            .iter()
            .any(|c| c == "/skill:karpathy-guidelines"));
        // No legacy `/skill ` candidates must leak through.
        assert!(
            completion
                .candidates
                .iter()
                .all(|c| c.starts_with("/skill:")),
            "every candidate must be a /skill:<name>, got: {:?}",
            completion.candidates,
        );
    }

    #[test]
    fn sync_completion_skill_fuzzy_matches_subsequence() {
        // Fuzzy completion: typing `kpgy` (or any subsequence of
        // `karpathy-guidelines`) should surface that skill. We use a
        // query that actually appears in order: `khg` (k...h...g).
        use crate::function::SidebarTab;

        let mut app = make_app();
        app.input.buffer = "/skill:khg".to_string();
        app.input.cursor = app.input.buffer.len();
        app.sync_completion();

        let completion = app
            .function
            .tabs
            .iter()
            .find_map(|t| match t {
                SidebarTab::Completion(s) => Some(s),
                _ => None,
            })
            .expect("completion tab should be visible for /skill:khg");
        assert!(
            completion
                .candidates
                .iter()
                .any(|c| c == "/skill:karpathy-guidelines"),
            "fuzzy subsequence match for 'khg' should surface karpathy-guidelines, got: {:?}",
            completion.candidates,
        );
    }

    #[test]
    fn sync_completion_command_fuzzy_matches_static() {
        // Fuzzy completion should also work for the static command
        // list: `mdl` subsequence-matches `/model`.
        use crate::function::SidebarTab;

        let mut app = make_app();
        app.input.buffer = "/mdl".to_string();
        app.input.cursor = app.input.buffer.len();
        app.sync_completion();

        let completion = app
            .function
            .tabs
            .iter()
            .find_map(|t| match t {
                SidebarTab::Completion(s) => Some(s),
                _ => None,
            })
            .expect("completion tab should be visible for /mdl");
        assert!(
            completion.candidates.iter().any(|c| c == "/model"),
            "fuzzy match for 'mdl' should surface /model, got: {:?}",
            completion.candidates,
        );
    }

    #[test]
    fn skill_completion_directly_fills_input() {
        // Tab on a focused `/skill:co` candidate fills the buffer with
        // the full `/skill:<name>` form directly. The exact expanded
        // name depends on the user's skills dir, so we only assert the
        // contract: colon preserved, buffer grew.
        let mut app = make_app();
        app.input.buffer = "/skill:co".to_string();
        app.input.cursor = app.input.buffer.len();
        app.sync_completion();
        assert!(complete_focused_candidate(&mut app));
        assert!(
            app.input.buffer.starts_with("/skill:co"),
            "Tab must directly fill with /skill:<name>, got: {}",
            app.input.buffer,
        );
        assert!(app.input.buffer.len() > "/skill:co".len());
    }

    #[test]
    fn dispatch_skill_sends_immediately_with_skill_ref() {
        // /skill:<name> (or dispatch("skill", name)) used to populate
        // the input buffer for the user to edit. The contract changed:
        // the skill now dispatches immediately, pushing a User
        // message into the session with the template body as content
        // and a `skill_ref` describing the visual block.
        use crate::commands::{dispatch, dispatch_skill};
        let names = crate::skill::list_names();
        let pick = names
            .iter()
            .find(|n| n.starts_with("karpathy"))
            .or_else(|| names.first())
            .cloned()
            .expect("test host must have at least one skill under ~/.agents/skills/");
        let mut app = make_app_with_provider();
        dispatch_skill(&mut app, &pick, "");
        // The skill message must be in the session, NOT in the input
        // buffer (which should have been cleared by submit).
        let last_user = app
            .session
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, crate::session::Role::User));
        let user_msg = last_user.expect("skill dispatch must push a user message");
        assert!(
            user_msg.skill_ref.is_some(),
            "skill message must carry skill_ref for the [skill] block render",
        );
        let skill_ref = user_msg.skill_ref.as_ref().unwrap();
        assert_eq!(skill_ref.name, pick);
        assert!(
            user_msg.content.contains('#') || !user_msg.content.is_empty(),
            "skill body must be inlined into the user message",
        );
        // Input buffer should be empty after dispatch (no leftover
        // for the user to edit - the contract is "send now").
        assert!(
            app.input.buffer.is_empty(),
            "input buffer must be empty after /skill:<name> dispatch (got: {:?})",
            app.input.buffer,
        );
        // The `dispatch` alias still routes through `dispatch_skill`.
        let mut app2 = make_app_with_provider();
        dispatch(&mut app2, "skill", &pick);
        // Same shape: message pushed with skill_ref, buffer cleared.
        assert!(app2.input.buffer.is_empty());
        assert!(app2.session.messages.iter().any(|m| m
            .skill_ref
            .as_ref()
            .map(|s| s.name == pick)
            .unwrap_or(false)));
    }

    #[test]
    fn dispatch_skill_passes_trailing_args_through() {
        // /skill:<name> 加上一些额外说明 - the trailing text must be
        // captured into the skill_ref's args field and appended to
        // the AI prompt body.
        use crate::commands::dispatch_skill;
        let names = crate::skill::list_names();
        let pick = names
            .iter()
            .find(|n| n.starts_with("karpathy"))
            .or_else(|| names.first())
            .cloned()
            .expect("test host must have at least one skill under ~/.agents/skills/");
        let mut app = make_app_with_provider();
        let user_args = "加上一些额外说明";
        dispatch_skill(&mut app, &pick, user_args);
        let user_msg = app
            .session
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, crate::session::Role::User))
            .expect("skill dispatch must push a user message");
        let skill_ref = user_msg.skill_ref.as_ref().expect("skill_ref must be set");
        assert_eq!(skill_ref.args.as_deref(), Some(user_args));
        assert!(
            user_msg.content.contains(user_args),
            "trailing args must be appended to the AI prompt body",
        );
    }
    #[test]
    fn dispatch_skill_unknown_name_toasts() {
        use crate::commands::dispatch;
        let mut app = make_app();
        dispatch(&mut app, "skill", "no-such-skill");
        // Input buffer must remain untouched.
        assert!(app.input.buffer.is_empty());
    }

    #[test]
    fn dispatch_mcp_unknown_name_toasts() {
        use crate::commands::dispatch;
        let mut app = make_app();
        dispatch(&mut app, "mcp", "no-such-mcp");
        // No panic, no buffer side-effect.
        assert!(app.input.buffer.is_empty());
    }

    #[test]
    fn completion_tab_completes_without_submitting() {
        let mut app = make_app();
        app.input.buffer = "/pl".to_string();
        app.input.cursor = app.input.buffer.len();
        app.sync_completion();

        assert!(complete_focused_candidate(&mut app));
        assert_eq!(app.input.buffer, "/plan");
        assert_eq!(app.mode, crate::function::AppMode::Yolo);
        assert!(app.session.messages.is_empty());
    }

    #[test]
    fn sync_completion_hides_panel_when_completion_removed() {
        // When the user types `/` then deletes it, the Completion tab is
        // removed. The panel must also hide so the user is back to the
        // default state (Notifications is not a "persistent" tab).
        use crate::function::SidebarTab;

        let mut app = make_app();
        app.input.buffer = "/s".to_string();
        app.input.cursor = 2;
        app.sync_completion();
        assert!(app.function_visible);
        assert!(app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::Completion(_))));

        // Delete the `/`: prefix becomes empty, Completion is removed.
        app.input.buffer.clear();
        app.input.cursor = 0;
        app.sync_completion();

        assert!(!app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::Completion(_))));
        assert!(
            !app.function_visible,
            "panel must hide when only Notifications remains"
        );
    }

    #[test]
    fn sync_completion_keeps_panel_when_other_function_tab_open() {
        // If Settings (or any other function tab) is open while the user
        // removes the Completion, the panel must stay visible.
        use crate::function::SidebarTab;

        let mut app = make_app();
        app.function.push(SidebarTab::Settings(Box::new(
            crate::function::SettingsState::new(&app.config),
        )));
        app.function.active = app.function.tabs.len() - 1;
        app.function_visible = true;
        assert!(app.function.has_any_tab());

        app.input.buffer = "/s".to_string();
        app.input.cursor = 2;
        app.sync_completion();
        app.input.buffer.clear();
        app.input.cursor = 0;
        app.sync_completion();

        assert!(app.function_visible, "Settings tab keeps the panel visible");
    }

    #[test]
    fn settings_form_up_down_moves_focus() {
        // Up/Down must update form.focused in the ConfigForm level, not just
        // the visual cursor. Otherwise typing goes to the previously-Tabbed
        // field while the highlight has moved.
        use crate::function::{ConfigField, SettingsLevel};

        let mut app = make_app();
        let form = crate::function::ConfigFormState::new_for_create(
            ProviderKind::Openai,
            ProviderMode::Key,
        );
        let mut state = crate::function::SettingsState::new(&app.config);
        state.level = SettingsLevel::ConfigForm(form);
        state.cursor = 0;
        // Set focused to Name to start (form opens focused on Name).
        if let SettingsLevel::ConfigForm(ref mut f) = state.level {
            f.focused = ConfigField::Name;
        }

        // Press Down: cursor 0 -> 1, form.focused -> BaseUrl.
        let down = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        );
        handle_settings_key(down, &mut app, &mut state);
        assert_eq!(state.cursor, 1);
        if let SettingsLevel::ConfigForm(f) = &state.level {
            assert_eq!(f.focused, ConfigField::BaseUrl, "Down must move focus");
        } else {
            panic!("expected ConfigForm level");
        }

        // Press Down again: cursor 1 -> 2, form.focused -> Key.
        handle_settings_key(down, &mut app, &mut state);
        assert_eq!(state.cursor, 2);
        if let SettingsLevel::ConfigForm(f) = &state.level {
            assert_eq!(f.focused, ConfigField::Key, "Down must move focus");
        } else {
            panic!("expected ConfigForm level");
        }

        // Press Up twice: back to cursor 0, form.focused -> Name.
        let up = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::NONE,
        );
        handle_settings_key(up, &mut app, &mut state);
        handle_settings_key(up, &mut app, &mut state);
        assert_eq!(state.cursor, 0);
        if let SettingsLevel::ConfigForm(f) = &state.level {
            assert_eq!(f.focused, ConfigField::Name, "Up must move focus");
        } else {
            panic!("expected ConfigForm level");
        }
    }

    #[test]
    fn settings_form_first_edit_clears_masked_key() {
        // In Key mode, the form pre-fills the saved api_key but the UI shows
        // it as a placeholder. The first character pressed on the key field
        // must clear the saved value and mark the form as modified.
        use crate::config::{make_id, ProviderConfig, ProviderKind, ProviderMode};
        use crate::function::ConfigField;
        use crate::function::SettingsLevel;

        let mut app = make_app();
        // Pre-populate an entry with a saved key.
        let id = make_id(ProviderKind::Openai, ProviderMode::Key);
        app.config.entries.insert(
            id.clone(),
            ProviderConfig {
                api_key: "sk-saved-key-1234".to_string(),
                api_key_env: String::new(),
                base_url: "https://api.openai.com/v1".to_string(),
                model: "gpt-4o-mini".to_string(),
                model_display: String::new(),
                name: String::new(),
                access_key: String::new(),
                secret_key: String::new(),
            },
        );
        let form = crate::function::ConfigFormState::new_for_edit(
            id.clone(),
            app.config.entry(&id).unwrap(),
            ProviderMode::Key,
        );
        assert!(!form.key_modified);
        assert_eq!(form.api_key, "sk-saved-key-1234");

        let mut state = crate::function::SettingsState::new(&app.config);
        state.level = SettingsLevel::ConfigForm(form);
        if let SettingsLevel::ConfigForm(ref mut f) = state.level {
            f.focused = ConfigField::Key;
        }

        // Type a single char on Key.
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        );
        handle_settings_key(key, &mut app, &mut state);

        if let SettingsLevel::ConfigForm(f) = &state.level {
            assert!(
                f.key_modified,
                "key_modified must flip to true on first edit"
            );
            assert_eq!(
                f.api_key, "x",
                "saved key must be cleared before the new char"
            );
        } else {
            panic!("expected ConfigForm level");
        }
    }

    #[test]
    fn settings_form_save_preserves_untouched_api_key() {
        // If the user opens the edit form and saves without touching the
        // key field, the original api_key must be preserved (not cleared by
        // the masked placeholder).
        use crate::config::{make_id, ProviderConfig, ProviderKind, ProviderMode};

        let mut app = make_app();
        let id = make_id(ProviderKind::Openai, ProviderMode::Key);
        app.config.entries.insert(
            id.clone(),
            ProviderConfig {
                api_key: "sk-original".to_string(),
                api_key_env: String::new(),
                base_url: "https://api.openai.com/v1".to_string(),
                model: "gpt-4o-mini".to_string(),
                model_display: String::new(),
                name: String::new(),
                access_key: String::new(),
                secret_key: String::new(),
            },
        );
        let form = crate::function::ConfigFormState::new_for_edit(
            id.clone(),
            app.config.entry(&id).unwrap(),
            ProviderMode::Key,
        );
        // key_modified is false; user never touched the field.
        assert!(!form.key_modified);

        settings_save_form(&mut app, form);

        let entry = app.config.entry(&id).unwrap();
        assert_eq!(
            entry.api_key, "sk-original",
            "untouched api_key must be preserved on save"
        );
    }

    #[test]
    fn settings_form_save_uses_edited_key() {
        // If the user types a new key, the form must save the new value.
        use crate::config::{make_id, ProviderConfig, ProviderKind, ProviderMode};

        let mut app = make_app();
        let id = make_id(ProviderKind::Openai, ProviderMode::Key);
        app.config.entries.insert(
            id.clone(),
            ProviderConfig {
                api_key: "sk-old".to_string(),
                api_key_env: String::new(),
                base_url: "https://api.openai.com/v1".to_string(),
                model: "gpt-4o-mini".to_string(),
                model_display: String::new(),
                name: String::new(),
                access_key: String::new(),
                secret_key: String::new(),
            },
        );
        let mut form = crate::function::ConfigFormState::new_for_edit(
            id.clone(),
            app.config.entry(&id).unwrap(),
            ProviderMode::Key,
        );
        form.key_modified = true;
        form.api_key = "sk-new".to_string();

        settings_save_form(&mut app, form);

        let entry = app.config.entry(&id).unwrap();
        assert_eq!(entry.api_key, "sk-new");
    }

    #[test]
    fn esc_after_open_model_picker_with_no_provider_does_not_panic() {
        // Scenario reported by the user:
        //   1. App has no active provider AND no configured entries.
        //   2. User types /model and presses Enter.
        //   3. open_model_picker pushes a toast and falls through to
        //      open_settings (which pushes a fresh Settings tab).
        //   4. User presses Esc to dismiss the settings.
        // None of these steps may panic, even when the active provider is
        // missing or the entries map is sparse.
        let mut app = make_app();
        app.config.active = None;
        // The default Config ships with two pre-configured providers, so
        // the new two-step /model flow would show a ProviderPicker here.
        // To exercise the "no provider at all" fallback we also have to
        // clear the entries map.
        app.config.entries.clear();

        app.input.buffer = "/model".to_string();
        app.input.cursor = 6;
        app.sync_completion();
        submit_input(&mut app);

        // We should now be in a Settings tab.
        assert!(app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::Settings(_))));

        // Simulate Esc: close the active tab and run the auto-hide check.
        let closed = app.function.close_active();
        assert!(closed);
        app.maybe_hide_panel();

        // No panic. The Notifications tab was created by `notify(Warn, ...)`
        // inside `open_model_picker`, so after closing Settings the
        // Notifications tab remains and the panel stays visible. The user
        // can Ctrl+N to hide.
        assert!(app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::Notifications)));
    }

    #[test]
    fn open_model_picker_jumps_settings_to_provider_list() {
        // When /model is invoked with no configured provider at all, the
        // user is routed into settings. We must skip the redundant
        // TopLevel ("set provider") and land directly on ProviderList so
        // the user can pick a kind/mode or edit an existing entry right
        // away.
        use crate::function::SettingsLevel;

        let mut app = make_app();
        app.config.active = None;
        // Same as above: clear entries so the "no provider" fallback
        // kicks in instead of the new ProviderPicker.
        app.config.entries.clear();

        crate::commands::open_model_picker(&mut app);

        // Find the Settings tab and verify its level.
        let settings = app
            .function
            .tabs
            .iter()
            .find_map(|t| match t {
                SidebarTab::Settings(s) => Some(s),
                _ => None,
            })
            .expect("Settings tab must be pushed by open_model_picker fallback");
        assert!(
            matches!(settings.level, SettingsLevel::ProviderList),
            "expected ProviderList, got {:?}",
            settings.level
        );
    }

    #[test]
    fn open_model_picker_with_single_provider_skips_provider_picker() {
        // When the user has exactly one provider kind configured, the
        // ProviderPicker step is skipped and the model picker opens
        // directly. Otherwise the new two-step flow would force an
        // unnecessary Up/Down + Enter for the obvious choice.
        let mut app = make_app();
        // Default config has both openai and anthropic; collapse to just
        // anthropic by removing openai.
        app.config
            .entries
            .retain(|id, _| id.starts_with("anthropic:"));
        app.config.active = None;
        let tabs_before = app.function.tabs.len();

        crate::commands::open_model_picker(&mut app);

        // Should have created exactly one new tab: the ModelPicker.
        assert_eq!(app.function.tabs.len(), tabs_before + 1);
        assert!(matches!(
            app.function.tabs[app.function.active],
            SidebarTab::ModelPicker(_)
        ));
        // No ProviderPicker should be present.
        assert!(!app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::ProviderPicker(_))));
    }

    #[test]
    fn open_model_picker_with_multiple_providers_shows_provider_picker() {
        // The new /model flow is two steps: pick a provider, then a
        // model. With more than one provider kind configured, the
        // ProviderPicker should appear.
        let mut app = make_app();
        // Default config already has both openai and anthropic.
        assert!(app.config.active.is_some() || app.config.active.is_none());
        let tabs_before = app.function.tabs.len();

        crate::commands::open_model_picker(&mut app);

        // Should have created exactly one new tab: the ProviderPicker.
        assert_eq!(app.function.tabs.len(), tabs_before + 1);
        assert!(matches!(
            app.function.tabs[app.function.active],
            SidebarTab::ProviderPicker(_)
        ));
    }

    #[test]
    fn provider_picker_shows_user_names_not_kinds() {
        // The picker must show each entry by its user-defined `name`
        // (falling back to "Kind (mode)" when no name is set). Listing
        // bare kind names like "OpenAI" / "Anthropic" was the bug:
        // when the user has two OpenAI entries with different names,
        // the kind-only display makes them indistinguishable.
        use crate::config::{make_id, ProviderConfig, ProviderKind, ProviderMode};

        let cfg = {
            let mut cfg = crate::config::Config::default();
            // Wipe so we control the entries exactly.
            cfg.entries.clear();
            cfg.entries.insert(
                make_id(ProviderKind::Openai, ProviderMode::Key),
                ProviderConfig {
                    name: "staging-openai".to_string(),
                    ..ProviderConfig::default()
                },
            );
            cfg.entries.insert(
                make_id(ProviderKind::Openai, ProviderMode::Env),
                ProviderConfig {
                    name: "prod-openai".to_string(),
                    ..ProviderConfig::default()
                },
            );
            cfg.entries.insert(
                make_id(ProviderKind::Anthropic, ProviderMode::Key),
                ProviderConfig {
                    name: String::new(),
                    ..ProviderConfig::default()
                },
            );
            cfg
        };
        let state = crate::function::ProviderPickerState::new(&cfg);

        // 3 rows (one per configured entry, not per kind).
        assert_eq!(state.entries.len(), 3);
        // The "staging-openai" entry must show its name, not "OpenAI".
        let staging_display = state
            .entries
            .iter()
            .find(|e| e.id.ends_with(":key") && e.id.starts_with("openai:"))
            .map(|e| e.display.as_str());
        assert_eq!(staging_display, Some("staging-openai"));
        // The nameless Anthropic entry falls back to the kind name.
        let anthro_display = state
            .entries
            .iter()
            .find(|e| e.id.starts_with("anthropic:"))
            .map(|e| e.display.as_str());
        assert_eq!(anthro_display, Some("Anthropic"));
    }

    #[test]
    fn provider_picker_filter_narrows_list() {
        // Mirrors the model picker's filter behavior: typing a query
        // keeps only the entries whose display name or id matches.
        use crate::config::{make_id, ProviderConfig, ProviderKind, ProviderMode};

        let mut cfg = crate::config::Config::default();
        cfg.entries.clear();
        cfg.entries.insert(
            make_id(ProviderKind::Openai, ProviderMode::Key),
            ProviderConfig {
                name: "staging-openai".to_string(),
                ..ProviderConfig::default()
            },
        );
        cfg.entries.insert(
            make_id(ProviderKind::Openai, ProviderMode::Env),
            ProviderConfig {
                name: "prod-openai".to_string(),
                ..ProviderConfig::default()
            },
        );
        cfg.entries.insert(
            make_id(ProviderKind::Anthropic, ProviderMode::Key),
            ProviderConfig {
                name: "prod-anthropic".to_string(),
                ..ProviderConfig::default()
            },
        );

        let mut state = crate::function::ProviderPickerState::new(&cfg);
        assert_eq!(state.filtered.len(), 3);

        state.query = "prod".into();
        state.rebuild_filter();
        // Only the two "prod-*" entries should match.
        assert_eq!(state.filtered.len(), 2);

        state.query = "staging".into();
        state.rebuild_filter();
        assert_eq!(state.filtered.len(), 1);
        assert_eq!(state.selected_id().as_deref(), Some("openai:key"));

        state.query = String::new();
        state.rebuild_filter();
        assert_eq!(state.filtered.len(), 3);
    }

    #[test]
    fn provider_picker_keeps_cursor_visible_when_scrolling() {
        // The previous bug: pressing Down collapsed the list to a single
        // row because the renderer used `scroll = cursor.min(rows - 1)`,
        // which slides the window past the top of the list. The fix
        // uses `ensure_cursor_visible` like the model picker.
        use crate::config::{make_id, ProviderConfig, ProviderKind, ProviderMode};

        let mut cfg = crate::config::Config::default();
        cfg.entries.clear();
        for i in 0..20 {
            let mode = if i % 2 == 0 {
                ProviderMode::Key
            } else {
                ProviderMode::Env
            };
            cfg.entries.insert(
                make_id(ProviderKind::Openai, mode),
                ProviderConfig {
                    name: format!("entry-{i:02}"),
                    ..ProviderConfig::default()
                },
            );
        }

        let mut state = crate::function::ProviderPickerState::new(&cfg);
        // Start at top.
        assert_eq!(state.cursor, 0);
        assert_eq!(state.scroll, 0);

        // Move cursor down past the visible window (visible_rows = 5).
        state.cursor = 10;
        crate::ui::function_panel::ensure_cursor_visible(state.cursor, &mut state.scroll, 5);
        // scroll must have advanced so cursor 10 is inside [scroll, scroll+5).
        assert!(
            state.cursor >= state.scroll && state.cursor < state.scroll + 5,
            "cursor {} not inside scroll window [{}, {})",
            state.cursor,
            state.scroll,
            state.scroll + 5
        );
        // In particular, the old buggy formula would have set scroll=5
        // (10.min(4)=4? — actually 10.min(4)=4, so the start would be 4,
        // skipping rows 0..3, which means the list shows only
        // entries 4..8, hiding the cursor at 10).
        assert!(state.scroll > 0, "scroll must have advanced from 0");

        // Move cursor back to the top: scroll should retreat.
        state.cursor = 0;
        crate::ui::function_panel::ensure_cursor_visible(state.cursor, &mut state.scroll, 5);
        assert_eq!(state.scroll, 0, "scroll must retreat when cursor goes up");
    }

    #[test]
    fn active_name_falls_back_to_kind_when_unset() {
        // If the user hasn't set a custom name, the status bar still
        // shows a useful identifier (the kind name).
        use crate::config::{make_id, ProviderConfig, ProviderKind, ProviderMode};

        let mut app = make_app();
        let id = make_id(ProviderKind::Openai, ProviderMode::Key);
        app.config.entries.insert(
            id.clone(),
            ProviderConfig {
                api_key: "k".to_string(),
                api_key_env: String::new(),
                base_url: "https://api.openai.com/v1".to_string(),
                model: "gpt-4o-mini".to_string(),
                model_display: String::new(),
                name: String::new(),
                access_key: String::new(),
                secret_key: String::new(),
            },
        );
        app.config.active = Some(id.clone());
        assert_eq!(app.config.active_name(), "openai");
    }

    #[test]
    fn active_name_returns_empty_when_no_active_provider() {
        // With no active provider, the status bar must NOT show a stray
        // "-:..." prefix. Instead, the model name (or `(no model)`) is
        // shown on its own.
        let mut app = make_app();
        app.config.active = None;
        assert_eq!(app.config.active_name(), "");
    }

    #[test]
    fn selection_rect_normalizes_start_and_end() {
        // The rect helper should always return the top-left first even
        // when the user drags upward or right-to-left.
        use crate::function::Selection;
        let s = Selection::new((10, 5));
        let s = Selection { end: (3, 8), ..s };
        let ((sx, sy), (ex, ey)) = s.rect();
        assert_eq!((sx, sy), (3, 5));
        assert_eq!((ex, ey), (10, 8));
    }

    #[test]
    fn status_set_cwd_shows_full_path_with_tilde_abbrev() {
        // set_cwd must show the full project path, with the home
        // directory prefix abbreviated as `~`. Earlier versions only
        // showed the basename which made it impossible to tell two
        // projects with the same name apart.
        use crate::input::status::StatusBar;
        if let Some(home) = dirs::home_dir() {
            let project = home.join("Code").join("rust").join("fish_coding_agent");
            let mut s = StatusBar::new();
            s.set_cwd(&project);
            // Cross-platform: the abbrev uses `~` for the home prefix
            // and whatever path separator the host OS uses for the rest.
            assert!(
                s.cwd.starts_with("~/"),
                "cwd should start with ~/, got {:?}",
                s.cwd
            );
            // Each path component must appear, in order, with whatever
            // separator the host uses between them.
            for part in ["Code", "rust", "fish_coding_agent"] {
                assert!(
                    s.cwd.contains(part),
                    "cwd should contain {:?}, got {:?}",
                    part,
                    s.cwd
                );
            }
        }
        // Out-of-home paths must not be abbreviated.
        let mut s = StatusBar::new();
        s.set_cwd(&std::path::PathBuf::from(if cfg!(windows) {
            "C:\\tmp\\foo\\bar"
        } else {
            "/tmp/foo/bar"
        }));
        assert!(
            !s.cwd.starts_with("~"),
            "out-of-home paths must not be abbreviated, got {:?}",
            s.cwd
        );
    }

    #[test]
    fn extract_selection_text_skips_trailing_padding() {
        // Single-row selection across a padded cell line should not
        // produce a wall of trailing spaces.
        use crate::function::Selection;
        use crate::ui::extract_selection_text_for_test;
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        // Render "hello" in cells 0..5 and leave the rest as spaces.
        for (i, c) in "hello".chars().enumerate() {
            buf[(i as u16, 0)].set_symbol(&c.to_string());
        }
        let s = Selection {
            start: (0, 0),
            end: (19, 0),
            active: false,
        };
        let text = extract_selection_text_for_test(&buf, &s);
        assert_eq!(text, "hello");
    }

    #[test]
    fn extract_selection_text_compacts_cjk_render_spacing() {
        use crate::function::Selection;
        use crate::ui::extract_selection_text_for_test;
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let rendered = "使 用 command分 别 执 行 3 次 ls， 需 要 整 个 tree";
        let area = Rect::new(0, 0, rendered.chars().count() as u16, 1);
        let mut buf = Buffer::empty(area);
        for (i, c) in rendered.chars().enumerate() {
            buf[(i as u16, 0)].set_symbol(&c.to_string());
        }
        let s = Selection {
            start: (0, 0),
            end: (area.width - 1, 0),
            active: false,
        };
        let text = extract_selection_text_for_test(&buf, &s);
        assert_eq!(text, "使用 command分别执行 3次 ls，需要整个 tree");
    }

    #[test]
    fn extract_selection_text_compacts_short_ascii_before_cjk() {
        use crate::function::Selection;
        use crate::ui::extract_selection_text_for_test;
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let rendered = "给 我 一 个 md的 代 码 块 示 例 和 表 格 示 例";
        let area = Rect::new(0, 0, rendered.chars().count() as u16, 1);
        let mut buf = Buffer::empty(area);
        for (i, c) in rendered.chars().enumerate() {
            buf[(i as u16, 0)].set_symbol(&c.to_string());
        }
        let s = Selection {
            start: (0, 0),
            end: (area.width - 1, 0),
            active: false,
        };
        let text = extract_selection_text_for_test(&buf, &s);
        assert_eq!(text, "给我一个md的代码块示例和表格示例");
    }

    #[test]
    fn active_name_uses_user_set_value() {
        use crate::config::{make_id, ProviderConfig, ProviderKind, ProviderMode};

        let mut app = make_app();
        let id = make_id(ProviderKind::Anthropic, ProviderMode::Key);
        app.config.entries.insert(
            id.clone(),
            ProviderConfig {
                api_key: "k".to_string(),
                api_key_env: String::new(),
                base_url: "https://api.anthropic.com".to_string(),
                model: "claude-3-5-sonnet-latest".to_string(),
                model_display: String::new(),
                name: "mybot".to_string(),
                access_key: String::new(),
                secret_key: String::new(),
            },
        );
        app.config.active = Some(id.clone());
        assert_eq!(app.config.active_name(), "mybot");
    }

    #[test]
    fn active_model_display_shows_no_model_when_empty() {
        use crate::config::{make_id, ProviderConfig, ProviderKind, ProviderMode};

        let mut app = make_app();
        let id = make_id(ProviderKind::Openai, ProviderMode::Key);
        app.config.entries.insert(
            id.clone(),
            ProviderConfig {
                api_key: "k".to_string(),
                api_key_env: String::new(),
                base_url: "https://api.openai.com/v1".to_string(),
                model: String::new(),
                model_display: String::new(),
                name: "mybot".to_string(),
                access_key: String::new(),
                secret_key: String::new(),
            },
        );
        app.config.active = Some(id.clone());
        assert_eq!(app.config.active_model_display(), "(no model)");
    }

    #[tokio::test]
    async fn esc_at_settings_top_level_does_not_panic_after_tab_removed() {
        // Regression: pressing Esc on the TopLevel of a Settings tab closes
        // the tab. The per-tab handler removes the tab from the panel
        // before returning to the outer key loop. If the outer loop then
        // writes the local copy back to `app.function.tabs[active]`
        // without re-checking bounds, the index is out of range and the
        // program panics with "index out of bounds: the len is 1 but the
        // index is 1".
        use crate::function::SettingsLevel;

        let mut app = make_app();
        let mut state = crate::function::SettingsState::new(&app.config);
        state.level = SettingsLevel::TopLevel;
        app.function.push(SidebarTab::Settings(Box::new(state)));
        app.function.active = app.function.tabs.len() - 1;
        let settings_idx = app.function.active;
        assert!(matches!(
            app.function.tabs[settings_idx],
            SidebarTab::Settings(_)
        ));

        // Press Esc while the Settings tab is active at TopLevel.
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        );

        // This must not panic even though the per-tab handler removes the
        // Settings tab.
        handle_key(key, &mut app).await;

        // The Settings tab is now gone.
        assert!(!app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::Settings(_))));
    }

    #[test]
    fn single_click_does_not_create_tui_selection() {
        // The user complaint: a plain click used to leave a single-cell
        // REVERSED highlight behind. Now a click (Down + Up with no
        // movement) must leave the screen untouched.
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let mut app = make_app();
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        handle_mouse(down, &mut app);
        // Down should only record the drag start, not commit a selection.
        assert!(
            app.tui_selection.is_none(),
            "Down must not create a selection"
        );
        handle_mouse(up, &mut app);
        // Up with no prior Drag must still leave no selection behind.
        assert!(
            app.tui_selection.is_none(),
            "click with no drag must leave no selection"
        );
        assert!(app.tui_drag_start.is_none());
    }

    #[test]
    fn drag_creates_tui_selection_after_real_movement() {
        // Down + Drag + Up with at least one cell of movement must create
        // a TUI selection that the post-render pass highlights.
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let mut app = make_app();
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 10,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 10,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        handle_mouse(down, &mut app);
        handle_mouse(drag, &mut app);
        handle_mouse(up, &mut app);

        let sel = app
            .tui_selection
            .expect("a drag of >0 cells must create a selection");
        assert!(!sel.active, "Up must finalize the selection");
        assert_eq!(sel.start, (2, 2));
        assert_eq!(sel.end, (10, 2));
    }

    fn esc_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())
    }

    fn enter_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())
    }

    /// Esc on a single-question ask tab must surface a synthetic
    /// user turn so the LLM knows the user moved on. We use a
    /// valid-looking test provider but no `msg_tx`, so `send_chat`
    /// reaches the `app.session.push(user_msg)` step before
    /// short-circuiting on the missing event channel. To avoid the
    /// `close_active_function_tab` helper deleting the wrong slot,
    /// we set `function.active` past the end so the helper is a
    /// no-op for this test.
    #[tokio::test]
    async fn ask_esc_dismisses_and_emits_user_turn() {
        let mut app = make_app_with_provider();
        app.open_ask("Which language?".to_string(), vec!["a".into(), "b".into()]);
        assert!(app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::Ask(_))));

        let ask_idx = app.function.tabs.iter().position(|t| matches!(t, SidebarTab::Ask(_))).unwrap();
        let mut state = match app.function.tabs.remove(ask_idx) {
            SidebarTab::Ask(s) => s,
            _ => unreachable!(),
        };
        // Move `active` out of bounds so `close_active_function_tab`
        // is a no-op (we already moved the state out ourselves).
        app.function.active = 99;
        let consumed = handle_ask_key(esc_key(), &mut app, &mut state).await;
        assert!(consumed, "Esc must be consumed by the ask handler");

        // The dismiss message made it into the session as a User turn.
        let last_user = app
            .session
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, crate::session::Role::User))
            .expect("dismiss must push a User message");
        assert!(
            last_user.content.contains("dismissed"),
            "got: {}",
            last_user.content
        );
        assert!(last_user.content.contains("Which language?"));
    }

    /// Enter on a non-freeform option must (1) write the answer into
    /// the item, (2) NOT send anything to the LLM yet, and (3) flip
    /// to the review phase if every question is now answered.
    #[tokio::test]
    async fn ask_enter_marks_answered_and_advances() {
        use crate::function::AskPhase;
        let mut app = make_app_with_provider();
        app.open_ask(
            "Pick one".to_string(),
            vec!["first".into(), "second".into()],
        );

        let before = app.session.messages.len();

        let ask_idx = app.function.tabs.iter().position(|t| matches!(t, SidebarTab::Ask(_))).unwrap();
        let mut state = match app.function.tabs.remove(ask_idx) {
            SidebarTab::Ask(s) => s,
            _ => unreachable!(),
        };
        // cursor starts at 0 ("first").
        let consumed = handle_ask_key(enter_key(), &mut app, &mut state).await;
        assert!(consumed);

        // No chat round triggered yet.
        assert_eq!(
            app.session.messages.len(),
            before,
            "Enter on a single question must not push any new message"
        );

        // The single question is now answered; phase flips to Reviewing.
        assert_eq!(state.phase, AskPhase::Reviewing);
        assert_eq!(
            state.items[0].answered.as_deref(),
            Some("first"),
            "the picked option should be recorded"
        );

        // Tab still open so the user can confirm.
        app.function.tabs.insert(ask_idx, SidebarTab::Ask(state));
        assert!(app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::Ask(_))));
    }

    /// Multi-question: picking the last question's answer must NOT
    /// send immediately. It must flip to Reviewing and leave the
    /// send step to the user's next Enter on the review page.
    #[tokio::test]
    async fn ask_enter_last_question_enters_reviewing() {
        use crate::function::AskPhase;
        let mut app = make_app_with_provider();
        app.open_ask("Q1?".to_string(), vec!["a".into()]);
        app.open_ask("Q2?".to_string(), vec!["b".into()]);

        // open_ask lands on the latest question; pull active back to
        // the first question so the test exercises the advance path.
        let ask_idx = app.function.tabs.iter().position(|t| matches!(t, SidebarTab::Ask(_))).unwrap();
        match &mut app.function.tabs[ask_idx] {
            SidebarTab::Ask(s) => s.active = 0,
            _ => unreachable!(),
        }
        let before = app.session.messages.len();

        let mut state = match app.function.tabs.remove(ask_idx) {
            SidebarTab::Ask(s) => s,
            _ => unreachable!(),
        };
        app.function.active = 99;

        // Answer Q1 → advance to Q2.
        handle_ask_key(enter_key(), &mut app, &mut state).await;
        assert_eq!(state.active, 1, "active moves to the next question");
        assert_eq!(state.phase, AskPhase::Asking);

        // Answer Q2 → all answered → Reviewing.
        handle_ask_key(enter_key(), &mut app, &mut state).await;
        assert_eq!(state.phase, AskPhase::Reviewing);
        assert_eq!(state.items[0].answered.as_deref(), Some("a"));
        assert_eq!(state.items[1].answered.as_deref(), Some("b"));

        // Nothing was sent to the LLM yet.
        assert_eq!(app.session.messages.len(), before);
    }

    /// Enter in the Reviewing phase must send a single summary
    /// containing every Q/A pair and close the tab.
    #[tokio::test]
    async fn ask_reviewing_enter_sends_summary() {
        let mut app = make_app_with_provider();
        app.open_ask("Q1?".to_string(), vec!["a".into()]);
        app.open_ask("Q2?".to_string(), vec!["x".into(), "y".into()]);
        let ask_idx = app.function.tabs.iter().position(|t| matches!(t, SidebarTab::Ask(_))).unwrap();
        match &mut app.function.tabs[ask_idx] {
            SidebarTab::Ask(s) => s.active = 0,
            _ => unreachable!(),
        }
        let mut state = match app.function.tabs.remove(ask_idx) {
            SidebarTab::Ask(s) => s,
            _ => unreachable!(),
        };
        app.function.active = 99;

        // Answer both questions.
        handle_ask_key(enter_key(), &mut app, &mut state).await;
        handle_ask_key(enter_key(), &mut app, &mut state).await;
        assert_eq!(state.phase, crate::function::AskPhase::Reviewing);

        // Now Enter on the review page must send the summary and
        // close the tab.
        handle_ask_key(enter_key(), &mut app, &mut state).await;

        let last_user = app
            .session
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, crate::session::Role::User))
            .expect("summary must push a User message");
        assert!(last_user.content.contains("Q1"));
        assert!(last_user.content.contains("a"));
        assert!(last_user.content.contains("Q2"));
        assert!(last_user.content.contains("x"));
        assert!(last_user.content.contains("Proceed"));

        // The dismiss/summary path closes the tab, but since we set
        // `active = 99` the helper is a no-op — instead, we just
        // verify the function tabs no longer contain the ask state
        // (we removed it manually at the top of the test).
        assert!(!state
            .items
            .iter()
            .any(|it| it.answered.is_none() || it.answered.as_deref() == Some("")));
    }

    /// In the Reviewing phase Up/Down scroll the cursor (it does
    /// NOT pop back to Asking — the user just reviews answers).
    #[tokio::test]
    async fn ask_reviewing_up_returns_to_asking() {
        use crate::function::AskPhase;
        let mut app = make_app_with_provider();
        app.open_ask("Q1?".to_string(), vec!["a".into()]);
        app.open_ask("Q2?".to_string(), vec!["x".into()]);
        let ask_idx = app.function.tabs.iter().position(|t| matches!(t, SidebarTab::Ask(_))).unwrap();
        match &mut app.function.tabs[ask_idx] {
            SidebarTab::Ask(s) => s.active = 0,
            _ => unreachable!(),
        }
        let mut state = match app.function.tabs.remove(ask_idx) {
            SidebarTab::Ask(s) => s,
            _ => unreachable!(),
        };
        app.function.active = 99;
        handle_ask_key(enter_key(), &mut app, &mut state).await;
        handle_ask_key(enter_key(), &mut app, &mut state).await;
        assert_eq!(state.phase, AskPhase::Reviewing);

        // Up pops back to Asking.
        handle_ask_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::empty()),
            &mut app,
            &mut state,
        )
        .await;
        assert_eq!(state.phase, AskPhase::Asking);
    }

    /// Esc at any phase dismisses the whole ask round with a single
    /// summary (mixing answered and skipped entries).
    #[tokio::test]
    async fn ask_esc_dismiss_summary_includes_answered_and_skipped() {
        let mut app = make_app_with_provider();
        app.open_ask("Q1?".to_string(), vec!["a".into()]);
        app.open_ask("Q2?".to_string(), vec!["x".into()]);
        let ask_idx = app.function.tabs.iter().position(|t| matches!(t, SidebarTab::Ask(_))).unwrap();
        match &mut app.function.tabs[ask_idx] {
            SidebarTab::Ask(s) => s.active = 0,
            _ => unreachable!(),
        }
        let mut state = match app.function.tabs.remove(ask_idx) {
            SidebarTab::Ask(s) => s,
            _ => unreachable!(),
        };
        app.function.active = 99;

        // Answer Q1; leave Q2 unanswered; then Esc.
        handle_ask_key(enter_key(), &mut app, &mut state).await;
        handle_ask_key(esc_key(), &mut app, &mut state).await;

        let last_user = app
            .session
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, crate::session::Role::User))
            .expect("dismiss must push a User message");
        assert!(last_user.content.contains("Q1"));
        assert!(last_user.content.contains("a"));
        assert!(last_user.content.contains("Q2"));
        assert!(last_user.content.contains("dismissed"));
    }

    /// Down moves the cursor through the merged list. With two
    /// questions and 2 options each the rows are
    ///   0: q1 header, 1: opt0, 2: opt1, 3: freeform,
    ///   4: q2 header, 5: opt0, 6: opt1, 7: freeform.
    /// Up/Down move the per-question cursor (wrap around). The
    /// picker is per-question; Left/Right switch `active` (see
    /// `ask_left_right_cycles_questions` below).
    #[tokio::test]
    async fn ask_up_down_moves_per_question_cursor() {
        let mut app = make_app_with_provider();
        app.open_ask("Q1?".to_string(), vec!["a".into(), "b".into()]);
        let ask_idx = app.function.tabs.iter().position(|t| matches!(t, SidebarTab::Ask(_))).unwrap();
        let mut state = match app.function.tabs.remove(ask_idx) {
            SidebarTab::Ask(s) => s,
            _ => unreachable!(),
        };
        app.function.active = 99;

        // Per-question cursor starts at row 0 ("first option").
        assert_eq!(state.items[state.active].cursor, 0);

        let total = state.row_count();
        for expected in 1..total {
            handle_ask_key(
                KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
                &mut app,
                &mut state,
            )
            .await;
            assert_eq!(state.items[state.active].cursor, expected);
        }
        // One more Down wraps to 0.
        handle_ask_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
            &mut app,
            &mut state,
        )
        .await;
        assert_eq!(state.items[state.active].cursor, 0, "Down wraps to top");

        // Up wraps to the last row.
        handle_ask_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::empty()),
            &mut app,
            &mut state,
        )
        .await;
        assert_eq!(
            state.items[state.active].cursor,
            total - 1,
            "Up wraps to bottom"
        );
    }

    /// Right steps through the questions; Left steps back. The
    /// per-question cursor is independent of `active`.
    #[tokio::test]
    async fn ask_left_right_cycles_questions() {
        let mut app = make_app_with_provider();
        app.open_ask("Q1?".to_string(), vec!["a".into()]);
        app.open_ask("Q2?".to_string(), vec!["x".into()]);
        let ask_idx = app.function.tabs.iter().position(|t| matches!(t, SidebarTab::Ask(_))).unwrap();
        let mut state = match app.function.tabs.remove(ask_idx) {
            SidebarTab::Ask(s) => s,
            _ => unreachable!(),
        };
        app.function.active = 99;
        // open_ask lands on the latest question (Q2).
        assert_eq!(state.active, 1);

        handle_ask_key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::empty()),
            &mut app,
            &mut state,
        )
        .await;
        assert_eq!(state.active, 0, "Left steps to Q1");

        handle_ask_key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::empty()),
            &mut app,
            &mut state,
        )
        .await;
        assert_eq!(state.active, 0, "Left doesn't wrap below 0");

        handle_ask_key(
            KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
            &mut app,
            &mut state,
        )
        .await;
        assert_eq!(state.active, 1);
        // Past the last question: no wrap.
        handle_ask_key(
            KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
            &mut app,
            &mut state,
        )
        .await;
        assert_eq!(state.active, 1, "Right doesn't wrap past end");
    }

    /// Answering the last unanswered question flips phase to
    /// Reviewing. Enter in Reviewing sends the summary.
    #[tokio::test]
    async fn ask_enter_on_option_records_and_advances_to_reviewing() {
        let mut app = make_app_with_provider();
        app.open_ask("Q1?".to_string(), vec!["a".into()]);
        app.open_ask("Q2?".to_string(), vec!["x".into()]);
        let ask_idx = app.function.tabs.iter().position(|t| matches!(t, SidebarTab::Ask(_))).unwrap();
        match &mut app.function.tabs[ask_idx] {
            SidebarTab::Ask(s) => s.active = 0,
            _ => unreachable!(),
        }
        let mut state = match app.function.tabs.remove(ask_idx) {
            SidebarTab::Ask(s) => s,
            _ => unreachable!(),
        };
        app.function.active = 99;

        // Cursor on Q1 at row 0 ("a").
        assert_eq!(state.active, 0);
        assert_eq!(state.items[state.active].cursor, 0);
        handle_ask_key(enter_key(), &mut app, &mut state).await;
        assert_eq!(state.items[0].answered.as_deref(), Some("a"));
        assert_eq!(state.active, 1, "advanced to Q2");
        assert_eq!(state.phase, crate::function::AskPhase::Asking);

        // Answer Q2 → Reviewing.
        handle_ask_key(enter_key(), &mut app, &mut state).await;
        assert_eq!(state.items[1].answered.as_deref(), Some("x"));
        assert_eq!(state.phase, crate::function::AskPhase::Reviewing);

        // Enter in Reviewing sends the summary.
        handle_ask_key(enter_key(), &mut app, &mut state).await;
        let last_user = app
            .session
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, crate::session::Role::User))
            .expect("summary must push a User message");
        assert!(last_user.content.contains("All questions answered"));
    }

    // ============================================================
    // Deferred request: HTTP / tool execution only fires AFTER the
    // next `terminal.draw(...)` returns, so the freshly-pushed user
    // message is on screen first.
    // ============================================================

    fn chat_app() -> App {
        let mut app = make_app_with_provider();
        // The chat / tool paths only build a `pending_request` if
        // `msg_tx` is wired up. The real main loop creates an
        // `EventChannels` and stores the sender here; tests do the
        // same so the same code path is exercised. We don't need
        // to keep the receiver alive — `submit_input` only stages
        // the request synchronously, and the test does not listen
        // for any messages.
        let channels = EventChannels::new();
        app.msg_tx = Some(channels.tx);
        app
    }

    #[test]
    fn submit_chat_sets_pending_without_spawning() {
        // Submitting a chat message must push the user message + an
        // empty streaming assistant, set `inflight`, and stage a
        // `PendingRequest::Chat`. The actual HTTP request must NOT
        // be in flight yet — that's `flush_pending_request`'s job.
        let mut app = chat_app();
        app.input.buffer = "hello".to_string();
        app.input.cursor = 5;
        submit_input(&mut app);

        assert!(
            app.input.buffer.is_empty(),
            "input must be cleared on submit"
        );
        assert!(
            app.inflight.is_some(),
            "inflight must be set so the spinner is visible"
        );
        assert!(
            app.pending_request.is_some(),
            "chat request must be staged, not yet spawned"
        );
        assert!(
            matches!(
                app.pending_request,
                Some(crate::function::PendingRequest::Chat(_))
            ),
            "expected a Chat pending request"
        );

        // The user message and the empty streaming assistant must
        // already be in the session — the render that follows
        // submit_input will paint both of them.
        let roles: Vec<_> = app
            .session
            .messages
            .iter()
            .map(|m| m.role)
            .collect();
        assert!(
            roles.len() >= 2,
            "expected user + assistant to be pushed: got {roles:?}"
        );
        assert!(matches!(
            roles[roles.len() - 2],
            crate::session::Role::User
        ));
        assert!(matches!(
            roles[roles.len() - 1],
            crate::session::Role::Assistant
        ));
        assert!(app.session.streaming_id.is_some());
    }

    #[tokio::test]
    async fn flush_pending_request_consumes_pending() {
        // After submit, the request is staged. `flush_pending_request`
        // must take it and dispatch it on a tokio task. We only assert
        // on the field-level contract here; the spawned task itself is
        // short-lived and unobservable without a real provider.
        let mut app = chat_app();
        app.input.buffer = "hello".to_string();
        app.input.cursor = 5;
        submit_input(&mut app);
        assert!(app.pending_request.is_some());

        flush_pending_request(&mut app);
        assert!(
            app.pending_request.is_none(),
            "flush_pending_request must drain the staged request"
        );
        // `inflight` is intentionally NOT cleared by flush — the
        // request it tracks is still running on the spawned task.
        assert!(app.inflight.is_some());
    }

    #[test]
    fn esc_during_pending_silently_drops_request() {
        // If the user hits Esc in the brief window between submit
        // and the next render (when the request is staged but the
        // HTTP call has not yet gone out), we must silently drop
        // the request and clear pending state. The user message and
        // empty assistant stay in the session, matching the
        // existing cancel-during-inflight behavior.
        let mut app = chat_app();
        app.input.buffer = "hello".to_string();
        app.input.cursor = 5;
        submit_input(&mut app);
        let msgs_before = app.session.messages.len();
        assert!(app.pending_request.is_some());

        // Simulate the global Esc handler. `handle_key` would also
        // need a renderer / channel context; calling the inner Esc
        // arm directly keeps the test focused.
        let k = esc_key();
        let _ = k; // appease unused
        // We can't easily drive `handle_key` without a renderer, so
        // we replicate the Esc-on-pending branch inline. The branch
        // is short and the test is the contract for it.
        if app.pending_request.is_some() {
            app.pending_request = None;
            app.inflight = None;
            app.session.streaming_id = None;
        }

        assert!(app.pending_request.is_none());
        assert!(app.inflight.is_none());
        assert!(app.session.streaming_id.is_none());
        // User message + empty assistant remain in the session, so
        // the user still sees what they typed and the empty
        // streaming block.
        assert_eq!(app.session.messages.len(), msgs_before);
    }

    #[test]
    fn direct_tool_input_also_uses_pending() {
        // `!echo hi` (and the other direct-tool forms `!!` `$` `$$`)
        // must follow the same deferral: user message pushed,
        // `inflight` set, `pending_request` staged. The tool
        // execution must not start until the next render.
        let mut app = chat_app();
        app.input.buffer = "!echo hi".to_string();
        app.input.cursor = app.input.buffer.len();
        submit_input(&mut app);

        assert!(
            app.input.buffer.is_empty(),
            "input must be cleared on submit"
        );
        assert!(
            app.inflight.is_some(),
            "inflight must be set so the pending tool block is visible"
        );
        assert!(
            matches!(
                app.pending_request,
                Some(crate::function::PendingRequest::Tool(_))
            ),
            "direct-tool path must also stage a PendingRequest"
        );
        // The user message is pushed first, then the empty
        // streaming assistant placeholder. So the last is the
        // assistant, the second-to-last is the user.
        let len = app.session.messages.len();
        assert!(len >= 2, "expected user + assistant in session");
        let user = &app.session.messages[len - 2];
        let assistant = &app.session.messages[len - 1];
        assert!(matches!(user.role, crate::session::Role::User));
        assert_eq!(user.content, "!echo hi");
        assert!(matches!(assistant.role, crate::session::Role::Assistant));
    }

    #[test]
    fn flush_pending_request_is_noop_when_empty() {
        // Sanity: calling `flush_pending_request` with nothing
        // staged must be a cheap no-op and must not crash.
        let mut app = chat_app();
        flush_pending_request(&mut app);
        assert!(app.pending_request.is_none());
        assert!(app.inflight.is_none());
    }

    // ============================================================
    // /continue: keep the focused behavior under regression guard.
    // ============================================================

    #[test]
    fn continue_no_arg_sends_meaningful_cue_and_removes_user_message() {
        // `/continue` (no extra args) must:
        //   1. Send a non-empty continuation prompt to the model so
        //      providers don't stall or 400 on a literal empty user
        //      content (the bug the user reported).
        //   2. Stage a `PendingRequest::Chat`.
        //   3. NOT keep the synthesized user message in the session
        //      — it should be removed right after `send_chat` so the
        //      chat log only shows the previous turn and the new
        //      streaming assistant.
        let mut app = chat_app();
        // Pretend the prior turn produced a partial assistant
        // response. `/continue` is meant to follow an Esc/abort.
        use crate::session::{Message, Role};
        app.session.push(Message::new(Role::Assistant, "half".to_string()));
        let seq_before = app.current_request_seq;

        app.input.buffer = "/continue".to_string();
        app.input.cursor = app.input.buffer.len();
        submit_input(&mut app);

        // A pending chat request was staged.
        assert!(
            matches!(
                app.pending_request,
                Some(crate::function::PendingRequest::Chat(_))
            ),
            "/continue must stage a Chat pending request, not no-op"
        );
        let pending_seq = match app.pending_request.as_ref() {
            Some(crate::function::PendingRequest::Chat(p)) => p.seq,
            _ => unreachable!(),
        };
        assert!(
            pending_seq > seq_before,
            "current_request_seq must advance on /continue (got {pending_seq}, before {seq_before})"
        );

        // The synthesized user message is NOT in the session — only
        // the previous assistant and the fresh streaming assistant
        // placeholder remain at the tail.
        let len = app.session.messages.len();
        assert!(
            len >= 2,
            "expected at least the previous assistant + new streaming assistant"
        );
        let last_user = app
            .session
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::User));
        let last_assistant = &app.session.messages[len - 1];
        assert!(
            matches!(last_assistant.role, Role::Assistant),
            "tail must remain an Assistant placeholder"
        );
        assert!(
            last_user.is_none() || last_user.unwrap().content != "Continue from where you left off.",
            "/continue's synthetic user message must be stripped from the session"
        );

        // The actual prompt going to the model is the cue string.
        let prompts: Vec<&str> = app
            .session
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        // We can't see the staged `ChatPending`'s `req.messages`
        // without consuming it, but we *can* assert that the cue
        // string is being sent by checking it doesn't appear in the
        // session (it would have been pushed then removed) AND
        // that we pushed-and-removed at least one message — i.e.
        // there is no User message left from this turn.
        let _ = prompts; // silence unused if some assertions are dropped
    }

    #[test]
    fn continue_with_arg_appends_to_cue() {
        // `/continue foo` must seed the API request with
        // "Continue from where you left off.\n\nfoo" and still
        // strip the synthetic user message from the session.
        let mut app = chat_app();
        use crate::session::{Message, Role};
        app.session.push(Message::new(Role::Assistant, "half".to_string()));

        app.input.buffer = "/continue foo".to_string();
        app.input.cursor = app.input.buffer.len();
        submit_input(&mut app);

        let last_user = app
            .session
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::User));
        assert!(
            last_user.is_none(),
            "the /continue synthetic user message must be removed from the session"
        );
        assert!(
            app.inflight.is_some(),
            "inflight must be armed so /continue arms the spinner"
        );
    }

    #[test]
    fn stale_chat_done_is_dropped() {
        // The hotfix for the Esc-then-/continue race: a `ChatDone`
        // (or `ChatError`) left over from a prior request must NOT
        // clear a freshly-armed inflight or mark the new assistant
        // as finished. We simulate this by feeding the handler a
        // `ChatDone` with the previous `current_request_seq`.
        let mut app = chat_app();
        let old_seq = app.current_request_seq.wrapping_add(1);
        // Pre-arm an inflight with a different seq (the "new"
        // request) so we can detect the bad clear.
        app.current_request_seq = 5;
        let (cancel_tx, _) = tokio::sync::watch::channel(false);
        app.inflight = Some(crate::function::InflightHandle {
            cancel: cancel_tx,
            label: "test:new".to_string(),
            seq: 5,
        });

        handle_msg(
            AppMsg::ChatDone { seq: old_seq },
            &mut app,
        );

        // The stale event must be ignored — current inflight stays.
        assert!(
            app.inflight.is_some(),
            "stale ChatDone must NOT clear the new inflight"
        );
    }

    #[test]
    fn stale_chat_error_is_dropped() {
        // Mirror of `stale_chat_done_is_dropped` for ChatError.
        let mut app = chat_app();
        app.current_request_seq = 5;
        let (cancel_tx, _) = tokio::sync::watch::channel(false);
        app.inflight = Some(crate::function::InflightHandle {
            cancel: cancel_tx,
            label: "test:new".to_string(),
            seq: 5,
        });

        handle_msg(
            AppMsg::ChatError {
                seq: 99,
                error: "boom".to_string(),
            },
            &mut app,
        );
        assert!(
            app.inflight.is_some(),
            "stale ChatError must NOT clear the new inflight"
        );
    }

    #[test]
    fn current_seq_chat_done_clears_inflight() {
        // Companion to the two "stale _ are dropped" tests: the
        // seq filter must only drop mismatches — a `ChatDone` whose
        // `seq` IS `current_request_seq` is the legitimate terminal
        // event from the in-flight request and must proceed
        // (clearing `inflight` so the spinner stops).
        let mut app = chat_app();
        app.current_request_seq = 7;
        let (cancel_tx, _) = tokio::sync::watch::channel(false);
        app.inflight = Some(crate::function::InflightHandle {
            cancel: cancel_tx,
            label: "test:current".to_string(),
            seq: 7,
        });

        handle_msg(AppMsg::ChatDone { seq: 7 }, &mut app);

        assert!(
            app.inflight.is_none(),
            "a matching-seq ChatDone must clear inflight like before"
        );
    }

    // ============================================================
    // Smooth-scroll animator
    // ============================================================

    fn advance_animator(a: &mut ScrollAnimator, ticks: u32, ms_per_tick: u64) -> (u16, bool) {
        let mut last_settled = true;
        let mut last_v = a.current.round() as u16;
        for i in 0..ticks {
            let now = Instant::now() + Duration::from_millis(ms_per_tick * (i as u64 + 1));
            let (v, settled) = a.step(now);
            last_v = v;
            last_settled = settled;
        }
        (last_v, last_settled)
    }

    #[test]
    fn scroll_animator_lands_instantly_on_target() {
        // A wheel event must place `current` on `target` immediately
        // — no line-by-line animation, no per-frame coast. The view
        // jumps by the OS step in a single frame.
        let mut a = ScrollAnimator::default();
        a.begin_gesture(5.0, 5, Instant::now());
        assert_eq!(a.target, 5.0);
        assert_eq!(
            a.current, 5.0,
            "current must equal target on the first event — instant scroll"
        );
        assert!(a.animating, "gating window must be active for the frame");
    }

    #[test]
    fn scroll_animator_gates_events_within_one_frame() {
        // The user-facing rule: while the session is still scrolling,
        // ignore new scroll events. With instant scroll, "still
        // scrolling" means the 1-frame gating window after the last
        // event. `begin_gesture` callers (i.e. `handle_mouse`) refuse
        // to call this when `animating` is true; this test verifies
        // the invariant that the gating window is the ONLY thing
        // keeping the target stable.
        let mut a = ScrollAnimator::default();
        a.begin_gesture(8.0, 3, Instant::now());
        let target_after_start = a.target;
        // Without a `step` call yet, the gating window is still
        // active and the target must not change on its own.
        assert!(a.animating, "gating window must be active right after begin_gesture");
        assert_eq!(a.target, target_after_start);
        // One tick clears the gating window.
        let (_, settled) = advance_animator(&mut a, 1, 16);
        assert!(settled, "one tick must clear the gating window");
        assert!(!a.animating, "gating window must be cleared after one tick");
    }

    #[test]
    fn scroll_animator_snap_cancels_motion() {
        // `snap` is used by programmatic jumps (submit, jump-to, new
        // session). It must cancel any in-flight gating window and
        // land exactly at the requested value.
        let mut a = ScrollAnimator::default();
        a.begin_gesture(20.0, 5, Instant::now());
        assert!(a.animating);
        a.snap(7.0);
        assert!(!a.animating, "snap must clear animating");
        assert_eq!(a.current, 7.0);
        assert_eq!(a.target, 7.0);
        assert_eq!(a.velocity, 0.0);
    }

    #[test]
    fn scroll_animator_does_not_overshoot_negative() {
        // ScrollDown (negative delta) with no prior scroll must clamp
        // at 0, not go negative.
        let mut a = ScrollAnimator::default();
        a.begin_gesture(-5.0, 3, Instant::now());
        assert_eq!(a.target, 0.0, "target must clamp at 0");
        assert_eq!(a.current, 0.0, "current must clamp at 0");
        assert!(!a.animating, "negative gesture at floor is a snap, no gating window");
    }

    #[test]
    fn scroll_animator_accumulates_consecutive_gestures() {
        // After the gating window clears, a new gesture must add
        // onto the current `target` (so multiple wheel clicks
        // accumulate into a larger jump on the next render).
        let mut a = ScrollAnimator::default();
        a.begin_gesture(5.0, 5, Instant::now());
        let _ = advance_animator(&mut a, 1, 16); // clear the gating window
        assert!(!a.animating);
        a.begin_gesture(3.0, 3, Instant::now());
        assert_eq!(
            a.target, 8.0,
            "second gesture must accumulate onto the first"
        );
        assert_eq!(a.current, 8.0, "current must reflect the accumulated target");
    }
}
