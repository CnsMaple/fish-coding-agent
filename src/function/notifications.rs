use chrono::Utc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Ok,
    Info,
    Warn,
    Fail,
}

impl ToastLevel {
    pub fn tag(&self) -> &'static str {
        match self {
            ToastLevel::Ok => "ok",
            ToastLevel::Info => "info",
            ToastLevel::Warn => "warn",
            ToastLevel::Fail => "fail",
        }
    }

    /// Does this level count toward the "pending events" counter?
    pub fn is_important(&self) -> bool {
        matches!(self, ToastLevel::Warn | ToastLevel::Fail)
    }
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub level: ToastLevel,
    pub text: String,
    pub ts: chrono::DateTime<chrono::Local>,
}

impl Toast {
    pub fn format_time(&self) -> String {
        self.ts.format("%H:%M:%S").to_string()
    }
}

#[derive(Debug, Default)]
pub struct Notifications {
    pub items: VecDeque<Toast>,
    pub query: String,
    pub cursor: usize,
    pub scroll: usize,
    pub searching: bool,
}

use std::collections::VecDeque;

impl Notifications {
    pub fn push(&mut self, level: ToastLevel, text: impl Into<String>) {
        let text = text.into();
        // Coalesce consecutive duplicates: if the most recent toast has the
        // same level and text, refresh its timestamp and skip the push. This
        // keeps the list from filling with the same error (e.g. a chat
        // repeatedly failing with "no active provider" while the user is
        // typing before fixing their config).
        if let Some(last) = self.items.back() {
            if last.level == level && last.text == text {
                let last = self.items.back_mut().expect("checked above");
                last.ts = chrono::Local::now();
                return;
            }
        }
        self.items.push_back(Toast {
            level,
            text,
            ts: chrono::Local::now(),
        });
        if self.items.len() > 200 {
            let drop = self.items.len() - 200;
            self.items.drain(0..drop);
        }
        self.clamp_cursor();
    }

    /// Drop all toasts. The user requested a transient model: toasts arrive,
    /// the user reads them, then the next panel open starts fresh.
    pub fn clear(&mut self) {
        self.items.clear();
        self.query.clear();
        self.cursor = 0;
        self.scroll = 0;
        self.searching = false;
    }

    pub fn latest_n(&self, n: usize) -> Vec<&Toast> {
        let start = self.items.len().saturating_sub(n);
        self.items.iter().skip(start).collect()
    }

    pub fn filtered_indices(&self) -> Vec<usize> {
        let query = self.query.trim().to_ascii_lowercase();
        self.items
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(idx, toast)| {
                if query.is_empty()
                    || toast.text.to_ascii_lowercase().contains(&query)
                    || toast.level.tag().contains(&query)
                    || toast.format_time().contains(&query)
                {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn clamp_cursor(&mut self) {
        let len = self.filtered_indices().len();
        if len == 0 {
            self.cursor = 0;
            self.scroll = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    pub fn move_up(&mut self) {
        self.clamp_cursor();
        self.cursor = self.cursor.saturating_sub(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
    }

    pub fn move_down(&mut self) {
        let len = self.filtered_indices().len();
        if len == 0 {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        self.cursor = (self.cursor + 1).min(len - 1);
    }

    pub fn insert_query_char(&mut self, c: char) {
        self.query.push(c);
        self.cursor = 0;
        self.scroll = 0;
        self.clamp_cursor();
    }

    pub fn backspace_query(&mut self) -> bool {
        if self.query.pop().is_some() {
            self.cursor = 0;
            self.scroll = 0;
            self.clamp_cursor();
            true
        } else {
            false
        }
    }

    pub fn enter_search_mode(&mut self) {
        self.searching = true;
    }

    pub fn exit_search_mode(&mut self) {
        self.searching = false;
    }
}

/// Rolling-average cache hit rate tracker.
#[derive(Debug)]
pub struct HitRate {
    window: Vec<f64>,
    cap: usize,
}

impl HitRate {
    pub fn new(cap: usize) -> Self {
        Self {
            window: Vec::with_capacity(cap),
            cap,
        }
    }

    pub fn record(&mut self, rate: f64) {
        if self.window.len() == self.cap {
            self.window.remove(0);
        }
        self.window.push(rate);
    }

    pub fn current(&self) -> Option<f64> {
        self.window.last().copied()
    }

    pub fn average(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let sum: f64 = self.window.iter().sum();
        Some(sum / self.window.len() as f64)
    }
}

/// Token rate tracker with a sliding window.
/// Stores the rate (tokens/second) of each completed response.
/// Exposes both the latest rate and the average across recent responses.
#[derive(Debug)]
pub struct TokenRate {
    window: VecDeque<f64>,
    cap: usize,
    current: Option<f64>,
}

impl Default for TokenRate {
    fn default() -> Self {
        Self {
            window: VecDeque::new(),
            cap: 50,
            current: None,
        }
    }
}

impl TokenRate {
    pub fn new(cap: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(cap),
            cap,
            current: None,
        }
    }

    pub fn record(&mut self, val: f64) {
        self.current = Some(val);
        if self.window.len() == self.cap {
            self.window.pop_front();
        }
        self.window.push_back(val);
    }

    pub fn current(&self) -> Option<f64> {
        self.current
    }

    pub fn average(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let sum: f64 = self.window.iter().sum();
        Some(sum / self.window.len() as f64)
    }
}

/// Cached model list per provider.
use crate::config::ProviderKind;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Stable display/selection id shown in the picker.
    pub id: String,
    pub display: String,
    /// Provider-specific id to send in chat requests. Defaults to `id` for older caches.
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub context_window_tokens: Option<u64>,
    /// When true, the user needs to manually pick a context window size.
    #[serde(default)]
    pub context_needs_pick: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedModels {
    pub models: Vec<ModelInfo>,
    pub fetched_at: chrono::DateTime<Utc>,
    pub base_url: String,
    pub api_key: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ModelCache {
    pub by_provider: HashMap<ProviderKind, CachedModels>,
}

impl ModelCache {
    pub fn get(&self, kind: ProviderKind) -> Option<&CachedModels> {
        self.by_provider.get(&kind)
    }

    pub fn put(
        &mut self,
        kind: ProviderKind,
        base_url: String,
        api_key: String,
        models: Vec<ModelInfo>,
    ) {
        self.by_provider.insert(
            kind,
            CachedModels {
                models,
                fetched_at: chrono::Utc::now(),
                base_url,
                api_key,
            },
        );
    }

    /// Returns true if base_url or api_key differ from cache, meaning a refetch is needed.
    pub fn needs_invalidation(&self, kind: ProviderKind, base_url: &str, api_key: &str) -> bool {
        match self.by_provider.get(&kind) {
            None => true,
            Some(c) => c.base_url != base_url || c.api_key != api_key,
        }
    }

    /// Load from a JSON file. Returns an empty cache if the file does not
    /// exist or cannot be parsed (best-effort — stale data is harmless).
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Save to a JSON file. Best-effort; the cache is not critical data.
    pub fn save(&self, path: &std::path::Path) {
        if let Ok(raw) = serde_json::to_string(self) {
            let _ = std::fs::write(path, &raw);
        }
    }

    pub fn clear(&mut self) {
        self.by_provider.clear();
    }
}
