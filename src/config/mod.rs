pub mod paths;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Openai,
    Anthropic,
    Cursor,
    DeepSeek,
    MiniMax,
    Volcengine,
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Openai => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Cursor => "cursor",
            ProviderKind::DeepSeek => "deepseek",
            ProviderKind::MiniMax => "minimax",
            ProviderKind::Volcengine => "volcengine",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            ProviderKind::Openai => "OpenAI",
            ProviderKind::Anthropic => "Anthropic",
            ProviderKind::Cursor => "Cursor",
            ProviderKind::DeepSeek => "DeepSeek",
            ProviderKind::MiniMax => "MiniMax",
            ProviderKind::Volcengine => "Volcengine",
        }
    }

    /// Label shown in the new-provider picker, e.g. "OpenAI (custom)".
    pub fn picker_label(&self) -> &'static str {
        match self {
            ProviderKind::Openai => "OpenAI (custom)",
            ProviderKind::Anthropic => "Anthropic (custom)",
            ProviderKind::Cursor => "Cursor (oauth)",
            ProviderKind::DeepSeek => "DeepSeek (openai)",
            ProviderKind::MiniMax => "MiniMax (openai)",
            ProviderKind::Volcengine => "Volcengine (openai)",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "openai" => Some(Self::Openai),
            "anthropic" => Some(Self::Anthropic),
            "cursor" => Some(Self::Cursor),
            "deepseek" => Some(Self::DeepSeek),
            "minimax" => Some(Self::MiniMax),
            "volcengine" => Some(Self::Volcengine),
            _ => None,
        }
    }

    pub fn all() -> [ProviderKind; 6] {
        [
            ProviderKind::Openai,
            ProviderKind::Anthropic,
            ProviderKind::Cursor,
            ProviderKind::DeepSeek,
            ProviderKind::MiniMax,
            ProviderKind::Volcengine,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ProviderMode {
    Key,
    Env,
    Oauth,
}

impl ProviderMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderMode::Key => "key",
            ProviderMode::Env => "env",
            ProviderMode::Oauth => "oauth",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "key" => Some(Self::Key),
            "env" => Some(Self::Env),
            "oauth" | "auth" => Some(Self::Oauth),
            _ => None,
        }
    }

    pub fn all() -> [ProviderMode; 3] {
        [ProviderMode::Key, ProviderMode::Env, ProviderMode::Oauth]
    }

    pub fn for_kind(kind: ProviderKind) -> &'static [ProviderMode] {
        match kind {
            ProviderKind::Cursor => &[ProviderMode::Oauth],
            _ => &[ProviderMode::Key],
        }
    }
}

/// String id of a provider entry, e.g. "openai:key" or "anthropic:env".
pub type ProviderId = String;

pub fn make_id(kind: ProviderKind, mode: ProviderMode) -> ProviderId {
    format!("{}:{}", kind.as_str(), mode.as_str())
}

pub fn parse_id(id: &str) -> Option<(ProviderKind, ProviderMode)> {
    let (k, m) = id.split_once(':')?;
    // Strip dedup suffix like "-2" from duplicated provider IDs (e.g. openai:key-2).
    let m = m.split('-').next().unwrap_or(m);
    Some((
        ProviderKind::from_str_opt(k)?,
        ProviderMode::from_str_opt(m)?,
    ))
}

/// Human-readable label, e.g. "OpenAI".
pub fn id_display(id: &str) -> String {
    match parse_id(id) {
        Some((k, _)) => k.display_name().to_string(),
        None => id.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningMode {
    #[default]
    Off,
    Low,
    Med,
    High,
    Adaptive,
}

impl ReasoningMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReasoningMode::Off => "off",
            ReasoningMode::Low => "low",
            ReasoningMode::Med => "med",
            ReasoningMode::High => "high",
            ReasoningMode::Adaptive => "adaptive",
        }
    }

    /// For Anthropic and Anthropic-compatible endpoints: returns the
    /// `thinking.type` value (or `None` to omit the field entirely).
    /// `Off` and `Adaptive` do not need a budget; other modes do.
    pub fn anthropic_thinking_type(self) -> Option<&'static str> {
        match self {
            ReasoningMode::Off => None,
            ReasoningMode::Adaptive => Some("adaptive"),
            ReasoningMode::Low | ReasoningMode::Med | ReasoningMode::High => Some("enabled"),
        }
    }

    pub fn anthropic_budget(self) -> Option<u32> {
        match self {
            ReasoningMode::Off | ReasoningMode::Adaptive => None,
            ReasoningMode::Low => Some(1024),
            ReasoningMode::Med => Some(4096),
            ReasoningMode::High => Some(16384),
        }
    }

    pub fn openai_effort(self) -> Option<&'static str> {
        match self {
            ReasoningMode::Off | ReasoningMode::Adaptive => None,
            ReasoningMode::Low => Some("low"),
            ReasoningMode::Med => Some("medium"),
            ReasoningMode::High => Some("high"),
        }
    }
}

