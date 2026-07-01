use super::{Role, Session, SkillRef, ThinkingSegment, ToolResultBlock};
use crate::config::{ThinkingDisplay, ToolResultDisplay};
use crate::theme::active_colors;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
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
    // Do the viewport math in u32 so a session that overflows u16
    // (10M+ token threads) still scrolls correctly. `session.scroll`
    // is u16 because it stores "scroll offset from bottom" which is
    // bounded by viewport height in practice; clamp it here against
    // the true u32 max so the offset is derived from the real total.
    let total_u32: u32 = total;
    let max_scroll_u32: u32 = total_u32.saturating_sub(inner_h as u32);
    let scroll_u32: u32 = (session.scroll as u32).min(max_scroll_u32);
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
///
/// Mirrors `Session::compute_total_lines`: no phantom role prefix,
/// plus the per-block trailing blank and (when applicable) the
/// leading gap and final spacer, so the rough estimate tracks the
/// real structure of `build_message_lines`.
fn count_lines_estimate(session: &Session) -> u32 {
    let mut n: u32 = 0;
    for m in &session.messages {
        let content_lines = read_cached_content_count(m);
        n += content_lines;
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
        if thinking_blocks > 0 || tool_blocks > 0 {
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
        n += 1; // spacer
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
    content_line_count_segmented(
        &m.content,
        width as usize,
        &m.thinking_segments,
        &m.tool_results,
    )
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
    //
    // `content_len` is a cheap extra guard: if a stale entry survived
    // a `truncate` / `remove` / `clear` (e.g. because the caller
    // forgot to invalidate), a length mismatch will force a rebuild
    // instead of returning the wrong render.
    {
        let lru = session.message_lines_cache.lock().unwrap();
        if let Some(cached) = lru.get(&msg_idx) {
            if cached.content_version == m.content_version
                && cached.width == width as u16
                && cached.display_cursor == m.display_cursor
                && cached.content_len == m.content.len()
            {
                return cached.lines.clone();
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
        return rendered;
    }

    let mut msg_lines: Vec<Line<'static>> = Vec::new();
    if let Some(skill_ref) = &m.skill_ref {
        let rows = build_skill_block_rows(skill_ref, width);
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
        let segments = get_thinking_segments(m);
        let has_thinking_content = segments.iter().any(|s| !s.content.trim().is_empty());
        if has_thinking_content && !matches!(session.display, ThinkingDisplay::Hide) {
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

    // Sort by offset; at the same offset, tools come before
    // thinking. Without this tiebreaker, a thinking segment
    // created after a tool block at the same content position
    // (e.g. the model thinks after seeing a tool result) would
    // be rendered before the already-visible tool block, causing
    // the tool block to jump down visually.
    items.sort_by(|a, b| {
        a.offset
            .cmp(&b.offset)
            .then_with(|| match (&a.kind, &b.kind) {
                (RenderItemKind::Tool(_), RenderItemKind::Thinking(_)) => {
                    std::cmp::Ordering::Less
                }
                (RenderItemKind::Thinking(_), RenderItemKind::Tool(_)) => {
                    std::cmp::Ordering::Greater
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
            render_content_segment(
                &strip_legacy_markers(&raw[cursor..offset]),
                width,
                &mut msg_lines,
            );
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
                let bg = if m.streaming {
                    colors.thinking_streaming_bg
                } else {
                    colors.thinking_done_bg
                };
                ensure_gap_before_block(&mut msg_lines);
                if content.trim().is_empty() {
                    // Empty / whitespace-only content: keep the legacy
                    // single-box placeholder path so the user still
                    // sees a `+--- Thinking ---+ [no thinking content]`
                    // frame rather than a silently dropped block.
                    let rows = build_thinking_block_rows(
                        &content,
                        visible,
                        session.tool_preview_lines,
                        width,
                        bg,
                    );
                    push_block_rows(&mut msg_lines, rows);
                } else {
                    // A single ThinkingSegment whose `content` already
                    // contains paragraph breaks (i.e. two-or-more
                    // consecutive newlines) is rendered as multiple
                    // bordered boxes — one per paragraph — separated
                    // by blank lines. This prevents the model from
                    // streaming its entire chain-of-thought in one
                    // burst and having it collapse into a single
                    // giant `+--- Thinking ---+` box that buries the
                    // interspersed tool results.
                    let mut emitted = false;
                    for_each_thinking_paragraph(&content, |para| {
                        if emitted {
                            msg_lines.push(Line::from(""));
                        }
                        emitted = true;
                        let rows = build_thinking_block_rows(
                            para,
                            visible,
                            session.tool_preview_lines,
                            width,
                            bg,
                        );
                        push_block_rows(&mut msg_lines, rows);
                    });
                }
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
                        // Ctrl+O.
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
    render_content_segment(&strip_legacy_markers(&raw[cursor..]), width, &mut msg_lines);

    // streaming cursor removed — content is immediately visible
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
                content_len: m.content.len(),
                lines: msg_lines.clone(),
            },
        );
    }

    msg_lines
}

/// Build only the lines that intersect the visible viewport.
/// `start_line` and `end_line` are absolute line indices into the
/// full rendered output.
///
/// No session-wide trailing blank is appended here: the visual gap
/// between the session and the input/function panel below is the
/// `final spacer` of the last message (a blank line emitted by
/// `build_message_lines`). Removing the extra blank avoids the
/// double-gap that appeared when both were present.
fn build_lines_viewport(
    session: &Session,
    width: usize,
    start_line: u32,
    end_line: u32,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut global_line: u32 = 0;

    // The last message's `final spacer` (the blank line emitted by
    // `build_message_lines`) is the visual gap between the session and
    // the input block below, so no extra session-wide trailing blank is
    // pushed here.
    let msg_end_line = end_line;

    for msg_idx in 0..session.messages.len() {
        let m = &session.messages[msg_idx];

        // Compute total rendered line count for this message, using the
        // per-block caches populated by `Session::count_all_lines_with_width`.
        // Must match the structure produced by `build_message_lines` exactly:
        //   content (post-markdown) + (thinking rows + 1 trailing blank per
        //   segment) + (tool rows + 1 trailing blank per block) + 1 leading
        //   gap if a block precedes empty content + 1 final spacer
        //   (this final spacer is also the visual gap to the input).
        let content_lines = read_cached_content_count_at(m, width as u16);
        let mut msg_total: u32 = content_lines;
        let mut thinking_blocks: u32 = 0;
        let mut tool_blocks: u32 = 0;

        if m.role == super::Role::Assistant
            && message_has_thinking(m)
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
                msg_total += 1; // trailing blank
                thinking_blocks += 1;
            }
        }
        if session.tool_display != crate::config::ToolResultDisplay::Hide {
            for t in &m.tool_results {
                // `t.running` no longer forces expansion — the
                // preview form is used during streaming so the chat
                // stays readable. The pending background colour
                // (driven by `running`) still signals "in flight".
                let t_vis = match session.tool_display {
                    crate::config::ToolResultDisplay::Show => t.visible,
                    crate::config::ToolResultDisplay::ShowWhileStreaming => {
                        m.streaming || t.visible
                    }
                    _ => false,
                };
                if t_vis {
                    msg_total += t.cached_line_count_visible.unwrap_or(0);
                } else {
                    msg_total += t.cached_line_count_collapsed.unwrap_or(0);
                }
                msg_total += 1; // trailing blank
                tool_blocks += 1;
            }
        }
        if thinking_blocks > 0 || tool_blocks > 0 {
            msg_total += 1; // leading gap
        }
        if m.role == super::Role::User {
            // Include the `[skill]` marker block rows when present
            // (5-6 rows + 1 trailing blank) so `msg_total` matches
            // the actual rendered line count and the slice math
            // below advances `global_line` by the right amount.
            if let Some(skill_ref) = &m.skill_ref {
                msg_total += skill_block_line_count(skill_ref, width);
            }
            msg_total += 2; // user-bg padding above and below
        }
        msg_total += 1; // final spacer

        let msg_end = global_line + msg_total;

        // Does this message intersect the viewport (excluding the
        // reserved trailing-blank slot)?
        if msg_end > start_line && global_line < msg_end_line {
            // Full render this message (maybe from cache).
            let rendered = build_message_lines(session, msg_idx, width);
            // Compute the slice of this message's lines that overlap
            // the message portion of the viewport.
            let local_start = start_line.saturating_sub(global_line) as usize;
            let local_end = msg_end_line
                .saturating_sub(global_line)
                .min(rendered.len() as u32) as usize;
            if local_start < local_end {
                out.extend(rendered[local_start..local_end].iter().cloned());
            }
        }

        global_line = msg_end;
        if global_line >= msg_end_line {
            break;
        }
    }

    // The `final spacer` emitted by `build_message_lines` is the visual
    // gap between the session and the input block below. With the
    // segmented line-count fix (`content_line_count_segmented`) the
    // viewport boundary calculation is now consistent with the actual
    // rendered output, so the last line of a properly-sized viewport is
    // always this blank spacer.
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
            let combined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            for wrapped in wrap_line(&combined, inner_w) {
                out.push(Line::from(vec![
                    Span::raw("   ".to_string()),
                    Span::raw(wrapped),
                ]));
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
    let bg = active_colors().thinking_done_bg;
    let mut total = 0usize;
    let mut emitted = false;
    for_each_thinking_paragraph(content, |para| {
        if emitted {
            total += 1; // blank line between consecutive paragraph boxes
        }
        emitted = true;
        total += build_thinking_block_rows(para, visible, preview_lines, width, bg).len();
    });
    if !emitted {
        // Content was non-empty but had no non-whitespace paragraphs
        // (e.g. all whitespace). The renderer takes the legacy
        // single-box placeholder path for this case, so the count
        // must match that — fall back to the original single-box
        // calculation to keep the viewport math consistent.
        return build_thinking_block_rows(content, visible, preview_lines, width, bg).len();
    }
    total
}

/// Iterate the non-empty paragraphs of a thinking segment. A paragraph
/// is a maximal run of text delimited by 2+ consecutive newlines
/// (paragraph break in chain-of-thought output). Whitespace-only
/// paragraphs are skipped. Used by both the renderer and the line
/// counter so a single `ThinkingSegment` whose `content` already
/// contains paragraph breaks renders as multiple bordered boxes
/// instead of one giant block.
fn for_each_thinking_paragraph(content: &str, mut f: impl FnMut(&str)) {
    let content = content.trim();
    if content.is_empty() {
        return;
    }
    let bytes = content.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'\n' && bytes[i + 1] == b'\n' {
            if i > start {
                f(&content[start..i]);
            }
            while i < bytes.len() && bytes[i] == b'\n' {
                i += 1;
            }
            start = i;
        } else {
            i += 1;
        }
    }
    if start < bytes.len() {
        f(&content[start..]);
    }
}

/// Count total thinking lines across all segments.
pub fn total_thinking_line_count(m: &super::Message, session: &Session, width: usize) -> usize {
    let show = m.role == super::Role::Assistant
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
    let inner_w = width.saturating_sub(3).max(1);
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

    let inner_w = width.saturating_sub(3).max(1);
    let mut cursor = 0usize;
    let mut total: u32 = 0;

    for item in &items {
        let offset = item.offset;
        if offset < cursor {
            continue;
        }
        if offset > cursor {
            total += count_md_segment(&raw[cursor..offset], inner_w);
            cursor = offset;
        }
    }
    if cursor < raw.len() {
        total += count_md_segment(&raw[cursor..], inner_w);
    }

    total
}

/// Apply the same pre-processing as `render_content_segment` (legacy
/// markers + text tool calls) and return the markdown line count.
fn count_md_segment(text: &str, inner_w: usize) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let text = strip_legacy_markers(text);
    let text = crate::session::strip_text_tool_calls(&text);
    if text.trim().is_empty() {
        return 0;
    }
    count_md_lines(&text, inner_w)
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
            closed: false,
            tool_results_len_at_open: 0,
            cached_line_count_expanded: None,
            cached_line_count_collapsed: None,
        }];
    }
    vec![]
}

fn build_thinking_block_rows(
    content: &str,
    visible: bool,
    preview_lines: usize,
    width: usize,
    bg: Color,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let mut rows = Vec::new();
    rows.push(border_with_label_line(width, " Thinking ", bg));
    if visible {
        let body_rows = output_row_lines(content.trim_end(), width, bg);
        if body_rows.is_empty() {
            rows.extend(box_row_lines("[no thinking content]", width, bg));
        } else {
            rows.extend(body_rows);
        }
    } else {
        let (preview, skipped) =
            collapsed_output_lines(content.trim_end(), preview_lines, width, bg);
        rows.extend(preview);
        if skipped > 0 {
            rows.push(ctrl_o_hint_line(skipped, width, bg));
        }
    }
    rows.push(border_line(width, bg));
    rows
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
    rows.extend(box_row_lines(
        &format!("context: {}", skill.context_path),
        width,
        bg,
    ));
    rows.push(border_line(width, bg));
    rows
}

/// Count the rendered display lines of a `[skill]` marker block at the
/// given viewport width, including the trailing blank line that
/// `build_message_lines` pushes after the block.
///
/// This mirrors `build_skill_block_rows` exactly — any change to one
/// must be reflected in the other. The block is:
///   1. top border
///   2. `[skill]`
///   3. `name: <name>`
///   4. `args: <args>` (only when args is non-empty)
///   5. `context: <path>`
///   6. bottom border
///   7. trailing blank line (pushed by `build_message_lines`)
///
/// Used by the per-message line counters (`compute_total_lines`,
/// `lines_before`, `count_lines_estimate`, `build_lines_viewport`,
/// and the `ui` toggle-row walk) so the viewport math matches the
/// actual rendered output. Without this, a user message with
/// `skill_ref` was undercounted by 5-6 rows and the bottom of long
/// skill bodies was hidden behind the input area.
pub fn skill_block_line_count(skill: &SkillRef, _width: usize) -> u32 {
    let mut rows = 2u32; // top + bottom borders
    rows += 1; // "[skill]"
    rows += 1; // "name: ..."
    if skill
        .args
        .as_deref()
        .map(|a| !a.trim().is_empty())
        .unwrap_or(false)
    {
        rows += 1; // "args: ..."
    }
    rows += 1; // "context: ..."
    rows += 1; // trailing blank after the block
    rows
}

fn build_tool_block_rows(
    tool: &ToolResultBlock,
    visible: bool,
    preview_lines: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let (bg, fg) = block_colors_for_tool(tool);

    let mut rows: Vec<Line<'static>> = if tool.name == "write_file" {
        if let Some(r) = build_write_file_diff_rows(tool, visible, preview_lines, width, bg) {
            r
        } else {
            return vec![];
        }
    } else if tool.name == "python_command" {
        if let Some(r) = build_python_command_rows(tool, visible, preview_lines, width, bg) {
            r
        } else {
            return vec![];
        }
    } else if tool.name == "ask" {
        // Individual ask tool calls do NOT render as independent
        // blocks. The `App::flush_ask_snapshot` method collapses
        // all ask calls from one assistant turn into a single
        // `+--- Ask ---+` block in the session — that is the only
        // ask block the user sees per turn.
        vec![]
    } else {
        let (output, footer) = tool_display_content(tool);
        let title_highlighted = tool.name == "shell_command" || tool.name == "command";
        if title_highlighted {
            build_shell_command_rows(
                &tool.title,
                &output,
                &footer,
                visible,
                preview_lines,
                width,
                bg,
            )
        } else {
            build_output_block_rows(
                &tool.title,
                " Output ",
                &output,
                &footer,
                visible,
                preview_lines,
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
    preview_lines: usize,
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
        let (preview, skipped) = collapsed_output_lines(output, preview_lines, width, bg);
        rows.extend(preview);
        match (skipped, footer.is_empty()) {
            (n, false) if n > 0 => {
                // Ctrl+O hint on the left, footer on the right — same row.
                rows.push(box_row_line_two(
                    &format!("[Ctrl+O to collapse/expand {n} lines]"),
                    footer,
                    width,
                    bg,
                ));
            }
            (0, false) => {
                rows.extend(box_row_lines(footer, width, bg));
            }
            (n, true) if n > 0 => {
                rows.push(ctrl_o_hint_line(n, width, bg));
            }
            _ => {}
        }
    }

    rows.push(border_line(width, bg));
    rows
}

#[allow(clippy::too_many_arguments)]
fn build_output_block_rows(
    title: &str,
    label: &str,
    output: &str,
    footer: &str,
    visible: bool,
    preview_lines: usize,
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
        let (preview, skipped) = collapsed_output_lines(output, preview_lines, width, bg);
        rows.extend(preview);
        if skipped > 0 {
            rows.push(ctrl_o_hint_line(skipped, width, bg));
        }
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

/// Render the last `preview_lines` lines of `output` as a collapsed
/// preview block. Returns the rendered rows *plus* the number of
/// hidden lines so callers can build their own Ctrl+O hint layout
/// (single full-width line, or combined with a footer on one row).
fn collapsed_output_lines(
    output: &str,
    preview_lines: usize,
    width: usize,
    bg: Color,
) -> (Vec<Line<'static>>, usize) {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return (box_row_lines("[no output]", width, bg), 0);
    }

    let total = lines.len();
    let shown = total.min(preview_lines);
    let skipped = total.saturating_sub(shown);
    let mut rows = Vec::new();
    // Show preview lines
    for line in lines.iter().skip(skipped) {
        rows.extend(box_row_lines(line, width, bg));
    }
    (rows, skipped)
}

/// Single full-width Ctrl+O hint line for collapsed blocks that
/// don't pair the hint with a footer.
fn ctrl_o_hint_line(skipped: usize, width: usize, bg: Color) -> Line<'static> {
    let line = format!("[Ctrl+O to collapse/expand {skipped} lines]");
    box_row_line(&line, width, bg)
}

/// One row inside a tool box with a left chunk (typically the
/// Ctrl+O hint) and a right chunk (typically the timing footer).
/// The middle is filled with the box background so it still looks
/// like a `box_row_line`. When the chunks would overflow the
/// available inner width, both are shown full-width stacked on
/// separate rows by the caller.
fn box_row_line_two(left: &str, right: &str, width: usize, bg: Color) -> Line<'static> {
    let inner_w = width.saturating_sub(4);
    let left_w = visible_width(left);
    let right_w = visible_width(right);
    let pad = inner_w.saturating_sub(left_w).saturating_sub(right_w);
    let mut spans = vec![
        Span::styled("| ", dim_bg_style(bg)),
        Span::styled(left.to_string(), bg_style(bg)),
    ];
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), bg_style(bg)));
    }
    spans.push(Span::styled(right.to_string(), bg_style(bg)));
    spans.push(Span::styled(" |", dim_bg_style(bg)));
    Line::from(spans)
}

