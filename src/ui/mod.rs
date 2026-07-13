use crate::app::App;
use crate::function::{CancelState, Selection};
use crate::session::Session;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

pub mod border_type;
pub mod function_panel;
pub mod picker_widget;
pub mod tab_widget;
pub mod trait_impls;

/// Height of the standalone cwd line that sits below the input block.
const CWD_HEIGHT: u16 = 1;
const AGENTS_AREA_HEIGHT: u16 = 5;

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let agents_height = if app.agents_visible { AGENTS_AREA_HEIGHT } else { 0 };
    let input_height = input_height(app, area.height, area.width);

    let mut constraints = vec![];
    if app.agents_visible {
        constraints.push(Constraint::Length(agents_height));
    }
    constraints.push(Constraint::Min(0));

    if app.function_visible {
        let remaining = area.height.saturating_sub(input_height + CWD_HEIGHT + agents_height);
        let pct_height = (remaining as f64 * 0.20) as u16;
        let panel_height = app.function.tabs.get(app.function.active)
            .map_or(4, |t| t.panel_height(pct_height, app));
        constraints.push(Constraint::Length(panel_height));
    }

    constraints.push(Constraint::Length(input_height));
    constraints.push(Constraint::Length(CWD_HEIGHT));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    app.session.display = app.config.thinking_display;
    app.session.tool_display = app.config.tool_display;
    app.session.tool_preview_lines = app.config.tool_preview_lines;

    let agents_idx = 0;
    let session_idx = if app.agents_visible { 1 } else { 0 };
    let panel_idx = session_idx + 1;
    let input_idx = if app.function_visible { panel_idx + 1 } else { session_idx + 1 };
    let cwd_idx = input_idx + 1;

    if app.agents_visible {
        render_agents_area(chunks[agents_idx], f.buffer_mut(), app);
    }
    let session_frame_area = chunks[session_idx];
    let content_area = session_content_area(session_frame_area);
    app.session_area = Some(content_area);

    let width_u16 = content_area.width;
    app.session.count_all_lines_with_width(width_u16 as usize);

    crate::session::render::render(
        content_area,
        f.buffer_mut(),
        &app.session,
        &mut app.tool_toggle_rows,
    );
    if app.function_visible {
        function_panel::render(chunks[panel_idx], f.buffer_mut(), app);
    }
    crate::input::render(chunks[input_idx], f.buffer_mut(), app);
    render_cwd(chunks[cwd_idx], f.buffer_mut(), app);

    app.thinking_toggle_rows.clear();
    app.tool_toggle_rows.clear();
    let inner_h = content_area.height as usize;

    let total_lines: usize = app
        .session
        .line_offsets
        .last()
        .copied()
        .unwrap_or(0) as usize;

    app.session.pin_scroll_for_total(width_u16, total_lines as u32);

    let scroll = app
        .session
        .scroll
        .min(total_lines.saturating_sub(inner_h).min(u32::MAX as usize) as u32);
    render_session_scrollbar(
        session_frame_area,
        f.buffer_mut(),
        total_lines,
        inner_h,
        scroll as usize,
    );
    let start = total_lines.saturating_sub(inner_h + scroll as usize);
    let end = start + inner_h;

    let first_visible = if app.session.messages.is_empty() || app.session.line_offsets.len() <= 1 {
        0
    } else {
        match app.session.line_offsets[..app.session.messages.len()]
            .binary_search(&(start as u32))
        {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    };

    let mut line_idx: usize = app
        .session
        .line_offsets
        .get(first_visible)
        .copied()
        .unwrap_or(0) as usize;

    for (msg_idx, m) in app
        .session
        .messages
        .iter()
        .enumerate()
        .skip(first_visible)
    {
        let msg_start = app.session.line_offsets[msg_idx] as usize;
        if msg_start >= end {
            break;
        }

        let think_show = m.role == crate::session::Role::Assistant
            && crate::session::render::message_has_thinking(m)
            && app.config.thinking_display != crate::config::ThinkingDisplay::Hide;
        let mut thinking_blocks: usize = 0;
        if think_show {
            let expanded = (app.config.thinking_display == crate::config::ThinkingDisplay::Show
                && m.thinking_visible)
                || (app.config.thinking_display
                    == crate::config::ThinkingDisplay::ShowWhileStreaming
                    && (m.streaming || m.thinking_visible));
            for seg in &m.thinking_segments {
                if line_idx >= start && line_idx < end {
                    let screen_y = content_area.y + (line_idx - start) as u16;
                    app.thinking_toggle_rows.push((screen_y, msg_idx));
                }
                let lines = if expanded {
                    seg.cached_line_count_expanded.unwrap_or(0) as usize
                } else {
                    seg.cached_line_count_collapsed.unwrap_or(0) as usize
                };
                line_idx += lines;
                line_idx += 1;
                thinking_blocks += 1;
            }
        }

        let content_lines =
            crate::session::render::read_cached_content_count_at(m, width_u16) as usize;
        line_idx += content_lines;

        let mut tool_blocks: usize = 0;
        if app.config.tool_display != crate::config::ToolResultDisplay::Hide {
            for (tool_idx, t) in m.tool_results.iter().enumerate() {
                let t_vis = t.name == "plan"
                    || match app.config.tool_display {
                        crate::config::ToolResultDisplay::Show => t.visible,
                        crate::config::ToolResultDisplay::ShowWhileStreaming => {
                            m.streaming || t.visible
                        }
                        _ => false,
                    };
                let lines = if t_vis {
                    t.cached_line_count_visible.unwrap_or(0) as usize
                } else {
                    t.cached_line_count_collapsed.unwrap_or(0) as usize
                };
                if lines > 0 && line_idx >= start && line_idx < end && t.name != "plan" {
                    let screen_y = content_area.y + (line_idx - start) as u16;
                    app.tool_toggle_rows.push((screen_y, msg_idx, tool_idx));
                }
                line_idx += lines;
                line_idx += 1;
                tool_blocks += 1;
            }
        }

        let first_offset = m.thinking_segments.iter().map(|s| s.offset)
            .chain(m.tool_results.iter().map(|t| t.content_offset))
            .min();
        if first_offset.is_some_and(|off| off > 0) && (thinking_blocks > 0 || tool_blocks > 0) {
            line_idx += 1;
        }

        if m.role == crate::session::Role::User {
            if let Some(skill_ref) = &m.skill_ref {
                line_idx += crate::session::render::skill_block_line_count(
                    skill_ref,
                    width_u16 as usize,
                ) as usize;
            }
            line_idx += 2;
        }

        line_idx += 1;
    }

    if let Some(sel) = app.tui_selection {
        let buf = f.buffer_mut();
        let total = app.session.line_offsets.last().copied().unwrap_or(0);
        let scroll = app.session.scroll;
        if let Some(area) = app.session_area {
            apply_selection_style(buf, &sel, &area, scroll, total);
        }
        let width = app.session_area.map(|a| a.width as usize).unwrap_or(80);
        app.selected_text = Some(extract_selection_text(&sel, &app.session, width));
    } else {
        app.selected_text = None;
    }

    if let Some((cx, cy)) = app.function_panel_cursor.or(app.input_cursor_screen) {
        f.set_cursor_position((cx, cy));
    }
}

