use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use futures_util::StreamExt;
use ratatui::backend::Backend;
use ratatui::Terminal;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::interval;

use crate::app::App;

/// Keep the terminal cursor hidden after every frame — the TUI draws its
/// own styled block cursor.  (No IME position logic; we are in the alternate
/// screen buffer where TSF-based IMEs cannot follow the cursor anyway.)
fn hide_cursor() {
    use std::io::Write;
    let _ = write!(std::io::stdout(), "\x1B[?25l");
    let _ = std::io::stdout().flush();
}

/// Async -> main loop messages.
pub enum AppMsg {
    /// A piece of streamed chat delta arrived.
    ChatDelta(String),
    /// A piece of thinking delta (Anthropic "thinking_delta") arrived.
    ChatThinkingDelta(String),
    /// Final usage arrived for a completed stream.
    ChatUsage(crate::providers::Usage),
    /// Stream finished successfully.
    ChatDone,
    /// Stream errored.
    ChatError(String),
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

pub async fn run<B>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()>
where
    B: Backend,
{
    let mut channels = EventChannels::new();
    // We need to put the sender into the App so spawned tasks can use it.
    app.msg_tx = Some(channels.tx.clone());
    app.check_config();

    let mut events = EventStream::new();
    let mut tick = interval(Duration::from_millis(100));
    let mut last_status_refresh = std::time::Instant::now();

    loop {
        // redraw
        if let Err(e) = terminal.draw(|f| crate::ui::render(f, app)) {
            let _ = e;
        }

        hide_cursor();

        tokio::select! {
            biased;
            evt = events.next() => {
                let Some(evt) = evt else { break; };
                match evt? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        handle_key(k, app).await;
                    }
                    Event::Mouse(m) => {
                        handle_mouse(m, app);
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
            msg = channels.rx.recv() => {
                if let Some(m) = msg { handle_msg(m, app); }
            }
            _ = tick.tick() => {
                // Advance the streaming display cursor so characters
                // appear one-by-one rather than in API-chunk bursts.
                if let Some(id) = app.session.streaming_id {
                    if let Some(m) = app.session.messages.get_mut(id) {
                        if m.streaming && m.display_cursor < m.content.len() {
                            m.display_cursor = (m.display_cursor + 15).min(m.content.len());
                        }
                    }
                }
                if last_status_refresh.elapsed() >= Duration::from_millis(500) {
                    app.status.update_hit(&app.hit_rate);
                    last_status_refresh = std::time::Instant::now();
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

fn handle_msg(msg: AppMsg, app: &mut App) {
    match msg {
        AppMsg::ChatDelta(s) => app.session.append_to_last(&s),
        AppMsg::ChatThinkingDelta(s) => app.session.append_thinking_to_last(&s),
        AppMsg::ChatUsage(u) => {
            let denom = u.input_tokens + u.cache_read_tokens;
            let rate = if denom == 0 { 0.0 } else { u.cache_read_tokens as f64 / denom as f64 };
            app.hit_rate.record(rate);
            app.status.update_hit(&app.hit_rate);
        }
        AppMsg::ChatDone => {
            app.session.finish_streaming();
            app.inflight = None;
            use crate::function::notifications::ToastLevel;
            app.notify(ToastLevel::Ok, "response complete");
        }
        AppMsg::ChatError(e) => {
            app.session.finish_streaming();
            app.inflight = None;
            use crate::function::notifications::ToastLevel;
            app.notify(ToastLevel::Fail, e.clone());
            app.session.push(crate::session::Message::new(
                crate::session::Role::System,
                format!("[request failed: {e}]"),
            ));
        }
        AppMsg::ModelsFetched { provider, base_url, api_key, models } => {
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
            app.model_cache.put(provider, base_url, api_key, models.clone());
            app.model_cache.save(&app.model_cache_path);
            use crate::function::notifications::ToastLevel;
            app.notify(ToastLevel::Ok, format!("fetched {} models for {}", models.len(), provider.as_str()));
        }
        AppMsg::ModelsFetchFailed { provider, error, no_endpoint } => {
            if let Some(crate::function::SidebarTab::ModelPicker(s)) = app
                .function
                .tabs
                .iter_mut()
                .find(|t| matches!(t, crate::function::SidebarTab::ModelPicker(_)))
            {
                s.fetching = false;
                s.fetch_error = Some(if no_endpoint {
                    "[no /v1/models endpoint at this base_url]".to_string()
                } else {
                    error.clone()
                });
                s.no_endpoint = no_endpoint;
            }
            use crate::function::notifications::ToastLevel;
            app.notify(ToastLevel::Fail, if no_endpoint {
                "base_url has no /v1/models; use Manual id".to_string()
            } else {
                format!("fetch models for {}: {}", provider.as_str(), error)
            });
        }
    }
}

async fn handle_key(k: crossterm::event::KeyEvent, app: &mut App) {
    use crossterm::event::{KeyCode, KeyModifiers};

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
                    app.notify(ToastLevel::Ok, format!("copied {} chars to clipboard", text.chars().count()));
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
        }
        return;
    }

    // Ctrl+I: focus input. Closes any active sidebar tab (returns to chat).
    if ctrl && matches!(k.code, KeyCode::Char('i') | KeyCode::Char('I')) {
        app.function.tabs.retain(|t| matches!(t, crate::function::SidebarTab::Notifications));
        app.function.active = 0;
        return;
    }

    // Ctrl+L clears session
    if ctrl && matches!(k.code, KeyCode::Char('l') | KeyCode::Char('L')) {
        app.session.clear();
        use crate::function::notifications::ToastLevel;
        app.notify(ToastLevel::Info, "session cleared");
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

    // If a sidebar tab is open, give it a chance to handle the key first.
    if dispatch_to_active_tab(k, app) {
        return;
    }

    match k.code {
        KeyCode::Esc => {
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
                }
            } else {
                // A function tab was closed. If it was the last non-
                // Notification tab, hide the panel so we return to the
                // default state.
                app.maybe_hide_panel();
            }
        }
        KeyCode::Tab => {
            // Tab cycles sidebar tabs forward. The Settings form and
            // ModelPicker each consume Tab themselves (for field-navigation
            // and search/list toggle), so they never reach here.
            cycle_sidebar_forward(app);
        }
        KeyCode::BackTab => {
            cycle_sidebar_tab_back(app);
        }
        KeyCode::Enter => {
            // If the completion tab is showing for a partial command, complete
            // the buffer with the focused candidate, then submit.
            if completion_is_focused(app) {
                if let Some(idx) = completion_idx(app) {
                    if let crate::function::SidebarTab::Completion(s) =
                        &app.function.tabs[idx]
                    {
                        if let Some(cand) = s.candidates.get(s.cursor).cloned() {
                            app.input.buffer = cand;
                            app.input.cursor = app.input.buffer.len();
                            app.input.clear_selection();
                        }
                    }
                }
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
                }
                app.input.insert_newline();
                app.sync_completion();
            } else {
                submit_input(app);
            }
        }
        KeyCode::Backspace => {
            if !app.input.delete_selection() {
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
            // Ctrl+Shift+Up is a terminal viewport-scroll command on
            // Windows Terminal — intercept it here if it arrives so it
            // does not also trigger history navigation.
            if k.modifiers.contains(KeyModifiers::CONTROL)
                && k.modifiers.contains(KeyModifiers::SHIFT)
            {
                return;
            }
            if completion_is_focused(app) {
                if let Some(idx) = completion_idx(app) {
                    if let crate::function::SidebarTab::Completion(s) =
                        &mut app.function.tabs[idx]
                    {
                        s.move_up();
                    }
                }
            } else if !app.input.move_up_line() {
                app.input.history_prev();
            }
        }
        KeyCode::Down => {
            if k.modifiers.contains(KeyModifiers::CONTROL)
                && k.modifiers.contains(KeyModifiers::SHIFT)
            {
                return;
            }
            if completion_is_focused(app) {
                if let Some(idx) = completion_idx(app) {
                    if let crate::function::SidebarTab::Completion(s) =
                        &mut app.function.tabs[idx]
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

fn cycle_sidebar_tab_back(app: &mut App) {
    if app.function.tabs.is_empty() {
        return;
    }
    app.function.active = (app.function.active + app.function.tabs.len() - 1) % app.function.tabs.len();
    if app.function_visible {
        app.acknowledge_panel();
    }
}

/// Returns the index of the Completion sidebar tab, if any.
fn completion_idx(app: &App) -> Option<usize> {
    app.function
        .tabs
        .iter()
        .position(|t| matches!(t, crate::function::SidebarTab::Completion(_)))
}

/// True if the Completion tab is present and the input buffer is a partial
/// command (still accepting more characters). In this state Up/Down navigate
/// candidates and Enter completes the focused one.
fn completion_is_focused(app: &App) -> bool {
    if !app.input.is_command() {
        return false;
    }
    if app.input.buffer.contains(' ') {
        return false;
    }
    completion_idx(app).is_some()
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

fn handle_mouse(m: MouseEvent, app: &mut App) {
    let prompt = app.input_prompt_area;
    let prefix_width = unicode_width::UnicodeWidthStr::width(" > ") as u16;
    let in_prompt_row = prompt.map(|r| m.row == r.y).unwrap_or(false);

    // Mouse wheel scroll — scroll the session content.
    // scroll = offset from bottom.  ScrollUp = see older content (increase
    // offset).  ScrollDown = see newer content (decrease offset).
    if matches!(m.kind, MouseEventKind::ScrollUp) {
        app.session.scroll = app.session.scroll.saturating_add(3);
        // Clamp so the stored value never drifts beyond the actual
        // maximum, preventing a multi-press dead-zone when reversing.
        if let Some(area) = app.session_area {
            let inner_h = area.height.saturating_sub(2);
            let total = app.session.count_all_lines();
            let max_scroll = total.saturating_sub(inner_h);
            app.session.scroll = app.session.scroll.min(max_scroll);
        }
        return;
    }
    if matches!(m.kind, MouseEventKind::ScrollDown) {
        app.session.scroll = app.session.scroll.saturating_sub(3);
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
        _ => {}
    }
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
    let raw = app.input.take();
    if raw.is_empty() {
        return;
    }
    if let Some(rest) = raw.strip_prefix('/') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let cmd: String = parts.next().unwrap_or("").to_lowercase();
        let arg: String = parts.next().unwrap_or("").trim().to_string();
        crate::commands::dispatch(app, &cmd, &arg);
    } else {
        crate::commands::send_chat(app, raw);
    }
    // The buffer is now empty, so the completion tab (if any) should close.
    app.sync_completion();
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
            if shift { EnterAction::Newline } else { EnterAction::Send }
        }
        // "Enter newline / Shift+Enter sends":
        //   plain Enter inserts a newline, Shift+Enter submits.
        EnterBehavior::EnterNewline => {
            if shift { EnterAction::Send } else { EnterAction::Newline }
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
fn dispatch_to_active_tab(k: crossterm::event::KeyEvent, app: &mut App) -> bool {
    let active = app.function.active;
    if active >= app.function.tabs.len() {
        return false;
    }
    let mut tab = std::mem::replace(
        &mut app.function.tabs[active],
        crate::function::SidebarTab::Notifications,
    );
    let consumed = match &mut tab {
        crate::function::SidebarTab::ModelPicker(state) => handle_picker_key(k, app, state),
        crate::function::SidebarTab::ProviderPicker(state) => handle_provider_picker_key(k, app, state),
        crate::function::SidebarTab::Settings(state) => handle_settings_key(k, app, state),
        crate::function::SidebarTab::ThinkingPicker(state) => handle_thinking_key(k, app, state),
        crate::function::SidebarTab::TimelinePicker(state) => handle_timeline_key(k, app, state),
        _ => false,
    };
    if active < app.function.tabs.len()
        && matches!(
            app.function.tabs[active],
            crate::function::SidebarTab::Notifications
        )
    {
        app.function.tabs[active] = tab;
    }
    consumed
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
    let open_model_picker_for_selected = |app: &mut App, state: &crate::function::ProviderPickerState| {
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
                } else {
                    // At the top: jump back to the search box.
                    state.focus = crate::function::PickerFocus::Search;
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
                // When the filter yields exactly one model, select it
                // directly — the user probably just typed enough of the
                // name to narrow the list and wants that hit. Otherwise
                // fall through to the manual-commit path (e.g. entering
                // a model id that does not match any cached entry).
                if state.filtered.len() == 1 {
                    let idx = state.filtered[0];
                    let id = state.models[idx].id.clone();
                    commit_model(_app, state.provider, id, false);
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
                } else {
                    // At the top of the list: go back to Search so the
                    // user can continue typing a filter.
                    state.focus = crate::function::PickerFocus::Search;
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
                    let id = state.models[idx].id.clone();
                    commit_model(_app, state.provider, id, false);
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
                    "low" => ReasoningMode::Low,
                    "med" => ReasoningMode::Med,
                    "high" => ReasoningMode::High,
                    "adaptive" => ReasoningMode::Adaptive,
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
                } else {
                    // At the top: jump back to the search box.
                    state.focus = crate::function::PickerFocus::Search;
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

/// Jump the session scroll to the focused message and close the
/// timeline picker tab.
fn commit_timeline_jump(app: &mut App, state: &crate::function::TimelinePickerState) {
    use crate::function::notifications::ToastLevel;
    let Some(msg_idx) = state.selected_msg_idx() else {
        return;
    };
    let viewport_h = app.session_area.map(|r| r.height).unwrap_or(20);
    app.session.jump_to_message(msg_idx, viewport_h);
    let active = app.function.active;
    if active < app.function.tabs.len() {
        app.function.tabs.remove(active);
        if app.function.active >= app.function.tabs.len() {
            app.function.active = app.function.tabs.len().saturating_sub(1);
        }
    }
    app.maybe_hide_panel();
    app.notify(ToastLevel::Info, format!("jumped to message #{}", msg_idx + 1));
}

fn trigger_picker_fetch(app: &mut App, state: &mut crate::function::ModelPickerState) {
    let p = state.provider;
    let active_id = match app.config.active.as_ref() {
        Some(id) => id.clone(),
        None => {
            use crate::function::notifications::ToastLevel;
            app.notify(ToastLevel::Fail, "no active provider; configure one in /settings");
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
        let client = app.reqwest.clone();
        tokio::spawn(async move {
            match crate::providers::list_models(&client, p, &base, &key).await {
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
        crate::function::SettingsLevel::ConfigForm(form) => {
            handle_form_text(k, ctrl, form)
        }
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
    use crate::function::{ConfigField, SettingsLevel};
    if let SettingsLevel::ConfigForm(form) = &mut state.level {
        form.focused = match state.cursor {
            0 => ConfigField::Name,
            1 => ConfigField::BaseUrl,
            2 => ConfigField::KeyOrEnv,
            3 => ConfigField::Save,
            _ => ConfigField::Exit,
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
            app.function.push(crate::function::SidebarTab::Notifications);
        }
        app.function_visible = true;
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
        SettingsLevel::ConfigForm(_) | SettingsLevel::NewProviderKind | SettingsLevel::ExistingActions(_) => {
            state.level = SettingsLevel::ProviderList;
            state.cursor = 0;
            state.clamp_cursor(&app.config);
        }
        SettingsLevel::ThinkingDisplayList | SettingsLevel::EnterBehaviorList => {
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
            _ => SettingsLevel::EnterBehaviorList,
        },
        SettingsLevel::ProviderList => {
            if cursor == 0 {
                SettingsLevel::NewProviderKind
            } else {
                let mut keys: Vec<String> = app.config.entries.keys().cloned().collect();
                keys.sort();
                match keys.get(cursor - 1) {
                    Some(id) => SettingsLevel::ExistingActions(id.clone()),
                    None => SettingsLevel::ProviderList,
                }
            }
        }
        SettingsLevel::NewProviderKind => {
            let ids = crate::config::Config::all_possible_ids();
            match ids.get(cursor).and_then(|id| parse_id(id).map(|(k, m)| (id, k, m))) {
                Some((id, kind, mode)) => {
                    if app.config.entries.contains_key(id) {
                        use crate::function::notifications::ToastLevel;
                        app.notify(
                            ToastLevel::Warn,
                            format!("{id} already exists; editing instead"),
                        );
                        let cfg = app.config.entry(id).cloned().unwrap_or_default();
                        SettingsLevel::ConfigForm(
                            crate::function::ConfigFormState::new_for_edit(id.clone(), &cfg, mode),
                        )
                    } else {
                        SettingsLevel::ConfigForm(
                            crate::function::ConfigFormState::new_for_create(kind, mode),
                        )
                    }
                }
                None => SettingsLevel::NewProviderKind,
            }
        }
        SettingsLevel::ExistingActions(id) => {
            if cursor == 0 {
                // edit
                if let Some((_kind, mode)) = parse_id(&id) {
                    let cfg = app.config.entry(&id).cloned().unwrap_or_default();
                    SettingsLevel::ConfigForm(
                        crate::function::ConfigFormState::new_for_edit(id, &cfg, mode),
                    )
                } else {
                    SettingsLevel::ProviderList
                }
            } else {
                // delete
                if let Some(cfg) = app.config.entry(&id).cloned() {
                    app.config.entries.remove(&id);
                    if app.config.active.as_deref() == Some(id.as_str()) {
                        app.config.active = app.config.entries.keys().next().cloned();
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
                    }
                }
                SettingsLevel::ProviderList
            }
        }
        SettingsLevel::ThinkingDisplayList => {
            use crate::config::ThinkingDisplay;
            use crate::function::notifications::ToastLevel;
            let modes = [ThinkingDisplay::Show, ThinkingDisplay::Hide, ThinkingDisplay::ShowWhileStreaming];
            if let Some(&mode) = modes.get(cursor) {
                app.config.thinking_display = mode;
                app.save_config();
                app.notify(ToastLevel::Ok, format!("thinking display: {}", mode.as_str()));
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
        SettingsLevel::ConfigForm(form) => {
            match form.focused {
                ConfigField::Name | ConfigField::BaseUrl | ConfigField::KeyOrEnv => {
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

    let id = form.id.clone();
    let (kind, mode) = parse_id(&id).unwrap_or((ProviderKind::Openai, ProviderMode::Key));
    let base_url = form.base_url.trim().to_string();
    let key_or_env = form.key_or_env.clone();
    let was_new = form.is_new;

    // Preserve existing model and api_key (for Key mode in edit form) if
    // the user did not touch the corresponding field. We always pull the
    // current entry from config, then overwrite with form values.
    let existing = app.config.entry(&id).cloned();
    let model = if let Some(c) = existing.as_ref() {
        if c.model.is_empty() {
            match kind {
                ProviderKind::Openai => "gpt-4o-mini".to_string(),
                ProviderKind::Anthropic => "claude-3-5-sonnet-latest".to_string(),
            }
        } else {
            c.model.clone()
        }
    } else {
        match kind {
            ProviderKind::Openai => "gpt-4o-mini".to_string(),
            ProviderKind::Anthropic => "claude-3-5-sonnet-latest".to_string(),
        }
    };

    let mut new_cfg = crate::config::ProviderConfig {
        api_key: existing
            .as_ref()
            .map(|c| c.api_key.clone())
            .unwrap_or_default(),
        api_key_env: existing
            .as_ref()
            .map(|c| c.api_key_env.clone())
            .unwrap_or_default(),
        base_url,
        model,
        name: String::new(),
    };
    // For new entries, use the form's name directly.
    // For edit entries, the user may have set a custom name.
    new_cfg.name = form.name.trim().to_string();
    // For new entries, use the form's key/env directly.
    // For edit entries, use the form's value only if the user actually
    // modified it (otherwise the masked placeholder would clobber the
    // real key on save).
    let apply_form_value = was_new
        || (mode == ProviderMode::Key && form.key_modified)
        || mode == ProviderMode::Env;
    if apply_form_value {
        match mode {
            ProviderMode::Key => new_cfg.api_key = key_or_env,
            ProviderMode::Env => new_cfg.api_key_env = key_or_env,
        }
    }

    app.config.entries.insert(id.clone(), new_cfg);
    app.config.active = Some(id.clone());

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

    // Open the model picker and trigger a fetch so the user can pick one
    // of the remote models right after configuring a provider.
    if let Some(k) = app.config.active_kind() {
        let mut state = crate::function::ModelPickerState::new(k);
        state.fetching = true;
        app.function.push(crate::function::SidebarTab::ModelPicker(state));
        app.function_visible = true;
        app.acknowledge_panel();

        let active_id = match app.config.active.clone() {
            Some(id) => id,
            None => return,
        };
        if let Err(e) = app.config.validate_provider(&active_id) {
            app.notify(ToastLevel::Fail, e);
            return;
        }
        let base = app
            .config
            .entry(&active_id)
            .map(|c| c.base_url.clone())
            .unwrap_or_default();
        let key = app.config.effective_api_key(&active_id).unwrap_or_default();
        let client = app.reqwest.clone();
        if let Some(tx) = app.msg_tx.clone() {
            tokio::spawn(async move {
                match crate::providers::list_models(&client, k, &base, &key).await {
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
        // Tab cycles fields within the form.
        form.focused = match form.focused {
            ConfigField::Name => ConfigField::BaseUrl,
            ConfigField::BaseUrl => ConfigField::KeyOrEnv,
            ConfigField::KeyOrEnv => ConfigField::Save,
            ConfigField::Save => ConfigField::Exit,
            ConfigField::Exit => ConfigField::Name,
        };
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
        ConfigField::KeyOrEnv => {
            // First edit on the api_key field clears the saved (masked)
            // value so the user can type a new key. If they don't touch
            // the field, the original is preserved on save.
            if !form.key_modified && !form.key_or_env.is_empty() {
                form.key_or_env.clear();
            }
            form.key_modified = true;
            match k.code {
                crossterm::event::KeyCode::Char(c) => {
                    form.key_or_env.push(c);
                    true
                }
                crossterm::event::KeyCode::Backspace => {
                    form.key_or_env.pop();
                    true
                }
                _ => false,
            }
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
        Some(id) if parse_id(id).map(|(k, _)| k == provider).unwrap_or(false) => Some(id.to_string()),
        Some(_) | None => app
            .config
            .entries
            .keys()
            .find(|id| parse_id(id).map(|(k2, _)| k2 == provider).unwrap_or(false))
            .cloned(),
    };

    // 2. Update the target entry's model and make it active.
    if let Some(id) = target_id {
        app.config.active = Some(id.clone());
        if let Some(entry) = app.config.entry_mut(&id) {
            entry.model = model_id.clone();
        }
    }

    // 3. Refresh the status bar and persist to disk.
    app.status.set_provider_name(&app.config.active_name());
    app.status.set_model(&app.config.active_model_display());
    app.save_config();

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

    // 5. Toast.
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
    use crate::config::{Config, ProviderConfig, ProviderId, ProviderKind, ProviderMode, make_id};
    use crate::function::{FunctionPanel, ModelPickerState};
    use crate::function::notifications::Notifications;
    use crate::function::SidebarTab;

    fn make_app() -> App {
        let cfg = Config::default();
        // Use a per-test config file so parallel `cargo test` invocations
        // do not race on the same path. The atomic counter is process-wide
        // and yields a unique id for every call to `make_app`.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir()
            .join(format!("fish-coding-agent-test-{id}.json"));
        let _ = std::fs::remove_file(&tmp);
        let cache_file = tmp.parent().unwrap_or(&tmp).join("model-cache.json");
        App {
            config: cfg,
            config_path: tmp,
            session: crate::session::Session::default(),
            function: FunctionPanel::new(),
            input: crate::input::InputState::new(),
            status: crate::input::status::StatusBar::new(),
            function_visible: false,
            pending_events: 0,
            notifications: Notifications::default(),
            model_cache: crate::function::notifications::ModelCache::default(),
            hit_rate: crate::function::notifications::HitRate::new(50),
            reqwest: reqwest::Client::new(),
            inflight: None,
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            should_quit: false,
            msg_tx: None,
            input_prompt_area: None,
            tui_selection: None,
            selected_text: None,
            tui_drag_start: None,
            model_cache_path: cache_file,
            thinking_toggle_rows: Vec::new(),
            session_area: None,
            input_cursor_screen: None,
            function_panel_cursor: None,
        }
    }

    #[test]
    fn settings_save_form_creates_new_entry() {
        let mut app = make_app();
        let form = crate::function::ConfigFormState::new_for_create(
            ProviderKind::Openai,
            ProviderMode::Key,
        );
        // form starts pre-populated with default base_url and empty key.
        settings_save_form(&mut app, form.clone());
        let id: ProviderId = make_id(ProviderKind::Openai, ProviderMode::Key);
        assert!(app.config.entries.contains_key(&id));
        assert_eq!(app.config.active.as_deref(), Some(id.as_str()));
        let entry = app.config.entry(&id).unwrap();
        assert_eq!(entry.base_url, "https://api.openai.com/v1");
        assert_eq!(entry.model, "gpt-4o-mini");
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
                name: String::new(),
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
        form.key_or_env = "CUSTOM_ENV".to_string();
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
        commit_model(&mut app, ProviderKind::Anthropic, "claude-3-5-sonnet-latest".to_string(), false);
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
                assert_eq!(f.focused, ConfigField::BaseUrl, "Enter on BaseUrl must not auto-advance");
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
        assert!(app.function.tabs.iter().any(|t| matches!(t, SidebarTab::ModelPicker(_))));

        commit_model(&mut app, ProviderKind::Openai, "gpt-4o".to_string(), false);

        // Picker tab should be gone.
        assert!(!app.function.tabs.iter().any(|t| matches!(t, SidebarTab::ModelPicker(_))));
        // Active entry's model updated.
        let entry = app.config.entry(&id).unwrap();
        assert_eq!(entry.model, "gpt-4o");
        // Either the panel is empty (active 0, len 0) or active is in
        // bounds. With the new design no tab is permanent.
        if !app.function.tabs.is_empty() {
            assert!(app.function.active < app.function.tabs.len());
        }
    }

    #[test]
    fn dispatch_provider_picker_enter_replaces_with_model_picker() {
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
        let consumed = dispatch_to_active_tab(key, &mut app);
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

    #[test]
    fn dispatch_model_picker_esc_returns_to_provider_picker() {
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
        dispatch_to_active_tab(key, &mut app);
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
        commit_model(&mut app, ProviderKind::Anthropic, "claude-3-5".to_string(), false);

        // Both tabs are gone — the flow ended cleanly.
        assert!(
            !app
                .function
                .tabs
                .iter()
                .any(|t| matches!(t, SidebarTab::ProviderPicker(_))),
            "ProviderPicker must close after commit (the flow ended)"
        );
        assert!(
            !app
                .function
                .tabs
                .iter()
                .any(|t| matches!(t, SidebarTab::ModelPicker(_))),
            "ModelPicker must close after commit"
        );
    }

    #[test]
    fn dispatch_settings_esc_at_toplevel_with_other_tab_does_not_resurrect_settings() {
        // The same dispatcher bug also affected Settings: pressing Esc
        // at TopLevel removed the Settings tab, but the dispatcher put
        // it back when there were other tabs after it. Fixing the
        // dispatcher's restore logic also fixes this.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use crate::function::{SettingsLevel, SettingsState};

        let mut app = make_app();
        // Add a Notifications tab AFTER the Settings tab so the bug
        // would have resurrected the Settings tab.
        app.function.push(SidebarTab::Notifications);
        let notif_idx = app.function.tabs.len() - 1;
        // Push a Settings tab and make it the active one.
        let mut s = SettingsState::new(&app.config);
        s.level = SettingsLevel::TopLevel;
        app.function.push(SidebarTab::Settings(s));
        app.function.active = app.function.tabs.len() - 1;
        let settings_idx = app.function.active;
        assert_ne!(settings_idx, notif_idx);

        // Simulate Esc through the dispatcher.
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let consumed = dispatch_to_active_tab(key, &mut app);
        assert!(consumed, "Esc at TopLevel must be consumed");

        // After Esc: Settings tab is gone; Notifications tab remains.
        assert!(
            !app
                .function
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
            });
        }
        s.rebuild_filter();
        assert_eq!(s.cursor, 0);
        assert_eq!(s.scroll, 0);

        // Move cursor to the end.
        s.cursor = 19;
        s.ensure_cursor_visible(5);
        assert_eq!(s.scroll, 15, "scroll must advance to keep cursor visible");

        // Move cursor to the top.
        s.cursor = 0;
        s.ensure_cursor_visible(5);
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
        crate::function::SidebarTab::ModelPicker(crate::function::ModelPickerState::new(
            ProviderKind::Openai,
        ));
        let mut picker = crate::function::ModelPickerState::new(ProviderKind::Openai);
        picker.focus = crate::function::PickerFocus::List;
        picker.models.push(crate::function::notifications::ModelInfo {
            id: "gpt-4o".to_string(),
            display: "gpt-4o".to_string(),
        });
        picker.models.push(crate::function::notifications::ModelInfo {
            id: "gpt-4o-mini".to_string(),
            display: "gpt-4o-mini".to_string(),
        });
        picker.rebuild_filter();
        picker.cursor = 1;
        app.function.push(crate::function::SidebarTab::ModelPicker(picker));
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
        assert!(!app.function.tabs.iter().any(|t| matches!(t, SidebarTab::ModelPicker(_))));
        let entry = app.config.entry(&id).unwrap();
        assert_eq!(entry.model, model_to_pick);
        let _ = AppMsg::ChatError(String::new()); // suppress unused
    }

    #[test]
    fn commit_model_with_empty_function_panel_does_not_panic() {
        // The function panel has only the picker, no Notifications. After
        // commit, the function is empty. Verify no panic.
        let mut app = make_app();
        // Remove the default Notifications tab.
        app.function.tabs.clear();
        app.function.active = 0;
        app.config.active = Some(make_id(ProviderKind::Openai, ProviderMode::Key));
        app.function.push(SidebarTab::ModelPicker(
            crate::function::ModelPickerState::new(ProviderKind::Openai),
        ));
        app.function.active = 0;

        commit_model(&mut app, ProviderKind::Openai, "gpt-4o".to_string(), false);

        assert_eq!(app.function.tabs.len(), 0);
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
        app.function.push(SidebarTab::Settings(st));
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
        app.function.push(SidebarTab::Settings(
            crate::function::SettingsState::new(&app.config),
        ));
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
        app.function.push(SidebarTab::Settings(
            crate::function::SettingsState::new(&app.config),
        ));
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
                name: String::new(),
            },
        );
        app.config.active = Some(id.clone());

        let result = app.check_config();
        assert!(!result, "check_config must return false when there are errors");
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
                name: String::new(),
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
        assert!(app.notifications.items.is_empty(), "notifications must be cleared on close");
        assert_eq!(app.pending_events, 0);
    }

    #[test]
    fn ctrl_n_does_not_clear_when_switching_tabs() {
        // Switching from another tab to Notifications (panel stays visible)
        // must NOT clear the list. Only closing (visible -> hidden) clears.
        use crate::function::notifications::ToastLevel;

        let mut app = make_app();
        app.function_visible = true;
        app.function.push(SidebarTab::Settings(
            crate::function::SettingsState::new(&app.config),
        ));
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
        use crate::config::EnterBehavior;
        use super::{EnterAction, enter_action};

        // EnterSends: "Enter sends | Shift+Enter newline"
        assert_eq!(enter_action(EnterBehavior::EnterSends, false), EnterAction::Send);
        assert_eq!(enter_action(EnterBehavior::EnterSends, true), EnterAction::Newline);

        // EnterNewline: "Enter newline | Shift+Enter sends"
        assert_eq!(enter_action(EnterBehavior::EnterNewline, false), EnterAction::Newline);
        assert_eq!(enter_action(EnterBehavior::EnterNewline, true), EnterAction::Send);
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
            !app
                .function
                .tabs
                .iter()
                .any(|t| matches!(t, SidebarTab::Completion(_))),
            "completion tab must be removed after submit"
        );
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

        assert!(
            !app
                .function
                .tabs
                .iter()
                .any(|t| matches!(t, SidebarTab::Completion(_)))
        );
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
        app.function.push(SidebarTab::Settings(
            crate::function::SettingsState::new(&app.config),
        ));
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

        // Press Down again: cursor 1 -> 2, form.focused -> KeyOrEnv.
        handle_settings_key(down, &mut app, &mut state);
        assert_eq!(state.cursor, 2);
        if let SettingsLevel::ConfigForm(f) = &state.level {
            assert_eq!(f.focused, ConfigField::KeyOrEnv, "Down must move focus");
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
                name: String::new(),
            },
        );
        let form = crate::function::ConfigFormState::new_for_edit(
            id.clone(),
            app.config.entry(&id).unwrap(),
            ProviderMode::Key,
        );
        assert!(!form.key_modified);
        assert_eq!(form.key_or_env, "sk-saved-key-1234");

        let mut state = crate::function::SettingsState::new(&app.config);
        state.level = SettingsLevel::ConfigForm(form);
        if let SettingsLevel::ConfigForm(ref mut f) = state.level {
            f.focused = ConfigField::KeyOrEnv;
        }

        // Type a single char on KeyOrEnv.
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        );
        handle_settings_key(key, &mut app, &mut state);

        if let SettingsLevel::ConfigForm(f) = &state.level {
            assert!(f.key_modified, "key_modified must flip to true on first edit");
            assert_eq!(f.key_or_env, "x", "saved key must be cleared before the new char");
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
                name: String::new(),
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
                name: String::new(),
            },
        );
        let mut form = crate::function::ConfigFormState::new_for_edit(
            id.clone(),
            app.config.entry(&id).unwrap(),
            ProviderMode::Key,
        );
        form.key_modified = true;
        form.key_or_env = "sk-new".to_string();

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
        app.config.entries.retain(|id, _| id.starts_with("anthropic:"));
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
        // The nameless Anthropic entry falls back to the "Kind (mode)"
        // label so it still reads sensibly.
        let anthro_display = state
            .entries
            .iter()
            .find(|e| e.id.starts_with("anthropic:"))
            .map(|e| e.display.as_str());
        assert_eq!(anthro_display, Some("Anthropic (key)"));
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
        assert_eq!(
            state.selected_id().as_deref(),
            Some("openai:key")
        );

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
            let mode = if i % 2 == 0 { ProviderMode::Key } else { ProviderMode::Env };
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
        state.ensure_cursor_visible(5);
        // scroll must have advanced so cursor 10 is inside [scroll, scroll+5).
        assert!(
            state.cursor >= state.scroll
                && state.cursor < state.scroll + 5,
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
        state.ensure_cursor_visible(5);
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
                name: String::new(),
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
            assert!(s.cwd.starts_with("~/"), "cwd should start with ~/, got {:?}", s.cwd);
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
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use crate::function::Selection;
        use crate::ui::extract_selection_text_for_test;

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
                name: "mybot".to_string(),
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
                name: "mybot".to_string(),
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
        app.function.push(SidebarTab::Settings(state));
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
        assert!(app.tui_selection.is_none(), "Down must not create a selection");
        handle_mouse(up, &mut app);
        // Up with no prior Drag must still leave no selection behind.
        assert!(app.tui_selection.is_none(), "click with no drag must leave no selection");
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
}


