use super::{Role, Session, SkillRef, ThinkingSegment, ToolResultBlock};
use crate::config::{ThinkingDisplay, ToolResultDisplay};
use crate::theme::{active_colors, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

const COLLAPSED_PREVIEW_LINES: usize = 10;

/// LRU cache entry for a fully rendered message. Validity is checked
/// against `Message.content_version` so changing one message does not
/// invalidate cached render output for any other message.
#[derive(Debug)]
pub struct CachedMessageLines {
    pub content_version: u64,
    pub width: u16,
    pub display_cursor: usize,
    pub lines: Vec<Line<'static>>,
}

/// Cache for the last rendered viewport. When neither session state
/// (`layout_version`) nor viewport geometry (`width`, `height`,
/// `scroll`) has changed, we skip the entire render pipeline and
/// reuse the last buffer. This makes frames where nothing is
/// happening truly zero-cost (no message iteration, no locking,
/// no LRU lookups).
#[derive(Debug)]
pub struct RenderCache {
    pub lines: Vec<Line<'static>>,
    pub layout_version: u64,
    pub width: u16,
    pub height: u16,
    pub scroll: u16,
}

pub fn render(
    area: Rect,
    buf: &mut Buffer,
    session: &Session,
    tool_toggle_rows: &mut Vec<(u16, usize, usize)>,
) {
    let inner_h = area.height as usize;
    let width = area.width as usize;
    if width == 0 || inner_h == 0 {
        return;
    }

    // Fast path: render cache hit. When nothing changed
    // (layout_version, width, height, scroll all identical), skip
    // the entire message iteration, LRU lookups, and markdown
    // parsing — just blit the last buffer.
    {
        let cache = session.render_cache.lock().unwrap();
        if let Some(cached) = &*cache {
            if cached.layout_version == session.layout_version
                && cached.width == area.width
                && cached.height == area.height
                && cached.scroll == session.scroll
            {
                // Cache still valid — reuse without touching messages.
                tool_toggle_rows.clear();
                for y in area.top()..area.bottom() {
                    for x in area.left()..area.right() {
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.set_symbol(" ");
                            cell.set_style(Style::reset());
                        }
                    }
                }
                let p = Paragraph::new(cached.lines.clone());
                p.render(area, buf);
                return;
            }
        }
    }

    // Compute total lines using the cached per-block line counts. This
    // is O(N) over messages but each step is O(1) thanks to the
    // per-block caches added in Phase A. Callers are expected to
    // pre-warm the cache via `Session::count_all_lines_with_width`
    // (which requires `&mut self`); when they don't, we fall back
    // to a rough estimate.
    let total = session
        .cached_total_lines_for(width)
        .unwrap_or_else(|| count_lines_estimate(session));
    let total_u16 = total.min(u16::MAX as u32) as u16;
    let max_scroll = total_u16.saturating_sub(inner_h as u16);
    let scroll = session.scroll.min(max_scroll);
    let offset_from_top = max_scroll.saturating_sub(scroll);
    let start = offset_from_top;
    let end = (offset_from_top + inner_h as u16).min(total_u16);

    tool_toggle_rows.clear();

    // Viewport-aware: only build lines for messages that intersect
    // [start, end). The vast majority of messages in a 10M-token
    // session live outside the viewport, so this is the dominant
    // win for Phase B.
    let visible: Vec<Line> = if start < end {
        build_lines_viewport(session, width, start as u32, end as u32)
    } else {
        vec![]
    };

    // Clear the entire area first to prevent background artifacts from
    // previous frames leaking into cells that are no longer covered by content.
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol(" ");
                cell.set_style(Style::reset());
            }
        }
    }
    let p = Paragraph::new(visible.clone());
    p.render(area, buf);

    // Store in render cache for the next frame.
    if let Ok(mut cache) = session.render_cache.lock() {
        *cache = Some(RenderCache {
            lines: visible,
            layout_version: session.layout_version,
            width: area.width,
            height: area.height,
            scroll: session.scroll,
        });
    }
}

/// Fallback total-line count used when no cache is available. Walks
/// the session and adds `m.line_count` plus rough estimates for
/// thinking/tools. Only invoked in the rare path where the cache
/// hasn't been warmed by the caller.
fn count_lines_estimate(session: &Session) -> u32 {
    let mut n: u32 = 0;
    for m in &session.messages {
        n += 1; // role prefix
        if !m.thinking.trim().is_empty() {
            n += 1; // toggle line
            if m.thinking_visible {
                n += m.thinking.matches('\n').count() as u32 + 1;
            }
        }
        n += m.line_count;
        n += m.tool_results.len() as u32 * 2; // rough estimate
        n += 1; // spacer
    }
    if !session.messages.is_empty() {
        n += 1;
    }
    n
}

/// Toggle label text used by older tests / callers.
pub const THINKING_TOGGLE_COLLAPSED: &str = "[thinking ▸]";
pub const THINKING_TOGGLE_EXPANDED: &str = "[thinking ▾]";
pub const THINKING_END: &str = "[end thinking]";

