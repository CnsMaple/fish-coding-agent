use crate::function::PickerFocus;
use crate::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

/// Renders a picker-style search row with focus-aware styling.
///
/// When `searching` is true, the search field is active and shows the
/// query text with a cursor. When `searching` is false, a dim placeholder
/// `"(press Alt+i to search)"` is shown instead.
///
/// The `focus` parameter is used for the standard picker search rows
/// (notifications always use the `searching` flag to decide).
pub fn render_search_row(
    area: Rect,
    buf: &mut Buffer,
    query: &str,
    focus: PickerFocus,
    searching: bool,
) -> Option<(u16, u16)> {
    let focused = if searching {
        true
    } else {
        focus == PickerFocus::Search
    };
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
        } else if searching {
            spans.push(Span::styled("(type to filter)", Theme::dim()));
        } else {
            spans.push(Span::styled("(press Alt+i to search)", Theme::dim()));
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