// ── Line-based helper functions for styled block rendering ──

/// Override the background color on all spans to match the block bg.
/// This ensures syntax-highlighted spans don't reset bg to terminal default.
fn spans_with_bg(spans: &[Span<'static>], bg: Color) -> Vec<Span<'static>> {
    spans
        .iter()
        .map(|s| {
            let style = s.style.bg(bg);
            Span::styled(s.content.clone(), style)
        })
        .collect()
}

fn border_line(width: usize, bg: Color) -> Line<'static> {
    Line::from(Span::styled(border_str(width), dim_bg_style(bg)))
}

fn border_with_label_line(width: usize, label: &str, bg: Color) -> Line<'static> {
    Line::from(Span::styled(
        border_with_label_str(width, label),
        dim_bg_style(bg),
    ))
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
    let mut all_spans = vec![Span::styled("| ", dim_bg_style(bg))];
    all_spans.extend(spans);
    if pad > 0 {
        all_spans.push(Span::styled(" ".repeat(pad), bg_style(bg)));
    }
    all_spans.push(Span::styled(" |", dim_bg_style(bg)));
    Line::from(all_spans)
}

/// Render an ask-snapshot message (content starts with `---ask---`)
/// as a single `+--- Ask ---+` block. One block per assistant turn,
/// regardless of how many ask tool calls the model emitted in
/// parallel. Each line is wrapped and clipped to the panel width.
fn render_ask_snapshot_message(
    content: &str,
    width: usize,
    _streaming: bool,
    _display_cursor: usize,
) -> Vec<Line<'static>> {
    let width = width.max(8);
    let colors = active_colors();
    let bg = colors.tool_success_bg;
    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(border_with_label_line(width, " Ask ", bg));
    // Strip the leading `---ask---` header line (it just signals the
    // snapshot; the border title already says Ask).
    let body = content
        .lines()
        .skip_while(|l| l.trim_start().starts_with("---ask---"))
        .collect::<Vec<_>>()
        .join("\n");
    for line in body.lines() {
        let wrapped = wrap_line(line, width.saturating_sub(4));
        for w in wrapped {
            out.push(box_row_line(&w, width, bg));
        }
    }
    out.push(border_line(width, bg));
    out
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
    preview_lines: usize,
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
        let (preview, skipped) = collapsed_output_lines(&output, preview_lines, width, bg);
        rows.extend(preview);
        if skipped > 0 {
            rows.push(ctrl_o_hint_line(skipped, width, bg));
        }
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
    if tool.name == "plan" {
        if let Some((body, footer)) = plan_tool_display(&tool.content) {
            return (body, footer);
        }
    }
    (tool.content.trim_end().to_string(), String::new())
}

