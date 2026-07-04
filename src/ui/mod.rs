use crate::app::App;
use crate::function::Selection;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui::Frame;

pub mod border_type;
pub mod function_panel;
pub mod picker_widget;

/// Height of the standalone cwd line that sits below the input block.
const CWD_HEIGHT: u16 = 1;

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    // Layout: [session, (function panel)?, input, cwd]. The cwd is shown
    // as a separate line below the input — the user no longer wants the
    // project path on the input block's title.
    let input_height = input_height(app, area.height, area.width);
    let chunks = if app.function_visible {
        // For PastePreview the panel height is exactly the content height
        // (capped at 20%); for other tabs it grows with 20% but never below
        // the minimum renderable height.
        let remaining = area.height.saturating_sub(input_height + CWD_HEIGHT);
        let pct_height = (remaining as f64 * 0.20) as u16;
        let panel_height = app.function.tabs.get(app.function.active)
            .map_or(4, |t| t.panel_height(pct_height));

        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(panel_height),
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
    app.session.tool_preview_lines = app.config.tool_preview_lines;
    let session_frame_area = chunks[0];
    let area = session_content_area(session_frame_area);
    app.session_area = Some(session_frame_area);

    // Pre-warm the layout cache before any render call, so that
    // `session::render::render` can read `cached_total_lines_for` cheaply
    // and `build_lines_viewport` knows which messages intersect the viewport.
    let width_u16 = area.width;
    app.session.count_all_lines_with_width(width_u16 as usize);

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
    //
    // This is now a SINGLE pass that:
    //   1. Computes `total_lines` (delegating to the cached
    //      `Session::count_all_lines_with_width`, which populates
    //      per-block line counts on first miss and is O(N) thereafter).
    //   2. Walks the session in lockstep with the cached counts to
    //      derive `start`, `thinking_toggle_rows`, and `tool_toggle_rows`.
    //   3. Renders the scrollbar.
    //
    // Before this refactor we did three full passes per frame and called
    // `thinking_block_line_count` / `tool_block_line_count` (which
    // invoke the full block renderer just to count lines). The caches
    // turn those into O(1) reads.
    app.thinking_toggle_rows.clear();
    app.tool_toggle_rows.clear();
    let area = session_content_area(session_frame_area);
    let inner_h = area.height as usize;
    let width_u16 = area.width;

    // Compute total lines. This populates the per-block caches inside
    // `Session` on the first call per invalidation.
    let total_lines: usize = app.session.count_all_lines_with_width(width_u16 as usize) as usize;

    // Pin the viewport when the user has scrolled up. New streamed
    // content height is absorbed into `scroll` so the rendered `start`
    // (total - inner_h - scroll) stays constant — the user keeps
    // reading the same lines instead of being gradually pulled back
    // to the tail. Skipped at tail (`scroll == 0`) so we keep
    // following the latest output. `last_rendered_total` is keyed by
    // viewport width so a resize resets the comparison instead of
    // spuriously subtracting across widths.
    app.session.pin_scroll_for_total(width_u16, total_lines as u32);

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
    let end = start + inner_h;

    // Walk the session in lockstep with the cached counts to record
    // toggle rows for messages whose first visible row is on-screen.
    // Re-uses the per-block cache populated above.
    //
    // `line_idx` mirrors `Session::compute_total_lines` /
// `build_lines_viewport` so toggle rows land on the exact screen
    // row of the corresponding block's first line. That means:
    //   - no phantom role prefix line
    //   - +1 for each thinking/tool block (the trailing blank)
    //   - +1 leading gap if content precedes the first block
    //   - +1 gap after each message (inter-message or bottom gap)
    let mut line_idx: usize = 0;
    for (msg_idx, m) in app.session.messages.iter().enumerate() {
        // Thinking segments.
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
                    let screen_y = area.y + (line_idx - start) as u16;
                    app.thinking_toggle_rows.push((screen_y, msg_idx));
                }
                let lines = if expanded {
                    seg.cached_line_count_expanded.unwrap_or(0) as usize
                } else {
                    seg.cached_line_count_collapsed.unwrap_or(0) as usize
                };
                line_idx += lines;
                line_idx += 1; // trailing blank after the thinking block
                thinking_blocks += 1;
            }
        }

        // Content (post-markdown rendered count, cached by width).
        let content_lines =
            crate::session::render::read_cached_content_count_at(m, width_u16) as usize;
        line_idx += content_lines;

        // Tool result blocks.
        let mut tool_blocks: usize = 0;
        if app.config.tool_display != crate::config::ToolResultDisplay::Hide {
            for (tool_idx, t) in m.tool_results.iter().enumerate() {
                // `t.running` no longer forces expansion — see the
                // matching note in `build_lines_viewport`. The
                // pending background colour alone signals "in flight".
                let t_vis = match app.config.tool_display {
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
                if lines > 0 && line_idx >= start && line_idx < end {
                    let screen_y = area.y + (line_idx - start) as u16;
                    app.tool_toggle_rows.push((screen_y, msg_idx, tool_idx));
                }
                line_idx += lines;
                line_idx += 1; // trailing blank after the tool block
                tool_blocks += 1;
            }
        }

        // Leading gap: added before the first thinking/tool block
        // only when content precedes it (offset > 0). When the
        // message starts with a block, the message-level gap
        // provides spacing.
        let first_offset = m.thinking_segments.iter().map(|s| s.offset)
            .chain(m.tool_results.iter().map(|t| t.content_offset))
            .min();
        if first_offset.map_or(false, |off| off > 0) && (thinking_blocks > 0 || tool_blocks > 0) {
            line_idx += 1;
        }

        // User messages get a background-filled padding line above
        // and below the content (`build_message_lines` inserts one
        // and pushes another). Assistant/system messages do not.
        // When the message carries a `skill_ref`, also add the rows
        // for the `[skill]` marker block (5-6 rows + 1 trailing
        // blank) so the toggle hit-boxes line up with the rendered
        // block — without this the screen y of the next block is
        // shifted up by the undercounted rows and clicks land on
        // the wrong message.
        if m.role == crate::session::Role::User {
            if let Some(skill_ref) = &m.skill_ref {
                line_idx += crate::session::render::skill_block_line_count(
                    skill_ref,
                    width_u16 as usize,
                ) as usize;
            }
            line_idx += 2;
        }

        // Gap after this message (inter-message or bottom gap).
        line_idx += 1;
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
    // least ~25% of the viewport. The previous 40%-of-viewport cap
    // silently truncated long single-line input down to one visual
    // row, hiding the rest of the message until something else
    // triggered a redraw (typically the first streaming token).
    let min_for_session = ((viewport_height as f32) * 0.25).floor() as u16;
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
    // Stream selection: from start through end, flowing across line breaks.
    let start = sel.start;
    let end = sel.end;
    let width = buf.area().width;
    let w = width.saturating_sub(1);
    if start.1 == end.1 {
        let x_min = start.0.min(end.0);
        let x_max = start.0.max(end.0);
        for x in x_min..=x_max {
            if let Some(cell) = buf.cell_mut((x, start.1)) {
                let new_style = cell.style().add_modifier(Modifier::REVERSED);
                cell.set_style(new_style);
            }
        }
    } else if start.1 < end.1 {
        for y in start.1..=end.1 {
            let row_sx = if y == start.1 { start.0 } else { 0 };
            let row_ex = if y == end.1 { end.0.min(w) } else { w };
            for x in row_sx..=row_ex {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    let new_style = cell.style().add_modifier(Modifier::REVERSED);
                    cell.set_style(new_style);
                }
            }
        }
    } else {
        for y in end.1..=start.1 {
            let row_sx = if y == end.1 { end.0 } else { 0 };
            let row_ex = if y == start.1 { start.0.min(w) } else { w };
            for x in row_sx..=row_ex {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    let new_style = cell.style().add_modifier(Modifier::REVERSED);
                    cell.set_style(new_style);
                }
            }
        }
    }
}

