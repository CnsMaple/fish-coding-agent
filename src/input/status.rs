use crate::config::ReasoningMode;
use crate::function::notifications::{HitRate, TokenRate};
use crate::theme::Theme;
use ratatui::text::{Line, Span};
use std::path::Path;

#[derive(Debug)]
pub struct StatusBar {
    pub cwd: String,
    pub mode: String,
    /// Display name for the active provider (either the user-defined
    /// `name` or the kind fallback like `openai`).
    pub provider: String,
    /// Display string for the active model, or `(no model)`.
    pub model: String,
    pub thinking: ReasoningMode,
    pub hit_cur: Option<f64>,
    pub hit_avg: Option<f64>,
    pub tok_cur: Option<f64>,
    pub tok_avg: Option<f64>,
    /// Cumulative output tokens across all responses (for total rate).
    pub total_output_tokens: u64,
    /// Cumulative elapsed time (secs) across all responses (for total rate).
    pub total_elapsed_secs: f64,
    /// Cumulative input tokens across all requests (for total hit rate).
    pub total_input_tokens: u64,
    /// Cumulative cache-read tokens across all requests (for total hit rate).
    pub total_cache_read: u64,
    pub token_total: Option<u64>,
    pub token_pct: Option<f64>,
    pub context_window_tokens: u64,
    pub context_window_known: bool,
    /// Mirrors `Config::auto_compact`. When `false`, the
    /// `cmp:` segment is omitted from the status line.
    pub auto_compact: bool,
    /// Best-effort `max_output_tokens` for the active model. `0`
    /// means "unknown" — the cmp segment is then suppressed
    /// because we cannot compute a stable `usable` value.
    pub max_output_tokens: u64,
    /// `(usable - used) / usable`, clamped to `[0.0, 1.0]`. `None`
    /// when the model has no known context window.
    pub compact_pct: Option<f64>,
    /// Set to `true` after the user has just triggered a compaction
    /// (or `/compact`), to flash the cmp segment in warn color
    /// ("cmp:triggered") instead of the green "cmp:N% free" form.
    /// Resets to `false` on the next `update_token_usage` call.
    pub compact_triggered: bool,
    /// Compact MCP server status summary, e.g. `"2✓ 1✗"`.
    /// `None` means no MCP servers are configured or enabled.
    pub mcp_summary: Option<String>,
}

impl StatusBar {
    pub fn new() -> Self {
        Self {
            cwd: String::from("~"),
            mode: String::from("yolo"),
            provider: String::new(),
            model: String::from("(no model)"),
            thinking: ReasoningMode::Off,
            hit_cur: None,
            hit_avg: None,
            tok_cur: None,
            tok_avg: None,
            total_output_tokens: 0,
            total_elapsed_secs: 0.0,
            total_input_tokens: 0,
            total_cache_read: 0,
            token_total: None,
            token_pct: None,
            context_window_tokens: 0,
            context_window_known: false,
            auto_compact: true,
            max_output_tokens: 0,
            compact_pct: None,
            compact_triggered: false,
            mcp_summary: None,
        }
    }

    pub fn set_cwd(&mut self, p: &Path) {
        // Show the full project path, but abbreviate the user's home
        // directory prefix as `~` so the line stays compact.
        if let Some(home) = dirs::home_dir() {
            if let Ok(stripped) = p.strip_prefix(&home) {
                self.cwd = format!("~/{}/", stripped.display());
                return;
            }
        }
        self.cwd = p.display().to_string();
    }

    pub fn set_mode(&mut self, mode: &str) {
        self.mode = mode.to_string();
    }

    pub fn set_provider_name(&mut self, name: &str) {
        self.provider = name.to_string();
    }

    pub fn set_model(&mut self, m: &str) {
        self.model = strip_stale_context_label(m).to_string();
    }

    pub fn set_context_window_tokens(&mut self, tokens: u64) {
        if tokens == 0 {
            return;
        }
        self.context_window_tokens = tokens;
        self.context_window_known = true;
        if let Some(total) = self.token_total {
            self.token_pct = Some(total as f64 / self.context_window_tokens as f64);
        }
        self.recompute_compact_pct();
    }

    pub fn clear_context_window_tokens(&mut self) {
        self.context_window_known = false;
        self.token_pct = None;
        self.compact_pct = None;
    }

    pub fn set_thinking(&mut self, t: ReasoningMode) {
        self.thinking = t;
    }

