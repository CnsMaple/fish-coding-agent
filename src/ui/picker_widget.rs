use crate::function::PickerFocus;
use crate::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

/// Renders a picker-style search row with focus-aware styling.
///
/// When the search field is focused (`focus == Search`), the label uses
/// bold style and a blank span at cursor position.
/// The actual terminal cursor is positioned at the end of the query text
/// (or at the label end when empty), so the terminal cursor color is used
/// instead of a drawn `█` character.
///
/// When unfocused, the label is dimmed and the placeholder
/// `"(type to filter)"` is shown instead.
pub fn render_search_row(
    area: Rect,
    buf: &mut Buffer,
    query: &str,
    focus: PickerFocus,
) -> Option<(u16, u16)> {
    let focused = focus == PickerFocus::Search;
    let prefix = if focused {
        Span::styled(" search: ", Theme::bold())
    } else {
        Span::styled(" search: ", Theme::dim())
    };
    let mut spans: Vec<Span<'static>> = vec![prefix];
    if query.is_empty() {
        if focused {
            // Show a visible empty-input indicator
            spans.push(Span::styled("\u{200B}", Theme::cursor()));
        } else {
            spans.push(Span::styled("(type to filter)", Theme::dim()));
        }
    } else {
        spans.push(Span::raw(query.to_string()));
        if focused {
            // Use a thin zero-width space so the terminal cursor is visible
            spans.push(Span::styled("\u{200B}", Theme::cursor()));
        }
    }
    Paragraph::new(Line::from(spans)).render(area, buf);

    if focused {
        let prefix_width = UnicodeWidthStr::width(" search: ") as u16;
        let query_width = UnicodeWidthStr::width(query) as u16;
        Some((area.x + prefix_width + query_width, area.y))
    } else {
        None
    }
}
