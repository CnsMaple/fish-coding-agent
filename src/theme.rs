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
    diff_added_bg: Color::Rgb(165, 214, 167),
    diff_added_fg: Color::Rgb(46, 125, 50),
    diff_removed_bg: Color::Rgb(239, 154, 154),
    diff_removed_fg: Color::Rgb(198, 40, 40),
});

/// Active theme variant, readable at any time.
static ACTIVE_VARIANT: RwLock<ThemeVariant> = RwLock::new(ThemeVariant::AutoEucalyptus);

/// Get the currently active theme colors.
pub fn active_colors() -> ThemeColors {
    ACTIVE_COLORS.read().unwrap().clone()
}

/// Get the currently active theme variant.
pub fn active_variant() -> ThemeVariant {
    *ACTIVE_VARIANT.read().unwrap()
}

/// Initialize or update the theme colors from the selected variant.
///
/// For `AutoEucalyptus` this blocks on the terminal-background probe to
/// detect light/dark before the colors are applied. At startup prefer
/// [`init_theme_deferred`] so the TUI appears immediately; the deferred
/// resolution runs in the background once the TUI is up.
pub fn init_theme(variant: ThemeVariant) {
    let colors = ThemeColors::from_variant(variant);
    *ACTIVE_COLORS.write().unwrap() = colors;
    *ACTIVE_VARIANT.write().unwrap() = variant;
}

/// Initialize the theme without blocking the calling thread.
///
/// `auto-eucalyptus` uses the dark fallback so startup never waits on
/// the terminal-background probe; the real light/dark resolution runs
/// later via [`resolve_auto_variant_async`] and is applied with
/// [`init_theme`].
pub fn init_theme_deferred(variant: ThemeVariant) {
    let colors = ThemeColors::from_variant_nonblocking(variant);
    *ACTIVE_COLORS.write().unwrap() = colors;
    *ACTIVE_VARIANT.write().unwrap() = variant;
}

/// The concrete variant backing the current selection, without probing.
/// An unresolved `auto-eucalyptus` maps to the dark fallback so the
/// markdown syntax-highlight path picks a stable default without
/// waiting on the `termbg` probe.
pub fn effective_variant() -> ThemeVariant {
    match active_variant() {
        ThemeVariant::AutoEucalyptus => ThemeVariant::DarkEucalyptus,
        v => v,
    }
}

/// Available theme variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThemeVariant {
    /// Follow the terminal's default colors; use modifiers for emphasis only.
    #[serde(rename = "terminal-color")]
    System,
    #[serde(rename = "light-eucalyptus")]
    LightEucalyptus,
    #[serde(rename = "dark-eucalyptus")]
    DarkEucalyptus,
    /// Auto-detect the terminal background and pick light or dark.
    /// This is the default theme.
    #[serde(rename = "auto-eucalyptus")]
    #[default]
    AutoEucalyptus,
}

impl ThemeVariant {
    pub fn as_str(&self) -> &'static str {
        match self {
            ThemeVariant::System => "terminal-color",
            ThemeVariant::LightEucalyptus => "light-eucalyptus",
            ThemeVariant::DarkEucalyptus => "dark-eucalyptus",
            ThemeVariant::AutoEucalyptus => "auto-eucalyptus",
        }
    }

    pub fn all() -> &'static [ThemeVariant] {
        &[
            ThemeVariant::System,
            ThemeVariant::LightEucalyptus,
            ThemeVariant::DarkEucalyptus,
            ThemeVariant::AutoEucalyptus,
        ]
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
    /// Background color for added (diff `+`) lines.
    pub diff_added_bg: Color,
    /// Foreground color for the added-line sign / text.
    pub diff_added_fg: Color,
    /// Background color for removed (diff `-`) lines.
    pub diff_removed_bg: Color,
    /// Foreground color for the removed-line sign / text.
    pub diff_removed_fg: Color,
}

impl Default for ThemeColors {
    fn default() -> Self {
        Self::auto_eucalyptus()
    }
}