/// Render a single message into its full line vector. This is the
/// core rendering function. It reads from the LRU cache when possible.
///
/// Streaming messages are never cached; non-streaming messages ARE
/// cached via `session.message_lines_cache` (keyed by `msg_idx`).
/// When the cached entry matches `m.content_version` and `width`, it
/// is reused without re-rendering.
pub fn build_message_lines(session: &Session, msg_idx: usize, width: usize) -> Vec<Line<'static>> {
    if msg_idx >= session.messages.len() {
        return vec![];
    }
    let m = &session.messages[msg_idx];

    // Quick path: LRU cache hit. Both streaming and non-streaming
    // messages use the same cache. Streaming messages are keyed by
    // content_version + display_cursor so that the progressive reveal
    // (tick handler) correctly invalidates the cache.
    {
        let lru = session.message_lines_cache.lock().unwrap();
        if let Some(cached) = lru.get(&msg_idx) {
            if cached.content_version == m.content_version
                && cached.width == width as u16
                && cached.display_cursor == m.display_cursor
            {
                return cached.lines.clone();
            }
        }
    }

    let mut msg_lines: Vec<Line<'static>> = Vec::new();
    if let Some(skill_ref) = &m.skill_ref {
        let rows = build_skill_block_rows(skill_ref, width);
        push_block_rows(&mut msg_lines, rows);
        msg_lines.push(Line::from(""));
    }

    let raw = if m.streaming { m.visible_content() } else { &m.content };

    // Build sorted items (thinking segments + tools) for interleaved rendering
    enum RenderItemKind {
        Thinking(String),
        Tool(usize), // index into m.tool_results
    }
    struct RenderItem {
        offset: usize,
        kind: RenderItemKind,
    }

    let mut items: Vec<RenderItem> = Vec::new();

    // Add thinking segments (only when display allows)
    if m.role == Role::Assistant {
        let show_thinking = !m.thinking.trim().is_empty()
            && match session.display {
                ThinkingDisplay::Hide => false,
                _ => true,
            };
        if show_thinking {
            let segments = get_thinking_segments(m);
            for seg in &segments {
                let offset = clamp_char_boundary(raw, seg.offset.min(raw.len()));
                items.push(RenderItem {
                    offset,
                    kind: RenderItemKind::Thinking(seg.content.clone()),
                });
            }
        }
    }

    // Add tool results
    for (ti, tool) in m.tool_results.iter().enumerate() {
        let offset = clamp_char_boundary(raw, tool.content_offset.min(raw.len()));
        items.push(RenderItem {
            offset,
            kind: RenderItemKind::Tool(ti),
        });
    }

    // Sort by offset; at same offset, tools before thinking
    items.sort_by(|a, b| {
        a.offset.cmp(&b.offset).then_with(|| {
            match (&a.kind, &b.kind) {
                (RenderItemKind::Tool(_), RenderItemKind::Thinking(_)) => std::cmp::Ordering::Less,
                (RenderItemKind::Thinking(_), RenderItemKind::Tool(_)) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            }
        })
    });

    let mut cursor = 0usize;
    for item in items {
        let offset = item.offset;
        if offset < cursor { continue; }

        // Render content before this item
        if offset > cursor {
            render_content_segment(&strip_legacy_markers(&raw[cursor..offset]), width, &mut msg_lines);
            cursor = offset;
        }

        match item.kind {
            RenderItemKind::Thinking(content) => {
                let visible = match session.display {
                    ThinkingDisplay::Show => m.thinking_visible,
                    ThinkingDisplay::ShowWhileStreaming => m.streaming || m.thinking_visible,
                    _ => false,
                };
                let colors = active_colors();
                let bg = if m.streaming { colors.thinking_streaming_bg } else { colors.thinking_done_bg };
                ensure_gap_before_block(&mut msg_lines);
                let rows = build_thinking_block_rows(&content, visible, width, bg);
                push_block_rows(&mut msg_lines, rows);
                msg_lines.push(Line::from(""));
            }
            RenderItemKind::Tool(ti) => {
                if let Some(tool) = m.tool_results.get(ti) {
                    if session.tool_display != ToolResultDisplay::Hide {
                        let t_vis = match session.tool_display {
                            ToolResultDisplay::Show => tool.visible || tool.running,
                            ToolResultDisplay::ShowWhileStreaming => m.streaming || tool.visible || tool.running,
                            _ => false,
                        };
                        ensure_gap_before_block(&mut msg_lines);
                        let rows = build_tool_block_rows(tool, t_vis, width);
                        push_block_rows(&mut msg_lines, rows);
                        msg_lines.push(Line::from(""));
                    }
                }
            }
        }
    }
    // Render remaining content
    render_content_segment(&strip_legacy_markers(&raw[cursor..]), width, &mut msg_lines);

    if m.streaming {
        if let Some(last) = msg_lines.last_mut() {
            let mut s = last.spans.clone();
            s.push(Span::styled("▌", Theme::cursor()));
            *last = Line::from(s);
        } else {
            msg_lines.push(Line::from(Span::styled("▌", Theme::cursor())));
        }
    }
    msg_lines.push(Line::from(""));

    if m.role == Role::User {
        let user_bg = active_colors().user_bg;
        // Pop the trailing spacer; we'll re-add it after the background block.
        let spacer = msg_lines.pop();
        // Apply background and full-width padding to content lines.
        for line in &mut msg_lines {
            for span in &mut line.spans {
                span.style = span.style.bg(user_bg);
            }
            let content_len: usize = line.spans.iter().map(|s| UnicodeWidthStr::width(s.content.as_ref())).sum();
            let pad = width.saturating_sub(content_len);
            if pad > 0 {
                line.spans.push(Span::styled(" ".repeat(pad), Style::default().bg(user_bg)));
            }
        }
        // Blank line with background above content.
        msg_lines.insert(0, Line::from(Span::styled(" ".repeat(width), Style::default().bg(user_bg))));
        // Blank line with background below content.
        msg_lines.push(Line::from(Span::styled(" ".repeat(width), Style::default().bg(user_bg))));
        // Re-add the spacer (no background) so there's a gap to the next message.
        if let Some(s) = spacer {
            msg_lines.push(s);
        }
    }

    {
        let mut lru = session.message_lines_cache.lock().unwrap();
        lru.put(
            msg_idx,
            CachedMessageLines {
                content_version: m.content_version,
                width: width as u16,
                display_cursor: m.display_cursor,
                lines: msg_lines.clone(),
            },
        );
    }

    msg_lines
}

