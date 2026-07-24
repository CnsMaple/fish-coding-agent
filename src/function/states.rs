use crate::config::{Config, ProviderId, ProviderKind};
use crate::function::notifications::ModelInfo;
use crate::session::{Role, Session};
use chrono::{DateTime, Utc};
use std::path::PathBuf;
use std::time::Instant;

/// Trait for pickers that have a search-box + filtered-list pattern
/// (ProviderPicker, ModelPicker, TimelinePicker). Provides default
/// implementations for the common Search/List key handling so the
/// per-picker handler only needs to implement `on_enter`.
pub trait FilterablePicker {
    fn query(&mut self) -> &mut String;
    fn filtered(&self) -> &[usize];
    fn cursor(&mut self) -> &mut usize;
    fn focus(&mut self) -> &mut PickerFocus;
    fn rebuild_filter(&mut self);

    /// Handle a key in Search focus mode. Returns `Some(true)` if the
    /// key was consumed, `Some(false)` if the global handler should
    /// close the tab (Esc with empty query), or `None` if the key was
    /// not recognised (caller falls through).
    fn handle_search_key(&mut self, k: crossterm::event::KeyEvent) -> Option<bool> {
        use crossterm::event::KeyCode;
        match k.code {
            KeyCode::Esc => {
                if self.query().is_empty() {
                    return Some(false);
                }
                self.query().clear();
                self.rebuild_filter();
                Some(true)
            }
            KeyCode::Down => {
                *self.focus() = PickerFocus::List;
                Some(true)
            }
            KeyCode::Backspace => {
                self.query().pop();
                self.rebuild_filter();
                Some(true)
            }
            KeyCode::Char(c) => {
                self.query().push(c);
                self.rebuild_filter();
                Some(true)
            }
            _ => None,
        }
    }

    /// Handle a key in List focus mode. Returns `Some(true)` if the
    /// key was consumed, `Some(false)` if the global handler should
    /// close the tab, or `None` for the caller to fall through.
    fn handle_list_key(&mut self, k: crossterm::event::KeyEvent) -> Option<bool> {
        use crossterm::event::KeyCode;
        match k.code {
            KeyCode::Up => {
                if *self.cursor() > 0 {
                    *self.cursor() -= 1;
                }
                Some(true)
            }
            KeyCode::Down => {
                if *self.cursor() + 1 < self.filtered().len() {
                    *self.cursor() += 1;
                }
                Some(true)
            }
            KeyCode::Tab | KeyCode::BackTab => {
                *self.focus() = PickerFocus::Search;
                Some(true)
            }
            KeyCode::Char(c) => {
                self.query().push(c);
                *self.focus() = PickerFocus::Search;
                self.rebuild_filter();
                Some(true)
            }
            KeyCode::Backspace => {
                self.query().pop();
                *self.focus() = PickerFocus::Search;
                self.rebuild_filter();
                Some(true)
            }
            _ => None,
        }
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
        Self {
            scroll: 0,
            cursor: 0,
            editing: None,
        }
    }
}

impl Default for TodoTabState {
    fn default() -> Self {
        Self::new()
    }
}

/// A single entry in the command palette (Ctrl+P).
#[derive(Debug, Clone)]
pub enum PaletteEntry {
    Command {
        name: &'static str,
        description: &'static str,
    },
    Skill {
        name: String,
        description: String,
        selected: bool,
    },
}

impl PaletteEntry {
    /// Get the display name of this entry.
    pub fn name(&self) -> Option<&str> {
        match self {
            PaletteEntry::Command { name, .. } => Some(name),
            PaletteEntry::Skill { name, .. } => Some(name.as_str()),
        }
    }
}

/// Command-palette state, opened via Ctrl+P.
#[derive(Debug)]
pub struct CommandPaletteState {
    pub query: String,
    pub cursor: usize,
    pub scroll: usize,
    /// Filtered entries matching the current query.
    pub entries: Vec<PaletteEntry>,
}

impl CommandPaletteState {
    pub fn new() -> Self {
        let all = Self::build_all();
        Self {
            query: String::new(),
            cursor: 0,
            scroll: 0,
            entries: all,
        }
    }