    /// Enable / disable the auto-compact `cmp:` segment. Mirrors
    /// `Config::auto_compact`. The compact headroom is computed
    /// on the fly inside `recompute_compact_pct` so callers do not
    /// have to keep the field in sync.
    pub fn set_auto_compact(&mut self, enabled: bool) {
        self.auto_compact = enabled;
        if !enabled {
            self.compact_pct = None;
            self.compact_triggered = false;
        }
        self.recompute_compact_pct();
    }

    /// Set the model's `max_output_tokens` used by the
    /// auto-compaction math. Pass `0` to indicate "unknown" (e.g.
    /// the active model has no metadata).
    pub fn set_max_output_tokens(&mut self, tokens: u64) {
        self.max_output_tokens = tokens;
        self.recompute_compact_pct();
    }

    /// Recompute `compact_pct` from the current `token_total` /
    /// `context_window_tokens` / `max_output_tokens`. No-op when
    /// auto-compact is disabled or when the context window /
    /// output budget is not known.
    ///
    /// When `max_output_tokens` is 0 (e.g. the active `ModelInfo`
    /// does not carry a separate output cap), we fall back to
    /// `ctx_window / 4` — the same default opencode uses when the
    /// provider does not advertise a max output. This keeps the cmp
    /// segment useful even for models that do not report their
    /// output limit.
    fn recompute_compact_pct(&mut self) {
        if !self.auto_compact {
            self.compact_pct = None;
            return;
        }
        let Some(used) = self.token_total else {
            self.compact_pct = None;
            return;
        };
        if !self.context_window_known || self.context_window_tokens == 0 {
            self.compact_pct = None;
            return;
        }
        let eff_output = if self.max_output_tokens == 0 {
            self.context_window_tokens / 4
        } else {
            self.max_output_tokens
        };
        let inp = crate::compaction::CompactionInputs {
            auto_enabled: self.auto_compact,
            ctx_window: self.context_window_tokens,
            max_output_tokens: eff_output,
            reserved_override: None,
        };
        self.compact_pct = crate::compaction::headroom_pct(used, inp);
    }

    /// Mark the segment as "triggered" — the next render emits
    /// `cmp:triggered` in warn color until token usage is updated
    /// again. Used by the auto-compaction path and `/compact` to
    /// flash the user when a summary is in flight.
    pub fn mark_compact_triggered(&mut self) {
        if self.auto_compact {
            self.compact_triggered = true;
        }
    }

    /// Reset token usage, hit rate, and token rate stats to defaults.
    /// Called when starting a new session (e.g., `/new`).
    pub fn reset_usage_stats(&mut self) {
        self.token_total = None;
        self.token_pct = None;
        self.compact_pct = None;
        self.hit_cur = None;
        self.hit_avg = None;
        self.tok_cur = None;
        self.tok_avg = None;
        self.total_output_tokens = 0;
        self.total_elapsed_secs = 0.0;
        self.total_input_tokens = 0;
        self.total_cache_read = 0;
        self.compact_triggered = false;
    }

    pub fn update_hit(&mut self, h: &HitRate) {
        self.hit_cur = h.current();
        self.hit_avg = h.average();
    }

    pub fn update_token_rate(&mut self, t: &TokenRate) {
        self.tok_cur = t.current();
        self.tok_avg = t.average();
    }

    pub fn set_mcp_summary(&mut self, summary: Option<String>) {
        self.mcp_summary = summary;
    }

    pub fn update_token_usage(&mut self, total: u64) {
        self.token_total = Some(total);
        self.token_pct = if self.context_window_known && self.context_window_tokens > 0 {
            Some(total as f64 / self.context_window_tokens as f64)
        } else {
            None
        };
        // A new usage reading also means we have a fresh headroom
        // number — clear the "triggered" flag so the bar returns
        // to the percentage form.
        self.compact_triggered = false;
        self.recompute_compact_pct();
    }