fn input_height(app: &App, viewport_height: u16, terminal_width: u16) -> u16 {
    // Count visual lines accounting for wrapping: each \n segment wraps
    // when prompt (2) + text exceeds inner width (terminal_width - 2 borders).
    let inner_w = terminal_width.saturating_sub(2).max(1) as usize;
    let prompt_w = 3usize;
    let mut visual_lines = 0u16;
    for seg in app.input.buffer.split('\n') {
        let tw = unicode_width::UnicodeWidthStr::width(seg);
        let total = prompt_w + tw;
        let seg_lines = if total <= inner_w {
            1
        } else {
            total.div_ceil(inner_w)
        };
        visual_lines = visual_lines.saturating_add(seg_lines as u16);
    }
    visual_lines = visual_lines.max(1);
    // Cap how tall the input can grow so the session always keeps at
    // least ~50% of the viewport.
    let min_for_session = ((viewport_height as f32) * 0.5).floor() as u16;
    let max_body = viewport_height
        .saturating_sub(min_for_session)
        .saturating_sub(2)
        .max(1);
    visual_lines.min(max_body) + 2
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
    // Scrollbar uses the full session area height. The thumb lands at
    // the bottom when `scroll == 0`, overwriting the last message's
    // bottom gap (a blank line) with `█` — that's a no-op visually
    // because the gap was empty anyway.
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
/// When a request is in flight, the cancel/interrupt hint is shown
/// on the left, separated by ` | ` from the path.
fn render_cwd(area: Rect, buf: &mut Buffer, app: &App) {
    use crate::theme::Theme;
    if area.height == 0 || area.width == 0 {
        return;
    }
    let avail = area.width as usize;
    let path = &app.status.cwd;

    // Compute the right-aligned stats line and its display width.
    let stats_line = app.status.render_stats_line();
    let stats_width = stats_line.width();
    let stats_pad = if stats_width > 0 && avail > stats_width { 1 } else { 0 };

    // Split area: left for cwd, right for stats.
    let left_w = avail.saturating_sub(stats_width + stats_pad);
    let right_w = stats_width;

    // --- Left: cwd / interrupt hint ---
    let left_area = Rect {
        x: area.x,
        y: area.y,
        width: left_w as u16,
        height: 1,
    };

    if app.inflight.is_some() {
        let elapsed = app
            .inflight
            .as_ref()
            .map(|h| h.started_at.elapsed())
            .unwrap_or(std::time::Duration::ZERO);
        let secs = elapsed.as_secs();
        let timer = if secs >= 3600 {
            format!("{}h{}m{}s", secs / 3600, (secs % 3600) / 60, secs % 60)
        } else if secs >= 60 {
            format!("{}m{}s", secs / 60, secs % 60)
        } else {
            format!("{}s", secs)
        };
        let hint = match app.cancel_state {
            CancelState::Idle => {
                format!("{} esc to interrupt [{timer}]", crate::input::spinner_prompt().trim())
            }
            CancelState::Confirming(_) => {
                format!("{} esc again [{timer}]", crate::input::spinner_prompt().trim())
            }
        };
        let hint_w = UnicodeWidthStr::width(hint.as_str());
        let sep = " | ";
        let prefix = "~ ";
        let fixed_w = hint_w + sep.len() + prefix.len();
        let path_max = left_w.saturating_sub(fixed_w);
        let truncated = truncate_path(path, path_max);
        let line = Line::from(vec![
            Span::styled(hint, Theme::dim()),
            Span::styled(sep, Theme::dim()),
            Span::styled(prefix, Theme::dim()),
            Span::styled(truncated, Theme::dim()),
        ]);
        let p = ratatui::widgets::Paragraph::new(line);
        p.render(left_area, buf);
    } else {
        let prefix = "~ ";
        let path_max = left_w.saturating_sub(prefix.len());
        let truncated = truncate_path(path, path_max);
        let line = Line::from(vec![
            Span::styled(prefix, Theme::dim()),
            Span::styled(truncated, Theme::dim()),
        ]);
        let p = ratatui::widgets::Paragraph::new(line);
        p.render(left_area, buf);
    }

    // --- Right: stats ---
    if right_w > 0 {
        let right_area = Rect {
            x: area.x + left_w as u16 + stats_pad as u16,
            y: area.y,
            width: right_w as u16,
            height: 1,
        };
        let p = ratatui::widgets::Paragraph::new(stats_line);
        p.render(right_area, buf);
    }
}

/// Truncate a path to fit within `max_width` columns.
/// Progressive shortening: full → `D:\...\dirname` → dirname → `xx...xxx`.
fn truncate_path(path: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(path) <= max_width {
        return path.to_string();
    }
    let sep = if path.contains('\\') { '\\' } else { '/' };
    let components: Vec<&str> = path.split(sep).collect();
    let dir_name = components.last().copied().unwrap_or(path);

    if components.len() >= 3 {
        let first = components[0];
        let abbreviated = format!("{first}{sep}...{sep}{dir_name}");
        if UnicodeWidthStr::width(abbreviated.as_str()) <= max_width {
            return abbreviated;
        }
    }

    if UnicodeWidthStr::width(dir_name) <= max_width {
        return dir_name.to_string();
    }

    let dot_count = 3;
    let half = max_width.saturating_sub(dot_count) / 2;
    if half == 0 {
        return dir_name.chars().take(max_width).collect();
    }
    let prefix: String = dir_name.chars().take(half).collect();
    let suffix: String = dir_name
        .chars()
        .rev()
        .take(half)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}...{suffix}")
}

