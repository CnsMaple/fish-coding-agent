use crate::config::ReasoningMode;
use crate::function::notifications::{HitRate, TokenRate};
use crate::theme::Theme;
use ratatui::text::{Line, Span};
use std::path::Path;

const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 128_000;

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
    pub token_total: Option<u64>,
    pub token_pct: Option<f64>,
    pub context_window_tokens: u64,
    pub context_window_known: bool,
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
            token_total: None,
            token_pct: None,
            context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            context_window_known: true,
        }
    }

    pub fn set_cwd(&mut self, p: &Path) {
        // Show the full project path, but abbreviate the user's home
        // directory prefix as `~` so the line stays compact.
        if let Some(home) = dirs::home_dir() {
            if let Ok(stripped) = p.strip_prefix(&home) {
                self.cwd = format!("~/{}", stripped.display());
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
        self.context_window_tokens = infer_context_window_tokens(m);
        self.context_window_known = true;
        if let Some(total) = self.token_total {
            self.token_pct = Some(total as f64 / self.context_window_tokens as f64);
        }
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
    }

    pub fn clear_context_window_tokens(&mut self) {
        self.context_window_known = false;
        self.token_pct = None;
    }

    pub fn set_thinking(&mut self, t: ReasoningMode) {
        self.thinking = t;
    }

    pub fn update_hit(&mut self, h: &HitRate) {
        self.hit_cur = h.current();
        self.hit_avg = h.average();
    }

    pub fn update_token_rate(&mut self, t: &TokenRate) {
        self.tok_cur = t.current();
        self.tok_avg = t.average();
    }

    pub fn update_token_usage(&mut self, total: u64) {
        self.token_total = Some(total);
        self.token_pct = if self.context_window_known {
            Some(total as f64 / self.context_window_tokens as f64)
        } else {
            None
        };
    }

    /// Render the model / thinking / hit line shown inside the input
    /// area. The project cwd is intentionally NOT included here — it is
    /// rendered on its own line below the input block.
    ///
    /// If no provider name is set, we omit the `name:` prefix so the
    /// user doesn't see a stray `-:(no model)` style label.
    pub fn render_line(&self) -> Line<'static> {
        self.render_line_with_mode(&self.mode)
    }

    pub fn render_line_with_mode(&self, mode: &str) -> Line<'static> {
        let fmt_pct = |v: Option<f64>| match v {
            None => "--".to_string(),
            Some(x) => format!("{:.1}%", x * 100.0),
        };
        let fmt_tps = |v: Option<f64>| match v {
            None => "--".to_string(),
            Some(x) => format!("{:.1}/s", x),
        };
        let fmt_total = |v: Option<u64>| match v {
            None => "--".to_string(),
            Some(x) => fmt_tokens(x),
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
        spans.push(Span::raw(" | tok:"));
        spans.push(Span::styled(fmt_tps(self.tok_cur), Theme::base()));
        spans.push(Span::raw("/avg "));
        spans.push(Span::styled(fmt_tps(self.tok_avg), Theme::dim()));
        spans.push(Span::raw(" | ctx:"));
        if self.context_window_known {
            spans.push(Span::styled(fmt_pct(self.token_pct), Theme::base()));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(fmt_total(self.token_total), Theme::dim()));
            spans.push(Span::raw("/"));
            spans.push(Span::styled(
                fmt_tokens(self.context_window_tokens),
                Theme::dim(),
            ));
        } else {
            spans.push(Span::styled(fmt_total(self.token_total), Theme::dim()));
        }
        spans.push(Span::raw(" | hit:"));
        spans.push(Span::styled(fmt_pct(self.hit_cur), Theme::base()));
        spans.push(Span::raw("/avg "));
        spans.push(Span::styled(fmt_pct(self.hit_avg), Theme::dim()));
        Line::from(spans)
    }
}

fn strip_stale_context_label(model: &str) -> &str {
    let trimmed = model.trim_end();
    trimmed
        .strip_suffix(" [200K]")
        .or_else(|| trimmed.strip_suffix(" [200k]"))
        .unwrap_or(trimmed)
}

fn infer_context_window_tokens(model: &str) -> u64 {
    let model = model.to_lowercase();
    if model.contains("minimax-m3") || model.contains("minimax:m3") || model == "m3" {
        512_000
    } else if model.contains("claude") {
        200_000
    } else if model.contains("gemini-1.5") || model.contains("gemini-2") {
        1_000_000
    } else if model.contains("gpt-4.1") || model.contains("gpt-4o") || model.contains("o3") {
        128_000
    } else {
        DEFAULT_CONTEXT_WINDOW_TOKENS
    }
}

fn fmt_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        let v = tokens as f64 / 1_000_000.0;
        if tokens % 1_000_000 == 0 {
            format!("{:.0}m", v)
        } else {
            format!("{:.1}m", v)
        }
    } else if tokens >= 1_000 {
        let v = tokens as f64 / 1_000.0;
        if tokens % 1_000 == 0 {
            format!("{:.0}k", v)
        } else {
            format!("{:.1}k", v)
        }
    } else {
        tokens.to_string()
    }
}