    /// Render the model / thinking / ctx / cmp line shown inside the
    /// input area title. tok, hit, and mcp stats are rendered
    /// separately on the cwd line via `render_stats_line`.
    pub fn render_line(&self) -> Line<'static> {
        self.render_line_with_mode(&self.mode)
    }

    pub fn render_line_with_mode(&self, mode: &str) -> Line<'static> {
        let fmt_pct = |v: Option<f64>| match v {
            None => "--".to_string(),
            Some(x) => format!("{:.1}%", x * 100.0),
        };
        let fmt_total = |v: Option<u64>| match v {
            None => "--".to_string(),
            Some(x) => fmt_tokens_k(x),
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled(mode.to_string(), Theme::bold()));
        spans.push(Span::raw(" | "));
        if !self.provider.is_empty() {
            spans.push(Span::styled(self.provider.clone(), Theme::bold()));
            spans.push(Span::raw(":"));
        }
        spans.push(Span::styled(
            strip_stale_context_label(&self.model).to_string(),
            Theme::base(),
        ));
        if self.provider != "cursor" {
            spans.push(Span::raw(" | think:"));
            spans.push(Span::styled(self.thinking.as_str(), Theme::bold()));
        }
        spans.push(Span::raw(" | ctx:"));
        if self.context_window_known {
            spans.push(Span::styled(fmt_pct(self.token_pct), Theme::base()));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(fmt_total(self.token_total), Theme::dim()));
            spans.push(Span::raw("/"));
            spans.push(Span::styled(
                fmt_tokens_k(self.context_window_tokens),
                Theme::dim(),
            ));
        } else {
            spans.push(Span::styled(fmt_total(self.token_total), Theme::dim()));
        }
        if self.auto_compact {
            if self.compact_triggered {
                spans.push(Span::raw(" | "));
                spans.push(Span::styled(
                    "cmp:triggered".to_string(),
                    Theme::status_warn(),
                ));
            } else if let Some(pct) = self.compact_pct {
                spans.push(Span::raw(" | cmp:"));
                spans.push(Span::styled(
                    format!("{:.0}% free", pct * 100.0),
                    Theme::base(),
                ));
            }
        }
        Line::from(spans)
    }

    /// Render only the tok, hit, and mcp stats — shown right-aligned on
    /// the cwd line below the input block.
    pub fn render_stats_line(&self) -> Line<'static> {
        let fmt_num = |v: Option<f64>| match v {
            None => "--".to_string(),
            Some(x) => format!("{:.1}", x),
        };
        let fmt_pct_int = |v: Option<f64>| match v {
            None => "--".to_string(),
            Some(x) => format!("{}", (x * 100.0).round() as u64),
        };
        let mut spans: Vec<Span<'static>> = Vec::new();

        // tok[current|average|total]
        let total_tok = if self.total_elapsed_secs > 0.0 {
            Some(self.total_output_tokens as f64 / self.total_elapsed_secs)
        } else {
            None
        };
        spans.push(Span::raw("tok["));
        spans.push(Span::styled(fmt_num(self.tok_cur), Theme::base()));
        spans.push(Span::raw("|"));
        spans.push(Span::styled(fmt_num(self.tok_avg), Theme::dim()));
        spans.push(Span::raw("|"));
        spans.push(Span::styled(fmt_num(total_tok), Theme::base()));
        spans.push(Span::raw("]"));

        // hit[current|average|total]
        let total_hit = if self.total_input_tokens > 0 {
            Some(self.total_cache_read as f64 / self.total_input_tokens as f64)
        } else {
            None
        };
        spans.push(Span::raw(" hit["));
        spans.push(Span::styled(fmt_pct_int(self.hit_cur), Theme::base()));
        spans.push(Span::raw("|"));
        spans.push(Span::styled(fmt_pct_int(self.hit_avg), Theme::dim()));
        spans.push(Span::raw("|"));
        spans.push(Span::styled(fmt_pct_int(total_hit), Theme::base()));
        spans.push(Span::raw("]"));

        if let Some(ref mcp) = self.mcp_summary {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled("mcp:", Theme::dim()));
            spans.push(Span::styled(mcp.clone(), Theme::base()));
        }
        Line::from(spans)
    }
}

impl Default for StatusBar {
    fn default() -> Self {
        Self::new()
    }
}

fn strip_stale_context_label(model: &str) -> &str {
    // Providers like "OpenAI: GPT-4o" or "Anthropic: Claude 3.5 Sonnet"
    // sometimes embed a stale " (128K)" context window label in the
    // saved session. Strip it unconditionally — the live context window
    // comes from the API response, not the model name.
    if let Some(idx) = model.rfind(" (") {
        let suffix = &model[idx + 2..];
        if suffix.ends_with('K') || suffix.ends_with('M') {
            let num_part = &suffix[..suffix.len() - 1];
            if num_part.parse::<u64>().is_ok() {
                return &model[..idx];
            }
        }
    }
    model
}

fn fmt_tokens_k(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        let v = tokens as f64 / 1_000.0;
        format!("{:.0}k", v)
    } else if tokens >= 1_000 {
        let v = tokens as f64 / 1_000.0;
        if tokens.is_multiple_of(1_000) {
            format!("{:.0}k", v)
        } else {
            format!("{:.1}k", v)
        }
    } else if tokens > 0 {
        format!("{}k", 1)
    } else {
        "0k".to_string()
    }
}
