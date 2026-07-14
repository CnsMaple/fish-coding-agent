use crate::config::{Config, ProviderKind};
use crate::function::notifications::{HitRate, ModelCache, Notifications, TokenRate};
use crate::session::{Role, Session};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

pub mod notifications;
mod states;
#[cfg(test)]
mod tests;

pub use states::*;

/// Top-level app state.
pub struct App {
    pub config: Config,
    pub config_path: PathBuf,
    pub session: Session,
    pub session_id: String,
    pub session_title: String,
    pub mode: AppMode,
    /// Mode to restore when the function panel is hidden or the
    /// Plan tab is closed. Updated when tab switching enters Plan mode.
    pub previous_mode: AppMode,
    /// Active agent role. Drives the tool permission gate and the
    /// system prompt template. Synced with `mode` by `set_mode`.
    pub active_agent: crate::permission::Agent,
    pub function: FunctionPanel,
    pub input: crate::input::InputState,
    pub status: crate::input::status::StatusBar,

    pub function_visible: bool,
    pub pending_events: u8,
    pub notifications: Notifications,
    pub model_cache: ModelCache,
    pub hit_rate: HitRate,
    pub token_rate: TokenRate,
    pub response_started_at: Option<Instant>,
    pub response_accumulated: std::time::Duration,
    pub response_output_chars: usize,
    pub response_output_tokens: Option<u64>,

    /// Non-streaming HTTP client (30s total timeout for list_models, OAuth, etc.)
    pub reqwest: reqwest::Client,
    /// Streaming HTTP client (no total timeout — relies on stream idle timeout)
    pub stream_client: reqwest::Client,
    pub inflight: Option<InflightHandle>,
    pub cancel_state: CancelState,

    /// Set when the MCP tool list has changed since the last
    /// `openai_tool_specs` / `anthropic_tool_specs` call. The
    /// provider layer re-reads on the next request; the field is
    /// a plain `bool` because the aggregate count is cheap to
    /// recompute.
    pub mcp_tools_dirty: bool,

    /// Monotonic counter incremented every time `send_message` /
    /// `send_chat` (or a direct-tool input) starts a new request.
    /// Spelled out into the corresponding `InflightHandle::seq` and
    /// carried inside the eventual `AppMsg::ChatDone` /
    /// `AppMsg::ChatError` so `handle_msg` can tell a freshly
    /// completed request from a `ChatDone`/`ChatError` left over
    /// from an older request that we already cancelled (e.g. via
    /// Esc) — those stale events must NOT clear the new inflight
    /// or mark the new assistant message as finished.
    pub current_request_seq: u64,

    /// A chat or tool request that has been fully prepared (messages
    /// pushed, inflight set, cancel channel wired up) but not yet
    /// dispatched. The main event loop spawns the actual `tokio::task`
    /// after the next `terminal.draw(...)` returns, so the freshly
    /// pushed user message is on screen before any HTTP / tool call
    /// goes out. While `Some`, the spinner / pending tool block is
    /// already visible (inflight is set) and Esc silently drops the
    /// request without sending it.
    pub pending_request: Option<PendingRequest>,

    pub cwd: PathBuf,
    pub should_quit: bool,
    pub msg_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::event::AppMsg>>,

    /// Cached screen rect of the input prompt line, updated on each render.
    pub input_prompt_area: Option<ratatui::layout::Rect>,

    /// Full-screen text selection driven by the mouse. `Some` while the
    /// user is dragging or has a finished selection. `None` when no
    /// selection is active.
    pub tui_selection: Option<Selection>,
    /// Text extracted from the most recent render of the current
    /// `tui_selection`. Refreshed every frame so that Ctrl+C can copy it
    /// without having to re-walk the buffer.
    pub selected_text: Option<String>,
    /// Where a drag started, if any. Set on Mouse Down, consumed on the
    /// first Drag to create `tui_selection`, and cleared on Mouse Up.
    /// Lets us avoid creating a single-cell "selection" for an
    /// ordinary click with no drag movement.
    pub tui_drag_start: Option<(u16, u16)>,
    /// `(msg_idx, tool_idx)` captured on Mouse Down when the click
    /// lands inside a tool block. The toggle is deferred to Mouse Up
    /// so a drag (text selection) inside the block cancels the toggle.
    pub pending_tool_toggle: Option<(usize, usize)>,
    /// Timestamp of the last mouse event. Used to detect stale drags
    /// when the mouse leaves and re-enters the terminal.
    pub last_mouse_event: Option<Instant>,
    /// Path to the persisted model-cache JSON file. Computed from
    /// `config_path` during construction.
    pub model_cache_path: std::path::PathBuf,
    /// Screen y-range of thinking toggle blocks, each paired with the
    /// index of the corresponding message. Populated after each render.
    pub thinking_toggle_rows: Vec<(u16, u16, usize)>,
    /// Screen y-range of tool result toggle blocks, each paired with
    /// the index of the corresponding message and tool. Populated after
    /// each render.
    pub tool_toggle_rows: Vec<(u16, u16, usize, usize)>,
    /// The screen rect of the session area, updated on each render. Used
    /// by the mouse handler to detect click-in-session for scroll.
    pub session_area: Option<ratatui::layout::Rect>,

