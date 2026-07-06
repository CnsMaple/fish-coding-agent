use crate::config::{Config, ProviderId, ProviderKind};
use crate::function::notifications::{HitRate, ModelCache, Notifications, TokenRate};
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
    pub scroll: usize,
}

impl CompletionState {
    pub fn new(prefix: &str) -> Self {
        let candidates = crate::input::completion_candidates_for(prefix);
        let mut s = Self {
            candidates,
            cursor: 0,
            scroll: 0,
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
    Plan,
    Yolo,
    Shell,
    ShellContext,
    Python,
    PythonContext,
}

impl AppMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AppMode::Plan => "plan",
            AppMode::Yolo => "yolo",
            AppMode::Shell => "shell",
            AppMode::ShellContext => "shell_context",
            AppMode::Python => "python",
            AppMode::PythonContext => "python_context",
        }
    }
}

/// State for the todo-list sidebar tab.
#[derive(Debug, Clone)]
pub struct TodoTabState {
    pub scroll: usize,
    pub cursor: usize,
    /// Index of the todo item currently being edited. `None` when not editing.
    pub editing: Option<usize>,
}

impl TodoTabState {
    pub fn new() -> Self {
        Self { scroll: 0, cursor: 0, editing: None }
    }
}

impl Default for TodoTabState {
    fn default() -> Self {
        Self::new()
    }
}

/// One sidebar tab entry.
#[derive(Debug)]
pub enum SidebarTab {
    Notifications,
    Completion(CompletionState),
    PastePreview(Box<PastePreviewState>),
    Settings(Box<SettingsState>),
    ModelPicker(ModelPickerState),
    ProviderPicker(ProviderPickerState),
    ThinkingPicker(ThinkingPickerState),
    TimelinePicker(TimelinePickerState),
    SessionPicker(SessionPickerState),
    SessionRename(SessionRenameState),
    Plan(PlanState),
    Ask(AskState),
    Todo(TodoTabState),
    Hotkey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusTarget {
    Input,
    FunctionPanel,
}

impl SidebarTab {
    /// Minimum panel height (including borders) required for this tab's
    /// content to render without clipping.
    pub fn min_panel_height(&self) -> u16 {
        match self {
            Self::PastePreview(s) => {
                if s.image.is_some() {
                    5
                } else if let Some(ref text) = s.text {
                    let n = text.lines().count().min(5) as u16;
                    (n + 1 + 2).min(8)
                } else {
                    4
                }
            }
            Self::Notifications => 5,
            Self::Completion(_) => 3,
            Self::Settings(_) => 5,
            Self::ModelPicker(_) => 5,
            Self::ProviderPicker(_) => 5,
            Self::ThinkingPicker(_) => 4,
            Self::TimelinePicker(_) => 5,
            Self::SessionPicker(_) => 5,
            Self::SessionRename(_) => 4,
            Self::Plan(_) => 6,
            Self::Ask(_) => 5,
            Self::Todo(_) => 5,
            Self::Hotkey => 3,
        }
    }

