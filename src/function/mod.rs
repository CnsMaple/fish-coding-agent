use crate::config::{Config, ProviderId, ProviderKind};
use crate::function::notifications::{HitRate, ModelCache, Notifications, TokenRate};
pub use crate::session::TodoItem;
use crate::session::{Role, Session};
use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

pub mod notifications;

/// Transient command-completion state, shown while the input buffer
/// looks like a partial slash command.
#[derive(Debug)]
pub struct CompletionState {
    pub candidates: Vec<String>,
    pub cursor: usize,
}

impl CompletionState {
    pub fn new(prefix: &str) -> Self {
        let candidates = crate::input::completion_candidates_for(prefix);
        let mut s = Self {
            candidates,
            cursor: 0,
        };
        s.clamp_cursor();
        s
    }

    pub fn clamp_cursor(&mut self) {
        if self.candidates.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.candidates.len() {
            self.cursor = self.candidates.len() - 1;
        }
    }

    pub fn move_up(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        if self.cursor == 0 {
            self.cursor = self.candidates.len() - 1;
        } else {
            self.cursor -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1) % self.candidates.len();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Ask,
    Plan,
    Todo,
    Yolo,
    Shell,
    ShellContext,
    Python,
    PythonContext,
}

impl AppMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AppMode::Ask => "ask",
            AppMode::Plan => "plan",
            AppMode::Todo => "todo",
            AppMode::Yolo => "yolo",
            AppMode::Shell => "shell",
            AppMode::ShellContext => "shell_context",
            AppMode::Python => "python",
            AppMode::PythonContext => "python_context",
        }
    }
}

/// One sidebar tab entry.
#[derive(Debug)]
pub enum SidebarTab {
    Notifications,
    Completion(CompletionState),
    Settings(Box<SettingsState>),
    ModelPicker(ModelPickerState),
    ProviderPicker(ProviderPickerState),
    ThinkingPicker(ThinkingPickerState),
    TimelinePicker(TimelinePickerState),
    SessionPicker(SessionPickerState),
    SessionRename(SessionRenameState),
    Ask(AskState),
    Todo(TodoState),
    Plan(PlanState),
    Hotkey,
}

// =====================================================================
// Settings: hierarchical navigation (no more double-tab).
// =====================================================================

/// Configurable field within a [`ConfigFormState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigField {
    Name,
    BaseUrl,
    Key,
    Env,
    AccessKey,
    SecretKey,
    Save,
    Exit,
}

/// State for the "edit / create provider" form.
#[derive(Debug, Clone)]
pub struct ConfigFormState {
    pub is_new: bool,
    pub id: String, // target entry id (kind:mode)
    /// Optional display name. When empty, the kind name (`openai` /
    /// `anthropic`) is used in the status bar.
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
    pub access_key: String,
    pub secret_key: String,
    /// The field the user is currently editing.
    pub focused: ConfigField,
    /// Error message to display inline (e.g. base_url is required).
    pub form_error: Option<String>,
    /// For edit forms: whether the user has touched the api_key field.
    /// Starts false (the saved key is hidden behind a placeholder);
    /// flips to true on the first character or backspace, at which point the
    /// value is cleared and the user can type a new key. On save, if this is
    /// still false, the original api_key is preserved.
    pub key_modified: bool,
    /// Same for api_key_env.
    pub env_modified: bool,
}

impl ConfigFormState {
    pub fn new_for_create(
        kind: crate::config::ProviderKind,
        mode: crate::config::ProviderMode,
    ) -> Self {
        let id = crate::config::make_id(kind, mode);
        let name = match kind {
            crate::config::ProviderKind::Cursor => "Cursor".to_string(),
            crate::config::ProviderKind::DeepSeek => "DeepSeek".to_string(),
            crate::config::ProviderKind::MiniMax => "MiniMax".to_string(),
            crate::config::ProviderKind::Volcengine => "Volcengine".to_string(),
            _ => String::new(),
        };
        let base_url = match kind {
            crate::config::ProviderKind::Cursor
            | crate::config::ProviderKind::DeepSeek
            | crate::config::ProviderKind::MiniMax
            | crate::config::ProviderKind::Volcengine => {
                crate::config::default_base_url(kind).to_string()
            }
            _ => String::new(),
        };
        Self {
            is_new: true,
            id,
            name,
            base_url,
            api_key: String::new(),
            api_key_env: String::new(),
            access_key: String::new(),
            secret_key: String::new(),
            focused: ConfigField::Name,
            form_error: None,
            key_modified: false,
            env_modified: false,
        }
    }