/// How thinking content (Anthropic "thinking_delta") is shown in the
/// session view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingDisplay {
    /// Always show thinking blocks, user can click to fold.
    #[default]
    Show,
    /// Never render thinking content.
    Hide,
    /// Auto-expand while the stream is in flight, auto-fold on finish.
    ShowWhileStreaming,
}

impl ThinkingDisplay {
    pub fn as_str(&self) -> &'static str {
        match self {
            ThinkingDisplay::Show => "show",
            ThinkingDisplay::Hide => "hide",
            ThinkingDisplay::ShowWhileStreaming => "while streaming",
        }
    }
}

/// How tool results are shown in the session view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolResultDisplay {
    /// Always show tool result blocks, user can click to fold.
    #[default]
    Show,
    /// Never render tool results.
    Hide,
    /// Auto-expand while streaming, auto-fold on finish.
    ShowWhileStreaming,
}

impl ToolResultDisplay {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolResultDisplay::Show => "show",
            ToolResultDisplay::Hide => "hide",
            ToolResultDisplay::ShowWhileStreaming => "while streaming",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EnterBehavior {
    #[default]
    EnterSends,
    EnterNewline,
}

impl EnterBehavior {
    pub fn as_str(&self) -> &'static str {
        match self {
            // Left half = plain Enter, right half = Shift+Enter. Keeping
            // "Enter" and "Shift+Enter" on the same line and consistent in
            // position avoids the "are these descriptions of the same key
            // or different keys?" confusion.
            EnterBehavior::EnterSends => "Enter sends | Shift+Enter newline",
            EnterBehavior::EnterNewline => "Enter newline | Shift+Enter sends",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    /// Optional friendly model label. `model` remains the provider request id.
    #[serde(default)]
    pub model_display: String,
    /// Optional user-defined name. When set, the status bar shows
    /// `name:model` instead of `kind:model`. Falls back to the kind name
    /// when empty.
    #[serde(default)]
    pub name: String,
    /// Volcengine Access Key for model list API (HMAC-SHA256 auth).
    #[serde(default)]
    pub access_key: String,
    /// Volcengine Secret Key for model list API (HMAC-SHA256 auth).
    #[serde(default)]
    pub secret_key: String,
}

impl ProviderConfig {
    pub fn preset(kind: ProviderKind) -> Self {
        Self {
            api_key: String::new(),
            api_key_env: default_api_key_env(kind).to_string(),
            base_url: default_base_url(kind).to_string(),
            model: String::new(),
            model_display: String::new(),
            name: String::new(),
            access_key: String::new(),
            secret_key: String::new(),
        }
    }
}

pub fn default_base_url(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Openai => "https://api.openai.com/v1",
        ProviderKind::Anthropic => "https://api.anthropic.com",
        ProviderKind::Cursor => "https://api2.cursor.sh",
        ProviderKind::DeepSeek => "https://api.deepseek.com",
        ProviderKind::MiniMax => "https://api.minimaxi.com",
        ProviderKind::Volcengine => "https://ark.cn-beijing.volces.com/api/plan/v3",
    }
}

pub fn default_api_key_env(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Openai => "OPENAI_API_KEY",
        ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
        ProviderKind::Cursor => "",
        ProviderKind::DeepSeek => "DEEPSEEK_API_KEY",
        ProviderKind::MiniMax => "MINIMAX_API_KEY",
        ProviderKind::Volcengine => "VOLCENGINE_API_KEY",
    }
}

pub fn default_model(_kind: ProviderKind) -> &'static str {
    ""
}

/// Default for `Config::auto_compact`. Kept as a `fn` (not a
/// `const`) so it can be referenced in `#[serde(default = ...)]`
/// attributes.
pub fn default_auto_compact() -> bool {
    true
}

/// Default number of output lines visible inside a collapsed tool
/// block before the Ctrl+O hint is offered. Adjustable via
/// `/settings → tool preview lines`.
pub fn default_tool_preview_lines() -> usize {
    10
}