    /// Actual panel height for this tab: for `PastePreview`, the exact
    /// content height (capped at the percentage); for other tabs, the
    /// percentage height (never below the minimum).
    pub fn panel_height(&self, pct_height: u16) -> u16 {
        match self {
            Self::PastePreview(s) => {
                let content_lines = if s.image.is_some() { 2 }
                    else if let Some(ref text) = s.text { text.lines().count().min(5) as u16 }
                    else { 1 };
                (content_lines + 1 + 2).min(pct_height)
            }
            _ => pct_height.max(self.min_panel_height()),
        }
    }
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
    /// Auto-compact on/off toggle.
    AutoCompact,
    /// Inline number stepper for `Config::tool_preview_lines`.
    ToolPreviewLines,
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
            SettingsLevel::AutoCompact => "settings / auto compact".to_string(),
            SettingsLevel::ToolPreviewLines => "settings / tool preview lines".to_string(),
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
            |             SettingsLevel::ThemeList => "Up/Down: nav | Enter: select | Esc: back",
            SettingsLevel::AutoCompact => "Up/Down: nav | Enter: select | Esc: back",
            SettingsLevel::ToolPreviewLines => "Up/Down: ±1 | Esc: back",
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
    /// Scroll offset for the list view.
    pub scroll: usize,
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
            scroll: 0,
            form_error: None,
            load_error: None,
            new_provider: NewProviderPickerState::new(),
        }
    }

    /// Number of items in the current list view (used to clamp cursor).
    pub fn list_len(&self, cfg: &Config) -> usize {
        match &self.level {
            SettingsLevel::TopLevel => 8, // set provider, thinking display, tool display, enter behavior, border type, theme, auto compact, tool preview lines
            SettingsLevel::ProviderList => 1 + cfg.configured_provider_ids().len(), // new + existing
            SettingsLevel::NewProviderKind => self.new_provider.filtered.len(),
            SettingsLevel::ExistingActions(_) => 2, // edit, delete
            SettingsLevel::ConfigForm(form) => form.active_fields().len(),
            SettingsLevel::ThinkingDisplayList => 3, // show, hide, while streaming
            SettingsLevel::ToolResultDisplayList => 3, // show, hide, while streaming
            SettingsLevel::EnterBehaviorList => 2,   // enter sends, enter newline
            SettingsLevel::BorderTypeList => 2,      // ascii, rounded
            SettingsLevel::ThemeList => crate::theme::ThemeVariant::all().len(),
            SettingsLevel::AutoCompact => 2, // on, off
            SettingsLevel::ToolPreviewLines => 1, // single-row stepper, the value lives in cfg
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

    fn adjust_scroll(&mut self) {
        // Without a known viewport, keep scroll near cursor (best effort).
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
    }
}

/// State for the clipboard paste preview panel. Opened by Ctrl+V
/// to show the user what's on the clipboard before confirming.
#[derive(Debug, Clone)]
pub struct PastePreviewState {
    /// Text content from clipboard, if any.
    pub text: Option<String>,
    /// Image attachment saved to disk, if clipboard had an image.
    pub image: Option<crate::session::ImageAttachment>,
    /// Raw image bytes for rendering (in-memory cache).
    pub image_bytes: Option<Vec<u8>>,
    /// MIME type of the clipboard content for display.
    pub media_type: Option<String>,
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
    /// Vertical scroll offset (top visible row in the list) so the
    /// focused row stays in view.
    pub scroll: usize,
}

impl ThinkingPickerState {
    pub fn new() -> Self {
        let mut s = Self {
            cursor: 0,
            query: String::new(),
            filtered: Vec::new(),
            scroll: 0,
        };
        s.rebuild_filter();
        s
    }

    pub const LEVELS: &'static [&'static str] = &["off", "minimal", "low", "medium", "high", "xhigh", "adaptive", "max"];

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
        if self.scroll > self.cursor {
            self.scroll = self.cursor;
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
pub struct PlanState {
    pub title: String,
    pub content: String,
    pub approved: Option<bool>,
    /// Absolute path of the file the plan was written to. `None`
    /// until the user explicitly saves the plan with the `S` key.
    pub path: Option<std::path::PathBuf>,
    /// `true` when the plan has unsaved changes — i.e. it has not
    /// been persisted to disk yet. The tab is rendered with a "press
    /// S to save" hint while this is set.
    pub dirty: bool,
}

impl PlanState {
    pub fn new(title: String, content: String) -> Self {
        Self {
            title,
            content,
            approved: None,
            path: None,
            dirty: true,
        }
    }

    pub fn with_path(mut self, path: std::path::PathBuf) -> Self {
        self.path = Some(path);
        self.dirty = false;
        self
    }
}

/// One question in the AskState stack. Options are the model-supplied
/// choices; the last visible row in the picker for THIS question is
/// the implicit "Type your own answer…" choice, which is not stored
/// here. `cursor` is the per-question picker row (only meaningful in
/// `AskPhase::Asking`).
#[derive(Debug, Clone)]
pub struct AskItem {
    pub question: String,
    pub options: Vec<String>,
    pub cursor: usize,
    pub scroll: usize,
    pub answered: Option<String>,
}

impl AskItem {
    pub fn new(question: String, options: Vec<String>) -> Self {
        Self {
            question,
            options,
            // 0 is the first option; the final row (the implicit
            // freeform input) is index `options.len()`.
            cursor: 0,
            scroll: 0,
            answered: None,
        }
    }

    /// Number of rows in the picker for THIS question: every option
    /// plus one for the implicit "Type your own answer…" row.
    pub fn row_count(&self) -> usize {
        self.options.len() + 1
    }
}

/// Phase of the ask picker.
///
/// - `Asking` — the user is picking an answer for the active
///   question. Enter writes the answer (advancing or going to
///   Reviewing when every question is answered), Up/Down moves the
///   picker row, Left/Right switches the active question.
/// - `Reviewing` — every question has an answer; the tab shows a
///   read-only preview of all Q/A pairs. Enter sends the whole batch
///   to the LLM; Esc goes back to `Asking` so the user can fix one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskPhase {
    Asking,
    Reviewing,
}

/// Multiple `ask` tool calls (possibly issued in parallel) are
/// queued into a single `AskState`. The tab strip in the UI shows
/// every question as a sub-tab (`Q1 Q2 Q3 Confirm`) and one
/// question is rendered at a time.
#[derive(Debug, Clone)]
pub struct AskState {
    pub items: Vec<AskItem>,
    /// Active question in `Asking` phase; the picker shows this
    /// question's options.
    pub active: usize,
    pub phase: AskPhase,
    /// Scroll offset for the body view.
    pub scroll: usize,
}

impl AskState {
    pub fn new(question: String, options: Vec<String>) -> Self {
        Self {
            items: vec![AskItem::new(question, options)],
            active: 0,
            phase: AskPhase::Asking,
            scroll: 0,
        }
    }

    /// Append a new question (typically from a parallel `ask` tool
    /// call). The new question becomes the active one so the user
    /// can answer it next.
    pub fn push(&mut self, question: String, options: Vec<String>) {
        self.items.push(AskItem::new(question, options));
        self.active = self.items.len() - 1;
        // Adding a question puts us back in the asking phase: the
        // user has at least one new thing to answer.
        self.phase = AskPhase::Asking;
    }

    /// Number of rows in the picker for the active question
    /// (options + implicit "Type your own…" choice).
    pub fn row_count(&self) -> usize {
        self.items
            .get(self.active)
            .map(|it| it.row_count())
            .unwrap_or(0)
    }

    /// True if every question in the queue has an answer.
    pub fn all_answered(&self) -> bool {
        !self.items.is_empty() && self.items.iter().all(|it| it.answered.is_some())
    }

    /// Find the next unanswered question after `from`, wrapping.
    /// Returns `None` if everything is already answered.
    pub fn next_unanswered(&self, from: usize) -> Option<usize> {
        let n = self.items.len();
        if n == 0 {
            return None;
        }
        for offset in 0..n {
            let idx = (from + offset) % n;
            if self.items[idx].answered.is_none() {
                return Some(idx);
            }
        }
        None
    }

    /// Build the user-message that gets sent to the LLM in the
    /// reviewing phase. One paragraph per Q/A pair, in the original
    /// order. Free-form answers are stored verbatim (the user typed
    /// them in the main input, prefixed with the question by the
    /// `submit_input` flow that triggered the answer).
    pub fn build_summary(&self) -> String {
        let mut out = String::from("(Answers to your questions:)\n");
        for (i, it) in self.items.iter().enumerate() {
            if let Some(ans) = &it.answered {
                out.push_str(&format!("\nQ{}. {}\n   A. {}\n", i + 1, it.question, ans));
            }
        }
        out.push_str("\n(All questions answered. Proceed.)");
        out
    }

    /// Build the dismiss message for Esc. Includes any answers the
    /// user already gave (they were on record), and notes each
    /// unanswered question as "dismissed" so the LLM knows to fill
    /// in defaults or proceed.
    pub fn build_dismiss_summary(&self) -> String {
        let mut out = String::from("(Ask round dismissed by the user.)\n");
        for (i, it) in self.items.iter().enumerate() {
            match &it.answered {
                Some(ans) => out.push_str(&format!(
                    "\nQ{}. {}\n   A. {}\n",
                    i + 1,
                    it.question,
                    ans
                )),
                None => out.push_str(&format!(
                    "\nQ{}. {}\n   A. (dismissed — no explicit answer)\n",
                    i + 1,
                    it.question
                )),
            }
        }
        out.push_str("\n(Proceed using the answers above, or sensible defaults.)");
        out
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

    #[allow(dead_code)]
    pub fn active_kind_name(&self) -> &'static str {
        match self.tabs.get(self.active) {
            Some(SidebarTab::Notifications) => "notifications",
            Some(SidebarTab::PastePreview(_)) => "paste",
            Some(SidebarTab::Completion(_)) => "completion",
            Some(SidebarTab::Settings(_)) => "settings",
            Some(SidebarTab::ModelPicker(_)) => "model picker",
            Some(SidebarTab::ProviderPicker(_)) => "provider",
            Some(SidebarTab::ThinkingPicker(_)) => "thinking",
            Some(SidebarTab::TimelinePicker(_)) => "timeline",
            Some(SidebarTab::SessionPicker(_)) => "sessions",
            Some(SidebarTab::SessionRename(_)) => "rename",
            Some(SidebarTab::Plan(_)) => "plan",
            Some(SidebarTab::Ask(_)) => "ask",
            Some(SidebarTab::Todo(_)) => "todo",
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

/// A request that `commands::send_message` (or
/// `event::submit_direct_tool_input`) prepared but did not yet spawn.
///
/// We defer the actual `tokio::spawn` until the next `terminal.draw(...)`
/// returns, so the freshly-pushed user message is on screen before the
/// HTTP / tool call goes out. While the request is sitting here, the
/// spinner / pending tool block is already visible (`inflight` is set),
/// and Esc can be pressed to silently drop the request without
/// dispatching it.
///
/// The cancel channel is split: `cancel_tx` lives in
/// `App::inflight` (so the existing cancel UI works unchanged), and
/// `cancel_rx` lives here so the spawned task can poll it once it
/// actually starts.
#[derive(Debug)]
pub enum PendingRequest {
    Chat(ChatPending),
    Tool(ToolPending),
}

#[derive(Debug)]
pub struct ChatPending {
    pub client: reqwest::Client,
    pub base: String,
    pub key: String,
    pub req: crate::providers::ChatRequest,
    pub provider: ProviderKind,
    pub cwd: PathBuf,
    pub agent: crate::permission::Agent,
    pub cancel_rx: tokio::sync::watch::Receiver<bool>,
    pub tx: tokio::sync::mpsc::UnboundedSender<crate::event::AppMsg>,
    /// See `App::current_request_seq`. Forwarded into the spawned
    /// chat task so its final `ChatDone`/`ChatError` is recognized
    /// (or ignored) by the main loop.
    pub seq: u64,
}

#[derive(Debug)]
pub struct ToolPending {
    pub name: String,
    pub title: String,
    pub args: String,
    pub include_context: bool,
    pub cwd: PathBuf,
    pub cancel_rx: tokio::sync::watch::Receiver<bool>,
    pub tx: tokio::sync::mpsc::UnboundedSender<crate::event::AppMsg>,
    /// See `App::current_request_seq`. Forwarded into the spawned
    /// tool task so its final `ChatDone`/`ChatError` is recognized
    /// (or ignored) by the main loop.
    pub seq: u64,
}

/// Progressive cancellation state for inflight requests.
/// First Esc switches to Confirming, second Esc cancels.
/// Falls back to Idle after 2 seconds of no input.
#[derive(Clone, Debug, Default)]
pub enum CancelState {
    #[default]
    Idle,
    /// First Esc was pressed; waiting for second Esc or 2s timeout.
    Confirming(Instant),
}

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
    /// Timestamp of the last mouse event. Used to detect stale drags
    /// when the mouse leaves and re-enters the terminal.
    pub last_mouse_event: Option<Instant>,
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
    /// The `current_request_seq` value at the time this inflight was
    /// created. The chat task tags its final `ChatDone`/`ChatError`
    /// with this number; `handle_msg` compares it against
    /// `App::current_request_seq` and ignores mismatches. See the
    /// field-level comment on `App::current_request_seq`.
    pub seq: u64,
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
            compacting: false,
            pending_post_compaction_prompt: None,
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
                self.show_panel();
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
            provider,
            model,
            thinking,
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
        self.image_blocks.clear();
        // Remove the todo tab when the session is cleared.
        self.function.tabs.retain(|t| !matches!(t, SidebarTab::Todo(_)));
        if self.function.active >= self.function.tabs.len() {
            self.function.active = self.function.tabs.len().saturating_sub(1);
        }
        self.maybe_hide_panel();
        // Land at the tail immediately; cancel any in-flight momentum.
        self.set_scroll_anchored(0);
    }

    /// Pin the session viewport to a specific scroll offset, cancelling
    /// any in-flight momentum animation. Use this for programmatic
    /// scrolls (submit, jump-to-message, new session, etc.) that should
    /// not coast. The integer offset is written to `session.scroll` so
    /// the existing render path picks it up, and the render cache is
    /// invalidated so the change is visible on the next frame.
    pub fn set_scroll_anchored(&mut self, value: u16) {
        self.session_scroll.snap(value as f32);
        self.session.scroll = value;
        if let Ok(mut c) = self.session.render_cache.lock() {
            *c = None;
        }
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
                    tool_preview_lines: self.config.tool_preview_lines,
                    line_cache: Default::default(),
                    message_lines_cache: Default::default(),
                    cached_total_lines: None,
                    layout_version: 0,
                    render_cache: Default::default(),
                    last_rendered_total: None,
                    expand_new_tool_results: false,
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
                self.focus_target = FocusTarget::Input;
                self.function_panel_cursor = None;
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
                    tool_preview_lines: self.config.tool_preview_lines,
                    line_cache: Default::default(),
                    message_lines_cache: Default::default(),
                    cached_total_lines: None,
                    layout_version: 0,
                    render_cache: Default::default(),
                    last_rendered_total: None,
                    expand_new_tool_results: false,
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
            self.show_panel();
        }
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
                self.show_panel();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_app() -> App {
        use crate::config::{make_id, Config, ProviderConfig, ProviderKind, ProviderMode};
        use crate::function::notifications::Notifications;
        let mut cfg = Config::default();
        let kind = ProviderKind::Openai;
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
        cfg.active = Some(make_id(ProviderKind::Openai, ProviderMode::Key));
        let tmp = std::env::temp_dir().join("fish-coding-agent-fns-test.json");
        let _ = std::fs::remove_file(&tmp);
        let cache_file = tmp.parent().unwrap_or(&tmp).join("model-cache.json");
        App {
            config: cfg,
            config_path: tmp,
            session: Session::default(),
            session_id: crate::session::store::new_session_id(),
            session_title: "test".to_string(),
            mode: AppMode::Yolo,
            previous_mode: AppMode::Yolo,
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
            focus_target: FocusTarget::Input,
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
            last_mouse_event: None,
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
        }
    }

    #[test]
    fn plan_state_is_dirty_on_open_and_clears_after_save() {
        let mut app = make_test_app();
        app.open_plan("t".to_string(), "body".to_string());
        let state = match app.function.tabs.first().unwrap() {
            SidebarTab::Plan(s) => s.clone(),
            _ => panic!("expected plan tab"),
        };
        assert!(state.dirty, "open_plan must start dirty");
        assert!(state.path.is_none(), "open_plan must NOT auto-save");

        // save_active_plan writes to the user's real config dir. We
        // accept either true (write succeeded) or false (sandbox
        // blocks disk), but if it returned true the path must be
        // populated and dirty must be false.
        let ok = app.save_active_plan();
        if ok {
            let state = match app.function.tabs.first().unwrap() {
                SidebarTab::Plan(s) => s.clone(),
                _ => panic!(),
            };
            assert!(!state.dirty);
            assert!(state.path.is_some());
        }
    }

    #[test]
    fn open_ask_pushes_first_question() {
        let mut app = make_test_app();
        app.open_ask("Q?".to_string(), vec!["a".to_string(), "b".to_string()]);
        // Notifications tab at 0, Ask tab at 1.
        let state = match app.function.tabs.get(1) {
            Some(SidebarTab::Ask(s)) => s.clone(),
            _ => panic!("expected ask tab"),
        };
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].question, "Q?");
        assert_eq!(state.items[0].options, vec!["a", "b"]);
        // The per-question cursor starts on the first option.
        assert_eq!(state.items[0].cursor, 0);
    }

    #[test]
    fn open_ask_appends_to_existing_tab() {
        let mut app = make_test_app();
        app.open_ask("first".to_string(), vec!["a".to_string()]);
        app.open_ask("second".to_string(), vec!["x".to_string(), "y".to_string()]);
        let state = match app.function.tabs.get(1) {
            Some(SidebarTab::Ask(s)) => s.clone(),
            _ => panic!(),
        };
        assert_eq!(state.items.len(), 2);
        // Adding a question makes it the active one so the user
        // answers it next.
        assert_eq!(state.active, 1);
        assert_eq!(state.items[1].question, "second");
    }

    #[test]
    fn ask_row_count_includes_options_and_freeform() {
        let s = AskState::new("q".to_string(), vec!["a".into(), "b".into(), "c".into()]);
        // The picker for this question has 3 options + 1 implicit
        // "Type your own answer…" row.
        assert_eq!(s.items[0].row_count(), 4);
        assert_eq!(s.row_count(), 4);
    }

    #[test]
    fn ask_all_answered_after_picking_last() {
        let mut s = AskState::new("q".to_string(), vec!["a".into()]);
        s.items[0].answered = Some("a".to_string());
        assert!(s.all_answered());
    }

    #[test]
    fn ask_all_answered_false_when_pending() {
        let s = AskState::new("q".to_string(), vec!["a".into()]);
        assert!(!s.all_answered());
    }

    #[test]
    fn ask_next_unanswered_wraps() {
        let mut s = AskState::new("q1".to_string(), vec!["a".into()]);
        s.push("q2".to_string(), vec!["b".into()]);
        s.push("q3".to_string(), vec!["c".into()]);
        s.items[0].answered = Some("a".to_string());
        s.items[2].answered = Some("c".to_string());
        // From index 1, the next unanswered is index 1 itself.
        assert_eq!(s.next_unanswered(1), Some(1));
        // From index 2 (answered), wrap and find index 1.
        assert_eq!(s.next_unanswered(2), Some(1));
        // From index 0 (answered), wrap and find index 1.
        assert_eq!(s.next_unanswered(0), Some(1));
    }

    #[test]
    fn ask_build_summary_lists_all_pairs() {
        let mut s = AskState::new("Q1?".to_string(), vec!["a".into()]);
        s.push("Q2?".to_string(), vec!["x".into()]);
        s.items[0].answered = Some("a".to_string());
        s.items[1].answered = Some("x".to_string());
        let summary = s.build_summary();
        assert!(summary.contains("Q1"));
        assert!(summary.contains("Q2"));
        assert!(summary.contains("a"));
        assert!(summary.contains("x"));
        assert!(summary.contains("Proceed"));
    }

    #[test]
    fn thinking_picker_ensure_cursor_visible_scrolls_down() {
        use crate::function::ThinkingPickerState;
        let mut s = ThinkingPickerState::new();
        s.cursor = 4;
        crate::ui::function_panel::ensure_cursor_visible(s.cursor, &mut s.scroll, 3);
        assert_eq!(s.scroll, 2, "scroll should jump so cursor is last visible row");
    }

    #[test]
    fn thinking_picker_ensure_cursor_visible_scrolls_up() {
        use crate::function::ThinkingPickerState;
        let mut s = ThinkingPickerState::new();
        s.scroll = 4;
        s.cursor = 0;
        crate::ui::function_panel::ensure_cursor_visible(s.cursor, &mut s.scroll, 3);
        assert_eq!(s.scroll, 0, "scroll should follow cursor up to top");
    }

    #[test]
    fn thinking_picker_no_scroll_when_fits() {
        use crate::function::ThinkingPickerState;
        let mut s = ThinkingPickerState::new();
        s.scroll = 0;
        s.cursor = 1;
        crate::ui::function_panel::ensure_cursor_visible(s.cursor, &mut s.scroll, 3);
        assert_eq!(s.scroll, 0, "no scroll needed when total fits visible");
    }

    /// `push` places the new question at the end and makes it
    /// active so the user can answer it next.
    #[test]
    fn ask_push_makes_new_question_active() {
        let mut s = AskState::new("q1".to_string(), vec!["a".into()]);
        s.items[0].cursor = 1; // user has scrolled within q1
        s.push("q2".to_string(), vec!["x".into()]);
        assert_eq!(s.active, 1);
        assert_eq!(s.items[1].cursor, 0);
    }
}