    /// Screen coordinates of the input cursor, set during rendering.
    /// Used to position the terminal cursor for IME support.
    pub input_cursor_screen: Option<(u16, u16)>,

    /// Screen coordinates of the function panel's focused text cursor
    /// (e.g., picker search input). Takes priority over `input_cursor_screen`
    /// when set, so IME composition windows appear at the right location
    /// when the user is typing in a picker search field.
    pub function_panel_cursor: Option<(u16, u16)>,

    /// Which area currently receives directional key events.
    pub focus_target: FocusTarget,
    pub paste_blocks: VecDeque<String>,
    /// Image blocks pasted from clipboard, indexed by VecDeque position.
    /// The input buffer shows `[image #K]` where K = 1-based index.
    pub image_blocks: VecDeque<crate::session::ImageAttachment>,
    pub last_paste_text: Option<String>,
    pub last_paste_at: Option<Instant>,
    /// Number of keystrokes to suppress after a paste (terminal re-sends
    /// raw text as individual key events). Decremented for each Char/Enter/Tab.
    pub paste_key_quota: usize,

    /// Burst buffer for legacy-paste detection in `handle_key`.
    /// Accumulates consecutive `Char` / `Enter` key events so that
    /// conhost-style pastes (which arrive as individual `KeyEvent`s)
    /// can be folded into a `[paste N lines]` block.
    pub burst_buf: String,

    /// Pending ask-snapshot content. The model may emit several
    /// `ask` tool calls in one turn; we accumulate their merged
    /// `q1: …` / ` - opt` lines here and flush one consolidated
    /// message into the session on `ChatDone`, so the user sees a
    /// single `+--- Ask ---+` block per assistant turn instead of
    /// one block per question.
    pub pending_ask_snapshot: String,

    /// Snapshot of cursor position and buffer length taken when the
    /// current burst started, so we can undo the inserted characters
    /// if the burst qualifies as a paste.
    pub burst_snapshot: Option<(Instant, usize, usize)>,

    /// Smooth / momentum-scroll animator for the session viewport.
    /// `animating == true` while the previous wheel gesture is still
    /// coasting; new wheel events are dropped during that window.
    /// Programmatic scrolls (submit, jump-to-message, clear, etc.)
    /// call `set_scroll_anchored` to land immediately.
    pub session_scroll: crate::event::ScrollAnimator,

    /// Scroll animator for the input area. Decoupled from cursor
    /// position when the user scrolls with the mouse wheel; snaps back
    /// to cursor on any edit action.
    pub input_scroll: crate::event::ScrollAnimator,
    /// `true` when the input view has been scrolled away from the
    /// cursor by the mouse wheel; reset to `false` on any edit action.
    pub input_scroll_decoupled: bool,

    /// `true` when the next draw should mark every buffer cell
    /// `AlwaysUpdate` so ratatui's diff engine repaints the full
    /// screen. Used after scroll changes to work around BufferDiff
    /// skipping CJK trailing cells (which leaves 1-cell bg streaks).
    pub force_full_repaint: bool,

    /// `true` while an auto- or manual-compaction stream is in
    /// flight. Independent from `inflight` so the spinner logic /
    /// inflight-based cancel do not interfere with a compaction
    /// task (which has its own cancel channel). The status bar
    /// shows `cmp:triggered` whenever this is `true`.
    pub compacting: bool,

    /// Once a compaction succeeds, the event loop schedules a
    /// synthetic "Continue if you have next steps…" user prompt
    /// (mirrors opencode's `experimental.compaction.autocontinue`).
    /// `None` means "no follow-up pending". Drained on the next
    /// idle frame.
    pub pending_post_compaction_prompt: Option<String>,

    /// Whether the agents.md splash area is visible (new session, no input yet).
    pub agents_visible: bool,
    /// Cursor position in the agents checkbox list.
    pub agents_cursor: usize,
}

/// Mouse-driven text selection spanning the full TUI. Coordinates are
/// document-global line indices (0-based from the top of the session)
/// plus screen-column offsets for intra-line selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub doc_start: usize,
    pub doc_end: usize,
    /// Screen column (absolute x within the session area) where the
    /// selection starts on the `doc_start` line. `None` = full width.
    pub col_start: Option<u16>,
    /// Screen column where the selection ends on the `doc_end` line.
    pub col_end: Option<u16>,
    pub active: bool,
}

