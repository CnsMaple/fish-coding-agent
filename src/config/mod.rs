pub mod paths;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CacheRetention {
    /// Default 5-minute TTL.
    #[default]
    Default,
    /// 1-hour TTL for prompt caching.
    Long,
}

impl CacheRetention {
    /// Return the Anthropic `cache_control` block with optional TTL.
    pub fn to_cache_control(&self) -> serde_json::Value {
        match self {
            CacheRetention::Default => serde_json::json!({"type": "ephemeral"}),
            CacheRetention::Long => serde_json::json!({"type": "ephemeral", "ttl": "1h"}),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    OpenaiChat,
    OpenaiResponses,
    AnthropicMessages,
    Cursor,
    Volcengine,
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::OpenaiChat => "openai_chat",
            ProviderKind::OpenaiResponses => "openai_responses",
            ProviderKind::AnthropicMessages => "anthropic_messages",
            ProviderKind::Cursor => "cursor",
            ProviderKind::Volcengine => "volcengine",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            ProviderKind::OpenaiChat => "OpenAI-chat",
            ProviderKind::OpenaiResponses => "OpenAI-response",
            ProviderKind::AnthropicMessages => "Anthropic-messages",
            ProviderKind::Cursor => "Cursor",
            ProviderKind::Volcengine => "Volcengine",
        }
    }

    /// Label shown in the new-provider picker, e.g. "OpenAI (custom)".
    pub fn picker_label(&self) -> &'static str {
        match self {
            ProviderKind::OpenaiChat => "OpenAI chat/completions",
            ProviderKind::OpenaiResponses => "OpenAI responses",
            ProviderKind::AnthropicMessages => "Anthropic messages",
            ProviderKind::Cursor => "Cursor (oauth)",
            ProviderKind::Volcengine => "Volcengine (openai)",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "openai_chat" => Some(Self::OpenaiChat),
            "openai_responses" => Some(Self::OpenaiResponses),
            "anthropic_messages" => Some(Self::AnthropicMessages),
            "cursor" => Some(Self::Cursor),
            "volcengine" => Some(Self::Volcengine),
            // Legacy aliases — still parseable for config migration.
            "deepseek" | "minimax" => Some(Self::OpenaiChat),
            _ => None,
        }
    }

    pub fn all() -> [ProviderKind; 5] {
        [
            ProviderKind::OpenaiChat,
            ProviderKind::OpenaiResponses,
            ProviderKind::AnthropicMessages,
            ProviderKind::Cursor,
            ProviderKind::Volcengine,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProviderMode {
    #[default]
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

/// Stable id of a provider entry. Generated (uuid-like) per entry and
/// used as the reference for `active` and the model cache. No longer
/// encodes the kind or mode.
pub type ProviderId = String;

/// A configured provider endpoint. Unlike the old scheme (a text id
/// encoding `kind:mode` as a map key), each entry carries its own
/// random id, its protocol kind, and its auth mode as fields.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderEntry {
    /// Stable random id used to reference this entry (active, cache).
    #[serde(default)]
    pub id: ProviderId,
    /// Protocol kind (OpenaiChat / OpenaiResponses / AnthropicMessages /
    /// Cursor / Volcengine).
    #[serde(default)]
    pub kind: ProviderKind,
    /// Auth mode (Key / Env / Oauth).
    #[serde(default)]
    pub mode: ProviderMode,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub model_display: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub access_key: String,
    #[serde(default)]
    pub secret_key: String,
    #[serde(default)]
    pub provider_id: String,
}

impl ProviderEntry {
    pub fn new(kind: ProviderKind, mode: ProviderMode) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            kind,
            mode,
            api_key: String::new(),
            api_key_env: default_api_key_env(kind).to_string(),
            base_url: default_base_url(kind).to_string(),
            model: String::new(),
            model_display: String::new(),
            name: String::new(),
            access_key: String::new(),
            secret_key: String::new(),
            provider_id: String::new(),
        }
    }

    /// Construct an entry with an explicit id (used by tests and by
    /// legacy `make_id`-style references).
    pub fn with_id(id: ProviderId, kind: ProviderKind, mode: ProviderMode) -> Self {
        Self {
            id,
            kind,
            mode,
            api_key: String::new(),
            api_key_env: String::new(),
            base_url: default_base_url(kind).to_string(),
            model: String::new(),
            model_display: String::new(),
            name: String::new(),
            access_key: String::new(),
            secret_key: String::new(),
            provider_id: String::new(),
        }
    }

    /// Human-readable label: the user-defined name if set, else the
    /// protocol kind's display name.
    pub fn display(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.clone();
        }
        self.kind.display_name().to_string()
    }
}

