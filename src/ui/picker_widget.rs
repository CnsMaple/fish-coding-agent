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
/// bold style, the query text is followed by a block cursor symbol,
/// and the function returns the screen coordinate of the text cursor
/// so the caller can pass it to `App::function_panel_cursor` for IME
/// composition window positioning.
///
/// When unfocused, the label is dimmed and the placeholder
/// `"(type to filter)"` is shown instead of the cursor.
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
        spans.push(Span::styled(
            if focused { "\u{2588}" } else { "(type to filter)" },
            if focused { Theme::cursor() } else { Theme::dim() },
        ));
    } else {
        spans.push(Span::raw(query.to_string()));
        if focused {
            spans.push(Span::styled("\u{2588}", Theme::cursor()));
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