/// Render a `plan` tool result in the session. The plan body is shown
/// directly so the user can read it without opening a sidebar tab;
/// the sidebar still surfaces the approve/reject actions.
fn plan_tool_display(content: &str) -> Option<(String, String)> {
    // Tool results come back wrapped in `{"ok":true,"result":"…"}`;
    // unwrap first so we can read the inner JSON the tool itself
    // emitted ({"kind":"plan",…}).
    let inner = unwrap_tool_result_content(content);
    let value: serde_json::Value = serde_json::from_str(&inner).ok()?;
    if value.get("kind").and_then(|v| v.as_str()) != Some("plan") {
        return None;
    }
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("Plan")
        .trim();
    let body = value
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let rendered = if title.is_empty() || title.eq_ignore_ascii_case("plan") {
        body
    } else {
        format!("# {title}\n\n{body}")
    };
    let status = value
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("pending");
    let footer = match status {
        "approved" => "approved — proceeding in build mode".to_string(),
        "rejected" => "rejected — awaiting a revised plan".to_string(),
        _ => "↳ approve / reject in the plan tab".to_string(),
    };
    Some((rendered, footer))
}

fn build_write_file_diff_rows(
    tool: &ToolResultBlock,
    visible: bool,
    preview_lines: usize,
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
        let (preview, skipped) = collapsed_output_lines(&body, preview_lines, width, bg);
        rows.extend(preview);
        if skipped > 0 {
            rows.push(ctrl_o_hint_line(skipped, width, bg));
        }
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

    let pad = width
        .saturating_sub(4)
        .saturating_sub(visible_width(diff_line));
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
    for (idx, line) in old_lines
        .iter()
        .enumerate()
        .take(prefix)
        .skip(context_start)
    {
        rows.push(diff_context_line(idx + 1, line, number_width));
    }
    for (idx, line) in old_lines
        .iter()
        .enumerate()
        .take(old_change_end)
        .skip(prefix)
    {
        rows.push(diff_removed_line(idx + 1, line, number_width));
    }
    for line in new_lines.iter().take(new_change_end).skip(prefix) {
        rows.push(diff_added_line(line, number_width));
    }
    for (idx, line) in old_lines
        .iter()
        .enumerate()
        .take(old_change_end.saturating_add(context_after))
        .skip(old_change_end)
    {
        rows.push(diff_context_line(idx + 1, line, number_width));
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
mod content_line_count_tests {
    //! Regression tests for the content-line-count fix. The bug: a
    //! message with a markdown table (or fenced code block, or wrapped
    //! line) has `Message::line_count` equal to the raw newline count
    //! of its source, which is strictly less than the actual number
    //! of display lines. Using that for viewport math hid the bottom
    //! of such messages behind the input area even when the scrollbar
    //! was at the maximum position. `content_line_count` and the
    //! per-message `cached_content_line_count` cache fix this.

    use super::*;
    use crate::session::{Message, Role, Session};

    #[test]
    fn raw_newline_count_undercounts_markdown_table() {
        // Source has 5 newlines → `line_count` = 6, but the table
        // expands to more display lines (top border, header,
        // separator, 3 data rows, bottom border = 7, minus the blank
        // header that gets a leading newline so net ~6 rendered
        // lines plus the leading intro line). The exact post-markdown
        // count is the one we care about.
        let content = "Here you go:\n\n\
                       | 类型 | 名称 |\n\
                       | --- | --- |\n\
                       | 📁 目录 | src |\n\
                       | 📄 文件 | .gitignore |\n\
                       | 📄 文件 | Cargo.toml |";
        let raw = content.matches('\n').count() as u32 + 1;
        let rendered = content_line_count(content, 80);
        assert!(
            rendered > raw,
            "rendered={rendered} should exceed raw line_count={raw} for table content"
        );
    }

    #[test]
    fn fenced_code_block_inflates_rendered_count() {
        // A long body line inside the code block wraps when the
        // viewport is narrow, while the raw newline count sees it as
        // a single line.  This is the simplest case where the
        // rendered count strictly exceeds the raw count for a fenced
        // code block.
        let long_line = "x".repeat(200);
        let content = format!("before\n```\n{long_line}\n```\nafter");
        let raw = content.matches('\n').count() as u32 + 1;
        let rendered = content_line_count(&content, 40);
        assert!(
            rendered > raw,
            "rendered={rendered} should exceed raw line_count={raw} for wrapped code block"
        );
    }

    #[test]
    fn count_total_lines_reflects_markdown_expansion() {
        // The actual bug repro: a session with a user message and an
        // assistant message containing a table. The total rendered
        // line count must be ≥ the sum of raw `line_count`s.
        let mut s = Session::default();
        s.push(Message::new(
            Role::User,
            "give me a table of the current directory",
        ));
        let mut asst = Message::new(
            Role::Assistant,
            "Here you go:\n\n\
             | 类型 | 名称 | 大小 | 修改时间 |\n\
             | --- | --- | --- | --- |\n\
             | 📁 目录 | src | — | 2026/6/26 15:03 |\n\
             | 📄 文件 | .gitignore | 53 B | 2026/6/19 14:58 |\n\
             | 📄 文件 | Cargo.toml | 1,312 B | 2026/6/25 13:05 |",
        );
        asst.thinking_visible = false;
        s.push(asst);

        let width = 80u16;
        let total = s.count_all_lines_with_width(width as usize);

        let raw_sum: u32 = s.messages.iter().map(|m| m.line_count).sum();
        // Total must be strictly greater than the raw sum (4: role
        // prefix + spacer for each of 2 messages + assistant's table
        // expansion).
        assert!(
            total > raw_sum,
            "total={total} should be > raw_sum={raw_sum} (table expansion undercounted)"
        );

        // Per-message cache should be populated after the warmup.
        let asst = &s.messages[1];
        assert!(
            asst.cached_content_line_count.is_some(),
            "assistant message should have a populated content cache after warmup"
        );
    }

    #[test]
    fn cache_is_width_aware() {
        // The cache is keyed by width; the first width-miss recomputes
        // and the second call at the same width is a hit.
        let mut s = Session::default();
        s.push(Message::new(
            Role::Assistant,
            "| h1 | h2 |\n| --- | --- |\n| a | b |",
        ));
        s.count_all_lines_with_width(80);
        let cached_at_80 = s.messages[0].cached_content_line_count;
        assert_eq!(cached_at_80.map(|c| c.width), Some(80));

        s.count_all_lines_with_width(120);
        let cached_at_120 = s.messages[0].cached_content_line_count;
        assert_eq!(
            cached_at_120.map(|c| c.width),
            Some(120),
            "width change should invalidate and recompute"
        );
    }

    #[test]
    fn content_change_invalidates_cache() {
        // `Message::new` leaves the cache as `None`. The first
        // `count_all_lines_with_width` populates it. A subsequent
        // mutation (simulated here by direct field write + a call to
        // `invalidate_layout_cache`) must reset it so the next read
        // recomputes against the new content.
        let mut s = Session::default();
        let m = Message::new(Role::Assistant, "| a | b |\n| --- | --- |");
        s.push(m);
        s.count_all_lines_with_width(80);
        assert!(s.messages[0].cached_content_line_count.is_some());

        // Simulate a content mutation by hand and invalidate.
        s.messages[0].content = "totally different content\nwith new lines".to_string();
        s.messages[0].line_count = 2;
        s.messages[0].cached_content_line_count = None;
        s.invalidate_layout_cache();

        let total = s.count_all_lines_with_width(80);
        let rendered = content_line_count(&s.messages[0].content, 80);
        // The recomputed cache count must reflect the new content,
        // not the old cached value.
        assert_eq!(
            s.messages[0].cached_content_line_count.map(|c| c.count),
            Some(rendered)
        );
        // And the total must include the new content's rendered lines.
        assert!(total > 0);
    }

    #[test]
    fn empty_content_returns_zero() {
        assert_eq!(content_line_count("", 80), 0);
        assert_eq!(content_line_count("   \n  \t  \n", 80), 0);
    }

    #[test]
    fn segmented_count_matches_build_message_lines_with_table_split() {
        // Regression: when a thinking/tool offset splits a markdown
        // table, `content_line_count_segmented` must produce the same
        // count of content lines as `build_message_lines` produces.
        // The old `content_line_count` (full-content) counted table
        // borders/rows that the split segments no longer render,
        // causing a mismatch in viewport total vs actual output.
        let width = 80usize;
        let table = "| A | B |\n| --- | --- |\n| X | Y |\n| Z | W |";
        // Capture text AFTER the first row to simulate a thinking
        // segment whose offset falls inside the table.
        let mut s = crate::session::Session::default();
        let mut asst = crate::session::Message::new(
            crate::session::Role::Assistant,
            format!("text\n\n{table}"),
        );
        // Thinking segment at an offset that splits the table
        // (inside the header area, after "text\n\n| A | B |").
        asst.thinking_segments.push(crate::session::ThinkingSegment {
            offset: "text\n\n| A | B |".len(),
            content: "thinking content".to_string(),
            closed: false,
            tool_results_len_at_open: 0,
            cached_line_count_expanded: None,
            cached_line_count_collapsed: None,
        });
        asst.thinking_visible = true;
        s.push(asst);

        s.display = crate::config::ThinkingDisplay::Show;
        s.tool_preview_lines = 10;
        s.count_all_lines_with_width(width);

        // Verify: the content line count from segmented counting
        // matches what build_message_lines actually renders.
        let rendered = crate::session::render::build_message_lines(&s, 0, width);
        let rendered_content_count = rendered.len() as u32;

        // Re-compute the msg_total components (content + thinking
        // blocks + trailing blanks + leading gap + spacer) to isolate
        // just the content portion.
        let msg = &s.messages[0];
        let seg_count = content_line_count_segmented(
            &msg.content,
            width,
            &msg.thinking_segments,
            &msg.tool_results,
        );

        // The rendered message has:
        //   content lines (seg_count) +
        //   thinking block rows (for 1 segment, expanded) +
        //   1 trailing blank after thinking +
        //   1 leading gap +
        //   1 final spacer
        // We verify that seg_count matches by subtracting the
        // known overhead from the total.
        let think_lines = thinking_block_line_count("thinking content", true, 10, width) as u32;
        let overhead = think_lines + 1 + 1 + 1; // thinking rows + trailing blank + leading gap + spacer
        assert_eq!(
            seg_count + overhead,
            rendered_content_count,
            "segmented content count ({seg_count}) + overhead ({overhead}) = {} \
             should match rendered total ({rendered_content_count}). \
             Full render:\n{}",
            seg_count + overhead,
            rendered
                .iter()
                .map(|l| l
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>())
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
}

#[cfg(test)]
mod tool_block_count_tests {
    //! Regression tests for the tool-block / thinking-block line-count
    //! fix. The bug: `compute_total_lines` (and its siblings) never
    //! accounted for the blank line that `build_message_lines` pushes
    //! after every thinking or tool block, and still added 1 for a
    //! phantom "role prefix" line that is never rendered. For
    //! messages with one or more blocks the count was off by 1 per
    //! block — typically cutting the bottom border of a long
    //! `write_file` diff (or the last `Wall: ...` row of a long shell
    //! command) off the viewport.

    use super::*;
    use crate::session::{Message, Role, Session, ToolResultBlock};

    fn lines_to_text(lines: &[Line]) -> String {
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

    fn make_write_file_tool() -> ToolResultBlock {
        // A small but valid write_file_diff payload so the bottom
        // border is part of the rendered block.
        ToolResultBlock {
            name: "write_file".to_string(),
            title: "[tool:write_file]".to_string(),
            content: serde_json::json!({
                "kind": "write_file_diff",
                "path": "src/demo.py",
                "old": "alpha\nold_call()\nomega\n",
                "new": "alpha\nnew_call()\nomega\n",
            })
            .to_string(),
            content_offset: 0,
            visible: true,
            running: false,
            cached_line_count_visible: None,
            cached_line_count_collapsed: None,
        }
    }

    fn make_shell_tool() -> ToolResultBlock {
        ToolResultBlock {
            name: "shell_command".to_string(),
            title: "$ echo hi".to_string(),
            content: serde_json::json!({
                "ok": true,
                "result": "exit_code: 0\nwall_secs: 0.01\ntimeout_secs: 300\nstdout:\nhi\n\nstderr:\n"
            })
            .to_string(),
            content_offset: 0,
            visible: true,
            running: false,
            cached_line_count_visible: None,
            cached_line_count_collapsed: None,
        }
    }

    fn session_with_tool(tool: ToolResultBlock, with_content: bool) -> Session {
        let mut s = Session {
            display: ThinkingDisplay::Show,
            ..Session::default()
        };
        s.push(Message::new(Role::User, "do it"));
        let mut asst = if with_content {
            Message::new(
                Role::Assistant,
                "I'll handle that. Here is the result:\n\nbody text",
            )
        } else {
            Message::new(Role::Assistant, "")
        };
        asst.tool_results.push(tool);
        s.push(asst);
        s
    }

    fn count_all(s: &mut Session, width: u16) -> u32 {
        s.count_all_lines_with_width(width as usize)
    }

    fn lines_for_msg(s: &Session, msg_idx: usize, width: usize) -> Vec<Line<'static>> {
        build_message_lines(s, msg_idx, width)
    }

    /// The exact total returned by `compute_total_lines` must equal
    /// the sum of lines `build_message_lines` produces for each
    /// message (each message already includes its own `final spacer`,
    /// and there is no session-wide trailing blank).
    #[test]
    fn tool_block_count_matches_rendered_no_content() {
        let mut s = session_with_tool(make_write_file_tool(), false);
        let width = 80u16;
        let total = count_all(&mut s, width);
        let user_lines = lines_for_msg(&s, 0, width as usize).len() as u32;
        let asst_lines = lines_for_msg(&s, 1, width as usize).len() as u32;
        let expected = user_lines + asst_lines;
        assert_eq!(
            total, expected,
            "total={total} but user={user_lines} + asst={asst_lines} = {expected}"
        );
    }

    #[test]
    fn tool_block_count_matches_rendered_with_content() {
        let mut s = session_with_tool(make_write_file_tool(), true);
        let width = 80u16;
        let total = count_all(&mut s, width);
        let user_lines = lines_for_msg(&s, 0, width as usize).len() as u32;
        let asst_lines = lines_for_msg(&s, 1, width as usize).len() as u32;
        let expected = user_lines + asst_lines;
        assert_eq!(total, expected);
    }

    #[test]
    fn two_tool_blocks_count_matches_rendered() {
        let mut s = session_with_tool(make_write_file_tool(), false);
        s.messages[1].tool_results.push(make_shell_tool());
        let width = 80u16;
        let total = count_all(&mut s, width);
        let asst_lines = lines_for_msg(&s, 1, width as usize).len() as u32;
        let user_lines = lines_for_msg(&s, 0, width as usize).len() as u32;
        let expected = user_lines + asst_lines;
        assert_eq!(total, expected);
    }

    #[test]
    fn thinking_plus_tool_count_matches_rendered() {
        let mut s = session_with_tool(make_write_file_tool(), false);
        // Add a thinking segment so the assistant has both kinds of
        // blocks.
        s.messages[1].thinking = "let me think about this".to_string();
        s.messages[1].thinking_segments = vec![crate::session::ThinkingSegment {
            offset: 0,
            content: "let me think about this".to_string(),
            closed: false,
            tool_results_len_at_open: 0,
            cached_line_count_expanded: None,
            cached_line_count_collapsed: None,
        }];
        s.messages[1].thinking_visible = true;
        let width = 80u16;
        let total = count_all(&mut s, width);
        let asst_lines = lines_for_msg(&s, 1, width as usize).len() as u32;
        let user_lines = lines_for_msg(&s, 0, width as usize).len() as u32;
        let expected = user_lines + asst_lines;
        assert_eq!(total, expected);
    }

    /// The bug, narrowed to a single assertion: the very last line
    /// emitted by `build_message_lines` for an assistant message that
    /// contains a tool block must be the block's bottom border
    /// (`+---…---+`), and the viewport slice built by
    /// `build_lines_viewport` at the bottom of the session must
    /// include that border.
    #[test]
    fn bottom_border_line_is_in_viewport() {
        let mut s = session_with_tool(make_write_file_tool(), false);
        let width: usize = 80;
        // Warm the layout cache and force a render so the per-block
        // counts are populated.
        let total = count_all(&mut s, width as u16) as usize;
        let asst_lines = lines_for_msg(&s, 1, width).len();
        let user_lines = lines_for_msg(&s, 0, width).len();
        // The viewport for the very last `inner_h` lines of the
        // session must include the tool block's bottom border.
        let inner_h = asst_lines + user_lines + 1; // big enough to show everything
        let start = total.saturating_sub(inner_h);
        let end = total;
        let rendered = build_lines_viewport(&s, width, start as u32, end as u32);
        let text = lines_to_text(&rendered);
        let last_text_line = text
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        assert!(
            last_text_line.starts_with('+') && last_text_line.contains("---"),
            "last visible line should be the tool block's bottom border, got: {last_text_line:?}"
        );
    }

    /// Regression test for the "trailing gap clipped" bug. The blank
    /// line that visually separates the chat from the input/function
    /// panel is now the LAST message's `final spacer` (a blank line
    /// emitted by `build_message_lines`); it must still be the LAST
    /// line of `build_lines_viewport`'s output even when the total
    /// session height exceeds the viewport. Previously, the
    /// session-wide trailing blank was the source of the gap, and
    /// Paragraph clipped it when the rendered output had `inner_h + 1`
    /// lines.
    #[test]
    fn trailing_gap_line_is_always_last() {
        // Build a session with enough message content to overflow a
        // 5-row viewport.
        let mut s = Session::default();
        s.push(Message::new(Role::User, "go"));
        let asst_content = (0..30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        s.push(Message::new(Role::Assistant, asst_content));
        let width: usize = 80;

        // Warm caches.
        let total = count_all(&mut s, width as u16) as usize;

        // Force the viewport to be SMALLER than the total: the
        // last rendered line MUST be the final-spacer blank.
        let inner_h = 5usize;
        let start = total.saturating_sub(inner_h);
        let end = total;
        let rendered = build_lines_viewport(&s, width, start as u32, end as u32);
        assert_eq!(
            rendered.len(),
            inner_h,
            "viewport overflow case must render exactly inner_h lines, got {}",
            rendered.len()
        );
        // The very last line must be an empty blank.
        let last = rendered.last().expect("rendered non-empty");
        assert!(
            last.spans.iter().all(|s| s.content.is_empty()),
            "last rendered line should be the trailing blank, got: {last:?}"
        );

        // And the count from `compute_total_lines` must match the
        // actual viewport-rendered line count when the viewport is
        // big enough: `inner_h = total` fits everything, last line
        // is still the final-spacer blank.
        let full_rendered = build_lines_viewport(&s, width, 0, total as u32);
        assert_eq!(
            full_rendered.len(),
            total,
            "full-viewport render must produce `total` lines (got {})",
            full_rendered.len()
        );
        assert!(
            full_rendered
                .last()
                .unwrap()
                .spans
                .iter()
                .all(|s| s.content.is_empty()),
            "final spacer must be the very last line in full-viewport mode too"
        );
    }

    /// Regression test for the "thinking fragmented into one box per
    /// delta" problem. The model streams thinking in many small SSE
    /// deltas, and each one used to become its own Thinking box in
    /// the rendered chat. The fix: `append_thinking_to_last` only
    /// opens a new segment when the previous one was closed by a
    /// `begin_thinking_segment` (i.e. a non-thinking content block
    /// started in between). When the model emits consecutive
    /// thinking deltas at the same content offset, they should
    /// collapse into a single continuous Thinking box.
    #[test]
    fn thinking_deltas_at_same_offset_merge_into_one_segment() {
        let mut s = Session::default();
        s.push(Message::new(Role::User, "do it"));
        let mut asst = Message::new(Role::Assistant, "");
        // Three consecutive thinking deltas with no intervening
        // text or tool_use → one open segment, three pushes that
        // all land in the same segment.
        asst.streaming = true;
        asst.thinking_visible = true;
        s.push(asst);
        s.streaming_id = Some(1);
        s.append_thinking_to_last("first thought ");
        s.append_thinking_to_last("second thought ");
        s.append_thinking_to_last("third thought");
        // No `begin_thinking_segment` between deltas — the model
        // emitted a single thinking content block.
        let asst = &s.messages[1];
        assert_eq!(
            asst.thinking_segments.len(),
            1,
            "three consecutive deltas should land in a single segment, got {}",
            asst.thinking_segments.len()
        );
        let combined = asst.thinking_segments[0].content.clone();
        assert!(
            combined.contains("first thought ")
                && combined.contains("second thought ")
                && combined.contains("third thought"),
            "merged segment should contain all three deltas in order, got: {combined:?}"
        );
        // Render and verify exactly one Thinking box (single top
        // border, single bottom border).
        let width = 80;
        let rendered = build_message_lines(&s, 1, width);
        let text = lines_to_text(&rendered);
        let thinking_count = text.matches("Thinking").count();
        assert_eq!(
            thinking_count, 1,
            "expected exactly 1 Thinking box, got {thinking_count}. Rendered:\n{text}"
        );
    }

    /// A `begin_thinking_segment` (signalling a new content block)
    /// between two thinking deltas must force the next delta into a
    /// fresh segment, so the two "phases" of thinking render as two
    /// separate boxes.
    #[test]
    fn begin_thinking_segment_opens_a_new_segment() {
        let mut s = Session::default();
        s.push(Message::new(Role::User, "do it"));
        let asst = Message::new(Role::Assistant, "");
        s.push(asst);
        s.streaming_id = Some(1);
        s.append_thinking_to_last("phase one ");
        s.begin_thinking_segment();
        s.append_thinking_to_last("phase two");
        let asst = &s.messages[1];
        assert_eq!(
            asst.thinking_segments.len(),
            2,
            "begin_thinking_segment should split deltas into separate segments"
        );
        // `begin_thinking_segment` closed the previous segment so
        // the next delta opens a fresh one.
        assert!(
            asst.thinking_segments[0].closed,
            "older segment should be closed by begin_thinking_segment"
        );
        assert!(
            !asst.thinking_segments[1].closed,
            "newly-opened segment must stay open for further deltas"
        );
        assert_eq!(asst.thinking_segments[0].content, "phase one ");
        assert_eq!(asst.thinking_segments[1].content, "phase two");
    }

    /// At the same offset, the tool block appears BEFORE the thinking
    /// block. This matches the user's visual expectation: when a tool
    /// result arrives first and the model subsequently thinks about
    /// the result, the thinking block should not be inserted before
    /// the already-visible tool block.
    #[test]
    fn tool_block_appears_before_thinking_at_same_offset() {
        let mut s = Session::default();
        s.push(Message::new(Role::User, "do it"));
        let mut asst = Message::new(Role::Assistant, "");
        asst.thinking_segments = vec![crate::session::ThinkingSegment {
            offset: 0,
            content: "plan".to_string(),
            closed: false,
            tool_results_len_at_open: 0,
            cached_line_count_expanded: None,
            cached_line_count_collapsed: None,
        }];
        asst.thinking_visible = true;
        asst.tool_results.push(make_write_file_tool());
        s.push(asst);
        let width = 80;
        let rendered = build_message_lines(&s, 1, width);
        let text = lines_to_text(&rendered);
        let tool_idx = text
            .find("Edit:")
            .or_else(|| text.find("Output"))
            .expect("tool block missing");
        let think_idx = text.find("Thinking").expect("Thinking block missing");
        assert!(
            tool_idx < think_idx,
            "Tool block must appear before the thinking block at the same offset, but tool at {tool_idx} came after thinking at {think_idx}.\nRendered:\n{text}"
        );
    }

    /// `begin_thinking_segment` should drop an in-flight empty
    /// segment so the next `append_thinking_to_last` lands in a
    /// fresh block rather than the just-opened-but-unused one.
    #[test]
    fn begin_thinking_segment_drops_empty_inflight_segment() {
        let mut s = Session::default();
        s.push(Message::new(Role::User, "go"));
        let mut asst = Message::new(Role::Assistant, "");
        asst.streaming = true;
        s.push(asst.clone());
        s.streaming_id = Some(1);
        // Simulate: a content_block_start fired before any delta
        // arrived, leaving an empty in-flight segment.
        asst.thinking_segments
            .push(crate::session::ThinkingSegment {
                offset: 0,
                content: String::new(),
                closed: false,
                tool_results_len_at_open: 0,
                cached_line_count_expanded: None,
                cached_line_count_collapsed: None,
            });
        s.messages[1] = asst;
        assert_eq!(s.messages[1].thinking_segments.len(), 1);
        s.begin_thinking_segment();
        assert_eq!(
            s.messages[1].thinking_segments.len(),
            0,
            "begin_thinking_segment should drop the in-flight empty segment"
        );
    }

    /// `append_thinking_to_last` must auto-close the in-flight segment
    /// once a tool call is appended to the message — so reasoning
    /// deltas that flank a tool call land in distinct segments and
    /// therefore distinct rendered boxes, even on OpenAI-style
    /// providers that never fire a `ContentBlockStart` for tool calls.
    #[test]
    fn append_thinking_splits_segment_when_tool_result_arrives() {
        let mut s = Session::default();
        s.push(Message::new(Role::User, "do it"));
        let mut asst = Message::new(Role::Assistant, "");
        asst.streaming = true;
        s.push(asst);
        s.streaming_id = Some(1);

        // Pre-tool reasoning: "Let me run the tool first."
        s.append_thinking_to_last("Let me run the tool first.");
        assert_eq!(s.messages[1].thinking_segments.len(), 1);
        assert!(!s.messages[1].thinking_segments[0].closed);

        // A tool result arrives between the two reasoning bursts.
        s.messages[1]
            .tool_results
            .push(crate::session::ToolResultBlock {
                name: "bash".to_string(),
                title: "Bash".to_string(),
                content: "ok".to_string(),
                content_offset: 0,
                visible: true,
                running: false,
                cached_line_count_visible: None,
                cached_line_count_collapsed: None,
            });

        // Post-tool reasoning must land in a NEW segment, not extend
        // the pre-tool one.
        s.append_thinking_to_last("Good, that worked.");
        assert_eq!(
            s.messages[1].thinking_segments.len(),
            2,
            "tool-call insertion should have auto-closed the first segment"
        );
        assert!(s.messages[1].thinking_segments[0].closed);
        assert!(!s.messages[1].thinking_segments[1].closed);
        assert_eq!(s.messages[1].thinking_segments[0].content, "Let me run the tool first.");
        assert_eq!(s.messages[1].thinking_segments[1].content, "Good, that worked.");
    }

    /// The renderer's `for_each_thinking_paragraph` must split on
    /// 2+ consecutive newlines and skip whitespace-only paragraphs
    /// so multi-paragraph chain-of-thought renders as multiple
    /// bordered boxes instead of one giant block.
    #[test]
    fn for_each_thinking_paragraph_splits_on_double_newlines() {
        fn paragraphs(s: &str) -> Vec<String> {
            let mut out = Vec::new();
            for_each_thinking_paragraph(s, |p| out.push(p.to_string()));
            out
        }
        assert_eq!(paragraphs("single paragraph"), vec!["single paragraph"]);
        assert_eq!(
            paragraphs("first\n\nsecond"),
            vec!["first", "second"]
        );
        assert_eq!(
            paragraphs("a\n\n\n\nb"),
            vec!["a", "b"],
            "runs of 2+ newlines collapse to a single paragraph break"
        );
        assert_eq!(
            paragraphs("\n\nleading\n\nmiddle\n\ntrailing\n\n"),
            vec!["leading", "middle", "trailing"]
        );
        assert!(paragraphs("   ").is_empty(), "whitespace-only yields no paragraphs");
        assert!(paragraphs("\n\n\n\n").is_empty(), "only newlines yields no paragraphs");
        assert!(paragraphs("").is_empty());
    }

    /// A `ThinkingSegment` whose `content` already contains paragraph
    /// breaks must render as multiple `+--- Thinking ---+` boxes,
    /// each with its own border, instead of one giant block.
    #[test]
    fn multi_paragraph_thinking_renders_multiple_boxes() {
        let mut s = Session::default();
        s.push(Message::new(Role::User, "go"));
        let mut asst = Message::new(Role::Assistant, "");
        asst.thinking_segments = vec![crate::session::ThinkingSegment {
            offset: 0,
            content: "First paragraph of reasoning.\n\nSecond paragraph, also reasoning.\n\nThird."
                .to_string(),
            closed: false,
            tool_results_len_at_open: 0,
            cached_line_count_expanded: None,
            cached_line_count_collapsed: None,
        }];
        asst.thinking_visible = true;
        s.push(asst);
        s.display = crate::config::ThinkingDisplay::Show;
        s.tool_preview_lines = 10;

        let lines = build_message_lines(&s, 1, 80);
        let text = lines_to_text(&lines);
        let label_count = text.matches("+--- Thinking").count();
        assert_eq!(
            label_count, 3,
            "expected three Thinking boxes (one per paragraph), got {label_count}.\nRendered:\n{text}"
        );
    }

    /// `thinking_block_line_count` must account for the blank line
    /// between consecutive paragraph boxes — otherwise the cached
    /// line counts drift away from `build_message_lines`'s actual
    /// output and the viewport math falls out of sync.
    #[test]
    fn thinking_line_count_includes_blanks_between_paragraphs() {
        let single = thinking_block_line_count("just one paragraph", true, 10, 80);
        let multi = thinking_block_line_count(
            "first paragraph\n\nsecond paragraph\n\nthird paragraph",
            true,
            10,
            80,
        );
        // Multi = single*3 + 2 inter-paragraph blanks.
        assert_eq!(
            multi,
            single * 3 + 2,
            "multi-paragraph count must include 1 blank between each pair"
        );
    }
}

#[cfg(test)]
mod skill_block_count_tests {
    //! Regression tests for the `[skill]` marker block line-count fix.
    //!
    //! The bug: `compute_total_lines` (and the matching
    //! `lines_before`, `count_lines_estimate`, `build_lines_viewport`,
    //! and `ui` toggle-row walk) never counted the 5-6 rows of the
    //! `[skill]` marker block that `build_message_lines` renders for
    //! any user message carrying `skill_ref`. A user message with a
    //! long skill body therefore reported a `total` that was 5-6
    //! rows short of the actual rendered output. The viewport
    //! scrolled accordingly, hiding the bottom of the skill body
    //! (typically the bullet list under `## Constraints`) until the
    //! assistant started streaming extra content that pushed the
    //! viewport back into range.
    //!
    //! These tests assert that `count_all_lines_with_width` returns
    //! the same total as the sum of `build_message_lines` line
    //! counts for both shapes (with and without `args`).

    use super::*;
    use crate::session::{Message, Role, Session, SkillRef};

    fn user_with_skill(args: Option<&str>) -> Message {
        let mut msg = Message::new(
            Role::User,
            "# Commit and Push All Changes\n\n\
             Step 1: run the thing.\n\n\
             ## Constraints\n\n\
             - The commit message must be in English.\n\
             - Always commit all changes.\n",
        );
        msg.skill_ref = Some(SkillRef {
            name: "commit-and-push-all".to_string(),
            context_path: "C:/Users/me/.agents/skills/commit-and-push-all/SKILL.md"
                .to_string(),
            args: args.map(|s| s.to_string()),
        });
        msg
    }

    #[test]
    fn skill_block_count_matches_rendered_user_message() {
        let mut s = Session::default();
        s.push(user_with_skill(None));
        s.push(Message::new(Role::Assistant, ""));

        let width = 80usize;
        let total = s.count_all_lines_with_width(width) as usize;
        let user_lines = build_message_lines(&s, 0, width).len();
        let asst_lines = build_message_lines(&s, 1, width).len();
        let expected = user_lines + asst_lines;
        assert_eq!(
            total, expected,
            "compute_total_lines returned {total} but actual rendered lines = \
             user({user_lines}) + asst({asst_lines}) = {expected}"
        );
    }

    #[test]
    fn skill_block_count_with_args_matches_rendered() {
        // Same as above but with non-empty `args` so the block has 6
        // rows instead of 5.
        let mut s = Session::default();
        s.push(user_with_skill(Some("extra instruction")));
        s.push(Message::new(Role::Assistant, ""));

        let width = 80usize;
        let total = s.count_all_lines_with_width(width) as usize;
        let user_lines = build_message_lines(&s, 0, width).len();
        let asst_lines = build_message_lines(&s, 1, width).len();
        assert_eq!(total, user_lines + asst_lines);
    }

    #[test]
    fn skill_block_line_count_matches_build_skill_block_rows() {
        // The count helper must match the actual builder PLUS the
        // trailing blank line that `build_message_lines` pushes
        // after the block. Tested at a few widths to be sure the
        // count is width-independent for the current row structure
        // (top/bottom borders, [skill], name, optional args,
        // context, plus the trailing blank).
        for width in [40usize, 80, 130, 200] {
            let skill = SkillRef {
                name: "demo".to_string(),
                context_path: "C:/path/to/SKILL.md".to_string(),
                args: None,
            };
            let built = build_skill_block_rows(&skill, width).len() as u32;
            let counted = skill_block_line_count(&skill, width);
            assert_eq!(
                built + 1,
                counted,
                "width={width}: build_skill_block_rows produced {built} rows + 1 trailing \
                 blank = {}, but skill_block_line_count returned {counted}",
                built + 1
            );

            let skill_with_args = SkillRef {
                args: Some("extra".to_string()),
                ..skill
            };
            let built_args = build_skill_block_rows(&skill_with_args, width).len() as u32;
            let counted_args = skill_block_line_count(&skill_with_args, width);
            assert_eq!(
                built_args + 1,
                counted_args,
                "width={width} (with args): build_skill_block_rows produced {built_args} \
                 rows + 1 trailing blank = {}, but skill_block_line_count returned \
                 {counted_args}",
                built_args + 1
            );
        }
    }

    #[test]
    fn lines_before_accounts_for_skill_block() {
        // `lines_before` must also count the skill block rows.
        // Without the fix, the undercount shifts the scroll target
        // computed by `jump_to_message` and `timeline`.
        let mut s = Session::default();
        s.push(user_with_skill(None));
        s.push(Message::new(Role::Assistant, ""));

        // Warm the per-message content cache that lines_before relies on.
        let _ = s.count_all_lines_with_width(120);
        let n = s.lines_before(1);
        // lines_before(1) should equal the line count of the user
        // message as rendered at width 120.
        let user_lines = build_message_lines(&s, 0, 120).len() as u32;
        assert_eq!(
            n, user_lines,
            "lines_before(1) = {n} but the user message renders as {user_lines} lines"
        );
    }
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
        let mut s = Session {
            display: ThinkingDisplay::Show,
            ..Session::default()
        };
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
            cached_content_line_count: None,
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
        let rows = build_tool_block_rows(&tool, true, 10, 100);
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

    /// Individual ask tool calls do NOT render as independent
    /// blocks — they are consumed by the snapshot mechanism.
    #[test]
    fn ask_individual_tool_block_is_empty() {
        let tool = ToolResultBlock {
            name: "ask".to_string(),
            title: "Ask".to_string(),
            content: serde_json::json!({
                "ok": true,
                "result": "{\"kind\":\"ask\",\"question\":\"theme?\",\"options\":[\"dark\",\"light\"]}"
            })
            .to_string(),
            content_offset: 0,
            visible: true,
            running: false,
            cached_line_count_visible: None,
            cached_line_count_collapsed: None,
        };
        let rows = build_tool_block_rows(&tool, true, 10, 100);
        assert!(rows.is_empty(), "ask tool block must be empty, got {rows:?}");
    }

    /// The snapshot message (pushed by `flush_ask_snapshot`) must
    /// render as a single `+--- Ask ---+` block containing the
    /// merged-list body.
    #[test]
    fn ask_snapshot_block_renders_merged_list() {
        let body = concat!(
            "---ask---\n",
            "q1: 你希望使用什么主题?\n",
            "   - 深色\n",
            "   - 浅色\n",
            "   - 跟随系统\n",
            "q2: 你偏好什么语言?\n",
            "   - 中文\n",
        );
        let lines =
            render_ask_snapshot_message(body, 60, false, 0);
        let text = lines_to_text(&lines);
        assert!(text.contains("Ask"), "missing header:\n{text}");
        assert!(text.contains("q1:"), "missing q1:\n{text}");
        assert!(text.contains("深色"), "missing option:\n{text}");
        assert!(text.contains("q2:"), "missing q2:\n{text}");
        assert!(text.contains("中文"), "missing option:\n{text}");
    }

    /// The plan tool's session block must unwrap the outer
    /// `{"ok":true,"result":"…"}` envelope and read the inner
    /// `kind:plan` payload.
    #[test]
    fn plan_block_unwraps_result_envelope() {
        let tool = ToolResultBlock {
            name: "plan".to_string(),
            title: "Plan: test".to_string(),
            content: serde_json::json!({
                "ok": true,
                "result": "{\"kind\":\"plan\",\"title\":\"test\",\"content\":\"# hello\\n\\nbody\"}"
            })
            .to_string(),
            content_offset: 0,
            visible: true,
            running: false,
            cached_line_count_visible: None,
            cached_line_count_collapsed: None,
        };
        let rows = build_tool_block_rows(&tool, true, 10, 100);
        let text = lines_to_text(&rows);
        assert!(text.contains("hello"), "body missing:\n{text}");
        assert!(text.contains("body"), "body missing:\n{text}");
        assert!(!text.contains("{\"ok\":"), "json envelope leaked:\n{text}");
        assert!(!text.contains("\"kind\":\"plan\""), "raw inner JSON leaked:\n{text}");
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
        let rows = build_tool_block_rows(&tool, true, 10, 80);
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
        s.push(Message::new(
            Role::User,
            "hello\nworld\nlonger line that should wrap maybe",
        ));
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
        s.push(Message::new(
            Role::User,
            "longer line that should wrap maybe",
        ));
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
                        Some(c) => {
                            if c == user_bg {
                                "U"
                            } else {
                                "?"
                            }
                        }
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
        let mut s = Session {
            display: ThinkingDisplay::Show,
            ..Session::default()
        };
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
                cached_content_line_count: None,
                content_version: 0,
            });
            if i % 2 == 0 {
                s.push(Message::new(Role::User, format!("prompt {}", i / 2)));
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
        assert!(
            lines.len() <= 60,
            "viewport should produce ~50 lines, got {}",
            lines.len()
        );

        // Verify that the pre-warm cache is read correctly and messages
        // beyond the viewport are not rendered into the output.
        // The last message contributes the last ~50 lines (its full content
        // plus spacers). The first rendered line should come from that message.
        assert!(
            !lines.is_empty(),
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
        let mut s = Session {
            display: ThinkingDisplay::Show,
            ..Session::default()
        };
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
        let mut asst = Message::new(Role::Assistant, "I will run a command for you.");
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
                        Some(_) => "?",
                        None => ".",
                    }
                })
                .collect();
            eprintln!("y={y:2} |{}| {}", chars, row);
        }
    }

    /// Diagnostic for the bottom-cut-off bug: a long assistant
    /// message with a markdown table + bullet list. Render the
    /// message and dump the buffer so we can see exactly which
    /// lines the renderer produces vs. what the count says.
    #[test]
    fn dump_assistant_table_with_bullets() {
        use crate::session::{Message, Role, Session};
        let mut s = Session::default();
        s.push(Message::new(
            Role::User,
            "all steps done. summarize the commit",
        ));
        let content = "所有步骤已成功完成。 ✅\n\n\
                       执行总结\n\n\
                       | 步骤 | 命令 | 结果 |\n\
                       | --- | --- | --- |\n\
                       | 1 | git status | 3个文件已修改 |\n\
                       | 2 | git add . | 暂存成功 |\n\
                       | 3 | Conventional Commit 构造 | fix(session): align viewport |\n\
                       | 4 | git commit -m \"...\" | 提交成功, hash cc8f35e |\n\
                       | 5 | git push | 推送成功 |\n\n\
                       Commit Message 说明\n\n\
                       - Type: fix — 修复 bug (viewport 末尾 1~N 行被截断)\n\
                       - Scope: session — 影响 session 模块的 line-count 计算\n\
                       - Description: 简短说明核心改动 (让 viewport 行数与 build_message_lines 输出一致)\n\
                       - Body: 详细说明原因 (遗漏 per-block trailing blank、多了 phantom role prefix) 、新规则、测试覆盖";
        let asst = Message::new(Role::Assistant, content);
        s.push(asst);

        let width: u16 = 130;
        let total = s.count_all_lines_with_width(width as usize);
        let asst_lines = crate::session::render::build_message_lines(&s, 1, width as usize);
        let user_lines = crate::session::render::build_message_lines(&s, 0, width as usize);
        eprintln!(
            "counted total={total}, asst rendered={}, user rendered={}, asst+user = {}",
            asst_lines.len(),
            user_lines.len(),
            asst_lines.len() + user_lines.len()
        );
        assert_eq!(
            total as usize,
            asst_lines.len() + user_lines.len(),
            "viewport line count must match the rendered output line for line"
        );
    }

    // --- Regression tests for the "long user message renders only
    //     half until AI replies" bug. The fix lives in three places:
    //     1. `Session::clear` must drop the per-message render LRU.
    //     2. `Session::invalidate_message_cache_from` must drop LRU
    //        entries whose slot was shifted by a `truncate` / `remove`.
    //     3. The LRU hit must also compare `content.len()` so a
    //        forgotten invalidation cannot return a stale render.

    #[test]
    fn clear_drops_message_render_lru() {
        use crate::session::Message;
        let mut s = Session::default();
        s.push(Message::new(Role::User, "first session message"));
        s.push(Message::new(Role::Assistant, "first session reply"));

        // Warm the LRU by rendering both messages.
        let _ = build_message_lines(&s, 0, 80);
        let _ = build_message_lines(&s, 1, 80);
        {
            let lru = s.message_lines_cache.lock().unwrap();
            assert_eq!(lru.len(), 2, "LRU should hold entries for both messages");
        }

        // Start a new session. The LRU must be wiped so a brand-new
        // message at index 0 cannot hit the old render.
        s.clear();
        let lru = s.message_lines_cache.lock().unwrap();
        assert_eq!(lru.len(), 0, "clear() must drop the per-message LRU");
    }

    #[test]
    fn invalidate_from_drops_shifted_slots() {
        use crate::session::Message;
        let mut s = Session::default();
        s.push(Message::new(Role::User, "msg 0"));
        s.push(Message::new(Role::Assistant, "msg 1"));
        s.push(Message::new(Role::User, "msg 2"));
        s.push(Message::new(Role::Assistant, "msg 3"));

        for i in 0..4 {
            let _ = build_message_lines(&s, i, 80);
        }
        assert_eq!(s.message_lines_cache.lock().unwrap().len(), 4);

        // Simulate `/retry` truncating at index 2: the user wants
        // the last user message + everything after it gone. Slots 2
        // and 3 will be reused by the retried prompt, so their
        // cached renders must be dropped.
        s.invalidate_message_cache_from(2);
        let lru = s.message_lines_cache.lock().unwrap();
        assert_eq!(lru.len(), 2, "only slots 0 and 1 should remain cached");
        assert!(lru.contains(&0));
        assert!(lru.contains(&1));
        assert!(!lru.contains(&2));
        assert!(!lru.contains(&3));
    }

    #[test]
    fn lru_check_rejects_stale_length() {
        // Even if a caller forgets to invalidate the LRU after a
        // truncate, a `content.len()` mismatch must force a rebuild
        // instead of returning the wrong render.
        use crate::session::Message;
        let mut s = Session::default();
        s.push(Message::new(Role::User, "original long content"));
        let _ = build_message_lines(&s, 0, 80);
        // Sanity: the LRU has one entry after rendering.
        assert_eq!(s.message_lines_cache.lock().unwrap().len(), 1);

        // Simulate a forgotten invalidation: mutate the message in
        // place WITHOUT bumping `content_version` and WITHOUT
        // clearing the LRU. The only thing that changed is
        // `content` (and therefore `content.len()`).
        let new_content = "x".to_string();
        let new_len = new_content.len();
        s.messages[0].content = new_content;
        s.messages[0].line_count = 1;
        s.messages[0].cached_content_line_count = None;
        // Intentionally leave content_version and display_cursor as
        // they were — a real regression would let them collide.
        s.messages[0].display_cursor = s.messages[0].content.len();

        let rebuilt = build_message_lines(&s, 0, 80);
        // The LRU must have been missed and a fresh render produced.
        // A stale hit would still carry the old content version's
        // `line_count` (raw-newline based) reflected in the line
        // count, but more importantly the rebuild path is the only
        // way to get the new short content — verify the rebuilt
        // span content is the new "x", not the old "original long
        // content".
        let content_chars: usize = rebuilt
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .filter(|s| !s.is_empty() && !s.chars().all(|c| c == ' '))
            .map(|s| s.chars().count())
            .sum();
        // The rebuilt content should reflect the new short text.
        // Old content "original long content" is 21 chars; new is 1.
        assert!(
            content_chars <= new_len + 2,
            "rebuild should show the new short content (got {content_chars} content chars)"
        );
    }

    #[test]
    fn long_chinese_message_does_not_truncate_viewport() {
        // Regression: a long single-block user message containing
        // CJK text (each char is width 2) used to underflow the
        // viewport math because the count was off vs. what
        // `build_message_lines` actually emitted. Verify the counted
        // total matches the rendered line count for a realistic
        // Chinese message that wraps to many display rows.
        use crate::session::Message;
        let mut s = Session::default();
        let long_zh = "中文测试 ".repeat(200);
        s.push(Message::new(Role::User, &long_zh));

        let width: u16 = 80;
        let total = s.count_all_lines_with_width(width as usize);
        let rendered = build_message_lines(&s, 0, width as usize);
        assert_eq!(
            total as usize,
            rendered.len(),
            "viewport total ({total}) must equal rendered line count ({})",
            rendered.len()
        );
    }
}