    pub fn new_for_edit(
        id: String,
        cfg: &crate::config::ProviderConfig,
        _mode: crate::config::ProviderMode,
    ) -> Self {
        Self {
            is_new: false,
            id,
            name: cfg.name.clone(),
            base_url: cfg.base_url.clone(),
            api_key: cfg.api_key.clone(),
            api_key_env: cfg.api_key_env.clone(),
            access_key: cfg.access_key.clone(),
            secret_key: cfg.secret_key.clone(),
            focused: ConfigField::Name,
            form_error: None,
            key_modified: false,
            env_modified: false,
        }
    }

    pub fn is_cursor(&self) -> bool {
        crate::config::parse_id(&self.id)
            .map(|(k, _)| k == crate::config::ProviderKind::Cursor)
            .unwrap_or(false)
    }

    pub fn is_volcengine(&self) -> bool {
        crate::config::parse_id(&self.id)
            .map(|(k, _)| k == crate::config::ProviderKind::Volcengine)
            .unwrap_or(false)
    }

    pub fn active_fields(&self) -> Vec<ConfigField> {
        if self.is_cursor() {
            vec![
                ConfigField::Name,
                ConfigField::BaseUrl,
                ConfigField::Save,
                ConfigField::Exit,
            ]
        } else if self.is_volcengine() {
            vec![
                ConfigField::Name,
                ConfigField::BaseUrl,
                ConfigField::Key,
                ConfigField::Env,
                ConfigField::AccessKey,
                ConfigField::SecretKey,
                ConfigField::Save,
                ConfigField::Exit,
            ]
        } else {
            vec![
                ConfigField::Name,
                ConfigField::BaseUrl,
                ConfigField::Key,
                ConfigField::Env,
                ConfigField::Save,
                ConfigField::Exit,
            ]
        }
    }

    pub fn field_label(&self, f: ConfigField) -> &'static str {
        match f {
            ConfigField::Name => "name",
            ConfigField::BaseUrl => "base url",
            ConfigField::Key => "api key",
            ConfigField::Env => "env name",
            ConfigField::AccessKey => "access key",
            ConfigField::SecretKey => "secret key",
            ConfigField::Save => "save",
            ConfigField::Exit => "exit",
        }
    }
}

/// The level currently being shown in the settings tab.
#[derive(Debug)]
pub enum SettingsLevel {
    /// Top: "set provider" and "thinking display".
    TopLevel,
    /// Provider list: "new provider" + existing entries.
    ProviderList,
    /// Choose (kind, mode) for a new entry.
    NewProviderKind,
    /// Action menu for an existing entry: edit / delete.
    ExistingActions(String),
    /// Edit / create form.
    ConfigForm(ConfigFormState),
    /// Thinking display mode chooser.
    ThinkingDisplayList,
    /// Tool result display mode chooser.
    ToolResultDisplayList,
    /// Enter/newline behavior chooser.
    EnterBehaviorList,
    /// Border type chooser.
    BorderTypeList,
    /// Theme chooser.
    ThemeList,
}

impl SettingsLevel {
    /// Title shown in the settings header (breadcrumb-like).
    pub fn title(&self) -> String {
        match self {
            SettingsLevel::TopLevel => "settings".to_string(),
            SettingsLevel::ProviderList => "settings / set provider".to_string(),
            SettingsLevel::NewProviderKind => "settings / set provider / new".to_string(),
            SettingsLevel::ExistingActions(id) => {
                format!(
                    "settings / set provider / {}",
                    crate::config::id_display(id)
                )
            }
            SettingsLevel::ConfigForm(s) => {
                let stage = if s.is_new { "new" } else { "edit" };
                format!(
                    "settings / set provider / {} / {}",
                    stage,
                    crate::config::id_display(&s.id)
                )
            }
            SettingsLevel::ThinkingDisplayList => "settings / thinking display".to_string(),
            SettingsLevel::ToolResultDisplayList => "settings / tool display".to_string(),
            SettingsLevel::EnterBehaviorList => "settings / enter behavior".to_string(),
            SettingsLevel::BorderTypeList => "settings / border type".to_string(),
            SettingsLevel::ThemeList => "settings / theme".to_string(),
        }
    }