/// Create a fresh random entry id.
pub fn new_entry_id() -> ProviderId {
    uuid::Uuid::new_v4().to_string()
}

/// Deterministic id for tests / legacy ids. Returns a stable string
/// derived from kind+mode (e.g. "openai_chat:key"). New entries should
/// use `new_entry_id()` instead; this helper exists so tests and any
/// legacy config references still resolve.
pub fn make_id(kind: ProviderKind, mode: ProviderMode) -> ProviderId {
    format!("{}:{}", kind.as_str(), mode.as_str())
}

/// Parse a legacy `kind:mode` id back into its parts. Only meaningful
/// for ids produced by `make_id`; random entry ids are not parseable.
pub fn parse_id(id: &str) -> Option<(ProviderKind, ProviderMode)> {
    let (k, m) = id.split_once(':')?;
    let m = m.split('-').next().unwrap_or(m);
    Some((
        ProviderKind::from_str_opt(k)?,
        ProviderMode::from_str_opt(m)?,
    ))
}

/// Human-readable label for an entry id, falling back to the id itself
/// when the entry cannot be resolved via its config.
pub fn id_display(id: &str) -> String {
    id.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningMode {
    #[default]
    Off,
    Minimal,
    Low,
    #[serde(alias = "med")]
    Medium,
    High,
    XHigh,
    Adaptive,
    Max,
}

impl ReasoningMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "off" => ReasoningMode::Off,
            "minimal" => ReasoningMode::Minimal,
            "low" => ReasoningMode::Low,
            "medium" | "med" => ReasoningMode::Medium,
            "high" => ReasoningMode::High,
            "xhigh" => ReasoningMode::XHigh,
            "adaptive" => ReasoningMode::Adaptive,
            "max" => ReasoningMode::Max,
            _ => ReasoningMode::default(),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ReasoningMode::Off => "off",
            ReasoningMode::Minimal => "minimal",
            ReasoningMode::Low => "low",
            ReasoningMode::Medium => "medium",
            ReasoningMode::High => "high",
            ReasoningMode::XHigh => "xhigh",
            ReasoningMode::Adaptive => "adaptive",
            ReasoningMode::Max => "max",
        }
    }

    /// For Anthropic and Anthropic-compatible endpoints: returns the
    /// `thinking.type` value (or `None` to omit the field entirely).
    /// `Off` and `Adaptive` do not need a budget; other modes do.
    pub fn anthropic_thinking_type(self) -> Option<&'static str> {
        match self {
            ReasoningMode::Off => None,
            ReasoningMode::Adaptive => Some("adaptive"),
            ReasoningMode::Minimal
            | ReasoningMode::Low
            | ReasoningMode::Medium
            | ReasoningMode::High
            | ReasoningMode::XHigh
            | ReasoningMode::Max => Some("enabled"),
        }
    }

    pub fn anthropic_budget(self) -> Option<u32> {
        match self {
            ReasoningMode::Off | ReasoningMode::Adaptive => None,
            ReasoningMode::Minimal => Some(512),
            ReasoningMode::Low => Some(1024),
            ReasoningMode::Medium => Some(4096),
            ReasoningMode::High => Some(16384),
            ReasoningMode::XHigh => Some(65536),
            ReasoningMode::Max => Some(131072),
        }
    }

    /// For OpenAI and OpenAI-compatible endpoints (DashScope, GLM,
    /// DeepSeek, etc.): returns the `reasoning_effort` value (or `None`
    /// to omit the field entirely).
    ///
    /// `Off` and `Adaptive` both omit the field (`None`) so the endpoint
    /// uses its own default — there is no standard `"none"` variant
    /// across OpenAI-compatible APIs.
    pub fn openai_effort(self) -> Option<&'static str> {
        match self {
            ReasoningMode::Adaptive => None,
            ReasoningMode::Off => None,
            ReasoningMode::Minimal => Some("minimal"),
            ReasoningMode::Low => Some("low"),
            ReasoningMode::Medium => Some("medium"),
            ReasoningMode::High => Some("high"),
            ReasoningMode::XHigh => Some("xhigh"),
            ReasoningMode::Max => Some("max"),
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

/// Backwards-compatible alias for the per-entry config type. New code
/// should use `ProviderEntry` directly; tests and legacy call sites use
/// `ProviderConfig`.
pub type ProviderConfig = ProviderEntry;

pub fn default_base_url(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenaiChat => "https://api.openai.com/v1",
        ProviderKind::OpenaiResponses => "https://api.deepseek.com",
        ProviderKind::AnthropicMessages => "https://api.anthropic.com",
        ProviderKind::Cursor => "https://api2.cursor.sh",
        ProviderKind::Volcengine => "https://ark.cn-beijing.volces.com/api/plan/v3",
    }
}

