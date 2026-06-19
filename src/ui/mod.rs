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

/// Height of the standalone cwd line that sits below the input block.
const CWD_HEIGHT: u16 = 1;

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    // Layout: [session, (function panel)?, input, cwd]. The cwd is shown
    // as a separate line below the input — the user no longer wants the
    // project path on the input block's title.
    let input_height = input_height(app, area.height);
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
    app.session.tool_display = app.config.tool_display;
    let session_frame_area = chunks[0];
    let area = session_content_area(session_frame_area);
    app.session_area = Some(session_frame_area);
    if app.function_visible {
        crate::session::render::render(
            area,
            f.buffer_mut(),
            &app.session,
            &mut app.tool_toggle_rows,
        );
        function_panel::render(chunks[1], f.buffer_mut(), app);
        crate::input::render(chunks[2], f.buffer_mut(), app);
        render_cwd(chunks[3], f.buffer_mut(), &app.status.cwd);
    } else {
        crate::session::render::render(
            area,
            f.buffer_mut(),
            &app.session,
            &mut app.tool_toggle_rows,
        );
        crate::input::render(chunks[1], f.buffer_mut(), app);
        render_cwd(chunks[2], f.buffer_mut(), &app.status.cwd);
    }

    // Re-derive which line of the scroll window each screen row maps to
    // and record the screen y of each thinking toggle for mouse hit-testing.
    app.thinking_toggle_rows.clear();
    let area = session_content_area(session_frame_area);
    let inner_h = area.height as usize;
    let total_lines: usize = {
        let mut n = 0;
        for m in &app.session.messages {
            n += 1; // role prefix line
            let think_show = m.role == crate::session::Role::Assistant
                && !m.thinking.trim().is_empty()
                && app.config.thinking_display != crate::config::ThinkingDisplay::Hide;
            if think_show {
                let expanded = (app.config.thinking_display
                    == crate::config::ThinkingDisplay::Show
                    && m.thinking_visible)
                    || (app.config.thinking_display
                        == crate::config::ThinkingDisplay::ShowWhileStreaming
                        && (m.streaming || m.thinking_visible));
                n += crate::session::render::thinking_block_line_count(
                    &m.thinking,
                    expanded,
                    area.width as usize,
                );
            }
            n += m.content.split('\n').count().max(1);
            if app.config.tool_display != crate::config::ToolResultDisplay::Hide {
                for t in &m.tool_results {
                    let t_vis = match app.config.tool_display {
                        crate::config::ToolResultDisplay::Show => t.visible,
                        crate::config::ToolResultDisplay::ShowWhileStreaming => {
                            m.streaming || t.visible
                        }
                        _ => false,
                    };
                    n += crate::session::render::tool_block_line_count(
                        t,
                        t_vis,
                        area.width as usize,
                    );
                }
            }
            n += 1; // spacer
        }
        if !app.session.messages.is_empty() {
            n += 1;
        }
        n
    };
    let scroll = app
        .session
        .scroll
        .min(total_lines.saturating_sub(inner_h) as u16);
    render_session_scrollbar(
        session_frame_area,
        f.buffer_mut(),
        total_lines,
        inner_h,
        scroll as usize,
    );
    let start = total_lines.saturating_sub(inner_h + scroll as usize);
    let mut line_idx = start;
    for (msg_idx, m) in app.session.messages.iter().enumerate() {
        line_idx += 1; // role prefix line
        let think_show = m.role == crate::session::Role::Assistant
            && !m.thinking.trim().is_empty()
            && app.config.thinking_display != crate::config::ThinkingDisplay::Hide;
        if think_show {
            if line_idx >= start && line_idx < start + inner_h {
                let screen_y = area.y + (line_idx - start) as u16;
                app.thinking_toggle_rows.push((screen_y, msg_idx));
            }
            let expanded = (app.config.thinking_display == crate::config::ThinkingDisplay::Show
                && m.thinking_visible)
                || (app.config.thinking_display
                    == crate::config::ThinkingDisplay::ShowWhileStreaming
                    && (m.streaming || m.thinking_visible));
            line_idx += crate::session::render::thinking_block_line_count(
                &m.thinking,
                expanded,
                area.width as usize,
            );
        }
        // Content (tool markers stripped by render.rs)
        let content_lines = m.content.split('\n').count().max(1);
        line_idx += content_lines;
        if app.config.tool_display != crate::config::ToolResultDisplay::Hide {
            for t in &m.tool_results {
                let t_vis = match app.config.tool_display {
                    crate::config::ToolResultDisplay::Show => t.visible,
                    crate::config::ToolResultDisplay::ShowWhileStreaming => {
                        m.streaming || t.visible
                    }
                    _ => false,
                };
                line_idx +=
                    crate::session::render::tool_block_line_count(t, t_vis, area.width as usize);
            }
        }
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

fn input_height(app: &App, viewport_height: u16) -> u16 {
    let lines = app.input.buffer.split('\n').count().max(1) as u16;
    let max_body = ((viewport_height as f32) * 0.40).floor() as u16;
    let max_body = max_body.max(1).saturating_sub(2).max(1);
    lines.min(max_body) + 2
}

fn session_content_area(area: Rect) -> Rect {
    Rect {
        x: area.x,
        y: area.y,
        width: area.width.saturating_sub(1),
        height: area.height,
    }
}

fn render_session_scrollbar(
    area: Rect,
    buf: &mut Buffer,
    total_lines: usize,
    viewport_lines: usize,
    scroll_from_bottom: usize,
) {
    if area.width == 0 || area.height == 0 || total_lines <= viewport_lines || viewport_lines == 0 {
        return;
    }

    let x = area.right().saturating_sub(1);
    let track_height = area.height as usize;
    if track_height == 0 {
        return;
    }

    let max_start = total_lines.saturating_sub(viewport_lines);
    let start = max_start.saturating_sub(scroll_from_bottom.min(max_start));
    let thumb_height = ((viewport_lines * track_height) / total_lines).clamp(1, track_height);
    let available = track_height.saturating_sub(thumb_height);
    let thumb_top = if max_start == 0 {
        0
    } else {
        (start * available + max_start / 2) / max_start
    };

    for row in 0..track_height {
        let y = area.y + row as u16;
        if let Some(cell) = buf.cell_mut((x, y)) {
            if row >= thumb_top && row < thumb_top + thumb_height {
                cell.set_symbol("█");
                cell.set_style(crate::theme::Theme::bold());
            } else {
                cell.set_symbol("│");
                cell.set_style(crate::theme::Theme::dim());
            }
        }
    }
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
    compact_render_spacing(&lines.join("\n"))
}

fn compact_render_spacing(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut idx = 0;
    while idx < chars.len() {
        if chars[idx] != ' ' {
            out.push(chars[idx]);
            idx += 1;
            continue;
        }

        let run_start = idx;
        while idx < chars.len() && chars[idx] == ' ' {
            idx += 1;
        }
        let run_len = idx - run_start;
        let prev = out.chars().last();
        let next = chars.get(idx).copied();
        if run_len == 1 && should_drop_render_space(prev, next, &chars, idx) {
            continue;
        }
        out.push_str(&" ".repeat(run_len));
    }
    out
}

fn should_drop_render_space(
    prev: Option<char>,
    next: Option<char>,
    chars: &[char],
    next_idx: usize,
) -> bool {
    let (Some(prev), Some(next)) = (prev, next) else {
        return false;
    };
    if prev == '\n' || next == '\n' {
        return false;
    }

    (is_cjk(prev) && is_cjk(next))
        || (is_cjk(prev) && is_cjk_punctuation(next))
        || (is_cjk_punctuation(prev) && is_cjk(next))
        || (prev.is_ascii_digit() && is_cjk(next))
        || (is_cjk(prev) && ascii_token_runs_into_cjk(chars, next_idx))
}

fn ascii_token_runs_into_cjk(chars: &[char], start: usize) -> bool {
    let mut idx = start;
    let mut len = 0usize;
    while let Some(ch) = chars.get(idx) {
        if !ch.is_ascii_alphanumeric() && *ch != '_' && *ch != '-' {
            break;
        }
        len += 1;
        idx += 1;
    }

    (1..=4).contains(&len) && chars.get(idx).copied().is_some_and(is_cjk)
}

fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2A6DF}'
            | '\u{2A700}'..='\u{2B73F}'
            | '\u{2B740}'..='\u{2B81F}'
            | '\u{2B820}'..='\u{2CEAF}'
    )
}

fn is_cjk_punctuation(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{303F}' | '\u{FF00}'..='\u{FFEF}')
}

/// Test-only re-export under a stable name. The implementation lives in
/// [`extract_selection_text`]; the alias keeps tests from depending on
/// visibility tweaks.
pub fn extract_selection_text_for_test(buf: &Buffer, sel: &Selection) -> String {
    extract_selection_text(buf, sel)
}
