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
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Openai => "openai",
            ProviderKind::Anthropic => "anthropic",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            ProviderKind::Openai => "OpenAI",
            ProviderKind::Anthropic => "Anthropic",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "openai" => Some(Self::Openai),
            "anthropic" => Some(Self::Anthropic),
            _ => None,
        }
    }

    pub fn all() -> [ProviderKind; 2] {
        [ProviderKind::Openai, ProviderKind::Anthropic]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ProviderMode {
    Key,
    Env,
}

impl ProviderMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderMode::Key => "key",
            ProviderMode::Env => "env",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "key" => Some(Self::Key),
            "env" => Some(Self::Env),
            _ => None,
        }
    }

    pub fn all() -> [ProviderMode; 2] {
        [ProviderMode::Key, ProviderMode::Env]
    }
}

/// String id of a provider entry, e.g. "openai:key" or "anthropic:env".
pub type ProviderId = String;

pub fn make_id(kind: ProviderKind, mode: ProviderMode) -> ProviderId {
    format!("{}:{}", kind.as_str(), mode.as_str())
}

pub fn parse_id(id: &str) -> Option<(ProviderKind, ProviderMode)> {
    let (k, m) = id.split_once(':')?;
    Some((ProviderKind::from_str_opt(k)?, ProviderMode::from_str_opt(m)?))
}

/// Human-readable label, e.g. "OpenAI (key)".
pub fn id_display(id: &str) -> String {
    match parse_id(id) {
        Some((k, m)) => format!("{} ({})", k.display_name(), m.as_str()),
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
    /// Optional user-defined name. When set, the status bar shows
    /// `name:model` instead of `kind:model`. Falls back to the kind name
    /// when empty.
    #[serde(default)]
    pub name: String,
}

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
    pub enter_behavior: EnterBehavior,
    #[serde(default)]
    pub entries: HashMap<ProviderId, ProviderConfig>,
}

impl Config {
    pub fn load_or_init(path: &Path) -> Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read config {}", path.display()))?;
            // Try the new format first.
            if let Ok(cfg) = serde_json::from_str::<Self>(&raw) {
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
                    base_url: p.base_url,
                    model: old.active_model.clone(),
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
            enter_behavior: EnterBehavior::EnterSends,
            entries,
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(path, raw).with_context(|| format!("write config {}", path.display()))?;
        Ok(())
    }

    pub fn entry(&self, id: &str) -> Option<&ProviderConfig> {
        self.entries.get(id)
    }

    pub fn entry_mut(&mut self, id: &str) -> Option<&mut ProviderConfig> {
        self.entries.get_mut(id)
    }

    pub fn active_entry(&self) -> Option<(&ProviderId, &ProviderConfig)> {
        let id = self.active.as_ref()?;
        let cfg = self.entries.get(id)?;
        Some((id, cfg))
    }

    pub fn active_kind(&self) -> Option<ProviderKind> {
        self.active.as_ref().and_then(|id| parse_id(id).map(|(k, _)| k))
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

    /// All `(kind, mode)` combinations, used by the "new provider" picker.
    pub fn all_possible_ids() -> Vec<ProviderId> {
        let mut out = Vec::new();
        for k in ProviderKind::all() {
            for m in ProviderMode::all() {
                out.push(make_id(k, m));
            }
        }
        out
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut entries = HashMap::new();
        entries.insert(
            make_id(ProviderKind::Openai, ProviderMode::Key),
            ProviderConfig {
                api_key: String::new(),
                api_key_env: "OPENAI_API_KEY".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                model: "gpt-4o-mini".to_string(),
                name: String::new(),
            },
        );
        entries.insert(
            make_id(ProviderKind::Anthropic, ProviderMode::Key),
            ProviderConfig {
                api_key: String::new(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                base_url: "https://api.anthropic.com".to_string(),
                model: "claude-3-5-sonnet-latest".to_string(),
                name: String::new(),
            },
        );
        Self {
            active: Some(make_id(ProviderKind::Openai, ProviderMode::Key)),
            thinking: ReasoningMode::Off,
            thinking_display: ThinkingDisplay::Show,
            enter_behavior: EnterBehavior::EnterSends,
            entries,
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
