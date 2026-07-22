use std::ops::Range;

use crate::app::App;
use crate::function::SidebarTab;
use crate::theme::Theme;
use crate::ui::tab_widget::TabWidget;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Widget, Wrap};

/// Keep the cursor inside the visible window while scrolling.
pub fn ensure_cursor_visible(cursor: usize, scroll: &mut usize, visible_rows: usize) {
    if visible_rows == 0 {
        return;
    }
    if cursor < *scroll {
        *scroll = cursor;
    } else if cursor >= *scroll + visible_rows {
        *scroll = cursor + 1 - visible_rows;
    }
}

/// Calculate the range of visible rows, auto-scrolling if the cursor is
/// outside the visible window.
pub fn visible_window(
    cursor: usize,
    scroll: &mut usize,
    visible_rows: usize,
    total: usize,
) -> Range<usize> {
    ensure_cursor_visible(cursor, scroll, visible_rows);
    let start = (*scroll).min(total);
    let end = (start + visible_rows).min(total);
    start..end
}

pub fn render(area: Rect, buf: &mut Buffer, app: &mut App) {
    if area.width < 4 || area.height < 4 {
        return;
    }

    let mut title_spans: Vec<Span> = Vec::new();
    for (i, tab) in app.function.tabs.iter().enumerate() {
        if i > 0 {
            title_spans.push(Span::raw(" | "));
        }
        let name = match tab {
            SidebarTab::Notifications => "notifications",
            SidebarTab::Completion(_) => "completion",
            SidebarTab::Settings(_) => "settings",
            SidebarTab::ModelPicker(_) => "model picker",
            SidebarTab::ProviderPicker(_) => "provider",
            SidebarTab::ThinkingPicker(_) => "thinking",
            SidebarTab::TimelinePicker(_) => "timeline",
            SidebarTab::SessionPicker(_) => "sessions",
            SidebarTab::SessionRename(_) => "rename",
            SidebarTab::Plan(_) => "plan",
            SidebarTab::Ask(_) => "ask",
            SidebarTab::Todo(_) => "todo",
            SidebarTab::PastePreview(_) => "paste",
            SidebarTab::ToolPicker(_) => "tools",
            SidebarTab::Hotkey => "hotkey",
        };
        if i == app.function.active {
            title_spans.push(Span::styled(format!(" {name} "), Theme::bold()));
        } else {
            title_spans.push(Span::styled(format!(" {name} "), Theme::dim()));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(app.config.border_type.ratatui_set())
        .border_style(match app.focus_target {
            crate::function::FocusTarget::FunctionPanel => Theme::focused_border(),
            crate::function::FocusTarget::Input => Theme::unfocused_border(),
            crate::function::FocusTarget::AgentsCheckbox => Theme::unfocused_border(),
        })
        .title(Line::from(title_spans));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height < 2 {
        return;
    }

    let todo_items: Vec<crate::session::TodoItem> = app.session.todo_items.clone();
    if let Some(tab) = app.function.tabs.get_mut(app.function.active) {
        let ctx = crate::ui::tab_widget::TabCtx {
            config: &app.config,
            todos: &todo_items,
            disabled_tools: &app.disabled_tools,
            agent: app.active_agent,
        };
        let cursor = match tab {
            SidebarTab::Notifications => {
                render_notifications(inner, buf, app);
                None
            }
            SidebarTab::Completion(s) => s.render_tab(inner, buf, &ctx),
            SidebarTab::Settings(s) => s.render_tab(inner, buf, &ctx),
            SidebarTab::ModelPicker(s) => s.render_tab(inner, buf, &ctx),
            SidebarTab::ProviderPicker(s) => s.render_tab(inner, buf, &ctx),
            SidebarTab::ThinkingPicker(s) => s.render_tab(inner, buf, &ctx),
            SidebarTab::TimelinePicker(s) => s.render_tab(inner, buf, &ctx),
            SidebarTab::SessionPicker(s) => s.render_tab(inner, buf, &ctx),
            SidebarTab::SessionRename(s) => s.render_tab(inner, buf, &ctx),
            SidebarTab::Plan(s) => s.render_tab(inner, buf, &ctx),
            SidebarTab::Ask(s) => s.render_tab(inner, buf, &ctx),
            SidebarTab::PastePreview(s) => s.render_tab(inner, buf, &ctx),
            SidebarTab::Todo(s) => s.render_tab(inner, buf, &ctx),
            SidebarTab::ToolPicker(s) => s.render_tab(inner, buf, &ctx),
            SidebarTab::Hotkey => {
                use crate::ui::trait_impls::HotkeyTab;
                let mut hk = HotkeyTab;
                hk.render_tab(inner, buf, &ctx)
            }
        };
        app.function_panel_cursor = cursor;
    }
}

fn render_notifications(area: Rect, buf: &mut Buffer, app: &mut App) {
    if area.height < 3 {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    crate::ui::picker_widget::render_search_row(
        rows[0],
        buf,
        &app.notifications.query,
        crate::function::PickerFocus::List,
        app.notifications.searching,
    );

    let filtered = app.notifications.filtered_indices();
    let list_area = rows[1];
    if filtered.is_empty() {
        let msg = if app.notifications.items.is_empty() {
            "  [no notifications]"
        } else {
            "  [no matches]"
        };
        Paragraph::new(Line::from(Span::styled(msg, Theme::dim())))
            .wrap(Wrap { trim: false })
            .render(list_area, buf);
    } else {
        let viewport_h = list_area.height as usize;
        let width = list_area.width.saturating_sub(2).max(8) as usize;
        let cursor = app.notifications.cursor.min(filtered.len() - 1);

        // Compute display heights for all filtered items, and the
        // display-line prefix sums so we can reason about which items
        // are visible given a display-line scroll offset.
        let mut heights: Vec<u16> = Vec::with_capacity(filtered.len());
        for idx in &filtered {
            let t = &app.notifications.items[*idx];
            let prefix = "> ";
            let head = format!("{}[{}] {}  ", prefix, t.level.tag(), t.format_time());
            let text_width = width.saturating_sub(display_width(&head)).max(8);
            let wrapped = wrap_plain_text(&t.text, text_width);
            heights.push(wrapped.len().max(1) as u16);
        }
        // Prefix sums: item i starts at display line tops[i].
        let mut tops: Vec<usize> = Vec::with_capacity(filtered.len());
        let mut acc = 0usize;
        for &h in &heights {
            tops.push(acc);
            acc += h as usize;
        }
        let total_display_lines = acc;

        // Adjust scroll (display-line offset) so the cursor item is visible.
        let cursor_top = tops[cursor];
        let cursor_bot = cursor_top + heights[cursor] as usize;
        let max_scroll = total_display_lines.saturating_sub(viewport_h);
        let scroll = &mut app.notifications.scroll;
        if *scroll > max_scroll {
            *scroll = max_scroll;
        }
        if cursor_top < *scroll {
            *scroll = cursor_top;
        } else if cursor_bot > *scroll + viewport_h {
            *scroll = cursor_bot.saturating_sub(viewport_h);
        }
        let scroll = (*scroll).min(max_scroll);

        // Render visible items: those whose display lines intersect
        // [scroll, scroll + viewport_h).
        let mut visible = Vec::new();
        for (row, idx) in filtered.iter().enumerate() {
            let item_top = tops[row];
            let item_bot = item_top + heights[row] as usize;
            if item_bot <= scroll {
                continue;
            }
            if item_top >= scroll + viewport_h {
                break;
            }
            let t = &app.notifications.items[*idx];
            let selected = row == cursor;
            let level_style = match t.level {
                crate::function::notifications::ToastLevel::Ok => Theme::status_ok(),
                crate::function::notifications::ToastLevel::Info => Theme::status_info(),
                crate::function::notifications::ToastLevel::Warn => Theme::status_warn(),
                crate::function::notifications::ToastLevel::Fail => Theme::status_fail(),
            };
            let prefix = if selected { "> " } else { "  " };
            let head = format!("{}[{}] {}  ", prefix, t.level.tag(), t.format_time());
            let text_width = width.saturating_sub(display_width(&head)).max(8);
            let wrapped = wrap_plain_text(&t.text, text_width);
            let mut lines = Vec::new();
            let first = wrapped.first().cloned().unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(
                    prefix.to_string(),
                    if selected {
                        Theme::bold()
                    } else {
                        Theme::base()
                    },
                ),
                Span::styled(format!("[{}]", t.level.tag()), level_style),
                Span::raw(" "),
                Span::styled(t.format_time(), Theme::dim()),
                Span::raw("  "),
                Span::raw(first),
            ]));
            for cont in wrapped.into_iter().skip(1) {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(display_width(&head))),
                    Span::raw(cont),
                ]));
            }
            visible.push(ListItem::new(lines));
        }
        List::new(visible).render(list_area, buf);
    }

    let hint = if app.notifications.searching {
        Line::from(Span::styled(
            " Up/Down: nav | type: filter | Backspace: edit | Esc: close ",
            Theme::dim(),
        ))
    } else {
        Line::from(Span::styled(
            " Up/Down: nav | Alt+i: search | Esc: close ",
            Theme::dim(),
        ))
    };
    Paragraph::new(hint).render(rows[2], buf);
}