impl ThemeColors {
    /// Terminal-color theme: defer to the system palette.
    fn system_theme() -> Self {
        Self {
            tool_pending_bg: Color::Yellow,
            tool_success_bg: Color::Green,
            tool_error_bg: Color::Red,
            tool_error_fg: Color::Reset,
            cursor_fg: Color::Reset,
            thinking_streaming_bg: Color::Yellow,
            thinking_done_bg: Color::Green,
            user_bg: Color::Rgb(224, 247, 250),
            diff_added_bg: Color::Rgb(165, 214, 167),
            diff_added_fg: Color::Rgb(46, 125, 50),
            diff_removed_bg: Color::Rgb(239, 154, 154),
            diff_removed_fg: Color::Rgb(198, 40, 40),
        }
    }

    fn light_eucalyptus() -> Self {
        Self {
            // Soothing pastel backgrounds for a light theme
            tool_pending_bg: Color::Rgb(230, 245, 243), // #E6F5F3
            tool_success_bg: Color::Rgb(232, 245, 233), // #E8F5E9
            tool_error_bg: Color::Rgb(255, 235, 238),   // #FFEBEE
            tool_error_fg: Color::Rgb(202, 67, 67),     // #CA4343
            cursor_fg: Color::Rgb(5, 150, 105),         // emerald-600
            thinking_streaming_bg: Color::Rgb(230, 245, 243), // #E6F5F3
            thinking_done_bg: Color::Rgb(232, 245, 233), // #E8F5E9
            user_bg: Color::Rgb(224, 247, 250),         // #E0F7FA (matches default)
            // Soft sage / blush diff colors tuned for the light palette
            diff_added_bg: Color::Rgb(150, 199, 152), // #96C798
            diff_added_fg: Color::Rgb(20, 75, 25),    // green-900
            diff_removed_bg: Color::Rgb(224, 139, 139), // #E08B8B
            diff_removed_fg: Color::Rgb(120, 20, 20), // red-900
        }
    }

    fn dark_eucalyptus() -> Self {
        Self {
            // Deep muted eucalyptus backgrounds for a dark theme
            tool_pending_bg: Color::Rgb(20, 40, 42), // #14282A
            tool_success_bg: Color::Rgb(22, 45, 35), // #162D23
            tool_error_bg: Color::Rgb(45, 22, 28),   // #2D161C
            tool_error_fg: Color::Rgb(235, 130, 130), // #EB8282
            cursor_fg: Color::Rgb(110, 231, 183),    // emerald-400
            thinking_streaming_bg: Color::Rgb(20, 40, 42), // #14282A
            thinking_done_bg: Color::Rgb(22, 45, 35), // #162D23
            user_bg: Color::Rgb(22, 50, 56),         // #163238
            // Diff backgrounds kept very dark so syntax-highlighted code
            // stays readable; only the sign uses a clear color.
            diff_added_bg: Color::Rgb(70, 96, 78), // dark green tint
            diff_added_fg: Color::Rgb(110, 200, 140), // clear green (sign)
            diff_removed_bg: Color::Rgb(96, 70, 74), // dark red tint
            diff_removed_fg: Color::Rgb(200, 120, 120), // clear red (sign)
        }
    }

    /// Pick light or dark based on the detected terminal background theme.
    fn auto_eucalyptus() -> Self {
        match resolve_auto_variant() {
            ThemeVariant::LightEucalyptus => Self::light_eucalyptus(),
            _ => Self::dark_eucalyptus(),
        }
    }

    pub fn from_variant(variant: ThemeVariant) -> Self {
        match variant {
            ThemeVariant::System => Self::system_theme(),
            ThemeVariant::LightEucalyptus => Self::light_eucalyptus(),
            ThemeVariant::DarkEucalyptus => Self::dark_eucalyptus(),
            ThemeVariant::AutoEucalyptus => Self::auto_eucalyptus(),
        }
    }

    /// Like [`Self::from_variant`] but never blocks: `auto-eucalyptus`
    /// falls back to the dark palette until the background is resolved.
    pub fn from_variant_nonblocking(variant: ThemeVariant) -> Self {
        match variant {
            ThemeVariant::System => Self::system_theme(),
            ThemeVariant::LightEucalyptus => Self::light_eucalyptus(),
            ThemeVariant::DarkEucalyptus => Self::dark_eucalyptus(),
            ThemeVariant::AutoEucalyptus => Self::dark_eucalyptus(),
        }
    }
}

