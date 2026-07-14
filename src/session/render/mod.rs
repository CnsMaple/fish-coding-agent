mod blocks;
mod utils;
#[cfg(test)]
mod tests;

pub use blocks::{ask_snapshot_line_count, attachment_block_line_count, get_thinking_segments, skill_block_line_count};
pub use utils::{
    clamp_char_boundary, content_line_count, content_line_count_segmented,
    count_md_segment, strip_legacy_markers, thinking_block_line_count,
    tool_block_line_count, total_thinking_line_count, visible_width,
};
use blocks::{
    build_attachment_block_rows, build_skill_block_rows, build_thinking_block_rows,
    build_tool_block_rows, ensure_gap_before_block, push_block_rows, render_ask_snapshot_message,
};
use utils::render_content_segment;

#[cfg(test)]
use blocks::{
    build_output_block_rows, build_shell_command_rows, box_row_line, diff_box_row_line,
    output_row_lines, DiffLine, DiffLineKind,
};
#[cfg(test)]
use utils::wrap_line;

use super::{Role, Session};
use crate::config::{ThinkingDisplay, ToolResultDisplay};
use crate::theme::active_colors;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

/// LRU cache entry for a fully rendered message. Validity is checked
/// against `Message.content_version` so changing one message does not
/// invalidate cached render output for any other message.
#[derive(Debug)]
pub struct CachedMessageLines {
    pub content_version: u64,
    pub width: u16,
    pub display_cursor: usize,
    /// Byte length of `Message::content` when this entry was cached.
    /// Used as a cheap belt-and-braces guard against stale entries
    /// that survived a missed invalidation: a length mismatch proves
    /// the slot now belongs to a different message.
    pub content_len: usize,
    /// Fully rendered lines, shared via Arc to avoid cloning on
    /// cache hit. The viewport renderer slices the Arc instead of
    /// copying the underlying Vec.
    pub lines: Arc<Vec<Line<'static>>>,
    /// Number of content-only display lines (excluding thinking/tool
    /// block rows, spacers, user-bg padding, leading gap). Written by
    /// `build_message_lines` so `render_cached_content_lines` can
    /// skip the full markdown re-parse that `content_line_count_segmented`
    /// would otherwise do.
    pub content_line_count: u32,
}