fn display_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

pub fn wrap_plain_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut line = String::new();
        let mut used = 0usize;
        for ch in raw.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used > 0 && used + w > width {
                out.push(line);
                line = String::new();
                used = 0;
            }
            line.push(ch);
            used += w;
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

pub fn render_new_provider_picker(
    area: Rect,
    buf: &mut Buffer,
    s: &mut crate::function::NewProviderPickerState,
) -> Option<(u16, u16)> {
    if area.height < 2 {
        return None;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);
    let search_cursor =
        crate::ui::picker_widget::render_search_row(rows[0], buf, &s.query, s.focus, false);
    let list_area = rows[1];
    if s.filtered.is_empty() {
        Paragraph::new(Line::from(Span::styled("  [no matches]", Theme::dim())))
            .wrap(Wrap { trim: false })
            .render(list_area, buf);
    } else {
        let range = visible_window(
            s.cursor,
            &mut s.scroll,
            list_area.height as usize,
            s.filtered.len(),
        );
        for row in range {
            let idx = s.filtered[row];
            let id = &s.entries[idx];
            let is_cursor = row == s.cursor;
            let y = list_area.y + (row - s.scroll) as u16;
            let picker_label = s.picker_label(id);
            let line = if is_cursor {
                Line::from(vec![
                    Span::styled("> ", Theme::bold()),
                    Span::raw(picker_label),
                ])
            } else {
                Line::from(Span::raw(format!("  {picker_label}")))
            };
            buf.set_line(list_area.x, y, &line, list_area.width);
        }
    }
    search_cursor
}