    /// Shortcut hint rendered in dim gray at the bottom of the panel.
    pub fn hint(&self) -> &'static str {
        match self {
            SettingsLevel::TopLevel => "Up/Down: nav | Enter: select | Esc: close",
            SettingsLevel::ProviderList => "Up/Down: nav | Enter: select | Esc: back",
            SettingsLevel::NewProviderKind => "Up/Down: nav | Enter: select | Esc: back",
            SettingsLevel::ExistingActions(_) => "Up/Down: nav | Enter: select | Esc: back",
            SettingsLevel::ConfigForm(_) => {
                "Up/Down: nav | type: edit | Enter: confirm | Esc: back"
            }
            SettingsLevel::ThinkingDisplayList
            | SettingsLevel::ToolResultDisplayList
            | SettingsLevel::EnterBehaviorList
            | SettingsLevel::BorderTypeList
            | SettingsLevel::ThemeList => "Up/Down: nav | Enter: select | Esc: back",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewProviderPickerState {
    pub entries: Vec<ProviderId>,
    pub filtered: Vec<usize>,
    pub query: String,
    pub cursor: usize,
    pub scroll: usize,
    pub focus: PickerFocus,
}

impl NewProviderPickerState {
    pub fn new() -> Self {
        let entries = crate::config::Config::all_possible_ids();
        let mut s = Self {
            entries,
            filtered: Vec::new(),
            query: String::new(),
            cursor: 0,
            scroll: 0,
            focus: PickerFocus::List,
        };
        s.rebuild_filter();
        s
    }

    pub fn picker_label(&self, id: &str) -> String {
        crate::config::parse_id(id)
            .map(|(k, _)| k.picker_label().to_string())
            .unwrap_or_else(|| crate::config::id_display(id))
    }

    pub fn rebuild_filter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, id)| {
                q.is_empty()
                    || id.to_lowercase().contains(&q)
                    || self.picker_label(id).to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.cursor >= self.filtered.len() {
            self.cursor = self.filtered.len().saturating_sub(1);
        }
        if self.scroll > self.cursor {
            self.scroll = self.cursor;
        }
    }

    pub fn ensure_cursor_visible(&mut self, visible_rows: usize) {
        if visible_rows == 0 {
            return;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + visible_rows {
            self.scroll = self.cursor + 1 - visible_rows;
        }
    }

    pub fn selected_id(&self) -> Option<ProviderId> {
        self.filtered
            .get(self.cursor)
            .map(|&i| self.entries[i].clone())
    }
}

impl Default for NewProviderPickerState {
    fn default() -> Self {
        Self::new()
    }
}

/// State of the settings sidebar tab.
#[derive(Debug)]
pub struct SettingsState {
    /// Current level in the navigation stack.
    pub level: SettingsLevel,
    /// Cursor inside the current list view (or the focused field for ConfigForm).
    pub cursor: usize,
    /// Validation error to surface inline in the form (e.g. empty base_url).
    pub form_error: Option<String>,
    /// Error reason when config failed to parse, so we can show it.
    pub load_error: Option<String>,
    pub new_provider: NewProviderPickerState,
}

impl SettingsState {
    pub fn new(_cfg: &Config) -> Self {
        Self {
            level: SettingsLevel::TopLevel,
            cursor: 0,
            form_error: None,
            load_error: None,
            new_provider: NewProviderPickerState::new(),
        }
    }

    /// Number of items in the current list view (used to clamp cursor).
    pub fn list_len(&self, cfg: &Config) -> usize {
        match &self.level {
            SettingsLevel::TopLevel => 6, // set provider, thinking display, tool display, enter behavior, border type, theme
            SettingsLevel::ProviderList => 1 + cfg.configured_provider_ids().len(), // new + existing
            SettingsLevel::NewProviderKind => self.new_provider.filtered.len(),
            SettingsLevel::ExistingActions(_) => 2, // edit, delete
            SettingsLevel::ConfigForm(form) => form.active_fields().len(),
            SettingsLevel::ThinkingDisplayList => 3, // show, hide, while streaming
            SettingsLevel::ToolResultDisplayList => 3, // show, hide, while streaming
            SettingsLevel::EnterBehaviorList => 2,   // enter sends, enter newline
            SettingsLevel::BorderTypeList => 2,      // ascii, rounded
            SettingsLevel::ThemeList => crate::theme::ThemeVariant::all().len(),
        }
    }

    pub fn clamp_cursor(&mut self, cfg: &Config) {
        let len = self.list_len(cfg);
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerFocus {
    Search,
    List,
}

#[derive(Debug)]
pub struct ModelPickerState {
    pub provider: ProviderKind,
    pub query: String,
    pub models: Vec<ModelInfo>,
    pub filtered: Vec<usize>,
    pub cursor: usize,
    pub focus: PickerFocus,
    pub fetching: bool,
    pub fetch_error: Option<String>,
    pub no_endpoint: bool,
    /// Scroll offset (top visible row in the list) so the focused row is
    /// always in view.
    pub scroll: usize,
}

use crate::function::notifications::ModelInfo;

impl ModelPickerState {
    pub fn new(provider: ProviderKind) -> Self {
        Self {
            provider,
            query: String::new(),
            models: vec![],
            filtered: vec![],
            cursor: 0,
            focus: PickerFocus::List,
            fetching: false,
            fetch_error: None,
            no_endpoint: false,
            scroll: 0,
        }
    }

    pub fn rebuild_filter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .models
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                q.is_empty()
                    || m.id.to_lowercase().contains(&q)
                    || m.display.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.cursor >= self.filtered.len() {
            self.cursor = self.filtered.len().saturating_sub(1);
        }
        // Keep cursor visible after the filter shrinks.
        self.adjust_scroll();
    }

    /// Move `scroll` so that `cursor` is inside the visible window of
    /// `visible_rows` rows. Call this from the renderer once the list height
    /// is known.
    pub fn ensure_cursor_visible(&mut self, visible_rows: usize) {
        if visible_rows == 0 {
            return;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + visible_rows {
            self.scroll = self.cursor + 1 - visible_rows;
        }
    }

    fn adjust_scroll(&mut self) {
        // Without a known viewport, keep scroll near cursor (best effort).
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
    }
}

/// One row in the provider picker. The user picks a specific configured
/// entry, not just a `ProviderKind` — the same kind can be configured
/// twice (once per mode) and the user typically gave each a distinct
/// `name` (or the default "Kind (mode)" label).
#[derive(Debug, Clone)]
pub struct ProviderPickerEntry {
    /// Provider entry id, e.g. `openai:key`.
    pub id: ProviderId,
    /// User-facing label: the entry's `name` when set, otherwise the
    /// `Kind (mode)` fallback.
    pub display: String,
}

/// First step of the `/model` flow: pick a configured provider entry.
/// On confirmation, the active tab is replaced with a `ModelPickerState`
/// for the selected entry's kind. Lists one row per configured entry
/// (not per kind) so the user can disambiguate, e.g., "prod-openai" vs
/// "dev-openai" when both are configured.
#[derive(Debug)]
pub struct ProviderPickerState {
    /// All configured entries, sorted by display name.
    pub entries: Vec<ProviderPickerEntry>,
    /// Indices into `entries` after applying the search filter.
    pub filtered: Vec<usize>,
    /// User's search/filter query.
    pub query: String,
    /// Cursor within the filtered list.
    pub cursor: usize,
    /// Scroll offset (top visible row in the list).
    pub scroll: usize,
    /// Where keyboard input is going: the search box or the list.
    pub focus: PickerFocus,
    /// The currently-active entry id, used for the `[active]` marker.
    pub active: Option<ProviderId>,
}

impl ProviderPickerState {
    pub fn new(cfg: &Config) -> Self {
        let mut entries: Vec<ProviderPickerEntry> = cfg
            .configured_provider_ids()
            .into_iter()
            .filter_map(|id| {
                let entry = cfg.entry(&id)?;
                let display = if entry.name.trim().is_empty() {
                    crate::config::id_display(&id)
                } else {
                    entry.name.clone()
                };
                Some(ProviderPickerEntry { id, display })
            })
            .collect();
        entries.sort_by(|a, b| a.display.to_lowercase().cmp(&b.display.to_lowercase()));
        let mut s = Self {
            entries,
            filtered: Vec::new(),
            query: String::new(),
            cursor: 0,
            scroll: 0,
            focus: PickerFocus::List,
            active: cfg.active.clone(),
        };
        s.rebuild_filter();
        s
    }

    pub fn rebuild_filter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                q.is_empty()
                    || e.display.to_lowercase().contains(&q)
                    || e.id.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.cursor >= self.filtered.len() {
            self.cursor = self.filtered.len().saturating_sub(1);
        }
        if self.scroll > self.cursor {
            self.scroll = self.cursor;
        }
    }

    /// Adjust `scroll` so `cursor` stays inside the visible window.
    pub fn ensure_cursor_visible(&mut self, visible_rows: usize) {
        if visible_rows == 0 {
            return;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + visible_rows {
            self.scroll = self.cursor + 1 - visible_rows;
        }
    }

    /// Returns the id of the focused entry, if any.
    pub fn selected_id(&self) -> Option<ProviderId> {
        self.filtered
            .get(self.cursor)
            .map(|&i| self.entries[i].id.clone())
    }
}

/// List picker for the four thinking levels, with a small search / filter
/// input like the model picker. Even with only four items the user wants
/// the same interaction pattern — type to filter, Arrow-key to navigate,
/// Enter to select, Esc to cancel.
#[derive(Debug)]
pub struct ThinkingPickerState {
    pub cursor: usize,
    pub query: String,
    pub filtered: Vec<usize>,
}

impl ThinkingPickerState {
    pub fn new() -> Self {
        let mut s = Self {
            cursor: 0,
            query: String::new(),
            filtered: Vec::new(),
        };
        s.rebuild_filter();
        s
    }

    pub const LEVELS: &'static [&'static str] = &["off", "low", "med", "high", "adaptive"];

    pub fn selected(&self) -> Option<&'static str> {
        self.filtered
            .get(self.cursor)
            .and_then(|&i| Self::LEVELS.get(i))
            .copied()
    }

    pub fn rebuild_filter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = Self::LEVELS
            .iter()
            .enumerate()
            .filter(|(_, level)| q.is_empty() || level.starts_with(&q))
            .map(|(i, _)| i)
            .collect();
        if self.cursor >= self.filtered.len() {
            self.cursor = self.filtered.len().saturating_sub(1);
        }
    }
}

impl Default for ThinkingPickerState {
    fn default() -> Self {
        Self::new()
    }
}

/// One entry shown in the timeline picker. A snapshot of a session
/// message at the time the picker was opened; the picker does not
/// react to new messages streaming in.
#[derive(Debug, Clone)]
pub struct TimelineEntry {
    pub msg_idx: usize,
    pub role: Role,
    /// Short single-line preview of the message content.
    pub preview: String,
    pub ts: DateTime<Utc>,
    /// If this entry represents a tool call within the message,
    /// this is the index into the message's tool_results.
    pub tool_idx: Option<usize>,
}

/// Sidebar picker that lists session messages (user prompts +
/// assistant replies) with a search/filter input. Pressing Enter
/// scrolls the session so the focused message appears at the top
/// of the viewport.
#[derive(Debug)]
pub struct TimelinePickerState {
    pub query: String,
    pub entries: Vec<TimelineEntry>,
    /// Indices into `entries` after filtering.
    pub filtered: Vec<usize>,
    pub cursor: usize,
    pub scroll: usize,
    pub focus: PickerFocus,
}

impl TimelinePickerState {
    pub fn new(session: &Session) -> Self {
        let entries = snapshot_session(session);
        let mut s = Self {
            query: String::new(),
            entries,
            filtered: Vec::new(),
            cursor: 0,
            scroll: 0,
            focus: PickerFocus::Search,
        };
        s.rebuild_filter();
        s
    }