pub fn default_api_key_env(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenaiChat => "OPENAI_API_KEY",
        ProviderKind::OpenaiResponses => "DEEPSEEK_API_KEY",
        ProviderKind::AnthropicMessages => "ANTHROPIC_API_KEY",
        ProviderKind::Cursor => "",
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

/// Default for `Config::prefix_cache`.
pub fn default_prefix_cache() -> bool {
    true
}

/// Default for `Config::cache_retention`. Respects the
/// `PI_CACHE_RETENTION=long` environment variable.
pub fn default_cache_retention() -> CacheRetention {
    match std::env::var("PI_CACHE_RETENTION").ok().as_deref() {
        Some("long" | "1" | "true") => CacheRetention::Long,
        _ => CacheRetention::Default,
    }
}

/// Default for `Config::tool_result_snip_ratio`.
pub fn default_tool_result_snip_ratio() -> f64 {
    0.6
}

/// Default for `Config::compact_ratio`.
pub fn default_compact_ratio() -> f64 {
    0.8
}

/// Default for `Config::compact_force_ratio`.
pub fn default_compact_force_ratio() -> f64 {
    0.9
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

/// Per-file enabled/disabled state for discovered agents.md files.
/// Keyed by absolute path so the user can independently control
/// `~/.agents/agents.md` and `./agents.md`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentsConfig {
    #[serde(default)]
    pub entries: HashMap<String, bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Active entry id, e.g. "openai_chat:key". None means no entry is active.
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
    /// When true, the system prompt is split into a stable core +
    /// dynamic suffix. The stable core (instructions + tool defs) is
    /// kept at the very front of the conversation and never changes,
    /// maximising DeepSeek prefix-cache reuse. The dynamic suffix
    /// (date, CWD, shell) is appended as a user message at the end.
    /// Default: true.
    #[serde(default = "default_prefix_cache")]
    pub prefix_cache: bool,
    /// Cache retention policy for prompt caching. `Default` (5-min TTL)
    /// or `Long` (1-hour TTL). Default: Default.
    #[serde(default = "default_cache_retention")]
    pub cache_retention: CacheRetention,
    /// Ratio of context window at which stale tool results are
    /// shortened (cheap in-place operation, no LLM call). Default 0.6
    /// (60%). Mirrors DeepSeek-Reasonix `toolResultSnipRatio`.
    #[serde(default = "default_tool_result_snip_ratio")]
    pub tool_result_snip_ratio: f64,
    /// Ratio of context window at which full compaction
    /// (archive + summarise stale turns) is triggered. Default 0.8
    /// (80%). Mirrors DeepSeek-Reasonix `compactRatio`.
    #[serde(default = "default_compact_ratio")]
    pub compact_ratio: f64,
    /// Ratio of context window at which compaction is forced even
    /// when the foldable region is small. Default 0.9 (90%).
    #[serde(default = "default_compact_force_ratio")]
    pub compact_force_ratio: f64,
    #[serde(default)]
    pub entries: Vec<ProviderEntry>,
    /// MCP server configuration. Mirrors the top-level
    /// `Config.mcp` record in opencode
    /// (`packages/core/src/v1/config/config.ts`). Each entry is
    /// either a full server config or a `{ "enabled": false }`
    /// toggle used to disable a remote default.
    #[serde(default)]
    pub mcp: HashMap<String, crate::mcp::McpEntry>,
    /// Per-file enabled/disabled state for discovered agents.md files.
    #[serde(default)]
    pub agents: AgentsConfig,
}

impl Config {
    pub fn load_or_init(path: &Path) -> Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read config {}", path.display()))?;
            // Try the new format first (fast path: everything is valid).
            if let Ok(mut cfg) = serde_json::from_str::<Self>(&raw) {
                if cfg.sanitize_entries() {
                    let _ = cfg.save(path);
                }
                return Ok(cfg);
            }
            // Last resort: field-level tolerant parse. Instead of
            // bailing on a single bad value (which used to make the
            // whole file fall back to `Config::default()` and get
            // overwritten on the next `save_config`), parse the JSON
            // as a `Value` and deserialize each top-level field
            // independently. Bad fields fall back to their defaults;
            // everything else is preserved. The repaired config is
            // written back so the file self-heals.
            if let Ok(mut cfg) = Self::load_tolerant(&raw) {
                let _ = cfg.sanitize_entries();
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

    /// Field-level tolerant parse. Parses `raw` as a JSON `Value` and
    /// deserializes each top-level `Config` field independently; a
    /// field whose value is missing or invalid (e.g.
    /// `"thinking": "not-a-mode"`) falls back to its `Default` value
    /// instead of rejecting the whole document. `entries`, `mcp` and
    /// `agents` are collections: a single corrupt entry (e.g. one bad
    /// `ProviderEntry`) is dropped rather than blanking the entire
    /// collection. Returns `Err` if `raw` is not a valid JSON object — a
    /// syntactically broken file is still surfaced to the caller.
    fn load_tolerant(raw: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(raw).context("parse config as JSON")?;
        let obj = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("config root is not a JSON object"))?;
        Ok(Self {
            active: field::<Option<ProviderId>>(obj, "active").unwrap_or_default(),
            thinking: field(obj, "thinking").unwrap_or_default(),
            thinking_display: field(obj, "thinking_display").unwrap_or_default(),
            tool_display: field(obj, "tool_display").unwrap_or_default(),
            enter_behavior: field(obj, "enter_behavior").unwrap_or_default(),
            tool_preview_lines: field(obj, "tool_preview_lines").unwrap_or_default(),
            border_type: field(obj, "border_type").unwrap_or_default(),
            theme: field(obj, "theme").unwrap_or_default(),
            auto_compact: field(obj, "auto_compact").unwrap_or_default(),
            compact_reserved: field(obj, "compact_reserved").unwrap_or_default(),
            prefix_cache: field(obj, "prefix_cache").unwrap_or_default(),
            cache_retention: field(obj, "cache_retention").unwrap_or_default(),
            tool_result_snip_ratio: field(obj, "tool_result_snip_ratio").unwrap_or_default(),
            compact_ratio: field(obj, "compact_ratio").unwrap_or_default(),
            compact_force_ratio: field(obj, "compact_force_ratio").unwrap_or_default(),
            entries: tolerant_vec::<ProviderEntry>(obj, "entries"),
            mcp: tolerant_map::<crate::mcp::McpEntry>(obj, "mcp"),
            agents: field(obj, "agents").unwrap_or_default(),
        })
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
        let mut changed = false;
        // Ensure every entry has a stable id.
        for e in self.entries.iter_mut() {
            if e.id.trim().is_empty() {
                e.id = new_entry_id();
                changed = true;
            }
            if e.kind == ProviderKind::Cursor && e.model.trim().eq_ignore_ascii_case("auto") {
                e.model.clear();
                changed = true;
            }
        }
        let active_is_valid = self
            .active
            .as_ref()
            .map(|id| self.entries.iter().any(|e| &e.id == id))
            .unwrap_or(false);
        if !active_is_valid {
            self.active = self.entries.first().map(|e| e.id.clone());
            changed = true;
        }
        changed
    }

    pub fn configured_provider_ids(&self) -> Vec<ProviderId> {
        self.entries.iter().map(|e| e.id.clone()).collect()
    }

    pub fn entry(&self, id: &str) -> Option<&ProviderEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn entry_mut(&mut self, id: &str) -> Option<&mut ProviderEntry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    /// Index of an entry by id.
    pub fn entry_index(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    pub fn active_entry(&self) -> Option<(&ProviderId, &ProviderEntry)> {
        let id = self.active.as_ref()?;
        let entry = self.entries.iter().find(|e| &e.id == id)?;
        Some((id, entry))
    }

    pub fn active_kind(&self) -> Option<ProviderKind> {
        self.active_entry().map(|(_, e)| e.kind)
    }

    pub fn active_model(&self) -> &str {
        match self.active_entry() {
            Some((_, c)) => &c.model,
            None => "-",
        }
    }

    /// Display name for the active provider. Returns the user-defined
    /// `name` field if set, otherwise the kind name's display string.
    /// Returns an empty string when there is no active entry, so the
    /// status bar can show just the model (or `(no model)`) without a
    /// dangling `name:` prefix.
    pub fn active_name(&self) -> String {
        match self.active_entry() {
            Some((_, e)) => e.display(),
            None => String::new(),
        }
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
            return Err(format!("{id}: base_url is required (set it in settings)"));
        }
        if p.mode == ProviderMode::Oauth {
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
            .iter()
            .filter_map(|e| self.validate_provider(&e.id).err())
            .collect()
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
            prefix_cache: true,
            cache_retention: CacheRetention::Default,
            tool_result_snip_ratio: default_tool_result_snip_ratio(),
            compact_ratio: default_compact_ratio(),
            compact_force_ratio: default_compact_force_ratio(),
            entries: Vec::new(),
            mcp: HashMap::new(),
            agents: AgentsConfig::default(),
        }
    }
}

/// Deserialize a single top-level field from a parsed JSON object.
/// Returns `Ok(value)` on success, `Err` if the field is missing or
/// its value is invalid — callers turn `Err` into the field's
/// `Default` via `.unwrap_or_default()`. Missing fields naturally
/// fail and fall back to default, which is the desired behaviour for
/// a tolerant load.
fn field<T: DeserializeOwned + Default>(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<T> {
    let v = obj
        .get(key)
        .ok_or_else(|| anyhow::anyhow!("missing field {key}"))?;
    serde_json::from_value(v.clone()).with_context(|| format!("decode field {key}"))
}

/// Deserialize a `HashMap<String, V>` field entry-by-entry. A single
/// corrupt entry (e.g. one `ProviderConfig` with a bad value) is
/// dropped rather than blanking the whole map. This preserves as
/// much of the user's data as possible while still discarding the
/// parts that cannot be decoded.
fn tolerant_map<V: DeserializeOwned>(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> HashMap<String, V> {
    let Some(serde_json::Value::Object(map)) = obj.get(key) else {
        return HashMap::new();
    };
    let mut out = HashMap::with_capacity(map.len());
    for (k, v) in map {
        match serde_json::from_value::<V>(v.clone()) {
            Ok(parsed) => {
                out.insert(k.clone(), parsed);
            }
            Err(_) => {
                // Skip the single bad entry; the rest of the map is kept.
            }
        }
    }
    out
}

/// Deserialize a `Vec<V>` field entry-by-entry. A single corrupt
/// element is dropped rather than blanking the whole list.
fn tolerant_vec<V: DeserializeOwned>(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Vec<V> {
    let Some(serde_json::Value::Array(arr)) = obj.get(key) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        match serde_json::from_value::<V>(v.clone()) {
            Ok(parsed) => out.push(parsed),
            Err(_) => {
                // Skip the single bad entry; the rest of the list is kept.
            }
        }
    }
    out
}

pub fn config_file_path() -> Result<PathBuf> {
    paths::config_file_path()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_config_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("fish-coding-agent-config-{name}.json"));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// Write `json` to a temp path and load it via `load_or_init`.
    fn load_from(name: &str, json: &str) -> Config {
        let path = temp_config_path(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(json.as_bytes()).unwrap();
        drop(f);
        Config::load_or_init(&path).expect("load must succeed")
    }

    #[test]
    fn bad_scalar_field_falls_back_to_default() {
        // `thinking` is an invalid enum value, but everything else is
        // valid. The old behaviour bailed and the whole config reset
        // to default; now `thinking` falls back to `Off` while
        // `auto_compact` and `tool_preview_lines` survive.
        let json = r#"{
            "thinking": "not-a-mode",
            "auto_compact": false,
            "tool_preview_lines": 42
        }"#;
        let cfg = load_from("bad_scalar", json);
        assert_eq!(cfg.thinking, ReasoningMode::Off, "bad enum -> default");
        assert!(!cfg.auto_compact, "valid field must survive");
        assert_eq!(cfg.tool_preview_lines, 42, "valid field must survive");
    }

    #[test]
    fn bad_nested_entry_is_dropped_rest_kept() {
        // Two provider entries: one valid, one with a structurally
        // invalid value (api_key is a number, not a string). The bad
        // entry is dropped; the good entry survives.
        let json = r#"{
            "entries": [
                {
                    "id": "entry-1",
                    "kind": "openai_chat",
                    "mode": "key",
                    "api_key": "sk-good",
                    "base_url": "https://api.openai.com/v1",
                    "model": "gpt-4o"
                },
                {
                    "id": "entry-2",
                    "kind": "openai_responses",
                    "api_key": 12345,
                    "base_url": "https://api.deepseek.com",
                    "model": "deepseek-v4-flash"
                }
            ]
        }"#;
        let cfg = load_from("bad_entry", json);
        assert_eq!(cfg.entries.len(), 1, "good entry survives");
        assert_eq!(cfg.entries[0].id, "entry-1");
    }

    #[test]
    fn syntactically_broken_json_still_bails() {
        // A file that is not valid JSON at all cannot be salvaged; the
        // tolerant path requires a parseable JSON object. We keep the
        // bail so the caller (main.rs) falls back to default and the
        // user sees a warning instead of silently losing data.
        let path = temp_config_path("broken_syntax");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"{ this is not json ").unwrap();
        drop(f);
        let res = Config::load_or_init(&path);
        assert!(res.is_err(), "non-JSON file must still bail");
    }

    #[test]
    fn valid_config_loads_unchanged() {
        // Regression: a fully valid config must continue to load
        // through the fast path with all fields intact.
        let json = r#"{
            "thinking": "high",
            "auto_compact": true,
            "tool_preview_lines": 7,
            "entries": [
                {
                    "id": "entry-1",
                    "kind": "openai_chat",
                    "mode": "key",
                    "api_key": "sk-x",
                    "base_url": "https://api.openai.com/v1",
                    "model": "gpt-4o"
                }
            ]
        }"#;
        let cfg = load_from("valid", json);
        assert_eq!(cfg.thinking, ReasoningMode::High);
        assert!(cfg.auto_compact);
        assert_eq!(cfg.tool_preview_lines, 7);
        assert_eq!(cfg.entries.len(), 1);
        assert_eq!(cfg.entries[0].model, "gpt-4o");
    }

    #[test]
    fn repaired_config_is_written_back() {
        // After a tolerant load the file on disk should be healed:
        // reloading it must go through the fast path (no tolerant
        // fallback) and the bad field must now hold its default value.
        let path = temp_config_path("repaired");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"{ \"thinking\": \"garbage\", \"auto_compact\": false }")
            .unwrap();
        drop(f);
        let cfg = Config::load_or_init(&path).expect("first load heals");
        assert_eq!(cfg.thinking, ReasoningMode::Off);
        assert!(!cfg.auto_compact);

        // Reload: the file should now be valid JSON with the default
        // `thinking` value.
        let cfg2 = Config::load_or_init(&path).expect("reload healed file");
        assert_eq!(cfg2.thinking, ReasoningMode::Off);
        assert!(!cfg2.auto_compact, "non-bad field still preserved");
    }
}