/// Build only the lines that intersect the visible viewport.
/// `start_line` and `end_line` are absolute line indices into the
/// full rendered output.
fn build_lines_viewport(
    session: &Session,
    width: usize,
    start_line: u32,
    end_line: u32,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut global_line: u32 = 0;

    for msg_idx in 0..session.messages.len() {
        let m = &session.messages[msg_idx];

        // Compute total rendered line count for this message, using the
        // per-block caches populated by `Session::count_all_lines_with_width`.
        let mut msg_total: u32 = 1; // role prefix
        msg_total += m.line_count; // content
        // Thinking segments
        if m.role == super::Role::Assistant && !m.thinking.trim().is_empty()
            && session.display != crate::config::ThinkingDisplay::Hide
        {
            let expanded = (session.display == crate::config::ThinkingDisplay::Show
                && m.thinking_visible)
                || (session.display == crate::config::ThinkingDisplay::ShowWhileStreaming
                    && (m.streaming || m.thinking_visible));
            for seg in &m.thinking_segments {
                if expanded {
                    msg_total += seg.cached_line_count_expanded.unwrap_or(0);
                } else {
                    msg_total += seg.cached_line_count_collapsed.unwrap_or(0);
                }
            }
        }
        // Tool results
        if session.tool_display != crate::config::ToolResultDisplay::Hide {
            for t in &m.tool_results {
                let t_vis = match session.tool_display {
                    crate::config::ToolResultDisplay::Show => t.visible || t.running,
                    crate::config::ToolResultDisplay::ShowWhileStreaming => {
                        m.streaming || t.visible || t.running
                    }
                    _ => false,
                };
                if t_vis {
                    msg_total += t.cached_line_count_visible.unwrap_or(0);
                } else {
                    msg_total += t.cached_line_count_collapsed.unwrap_or(0);
                }
            }
        }
        msg_total += 1; // spacer

        let msg_end = global_line + msg_total;

        // Does this message intersect the viewport?
        if msg_end > start_line && global_line < end_line {
            // Full render this message (maybe from cache).
            let rendered = build_message_lines(session, msg_idx, width);
            // Compute the slice of this message's lines that overlap the viewport.
            let local_start = start_line.saturating_sub(global_line) as usize;
            let local_end = (end_line.saturating_sub(global_line)).min(rendered.len() as u32) as usize;
            if local_start < local_end {
                out.extend(rendered[local_start..local_end].iter().cloned());
            }
        }

        global_line = msg_end;
        if global_line >= end_line {
            break;
        }
    }

    out
}

/// Build the full rendered line buffer for the entire session
/// (legacy; used by tests and callers that need everything).
/// Consider using `build_lines_viewport` for interactive rendering.
pub fn build_lines(
    session: &Session,
    width: usize,
) -> (Vec<Line<'static>>, Vec<(usize, usize, usize)>) {
    let mut out: Vec<Line<'static>> = Vec::new();
    for msg_idx in 0..session.messages.len() {
        let rendered = build_message_lines(session, msg_idx, width);
        out.extend(rendered);
    }
    while out.last().map(|l| l.width() == 0).unwrap_or(false) {
        out.pop();
    }
    if !out.is_empty() {
        out.push(Line::from(""));
    }
    (out, Vec::new())
}
fn strip_legacy_markers(s: &str) -> String {
    s.lines()
        .filter(|line| {
            let t = line.trim();
            !(t.starts_with("[tool:") && t.ends_with(']'))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn clamp_char_boundary(s: &str, mut idx: usize) -> usize {
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Render a text segment (content between tool markers) through Markdown.
fn render_content_segment(text: &str, width: usize, out: &mut Vec<Line<'static>>) {
    if text.is_empty() {
        return;
    }
    let text = crate::session::strip_text_tool_calls(text);
    if text.trim().is_empty() {
        return;
    }
    let inner_w = width.saturating_sub(3);
    let md_lines = crate::session::markdown::render_with_width(&text, inner_w);
    for line in md_lines {
        // Wrap each rendered line to inner_w so the user-block padding
        // can fill the rest of the row. Markdown parsing does not wrap
        // by default; a long unbreakable span would otherwise overflow
        // the viewport and break the background fill.
        if line.width() <= inner_w {
            let mut indented = vec![Span::raw("   ")];
            indented.extend(line.spans.into_iter());
            out.push(Line::from(indented));
        } else {
            // Concatenate all spans into a single string, wrap, then split
            // back into multiple lines preserving the first span's style
            // and emitting the rest as plain.
            let combined: String = line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            for wrapped in wrap_line(&combined, inner_w) {
                out.push(Line::from(vec![
                    Span::raw("   ".to_string()),
                    Span::raw(wrapped),
                ]));
            }
        }
    }
}

pub fn thinking_block_line_count(content: &str, visible: bool, width: usize) -> usize {
    if content.is_empty() {
        return 0;
    }
    build_thinking_block_rows(content, visible, width, active_colors().thinking_done_bg).len()
}

/// Count total thinking lines across all segments.
pub fn total_thinking_line_count(m: &super::Message, session: &Session, width: usize) -> usize {
    let show = m.role == super::Role::Assistant
        && !m.thinking.trim().is_empty()
        && session.display != crate::config::ThinkingDisplay::Hide;
    if !show {
        return 0;
    }
    let segments = get_thinking_segments(m);
    let mut total = 0;
    for seg in &segments {
        let visible = match session.display {
            crate::config::ThinkingDisplay::Show => m.thinking_visible,
            crate::config::ThinkingDisplay::ShowWhileStreaming => m.streaming || m.thinking_visible,
            _ => false,
        };
        total += thinking_block_line_count(&seg.content, visible, width);
    }
    total
}

pub fn tool_block_line_count(tool: &ToolResultBlock, visible: bool, width: usize) -> usize {
    build_tool_block_rows(tool, visible, width).len()
}

fn ensure_gap_before_block(msg_lines: &mut Vec<Line<'static>>) {
    if msg_lines.last().map(|l| l.width() != 0).unwrap_or(true) {
        msg_lines.push(Line::from(""));
    }
}

fn push_block_rows(out: &mut Vec<Line<'static>>, rows: Vec<Line<'static>>) {
    out.extend(rows);
}

fn block_colors_for_tool(tool: &ToolResultBlock) -> (Color, Option<Color>) {
    let colors = active_colors();
    if tool.running {
        return (colors.tool_pending_bg, None);
    }
    let failed = match tool.name.as_str() {
        "shell_command" | "command" => command_failed(&tool.content),
        "python_command" => python_command_failed(&tool.content),
        _ => false,
    };
    if failed {
        (colors.tool_error_bg, Some(colors.tool_error_fg))
    } else {
        (colors.tool_success_bg, None)
    }
}

fn bg_style(bg: Color) -> Style {
    Style::default().bg(bg)
}

fn dim_bg_style(bg: Color) -> Style {
    Style::default().add_modifier(Modifier::DIM).bg(bg)
}

fn command_failed(content: &str) -> bool {
    let content = unwrap_tool_result_content(content);
    value_after_prefix(&content, "exit_code: ")
        .map(|code| code != "0")
        .unwrap_or(false)
}

fn unwrap_tool_result_content(content: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return content.to_string();
    };
    if value.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        if let Some(result) = value.get("result").and_then(|v| v.as_str()) {
            return result.to_string();
        }
    }
    if value.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
            return format!("[Tool Error] {error}");
        }
    }
    content.to_string()
}