/// Lower / upper bounds for `Config::tool_preview_lines`. The
/// settings UI clamps the user's selection to this range so the
/// preview stays useful (no 0-line boxes, no overflowing the box).
pub const TOOL_PREVIEW_LINES_MIN: usize = 3;
pub const TOOL_PREVIEW_LINES_MAX: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Active entry id, e.g. "openai:key". None means no entry is active.
    #[serde(default)]
    pub active: Option<ProviderId>,
    #[serde(default)]
    pub thinking: ReasoningMode,
    #[serde(default)]
    pub thinking_display: ThinkingDisplay,
    #[serde(default)]
    pub tool_display: ToolResultDisplay,
    #[serde(default)]
    pub enter_behavior: EnterBehavior,
    /// Number of output lines shown in a collapsed tool block before
    /// the Ctrl+O hint is offered. Clamped to
    /// `[TOOL_PREVIEW_LINES_MIN, TOOL_PREVIEW_LINES_MAX]` by the
    /// settings UI.
    #[serde(default = "default_tool_preview_lines")]
    pub tool_preview_lines: usize,
    /// Border style for markdown tables and code blocks.
    #[serde(default)]
    pub border_type: crate::ui::border_type::BorderType,
    /// Color theme for the TUI.
    #[serde(default)]
    pub theme: crate::theme::ThemeVariant,
    /// When true, the session is auto-compacted (older turns are
    /// summarized) once the cumulative token usage reaches
    /// `ctx_window - reserved`. Toggleable from `/settings`.
    /// Default: `true`. Mirrors opencode's
    /// `Config::compaction.auto` knob.
    #[serde(default = "default_auto_compact")]
    pub auto_compact: bool,
    /// Optional override for the reserved token buffer used by
    /// auto-compaction. `None` means "use the default 20 000
    /// token buffer, clamped to the model's max output". Not
    /// exposed in the settings UI for now; reserved for future
    /// advanced settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_reserved: Option<u64>,
    #[serde(default)]
    pub entries: HashMap<ProviderId, ProviderConfig>,
    /// MCP server configuration. Mirrors the top-level
    /// `Config.mcp` record in opencode
    /// (`packages/core/src/v1/config/config.ts`). Each entry is
    /// either a full server config or a `{ "enabled": false }`
    /// toggle used to disable a remote default.
    #[serde(default)]
    pub mcp: HashMap<String, crate::mcp::McpEntry>,
}