    fn build_all() -> Vec<PaletteEntry> {
        let mut v: Vec<PaletteEntry> = vec![
            PaletteEntry::Command {
                name: "model",
                description: "Switch provider/model",
            },
            PaletteEntry::Command {
                name: "settings",
                description: "Open settings",
            },
            PaletteEntry::Command {
                name: "session",
                description: "Manage sessions",
            },
            PaletteEntry::Command {
                name: "timeline",
                description: "Jump to message",
            },
            PaletteEntry::Command {
                name: "think",
                description: "Configure thinking mode",
            },
            PaletteEntry::Command {
                name: "tool",
                description: "Toggle tools",
            },
            PaletteEntry::Command {
                name: "hotkey",
                description: "Show key bindings",
            },
            PaletteEntry::Command {
                name: "retry",
                description: "Retry last prompt",
            },
            PaletteEntry::Command {
                name: "continue",
                description: "Continue response",
            },
            PaletteEntry::Command {
                name: "compact",
                description: "Compact session",
            },
            PaletteEntry::Command {
                name: "new",
                description: "New session",
            },
            PaletteEntry::Command {
                name: "plan",
                description: "Switch to plan mode",
            },
            PaletteEntry::Command {
                name: "yolo",
                description: "Switch to yolo mode",
            },
            PaletteEntry::Command {
                name: "clear",
                description: "Clear session",
            },
        ];
        for skill in crate::skill::load_all() {
            v.push(PaletteEntry::Skill {
                name: skill.name,
                description: skill.description,
                selected: false,
            });
        }
        v
    }

    pub fn rebuild_filter(&mut self) {
        let q = self.query.to_lowercase();
        if q.is_empty() {
            self.entries = Self::build_all();
        } else if q == "skill" || q.starts_with("skill:") {
            // Show all skills, optionally filtered by the text after "skill:".
            let suffix = q.strip_prefix("skill:").unwrap_or("").trim();
            let all = Self::build_all();
            self.entries = all
                .into_iter()
                .filter(|e| match e {
                    PaletteEntry::Skill { name, .. } => {
                        if suffix.is_empty() {
                            true
                        } else {
                            name.to_lowercase().contains(suffix)
                                || crate::fuzzy::score(suffix, name).is_some()
                        }
                    }
                    PaletteEntry::Command { .. } => false,
                })
                .collect();
            self.clamp_cursor();
        } else {
            let mut scored: Vec<(u32, PaletteEntry)> = Self::build_all()
                .into_iter()
                .filter_map(|e| {
                    let name = e.name().unwrap_or("");
                    if name.to_lowercase().contains(&q) {
                        Some((crate::fuzzy::score(&q, name).unwrap_or(0), e))
                    } else {
                        crate::fuzzy::score(&q, name).map(|sc| (sc, e))
                    }
                })
                .collect();
            scored.sort_by_key(|&(sc, _)| sc);
            self.entries = scored.into_iter().map(|(_, e)| e).collect();
        }
        self.clamp_cursor();
    }

    fn clamp_cursor(&mut self) {
        if self.entries.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.entries.len() {
            self.cursor = self.entries.len().saturating_sub(1);
        }
    }

    pub fn move_up(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        if self.cursor == 0 {
            self.cursor = self.entries.len() - 1;
        } else {
            self.cursor -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1) % self.entries.len();
    }

    /// Toggle the `selected` flag of the skill at the current cursor.
    /// No-op for command entries.
    pub fn toggle_selected(&mut self) {
        if let Some(PaletteEntry::Skill {
            ref mut selected, ..
        }) = self.entries.get_mut(self.cursor)
        {
            *selected = !*selected;
        }
    }
}

impl Default for CommandPaletteState {
    fn default() -> Self {
        Self::new()
    }
}

