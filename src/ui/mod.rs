use crate::app::App;
use crate::function::Selection;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui::Frame;

pub mod function_panel;
pub mod picker_widget;

/// Input body supports up to three prompt rows plus two border rows.
const MAX_INPUT_BODY_HEIGHT: u16 = 3;

/// Height of the standalone cwd line that sits below the input block.
const CWD_HEIGHT: u16 = 1;

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    // Layout: [session, (function panel)?, input, cwd]. The cwd is shown
    // as a separate line below the input — the user no longer wants the
    // project path on the input block's title.
    let input_height = input_height(app);
    let chunks = if app.function_visible {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Percentage(30),
                Constraint::Length(input_height),
                Constraint::Length(CWD_HEIGHT),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(input_height),
                Constraint::Length(CWD_HEIGHT),
            ])
            .split(area)
    };

    app.session.display = app.config.thinking_display;
    app.session_area = Some(chunks[0]);
    if app.function_visible {
        crate::session::render::render(chunks[0], f.buffer_mut(), &app.session);
        function_panel::render(chunks[1], f.buffer_mut(), app);
        crate::input::render(chunks[2], f.buffer_mut(), app);
        render_cwd(chunks[3], f.buffer_mut(), &app.status.cwd);
    } else {
        crate::session::render::render(chunks[0], f.buffer_mut(), &app.session);
        crate::input::render(chunks[1], f.buffer_mut(), app);
        render_cwd(chunks[2], f.buffer_mut(), &app.status.cwd);
    }

    // Re-derive which line of the scroll window each screen row maps to
    // and record the screen y of each thinking toggle for mouse hit-testing.
    app.thinking_toggle_rows.clear();
    let area = chunks[0];
    let inner_h = area.height as usize;
    let total_lines: usize = {
        let mut n = 0;
        for m in &app.session.messages {
            n += 1; // role prefix line
            let show = m.role == crate::session::Role::Assistant
                && !m.thinking.trim().is_empty()
                && app.config.thinking_display != crate::config::ThinkingDisplay::Hide;
            if show {
                n += 1; // toggle (after prefix, before content)
                let expanded = (app.config.thinking_display == crate::config::ThinkingDisplay::Show && m.thinking_visible)
                    || (app.config.thinking_display == crate::config::ThinkingDisplay::ShowWhileStreaming && (m.streaming || m.thinking_visible));
                if expanded {
                    n += m.thinking.split('\n').count() + 1;
                }
            }
            n += m.content.split('\n').count();     // content (0 if empty)
            n += 1; // spacer
        }
        if !app.session.messages.is_empty() {
            n += 1; // trailing gap line at the bottom
        }
        n
    };
    let scroll = app.session.scroll.min(total_lines.saturating_sub(inner_h) as u16);
    let start = total_lines.saturating_sub(inner_h + scroll as usize);
    let mut line_idx = start;
    for (msg_idx, m) in app.session.messages.iter().enumerate() {
        line_idx += 1; // role prefix line
        let show = m.role == crate::session::Role::Assistant
            && !m.thinking.trim().is_empty()
            && app.config.thinking_display != crate::config::ThinkingDisplay::Hide;
        if show {
            if line_idx >= start && line_idx < start + inner_h {
                let screen_y = area.y + (line_idx - start) as u16;
                app.thinking_toggle_rows.push((screen_y, msg_idx));
            }
            line_idx += 1; // toggle (after prefix)
            let expanded = (app.config.thinking_display == crate::config::ThinkingDisplay::Show && m.thinking_visible)
                || (app.config.thinking_display == crate::config::ThinkingDisplay::ShowWhileStreaming && (m.streaming || m.thinking_visible));
            if expanded {
                line_idx += m.thinking.split('\n').count() + 1;
            }
        }
        line_idx += m.content.split('\n').count();
        line_idx += 1; // spacer
    }

    // Post-render: highlight the mouse-driven TUI selection and refresh
    // the cached text that Ctrl+C will copy.
    if let Some(sel) = app.tui_selection {
        let buf = f.buffer_mut();
        apply_selection_style(buf, &sel);
        app.selected_text = Some(extract_selection_text(buf, &sel));
    } else {
        app.selected_text = None;
    }

    // Move the terminal cursor so IME composition windows appear at the
    // correct location. The function panel cursor (e.g. picker search input)
    // takes priority over the main input cursor.
    if let Some((cx, cy)) = app.function_panel_cursor.or(app.input_cursor_screen) {
        f.set_cursor_position((cx, cy));
    }
}

fn input_height(app: &App) -> u16 {
    let lines = app.input.buffer.split('\n').count().max(1) as u16;
    lines.min(MAX_INPUT_BODY_HEIGHT) + 2
}

/// Render the project cwd as a dim line below the input block.
fn render_cwd(area: Rect, buf: &mut Buffer, cwd: &str) {
    use crate::theme::Theme;
    if area.height == 0 || area.width == 0 {
        return;
    }
    let line = Line::from(vec![
        Span::styled("~ ", Theme::dim()),
        Span::styled(cwd.to_string(), Theme::dim()),
    ]);
    let p = ratatui::widgets::Paragraph::new(line);
    p.render(area, buf);
}

/// Apply a REVERSED style to every cell inside the selection rectangle so
/// the user can see what they have highlighted.
fn apply_selection_style(buf: &mut Buffer, sel: &Selection) {
    let ((sx, sy), (ex, ey)) = sel.rect();
    let width = buf.area().width;
    let x_end = ex.min(width.saturating_sub(1));
    for y in sy..=ey {
        for x in sx..=x_end {
            if let Some(cell) = buf.cell_mut((x, y)) {
                let new_style = cell.style().add_modifier(Modifier::REVERSED);
                cell.set_style(new_style);
            }
        }
    }
}

/// Read the rendered symbols from the buffer in the selection area and
/// return them as plain text. Trailing whitespace on each row is trimmed
/// and empty trailing rows are dropped, so a single-row selection across a
/// padded cell line does not produce a wall of spaces.
pub fn extract_selection_text(buf: &Buffer, sel: &Selection) -> String {
    let ((sx, sy), (ex, ey)) = sel.rect();
    let width = buf.area().width;
    let x_end = ex.min(width.saturating_sub(1));
    let mut lines: Vec<String> = Vec::new();
    for y in sy..=ey {
        let mut line = String::new();
        for x in sx..=x_end {
            if let Some(cell) = buf.cell((x, y)) {
                line.push_str(cell.symbol());
            }
        }
        lines.push(line.trim_end().to_string());
    }
    while lines.len() > 1 && lines.last().unwrap().is_empty() {
        lines.pop();
    }
    lines.join("\n")
}

/// Test-only re-export under a stable name. The implementation lives in
/// [`extract_selection_text`]; the alias keeps tests from depending on
/// visibility tweaks.
pub fn extract_selection_text_for_test(buf: &Buffer, sel: &Selection) -> String {
    extract_selection_text(buf, sel)
}