impl Config {
    pub fn load_or_init(path: &Path) -> Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read config {}", path.display()))?;
            // Try the new format first.
            if let Ok(mut cfg) = serde_json::from_str::<Self>(&raw) {
                if cfg.sanitize_entries() {
                    let _ = cfg.save(path);
                }
                return Ok(cfg);
            }
            // Migrate from the old (kind-only) format if possible.
            if let Ok(old) = serde_json::from_str::<OldConfig>(&raw) {
                let cfg = Self::migrate_from(old);
                let _ = cfg.save(path);
                return Ok(cfg);
            }
            anyhow::bail!("config parse failed and no migration available");
        } else {
            let cfg = Self::default();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let pretty = serde_json::to_string_pretty(&cfg)?;
            std::fs::write(path, pretty).ok();
            Ok(cfg)
        }
    }

    fn migrate_from(old: OldConfig) -> Self {
        let mut entries = HashMap::new();
        for (kind, p) in old.providers {
            entries.insert(
                make_id(kind, ProviderMode::Key),
                ProviderConfig {
                    api_key: p.api_key,
                    api_key_env: p.api_key_env,
                    access_key: String::new(),
                    secret_key: String::new(),
                    base_url: p.base_url,
                    model: old.active_model.clone(),
                    model_display: String::new(),
                    name: String::new(),
                },
            );
        }
        let active = entries
            .keys()
            .next()
            .cloned()
            .or_else(|| Some(make_id(old.active_provider, ProviderMode::Key)));
        Self {
            active,
            thinking: old.thinking,
            thinking_display: ThinkingDisplay::Show,
            tool_display: ToolResultDisplay::Show,
            enter_behavior: EnterBehavior::EnterSends,
            tool_preview_lines: default_tool_preview_lines(),
            border_type: crate::ui::border_type::BorderType::default(),
            theme: crate::theme::ThemeVariant::default(),
            auto_compact: default_auto_compact(),
            compact_reserved: None,
            entries,
            mcp: HashMap::new(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut cfg = self.clone();
        cfg.sanitize_entries();
        let raw = serde_json::to_string_pretty(&cfg)?;
        std::fs::write(path, raw).with_context(|| format!("write config {}", path.display()))?;
        Ok(())
    }

    pub fn sanitize_entries(&mut self) -> bool {
        let before = self.entries.len();
        self.entries.retain(|id, _| parse_id(id).is_some());
        let mut changed = self.entries.len() != before;
        for (id, cfg) in self.entries.iter_mut() {
            if parse_id(id)
                .map(|(k, _)| k == ProviderKind::Cursor)
                .unwrap_or(false)
                && cfg.model.trim().eq_ignore_ascii_case("auto")
            {
                cfg.model.clear();
                changed = true;
            }
        }
        let active_is_valid = self
            .active
            .as_ref()
            .map(|id| self.entries.contains_key(id) && parse_id(id).is_some())
            .unwrap_or(false);
        if !active_is_valid {
            self.active = self.configured_provider_ids().into_iter().next();
            changed = true;
        }
        changed
    }

    pub fn configured_provider_ids(&self) -> Vec<ProviderId> {
        let mut ids: Vec<_> = self
            .entries
            .keys()
            .filter(|id| parse_id(id).is_some())
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    pub fn entry(&self, id: &str) -> Option<&ProviderConfig> {
        self.entries.get(id)
    }

    pub fn entry_mut(&mut self, id: &str) -> Option<&mut ProviderConfig> {
        self.entries.get_mut(id)
    }

    pub fn active_entry(&self) -> Option<(&ProviderId, &ProviderConfig)> {
        let id = self.active.as_ref()?;
        parse_id(id)?;
        let cfg = self.entries.get(id)?;
        Some((id, cfg))
    }

    pub fn active_kind(&self) -> Option<ProviderKind> {
        self.active
            .as_ref()
            .and_then(|id| parse_id(id).map(|(k, _)| k))
    }

    pub fn active_model(&self) -> &str {
        match self.active_entry() {
            Some((_, c)) => &c.model,
            None => "-",
        }
    }

    /// Display name for the active provider. Returns the user-defined
    /// `name` field if set, otherwise the kind name (`openai` / `anthropic`).
    /// Returns an empty string when there is no active entry, so the
    /// status bar can show just the model (or `(no model)`) without a
    /// dangling `name:` prefix.
    pub fn active_name(&self) -> String {
        if let Some((_, c)) = self.active_entry() {
            if !c.name.trim().is_empty() {
                return c.name.clone();
            }
        }
        self.active_kind()
            .map(|k| k.as_str().to_string())
            .unwrap_or_default()
    }

    /// Display string for the active model. Empty / unset models are shown
    /// as `(no model)` so the status bar is unambiguous.
    pub fn active_model_display(&self) -> String {
        if let Some((_, c)) = self.active_entry() {
            if !c.model_display.trim().is_empty() {
                return c.model_display.clone();
            }
            if !c.model.trim().is_empty() {
                return c.model.clone();
            }
        }
        "(no model)".to_string()
    }

    pub fn effective_api_key(&self, id: &str) -> Option<String> {
        let p = self.entry(id)?;
        if !p.api_key.is_empty() {
            return Some(p.api_key.clone());
        }
        if !p.api_key_env.is_empty() {
            return std::env::var(&p.api_key_env).ok();
        }
        None
    }

    pub fn validate_provider(&self, id: &str) -> Result<(), String> {
        let p = self
            .entry(id)
            .ok_or_else(|| format!("{id}: not configured"))?;
        if p.base_url.trim().is_empty() {
            return Err(format!("{id}: base_url is required (set it in /settings)"));
        }
        if parse_id(id)
            .map(|(_, m)| m == ProviderMode::Oauth)
            .unwrap_or(false)
        {
            return match self.effective_api_key(id) {
                Some(k) if !k.is_empty() => Ok(()),
                _ => Err(format!("{id}: Cursor OAuth is not authorized yet")),
            };
        }
        match self.effective_api_key(id) {
            Some(k) if !k.is_empty() => Ok(()),
            _ => {
                let env_hint = if p.api_key_env.is_empty() {
                    "<unset>".to_string()
                } else {
                    p.api_key_env.clone()
                };
                Err(format!(
                    "{id}: api_key is empty and env {env_hint} is not set"
                ))
            }
        }
    }

    pub fn validate_all(&self) -> Vec<String> {
        self.entries
            .keys()
            .filter_map(|id| self.validate_provider(id).err())
            .collect()
    }

    /// All possible provider kinds, used by the "new provider" picker.
    pub fn all_possible_ids() -> Vec<ProviderId> {
        let mut out = Vec::new();
        for k in ProviderKind::all() {
            let mode = match k {
                ProviderKind::Cursor => ProviderMode::Oauth,
                _ => ProviderMode::Key,
            };
            out.push(make_id(k, mode));
        }
        out
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            active: None,
            thinking: ReasoningMode::Off,
            thinking_display: ThinkingDisplay::Show,
            tool_display: ToolResultDisplay::Show,
            enter_behavior: EnterBehavior::EnterSends,
            tool_preview_lines: default_tool_preview_lines(),
            border_type: crate::ui::border_type::BorderType::default(),
            theme: crate::theme::ThemeVariant::default(),
            auto_compact: default_auto_compact(),
            compact_reserved: None,
            entries: HashMap::new(),
            mcp: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OldConfig {
    pub active_provider: ProviderKind,
    pub active_model: String,
    pub thinking: ReasoningMode,
    pub providers: HashMap<ProviderKind, OldProviderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OldProviderConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub base_url: String,
}

pub fn config_file_path() -> Result<PathBuf> {
    paths::config_file_path()
}