pub fn list_item(focused: bool, label: &str, value: Option<String>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if focused {
        spans.push(Span::styled("> ", Theme::bold()));
        spans.push(Span::raw(label.to_string()));
    } else {
        spans.push(Span::raw("  "));
        spans.push(Span::raw(label.to_string()));
    }
    if let Some(v) = value {
        spans.push(Span::raw(":  "));
        if v.is_empty() {
            if focused {
                spans.push(Span::styled("<empty>", Theme::dim()));
                // Terminal cursor is positioned here, no drawn cursor needed
            } else {
                spans.push(Span::styled("<empty>".to_string(), Theme::dim()));
            }
        } else if focused {
            spans.push(Span::raw(v));
            // Terminal cursor is positioned at end of value
        } else {
            spans.push(Span::raw(v));
        }
    }
    Line::from(spans)
}

/// Returns (display_lines, row_starts) where each logical row maps
/// to a display-line index via `row_starts`.  The text is wrapped
/// to fit within `width`.  Each row's CONTENT (without prefix) is
/// wrapped at `width - prefix_width`, then the styled prefix/indent
/// is prepended so every rendered line stays within `width`.
///
/// Logical rows: 0 = question, 1..N = options, N = freeform.
pub fn ask_active_question_lines(
    s: &crate::function::AskState,
    active_idx: usize,
    width: usize,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let Some(it) = s.items.get(active_idx) else {
        return (
            vec![Line::from(Span::styled("(no question)", Theme::dim()))],
            vec![0],
        );
    };
    let mut lines: Vec<Line> = Vec::new();
    let mut row_starts: Vec<usize> = Vec::new();
    let w = width.max(8);

    // Question row (logical row 0) — no prefix.
    // Continuation lines get a 3-space visual indent, so wrap at w-3.
    row_starts.push(lines.len());
    let qw = w.saturating_sub(3).max(1);
    let q_wrapped = wrap_plain_text(&it.question, qw);
    for (i, wr) in q_wrapped.into_iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(Span::raw(wr)));
        } else {
            lines.push(Line::from(Span::raw(format!("   {wr}"))));
        }
    }

    // Option rows
    for (j, opt) in it.options.iter().enumerate() {
        row_starts.push(lines.len());
        let selected = j == it.cursor;
        let prefix = if selected { ">  - " } else { "   - " };
        let prefix_w = display_width(prefix);
        let indent = " ".repeat(prefix_w);
        let content_w = w.saturating_sub(prefix_w).max(1);
        let wrapped = wrap_plain_text(opt, content_w);
        for (i, wr) in wrapped.into_iter().enumerate() {
            if i == 0 {
                if selected {
                    lines.push(Line::from(vec![
                        Span::styled(">  - ", Theme::bold()),
                        Span::styled(wr, Theme::bold()),
                    ]));
                } else {
                    lines.push(Line::from(Span::raw(format!("{prefix}{wr}"))));
                }
            } else if selected {
                lines.push(Line::from(vec![
                    Span::styled(indent.clone(), Theme::bold()),
                    Span::styled(wr, Theme::bold()),
                ]));
            } else {
                lines.push(Line::from(Span::raw(format!("{indent}{wr}"))));
            }
        }
    }

    // Freeform / custom input row
    let freeform_idx = it.options.len();
    row_starts.push(lines.len());
    let selected = freeform_idx == it.cursor;
    let prefix = if selected { ">  - " } else { "   - " };
    let prefix_w = display_width(prefix);
    let indent = " ".repeat(prefix_w);
    let label = if it.custom_input.is_empty() {
        "Type your own answer…".to_string()
    } else {
        format!("Custom: [{}]", it.custom_input)
    };
    let content_w = w.saturating_sub(prefix_w).max(1);
    let wrapped = wrap_plain_text(&label, content_w);
    let label_style = if it.custom_input.is_empty() {
        Theme::dim()
    } else {
        Theme::bold()
    };
    for (i, wr) in wrapped.into_iter().enumerate() {
        if i == 0 {
            if selected {
                lines.push(Line::from(vec![
                    Span::styled(">  - ", Theme::bold()),
                    Span::styled(wr, label_style),
                ]));
            } else {
                lines.push(Line::from(Span::styled(
                    format!("{prefix}{wr}"),
                    Theme::dim(),
                )));
            }
        } else if selected {
            lines.push(Line::from(vec![
                Span::styled(indent.clone(), Theme::bold()),
                Span::styled(wr, label_style),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                format!("{indent}{wr}"),
                Theme::dim(),
            )));
        }
    }

    (lines, row_starts)
}

