use ratatui::style::{Color, Modifier, Style};

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
        // The input cursor is a solid block drawn in the same color as text.
        // Use base style (no REVERSED) so the block visually matches the
        // surrounding text foreground.
        Self::base()
    }

    /// Visible cursor indicator for form fields. Uses bold so the cursor
    /// character stands out without relying on REVERSED background.
    pub fn cursor_visible() -> Style {
        Self::bold()
    }
}
