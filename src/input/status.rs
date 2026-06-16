use crate::config::ReasoningMode;
use crate::function::notifications::HitRate;
use crate::theme::Theme;
use ratatui::text::{Line, Span};
use std::path::Path;

#[derive(Debug)]
pub struct StatusBar {
    pub cwd: String,
    /// Display name for the active provider (either the user-defined
    /// `name` or the kind fallback like `openai`).
    pub provider: String,
    /// Display string for the active model, or `(no model)`.
    pub model: String,
    pub thinking: ReasoningMode,
    pub hit_cur: Option<f64>,
    pub hit_avg: Option<f64>,
}

impl StatusBar {
    pub fn new() -> Self {
        Self {
            cwd: String::from("~"),
            provider: String::new(),
            model: String::from("(no model)"),
            thinking: ReasoningMode::Off,
            hit_cur: None,
            hit_avg: None,
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

    pub fn set_provider_name(&mut self, name: &str) {
        self.provider = name.to_string();
    }

    pub fn set_model(&mut self, m: &str) {
        self.model = m.to_string();
    }

    pub fn set_thinking(&mut self, t: ReasoningMode) {
        self.thinking = t;
    }

    pub fn update_hit(&mut self, h: &HitRate) {
        self.hit_cur = h.current();
        self.hit_avg = h.average();
    }

    /// Render the model / thinking / hit line shown inside the input
    /// area. The project cwd is intentionally NOT included here — it is
    /// rendered on its own line below the input block.
    ///
    /// If no provider name is set, we omit the `name:` prefix so the
    /// user doesn't see a stray `-:(no model)` style label.
    pub fn render_line(&self) -> Line<'static> {
        let fmt = |v: Option<f64>| match v {
            None => "--".to_string(),
            Some(x) => format!("{:.1}%", x * 100.0),
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        if !self.provider.is_empty() {
            spans.push(Span::styled(self.provider.clone(), Theme::bold()));
            spans.push(Span::raw(":"));
        }
        spans.push(Span::styled(self.model.clone(), Theme::base()));
        spans.push(Span::raw(" | think:"));
        spans.push(Span::styled(self.thinking.as_str(), Theme::bold()));
        spans.push(Span::raw(" | hit:"));
        spans.push(Span::styled(fmt(self.hit_cur), Theme::base()));
        spans.push(Span::raw("/avg "));
        spans.push(Span::styled(fmt(self.hit_avg), Theme::dim()));
        Line::from(spans)
    }
}