/// Returns (display_lines, row_starts) where each item is one
/// logical row wrapping to fit `width`.  Each row's CONTENT is
/// wrapped at `width - prefix_width`, then the styled prefix/indent
/// is prepended so every rendered line stays within `width`.
pub fn ask_review_lines(
    s: &crate::function::AskState,
    width: usize,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut lines: Vec<Line> = Vec::new();
    let mut row_starts: Vec<usize> = Vec::new();
    let w = width.max(8);

    for (i, it) in s.items.iter().enumerate() {
        row_starts.push(lines.len());
        let ans = it.answered.as_deref().unwrap_or("(no answer)");
        let body = format!("Q{}. {}  →  {ans}", i + 1, it.question);
        let is_active = i == s.active;
        let prefix = if is_active { ">  " } else { "   " };
        let prefix_w = display_width(prefix);
        let content_w = w.saturating_sub(prefix_w).max(1);
        let wrapped = wrap_plain_text(&body, content_w);
        let indent = " ".repeat(prefix_w);
        for (j, wr) in wrapped.into_iter().enumerate() {
            if j == 0 {
                if is_active {
                    lines.push(Line::from(vec![
                        Span::styled(">  ", Theme::bold()),
                        Span::styled(wr, Theme::bold()),
                    ]));
                } else {
                    lines.push(Line::from(Span::raw(format!("{prefix}{wr}"))));
                }
            } else if is_active {
                lines.push(Line::from(vec![
                    Span::styled(indent.clone(), Theme::bold()),
                    Span::styled(wr, Theme::bold()),
                ]));
            } else {
                lines.push(Line::from(Span::raw(format!("{indent}{wr}"))));
            }
        }
    }

    (lines, row_starts)
}