impl Selection {
    pub fn new(doc_line: usize) -> Self {
        Self {
            doc_start: doc_line,
            doc_end: doc_line,
            col_start: None,
            col_end: None,
            active: true,
        }
    }

    pub fn clear(&mut self) {
        self.doc_start = 0;
        self.doc_end = 0;
        self.col_start = None;
        self.col_end = None;
        self.active = false;
    }
}

#[derive(Debug)]
pub struct InflightHandle {
    pub cancel: tokio::sync::watch::Sender<bool>,
    pub label: String,
    /// The `current_request_seq` value at the time this inflight was
    /// created. The chat task tags its final `ChatDone`/`ChatError`
    /// with this number; `handle_msg` compares it against
    /// `App::current_request_seq` and ignores mismatches. See the
    /// field-level comment on `App::current_request_seq`.
    pub seq: u64,
    /// Wall-clock time when this inflight was armed, used by the TUI
    /// to show an incrementing `[12s]` elapsed indicator next to the
    /// "esc to interrupt" hint.
    pub started_at: std::time::Instant,
}

impl App {
    pub fn new(config: Config, config_path: PathBuf, cwd: PathBuf) -> Self {
        let mut status = crate::input::status::StatusBar::new();
        status.set_provider_name(&config.active_name());
        status.set_model(&config.active_model_display());
        status.set_thinking(config.thinking);
        status.set_cwd(&cwd);
        status.set_mode(AppMode::Yolo.as_str());
        let cache_file = config_path
            .parent()
            .unwrap_or(&config_path)
            .join("model-cache.json");
        let model_cache = ModelCache::load(&cache_file);
        let session_id = crate::session::store::new_session_id();
        let session_title = crate::session::store::default_title(&cwd);
        let mut app = Self {
            config,
            config_path,
            session: Session::default(),
            session_id,
            session_title,
            mode: AppMode::Yolo,
            previous_mode: AppMode::Yolo,
            function: FunctionPanel::new(),
            active_agent: crate::permission::Agent::Build,
            input: crate::input::InputState::new(),
            status,
            function_visible: false,
            pending_events: 0,
            notifications: Notifications::default(),
            model_cache,
            model_cache_path: cache_file,
            thinking_toggle_rows: Vec::new(),
            tool_toggle_rows: Vec::new(),
            session_area: None,
            hit_rate: HitRate::new(50),
            token_rate: TokenRate::new(50),
            response_started_at: None,
            response_accumulated: std::time::Duration::ZERO,
            response_output_chars: 0,
            response_output_tokens: None,
            reqwest: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
            stream_client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("stream client"),
            inflight: None,
            cancel_state: CancelState::default(),
            current_request_seq: 0,
            pending_request: None,
            cwd,
            should_quit: false,
            msg_tx: None,
            mcp_tools_dirty: true,
            input_prompt_area: None,
            tui_selection: None,
            selected_text: None,
            tui_drag_start: None,
            pending_tool_toggle: None,
            last_mouse_event: None,
            input_cursor_screen: None,
            focus_target: FocusTarget::Input,
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
            force_full_repaint: false,
            compacting: false,
            pending_post_compaction_prompt: None,
            agents_visible: true,
            agents_cursor: 0,
        };
        // Sync the auto-compact status from the loaded config so
        // the very first render of the input bar shows the right
        // state. `config` was moved into `app.config` above, so
        // read from there.
        let auto = app.config.auto_compact;
        app.status.set_auto_compact(auto);
        // Reset max_output_tokens to 0; the live provider can fill
        // it in via `refresh_status_model_context`. For now, the
        // cmp segment is suppressed when the model is unknown.
        app.status.set_max_output_tokens(0);
        app.refresh_status_model_context();
        app.load_agents();
        app
    }

    pub fn refresh_status_model_context(&mut self) {
        let Some(kind) = self.config.active_kind() else {
            return;
        };
        let request_model = self.config.active_model().trim().to_string();
        let display_model = self.config.active_model_display();
        let Some(cache) = self.model_cache.get(kind) else {
            if kind == ProviderKind::Cursor {
                self.status.clear_context_window_tokens();
            }
            return;
        };
        let selected = cache.models.iter().find(|m| {
            m.id == request_model
                || m.request_id.as_deref() == Some(request_model.as_str())
                || m.display == display_model
        });
        if kind == ProviderKind::Cursor {
            // GetUsableModels does not include a reliable context window.
            // Keep Cursor unknown until a runtime checkpoint reports max_tokens.
            self.status.clear_context_window_tokens();
        } else if let Some(tokens) = selected.and_then(|m| m.context_window_tokens) {
            self.status.set_context_window_tokens(tokens);
        }
        // `ModelInfo` does not yet carry a separate `max_output_tokens`
        // field. Until it does, the cmp segment is shown when the
        // context window is known but `max_output_tokens == 0`, with
        // the formula falling back to `ctx_window * 0.25` (opencode's
        // default) inside `StatusBar::recompute_compact_pct`.
    }