/// Resolve the concrete eucalyptus variant backing `auto-eucalyptus` by
/// detecting the terminal background theme. Falls back to dark on failure.
pub fn resolve_auto_variant() -> ThemeVariant {
    let timeout = std::time::Duration::from_millis(100);
    match termbg::theme(timeout) {
        Ok(termbg::Theme::Light) => ThemeVariant::LightEucalyptus,
        _ => ThemeVariant::DarkEucalyptus,
    }
}

/// Resolve the auto theme variant on a blocking thread pool and return
/// the resolved variant. The `termbg` probe can block for up to ~100ms,
/// so this keeps it off the async runtime / render thread.
pub async fn resolve_auto_variant_async() -> ThemeVariant {
    tokio::task::spawn_blocking(resolve_auto_variant)
        .await
        .unwrap_or(ThemeVariant::DarkEucalyptus)
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
        Style::default()
            .fg(active_colors().cursor_fg)
            .bg(Color::Reset)
    }

    /// Visible cursor indicator for form fields. Uses bold so the cursor
    /// character stands out without relying on REVERSED background.
    pub fn cursor_visible() -> Style {
        Style::default()
            .fg(active_colors().cursor_fg)
            .bg(Color::Reset)
            .add_modifier(Modifier::BOLD)
    }

    pub fn block_running() -> Style {
        Self::base().bg(active_colors().tool_pending_bg)
    }

    pub fn block_done() -> Style {
        Self::base().bg(active_colors().tool_success_bg)
    }

    pub fn block_failed() -> Style {
        Self::base()
            .fg(active_colors().tool_error_fg)
            .bg(active_colors().tool_error_bg)
            .add_modifier(Modifier::BOLD)
    }

    /// Background for user message blocks.
    pub fn user_block() -> Style {
        Self::base().bg(active_colors().user_bg)
    }

    /// Todo status styles.
    pub fn todo_pending() -> Style {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    }

    pub fn todo_in_progress() -> Style {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }

    pub fn todo_completed() -> Style {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    }

    /// Background style for an added (diff `+`) line.
    pub fn diff_added_bg() -> Style {
        Self::base().bg(active_colors().diff_added_bg)
    }

    /// Foreground color for an added-line sign / text.
    pub fn diff_added_fg() -> Color {
        active_colors().diff_added_fg
    }

    /// Background color for an added-line.
    pub fn diff_added_bg_color() -> Color {
        active_colors().diff_added_bg
    }

    /// Background style for a removed (diff `-`) line.
    pub fn diff_removed_bg() -> Style {
        Self::base().bg(active_colors().diff_removed_bg)
    }

    /// Foreground color for a removed-line sign / text.
    pub fn diff_removed_fg() -> Color {
        active_colors().diff_removed_fg
    }

    /// Background color for a removed-line.
    pub fn diff_removed_bg_color() -> Color {
        active_colors().diff_removed_bg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_variant_falls_back_to_dark_for_auto() {
        let prev = active_variant();
        init_theme(ThemeVariant::AutoEucalyptus);
        assert_eq!(effective_variant(), ThemeVariant::DarkEucalyptus);
        init_theme(prev);
    }

    #[test]
    fn effective_variant_passes_through_concrete_variants() {
        assert_eq!(
            effective_variant_for(ThemeVariant::LightEucalyptus),
            ThemeVariant::LightEucalyptus
        );
        assert_eq!(
            effective_variant_for(ThemeVariant::DarkEucalyptus),
            ThemeVariant::DarkEucalyptus
        );
        assert_eq!(
            effective_variant_for(ThemeVariant::System),
            ThemeVariant::System
        );
    }

    fn effective_variant_for(v: ThemeVariant) -> ThemeVariant {
        let prev = active_variant();
        init_theme(v);
        let out = effective_variant();
        init_theme(prev);
        out
    }
}