/// Read the rendered symbols from the buffer in the selection area and
/// return them as plain text. Trailing whitespace on each row is trimmed
/// and empty trailing rows are dropped, so a single-row selection across a
/// padded cell line does not produce a wall of spaces.
pub fn extract_selection_text(buf: &Buffer, sel: &Selection) -> String {
    let start = sel.start;
    let end = sel.end;
    let width = buf.area().width;
    let w = width.saturating_sub(1);
    let mut lines: Vec<String> = Vec::new();
    if start.1 == end.1 {
        let x_min = start.0.min(end.0);
        let x_max = start.0.max(end.0);
        let mut line = String::new();
        for x in x_min..=x_max {
            if let Some(cell) = buf.cell((x, start.1)) {
                line.push_str(cell.symbol());
            }
        }
        lines.push(line.trim_end().to_string());
    } else if start.1 < end.1 {
        for y in start.1..=end.1 {
            let row_sx = if y == start.1 { start.0 } else { 0 };
            let row_ex = if y == end.1 { end.0.min(w) } else { w };
            let mut line = String::new();
            for x in row_sx..=row_ex {
                if let Some(cell) = buf.cell((x, y)) {
                    line.push_str(cell.symbol());
                }
            }
            lines.push(line.trim_end().to_string());
        }
    } else {
        for y in end.1..=start.1 {
            let row_sx = if y == end.1 { end.0 } else { 0 };
            let row_ex = if y == start.1 { start.0.min(w) } else { w };
            let mut line = String::new();
            for x in row_sx..=row_ex {
                if let Some(cell) = buf.cell((x, y)) {
                    line.push_str(cell.symbol());
                }
            }
            lines.push(line.trim_end().to_string());
        }
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