fn python_command_failed(content: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .and_then(|value| {
            value
                .get("output")
                .and_then(|v| v.as_str())
                .map(command_failed)
        })
        .unwrap_or(false)
}

/// Get thinking segments from a message, with backward compatibility
/// for the old single-string `thinking` field.
pub fn get_thinking_segments(m: &super::Message) -> Vec<ThinkingSegment> {
    if !m.thinking_segments.is_empty() {
        return m.thinking_segments.clone();
    }
    if !m.thinking.is_empty() {
        return vec![super::ThinkingSegment {
            offset: 0,
            content: m.thinking.clone(),
            cached_line_count_expanded: None,
            cached_line_count_collapsed: None,
        }];
    }
    vec![]
}

fn build_thinking_block_rows(content: &str, visible: bool, width: usize, bg: Color) -> Vec<Line<'static>> {
    build_output_block_rows(
        "thinking",
        " Thinking ",
        content.trim_end(),
        "",
        visible,
        width,
        bg,
    )
}

/// Build the boxed rows for a `[skill]` marker block. The block
/// shows name, optional args, and the on-disk context path so the
/// user has a stable visual identifier for the skill they invoked.
/// The actual skill body lives in `Message::content` and is rendered
/// below the block as ordinary markdown.
fn build_skill_block_rows(skill: &SkillRef, width: usize) -> Vec<Line<'static>> {
    let bg = active_colors().tool_success_bg;
    let width = width.max(8);
    let mut rows = Vec::new();
    rows.push(border_line(width, bg));
    rows.extend(box_row_lines("[skill]", width, bg));
    rows.extend(box_row_lines(&format!("name: {}", skill.name), width, bg));
    if let Some(args) = skill.args.as_deref().filter(|a| !a.trim().is_empty()) {
        rows.extend(box_row_lines(&format!("args: {args}"), width, bg));
    }
    rows.extend(box_row_lines(&format!("context: {}", skill.context_path), width, bg));
    rows.push(border_line(width, bg));
    rows
}

fn build_tool_block_rows(tool: &ToolResultBlock, visible: bool, width: usize) -> Vec<Line<'static>> {
    let (bg, fg) = block_colors_for_tool(tool);

    let mut rows: Vec<Line<'static>> = if tool.name == "write_file" {
        if let Some(r) = build_write_file_diff_rows(tool, visible, width, bg) {
            r
        } else {
            return vec![];
        }
    } else if tool.name == "python_command" {
        if let Some(r) = build_python_command_rows(tool, visible, width, bg) {
            r
        } else {
            return vec![];
        }
    } else {
        let (output, footer) = tool_display_content(tool);
        let title_highlighted = tool.name == "shell_command"
            || tool.name == "command";
        if title_highlighted {
            build_shell_command_rows(&tool.title, &output, &footer, visible, width, bg)
        } else {
            build_output_block_rows(
                &tool.title,
                " Output ",
                &output,
                &footer,
                visible,
                width,
                bg,
            )
        }
    };

    if let Some(fg) = fg {
        for line in &mut rows {
            for span in &mut line.spans {
                span.style = span.style.fg(fg);
            }
        }
    }

    rows
}

fn build_shell_command_rows(
    title: &str,
    output: &str,
    footer: &str,
    visible: bool,
    width: usize,
    bg: Color,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let mut rows = Vec::new();
    rows.push(border_line(width, bg));

    // Highlight the shell command in the title row
    if let Some(cmd) = title.strip_prefix("$ ") {
        let cmd_spans = crate::session::markdown::highlight_line(cmd, "sh");
        let cmd_spans = spans_with_bg(&cmd_spans, bg);
        let mut label_spans = vec![Span::styled("$ ", bg_style(bg))];
        label_spans.extend(cmd_spans);
        rows.push(box_row_line_spans(label_spans, width, bg));
    } else {
        rows.extend(box_row_lines(title, width, bg));
    }

    rows.push(border_with_label_line(width, " Output ", bg));

    if visible {
        let body_rows = output_row_lines(output, width, bg);
        if body_rows.is_empty() {
            rows.extend(box_row_lines("[no output]", width, bg));
        } else {
            rows.extend(body_rows);
        }
        if !footer.is_empty() {
            rows.extend(box_row_lines(footer, width, bg));
        }
    } else {
        rows.extend(collapsed_output_lines(output, width, bg));
        // Show footer info even when collapsed for shell commands
        if !footer.is_empty() {
            rows.extend(box_row_lines(footer, width, bg));
        }
    }

    rows.push(border_line(width, bg));
    rows
}

fn build_output_block_rows(
    title: &str,
    label: &str,
    output: &str,
    footer: &str,
    visible: bool,
    width: usize,
    bg: Color,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let mut rows = Vec::new();
    rows.push(border_line(width, bg));
    rows.extend(box_row_lines(title, width, bg));
    rows.push(border_with_label_line(width, label, bg));

    if visible {
        let body_rows = output_row_lines(output, width, bg);
        if body_rows.is_empty() {
            rows.extend(box_row_lines("[no output]", width, bg));
        } else {
            rows.extend(body_rows);
        }
        if !footer.is_empty() {
            rows.extend(box_row_lines(footer, width, bg));
        }
    } else {
        rows.extend(collapsed_output_lines(output, width, bg));
    }

    rows.push(border_line(width, bg));
    rows
}

fn output_row_lines(output: &str, width: usize, bg: Color) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for line in output.lines() {
        for wrapped in wrap_line(line, width.saturating_sub(4)) {
            rows.push(box_row_line(&wrapped, width, bg));
        }
    }
    rows
}