/// Convert a screen Y (within the session area) to a global document line index.
pub(crate) fn screen_y_to_doc_line(y: u16, area: &Rect, scroll: u32, total: u32) -> usize {
    let inner_h = area.height as u32;
    let max_scroll = total.saturating_sub(inner_h);
    let offset_from_top = max_scroll.saturating_sub(scroll);
    (offset_from_top + (y - area.top()) as u32) as usize
}

/// Convert a global document line index to a screen Y, if visible.
pub(crate) fn doc_line_to_screen_y(line: usize, area: &Rect, scroll: u32, total: u32) -> Option<u16> {
    let inner_h = area.height as u32;
    let max_scroll = total.saturating_sub(inner_h);
    let offset_from_top = max_scroll.saturating_sub(scroll);
    if (line as u32) < offset_from_top || (line as u32) >= offset_from_top + inner_h {
        return None;
    }
    Some(area.top() + ((line as u32) - offset_from_top) as u16)
}

/// Apply a REVERSED style to every cell inside the selection rectangle so
/// the user can see what they have highlighted.
fn apply_selection_style(buf: &mut Buffer, sel: &Selection, area: &Rect, scroll: u32, total: u32) {
    let y_start = sel.doc_start.min(sel.doc_end);
    let y_end = sel.doc_start.max(sel.doc_end);
    // Determine column range. When the user drags upward (doc_end <
    // doc_start), the visual start column belongs to the bottom-most
    // original line, so normalize accordingly.
    let (col_lo, col_hi) = if sel.doc_start <= sel.doc_end {
        (sel.col_start, sel.col_end)
    } else {
        (sel.col_end, sel.col_start)
    };
    // Columns are relative to the session area; convert to absolute x.
    let x_lo = col_lo.map(|c| area.x + c.min(area.width.saturating_sub(1)));
    let x_hi = col_hi.map(|c| area.x + c.min(area.width.saturating_sub(1)));
    let width = buf.area().width;
    let buf_x_start = x_lo.unwrap_or(0);
    let buf_x_end = x_hi.unwrap_or(width.saturating_sub(1));
    for doc_line in y_start..=y_end {
        if let Some(screen_y) = doc_line_to_screen_y(doc_line, area, scroll, total) {
            // First and last rows use the column clamp; middle rows
            // span the full width.
            let (row_x_start, row_x_end) = if y_start == y_end {
                (buf_x_start, buf_x_end)
            } else if doc_line == y_start {
                (buf_x_start, width.saturating_sub(1))
            } else if doc_line == y_end {
                (0, buf_x_end)
            } else {
                (0, width.saturating_sub(1))
            };
            for x in row_x_start..=row_x_end {
                if let Some(cell) = buf.cell_mut((x, screen_y)) {
                    let new_style = cell.style().add_modifier(Modifier::REVERSED);
                    cell.set_style(new_style);
                }
            }
        }
    }
}