    /// Push a toast; force-show the panel only for `Fail`-level
    /// notifications. All other levels increment the unread count
    /// without popping open the panel so the user sees the badge
    /// in the status line instead.
    /// The Notifications tab is created on-demand — when no other tab
    /// is already open, it also becomes the active tab.
    pub fn notify(
        &mut self,
        level: crate::function::notifications::ToastLevel,
        text: impl Into<String>,
    ) {
        let text = text.into();
        let notif_exists = self
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::Notifications));
        if !notif_exists {
            let saved_active = self.function.active;
            self.function.push(SidebarTab::Notifications);
            // Restore the previous active tab — push() always sets
            // active to the new tab, but we don't want to steal focus
            // from an existing tab (e.g. Settings, Ask).
            if saved_active < self.function.tabs.len() - 1 {
                self.function.active = saved_active;
            }
        }
        if !self.function_visible {
            if level == crate::function::notifications::ToastLevel::Fail {
                self.function_visible = true;
            }
            self.pending_events = self.pending_events.saturating_add(1);
        }
        self.notifications.push(level, text);
    }

    pub fn save_config(&mut self) {
        if let Err(e) = self.config.save(&self.config_path) {
            self.notify(
                crate::function::notifications::ToastLevel::Fail,
                format!("save config: {e}"),
            );
        } else {
            // Layout-affecting config (e.g. `tool_preview_lines`)
            // may have changed; force the session renderer to
            // re-compute viewport math and the viewport cache.
            self.session.invalidate_layout_cache();
            self.notify(
                crate::function::notifications::ToastLevel::Ok,
                format!("config saved to {}", self.config_path.display()),
            );
        }
    }

    pub fn save_current_session(&mut self) {
        if self.session.messages.is_empty() {
            return;
        }
        self.ensure_prompt_title();
        let provider = if self.status.provider.is_empty() {
            None
        } else {
            Some(self.status.provider.clone())
        };
        let model = if self.status.model.is_empty() || self.status.model == "(no model)" {
            None
        } else {
            Some(self.status.model.clone())
        };
        let thinking = Some(self.status.thinking.as_str().to_string());
        if let Err(e) = crate::session::store::save(
            &self.session_id,
            &self.session_title,
            &self.cwd,
            &self.session,
            crate::session::store::SaveMeta {
                provider,
                model,
                thinking,
                token_total: self.status.token_total,
                context_window_tokens: self.status.context_window_tokens,
                context_window_known: self.status.context_window_known,
                max_output_tokens: self.status.max_output_tokens,
                auto_compact: self.status.auto_compact,
                mcp_summary: self.status.mcp_summary.clone(),
            },
        ) {
            self.notify(
                crate::function::notifications::ToastLevel::Fail,
                format!("save session: {e}"),
            );
        }
    }

    /// Discover agents.md files at known paths and seed the config entries.
    /// Called once on startup so the checkbox defaults are populated.
    pub fn load_agents(&mut self) {
        let mut paths: Vec<String> = Vec::new();

        // ~/.agents/agents.md
        if let Some(home) = dirs::home_dir() {
            let p = home.join(".agents").join("agents.md");
            paths.push(p.to_string_lossy().to_string());
        }

        // ./agents.md (relative to cwd)
        let local = self.cwd.join("agents.md");
        paths.push(local.to_string_lossy().to_string());

        let mut changed = false;
        for p in &paths {
            if std::path::Path::new(p).exists() {
                if !self.config.agents.entries.contains_key(p) {
                    self.config.agents.entries.insert(p.clone(), true);
                    changed = true;
                }
            } else {
                if self.config.agents.entries.remove(p).is_some() {
                    changed = true;
                }
            }
        }
        if changed {
            self.save_config();
        }
        // Clamp cursor to valid range after entries may have changed.
        let count = self.config.agents.entries.len();
        if count == 0 {
            self.agents_cursor = 0;
        } else if self.agents_cursor >= count {
            self.agents_cursor = count - 1;
        }
    }

    pub fn start_new_session(&mut self) {
        if !self.session.messages.is_empty() {
            self.save_current_session();
        }
        self.session.clear();
        self.session_id = crate::session::store::new_session_id();
        self.session_title = crate::session::store::default_title(&self.cwd);
        self.image_blocks.clear();
        // Remove the todo tab when the session is cleared.
        self.function.tabs.retain(|t| !matches!(t, SidebarTab::Todo(_)));
        if self.function.active >= self.function.tabs.len() {
            self.function.active = self.function.tabs.len().saturating_sub(1);
        }
        self.maybe_hide_panel();
        // Show the agents splash area for the new session.
        self.load_agents();
        self.agents_visible = true;
        self.agents_cursor = 0;
        // Land at the tail immediately; cancel any in-flight momentum.
        self.set_scroll_anchored(0);
        self.status.reset_usage_stats();
    }

    /// Pin the session viewport to a specific scroll offset, cancelling
    /// any in-flight momentum animation. Use this for programmatic
    /// scrolls (submit, jump-to-message, new session, etc.) that should
    /// not coast. The integer offset is written to `session.scroll` so
    /// the existing render path picks it up, and the render cache is
    /// invalidated so the change is visible on the next frame.
    pub fn set_scroll_anchored(&mut self, value: u32) {
        self.session_scroll.snap(value as f32);
        self.session.scroll = value;
    }

    pub fn maybe_title_from_first_prompt(&mut self, prompt: &str) {
        if self.session.messages.is_empty()
            && self.session_title == crate::session::store::default_title(&self.cwd)
        {
            self.session_title = crate::session::store::title_from_prompt(prompt);
        }
    }

    fn ensure_prompt_title(&mut self) {
        if self.session_title != crate::session::store::default_title(&self.cwd) {
            return;
        }
        if let Some(prompt) = self
            .session
            .messages
            .iter()
            .find(|m| matches!(m.role, Role::User))
            .map(|m| m.content.as_str())
        {
            self.session_title = crate::session::store::title_from_prompt(prompt);
        }
    }

    pub fn resume_session(&mut self, id: &str) {
        self.agents_visible = false;
        match crate::session::store::load(id) {
            Ok(stored) => {
                self.session = Session {
                    messages: stored.messages,
                    todo_items: stored.todo_items.clone(),
                    scroll: 0,
                    streaming_id: None,
                    display: self.config.thinking_display,
                    tool_display: self.config.tool_display,
                    tool_preview_lines: self.config.tool_preview_lines,
                    line_cache: Default::default(),
                    message_lines_cache: Default::default(),
                    cached_total_lines: None,
                    last_rendered_total: None,
                    expand_new_tool_results: false,
                    line_offsets: Vec::new(),
                    pending_scroll_top: None,
                };
                self.image_blocks.clear();
                self.session.invalidate_layout_cache();
                if !stored.todo_items.is_empty() {
                    self.open_todo_tab();
                }
                self.session_id = stored.id;
                self.session_title = stored.title;
                if let Some(ref p) = stored.provider {
                    self.status.set_provider_name(p);
                }
                if let Some(ref m) = stored.model {
                    self.status.set_model(m);
                }
                if let Some(ref t) = stored.thinking {
                    self.status.set_thinking(crate::config::ReasoningMode::parse(t));
                }
                if let Some(total) = stored.token_total {
                    self.status.update_token_usage(total);
                }
                if stored.context_window_known && stored.context_window_tokens > 0 {
                    self.status.set_context_window_tokens(stored.context_window_tokens);
                }
                if stored.max_output_tokens > 0 {
                    self.status.set_max_output_tokens(stored.max_output_tokens);
                }
                self.status.set_auto_compact(stored.auto_compact);
                self.status.set_mcp_summary(stored.mcp_summary.clone());
                self.focus_target = FocusTarget::Input;
                self.function_panel_cursor = None;
                if stored.todo_items.is_empty() {
                    self.function_visible = false;
                }
                self.notify(
                    crate::function::notifications::ToastLevel::Ok,
                    format!("resumed session: {}", self.session_title),
                );
            }
            Err(e) => self.notify(
                crate::function::notifications::ToastLevel::Fail,
                format!("resume session: {e}"),
            ),
        }
    }

    pub fn rename_session(&mut self, target_id: Option<String>, title: String) {
        let title = title.trim().to_string();
        if title.is_empty() {
            self.notify(
                crate::function::notifications::ToastLevel::Fail,
                "session title is empty",
            );
            return;
        }
        match target_id {
            Some(id) => {
                if let Err(e) = crate::session::store::rename(&id, &title) {
                    self.notify(
                        crate::function::notifications::ToastLevel::Fail,
                        format!("rename session: {e}"),
                    );
                } else {
                    if id == self.session_id {
                        self.session_title = title.clone();
                    }
                    self.notify(
                        crate::function::notifications::ToastLevel::Ok,
                        format!("renamed session: {title}"),
                    );
                }
            }
            None => {
                self.session_title = title.clone();
                self.save_current_session();
                self.notify(
                    crate::function::notifications::ToastLevel::Ok,
                    format!("renamed session: {title}"),
                );
            }
        }
    }

    pub fn fork_session(&mut self, source_id: Option<String>) {
        self.save_current_session();
        self.agents_visible = false;
        let source = source_id.unwrap_or_else(|| self.session_id.clone());
        match crate::session::store::fork(&source, &self.cwd, None) {
            Ok(stored) => {
                self.session_id = stored.id;
                self.session_title = stored.title;
                self.session = Session {
                    messages: stored.messages,
                    todo_items: stored.todo_items.clone(),
                    scroll: 0,
                    streaming_id: None,
                    display: self.config.thinking_display,
                    tool_display: self.config.tool_display,
                    tool_preview_lines: self.config.tool_preview_lines,
                    line_cache: Default::default(),
                    message_lines_cache: Default::default(),
                    cached_total_lines: None,
                    last_rendered_total: None,
                    expand_new_tool_results: false,
                    line_offsets: Vec::new(),
                    pending_scroll_top: None,
                };
                if let Some(ref p) = stored.provider {
                    self.status.set_provider_name(p);
                }
                if let Some(ref m) = stored.model {
                    self.status.set_model(m);
                }
                if let Some(ref t) = stored.thinking {
                    self.status.set_thinking(crate::config::ReasoningMode::parse(t));
                }
                if let Some(total) = stored.token_total {
                    self.status.update_token_usage(total);
                }
                if stored.context_window_known && stored.context_window_tokens > 0 {
                    self.status.set_context_window_tokens(stored.context_window_tokens);
                }
                if stored.max_output_tokens > 0 {
                    self.status.set_max_output_tokens(stored.max_output_tokens);
                }
                self.status.set_auto_compact(stored.auto_compact);
                self.status.set_mcp_summary(stored.mcp_summary.clone());
                self.session.invalidate_layout_cache();
                if !stored.todo_items.is_empty() {
                    self.open_todo_tab();
                }
                self.notify(
                    crate::function::notifications::ToastLevel::Ok,
                    format!("forked session: {}", self.session_title),
                );
            }
            Err(e) => self.notify(
                crate::function::notifications::ToastLevel::Fail,
                format!("fork session: {e}"),
            ),
        }
    }

    pub fn set_mode(&mut self, mode: AppMode) {
        self.mode = mode;
        self.active_agent = match mode {
            AppMode::Plan => crate::permission::Agent::Plan,
            _ => crate::permission::Agent::Build,
        };
        self.status.set_mode(mode.as_str());
    }

    /// Toggle Plan mode. If not in Plan mode: save current mode as
    /// `previous_mode`, switch to Plan (read-only), and focus the
    /// existing Plan tab if any. The panel is only shown when there
    /// is at least one tab to display. If already in Plan mode:
    /// restore `previous_mode` and hide the panel.
    pub fn jump_to_plan(&mut self) {
        if self.mode == AppMode::Plan {
            self.set_mode(self.previous_mode);
            self.maybe_hide_panel();
            return;
        }
        self.previous_mode = self.mode;
        self.set_mode(AppMode::Plan);
        let has_plan_tab = if let Some((i, _)) = self
            .function
            .tabs
            .iter_mut()
            .enumerate()
            .find(|(_, t)| matches!(t, SidebarTab::Plan(_)))
        {
            self.function.active = i;
            true
        } else {
            false
        };
        // Only show the panel when there is a Plan tab to display,
        // otherwise the user would see an empty bordered box even
        // when other tabs (e.g. Notifications) exist.
        if has_plan_tab {
            self.show_panel();
            self.acknowledge_panel();
        }
    }

    pub fn open_plan(&mut self, title: String, content: String) {
        if self.mode != AppMode::Plan {
            self.previous_mode = self.mode;
        }
        self.set_mode(AppMode::Plan);
        // Plans are not auto-saved: the user reviews the plan in the
        // session and presses S in the plan tab to persist it.
        let state = PlanState::new(title, content);
        if let Some((i, _)) = self
            .function
            .tabs
            .iter_mut()
            .enumerate()
            .find(|(_, t)| matches!(t, SidebarTab::Plan(_)))
        {
            self.function.tabs[i] = SidebarTab::Plan(state);
            self.function.active = i;
        } else {
            self.function.push(SidebarTab::Plan(state));
        }
        self.show_panel();
        self.acknowledge_panel();
    }

    /// Persist the active plan tab to `<config_dir>/plans/<ts>-<slug>.md`.
    /// Sets `dirty = false` and stores the resulting path on the state.
    /// Returns true on success.
    pub fn save_active_plan(&mut self) -> bool {
        let Some(idx) = self
            .function
            .tabs
            .iter()
            .position(|t| matches!(t, SidebarTab::Plan(_)))
        else {
            return false;
        };
        let title = match &self.function.tabs[idx] {
            SidebarTab::Plan(s) => s.title.clone(),
            _ => unreachable!(),
        };
        let content = match &self.function.tabs[idx] {
            SidebarTab::Plan(s) => s.content.clone(),
            _ => unreachable!(),
        };
        let Some(path) = self.persist_plan(&title, &content) else {
            return false;
        };
        if let Some(SidebarTab::Plan(state)) = self.function.tabs.get_mut(idx) {
            state.path = Some(path);
            state.dirty = false;
        }
        true
    }

    /// Open (or extend) an ask picker tab. If an Ask tab is already
    /// open, the new question is appended to its queue and becomes
    /// the active one.
    pub fn open_ask(&mut self, question: String, options: Vec<String>) {
        // Ensure the Notifications tab exists so the toast is recorded.
        let notif_exists = self
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::Notifications));
        if !notif_exists {
            self.function.push(SidebarTab::Notifications);
        }
        // Surface a short toast summary so the notification panel records it.
        self.notifications.push(
            crate::function::notifications::ToastLevel::Info,
            format!("AI asks: {}", {
                let s = question.trim();
                if s.chars().count() > 60 {
                    let cut: String = s.chars().take(57).collect();
                    format!("{cut}…")
                } else {
                    s.to_string()
                }
            }),
        );
        // Ensure the panel is visible when it was hidden.
        if !self.function_visible {
            self.show_panel();
        }

        // Also accumulate the merged-list body so a single `+--- Ask
        // ---+` block can land in the session at the end of the
        // assistant turn (one block per turn, no matter how many ask
        // tool calls the model emitted in parallel).
        self.accumulate_ask_snapshot(&question, &options);

        if let Some((i, _)) = self
            .function
            .tabs
            .iter_mut()
            .enumerate()
            .find(|(_, t)| matches!(t, SidebarTab::Ask(_)))
        {
            if let SidebarTab::Ask(state) = &mut self.function.tabs[i] {
                state.push(question, options);
                state.active = state.items.len().saturating_sub(1);
            }
            self.function.active = i;
        } else {
            self.function.push(SidebarTab::Ask(AskState::new(question, options)));
        }
        self.show_panel();
        self.acknowledge_panel();
    }

    pub fn open_todo_tab(&mut self) {
        if !self.function.tabs.iter().any(|t| matches!(t, SidebarTab::Todo(_))) {
            self.function.push(SidebarTab::Todo(TodoTabState::new()));
        }
        if !self.function_visible {
            self.function_visible = true;
        }
    }

    /// Remove the todo tab (if present) and hide the panel when no
    /// other non-trivial tabs remain. Used when the todo list is cleared.
    pub fn close_todo_tab(&mut self) {
        let todo_idx = self
            .function
            .tabs
            .iter()
            .position(|t| matches!(t, SidebarTab::Todo(_)));
        if let Some(idx) = todo_idx {
            self.function.tabs.remove(idx);
            if self.function.active >= self.function.tabs.len() {
                self.function.active = self.function.tabs.len().saturating_sub(1);
            }
        }
        self.maybe_hide_panel();
    }

    /// Append one question's merged-list lines to the pending ask
    /// snapshot. The snapshot is consumed by `flush_ask_snapshot`
    /// when the assistant turn finishes (`ChatDone`).
    fn accumulate_ask_snapshot(&mut self, question: &str, options: &[String]) {
        // Each call increments the question number so the snapshot
        // can be rendered with `q1: …`, `q2: …` etc.
        let n = self.pending_ask_snapshot.matches("\nq").count() + 1;
        if self.pending_ask_snapshot.is_empty() {
            // First question — open the snapshot with the header on
            // its own line so the renderer can detect "this is an ask
            // snapshot" via the leading `---ask---` token.
            self.pending_ask_snapshot.push_str("---ask---\n");
        }
        self.pending_ask_snapshot
            .push_str(&format!("q{n}: {}\n", question.trim()));
        for opt in options {
            if !opt.trim().is_empty() {
                self.pending_ask_snapshot
                    .push_str(&format!("   - {}\n", opt.trim()));
            }
        }
    }

    /// Flush the accumulated ask snapshot into the session as one
    /// `+--- Ask ---+` block. Called from the `ChatDone` /
    /// `ChatError` event handlers so the snapshot only lands once
    /// the model is done streaming.
    pub fn flush_ask_snapshot(&mut self) {
        if self.pending_ask_snapshot.is_empty() {
            return;
        }
        let body = std::mem::take(&mut self.pending_ask_snapshot);
        // Push as an assistant message so it sits in the same
        // transcript turn as the tool calls; the renderer detects
        // `---ask---` and draws the `+--- Ask ---+` block.
        use crate::session::Message;
        let mut msg = Message::new(crate::session::Role::Assistant, body);
        msg.line_count = 1;
        self.session.push(msg);
    }

    /// Write the plan to `<config_dir>/plans/<ts>-<slug>.md`.
    /// Returns the absolute path on success, or `None` if the file
    /// could not be written (we still show the plan in the UI — the
    /// user just won't have a file to refer to).
    fn persist_plan(&mut self, title: &str, content: &str) -> Option<std::path::PathBuf> {
        use crate::function::notifications::ToastLevel;
        let dir = match crate::config::paths::plans_dir() {
            Ok(d) => d,
            Err(e) => {
                self.notify(
                    ToastLevel::Warn,
                    format!("plan: could not resolve plans dir: {e}"),
                );
                return None;
            }
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.notify(
                ToastLevel::Warn,
                format!("plan: could not create plans dir: {e}"),
            );
            return None;
        }
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
        let slug: String = title
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .collect();
        let slug = slug.trim_matches('-');
        let slug = if slug.is_empty() { "plan".to_string() } else { slug.to_string() };
        let filename = format!("{ts}-{slug}.md");
        let path = dir.join(filename);
        let body = format!(
            "# {}\n\n_Generated at {}_\n\n{}\n",
            title,
            chrono::Utc::now().to_rfc3339(),
            content
        );
        match std::fs::write(&path, body) {
            Ok(()) => {
                self.notify(
                    ToastLevel::Info,
                    format!("plan saved to {}", path.display()),
                );
                Some(path)
            }
            Err(e) => {
                self.notify(
                    ToastLevel::Warn,
                    format!("plan: could not write file: {e}"),
                );
                None
            }
        }
    }

    /// Mark that the panel was shown by user (Ctrl+N) so we clear pending marker.
    pub fn acknowledge_panel(&mut self) {
        self.pending_events = 0;
    }

    /// Show the function panel and move focus to it.
    pub fn show_panel(&mut self) {
        self.function_visible = true;
        self.focus_target = FocusTarget::FunctionPanel;
    }

    /// Ensure the Completion sidebar tab reflects the current input buffer.
    /// - If the buffer is a partial `/` command, populate (or create) the tab
    ///   with matching candidates and reset its cursor.
    /// - Otherwise, remove the tab if it is present. If that leaves the
    ///   function panel with no function tabs, hide the panel so the user
    ///   returns to the default hidden state.
    pub fn sync_completion(&mut self) {
        let buffer = self.input.buffer.clone();
        let cursor = self.input.cursor;
        let prefix = if buffer.starts_with('/') && !buffer[..cursor].contains('\n') {
            buffer[..cursor].to_string()
        } else {
            String::new()
        };

        let pos = self
            .function
            .tabs
            .iter()
            .position(|t| matches!(t, SidebarTab::Completion(_)));

        let candidates = if prefix.is_empty() {
            Vec::new()
        } else {
            crate::input::completion_candidates_for(&prefix)
        };

        if candidates.is_empty() {
            if let Some(idx) = pos {
                self.function.tabs.remove(idx);
                if self.function.active >= self.function.tabs.len() {
                    self.function.active = self.function.tabs.len().saturating_sub(1);
                }
                self.maybe_hide_panel();
            }
            return;
        }

        match pos {
            Some(idx) => {
                if let SidebarTab::Completion(s) = &mut self.function.tabs[idx] {
                    s.candidates = candidates;
                    s.clamp_cursor();
                }
            }
            None => {
                self.function.push(SidebarTab::Completion(CompletionState {
                    candidates,
                    cursor: 0,
                    scroll: 0,
                }));
                // Typing `/` is a "function trigger" — the user must see
                // the candidate list, so auto-show the panel and focus
                // the new Completion tab.
                self.function.active = self.function.tabs.len() - 1;
                // Completion is shown while the user is still typing in
                // the input — keep focus on the input so typing continues.
                self.function_visible = true;
                self.acknowledge_panel();
            }
        }
    }

    /// Hide the function panel when the last tab is removed. Called
    /// after any tab removal so the panel returns to the default hidden
    /// state when nothing is open. Focus is only returned to the input
    /// when the panel actually disappears; if other tabs remain visible
    /// the focus stays where it is (the user can switch with Alt+L).
    pub fn maybe_hide_panel(&mut self) {
        let has_non_trivial = self.function.tabs.iter().any(|t| {
            !matches!(t, SidebarTab::Notifications)
        });
        if !has_non_trivial {
            self.focus_target = FocusTarget::Input;
            self.function_panel_cursor = None;
            self.function_visible = false;
            if self.mode == AppMode::Plan {
                self.set_mode(self.previous_mode);
            }
        }
    }
}

pub fn active_provider_string(_kind: ProviderKind) -> &'static str {
    "ok"
}

pub fn _ignore_unused() -> VecDeque<()> {
    VecDeque::new()
}