    pub fn rebuild_filter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| q.is_empty() || e.preview.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        if self.cursor >= self.filtered.len() {
            self.cursor = self.filtered.len().saturating_sub(1);
        }
        if self.scroll > self.cursor {
            self.scroll = self.cursor;
        }
    }

    /// Move `scroll` so that `cursor` is inside the visible window.
    pub fn ensure_cursor_visible(&mut self, visible_rows: usize) {
        if visible_rows == 0 {
            return;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + visible_rows {
            self.scroll = self.cursor + 1 - visible_rows;
        }
    }

    /// Returns the `session.messages` index and optional tool index
    /// of the currently focused entry, if any.
    pub fn selected_entry(&self) -> Option<(usize, Option<usize>)> {
        self.filtered.get(self.cursor).map(|&i| {
            let e = &self.entries[i];
            (e.msg_idx, e.tool_idx)
        })
    }
}

fn snapshot_session(session: &Session) -> Vec<TimelineEntry> {
    let mut out = Vec::new();
    for (i, m) in session.messages.iter().enumerate() {
        // Hide empty assistant placeholders (the in-flight streaming
        // message before the first delta arrives).
        if matches!(m.role, Role::Assistant) && m.content.trim().is_empty() {
            continue;
        }
        let first_line = m.content.lines().next().unwrap_or("").trim();
        let preview = if first_line.chars().count() > 60 {
            let mut s: String = first_line.chars().take(60).collect();
            s.push('\u{2026}');
            s
        } else if first_line.is_empty() {
            "(no content)".to_string()
        } else {
            first_line.to_string()
        };
        out.push(TimelineEntry {
            msg_idx: i,
            role: m.role,
            preview,
            ts: m.ts,
            tool_idx: None,
        });
        // Add tool call entries for this message.
        for (ti, t) in m.tool_results.iter().enumerate() {
            let tool_preview = if t.title.is_empty() {
                t.name.clone()
            } else {
                t.title.clone()
            };
            let preview = if tool_preview.chars().count() > 60 {
                let mut s: String = tool_preview.chars().take(60).collect();
                s.push('\u{2026}');
                s
            } else {
                tool_preview
            };
            out.push(TimelineEntry {
                msg_idx: i,
                role: m.role,
                preview,
                ts: m.ts,
                tool_idx: Some(ti),
            });
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    Local,
    Global,
}

impl SessionScope {
    pub fn toggle(self) -> Self {
        match self {
            SessionScope::Local => SessionScope::Global,
            SessionScope::Global => SessionScope::Local,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SessionScope::Local => "local",
            SessionScope::Global => "global",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPickerMode {
    Manage,
}

#[derive(Debug)]
pub struct SessionPickerState {
    pub mode: SessionPickerMode,
    pub scope: SessionScope,
    pub query: String,
    pub entries: Vec<crate::session::store::SessionSummary>,
    pub filtered: Vec<usize>,
    pub cursor: usize,
    pub scroll: usize,
    pub focus: PickerFocus,
}

impl SessionPickerState {
    pub fn new(mode: SessionPickerMode, cwd: &std::path::Path) -> Self {
        let mut s = Self {
            mode,
            scope: SessionScope::Local,
            query: String::new(),
            entries: Vec::new(),
            filtered: Vec::new(),
            cursor: 0,
            scroll: 0,
            focus: PickerFocus::Search,
        };
        s.reload(cwd);
        s
    }

    pub fn reload(&mut self, cwd: &std::path::Path) {
        let scope = match self.scope {
            SessionScope::Local => Some(cwd),
            SessionScope::Global => None,
        };
        self.entries = crate::session::store::list(scope).unwrap_or_default();
        self.rebuild_filter();
    }

    pub fn toggle_scope(&mut self, cwd: &std::path::Path) {
        self.scope = self.scope.toggle();
        self.cursor = 0;
        self.scroll = 0;
        self.reload(cwd);
    }

    pub fn rebuild_filter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                q.is_empty()
                    || e.title.to_lowercase().contains(&q)
                    || e.cwd.to_lowercase().contains(&q)
                    || e.id.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.cursor >= self.filtered.len() {
            self.cursor = self.filtered.len().saturating_sub(1);
        }
        if self.scroll > self.cursor {
            self.scroll = self.cursor;
        }
    }

    pub fn ensure_cursor_visible(&mut self, visible_rows: usize) {
        if visible_rows == 0 {
            return;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + visible_rows {
            self.scroll = self.cursor + 1 - visible_rows;
        }
    }

    pub fn selected_id(&self) -> Option<String> {
        self.filtered
            .get(self.cursor)
            .map(|&i| self.entries[i].id.clone())
    }

    pub fn selected_title(&self) -> Option<String> {
        self.filtered
            .get(self.cursor)
            .map(|&i| self.entries[i].title.clone())
    }
}

#[derive(Debug)]
pub struct SessionRenameState {
    pub target_id: Option<String>,
    pub title: String,
    pub cursor: usize,
}

impl SessionRenameState {
    pub fn new_current(title: &str) -> Self {
        Self {
            target_id: None,
            title: title.to_string(),
            cursor: title.len(),
        }
    }

    pub fn new_target(id: String, title: String) -> Self {
        let cursor = title.len();
        Self {
            target_id: Some(id),
            title,
            cursor,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AskState {
    pub question: String,
    pub options: Vec<String>,
    pub cursor: usize,
    pub answered: Option<String>,
    pub input: String,
    pub input_cursor: usize,
}

impl AskState {
    pub fn new(question: String, options: Vec<String>) -> Self {
        Self {
            question,
            options,
            cursor: 0,
            answered: None,
            input: String::new(),
            input_cursor: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TodoState {
    pub items: Vec<TodoItem>,
    pub cursor: usize,
    /// Index of the item being edited inline, or None.
    pub editing: Option<usize>,
    /// Buffer for the in-progress edit.
    pub edit_buffer: String,
}

impl TodoState {
    pub fn new(items: Vec<TodoItem>) -> Self {
        Self {
            items,
            cursor: 0,
            editing: None,
            edit_buffer: String::new(),
        }
    }

    /// Start editing the item at `idx`. Returns false if out of bounds.
    pub fn start_edit(&mut self, idx: usize) -> bool {
        if idx >= self.items.len() {
            return false;
        }
        self.editing = Some(idx);
        self.edit_buffer = self.items[idx].content.clone();
        true
    }

    /// Commit the edit buffer to the item.
    pub fn commit_edit(&mut self) {
        if let Some(idx) = self.editing {
            if idx < self.items.len() {
                self.items[idx].content = std::mem::take(&mut self.edit_buffer);
            }
        }
        self.editing = None;
    }

    /// Cancel the edit (discard buffer).
    pub fn cancel_edit(&mut self) {
        self.editing = None;
        self.edit_buffer.clear();
    }
}

#[derive(Debug, Clone)]
pub struct PlanState {
    pub title: String,
    pub content: String,
    pub approved: Option<bool>,
}

impl PlanState {
    pub fn new(title: String, content: String) -> Self {
        Self {
            title,
            content,
            approved: None,
        }
    }
}

/// Top-level state for the function panel.
#[derive(Debug)]
pub struct FunctionPanel {
    pub tabs: Vec<SidebarTab>,
    pub active: usize,
}

impl FunctionPanel {
    pub fn new() -> Self {
        Self {
            tabs: vec![],
            active: 0,
        }
    }

    pub fn active_kind_name(&self) -> &'static str {
        match self.tabs.get(self.active) {
            Some(SidebarTab::Notifications) => "notifications",
            Some(SidebarTab::Completion(_)) => "completion",
            Some(SidebarTab::Settings(_)) => "settings",
            Some(SidebarTab::ModelPicker(_)) => "model picker",
            Some(SidebarTab::ProviderPicker(_)) => "provider",
            Some(SidebarTab::ThinkingPicker(_)) => "thinking",
            Some(SidebarTab::TimelinePicker(_)) => "timeline",
            Some(SidebarTab::SessionPicker(_)) => "sessions",
            Some(SidebarTab::SessionRename(_)) => "rename",
            Some(SidebarTab::Ask(_)) => "ask",
            Some(SidebarTab::Todo(_)) => "todo",
            Some(SidebarTab::Plan(_)) => "plan",
            Some(SidebarTab::Hotkey) => "hotkey",
            None => "?",
        }
    }

    pub fn push(&mut self, tab: SidebarTab) {
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    /// Remove the active tab and return true. If the list is already
    /// empty, return false so the caller can fall back to clearing the
    /// input buffer instead.
    pub fn close_active(&mut self) -> bool {
        if self.tabs.is_empty() {
            return false;
        }
        if self.active < self.tabs.len() {
            self.tabs.remove(self.active);
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len().saturating_sub(1);
            }
        }
        true
    }

    /// True if any tab exists at all. Used by `maybe_hide_panel` to
    /// decide whether to hide the function panel after a tab is removed.
    pub fn has_any_tab(&self) -> bool {
        !self.tabs.is_empty()
    }
}

impl Default for FunctionPanel {
    fn default() -> Self {
        Self::new()
    }
}

/// Top-level app state.
pub struct App {
    pub config: Config,
    pub config_path: PathBuf,
    pub session: Session,
    pub session_id: String,
    pub session_title: String,
    pub mode: AppMode,
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

    pub reqwest: reqwest::Client,
    pub inflight: Option<InflightHandle>,

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
    /// Path to the persisted model-cache JSON file. Computed from
    /// `config_path` during construction.
    pub model_cache_path: std::path::PathBuf,
    /// Screen y-positions of thinking toggle lines, each paired with the
    /// index of the corresponding message. Populated after each render.
    pub thinking_toggle_rows: Vec<(u16, usize)>,
    /// Screen y-positions of tool result toggle lines, each paired with
    /// the index of the corresponding message. Populated after each render.
    pub tool_toggle_rows: Vec<(u16, usize, usize)>,
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
    pub paste_blocks: VecDeque<String>,
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

    /// Snapshot of cursor position and buffer length taken when the
    /// current burst started, so we can undo the inserted characters
    /// if the burst qualifies as a paste.
    pub burst_snapshot: Option<(Instant, usize, usize)>,
}

/// Mouse-driven text selection spanning the full TUI. Coordinates are
/// 0-based (column, row) screen cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub start: (u16, u16),
    pub end: (u16, u16),
    pub active: bool,
}

impl Selection {
    pub fn new(start: (u16, u16)) -> Self {
        Self {
            start,
            end: start,
            active: true,
        }
    }

    /// Normalized bounding box: top-left to bottom-right (inclusive).
    /// The selection rectangle always covers the full bounding box even
    /// when the drag went diagonally — we treat any cell inside the box
    /// as highlighted, which matches how GUI text selection is usually
    /// drawn.
    pub fn rect(&self) -> ((u16, u16), (u16, u16)) {
        let x_min = self.start.0.min(self.end.0);
        let y_min = self.start.1.min(self.end.1);
        let x_max = self.start.0.max(self.end.0);
        let y_max = self.start.1.max(self.end.1);
        ((x_min, y_min), (x_max, y_max))
    }

    pub fn clear(&mut self) {
        self.start = (0, 0);
        self.end = (0, 0);
        self.active = false;
    }
}

#[derive(Debug)]
pub struct InflightHandle {
    pub cancel: tokio::sync::watch::Sender<bool>,
    pub label: String,
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
            function: FunctionPanel::new(),
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
            inflight: None,
            cwd,
            should_quit: false,
            msg_tx: None,
            input_prompt_area: None,
            tui_selection: None,
            selected_text: None,
            tui_drag_start: None,
            input_cursor_screen: None,
            function_panel_cursor: None,
            paste_blocks: VecDeque::new(),
            last_paste_text: None,
            last_paste_at: None,
            paste_key_quota: 0,
            burst_buf: String::new(),
            burst_snapshot: None,
        };
        app.refresh_status_model_context();
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
    }

    /// Push a toast; if level is important and panel is hidden, force-show
    /// and bump pending_events. The Notifications tab is created on-demand
    /// — it no longer sits permanently at index 0.
    pub fn notify(
        &mut self,
        level: crate::function::notifications::ToastLevel,
        text: impl Into<String>,
    ) {
        let text = text.into();
        if level.is_important() {
            let notif_exists = self
                .function
                .tabs
                .iter()
                .any(|t| matches!(t, SidebarTab::Notifications));
            if notif_exists {
                for (i, t) in self.function.tabs.iter().enumerate() {
                    if matches!(t, SidebarTab::Notifications) {
                        self.function.active = i;
                        break;
                    }
                }
            } else {
                self.function.push(SidebarTab::Notifications);
            }
            if !self.function_visible {
                self.function_visible = true;
                self.pending_events = self.pending_events.saturating_add(1);
            }
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
            self.notify(
                crate::function::notifications::ToastLevel::Ok,
                format!("config saved to {}", self.config_path.display()),
            );
        }
    }

    pub fn save_current_session(&mut self) {
        // Sync todo items from the function panel to the session.
        for tab in &self.function.tabs {
            if let crate::function::SidebarTab::Todo(state) = tab {
                self.session.todo_items = state.items.clone();
                break;
            }
        }
        if self.session.messages.is_empty() {
            return;
        }
        self.ensure_prompt_title();
        if let Err(e) = crate::session::store::save(
            &self.session_id,
            &self.session_title,
            &self.cwd,
            &self.session,
        ) {
            self.notify(
                crate::function::notifications::ToastLevel::Fail,
                format!("save session: {e}"),
            );
        }
    }

    pub fn start_new_session(&mut self) {
        if !self.session.messages.is_empty() {
            self.save_current_session();
        }
        self.session.clear();
        self.session_id = crate::session::store::new_session_id();
        self.session_title = crate::session::store::default_title(&self.cwd);
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
        match crate::session::store::load(id) {
            Ok(stored) => {
                self.session = Session {
                    messages: stored.messages,
                    todo_items: stored.todo_items.clone(),
                    scroll: 0,
                    streaming_id: None,
                    display: self.config.thinking_display,
                    tool_display: self.config.tool_display,
                    line_cache: Default::default(),
                    message_lines_cache: Default::default(),
                    cached_total_lines: None,
                    layout_version: 0,
                    render_cache: Default::default(),
                };
                self.session.invalidate_layout_cache();
                if !stored.todo_items.is_empty() {
                    self.open_todo(stored.todo_items);
                }
                self.session_id = stored.id;
                self.session_title = stored.title;
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
                    line_cache: Default::default(),
                    message_lines_cache: Default::default(),
                    cached_total_lines: None,
                    layout_version: 0,
                    render_cache: Default::default(),
                };
                self.session.invalidate_layout_cache();
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
        self.status.set_mode(mode.as_str());
    }

    pub fn open_ask(&mut self, question: String, options: Vec<String>) {
        self.set_mode(AppMode::Ask);
        // Reuse existing Ask tab if present, otherwise push a new one.
        if let Some((i, _)) = self
            .function
            .tabs
            .iter_mut()
            .enumerate()
            .find(|(_, t)| matches!(t, SidebarTab::Ask(_)))
        {
            self.function.tabs[i] = SidebarTab::Ask(AskState::new(question, options));
            self.function.active = i;
        } else {
            self.function
                .push(SidebarTab::Ask(AskState::new(question, options)));
        }
        self.function_visible = true;
        self.acknowledge_panel();
    }

    pub fn open_todo(&mut self, items: Vec<TodoItem>) {
        self.set_mode(AppMode::Todo);
        // Reuse existing Todo tab if present, otherwise push a new one.
        if let Some((i, _)) = self
            .function
            .tabs
            .iter_mut()
            .enumerate()
            .find(|(_, t)| matches!(t, SidebarTab::Todo(_)))
        {
            self.function.tabs[i] = SidebarTab::Todo(TodoState::new(items));
            self.function.active = i;
        } else {
            self.function.push(SidebarTab::Todo(TodoState::new(items)));
        }
        self.function_visible = true;
        self.acknowledge_panel();
    }

    pub fn open_plan(&mut self, title: String, content: String) {
        self.set_mode(AppMode::Plan);
        // Reuse existing Plan tab if present, otherwise push a new one.
        if let Some((i, _)) = self
            .function
            .tabs
            .iter_mut()
            .enumerate()
            .find(|(_, t)| matches!(t, SidebarTab::Plan(_)))
        {
            self.function.tabs[i] = SidebarTab::Plan(PlanState::new(title, content));
            self.function.active = i;
        } else {
            self.function
                .push(SidebarTab::Plan(PlanState::new(title, content)));
        }
        self.function_visible = true;
        self.acknowledge_panel();
    }

    /// Mark that the panel was shown by user (Ctrl+N) so we clear pending marker.
    pub fn acknowledge_panel(&mut self) {
        self.pending_events = 0;
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
                }));
                // Typing `/` is a "function trigger" — the user must see
                // the candidate list, so auto-show the panel and focus
                // the new Completion tab.
                self.function.active = self.function.tabs.len() - 1;
                self.function_visible = true;
                self.acknowledge_panel();
            }
        }
    }

    /// Hide the function panel when the last tab is removed. Called
    /// after any tab removal so the panel returns to the default hidden
    /// state when nothing is open.
    pub fn maybe_hide_panel(&mut self) {
        if !self.function.has_any_tab() {
            self.function_visible = false;
        }
    }
}

pub fn active_provider_string(_kind: ProviderKind) -> &'static str {
    "ok"
}

pub fn _ignore_unused() -> VecDeque<()> {
    VecDeque::new()
}
