use crate::app::App;
use crate::config::Config;
use crate::function::{CancelState, Selection};
use crate::session::Session;
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

pub mod backend;
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
    let agents_height = if app.agents_visible {
        AGENTS_AREA_HEIGHT
    } else {
        0
    };
    let input_height = input_height(app, area.height, area.width);

    let mut constraints = vec![];
    if app.agents_visible {
        constraints.push(Constraint::Length(agents_height));
    }
    constraints.push(Constraint::Min(0));

    if app.function_visible {
        let remaining = area
            .height
            .saturating_sub(input_height + CWD_HEIGHT + agents_height);
        let pct_height = (remaining as f64 * 0.30) as u16;
        let panel_height = app
            .function
            .tabs
            .get(app.function.active)
            .map_or(4, |t| t.panel_height(pct_height, app, area.width));
        constraints.push(Constraint::Length(panel_height));
    }

    constraints.push(Constraint::Length(input_height));
    constraints.push(Constraint::Length(CWD_HEIGHT));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    app.session.sync_display_mode(
        app.config.thinking_display,
        app.config.tool_display,
        app.config.tool_preview_lines,
    );

    let agents_idx = 0;
    let session_idx = if app.agents_visible { 1 } else { 0 };
    let panel_idx = session_idx + 1;
    let input_idx = if app.function_visible {
        panel_idx + 1
    } else {
        session_idx + 1
    };
    let cwd_idx = input_idx + 1;

    if app.agents_visible {
        app.agents_area = Some(chunks[agents_idx]);
        render_agents_area(chunks[agents_idx], f.buffer_mut(), app);
    } else {
        app.agents_area = None;
    }
    let session_frame_area = chunks[session_idx];
    let content_area = session_content_area(session_frame_area);
    app.session_area = Some(content_area);

    let width_u16 = content_area.width;
    let inner_h = content_area.height as usize;
    app.session.count_all_lines_with_width(width_u16 as usize);

    let total_lines: usize = app.session.line_offsets.last().copied().unwrap_or(0) as usize;

    // If a pending scroll-to-top was set (e.g. by jump_to_message),
    // compute the scroll using the real inner_h known only at render
    // time. This avoids the stale-height bug where the panel was
    // visible when the jump was computed but hidden by the time render
    // runs, making the viewport taller and the clamp resetting scroll.
    if let Some(lines_before) = app.session.pending_scroll_top.take() {
        let new_scroll = (total_lines as u32)
            .saturating_sub(inner_h as u32)
            .saturating_sub(lines_before);
        app.session.scroll = new_scroll;
        app.session_scroll.snap(new_scroll as f32);
        // Sync last_rendered_total so pin_scroll_for_total (which
        // runs next) doesn't treat the total as "grew from 0" and
        // add a huge delta that clamps scroll back to max (top).
        app.session.last_rendered_total = Some((width_u16, total_lines as u32));
    }

    // Pin + clamp scroll BEFORE rendering so the viewport uses the
    // correct offset this frame. Doing this after `render()` caused a
    // one-frame mismatch during streaming: content grew, the view
    // shifted, then scroll was adjusted on the next frame — producing
    // a visible up-then-down jitter.
    app.session
        .pin_scroll_for_total(width_u16, total_lines as u32);
    app.session.scroll = app
        .session
        .scroll
        .min(total_lines.saturating_sub(inner_h).min(u32::MAX as usize) as u32);

    crate::session::render::render(content_area, f.buffer_mut(), &app.session);
    if app.function_visible {
        app.function_panel_area = Some(chunks[panel_idx]);
        function_panel::render(chunks[panel_idx], f.buffer_mut(), app);
    } else {
        app.function_panel_area = None;
    }
    crate::input::render(chunks[input_idx], f.buffer_mut(), app);
    render_cwd(chunks[cwd_idx], f.buffer_mut(), app);

    app.thinking_toggle_rows.clear();
    app.tool_toggle_rows.clear();

    let scroll = app.session.scroll;
    render_session_scrollbar(
        session_frame_area,
        f.buffer_mut(),
        total_lines,
        inner_h,
        scroll as usize,
    );
    let start = total_lines.saturating_sub(inner_h + scroll as usize);
    let end = start + inner_h;

    collect_toggle_rows(app, content_area, start, end, inner_h, width_u16);
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

    if app.force_full_repaint && app.inflight.is_none() {
        let buf = f.buffer_mut();
        let area = app.session_area.unwrap_or(*buf.area());
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    if matches!(cell.diff_option, CellDiffOption::None) {
                        cell.set_diff_option(CellDiffOption::AlwaysUpdate);
                    }
                }
            }
        }
        app.force_full_repaint = false;
    }

    // Position the hardware cursor for the focused input.
    //
    // We always set the cursor position (even during inflight) so that
    // ratatui calls `show_cursor()` + `set_cursor_position()` on every
    // draw. The `CursorTrackingBackend` wrapper de-duplicates the
    // `show_cursor` calls, preventing the terminal's native blink timer
    // from being reset on every frame.
    let cursor = match app.focus_target {
        crate::function::FocusTarget::FunctionPanel => app.function_panel_cursor,
        crate::function::FocusTarget::Input => app.input_cursor_screen,
        crate::function::FocusTarget::AgentsCheckbox => None,
    };
    if let Some((cx, cy)) = cursor {
        f.set_cursor_position((cx, cy));
    }
}

