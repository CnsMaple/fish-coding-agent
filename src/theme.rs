use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};
use std::sync::RwLock;

/// Active theme colors, readable at any time.
/// Initialized with the default theme; updated via `init_theme` at startup or on settings change.
static ACTIVE_COLORS: RwLock<ThemeColors> = RwLock::new(ThemeColors {
    tool_pending_bg: Color::Yellow,
    tool_success_bg: Color::Green,
    tool_error_bg: Color::Red,
    tool_error_fg: Color::Reset,
    cursor_fg: Color::Reset,
    thinking_streaming_bg: Color::Yellow,
    thinking_done_bg: Color::Green,
    user_bg: Color::Rgb(224, 247, 250),
});

/// Get the currently active theme colors.
pub fn active_colors() -> ThemeColors {
    ACTIVE_COLORS.read().unwrap().clone()
}

/// Initialize or update the theme colors from the selected variant.
pub fn init_theme(variant: ThemeVariant) {
    let colors = ThemeColors::from_variant(variant);
    *ACTIVE_COLORS.write().unwrap() = colors;
}

/// Available theme variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeVariant {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "light-eucalyptus")]
    LightEucalyptus,
}

impl ThemeVariant {
    pub fn as_str(&self) -> &'static str {
        match self {
            ThemeVariant::Default => "default",
            ThemeVariant::LightEucalyptus => "light-eucalyptus",
        }
    }

    pub fn all() -> &'static [ThemeVariant] {
        &[ThemeVariant::Default, ThemeVariant::LightEucalyptus]
    }
}

impl Default for ThemeVariant {
    fn default() -> Self {
        ThemeVariant::Default
    }
}

/// Color values used throughout the TUI.
#[derive(Clone)]
pub struct ThemeColors {
    /// Background color for running/pending tool blocks.
    pub tool_pending_bg: Color,
    /// Background color for successful tool blocks.
    pub tool_success_bg: Color,
    /// Background color for failed tool blocks.
    pub tool_error_bg: Color,
    /// Foreground color for text in failed tool blocks.
    pub tool_error_fg: Color,
    /// Foreground color for cursor indicators.
    pub cursor_fg: Color,
    /// Background color for the thinking block when streaming.
    pub thinking_streaming_bg: Color,
    /// Background color for the thinking block when done.
    pub thinking_done_bg: Color,
    /// Background color for user message blocks.
    pub user_bg: Color,
}

impl Default for ThemeColors {
    fn default() -> Self {
        Self::default_theme()
    }
}

impl ThemeColors {
    fn default_theme() -> Self {
        Self {
            tool_pending_bg: Color::Yellow,
            tool_success_bg: Color::Green,
            tool_error_bg: Color::Red,
            tool_error_fg: Color::Reset,
            cursor_fg: Color::Reset,
            thinking_streaming_bg: Color::Yellow,
            thinking_done_bg: Color::Green,
            user_bg: Color::Rgb(224, 247, 250),
        }
    }

    fn light_eucalyptus() -> Self {
        Self {
            // Soothing pastel backgrounds for a light theme
            tool_pending_bg: Color::Rgb(230, 245, 243),     // #E6F5F3
            tool_success_bg: Color::Rgb(232, 245, 233),     // #E8F5E9
            tool_error_bg: Color::Rgb(232, 245, 233),       // #E8F5E9 (same as success)
            tool_error_fg: Color::Rgb(202, 67, 67),          // #CA4343
            cursor_fg: Color::Rgb(5, 150, 105),              // emerald-600
            thinking_streaming_bg: Color::Rgb(230, 245, 243), // #E6F5F3
            thinking_done_bg: Color::Rgb(232, 245, 233),     // #E8F5E9
            user_bg: Color::Rgb(224, 247, 250),              // #E0F7FA (matches default)
        }
    }

    pub fn from_variant(variant: ThemeVariant) -> Self {
        match variant {
            ThemeVariant::Default => Self::default_theme(),
            ThemeVariant::LightEucalyptus => Self::light_eucalyptus(),
        }
    }
}

/// System theme: defer to terminal defaults, use modifiers for emphasis only.
pub struct Theme;

impl Theme {
    pub fn base() -> Style {
        Style::default().fg(Color::Reset).bg(Color::Reset)
    }

    pub fn dim() -> Style {
        Self::base().add_modifier(Modifier::DIM)
    }

    pub fn bold() -> Style {
        Self::base().add_modifier(Modifier::BOLD)
    }

    pub fn underlined() -> Style {
        Self::base().add_modifier(Modifier::UNDERLINED)
    }

    pub fn reversed() -> Style {
        Self::base().add_modifier(Modifier::REVERSED)
    }

    pub fn italic() -> Style {
        Self::base().add_modifier(Modifier::ITALIC)
    }

    pub fn role_user() -> Style {
        Self::base()
    }

    pub fn role_assistant() -> Style {
        Self::base().add_modifier(Modifier::BOLD)
    }

    pub fn role_system() -> Style {
        Self::base().add_modifier(Modifier::DIM)
    }

    /// Status text in [ok] [fail] [warn] [info] cells.
    pub fn status_ok() -> Style {
        Self::base()
    }

    pub fn status_info() -> Style {
        Self::dim()
    }

    pub fn status_warn() -> Style {
        Self::bold()
    }

    pub fn status_fail() -> Style {
        Self::reversed()
    }

    pub fn focused_border() -> Style {
        Self::base()
    }

    pub fn unfocused_border() -> Style {
        Self::dim()
    }

    pub fn selection() -> Style {
        Self::base().add_modifier(Modifier::REVERSED)
    }

    pub fn cursor() -> Style {
        Style::default().fg(active_colors().cursor_fg).bg(Color::Reset)
    }

    /// Visible cursor indicator for form fields. Uses bold so the cursor
    /// character stands out without relying on REVERSED background.
    pub fn cursor_visible() -> Style {
        Style::default().fg(active_colors().cursor_fg).bg(Color::Reset).add_modifier(Modifier::BOLD)
    }

    pub fn block_running() -> Style {
        Self::base().bg(active_colors().tool_pending_bg)
    }

    pub fn block_done() -> Style {
        Self::base().bg(active_colors().tool_success_bg)
    }

    pub fn block_failed() -> Style {
        Self::base().fg(active_colors().tool_error_fg).bg(active_colors().tool_error_bg).add_modifier(Modifier::BOLD)
    }

    /// Background for user message blocks.
    pub fn user_block() -> Style {
        Self::base().bg(active_colors().user_bg)
    }
}