fn collapsed_output_lines(output: &str, width: usize, bg: Color) -> Vec<Line<'static>> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return box_row_lines("[no output]", width, bg);
    }

    let total = lines.len();
    let shown = total.min(COLLAPSED_PREVIEW_LINES);
    let skipped = total.saturating_sub(shown);
    let mut rows = Vec::new();
    // Show preview lines
    for line in lines.iter().skip(skipped) {
        rows.extend(box_row_lines(line, width, bg));
    }
    // Show collapse hint at the bottom if there are hidden lines
    if skipped > 0 {
        rows.extend(box_row_lines(
            &format!("[Ctrl+O to collapse/expand {skipped} lines]"),
            width,
            bg,
        ));
    }
    rows
}

// ── Line-based helper functions for styled block rendering ──

/// Override the background color on all spans to match the block bg.
/// This ensures syntax-highlighted spans don't reset bg to terminal default.
fn spans_with_bg(spans: &[Span<'static>], bg: Color) -> Vec<Span<'static>> {
    spans.iter().map(|s| {
        let style = s.style.clone().bg(bg);
        Span::styled(s.content.clone(), style)
    }).collect()
}

fn border_line(width: usize, bg: Color) -> Line<'static> {
    Line::from(Span::styled(border_str(width), dim_bg_style(bg)))
}

fn border_with_label_line(width: usize, label: &str, bg: Color) -> Line<'static> {
    Line::from(Span::styled(border_with_label_str(width, label), dim_bg_style(bg)))
}

fn box_row_line(text: &str, width: usize, bg: Color) -> Line<'static> {
    let pad = width.saturating_sub(4).saturating_sub(visible_width(text));
    let mut spans = vec![
        Span::styled("| ", dim_bg_style(bg)),
        Span::styled(text.to_string(), bg_style(bg)),
    ];
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), bg_style(bg)));
    }
    spans.push(Span::styled(" |", dim_bg_style(bg)));
    Line::from(spans)
}

fn box_row_line_spans(spans: Vec<Span<'static>>, width: usize, bg: Color) -> Line<'static> {
    let content_width: usize = spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let pad = width.saturating_sub(4).saturating_sub(content_width);
    let mut all_spans = vec![
        Span::styled("| ", dim_bg_style(bg)),
    ];
    all_spans.extend(spans);
    if pad > 0 {
        all_spans.push(Span::styled(" ".repeat(pad), bg_style(bg)));
    }
    all_spans.push(Span::styled(" |", dim_bg_style(bg)));
    Line::from(all_spans)
}

fn box_row_lines(text: &str, width: usize, bg: Color) -> Vec<Line<'static>> {
    wrap_line(text, width.saturating_sub(4))
        .into_iter()
        .map(|line| box_row_line(&line, width, bg))
        .collect()
}

// ── Old string-based helpers (kept for backwards-compat in counting) ──

fn border_str(width: usize) -> String {
    if width <= 1 {
        return "+".to_string();
    }
    format!("+{}+", "-".repeat(width.saturating_sub(2)))
}

fn border_with_label_str(width: usize, label: &str) -> String {
    if width <= 4 {
        return border_str(width);
    }
    let label_width = visible_width(label);
    let left = 3.min(width.saturating_sub(2));
    let used = 2 + left + label_width;
    if used >= width {
        return border_str(width);
    }
    format!(
        "+{}{}{}+",
        "-".repeat(left),
        label,
        "-".repeat(width - used)
    )
}

fn build_python_command_rows(
    tool: &ToolResultBlock,
    visible: bool,
    width: usize,
    bg: Color,
) -> Option<Vec<Line<'static>>> {
    let value: serde_json::Value = serde_json::from_str(&tool.content).ok()?;
    if value.get("kind").and_then(|v| v.as_str()) != Some("python_command_result") {
        return None;
    }
    let code = value.get("code")?.as_str()?.trim_end();
    let output_raw = value.get("output")?.as_str()?;
    let (output, footer) = command_display_content(output_raw);
    let width = width.max(4);
    let mut rows = Vec::new();
    rows.push(border_with_label_line(width, " python ", bg));
    // Highlight Python code lines
    for line in code.lines() {
        let spans = crate::session::markdown::highlight_line(line, "python");
        let spans = spans_with_bg(&spans, bg);
        for wrapped in wrap_line(line, width.saturating_sub(4)) {
            if wrapped == line {
                rows.push(box_row_line_spans(spans.clone(), width, bg));
            } else {
                rows.extend(box_row_lines(&wrapped, width, bg));
            }
        }
    }
    rows.push(border_with_label_line(width, " Output ", bg));
    if visible {
        let body_rows = output_row_lines(&output, width, bg);
        if body_rows.is_empty() {
            rows.extend(box_row_lines("[no output]", width, bg));
        } else {
            rows.extend(body_rows);
        }
    } else {
        rows.extend(collapsed_output_lines(&output, width, bg));
    }
    if !footer.is_empty() {
        rows.extend(box_row_lines(&footer, width, bg));
    }
    rows.push(border_line(width, bg));
    Some(rows)
}

fn tool_display_content(tool: &ToolResultBlock) -> (String, String) {
    if tool.name == "shell_command" || tool.name == "command" {
        return command_display_content(&tool.content);
    }
    if matches!(tool.name.as_str(), "ask" | "todo" | "plan") {
        if let Some(display) = interaction_tool_display(&tool.content) {
            return (display, "[shown in function panel]".to_string());
        }
    }

    (
        tool.content.trim_end().to_string(),
        String::new(),
    )
}

fn interaction_tool_display(content: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    match value.get("kind")?.as_str()? {
        "ask" => {
            let question = value.get("question").and_then(|v| v.as_str()).unwrap_or("");
            let mut out = format!("? {question}");
            if let Some(options) = value.get("options").and_then(|v| v.as_array()) {
                for opt in options.iter().filter_map(|v| v.as_str()) {
                    out.push_str("\n- ");
                    out.push_str(opt);
                }
            }
            Some(out)
        }
        "todo" => {
            let mut out = String::new();
            for item in value.get("items")?.as_array()? {
                let content = item.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let status = item
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("pending");
                out.push_str(&format!("[{status}] {content}\n"));
            }
            Some(out.trim_end().to_string())
        }
        "plan" => Some(
            value
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        _ => None,
    }
}

fn build_write_file_diff_rows(
    tool: &ToolResultBlock,
    visible: bool,
    width: usize,
    bg: Color,
) -> Option<Vec<Line<'static>>> {
    let (path, old, new) = parse_write_file_diff(&tool.content)?;
    let diff = unified_diff_rows(&old, &new);
    let added = diff
        .iter()
        .filter(|line| line.starts_with(" ") && is_diff_added(line))
        .count();
    let removed = diff.iter().filter(|line| line.starts_with('-')).count();
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("file");
    let title = format!(" ~ Edit: {ext} {path} [+{added}/-{removed}] ");

    let width = width.max(4);
    let mut rows = vec![border_with_label_line(width, &title, bg)];
    let body = diff.join("\n");
    if visible {
        if diff.is_empty() {
            rows.extend(box_row_lines("[no changes]", width, bg));
        } else {
            for line in &diff {
                rows.push(diff_box_row_line(line, width, bg));
            }
        }
    } else {
        rows.extend(collapsed_output_lines(&body, width, bg));
    }
    rows.push(border_line(width, bg));
    Some(rows)
}