/// A thinking or tool block's document-line span produced by the
/// toggle-row walk. `top`/`bottom` are exclusive/inclusive doc lines
/// (like `Message::line_offsets`), `msg_idx`/`idx` are the message and
/// segment/tool indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ToggleBlock {
    top: u32,
    bottom: u32,
    msg_idx: usize,
    idx: usize,
}

/// Walk the session and compute the document-line span of every
/// thinking / tool toggle block, in render order. This is the single
/// source of truth for toggle geometry and must stay in lockstep with
/// `Session::compute_total_lines` / `build_message_lines` — the walk
/// mirrors their block placement (content segments, `ensure_gap_before_block`,
/// trailing blank per block, user +2 fill lines, inter-message gap).
///
/// Returns `(thinking, tool)` blocks in doc-line coordinates. The
/// viewport→screen mapping is applied later by `collect_toggle_rows`,
/// which makes this function trivially unit-testable against
/// `Session::count_all_lines_with_width` / `build_message_lines`.
fn collect_toggle_blocks(
    session: &Session,
    config: &Config,
    width: u16,
) -> (Vec<ToggleBlock>, Vec<ToggleBlock>, usize) {
    let mut thinking: Vec<ToggleBlock> = Vec::new();
    let mut tool: Vec<ToggleBlock> = Vec::new();
    let width_u16: u16 = width;
    let mut line_idx: usize = 0;

    for (msg_idx, m) in session.messages.iter().enumerate() {
        // ── Skill block (rendered before everything else, like
        // build_message_lines) ──────────────────────────────────
        if m.role == crate::session::Role::User {
            if let Some(skill_ref) = &m.skill_ref {
                line_idx +=
                    crate::session::render::skill_block_line_count(skill_ref, width_u16 as usize)
                        as usize;
            }
        }

        // ── Attachment block ─────────────────────────────────────
        if !m.attachments.is_empty() {
            line_idx +=
                crate::session::render::attachment_block_line_count(&m.attachments) as usize;
        }

        // ── Interleave content + thinking blocks by offset ─
        // Mirrors build_message_lines: thinking blocks are placed by
        // their offsets, content renders between them, but ALL tool
        // blocks render after the complete content text (and after
        // every thinking block). This keeps the tool boxes together
        // at the end so text is never split by a mid-line tool offset.
        let raw = if m.streaming {
            m.visible_content()
        } else {
            &m.content
        };

        let think_show = m.role == crate::session::Role::Assistant
            && crate::session::render::message_has_thinking(m)
            && config.thinking_display != crate::config::ThinkingDisplay::Hide;
        let tool_show = config.tool_display != crate::config::ToolResultDisplay::Hide;

        // Build sorted thinking items matching build_message_lines.
        enum WalkItem {
            Thinking(usize),
            Tool(usize),
        }
        let mut items: Vec<(usize, WalkItem)> = Vec::new();
        if think_show {
            let segments = crate::session::render::get_thinking_segments(m);
            for (si, seg) in segments.iter().enumerate() {
                let offset =
                    crate::session::render::clamp_char_boundary(raw, seg.offset.min(raw.len()));
                let offset = crate::session::render::advance_to_word_boundary(raw, offset);
                items.push((offset, WalkItem::Thinking(si)));
            }
        }
        // Tools anchor at the end of content (matching build_message_lines).
        if tool_show {
            for (ti, t) in m.tool_results.iter().enumerate() {
                if t.content.is_empty() && t.streaming_input.is_empty() {
                    continue;
                }
                items.push((raw.len(), WalkItem::Tool(ti)));
            }
        }
        // Sort by offset; tools at the end render after thinking blocks.
        items.sort_by(|(off_a, a), (off_b, b)| {
            off_a.cmp(off_b).then_with(|| match (a, b) {
                (WalkItem::Tool(_), WalkItem::Thinking(_)) => std::cmp::Ordering::Greater,
                (WalkItem::Thinking(_), WalkItem::Tool(_)) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Equal,
            })
        });

        let mut cursor = 0usize;
        let mut prev_line_was_blank = false;
        let mut has_any_line = false;

        for (offset, item) in &items {
            let offset = *offset;
            if offset < cursor {
                continue;
            }

            // Render content before this item, exactly like
            // build_message_lines, so the line count and the
            // blank-ness of the last line match the real render.
            if offset > cursor {
                let seg_text = crate::session::render::strip_legacy_markers(&raw[cursor..offset]);
                let mut seg_buf: Vec<ratatui::text::Line<'static>> = Vec::new();
                crate::session::render::render_content_segment(
                    &seg_text,
                    width_u16 as usize,
                    &mut seg_buf,
                );
                let seg_lines = seg_buf.len();
                line_idx += seg_lines;
                cursor = offset;
                if seg_lines > 0 {
                    has_any_line = true;
                    prev_line_was_blank = seg_buf.last().map(|l| l.width() == 0).unwrap_or(false);
                }
            }

            // ensure_gap_before_block: add a blank line if there are
            // existing lines and the last line is non-blank.
            if has_any_line && !prev_line_was_blank {
                line_idx += 1;
            }

            match item {
                WalkItem::Thinking(si) => {
                    let seg = &m.thinking_segments[*si];
                    let expanded = (config.thinking_display
                        == crate::config::ThinkingDisplay::Show
                        && seg.visible)
                        || (config.thinking_display
                            == crate::config::ThinkingDisplay::ShowWhileStreaming
                            && (m.streaming || seg.visible));
                    let lines = if expanded {
                        seg.cached_line_count_expanded.unwrap_or(0) as usize
                    } else {
                        seg.cached_line_count_collapsed.unwrap_or(0) as usize
                    };
                    let block_top = line_idx;
                    let block_bot = line_idx + lines; // exclusive
                    if lines > 0 {
                        thinking.push(ToggleBlock {
                            top: block_top as u32,
                            bottom: block_bot as u32,
                            msg_idx,
                            idx: *si,
                        });
                    }
                    line_idx += lines;
                    line_idx += 1; // trailing blank
                    has_any_line = true;
                    prev_line_was_blank = true;
                }
                WalkItem::Tool(ti) => {
                    let t = &m.tool_results[*ti];
                    let t_vis = t.name == "plan"
                        || match config.tool_display {
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
                    let block_top = line_idx;
                    let block_bot = line_idx + lines; // exclusive
                    if lines > 0 && t.name != "plan" {
                        tool.push(ToggleBlock {
                            top: block_top as u32,
                            bottom: block_bot as u32,
                            msg_idx,
                            idx: *ti,
                        });
                    }
                    line_idx += lines;
                    line_idx += 1; // trailing blank
                    has_any_line = true;
                    prev_line_was_blank = true;
                }
            }
        }

        // Render remaining content after last item.
        if cursor < raw.len() {
            let seg_text = crate::session::render::strip_legacy_markers(&raw[cursor..]);
            let seg_lines = crate::session::render::count_md_segment(&seg_text, width_u16 as usize);
            line_idx += seg_lines as usize;
        }

        // User messages: 2 background-fill lines (one above content,
        // one below).
        if m.role == crate::session::Role::User {
            line_idx += 2;
        }

        line_idx += 1; // inter-message gap
    }

    (thinking, tool, line_idx)
}

/// Map document-line toggle blocks into screen rows for the current
/// viewport and append them to `app.thinking_toggle_rows` /
/// `app.tool_toggle_rows`. `start`..`end` is the visible doc-line
/// range; `content_area.y` is the screen origin.
fn collect_toggle_rows(
    app: &mut App,
    content_area: Rect,
    start: usize,
    end: usize,
    inner_h: usize,
    width_u16: u16,
) {
    let (thinking, tool, _total) = collect_toggle_blocks(&app.session, &app.config, width_u16);
    app.thinking_toggle_rows.clear();
    app.tool_toggle_rows.clear();
    for b in thinking {
        if b.bottom > b.top && b.bottom as usize > start && (b.top as usize) < end {
            let screen_top =
                content_area.y + (b.top as usize).saturating_sub(start).min(inner_h) as u16;
            let screen_bot =
                content_area.y + ((b.bottom as usize).min(end) - start).min(inner_h) as u16;
            app.thinking_toggle_rows.push((
                screen_top,
                screen_bot.saturating_sub(1),
                b.msg_idx,
                b.idx,
            ));
        }
    }
    for b in tool {
        if b.bottom > b.top && b.bottom as usize > start && (b.top as usize) < end {
            let screen_top =
                content_area.y + (b.top as usize).saturating_sub(start).min(inner_h) as u16;
            let screen_bot =
                content_area.y + ((b.bottom as usize).min(end) - start).min(inner_h) as u16;
            app.tool_toggle_rows
                .push((screen_top, screen_bot.saturating_sub(1), b.msg_idx, b.idx));
        }
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
    let stats_pad = if stats_width > 0 && avail > stats_width {
        1
    } else {
        0
    };

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
                format!(
                    "{} esc to interrupt [{timer}]",
                    crate::input::spinner_prompt().trim()
                )
            }
            CancelState::Confirming(_) => {
                format!(
                    "{} esc again [{timer}]",
                    crate::input::spinner_prompt().trim()
                )
            }
        };
        let hint_w = UnicodeWidthStr::width(hint.as_str());
        let sep = " | ";
        let fixed_w = hint_w + sep.len();
        let path_max = left_w.saturating_sub(fixed_w);
        let truncated = truncate_path(path, path_max);
        let line = Line::from(vec![
            Span::styled(hint, Theme::dim()),
            Span::styled(sep, Theme::dim()),
            Span::styled(truncated, Theme::dim()),
        ]);
        let p = ratatui::widgets::Paragraph::new(line);
        p.render(left_area, buf);
    } else {
        let path_max = left_w;
        let truncated = truncate_path(path, path_max);
        let line = Line::from(vec![Span::styled(truncated, Theme::dim())]);
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
    // Clamp the screen-relative row into [0, inner_h) so a click/drag that
    // leaves the session area (into the header above or the input below)
    // maps to the topmost/bottom-most visible document line instead of a
    // doc line that does not exist. Without this, when total < inner_h
    // (empty session at startup) a click in the blank lower half produced a
    // doc line far past `total`, and dragging up to the top then spanned the
    // entire viewport with REVERSED style.
    let rel = (y.saturating_sub(area.top())) as u32;
    let clamped_rel = rel.min(inner_h.saturating_sub(1));
    let mut doc = offset_from_top + clamped_rel;
    if doc >= total {
        doc = total.saturating_sub(1);
    }
    doc as usize
}

