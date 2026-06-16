use super::{Role, Session};
use crate::config::ThinkingDisplay;
use crate::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

pub fn render(area: Rect, buf: &mut Buffer, session: &Session) {
    let inner_h = area.height as usize;
    let width = area.width as usize;
    if width == 0 || inner_h == 0 {
        return;
    }

    let lines = build_lines(session, width);

    let total = lines.len() as u16;
    let max_scroll = total.saturating_sub(inner_h as u16);
    let scroll = session.scroll.min(max_scroll);
    let start = total.saturating_sub(inner_h as u16 + scroll);
    let end = total.saturating_sub(scroll);

    let visible: Vec<Line> = if start < end {
        lines[start as usize..end as usize].to_vec()
    } else {
        vec![]
    };

    let p = Paragraph::new(visible)
        .wrap(Wrap { trim: false });
    p.render(area, buf);
}

/// Toggle label text used to identify thinking blocks in the rendered
/// buffer for mouse-interaction hit-testing.
pub const THINKING_TOGGLE_COLLAPSED: &str = "[thinking \u{25B8}]"; // ▸
pub const THINKING_TOGGLE_EXPANDED: &str = "[thinking \u{25BE}]";  // ▾
pub const THINKING_END: &str = "[end thinking]";

pub fn build_lines(session: &Session, width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for m in &session.messages {
        let role_style = match m.role {
            Role::User => Theme::role_user(),
            Role::Assistant => Theme::role_assistant(),
            Role::System => Theme::role_system(),
        };
        let arrow = Span::styled(" \u{203A} ", role_style);
        let prefix = Span::styled(m.role.prefix(), role_style);

        // Role prefix on its own line — content starts on the next line.
        out.push(Line::from(vec![prefix.clone(), arrow.clone()]));

        // Thinking block — after the prefix, before the content.
        let show_thinking = m.role == Role::Assistant
            && !m.thinking.trim().is_empty()
            && match session.display {
                ThinkingDisplay::Hide => false,
                ThinkingDisplay::Show => true,
                ThinkingDisplay::ShowWhileStreaming => true,
            };
        if show_thinking {
            let visible = match session.display {
                ThinkingDisplay::Show => m.thinking_visible,
                ThinkingDisplay::ShowWhileStreaming => m.streaming || m.thinking_visible,
                _ => false,
            };
            let toggle = if visible { THINKING_TOGGLE_EXPANDED } else { THINKING_TOGGLE_COLLAPSED };
            out.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(toggle.to_string(), Theme::dim()),
            ]));
            if visible {
                for tl in m.thinking.split('\n') {
                    out.push(Line::from(vec![
                        Span::raw("      "),
                        Span::styled(tl.to_string(), Theme::dim()),
                    ]));
                }
                out.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(THINKING_END.to_string(), Theme::dim()),
                ]));
            }
        }

        // Message content rendered from Markdown, each line indented.
        // During streaming the parser may see an incomplete table (header
        // without separator) and emit raw pipe characters for a frame or
        // two; this is acceptable and much better than flickering between
        // plain and rendered text.
        let display_text = if m.streaming {
            m.visible_content()
        } else {
            &m.content
        };
        let md_lines = crate::session::markdown::render_with_width(display_text, width.saturating_sub(3));
        for line in md_lines {
            let mut indented = vec![Span::raw("   ")];
            indented.extend(line.spans.into_iter());
            out.push(Line::from(indented));
        }

        // Streaming cursor
        if m.streaming {
            if let Some(last) = out.last_mut() {
                let mut s = last.spans.clone();
                s.push(Span::styled("\u{258C}", Theme::cursor()));
                *last = Line::from(s);
            } else {
                out.push(Line::from(Span::styled("\u{258C}", Theme::cursor())));
            }
        }
        // blank line between messages
        out.push(Line::from(""));
    }
    while out.last().map(|l| l.width() == 0).unwrap_or(false) {
        out.pop();
    }
    if !out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

/// helper used by tests / other renderers
pub fn visible_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThinkingDisplay;
    use crate::session::{Message, Role, Session};

    fn session_with_table_table() -> Session {
        let mut s = Session::default();
        s.display = ThinkingDisplay::Show;
        s.push(Message::new(Role::User, "give me a table"));
        s.push(Message {
            role: Role::Assistant,
            content: "| 列 1 | 列 2 |\n|---|---|\n| A | B |".into(),
            thinking: String::new(),
            thinking_visible: false,
            display_cursor: usize::MAX,
            ts: chrono::Utc::now(),
            streaming: false,
        });
        s
    }

    #[test]
    fn build_lines_renders_table() {
        let session = session_with_table_table();
        let lines = build_lines(&session, 100);
        // Join each line's spans into a string first, then join lines
        // with a space. This is the same shape the markdown tests use
        // and avoids inserting a space between every single-char span
        // (cells get wrapped into one span per char so the column
        // widths line up; flat-map+join would put phantom spaces
        // between "列" and "1" inside a cell).
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("列 1"), "header missing:\n{text}");
        assert!(text.contains("列 2"), "header missing:\n{text}");
        assert!(text.contains("A"), "cell A missing:\n{text}");
        assert!(text.contains("B"), "cell B missing:\n{text}");
        // Pipes should NOT appear raw.
        assert!(!text.contains("||"), "raw pipes leaked:\n{text}");
        // ...and the box-drawing border should be present.
        assert!(text.contains("┌"), "border missing:\n{text}");
    }
}