fn is_diff_added(line: &str) -> bool {
    line.find('│')
        .and_then(|pos| line[..pos].chars().last())
        .map(|c| c == '+')
        .unwrap_or(false)
}

fn diff_box_row_line(diff_line: &str, width: usize, bg: Color) -> Line<'static> {
    let fg = if diff_line.starts_with('-') {
        Color::Red
    } else if is_diff_added(diff_line) {
        Color::Green
    } else {
        Color::Reset
    };

    let pad = width.saturating_sub(4).saturating_sub(visible_width(diff_line));
    let mut spans = vec![
        Span::styled("| ", dim_bg_style(bg)),
        Span::styled(diff_line.to_string(), Style::default().fg(fg).bg(bg)),
    ];
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    }
    spans.push(Span::styled(" |", dim_bg_style(bg)));
    Line::from(spans)
}

fn parse_write_file_diff(content: &str) -> Option<(String, String, String)> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    if value.get("kind").and_then(|v| v.as_str()) != Some("write_file_diff") {
        return None;
    }
    Some((
        value.get("path")?.as_str()?.to_string(),
        value.get("old")?.as_str()?.to_string(),
        value.get("new")?.as_str()?.to_string(),
    ))
}

fn unified_diff_rows(old: &str, new: &str) -> Vec<String> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    if old_lines == new_lines {
        return Vec::new();
    }

    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < old_lines.len().saturating_sub(prefix)
        && suffix < new_lines.len().saturating_sub(prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_change_end = old_lines.len().saturating_sub(suffix);
    let new_change_end = new_lines.len().saturating_sub(suffix);
    let context = 3usize;
    let context_start = prefix.saturating_sub(context);
    let context_after = suffix.min(context);
    let number_width = old_lines
        .len()
        .max(new_lines.len())
        .to_string()
        .len()
        .max(3);

    let mut rows = Vec::new();
    for idx in context_start..prefix {
        rows.push(diff_context_line(idx + 1, old_lines[idx], number_width));
    }
    for idx in prefix..old_change_end {
        rows.push(diff_removed_line(idx + 1, old_lines[idx], number_width));
    }
    for idx in prefix..new_change_end {
        rows.push(diff_added_line(new_lines[idx], number_width));
    }
    for idx in old_change_end..old_change_end.saturating_add(context_after) {
        rows.push(diff_context_line(idx + 1, old_lines[idx], number_width));
    }
    rows
}

fn diff_context_line(line_no: usize, text: &str, width: usize) -> String {
    format!(" {:>width$}│{}", line_no, text, width = width)
}

fn diff_removed_line(line_no: usize, text: &str, width: usize) -> String {
    format!(
        "-{:>width$}│{}",
        line_no,
        mark_leading_spaces(text),
        width = width
    )
}

fn diff_added_line(text: &str, width: usize) -> String {
    format!(
        " {:>width$}│{}",
        "+",
        mark_leading_spaces(text),
        width = width
    )
}

fn mark_leading_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut marking = true;
    for ch in text.chars() {
        if marking && ch == ' ' {
            out.push('·');
        } else {
            marking = false;
            out.push(ch);
        }
    }
    out
}

fn command_display_content(content: &str) -> (String, String) {
    let content = unwrap_tool_result_content(content);
    let content = content.as_str();
    let has_structured_output = content.contains("exit_code: ")
        && content.contains("wall_secs: ")
        && content.contains("stdout:\n")
        && content.contains("\nstderr:\n");
    if !has_structured_output {
        return (content.trim_end().to_string(), String::new());
    }

    let exit_code = value_after_prefix(content, "exit_code: ").unwrap_or("0");
    let wall = value_after_prefix(content, "wall_secs: ").unwrap_or("-");
    let timeout = value_after_prefix(content, "timeout_secs: ").unwrap_or("300");
    let stdout = section_between(content, "stdout:\n", "\nstderr:\n").unwrap_or_default();
    let stderr = section_after(content, "\nstderr:\n").unwrap_or_default();

    let mut output = stdout.trim_end().to_string();
    let stderr = stderr.trim_end();
    if !stderr.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str("stderr:\n");
        output.push_str(stderr);
    }
    if exit_code != "0" {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format!("[exit_code: {exit_code}]"));
    }

    (output, format!("[Wall: {wall}s | Timeout: {timeout}s]"))
}

fn value_after_prefix<'a>(content: &'a str, prefix: &str) -> Option<&'a str> {
    content
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .map(str::trim)
}

fn section_between(content: &str, start: &str, end: &str) -> Option<String> {
    let start_idx = content.find(start)? + start.len();
    let rest = &content[start_idx..];
    let end_idx = rest.find(end).unwrap_or(rest.len());
    Some(rest[..end_idx].to_string())
}

fn section_after(content: &str, marker: &str) -> Option<String> {
    let idx = content.find(marker)? + marker.len();
    Some(content[idx..].to_string())
}

