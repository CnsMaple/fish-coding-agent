use super::blocks::{build_thinking_block_rows, build_tool_block_rows, get_thinking_segments};
use super::message_has_thinking;
use crate::session::{Message, Role, Session, ThinkingSegment, ToolResultBlock};
use crate::theme::active_colors;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

pub fn strip_legacy_markers(s: &str) -> String {
    s.lines()
        .filter(|line| {
            let t = line.trim();
            !(t.starts_with("[tool:") && t.ends_with(']'))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn clamp_char_boundary(s: &str, mut idx: usize) -> usize {
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Render a text segment (content between tool markers) through Markdown.
pub(super) fn render_content_segment(text: &str, width: usize, out: &mut Vec<Line<'static>>) {
    if text.is_empty() {
        return;
    }
    // Fast path: skip the String-allocating strip passes when the
    // text contains no tool-call markers at all. During streaming,
    // the vast majority of content segments have no markers, so this
    // avoids two full String allocations + line scans per call.
    let text: std::borrow::Cow<str> =
        if text.contains("[tool:") || text.contains(">>>") || text.contains("<<<") {
            let stripped = strip_legacy_markers(text);
            let stripped = crate::session::strip_text_tool_calls(&stripped);
            std::borrow::Cow::Owned(stripped)
        } else {
            std::borrow::Cow::Borrowed(text)
        };
    if text.trim().is_empty() {
        return;
    }
    let inner_w = width.saturating_sub(4).max(1);
    let md_lines = crate::session::markdown::render_with_width(&text, inner_w);
    for line in md_lines {
        // Wrap each rendered line to inner_w so the user-block padding
        // can fill the rest of the row. Markdown parsing does not wrap
        // by default; a long unbreakable span would otherwise overflow
        // the viewport and break the background fill.
        if line.width() <= inner_w {
            let pad = (inner_w + 1).saturating_sub(line.width());
            let mut indented = vec![Span::raw("   ")];
            indented.extend(line.spans.into_iter());
            if pad > 0 {
                indented.push(Span::raw(" ".repeat(pad)));
            }
            out.push(Line::from(indented));
        } else {
            // Concatenate all spans into a single string, wrap, then split
            // back into multiple lines preserving the first span's style
            // and emitting the rest as plain.
            let combined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let wrapped_lines = wrap_line(&combined, inner_w);
            for (i, wrapped) in wrapped_lines.into_iter().enumerate() {
                let ww = visible_width(&wrapped);
                if i == 0 {
                    let pad = (inner_w + 1).saturating_sub(ww);
                    out.push(Line::from(vec![
                        Span::raw("   ".to_string()),
                        Span::raw(wrapped),
                        Span::raw(" ".repeat(pad)),
                    ]));
                } else {
                    let pad = (inner_w + 3).saturating_sub(ww);
                    out.push(Line::from(vec![
                        Span::raw(" ".to_string()),
                        Span::raw(wrapped),
                        Span::raw(" ".repeat(pad)),
                    ]));
                }
            }
        }
    }
}

pub fn thinking_block_line_count(
    content: &str,
    visible: bool,
    preview_lines: usize,
    width: usize,
) -> usize {
    if content.is_empty() {
        return 0;
    }
    build_thinking_block_rows(
        content,
        visible,
        preview_lines,
        width,
        active_colors().thinking_done_bg,
        None,
    )
    .len()
}

/// Count total thinking lines across all segments.
pub fn total_thinking_line_count(m: &Message, session: &Session, width: usize) -> usize {
    let show = m.role == Role::Assistant
        && message_has_thinking(m)
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
        total +=
            thinking_block_line_count(&seg.content, visible, session.tool_preview_lines, width);
    }
    total
}

pub fn tool_block_line_count(
    tool: &ToolResultBlock,
    visible: bool,
    preview_lines: usize,
    width: usize,
) -> usize {
    build_tool_block_rows(tool, visible, preview_lines, width).len()
}

/// Count the rendered display lines of a message's `content` field at
/// the given viewport `width`. This is the post-markdown / post-wrap
/// count, NOT the raw `content.split('\n').count()`.
///
/// Why this exists: `Message::line_count` is just the raw newline
/// count, which undercounts whenever the content contains markdown
/// constructs that expand to more display lines (tables, fenced code
/// blocks, indented lists, etc.) or any long line that wraps. Using
/// `line_count` for viewport math made the scroll position land
/// above the true bottom of such messages, so the last rows were
/// hidden behind the input area even when the scrollbar was at the
/// maximum position. This function mirrors the `render_content_segment`
/// path exactly so the count always matches the viewport.
pub fn content_line_count(content: &str, width: usize) -> u32 {
    if content.is_empty() {
        return 0;
    }
    let text = crate::session::strip_text_tool_calls(content);
    if text.trim().is_empty() {
        return 0;
    }
    let inner_w = width.saturating_sub(4).max(1);
    count_md_lines(&text, inner_w)
}

/// Count rendered markdown lines for a text at the given inner width.
/// Does NOT strip tool calls or legacy markers — caller must pre-process.
fn count_md_lines(text: &str, inner_w: usize) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let md_lines = crate::session::markdown::render_with_width(text, inner_w);
    let mut count: u32 = 0;
    for line in &md_lines {
        if line.width() <= inner_w {
            count += 1;
        } else {
            let combined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            count += wrap_line(&combined, inner_w).len() as u32;
        }
    }
    count
}

/// Count content lines matching the **segmented** rendering of
/// `build_message_lines`.  Instead of rendering the full content
/// through markdown (which can disagree with the per-segment rendering
/// when a thinking/tool offset splits a markdown construct such as a
/// table or fenced code block), this function splits the content at
/// the same offsets as `build_message_lines` and counts each segment
/// separately.
///
/// The result is the number of content-only display lines that
/// `build_message_lines` would produce for the text portions of the
/// message (excluding thinking/tool block rows, spacers, user-bg
/// padding, and the leading gap).
pub fn content_line_count_segmented(
    raw: &str,
    width: usize,
    thinking_segments: &[ThinkingSegment],
    tool_results: &[ToolResultBlock],
) -> u32 {
    #[allow(dead_code)]
    enum ItemKind {
        Thinking,
        Tool,
    }
    #[allow(dead_code)]
    struct Item {
        offset: usize,
        kind: ItemKind,
    }

    let mut items: Vec<Item> = Vec::new();
    for seg in thinking_segments {
        let offset = clamp_char_boundary(raw, seg.offset.min(raw.len()));
        items.push(Item {
            offset,
            kind: ItemKind::Thinking,
        });
    }
    for tool in tool_results.iter() {
        let offset = clamp_char_boundary(raw, tool.content_offset.min(raw.len()));
        items.push(Item {
            offset,
            kind: ItemKind::Tool,
        });
    }
    // Sort by offset; stable sort keeps thinking before tools at the
    // same offset, matching `build_message_lines`.
    items.sort_by(|a, b| a.offset.cmp(&b.offset));

    let mut cursor = 0usize;
    let mut total: u32 = 0;

    for item in &items {
        let offset = item.offset;
        if offset < cursor {
            continue;
        }
        if offset > cursor {
            total += count_md_segment(&raw[cursor..offset], width);
            cursor = offset;
        }
    }
    if cursor < raw.len() {
        total += count_md_segment(&raw[cursor..], width);
    }

    total
}

/// Apply the same pre-processing as `render_content_segment` (legacy
/// markers + text tool calls) and return the markdown line count.
/// Uses `render_content_segment` directly so the count is guaranteed
/// to match the actual rendered output — no divergence between the
/// counting path and the rendering path.
pub fn count_md_segment(text: &str, width: usize) -> u32 {
    let mut tmp: Vec<Line<'static>> = Vec::new();
    render_content_segment(text, width, &mut tmp);
    tmp.len() as u32
}

pub(super) fn value_after_prefix<'a>(content: &'a str, prefix: &str) -> Option<&'a str> {
    content
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .map(str::trim)
}