pub fn render(
    area: Rect,
    buf: &mut Buffer,
    session: &Session,
    tool_toggle_rows: &mut Vec<(u16, u16, usize, usize)>,
) {
    let inner_h = area.height as usize;
    let width = area.width as usize;
    if width == 0 || inner_h == 0 {
        return;
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
    // Do the viewport math in u32 so a session that overflows u16
    // (10M+ token threads) still scrolls correctly. `session.scroll`
    // is u32 and stores "scroll offset from bottom"; clamp it here
    // against the true u32 max so the offset is derived from the real total.
    let total_u32: u32 = total;
    let max_scroll_u32: u32 = total_u32.saturating_sub(inner_h as u32);
    let scroll_u32: u32 = session.scroll.min(max_scroll_u32);
    let offset_from_top: u32 = max_scroll_u32.saturating_sub(scroll_u32);
    let start: u32 = offset_from_top;
    let end: u32 = (offset_from_top + inner_h as u32).min(total_u32);

    tool_toggle_rows.clear();

    // Viewport-aware: only build lines for messages that intersect
    // [start, end). The vast majority of messages in a 10M-token
    // session live outside the viewport, so this is the dominant
    // win for Phase B.
    let visible: Vec<Line> = if start < end {
        build_lines_viewport(session, width, start, end)
    } else {
        vec![]
    };

    let p = Paragraph::new(visible).style(Style::reset());
    p.render(area, buf);
}

/// Fallback total-line count used when no cache is available. Walks
/// the session and adds `m.line_count` plus rough estimates for
/// thinking/tools. Only invoked in the rare path where the cache
/// hasn't been warmed by the caller.
///
/// Mirrors `Session::compute_total_lines`: no phantom role prefix,
/// plus the per-block trailing blank and leading gap, so the rough
/// estimate tracks the real structure of `build_message_lines`.
/// Inter-message and bottom gaps are added at the session level.
fn count_lines_estimate(session: &Session) -> u32 {
    let mut n: u32 = 0;
    for m in &session.messages {
        let content_lines = read_cached_content_count(m);
        n += content_lines;
        // Attachment blocks: rough estimate.
        if !m.attachments.is_empty() {
            n += attachment_block_line_count(&m.attachments);
        }
        let mut thinking_blocks: u32 = 0;
        if message_has_thinking(m) {
            n += 1; // toggle line
            if m.thinking_visible {
                n += m.thinking.matches('\n').count() as u32 + 1;
            }
            n += 1; // trailing blank after the thinking block
            thinking_blocks = 1;
        }
        let tool_blocks = m.tool_results.len() as u32;
        n += tool_blocks * 2; // rough per-block estimate + 1 trailing blank
        let first_offset = m.thinking_segments.iter().map(|s| s.offset)
            .chain(m.tool_results.iter().map(|t| t.content_offset))
            .min();
        if first_offset.is_some_and(|off| off > 0) && (thinking_blocks > 0 || tool_blocks > 0) {
            n += 1; // leading gap
        }
        if m.role == super::Role::User {
            // Include the `[skill]` marker block rows when present
            // (5-6 rows + 1 trailing blank). The estimate width
            // (120) is the same as `read_cached_content_count` below
            // so this stays in lockstep with the per-message count.
            if let Some(skill_ref) = &m.skill_ref {
                n += skill_block_line_count(skill_ref, 120);
            }
            n += 2; // user-bg padding above and below
        }
    }
    // Inter-message gaps + bottom gap (one per message).
    if !session.messages.is_empty() {
        n += session.messages.len() as u32;
    }
    n
}

/// Read-only content line count for the fallback estimator. Uses a
/// fixed width of 120 (the historical estimate width); the caller
/// doesn't need an exact value, just something in the right
/// ballpark. Will use the per-message cache if it's at the same
/// width, otherwise computes live without writing back.
pub(crate) fn read_cached_content_count(m: &super::Message) -> u32 {
    read_cached_content_count_at(m, 120)
}

/// True if the message has any thinking content (either the legacy
/// `thinking` field or any segment in `thinking_segments`). Use this
/// instead of checking `m.thinking.trim().is_empty()` because
/// `append_thinking_to_last` no longer mutates `m.thinking`; content
/// lives entirely in `thinking_segments` after the per-block
/// non-merging fix.
pub(crate) fn message_has_thinking(m: &super::Message) -> bool {
    if !m.thinking.trim().is_empty() {
        return true;
    }
    m.thinking_segments
        .iter()
        .any(|s| !s.content.trim().is_empty())
}

/// Read-only content line count at a specific width. Used by callers
/// that have `&Message` (not `&mut Message`) and therefore cannot
/// write to the cache — they accept the live-compute cost.
pub(crate) fn read_cached_content_count_at(m: &super::Message, width: u16) -> u32 {
    if let Some(c) = m.cached_content_line_count {
        if c.width == width {
            return c.count;
        }
    }
    if m.content.trim_start().starts_with("---ask---") {
        ask_snapshot_line_count(&m.content, width as usize)
    } else {
        let segments = get_thinking_segments(m);
        content_line_count_segmented(
            &m.content,
            width as usize,
            &segments,
            &m.tool_results,
        )
    }
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
pub fn build_message_lines(
    session: &Session,
    msg_idx: usize,
    width: usize,
) -> Arc<Vec<Line<'static>>> {
    if msg_idx >= session.messages.len() {
        return Arc::new(vec![]);
    }
    let m = &session.messages[msg_idx];

    {
        let lru = session.message_lines_cache.lock().unwrap();
        if let Some(cached) = lru.get(&msg_idx) {
            if cached.content_version == m.content_version
                && cached.width == width as u16
                && cached.display_cursor == m.display_cursor
                && cached.content_len == m.content.len()
            {
                return Arc::clone(&cached.lines);
            }
        }
    }

    // Ask snapshots: assistant messages whose content starts with
    // `---ask---` are the merged-list bodies of a `ChatDone` flush,
    // not raw chat. Render them as a single `+--- Ask ---+` block
    // so concurrent ask calls in one assistant turn collapse into
    // one block instead of N. Also bypass the normal thinking /
    // tool-result pipeline.
    if m.content.trim_start().starts_with("---ask---") {
        let rendered = render_ask_snapshot_message(&m.content, width, m.streaming, m.display_cursor);
        let content_line_count = ask_snapshot_line_count(&m.content, width);
        let lines = Arc::new(rendered);
        let mut lru = session.message_lines_cache.lock().unwrap();
        lru.put(
            msg_idx,
            CachedMessageLines {
                content_version: m.content_version,
                width: width as u16,
                display_cursor: m.display_cursor,
                content_len: m.content.len(),
                lines: Arc::clone(&lines),
                content_line_count,
            },
        );
        return lines;
    }

    let mut msg_lines: Vec<Line<'static>> = Vec::new();
    let mut content_line_count: u32 = 0;
    if let Some(skill_ref) = &m.skill_ref {
        let rows = build_skill_block_rows(skill_ref, width);
        push_block_rows(&mut msg_lines, rows);
        msg_lines.push(Line::from(""));
    }

    // Render image attachments as dim placeholder blocks.
    if !m.attachments.is_empty() {
        ensure_gap_before_block(&mut msg_lines);
        let rows = build_attachment_block_rows(&m.attachments, width);
        push_block_rows(&mut msg_lines, rows);
        msg_lines.push(Line::from(""));
    }

    let raw = if m.streaming {
        m.visible_content()
    } else {
        &m.content
    };

    // Build sorted items (thinking segments + tools) for interleaved rendering
    enum RenderItemKind {
        Thinking {
            content: String,
            /// `true` once a non-thinking content block has begun
            /// after this segment. Closed segments render with the
            /// "done" background color; open segments use the
            /// "streaming" color while the message is still in
            /// flight.
            closed: bool,
            /// Snapshot of `m.tool_results.len()` when this segment
            /// was created. Used by the sort tiebreaker below to
            /// distinguish pre-tool thinking (renders before its
            /// sibling tool block) from post-tool thinking (renders
            /// after it) when both items share the same content
            /// offset.
            tool_results_len_at_open: usize,
            /// Elapsed time for this thinking segment, computed
            /// from `started_at`/`ended_at`. `None` when timing
            /// info is unavailable (e.g. legacy sessions).
            duration: Option<std::time::Duration>,
        },
        Tool(usize), // index into m.tool_results
    }
    struct RenderItem {
        offset: usize,
        kind: RenderItemKind,
    }

    let mut items: Vec<RenderItem> = Vec::new();

    // Add thinking segments (only when display allows)
    if m.role == Role::Assistant {
        let segments = get_thinking_segments(m);
        let has_thinking_content = segments.iter().any(|s| !s.content.trim().is_empty());
        if has_thinking_content && !matches!(session.display, ThinkingDisplay::Hide) {
            for seg in &segments {
                let offset = clamp_char_boundary(raw, seg.offset.min(raw.len()));
                let duration = match (seg.started_at, seg.ended_at) {
                    (Some(start), Some(end)) => {
                        let d = end.signed_duration_since(start);
                        if d.num_milliseconds() >= 0 {
                            Some(std::time::Duration::from_millis(
                                d.num_milliseconds().max(0) as u64,
                            ))
                        } else {
                            None
                        }
                    }
                    (Some(start), None) => {
                        // Still streaming — use elapsed since start
                        let now = chrono::Utc::now();
                        let d = now.signed_duration_since(start);
                        if d.num_milliseconds() >= 0 {
                            Some(std::time::Duration::from_millis(
                                d.num_milliseconds().max(0) as u64,
                            ))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                items.push(RenderItem {
                    offset,
                    kind: RenderItemKind::Thinking {
                        content: seg.content.clone(),
                        closed: seg.closed,
                        tool_results_len_at_open: seg.tool_results_len_at_open,
                        duration,
                    },
                });
            }
        }
    }

    // Add tool results
    for (ti, tool) in m.tool_results.iter().enumerate() {
        // Defensive: skip placeholder blocks that never received any
        // content or streaming input (e.g. duplicate blocks created by
        // stale provider deltas before call-id routing). Rendering an
        // empty box here is what produced the cascade of blank bordered
        // blocks during parallel tool calls.
        if tool.content.is_empty() && tool.streaming_input.is_empty() {
            continue;
        }
        let offset = clamp_char_boundary(raw, tool.content_offset.min(raw.len()));
        // Clamp a tool that was anchored beyond the current content
        // length back to the end of the content so it renders in the
        // right visual order and does not create a bogus gap for
        // content that no longer exists.
        let offset = offset.min(raw.len());
        items.push(RenderItem {
            offset,
            kind: RenderItemKind::Tool(ti),
        });
    }

    // Sort by offset; at the same offset, disambiguate thinking vs.
    // tool with `tool_results_len_at_open` instead of a hard-coded
    // "tools win" rule. The hard-coded rule was wrong for the
    // common "model thinks, then calls a tool" pattern: when the
    // pre-tool thinking segment and the tool block both anchor at
    // the same offset (e.g. content was empty when both happened),
    // the pre-tool thinking should render BEFORE the tool it
    // produced, not after. A segment created when
    // `tool_results.len()` was `tool_results_len_at_open` came
    // before any tool with index `>= tool_results_len_at_open` —
    // so at the same offset, sort such a segment before that tool,
    // and any tool with a smaller index (i.e. one that already
    // existed when the segment opened) before the segment.
    items.sort_by(|a, b| {
        a.offset
            .cmp(&b.offset)
            .then_with(|| match (&a.kind, &b.kind) {
                (
                    RenderItemKind::Tool(ti),
                    RenderItemKind::Thinking {
                        tool_results_len_at_open,
                        ..
                    },
                ) => {
                    if *ti >= *tool_results_len_at_open {
                        // Tool didn't exist yet when the segment
                        // opened → segment is pre-tool → tool after.
                        std::cmp::Ordering::Greater
                    } else {
                        // Tool already existed when the segment
                        // opened → segment is post-tool → tool before.
                        std::cmp::Ordering::Less
                    }
                }
                (
                    RenderItemKind::Thinking {
                        tool_results_len_at_open,
                        ..
                    },
                    RenderItemKind::Tool(ti),
                ) => {
                    if *ti >= *tool_results_len_at_open {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
                }
                _ => std::cmp::Ordering::Equal,
            })
    });

    let mut cursor = 0usize;
    for item in items {
        let offset = item.offset;
        if offset < cursor {
            continue;
        }

        // Render content before this item
        if offset > cursor {
            let before = msg_lines.len();
            render_content_segment(
                &strip_legacy_markers(&raw[cursor..offset]),
                width,
                &mut msg_lines,
            );
            content_line_count += (msg_lines.len() - before) as u32;
            cursor = offset;
        }

        match item.kind {
            RenderItemKind::Thinking {
                content, closed, duration, ..
            } => {
                let visible = match session.display {
                    ThinkingDisplay::Show => m.thinking_visible,
                    ThinkingDisplay::ShowWhileStreaming => m.streaming || m.thinking_visible,
                    _ => false,
                };
                let colors = active_colors();
                let bg = if closed || !m.streaming {
                    colors.thinking_done_bg
                } else {
                    colors.thinking_streaming_bg
                };
                ensure_gap_before_block(&mut msg_lines);
                let rows = build_thinking_block_rows(
                    &content,
                    visible,
                    session.tool_preview_lines,
                    width,
                    bg,
                    duration,
                );
                push_block_rows(&mut msg_lines, rows);
                msg_lines.push(Line::from(""));
            }
            RenderItemKind::Tool(ti) => {
                if let Some(tool) = m.tool_results.get(ti) {
                    if session.tool_display != ToolResultDisplay::Hide {
                        // Same logic as `build_lines_viewport`:
                        // `tool.running` no longer forces expansion
                        // — the preview form is used during streaming
                        // and the pending background colour alone
                        // signals "in flight". The user expands with
                        // a click.
                        let t_vis = match session.tool_display {
                            ToolResultDisplay::Show => tool.visible,
                            ToolResultDisplay::ShowWhileStreaming => m.streaming || tool.visible,
                            _ => false,
                        };
                        ensure_gap_before_block(&mut msg_lines);
                        let rows =
                            build_tool_block_rows(tool, t_vis, session.tool_preview_lines, width);
                        push_block_rows(&mut msg_lines, rows);
                        msg_lines.push(Line::from(""));
                    }
                }
            }
        }
    }
    // Render remaining content
    let before = msg_lines.len();
    render_content_segment(&strip_legacy_markers(&raw[cursor..]), width, &mut msg_lines);
    content_line_count += (msg_lines.len() - before) as u32;

    if m.role == Role::User {
        let user_bg = active_colors().user_bg;
        // Apply background and full-width padding to content lines.
        for line in &mut msg_lines {
            for span in &mut line.spans {
                span.style = span.style.bg(user_bg);
            }
            let content_len: usize = line
                .spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            let pad = width.saturating_sub(content_len);
            if pad > 0 {
                line.spans
                    .push(Span::styled(" ".repeat(pad), Style::default().bg(user_bg)));
            }
        }
        // Blank line with background above content.
        msg_lines.insert(
            0,
            Line::from(Span::styled(
                " ".repeat(width),
                Style::default().bg(user_bg),
            )),
        );
        // Blank line with background below content.
        msg_lines.push(Line::from(Span::styled(
            " ".repeat(width),
            Style::default().bg(user_bg),
        )));
        }

    {
        let mut lru = session.message_lines_cache.lock().unwrap();
        let lines = Arc::new(msg_lines);
        lru.put(
            msg_idx,
            CachedMessageLines {
                content_version: m.content_version,
                width: width as u16,
                display_cursor: m.display_cursor,
                content_len: m.content.len(),
                lines: Arc::clone(&lines),
                content_line_count,
            },
        );
        lines
    }
}

/// Count the number of blank-line gaps that `ensure_gap_before_block`
/// inserts before thinking/tool blocks.  A gap is inserted before the
/// *first* block when content text precedes it (offset > 0), and before
/// each *subsequent* block whose offset differs from the previous
/// block's offset (i.e. content text sits between them).
pub(crate) fn count_block_gaps(
    thinking_segments: &[super::ThinkingSegment],
    tool_results: &[super::ToolResultBlock],
) -> u32 {
    let mut offsets: Vec<usize> = thinking_segments
        .iter()
        .map(|s| s.offset)
        .chain(tool_results.iter().filter(|t| has_renderable_content(t)).map(|t| t.content_offset))
        .collect();
    offsets.sort();
    let mut gaps: u32 = 0;
    let mut prev: Option<usize> = None;
    for &off in &offsets {
        match prev {
            None => {
                if off > 0 {
                    gaps += 1;
                }
            }
            Some(p) => {
                if off > p {
                    gaps += 1;
                }
            }
        }
        prev = Some(off);
    }
    gaps
}

fn has_renderable_content(tool: &super::ToolResultBlock) -> bool {
    !tool.content.is_empty() || !tool.streaming_input.is_empty()
}

/// Build only the lines that intersect the visible viewport.
/// `start_line` and `end_line` are absolute line indices into the
/// full rendered output.
///
/// Gaps between messages and the bottom gap (between session and
/// input/function panel) are inserted here, ONE blank line each.
/// `build_message_lines` no longer emits a per-message final spacer.
pub(super) fn build_lines_viewport(
    session: &Session,
    width: usize,
    start_line: u32,
    end_line: u32,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if session.messages.is_empty() {
        return out;
    }
    // When line_offsets is stale (e.g. cache was invalidated between
    // count_all_lines_with_width and render), fall back to a full
    // build + slice so the viewport is never blank.
    if session.line_offsets.len() <= 1 {
        let (all, _) = build_lines(session, width);
        let start = (start_line as usize).min(all.len());
        let end = (end_line as usize).min(all.len());
        if start < end {
            out.extend(all[start..end].iter().cloned());
        }
        return out;
    }

    let msg_end_line = end_line;

    // Binary search: find the first message that intersects [start_line, end_line).
    // line_offsets[i] = start line of message i; line_offsets[N] = total lines.
    let first_visible = match session.line_offsets[..session.messages.len()]
        .binary_search(&start_line)
    {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };

    for msg_idx in first_visible..session.messages.len() {
        let msg_start = session.line_offsets[msg_idx];
        if msg_start >= msg_end_line {
            break;
        }

        let msg_end = session.line_offsets[msg_idx + 1]; // includes gap

        // Content spans [msg_start, msg_end - 1), gap is at msg_end - 1.
        if msg_end - 1 > start_line && msg_start < msg_end_line {
            let rendered = build_message_lines(session, msg_idx, width);
            let local_start = start_line.saturating_sub(msg_start) as usize;
            let local_end = msg_end_line
                .saturating_sub(msg_start)
                .min(msg_end - 1 - msg_start) as usize;
            let local_end = local_end.min(rendered.len());
            if local_start < local_end {
                out.extend(rendered[local_start..local_end].iter().cloned());
            }
        }

        // Gap line.
        let gap_line = msg_end - 1;
        if gap_line >= start_line && gap_line < msg_end_line {
            out.push(Line::from(""));
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
        if msg_idx > 0 {
            out.push(Line::from("")); // inter-message gap
        }
        let rendered = build_message_lines(session, msg_idx, width);
        out.extend(rendered.iter().cloned());
    }
    if !out.is_empty() {
        out.push(Line::from("")); // bottom gap
    }
    (out, Vec::new())
}