/// Convert a global document line index to a screen Y, if visible.
pub(crate) fn doc_line_to_screen_y(
    line: usize,
    area: &Rect,
    scroll: u32,
    total: u32,
) -> Option<u16> {
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
        // Never highlight doc lines past `total` — they correspond to the
        // blank padding below a short session and would otherwise fill the
        // whole viewport when the user drags into the empty lower half.
        if (doc_line as u32) >= total {
            break;
        }
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
pub fn extract_selection_text(sel: &Selection, session: &Session, width: usize) -> String {
    let y_start = sel.doc_start.min(sel.doc_end);
    let y_end = sel.doc_start.max(sel.doc_end);
    let (col_lo, col_hi) = if sel.doc_start <= sel.doc_end {
        (sel.col_start, sel.col_end)
    } else {
        (sel.col_end, sel.col_start)
    };
    let col_lo = col_lo.unwrap_or(0) as usize;
    // `col_hi` is the exclusive end passed to `slice_by_visual_width`
    // ([start, end)). The highlight rect (apply_selection_style) uses an
    // inclusive end, so a drag ending on column `c` highlights column `c`
    // but a raw `c` here would drop it. Bump any concrete end column by 1
    // to include it; a None (full-width) end already maps to `width`.
    let col_hi = col_hi.map(|c| c as usize + 1).unwrap_or(width);
    let mut lines: Vec<String> = Vec::new();

    let offsets = &session.line_offsets;
    if offsets.len() < 2 {
        return String::new();
    }

    // Guard: clamp the selection's end to the real total so the blank
    // padding rows of a short session are never returned as text.
    let total = *offsets.last().unwrap_or(&0) as usize;
    let y_end = y_end.min(total.saturating_sub(1));
    if y_start > y_end {
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
        let local_end = y_end
            .min(msg_end.saturating_sub(1))
            .saturating_sub(msg_start);

        let rendered = crate::session::render::build_message_lines(session, msg_idx, width);
        for (i, line) in rendered.iter().enumerate() {
            if i < local_start || i > local_end {
                continue;
            }
            let full: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            // Determine column slice for this row. The full-width end
            // column must be the row's *visual* width (terminal cells),
            // NOT its Unicode char count. A CJK char occupies 2 cells,
            // so counting chars undercuts the end column and truncates
            // the right edge of any row containing wide characters.
            let full_width = UnicodeWidthStr::width(full.as_str());
            let (cs, ce) = if y_start == y_end {
                (col_lo, col_hi)
            } else if i == local_start {
                (col_lo, full_width)
            } else if i == local_end {
                (0, col_hi)
            } else {
                (0, full_width)
            };
            let sliced = slice_by_visual_width(&full, cs, ce);
            lines.push(sliced.trim_end().to_string());
        }
    }

    while lines.len() > 1 && lines.last().unwrap().is_empty() {
        lines.pop();
    }
    lines.join("\n")
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

/// Render the agents.md splash area at the top of a new session.
/// Left side: logo, right side: checkboxes for discovered agents.md files.
pub fn render_agents_area(
    area: ratatui::layout::Rect,
    buf: &mut ratatui::buffer::Buffer,
    app: &mut crate::app::App,
) {
    use crate::theme::Theme;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Paragraph};

    if area.height < 5 || area.width < 20 {
        return;
    }

    // Wrap in a bordered block
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(app.config.border_type.ratatui_set())
        .border_style(match app.focus_target {
            crate::function::FocusTarget::AgentsCheckbox => Theme::focused_border(),
            crate::function::FocusTarget::Input => Theme::unfocused_border(),
            crate::function::FocusTarget::FunctionPanel => Theme::unfocused_border(),
        });
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height < 3 {
        return;
    }

    let logo_lines = [
        "\u{2590}\u{2588}\u{259B}\u{2588}\u{259B}\u{2588}\u{258C}",
        "\u{2590}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{258C}",
    ];
    let logo_width = 7u16;
    let right_x = inner.x + logo_width + 1;
    let right_w = inner.width.saturating_sub(logo_width + 1);

    // Render logo
    for (i, line) in logo_lines.iter().enumerate() {
        let y = inner.y + i as u16;
        let logo_line = Line::from(Span::styled(*line, Theme::bold()));
        let p = Paragraph::new(logo_line);
        p.render(
            ratatui::layout::Rect {
                x: inner.x,
                y,
                width: logo_width,
                height: 1,
            },
            buf,
        );
    }

    // Render load duration below the logo
    let load_text = format!(" ⚡{}ms", app.load_duration.as_millis());
    let load_line = Line::from(Span::styled(load_text, Theme::dim()));
    let p_launch = Paragraph::new(load_line);
    p_launch.render(
        ratatui::layout::Rect {
            x: inner.x,
            y: inner.y + logo_lines.len() as u16,
            width: inner.width,
            height: 1,
        },
        buf,
    );

    // Render checkboxes
    let entries: Vec<(&String, &bool)> = app.config.agents.entries.iter().collect();
    if entries.is_empty() {
        let hint = Line::from(Span::styled("No agents.md found", Theme::dim()));
        let p = Paragraph::new(hint);
        p.render(
            ratatui::layout::Rect {
                x: right_x,
                y: inner.y,
                width: right_w,
                height: 1,
            },
            buf,
        );
        return;
    }

    for (i, (path, &enabled)) in entries.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + 2 {
            break;
        }
        let marker = if enabled { "[x]" } else { "[ ]" };
        let cursor = if app.agents_cursor == i
            && app.focus_target == crate::function::FocusTarget::AgentsCheckbox
        {
            "> "
        } else {
            "  "
        };
        let short = path
            .rsplit('/')
            .next()
            .or_else(|| path.rsplit('\\').next())
            .unwrap_or(path);
        let label = format!("{cursor}{marker} {short}");
        let style = if app.focus_target == crate::function::FocusTarget::AgentsCheckbox
            && app.agents_cursor == i
        {
            Theme::bold()
        } else {
            Theme::dim()
        };
        let line = Line::from(Span::styled(label, style));
        let p = Paragraph::new(line);
        p.render(
            ratatui::layout::Rect {
                x: right_x,
                y,
                width: right_w,
                height: 1,
            },
            buf,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThinkingDisplay;
    use crate::session::{Message, Role, ThinkingSegment, ToolResultBlock};

    fn shell_tool() -> ToolResultBlock {
        ToolResultBlock {
            name: "shell_command".to_string(),
            title: "$ echo hi".to_string(),
            content: serde_json::json!({
                "ok": true,
                "result": "exit_code: 0\nwall_secs: 0.01\ntimeout_secs: 300\nstdout:\nhi\n\nstderr:\n"
            })
            .to_string(),
            metadata: String::new(),
            content_offset: 0,
            visible: true,
            running: false,
            failed: false,
            call_id: String::new(),
            pruned: false,
            streaming_input: String::new(),
            cached_line_count_visible: None,
            cached_line_count_collapsed: None,
            started_at: None,
        }
    }

    fn thinking_seg(offset: usize, content: &str) -> ThinkingSegment {
        ThinkingSegment {
            offset,
            content: content.to_string(),
            closed: false,
            tool_results_len_at_open: 0,
            cached_line_count_expanded: None,
            cached_line_count_collapsed: None,
            started_at: None,
            ended_at: None,
            visible: false,
        }
    }

    /// The toggle walk and `Session::compute_total_lines` must agree on
    /// the total document line count. Any placement drift in either side
    /// (content segments, gaps, trailing blanks, user +2 fill, inter-
    /// message gap) breaks toggle click alignment, so this is the core
    /// regression guard for `collect_toggle_blocks`.
    fn assert_walk_matches_total(session: &mut Session, config: &Config, width: u16) {
        // Mirror what `ui::render` does before counting: sync the
        // session's display fields from the config so `compute_total_lines`
        // and the walk agree on which blocks are visible.
        session.sync_display_mode(
            config.thinking_display,
            config.tool_display,
            config.tool_preview_lines,
        );
        let expected = session.count_all_lines_with_width(width as usize);
        let (thinking, tool, walked) = collect_toggle_blocks(session, config, width);
        assert_eq!(
            walked as u32, expected,
            "toggle walk total ({walked}) != compute_total_lines ({expected})"
        );
        // Every collected block must be non-empty and within [0, total).
        for b in thinking.iter().chain(tool.iter()) {
            assert!(b.bottom > b.top, "empty block span {:?}", b);
            assert!(
                b.bottom as u32 <= expected,
                "block {:?} exceeds total {expected}",
                b
            );
        }
    }

    #[test]
    fn toggle_walk_total_matches_compute_for_mixed_session() {
        let cfg = Config::default(); // thinking_display=Show, tool_display=Show
        let mut s = Session::default();
        s.push(Message::new(Role::User, "short user message"));

        let mut asst = Message::new(Role::Assistant, "I will run a command for you.");
        asst.thinking_segments
            .push(thinking_seg(0, "hidden reasoning"));
        asst.thinking_visible = true;
        asst.tool_results.push(shell_tool());
        s.push(asst);

        let width = 80u16;
        assert_walk_matches_total(&mut s, &cfg, width);

        // Thinking and tool blocks must both be collected exactly once.
        let (thinking, tool, _) = collect_toggle_blocks(&s, &cfg, width);
        assert_eq!(thinking.len(), 1, "expected one thinking block");
        assert_eq!(tool.len(), 1, "expected one tool block");
    }

    #[test]
    fn toggle_walk_honors_hidden_display_modes() {
        let mut cfg = Config::default();
        cfg.thinking_display = ThinkingDisplay::Hide;
        cfg.tool_display = crate::config::ToolResultDisplay::Hide;

        let mut s = Session::default();
        s.push(Message::new(Role::User, "do it"));
        let mut asst = Message::new(Role::Assistant, "ok");
        asst.thinking_segments
            .push(thinking_seg(0, "secret reasoning"));
        asst.thinking_visible = true;
        asst.tool_results.push(shell_tool());
        s.push(asst);

        let width = 80u16;
        assert_walk_matches_total(&mut s, &cfg, width);
        let (thinking, tool, _) = collect_toggle_blocks(&s, &cfg, width);
        assert!(thinking.is_empty(), "hidden thinking must not be collected");
        assert!(tool.is_empty(), "hidden tool must not be collected");
    }

    #[test]
    fn toggle_walk_plan_tool_is_not_collected_but_still_counts_lines() {
        let cfg = Config::default();
        let mut s = Session::default();
        s.push(Message::new(Role::User, "make a plan"));
        let mut asst = Message::new(Role::Assistant, "here is a plan:");
        asst.tool_results.push(ToolResultBlock {
            name: "plan".to_string(),
            title: "plan".to_string(),
            content: "1. step one\n2. step two".to_string(),
            metadata: String::new(),
            content_offset: 0,
            visible: true,
            running: false,
            failed: false,
            call_id: String::new(),
            pruned: false,
            streaming_input: String::new(),
            cached_line_count_visible: None,
            cached_line_count_collapsed: None,
            started_at: None,
        });
        s.push(asst);

        let width = 80u16;
        assert_walk_matches_total(&mut s, &cfg, width);
        let (_, tool, _) = collect_toggle_blocks(&s, &cfg, width);
        assert!(tool.is_empty(), "plan tool is not toggleable");
    }

    #[test]
    fn toggle_walk_spans_offsets_within_content() {
        let cfg = Config::default();
        let mut s = Session::default();
        s.push(Message::new(Role::User, "analyze this"));
        // Thinking segment at an offset inside the content, followed by
        // a tool, exercising the interleave + gap logic.
        let mut asst = Message::new(Role::Assistant, "first part\n\nsecond part");
        asst.thinking_segments
            .push(thinking_seg("first part\n\n".len(), "reasoning"));
        asst.thinking_visible = true;
        asst.tool_results.push(shell_tool());
        s.push(asst);

        let width = 80u16;
        assert_walk_matches_total(&mut s, &cfg, width);
        let (thinking, tool, _) = collect_toggle_blocks(&s, &cfg, width);
        assert_eq!(thinking.len(), 1);
        assert_eq!(tool.len(), 1);
        // The thinking block must start after the "first part" content.
        assert!(
            thinking[0].top > 0,
            "thinking block should be offset past leading content"
        );
    }
}
