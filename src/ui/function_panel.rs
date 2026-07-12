use std::ops::Range;

use crate::app::App;
use crate::function::SidebarTab;
use crate::theme::Theme;
use crate::ui::tab_widget::TabWidget;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap, Widget};

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
        let filtered_len = filtered.len();
        if filtered_len > 0 {
            ensure_cursor_visible(app.notifications.cursor, &mut app.notifications.scroll, list_area.height as usize);
        }
        let width = list_area.width.saturating_sub(2).max(8) as usize;
        let start = app.notifications.scroll.min(filtered_len);
        let mut visible = Vec::new();
        let mut row_count = 0u16;
        for (row, idx) in filtered.iter().enumerate().skip(start) {
            if row_count >= list_area.height {
                break;
            }
            let t = &app.notifications.items[*idx];
            let selected = row == app.notifications.cursor;
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
            let item_height = lines.len() as u16;
            if row_count + item_height > list_area.height && row_count > 0 {
                break;
            }
            row_count = row_count.saturating_add(item_height.max(1));
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

fn wrap_plain_text(text: &str, width: usize) -> Vec<String> {
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
    if area.height < 3 {
        return None;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let search_cursor =
        crate::ui::picker_widget::render_search_row(rows[0], buf, &s.query, s.focus, false);
    let list_area = rows[1];
    if s.filtered.is_empty() {
        Paragraph::new(Line::from(Span::styled("  [no matches]", Theme::dim())))
            .wrap(Wrap { trim: false })
            .render(list_area, buf);
    } else {
        let range = visible_window(s.cursor, &mut s.scroll, list_area.height as usize, s.filtered.len());
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
    Paragraph::new(Line::from(Span::styled(
        " Enter: select | type: filter | Esc: back ",
        Theme::dim(),
    )))
    .render(rows[2], buf);
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

/// Body lines for the Asking phase: the active question + its
/// options. Cursor is marked with `>` and rendered bold; the
/// implicit "Type your own answer…" row is appended as the last
/// row.
///
/// ```text
/// <question>
/// >  - <option>
///    - <option>
/// ```
pub fn ask_active_question_lines(
    s: &crate::function::AskState,
    active_idx: usize,
) -> Vec<Line<'static>> {
    let Some(it) = s.items.get(active_idx) else {
        return vec![Line::from(Span::styled("(no question)", Theme::dim()))];
    };
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::raw(it.question.clone())));
    for (j, opt) in it.options.iter().enumerate() {
        if j == it.cursor {
            lines.push(Line::from(vec![
                Span::styled(">  - ", Theme::bold()),
                Span::styled(opt.clone(), Theme::bold()),
            ]));
        } else {
            lines.push(Line::from(Span::raw(format!("   - {opt}"))));
        }
    }
    let freeform_idx = it.options.len();
    if freeform_idx == it.cursor {
        lines.push(Line::from(vec![
            Span::styled(">  - ", Theme::bold()),
            Span::styled("Type your own answer…", Theme::dim()),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "   - Type your own answer…",
            Theme::dim(),
        )));
    }
    lines
}

/// Body lines for the Reviewing phase: one Q/A pair per question.
///
/// ```text
/// Q1. <question>
///    A. <answer>
/// Q2. <question>
///    A. <answer>
/// ```
pub fn ask_review_lines(s: &crate::function::AskState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    for (i, it) in s.items.iter().enumerate() {
        let ans = it.answered.as_deref().unwrap_or("(no answer)");
        lines.push(Line::from(Span::styled(
            format!("Q{}. {}", i + 1, it.question),
            Theme::bold(),
        )));
        lines.push(Line::from(Span::raw(format!("   A. {ans}"))));
    }
    lines
}