/// Read the rendered symbols from message lines in the selection range and
/// return them as plain text. Trailing whitespace on each row is trimmed
/// and empty trailing rows are dropped, so a single-row selection across a
/// padded cell line does not produce a wall of spaces.
pub fn extract_selection_text(
    sel: &Selection,
    session: &Session,
    width: usize,
) -> String {
    let y_start = sel.doc_start.min(sel.doc_end);
    let y_end = sel.doc_start.max(sel.doc_end);
    let (col_lo, col_hi) = if sel.doc_start <= sel.doc_end {
        (sel.col_start, sel.col_end)
    } else {
        (sel.col_end, sel.col_start)
    };
    let col_lo = col_lo.unwrap_or(0) as usize;
    let col_hi = col_hi.map(|c| c as usize).unwrap_or(width);
    let mut lines: Vec<String> = Vec::new();

    let offsets = &session.line_offsets;
    if offsets.len() < 2 {
        return String::new();
    }

    let first_msg = match offsets[..offsets.len() - 1].binary_search(&(y_start as u32)) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };

    for msg_idx in first_msg..session.messages.len() {
        let msg_start = offsets[msg_idx] as usize;
        if msg_start > y_end {
            break;
        }
        let msg_end = if msg_idx + 1 < offsets.len() {
            offsets[msg_idx + 1] as usize
        } else {
            y_end + 1
        };
        let local_start = y_start.saturating_sub(msg_start);
        let local_end = y_end.min(msg_end.saturating_sub(1)).saturating_sub(msg_start);

        let rendered = crate::session::render::build_message_lines(session, msg_idx, width);
        for (i, line) in rendered.iter().enumerate() {
            if i < local_start || i > local_end {
                continue;
            }
            let full: String = line.spans.iter()
                .map(|s| s.content.as_ref())
                .collect();
            // Determine column slice for this row.
            let (cs, ce) = if y_start == y_end {
                (col_lo, col_hi)
            } else if i == local_start {
                (col_lo, full.chars().count())
            } else if i == local_end {
                (0, col_hi)
            } else {
                (0, full.chars().count())
            };
            let sliced = slice_by_visual_width(&full, cs, ce);
            lines.push(sliced.trim_end().to_string());
        }
    }

    while lines.len() > 1 && lines.last().unwrap().is_empty() {
        lines.pop();
    }
    compact_render_spacing(&lines.join("\n"))
}