pub(super) fn section_between(content: &str, start: &str, end: &str) -> Option<String> {
    let start_idx = content.find(start)? + start.len();
    let rest = &content[start_idx..];
    let end_idx = rest.find(end).unwrap_or(rest.len());
    Some(rest[..end_idx].to_string())
}

pub(super) fn section_after(content: &str, marker: &str) -> Option<String> {
    let idx = content.find(marker)? + marker.len();
    Some(content[idx..].to_string())
}

pub(super) fn wrap_line(line: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![String::new()];
    }

    // Strip control chars that cause width mismatches between
    // unicode-width v0.1 (our crate, counts them as 0) and v0.2
    // (ratatui's crate, counts them as 1). Without this, a stray
    // \r would make wrap_line think the line is narrower than it
    // actually renders, causing the right border to shift.
    let line: String = line
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect();

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

pub(super) fn truncate_str_to_width(s: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(s) <= max_width {
        return s.to_string();
    }
    let mut result = String::new();
    let mut current_width = 0;
    for ch in s.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + ch_width > max_width {
            break;
        }
        result.push(ch);
        current_width += ch_width;
    }
    result
}

/// helper used by tests / other renderers
pub fn visible_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Strip control characters (except newline) that cause width
/// mismatches between `unicode-width` v0.1 (used by our code,
/// counts them as width 0) and v0.2 (used by ratatui, counts
/// them as width 1). Without this, a stray `\r` in the content
/// would push the right border `|` past the visible area.
pub(super) fn strip_control_chars(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect()
}