/// One sidebar tab entry.
#[derive(Debug)]
pub enum SidebarTab {
    Notifications,
    PastePreview(Box<PastePreviewState>),
    Settings(Box<SettingsState>),
    ModelPicker(ModelPickerState),
    ProviderPicker(ProviderPickerState),
    ThinkingPicker(ThinkingPickerState),
    TimelinePicker(TimelinePickerState),
    SessionPicker(SessionPickerState),
    SessionRename(SessionRenameState),
    Plan(PlanState),
    Todo(TodoTabState),
    ToolPicker(ToolPickerState),
    Hotkey,
    CommandPalette(CommandPaletteState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusTarget {
    Input,
    FunctionPanel,
    AgentsCheckbox,
}

impl SidebarTab {
    /// Number of content lines this tab needs to display, given the
    /// current app state and available width. Used to compute a dynamic
    /// panel height that shrinks to fit the content and expands up to
    /// the 30% cap.
    pub fn content_lines(&self, app: &crate::function::App, _width: u16) -> usize {
        match self {
            Self::PastePreview(s) => {
                if s.image.is_some() {
                    2
                } else if let Some(ref text) = s.text {
                    text.lines().count().min(5)
                } else {
                    1
                }
            }
            Self::Notifications => {
                if app.notifications.searching {
                    app.notifications.filtered_indices().len()
                } else {
                    app.notifications.items.len()
                }
            }
            Self::Settings(s) => settings_body_lines(s, &app.config).len(),
            Self::ModelPicker(s) => {
                if s.fetching || s.fetch_error.is_some() || s.models.is_empty() {
                    1
                } else {
                    s.filtered.len().max(1)
                }
            }
            Self::ProviderPicker(s) => {
                if s.entries.is_empty() || s.filtered.is_empty() {
                    1
                } else {
                    s.filtered.len()
                }
            }
            Self::ThinkingPicker(s) => {
                if s.filtered.is_empty() {
                    1
                } else {
                    s.filtered.len()
                }
            }
            Self::TimelinePicker(s) => {
                if s.entries.is_empty() || s.filtered.is_empty() {
                    1
                } else {
                    s.filtered.len()
                }
            }
            Self::SessionPicker(s) => {
                if s.entries.is_empty() || s.filtered.is_empty() {
                    1
                } else {
                    s.filtered.len()
                }
            }
            Self::SessionRename(_) => 1,
            Self::Plan(_) => 3,
            Self::Todo(_) => app.session.todo_items.len().max(1),
            Self::ToolPicker(s) => {
                if s.filtered.is_empty() {
                    1
                } else {
                    s.filtered.len()
                }
            }
            Self::Hotkey => 18,
            Self::CommandPalette(s) => s.entries.len().max(1),
        }
    }

    /// True when the tab has a search/filter input row.
    pub fn has_search(&self) -> bool {
        matches!(
            self,
            Self::Notifications
                | Self::ModelPicker(_)
                | Self::ProviderPicker(_)
                | Self::ThinkingPicker(_)
                | Self::TimelinePicker(_)
                | Self::SessionPicker(_)
                | Self::ToolPicker(_)
                | Self::CommandPalette(_)
                | Self::Settings(_)
        )
    }

    /// Footer hint text. Empty string means no hint row.
    /// Tabs with dynamic hints return a placeholder " " so the
    /// layout reserves a hint row; their `render_hint` override
    /// draws the actual content.
    pub fn hint(&self) -> &'static str {
        match self {
            Self::Notifications => " ",
            Self::ModelPicker(_) => {
                " Enter: select | Ctrl+R: refresh | Ctrl+M: manual | Ctrl+E: edit | Esc: close "
            }
            Self::ProviderPicker(_) => {
                " Enter: pick | Up/Down: nav | type to filter | Ctrl+E: edit | Esc: close "
            }
            Self::ThinkingPicker(_) => "",
            Self::TimelinePicker(_) => {
                " Enter: jump to message | Up/Down: nav | Ctrl+E: edit | Esc: close "
            }
            Self::SessionPicker(_) => " ",
            Self::SessionRename(_) => " Enter: save | Ctrl+E: edit | Esc: close ",
            Self::Plan(_) => " Enter: approve | Alt+R: reject | Alt+S: save | Esc: close ",
            Self::Todo(_) => " ",
            Self::ToolPicker(_) => " Space: toggle | Enter: confirm | Esc: close ",
            Self::Hotkey => "",
            Self::PastePreview(_) => " Enter: paste | Esc: cancel ",
            Self::Settings(_) => " ",
            Self::CommandPalette(_) => " Enter: execute/skill | Space: toggle | Esc: close ",
        }
    }

    /// Fixed overhead lines for this tab: 2 (top+bottom border) + search
    /// row (0 or 1) + hint row (0 or 1).
    pub fn overhead(&self) -> u16 {
        let mut oh = 2u16;
        if self.has_search() {
            oh += 1;
        }
        if !self.hint().is_empty() {
            oh += 1;
        }
        oh
    }

    /// Dynamic panel height: `min(content_lines + overhead, pct_height)`,
    /// clamped to an absolute minimum of `overhead + 1`
    /// (borders + search/hint rows + at least 1 content line).
    pub fn panel_height(&self, pct_height: u16, app: &crate::function::App, width: u16) -> u16 {
        let content = self.content_lines(app, width) as u16;
        let h = content.saturating_add(self.overhead());
        h.min(pct_height).max(self.overhead() + 1)
    }
}

/// Compute the number of body lines for the settings tab — mirrors the
/// `body_lines.len()` calculation in `render_settings`.
pub fn settings_body_lines(
    s: &SettingsState,
    cfg: &crate::config::Config,
) -> Vec<ratatui::text::Line<'static>> {
    use crate::function::SettingsLevel;
    let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
    match &s.level {
        SettingsLevel::TopLevel => {
            let count = if s.query.is_empty() {
                8
            } else {
                s.filtered.len()
            };
            for _ in 0..count.max(1) {
                lines.push(ratatui::text::Line::raw(""));
            }
        }
        SettingsLevel::ProviderList => {
            lines.push(ratatui::text::Line::raw(""));
            for _ in cfg.configured_provider_ids() {
                lines.push(ratatui::text::Line::raw(""));
            }
        }
        SettingsLevel::NewProviderKind => {
            // Search line + list items.
            for _ in 0..s.new_provider.filtered.len().max(1) + 1 {
                lines.push(ratatui::text::Line::raw(""));
            }
        }
        SettingsLevel::ExistingActions(_) => {
            lines.push(ratatui::text::Line::raw(""));
            lines.push(ratatui::text::Line::raw(""));
        }
        SettingsLevel::ConfigForm(form) => {
            for _ in form.active_fields() {
                lines.push(ratatui::text::Line::raw(""));
            }
            if form.form_error.is_some() {
                lines.push(ratatui::text::Line::raw(""));
                lines.push(ratatui::text::Line::raw(""));
            }
        }
        SettingsLevel::ThinkingDisplayList | SettingsLevel::ToolResultDisplayList => {
            for _ in 0..3 {
                lines.push(ratatui::text::Line::raw(""));
            }
        }
        SettingsLevel::EnterBehaviorList
        | SettingsLevel::BorderTypeList
        | SettingsLevel::AutoCompact => {
            for _ in 0..2 {
                lines.push(ratatui::text::Line::raw(""));
            }
        }
        SettingsLevel::ThemeList => {
            for _ in 0..crate::theme::ThemeVariant::all().len() {
                lines.push(ratatui::text::Line::raw(""));
            }
        }
        SettingsLevel::ToolPreviewLines => {
            lines.push(ratatui::text::Line::raw(""));
        }
    }
    if s.load_error.is_some() {
        lines.push(ratatui::text::Line::raw(""));
        lines.push(ratatui::text::Line::raw(""));
    }
    lines
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
    /// models.dev provider ID (not user-editable, set automatically
    /// when created from models.dev picker).
    pub provider_id: String,
}