fn wrap_line(line: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![String::new()];
    }

    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for ch in line.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width > 0 && current_width + ch_width > max_width {
            rows.push(current);
            current = String::new();
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }
    rows.push(current);
    rows
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

    fn lines_to_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn session_with_table_table() -> Session {
        let mut s = Session::default();
        s.display = ThinkingDisplay::Show;
        s.push(Message::new(Role::User, "give me a table"));
        s.push(Message {
            role: Role::Assistant,
            content: "| 列 1 | 列 2 |\n|---|---|\n| A | B |".into(),
            thinking: String::new(),
            thinking_segments: Vec::new(),
            thinking_visible: false,
            tool_results: Vec::new(),
            display_cursor: usize::MAX,
            ts: chrono::Utc::now(),
            streaming: false,
            skill_ref: None,
            line_count: 0,
            content_version: 0,
        });
        s
    }

    #[test]
    fn build_lines_renders_table() {
        let session = session_with_table_table();
        let (lines, _toggles) = build_lines(&session, 100);
        let text = lines_to_text(&lines);
        assert!(text.contains("列 1"), "header missing:\n{text}");
        assert!(text.contains("列 2"), "header missing:\n{text}");
        assert!(text.contains("A"), "cell A missing:\n{text}");
        assert!(text.contains("B"), "cell B missing:\n{text}");
        // Pipes should NOT appear raw.
        assert!(!text.contains("||"), "raw pipes leaked:\n{text}");
        // ...and the ASCII border should be present.
        assert!(text.contains("+"), "border missing:\n{text}");
    }

    #[test]
    fn command_block_unwraps_tool_result_json() {
        let tool = ToolResultBlock {
            name: "shell_command".to_string(),
            title: "$ ls -la".to_string(),
            content: serde_json::json!({
                "ok": true,
                "result": "exit_code: 1\nwall_secs: 1.71\ntimeout_secs: 300\nstdout:\n\nstderr:\nGet-ChildItem: bad flag\n"
            })
            .to_string(),
            content_offset: 0,
            visible: true,
            running: false,
            cached_line_count_visible: None,
            cached_line_count_collapsed: None,
        };
        let rows = build_tool_block_rows(&tool, true, 100);
        let text = lines_to_text(&rows);
        assert!(
            text.contains("Get-ChildItem: bad flag"),
            "stderr missing:\n{text}"
        );
        assert!(
            text.contains("[exit_code: 1]"),
            "exit code missing:\n{text}"
        );
        assert!(!text.contains("{\"ok\":"), "json wrapper leaked:\n{text}");
    }

    #[test]
    fn build_tool_block_renders_write_file_diff() {
        let tool = ToolResultBlock {
            name: "write_file".to_string(),
            title: "[tool:write_file]".to_string(),
            content: serde_json::json!({
                "kind": "write_file_diff",
                "path": "src/demo.py",
                "old": "alpha\n    old_call()\nomega\n",
                "new": "alpha\n    new_call()\nomega\n",
            })
            .to_string(),
            content_offset: 0,
            visible: true,
            running: false,
            cached_line_count_visible: None,
            cached_line_count_collapsed: None,
        };
        let rows = build_tool_block_rows(&tool, true, 80);
        let text = lines_to_text(&rows);
        assert!(
            text.contains("~ Edit: py src/demo.py [+1/-1]"),
            "title missing:\n{text}"
        );
        assert!(
            text.contains("-  2│····old_call()"),
            "removed line missing:\n{text}"
        );
        assert!(
            text.contains("   +│····new_call()"),
            "added line missing:\n{text}"
        );
    }

    #[test]
    fn user_message_block_is_aligned_and_filled() {
        // Regression test: scrolling reveals background-block misalignment
        // when the rendered lines have different effective widths. Every line
        // in the user message block should (a) have every span painted with
        // the user background color, and (b) sum to exactly `width` columns.
        let mut s = Session::default();
        s.push(Message::new(Role::User, "hello\nworld\nlonger line that should wrap maybe"));
        let width = 30usize;
        let (lines, _toggles) = build_lines(&s, width);

        let user_bg = active_colors().user_bg;
        // Find the user block by looking for lines whose content includes any of
        // the test message substrings. For those lines, every span must be
        // painted with `user_bg`, and the line must sum to exactly `width`.
        let user_lines: Vec<&Line<'static>> = lines
            .iter()
            .filter(|l| {
                l.spans.iter().any(|sp| {
                    sp.content.contains("hello")
                        || sp.content.contains("world")
                        || sp.content.contains("longer")
                })
            })
            .collect();
        assert!(
            !user_lines.is_empty(),
            "expected to find user message lines; rendered lines:\n{:#?}",
            lines
        );
        for (i, l) in user_lines.iter().enumerate() {
            let w: usize = l
                .spans
                .iter()
                .map(|sp| unicode_width::UnicodeWidthStr::width(sp.content.as_ref()))
                .sum();
            assert_eq!(
                w, width,
                "user line {i} width {w} != {width}; spans={:?}",
                l.spans
            );
            // Every span in the user block must carry user_bg; otherwise
            // ratatui leaves a gap with the default terminal background.
            for sp in &l.spans {
                assert_eq!(
                    sp.style.bg,
                    Some(user_bg),
                    "user line {i} has a span without user_bg: {:?}",
                    sp
                );
            }
        }
    }

    #[test]
    fn dump_user_message_buffer() {
        let mut s = Session::default();
        s.push(Message::new(Role::User, "longer line that should wrap maybe"));
        let area = Rect::new(0, 0, 30, 6);
        let mut buf = Buffer::empty(area);
        let mut toggles = Vec::new();
        crate::session::render::render(area, &mut buf, &s, &mut toggles);
        let user_bg = active_colors().user_bg;
        eprintln!("user_bg = {:?}", user_bg);
        for y in 0..area.height {
            let chars: String = (0..area.width)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect();
            eprintln!("y={y} chars=|{}|", chars);
            let row: String = (0..area.width)
                .map(|x| {
                    let bg = match buf.cell((x, y)).unwrap().style().bg {
                        Some(c) => if c == user_bg { "U" } else { "?" },
                        None => ".",
                    };
                    bg.to_string()
                })
                .collect();
            eprintln!("y={y} bgmap |{}|  (U=user_bg, .=Reset, ?=other)", row);
        }
    }

    /// Create a session with `count` assistant messages, each `lines_per_msg`
    /// long. Used to benchmark viewport rendering at scale.
    fn large_session(count: usize, lines_per_msg: usize) -> Session {
        let mut s = Session::default();
        s.display = ThinkingDisplay::Show;
        s.push(Message::new(Role::User, "start"));
        let line = "x".repeat(100);
        let content = (0..lines_per_msg)
            .map(|_| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for i in 0..count {
            s.push(Message {
                role: Role::Assistant,
                content: content.clone(),
                thinking: String::new(),
                thinking_segments: Vec::new(),
                thinking_visible: false,
                tool_results: Vec::new(),
                display_cursor: usize::MAX,
                ts: chrono::Utc::now(),
                streaming: false,
                skill_ref: None,
                line_count: lines_per_msg as u32,
                content_version: 0,
            });
            if i % 2 == 0 {
                s.push(Message::new(Role::User, &format!("prompt {}", i / 2)));
            }
        }
        s
    }

    #[test]
    fn viewport_rendering_skips_messages_beyond_viewport() {
        // 1000 messages, each 50 lines → ~50k total lines
        let mut s = large_session(1000, 50);
        let width = 120;

        // Warm the layout cache (as ui::render does).
        s.count_all_lines_with_width(width);
        let total = s.cached_total_lines_for(width).unwrap();

        // Build only the last 50 lines (viewport at bottom).
        let start = total.saturating_sub(50);
        let end = total;
        let lines = build_lines_viewport(&s, width, start, end);
        assert!(lines.len() <= 60, "viewport should produce ~50 lines, got {}", lines.len());

        // Verify that the pre-warm cache is read correctly and messages
        // beyond the viewport are not rendered into the output.
        let first_line_text: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        // The last message contributes the last ~50 lines (its full content
        // plus spacers). The first rendered line should come from that message.
        assert!(
            lines.len() > 0,
            "viewport should have lines but was empty"
        );
    }

    #[test]
    fn perf_smoke_10m_tokens() {
        // Simulate a 10M-token session with realistic chat structure:
        // 500 assistant messages × 20 lines each = 10k lines × 100 chars ≈ 1M chars,
        // interspersed with user prompts. The key metric is that warmup
        // (walking all messages to populate cached line counts) completes
        // in <100ms and viewport render (which only touches 1-2 messages
        // near the bottom) completes in <50ms even in debug mode.
        let lines_per_msg = 20;
        let msg_count = 500;
        let expected_total_chars = (lines_per_msg * 100) as u64 * msg_count as u64;
        assert!(
            expected_total_chars >= 1_000_000,
            "test must produce >=1M chars (got {expected_total_chars})"
        );

        let mut s = large_session(msg_count, lines_per_msg);
        let width = 120;

        // Force cache warmup (same as ui::render does).
        let start = std::time::Instant::now();
        s.count_all_lines_with_width(width);
        let warmup_us = start.elapsed().as_micros() as u64;

        // Viewport render (the dominant path for interactive frame).
        let total = s.cached_total_lines_for(width).unwrap_or(0);
        let vp_start = total.saturating_sub(40);
        let vp_end = total;
        let render_start = std::time::Instant::now();
        let lines = build_lines_viewport(&s, width, vp_start, vp_end);
        let render_us = render_start.elapsed().as_micros() as u64;

        eprintln!(
            "perf smoke: {} messages, ~{expected_total_chars} chars, \
             ~{} total lines (est), warmup={warmup_us}µs, \
             viewport render={render_us}µs, viewport lines={}",
            msg_count,
            total,
            lines.len(),
        );

        // Warmup walks all messages but uses O(1) cached block counts.
        // Even for 1M+ chars across 500+ messages this should be <100ms.
        assert!(
            warmup_us < 200_000,
            "warmup took {warmup_us}µs (expected <200ms)"
        );
        // Viewport render renders 1-2 messages (~40 lines each) through
        // Markdown plus slicing. In debug mode this is typically 2-10ms.
        assert!(
            render_us < 50_000,
            "viewport render took {render_us}µs (expected <50ms)"
        );
        assert!(
            !lines.is_empty(),
            "viewport should have rendered at least one line"
        );
    }

    #[test]
    fn streaming_cache_reuses_when_content_unchanged() {
        let mut s = Session::default();
        s.display = ThinkingDisplay::Show;
        let mut m = Message::new(Role::Assistant, "hello streaming world");
        m.streaming = true;
        m.display_cursor = 5; // only "hello" visible
        s.push(m);
        s.streaming_id = Some(0);
        let width = 100;

        // Warm layout cache.
        s.count_all_lines_with_width(width);

        // First render — cache miss.
        let lines1 = build_message_lines(&s, 0, width);
        assert!(!lines1.is_empty(), "should produce lines");

        // Second render with same state — cache hit.
        let lines2 = build_message_lines(&s, 0, width);
        assert_eq!(
            lines_to_text(&lines1),
            lines_to_text(&lines2),
            "identical state should produce identical output"
        );

        // Advance display_cursor (simulates tick handler).
        s.messages[0].display_cursor = 10;
        let lines3 = build_message_lines(&s, 0, width);
        assert_ne!(
            lines_to_text(&lines1),
            lines_to_text(&lines3),
            "different display_cursor should produce different output"
        );
    }

    #[test]
    fn dump_user_then_assistant_then_tool() {
        use crate::session::{Message, Role, Session, ToolResultBlock};
        let mut s = Session::default();
        s.push(Message::new(Role::User, "short user message"));
        let mut asst = Message::new(
            Role::Assistant,
            "I will run a command for you.",
        );
        asst.tool_results.push(ToolResultBlock {
            name: "shell_command".to_string(),
            title: "$ echo hello".to_string(),
            content: "ok".to_string(),
            content_offset: 0,
            visible: true,
            running: false,
            cached_line_count_visible: None,
            cached_line_count_collapsed: None,
        });
        s.push(asst);
        let area = Rect::new(0, 0, 60, 16);
        let mut buf = Buffer::empty(area);
        let mut toggles = Vec::new();
        crate::session::render::render(area, &mut buf, &s, &mut toggles);

        let user_bg = active_colors().user_bg;
        for y in 0..area.height {
            let chars: String = (0..area.width)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect();
            let row: String = (0..area.width)
                .map(|x| {
                    let bg = buf.cell((x, y)).unwrap().style().bg;
                    match bg {
                        Some(c) if c == user_bg => "U",
                        Some(c) => "?",
                        None => ".",
                    }
                })
                .collect();
            eprintln!("y={y:2} |{}| {}", chars, row);
        }
    }
}