/// Slice a string by visual (terminal cell) column range [start, end),
/// respecting wide (CJK) characters that occupy 2 cells.
fn slice_by_visual_width(s: &str, start_col: usize, end_col: usize) -> String {
    let start_col = start_col.min(end_col);
    let mut out = String::with_capacity(s.len());
    let mut col = 0usize;
    let mut started = false;
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if !started && col + w > start_col {
            started = true;
        }
        if started {
            if col >= end_col {
                break;
            }
            out.push(ch);
        }
        col += w;
    }
    out
}

pub(crate) fn compact_render_spacing(text: &str) -> String {
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


/// Render the agents.md splash area at the top of a new session.
/// Left side: logo, right side: checkboxes for discovered agents.md files.
pub fn render_agents_area(area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer, app: &mut crate::app::App) {
    use crate::theme::Theme;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    if area.height < 5 || area.width < 20 {
        return;
    }
    let logo_lines = [
        "\u{2590}\u{2588}\u{259B}\u{2588}\u{259B}\u{2588}\u{258C}",
        "\u{2590}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{258C}",
    ];
    let logo_width = 7u16;
    let right_x = area.x + logo_width + 2;
    let right_w = area.width.saturating_sub(logo_width + 2);

    // Render logo
    for (i, line) in logo_lines.iter().enumerate() {
        let y = area.y + 1 + i as u16;
        let logo_line = Line::from(Span::styled(*line, Theme::bold()));
        let p = Paragraph::new(logo_line);
        p.render(
            ratatui::layout::Rect { x: area.x + 1, y, width: logo_width, height: 1 },
            buf,
        );
    }

    // Render line separator
    let sep_y = area.y + 4;
    let sep_line = Line::from(Span::styled(
        "-".repeat(area.width.saturating_sub(1) as usize),
        Theme::dim(),
    ));
    let p_sep = Paragraph::new(sep_line);
    p_sep.render(
        ratatui::layout::Rect { x: area.x, y: sep_y, width: area.width.saturating_sub(1), height: 1 },
        buf,
    );

    // Render checkboxes
    let entries: Vec<(&String, &bool)> = app.config.agents.entries.iter().collect();
    if entries.is_empty() {
        let hint = Line::from(Span::styled("No agents.md found", Theme::dim()));
        let p = Paragraph::new(hint);
        p.render(
            ratatui::layout::Rect { x: right_x, y: area.y + 1, width: right_w, height: 1 },
            buf,
        );
        return;
    }

    for (i, (path, &enabled)) in entries.iter().enumerate() {
        let y = area.y + 1 + i as u16;
        if y >= area.y + 3 {
            break;
        }
        let marker = if enabled { "[x]" } else { "[ ]" };
        let cursor = if app.agents_cursor == i && app.focus_target == crate::function::FocusTarget::AgentsCheckbox {
            "> "
        } else {
            "  "
        };
        let short = path.rsplit('/').next().or_else(|| path.rsplit('\\').next()).unwrap_or(path);
        let label = format!("{cursor}{marker} {short}");
        let style = if app.focus_target == crate::function::FocusTarget::AgentsCheckbox && app.agents_cursor == i {
            Theme::bold()
        } else {
            Theme::dim()
        };
        let line = Line::from(Span::styled(label, style));
        let p = Paragraph::new(line);
        p.render(
            ratatui::layout::Rect { x: right_x, y, width: right_w, height: 1 },
            buf,
        );
    }
}


