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

    /// Remove the most recent toast whose text contains `substr`
    /// (case-insensitive). Used to clean up transient warnings such as
    /// rate-limit retry messages once the retry succeeds.
    pub fn remove_last_containing(&mut self, substr: &str) {
        let query = substr.to_ascii_lowercase();
        if let Some(idx) = self
            .items
            .iter()
            .rposition(|t| t.text.to_ascii_lowercase().contains(&query))
        {
            self.items.remove(idx);
            self.clamp_cursor();
        }
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
    }

    pub fn move_down(&mut self) {
        let len = self.filtered_indices().len();
        if len == 0 {
            self.cursor = 0;
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

    pub fn clear(&mut self) {
        self.window.clear();
    }
}

/// Token rate tracker.
///
/// Tracks the token rate (tokens/second) of the current in-flight
/// response as a live value, plus the cumulative average of every
/// completed response (no sliding window — the average spans all
/// recorded samples). The live rate is also folded into the average
/// and the total while streaming so all three numbers update together.
#[derive(Debug)]
pub struct TokenRate {
    /// Latest rate: the live in-flight rate while streaming, or the
    /// final rate of the last completed response.
    current: Option<f64>,
    /// Sum of the rates of all completed responses.
    sum: f64,
    /// Number of completed responses.
    count: u64,
    /// `true` while a live (uncommitted) rate is being displayed.
    live: bool,
    /// Live token count of the in-flight response (for the total).
    live_tokens: u64,
    /// Live effective elapsed seconds of the in-flight response (for the total).
    live_elapsed: f64,
}

impl Default for TokenRate {
    fn default() -> Self {
        Self {
            current: None,
            sum: 0.0,
            count: 0,
            live: false,
            live_tokens: 0,
            live_elapsed: 0.0,
        }
    }
}

impl TokenRate {
    pub fn new(_cap: usize) -> Self {
        Self::default()
    }

    /// Update the live in-flight rate without committing it to the
    /// cumulative average. Called periodically while streaming.
    pub fn update_live(&mut self, tokens: u64, elapsed: f64) {
        self.live_tokens = tokens;
        self.live_elapsed = elapsed;
        self.live = true;
        if elapsed > 0.0 {
            self.current = Some(tokens as f64 / elapsed);
        }
    }

    /// Commit a completed response's rate into the cumulative average.
    pub fn record(&mut self, val: f64) {
        self.sum += val;
        self.count += 1;
        self.current = Some(val);
        self.live_tokens = 0;
        self.live_elapsed = 0.0;
    }

    pub fn current(&self) -> Option<f64> {
        self.current
    }

    /// Average of all recorded rates, including the live in-flight
    /// rate as one additional sample while streaming.
    pub fn average(&self) -> Option<f64> {
        if let Some(c) = self.current {
            if self.live {
                return Some((self.sum + c) / (self.count as f64 + 1.0));
            }
        }
        if self.count > 0 {
            Some(self.sum / self.count as f64)
        } else {
            self.current
        }
    }

    pub fn live_tokens(&self) -> u64 {
        self.live_tokens
    }

    pub fn live_elapsed(&self) -> f64 {
        self.live_elapsed
    }

    pub fn clear(&mut self) {
        *self = Self::default();
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
    /// Input modality types from models.dev (e.g. ["text", "image"]).
    #[serde(default)]
    pub modalities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedModels {
    pub models: Vec<ModelInfo>,
    pub fetched_at: chrono::DateTime<Utc>,
    pub base_url: String,
    pub api_key: String,
    /// The ProviderKind of the entry that populated this cache.
    /// Used for backward-compatible kind-based lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ProviderKind>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ModelCache {
    /// Cache keyed by entry_id (e.g. "openai_chat:key" or "openai:key-2")
    /// so that different entries with the same ProviderKind do not
    /// overwrite each other's cached model lists.
    pub by_entry: HashMap<String, CachedModels>,
}

impl ModelCache {
    /// Look up cached models by exact entry_id.
    pub fn get(&self, entry_id: &str) -> Option<&CachedModels> {
        self.by_entry.get(entry_id)
    }

    pub fn put(
        &mut self,
        entry_id: String,
        kind: ProviderKind,
        base_url: String,
        api_key: String,
        models: Vec<ModelInfo>,
    ) {
        self.by_entry.insert(
            entry_id,
            CachedModels {
                models,
                fetched_at: chrono::Utc::now(),
                base_url,
                api_key,
                kind: Some(kind),
            },
        );
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
        self.by_entry.clear();
    }
}