impl ConfigFormState {
    pub fn new_for_create(
        kind: crate::config::ProviderKind,
        mode: crate::config::ProviderMode,
    ) -> Self {
        let id = crate::config::make_id(kind, mode);
        let name = match kind {
            crate::config::ProviderKind::Cursor => "Cursor".to_string(),
            crate::config::ProviderKind::Volcengine => "Volcengine".to_string(),
            _ => String::new(),
        };
        let base_url = match kind {
            crate::config::ProviderKind::Cursor | crate::config::ProviderKind::Volcengine => {
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
            provider_id: String::new(),
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
            provider_id: cfg.provider_id.clone(),
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
            SettingsLevel::NewProviderKind => {
                "Up/Down: nav | type: filter | Enter: select | Esc: back"
            }
            SettingsLevel::ExistingActions(_) => "Up/Down: nav | Enter: select | Esc: back",
            SettingsLevel::ConfigForm(_) => {
                "Up/Down: nav | type: edit | Enter: confirm | Esc: back"
            }
            SettingsLevel::ThinkingDisplayList
            | SettingsLevel::ToolResultDisplayList
            | SettingsLevel::EnterBehaviorList
            | SettingsLevel::BorderTypeList
            | SettingsLevel::ThemeList => "Up/Down: nav | Enter: select | Esc: back",
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

    /// Load models.dev providers from the cache and append them to the
    /// entry list with a special `__md__/{name}/{provider_id}` prefix.
    /// Idempotent — already-present entries are not added again.
    pub fn load_model_dev_providers(&mut self, cache_path: &std::path::Path) {
        let model_data_path = cache_path.join("model-data.json");
        let Some(data) = crate::model_data::ModelData::load(&model_data_path) else {
            return;
        };
        let mut dev_entries: Vec<String> = data
            .providers
            .iter()
            .map(|(id, meta)| format!("__md__/{}/{}", meta.name, id))
            .collect();
        if dev_entries.is_empty() {
            return;
        }
        dev_entries.sort();
        // Filter out already-present entries.
        let existing: std::collections::HashSet<String> = self.entries.iter().cloned().collect();
        let insert_at = self
            .entries
            .iter()
            .position(|id| {
                crate::config::parse_id(id)
                    .map(|(k, _)| k == crate::config::ProviderKind::Openai)
                    .unwrap_or(false)
            })
            .unwrap_or(self.entries.len());
        for entry in dev_entries {
            if !existing.contains(&entry) {
                self.entries.insert(insert_at, entry);
            }
        }
        self.rebuild_filter();
    }

    pub fn picker_label(&self, id: &str) -> String {
        if let Some(rest) = id.strip_prefix("__md__/") {
            // Format: __md__/{name}/{provider_id}
            if let Some(name_end) = rest.rfind('/') {
                return format!("{} (models.dev)", &rest[..name_end]);
            }
            return format!("{} (models.dev)", rest);
        }
        crate::config::parse_id(id)
            .map(|(k, _)| k.picker_label().to_string())
            .unwrap_or_else(|| crate::config::id_display(id))
    }

    pub fn rebuild_filter(&mut self) {
        if self.query.is_empty() {
            self.filtered = (0..self.entries.len()).collect();
        } else {
            let mut scored: Vec<(u32, usize)> = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(i, id)| {
                    let label = self.picker_label(id);
                    crate::fuzzy::score(&self.query, id)
                        .or_else(|| crate::fuzzy::score(&self.query, &label))
                        .map(|sc| (sc, i))
                })
                .collect();
            scored.sort_by_key(|&(sc, i)| (sc, i));
            self.filtered = scored.into_iter().map(|(_, i)| i).collect();
        }
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
    /// Search query for TopLevel filtering.
    pub query: String,
    /// Filtered indices into top-level items (empty = show all).
    pub filtered: Vec<usize>,
}

impl SettingsState {
    pub fn new(cfg: &Config) -> Self {
        Self::with_cache(cfg, None)
    }

    pub fn with_cache(_cfg: &Config, model_cache_parent: Option<&std::path::Path>) -> Self {
        let mut state = Self {
            level: SettingsLevel::TopLevel,
            cursor: 0,
            scroll: 0,
            form_error: None,
            load_error: None,
            new_provider: NewProviderPickerState::new(),
            query: String::new(),
            filtered: Vec::new(),
        };
        if let Some(cache_path) = model_cache_parent {
            state.new_provider.load_model_dev_providers(cache_path);
        }
        state
    }

    /// The 8 top-level settings item labels.
    pub fn top_level_keys() -> [&'static str; 8] {
        [
            "set provider",
            "thinking display",
            "tool display",
            "enter behavior",
            "border type",
            "theme",
            "auto compact",
            "tool preview lines",
        ]
    }

    /// Rebuild the filtered index list for TopLevel based on `self.query`.
    pub fn rebuild_filter(&mut self) {
        if self.query.is_empty() {
            self.filtered.clear();
        } else {
            let q = self.query.to_lowercase();
            self.filtered = Self::top_level_keys()
                .iter()
                .enumerate()
                .filter_map(|(i, label)| {
                    if label.contains(&q) || crate::fuzzy::score(&q, label).is_some() {
                        Some(i)
                    } else {
                        None
                    }
                })
                .collect();
        }
    }

    /// Number of items in the current list view (used to clamp cursor).
    pub fn list_len(&self, cfg: &Config) -> usize {
        match &self.level {
            SettingsLevel::TopLevel => {
                if self.query.is_empty() {
                    8
                } else {
                    self.filtered.len()
                }
            }
            SettingsLevel::ProviderList => 1 + cfg.configured_provider_ids().len(), // new + existing
            SettingsLevel::NewProviderKind => self.new_provider.filtered.len(),
            SettingsLevel::ExistingActions(_) => 2, // edit, delete
            SettingsLevel::ConfigForm(form) => form.active_fields().len(),
            SettingsLevel::ThinkingDisplayList => 3, // show, hide, while streaming
            SettingsLevel::ToolResultDisplayList => 3, // show, hide, while streaming
            SettingsLevel::EnterBehaviorList => 2,   // enter sends, enter newline
            SettingsLevel::BorderTypeList => 2,      // ascii, rounded
            SettingsLevel::ThemeList => crate::theme::ThemeVariant::all().len(),
            SettingsLevel::AutoCompact => 2,      // on, off
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
    /// The specific configured entry this picker was opened for (e.g.
    /// `openai:key`). Multiple entries can share the same `provider` kind
    /// (e.g. a "prod" and "dev" OpenAI endpoint), so the kind alone is
    /// not enough to resolve credentials — fetches and commits must use
    /// this id when set. `None` only for legacy/picker-created-without-id
    /// paths, in which case callers fall back to kind-based resolution.
    pub entry_id: Option<crate::config::ProviderId>,
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
    /// Context window picker — when set, the model at this index needs
    /// the user to pick a context window. The picker shows options from
    /// models.dev plus a custom input.
    pub context_pick: Option<ContextPickerState>,
}

#[derive(Debug, Clone)]
pub struct ContextPickerState {
    /// Index of the model in `models` that needs a context window.
    pub model_idx: usize,
    /// Unique context window + modality combinations for this provider.
    pub options: Vec<crate::model_data::ContextOption>,
    /// Cursor position in the options list.
    pub cursor: usize,
    /// Custom input value (empty if not editing custom).
    pub custom_input: String,
    /// Focus: Options or CustomInput.
    pub focus: ContextPickerFocus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextPickerFocus {
    Options,
    CustomInput,
}

impl FilterablePicker for ModelPickerState {
    fn query(&mut self) -> &mut String {
        &mut self.query
    }
    fn filtered(&self) -> &[usize] {
        &self.filtered
    }
    fn cursor(&mut self) -> &mut usize {
        &mut self.cursor
    }
    fn focus(&mut self) -> &mut PickerFocus {
        &mut self.focus
    }
    fn rebuild_filter(&mut self) {
        ModelPickerState::rebuild_filter(self)
    }
}

impl ModelPickerState {
    pub fn new(provider: ProviderKind) -> Self {
        Self {
            provider,
            entry_id: None,
            query: String::new(),
            models: vec![],
            filtered: vec![],
            cursor: 0,
            focus: PickerFocus::List,
            fetching: false,
            fetch_error: None,
            no_endpoint: false,
            scroll: 0,
            context_pick: None,
        }
    }

    /// Construct a picker bound to a specific configured entry id. The
    /// kind is derived from the id; `entry_id` is stored so fetches and
    /// commits target this exact entry rather than the global active one
    /// (which may be a different entry of the same kind).
    pub fn new_for_entry(id: &str) -> Option<Self> {
        let (kind, _) = crate::config::parse_id(id)?;
        let mut s = Self::new(kind);
        s.entry_id = Some(id.to_string());
        Some(s)
    }

    pub fn rebuild_filter(&mut self) {
        if self.query.is_empty() {
            self.filtered = (0..self.models.len()).collect();
        } else {
            let mut scored: Vec<(u32, usize)> = self
                .models
                .iter()
                .enumerate()
                .filter_map(|(i, m)| {
                    crate::fuzzy::score(&self.query, &m.id)
                        .or_else(|| crate::fuzzy::score(&self.query, &m.display))
                        .map(|sc| (sc, i))
                })
                .collect();
            scored.sort_by_key(|&(sc, i)| (sc, i));
            self.filtered = scored.into_iter().map(|(_, i)| i).collect();
        }
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

impl FilterablePicker for ProviderPickerState {
    fn query(&mut self) -> &mut String {
        &mut self.query
    }
    fn filtered(&self) -> &[usize] {
        &self.filtered
    }
    fn cursor(&mut self) -> &mut usize {
        &mut self.cursor
    }
    fn focus(&mut self) -> &mut PickerFocus {
        &mut self.focus
    }
    fn rebuild_filter(&mut self) {
        ProviderPickerState::rebuild_filter(self)
    }
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

    pub const LEVELS: &'static [&'static str] = &[
        "off", "minimal", "low", "medium", "high", "xhigh", "adaptive", "max",
    ];

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

/// Picker for toggling individual tools on/off for the current
/// session+mode. Mirrors the ThinkingPicker pattern (search + list)
/// but each row has a checkbox; Space toggles, Enter confirms.
#[derive(Debug)]
pub struct ToolPickerState {
    pub cursor: usize,
    pub query: String,
    pub scroll: usize,
    /// All tool names available to toggle (built once on open).
    pub tools: Vec<String>,
    /// Filtered indices into `tools`.
    pub filtered: Vec<usize>,
}

impl ToolPickerState {
    pub fn new(disabled: &std::collections::HashSet<String>) -> Self {
        let tools = crate::tools::all_tool_names();
        let filtered: Vec<usize> = (0..tools.len()).collect();
        let _ = disabled;
        let mut s = Self {
            cursor: 0,
            query: String::new(),
            scroll: 0,
            tools,
            filtered,
        };
        s.rebuild_filter();
        s
    }

    pub fn rebuild_filter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .tools
            .iter()
            .enumerate()
            .filter(|(_, name)| q.is_empty() || name.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        if self.cursor >= self.filtered.len() {
            self.cursor = self.filtered.len().saturating_sub(1);
        }
        if self.scroll > self.cursor {
            self.scroll = self.cursor;
        }
    }

    pub fn selected(&self) -> Option<&str> {
        self.filtered
            .get(self.cursor)
            .and_then(|&i| self.tools.get(i))
            .map(|s| s.as_str())
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

impl FilterablePicker for TimelinePickerState {
    fn query(&mut self) -> &mut String {
        &mut self.query
    }
    fn filtered(&self) -> &[usize] {
        &self.filtered
    }
    fn cursor(&mut self) -> &mut usize {
        &mut self.cursor
    }
    fn focus(&mut self) -> &mut PickerFocus {
        &mut self.focus
    }
    fn rebuild_filter(&mut self) {
        TimelinePickerState::rebuild_filter(self)
    }
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

/// A segment of content or a tool call, used to sort timeline entries
/// by their visual position in the session (matching `build_message_lines`).
struct SnapItem {
    offset: usize,
    tool_idx: usize,
}

fn snapshot_session(session: &Session) -> Vec<TimelineEntry> {
    let mut out = Vec::new();
    for (i, m) in session.messages.iter().enumerate() {
        // Hide empty assistant placeholders (the in-flight streaming
        // message before the first delta arrives).
        if matches!(m.role, Role::Assistant) && m.content.trim().is_empty() {
            continue;
        }

        // For user messages (no tools/thinking), just add the message.
        if m.role != Role::Assistant || m.tool_results.is_empty() {
            let preview = preview_first_line(&m.content);
            out.push(TimelineEntry {
                msg_idx: i,
                role: m.role,
                preview,
                ts: m.ts,
                tool_idx: None,
            });
            continue;
        }

        // For assistant messages with tools, interleave content segments
        // and tool entries by their content_offset — matching the visual
        // order in `build_message_lines`.
        let raw = &m.content;

        // Build sorted items (same logic as build_message_lines).
        let mut items: Vec<SnapItem> = Vec::new();
        for (ti, t) in m.tool_results.iter().enumerate() {
            if t.content.is_empty() && t.streaming_input.is_empty() {
                continue;
            }
            let offset = t.content_offset.min(raw.len());
            items.push(SnapItem {
                offset,
                tool_idx: ti,
            });
        }
        // Sort by offset; at the same offset, tools are sorted by index.
        // (Thinking segments are not shown as separate timeline entries.)
        items.sort_by(|a, b| a.offset.cmp(&b.offset));

        // Walk items, emitting a content segment entry before each tool
        // when there's text between the previous offset and this tool's
        // offset. Then emit the tool entry. This produces the same
        // interleave order as the session view.
        let mut cursor: usize = 0;
        for item in &items {
            if item.offset > cursor {
                // Content segment between cursor and offset.
                let seg = &raw[cursor..item.offset];
                let preview = preview_first_line(seg);
                if !preview.is_empty() && preview != "(no content)" {
                    out.push(TimelineEntry {
                        msg_idx: i,
                        role: m.role,
                        preview,
                        ts: m.ts,
                        tool_idx: None,
                    });
                }
                cursor = item.offset;
            }
            let tool_idx = item.tool_idx;
            let t = &m.tool_results[tool_idx];
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
                tool_idx: Some(tool_idx),
            });
        }

        // Remaining content after the last tool.
        if cursor < raw.len() {
            let seg = &raw[cursor..];
            let preview = preview_first_line(seg);
            if !preview.is_empty() && preview != "(no content)" {
                out.push(TimelineEntry {
                    msg_idx: i,
                    role: m.role,
                    preview,
                    ts: m.ts,
                    tool_idx: None,
                });
            }
        }

        // If nothing was emitted (all segments were empty), emit the
        // message itself so it still appears in the timeline.
        if !out.iter().any(|e| e.msg_idx == i) {
            let preview = preview_first_line(&m.content);
            out.push(TimelineEntry {
                msg_idx: i,
                role: m.role,
                preview,
                ts: m.ts,
                tool_idx: None,
            });
        }
    }
    out
}

fn preview_first_line(content: &str) -> String {
    let first_line = content.lines().next().unwrap_or("").trim();
    if first_line.chars().count() > 60 {
        let mut s: String = first_line.chars().take(60).collect();
        s.push('\u{2026}');
        s
    } else if first_line.is_empty() {
        "(no content)".to_string()
    } else {
        first_line.to_string()
    }
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
    /// Set to `true` when the user presses Enter to resume a session,
    /// so that `dispatch_to_active_tab` removes the tab instead of
    /// restoring it (the handler already closed the tab and resumed).
    pub consumed: bool,
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
            consumed: false,
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
        if q.is_empty() {
            self.filtered = (0..self.entries.len()).collect();
        } else {
            let mut scored: Vec<(u32, usize)> = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(i, e)| {
                    crate::fuzzy::score(&q, &e.title)
                        .or_else(|| crate::fuzzy::score(&q, &e.cwd))
                        .or_else(|| crate::fuzzy::score(&q, &e.id))
                        .map(|sc| (sc, i))
                })
                .collect();
            scored.sort_by_key(|&(sc, i)| (sc, i));
            self.filtered = scored.into_iter().map(|(_, i)| i).collect();
        }
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
            Some(SidebarTab::Settings(_)) => "settings",
            Some(SidebarTab::ModelPicker(_)) => "model picker",
            Some(SidebarTab::ProviderPicker(_)) => "provider",
            Some(SidebarTab::ThinkingPicker(_)) => "thinking",
            Some(SidebarTab::TimelinePicker(_)) => "timeline",
            Some(SidebarTab::SessionPicker(_)) => "sessions",
            Some(SidebarTab::SessionRename(_)) => "rename",
            Some(SidebarTab::Plan(_)) => "plan",
            Some(SidebarTab::Todo(_)) => "todo",
            Some(SidebarTab::ToolPicker(_)) => "tools",
            Some(SidebarTab::Hotkey) => "hotkey",
            Some(SidebarTab::CommandPalette(_)) => "command palette",
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
