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

    let (lines, _) = build_lines(session, width);

    let total = lines.len() as u16;
    let max_scroll = total.saturating_sub(inner_h as u16);
    let scroll = session.scroll.min(max_scroll);
    // scroll=n  means "skip n lines from the bottom", so offset_from_top
    // is max_scroll - scroll (clamped to 0).  At scroll=max_scroll the
    // offset is 0 → top of session.  At scroll=0 the offset is max_scroll
    // → bottom of session.
    let offset_from_top = max_scroll.saturating_sub(scroll);
    let start = offset_from_top;
    let end = (offset_from_top + inner_h as u16).min(total);

    tool_toggle_rows.clear();

    let visible: Vec<Line> = if start < end {
        lines[start as usize..end as usize].to_vec()
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
    let p = Paragraph::new(visible);
    p.render(area, buf);
}

/// Toggle label text used by older tests / callers.
pub const THINKING_TOGGLE_COLLAPSED: &str = "[thinking ▸]";
pub const THINKING_TOGGLE_EXPANDED: &str = "[thinking ▾]";
pub const THINKING_END: &str = "[end thinking]";

pub fn build_lines(
    session: &Session,
    width: usize,
) -> (Vec<Line<'static>>, Vec<(usize, usize, usize)>) {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cache = session.line_cache.lock().unwrap();
    if cache.len() < session.messages.len() {
        cache.resize(session.messages.len(), None);
    }
    for (cache_idx, m) in session.messages.iter().enumerate() {
        if !m.streaming {
            if let Some(cached) = &cache[cache_idx] {
                out.extend(cached.iter().cloned());
                continue;
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
            let user_bg = Color::Rgb(224, 247, 250);
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

        if !m.streaming {
            cache[cache_idx] = Some(msg_lines.clone());
        }
        out.extend(msg_lines);
    }
    drop(cache);
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
    let md_lines = crate::session::markdown::render_with_width(&text, width.saturating_sub(3));
    for line in md_lines {
        let mut indented = vec![Span::raw("   ")];
        indented.extend(line.spans.into_iter());
        out.push(Line::from(indented));
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
}
