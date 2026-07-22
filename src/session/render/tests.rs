use super::*;

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
        asst.thinking_segments
            .push(crate::session::ThinkingSegment {
                offset: "text\n\n| A | B |".len(),
                content: "thinking content".to_string(),
                closed: false,
                tool_results_len_at_open: 0,
                cached_line_count_expanded: None,
                cached_line_count_collapsed: None,
                started_at: None,
                ended_at: None,
                visible: false,
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
        //   1 leading gap
        // (No final spacer — inter-message/bottom gaps are managed
        // at the viewport level.)
        let think_lines = thinking_block_line_count("thinking content", true, 10, width) as u32;
        let overhead = think_lines + 1 + 1; // thinking rows + trailing blank + leading gap
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
    //! `write_file` diff (or the last `[wall|timeout]` row of a long shell
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

    fn make_edit_tool() -> ToolResultBlock {
        // A small but valid write_file_diff payload so the bottom
        // border is part of the rendered block. The diff lives in
        // `metadata` (UI-only); `content` is the short AI-facing
        // success message.
        ToolResultBlock {
            name: "edit".to_string(),
            title: "edit".to_string(),
            content: "Edit applied successfully.".to_string(),
            metadata: serde_json::json!({
                "kind": "edit_diff",
                "path": "src/demo.py",
                "old": "alpha\nold_call()\nomega\n",
                "new": "alpha\nnew_call()\nomega\n",
                "output": "Edit applied successfully.",
            })
            .to_string(),
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

    fn make_shell_tool() -> ToolResultBlock {
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
            failed: false,            call_id: String::new(),
            pruned: false,
            streaming_input: String::new(), cached_line_count_visible: None,
            cached_line_count_collapsed: None,
            started_at: None,
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
        build_message_lines(s, msg_idx, width).as_ref().clone()
    }

    /// The exact total returned by `compute_total_lines` must equal
    /// the sum of `build_message_lines` per-message outputs plus the
    /// session-level gaps (one per message).
    #[test]
    fn tool_block_count_matches_rendered_no_content() {
        let mut s = session_with_tool(make_edit_tool(), false);
        let width = 80u16;
        let total = count_all(&mut s, width);
        let user_lines = lines_for_msg(&s, 0, width as usize).len() as u32;
        let asst_lines = lines_for_msg(&s, 1, width as usize).len() as u32;
        let expected = user_lines + asst_lines + s.messages.len() as u32;
        assert_eq!(
            total,
            expected,
            "total={total} but user={user_lines} + asst={asst_lines} + gaps({}) = {expected}",
            s.messages.len()
        );
    }

    #[test]
    fn tool_block_count_matches_rendered_with_content() {
        let mut s = session_with_tool(make_edit_tool(), true);
        let width = 80u16;
        let total = count_all(&mut s, width);
        let user_lines = lines_for_msg(&s, 0, width as usize).len() as u32;
        let asst_lines = lines_for_msg(&s, 1, width as usize).len() as u32;
        let expected = user_lines + asst_lines + s.messages.len() as u32;
        assert_eq!(total, expected);
    }

    #[test]
    fn two_tool_blocks_count_matches_rendered() {
        let mut s = session_with_tool(make_edit_tool(), false);
        s.messages[1].tool_results.push(make_shell_tool());
        let width = 80u16;
        let total = count_all(&mut s, width);
        let asst_lines = lines_for_msg(&s, 1, width as usize).len() as u32;
        let user_lines = lines_for_msg(&s, 0, width as usize).len() as u32;
        let expected = user_lines + asst_lines + s.messages.len() as u32;
        assert_eq!(total, expected);
    }

    #[test]
    fn thinking_plus_tool_count_matches_rendered() {
        let mut s = session_with_tool(make_edit_tool(), false);
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
            started_at: None,
            ended_at: None,
            visible: false,
        }];
        s.messages[1].thinking_visible = true;
        let width = 80u16;
        let total = count_all(&mut s, width);
        let asst_lines = lines_for_msg(&s, 1, width as usize).len() as u32;
        let user_lines = lines_for_msg(&s, 0, width as usize).len() as u32;
        let expected = user_lines + asst_lines + s.messages.len() as u32;
        assert_eq!(total, expected);
    }

    /// The bug, narrowed to a single assertion: the tool block's
    /// bottom border (`+---…---+`) must be visible in the viewport
    /// slice built by `build_lines_viewport` at the bottom of the
    /// session, with only the bottom-gap blank line after it.
    #[test]
    fn bottom_border_line_is_in_viewport() {
        let mut s = session_with_tool(make_edit_tool(), false);
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
    /// panel is now the bottom gap (a blank line inserted by
    /// `build_lines_viewport`); it must still be the LAST
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
        // last rendered line MUST be the bottom-gap blank.
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
        // is still the bottom-gap blank.
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
            "bottom gap must be the very last line in full-viewport mode too"
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
        // The tool already exists in `tool_results`, then the
        // model thinks about the result — so the segment's
        // `tool_results_len_at_open` must be 1 (the count at the
        // moment the segment opened), telling the sort tiebreaker
        // this is post-tool reasoning.
        asst.thinking_segments = vec![crate::session::ThinkingSegment {
            offset: 0,
            content: "plan".to_string(),
            closed: false,
            tool_results_len_at_open: 1,
            cached_line_count_expanded: None,
            cached_line_count_collapsed: None,
            started_at: None,
            ended_at: None,
            visible: false,
        }];
        asst.thinking_visible = true;
        asst.tool_results.push(make_edit_tool());
        s.push(asst);
        let width = 80;
        let rendered = build_message_lines(&s, 1, width);
        let text = lines_to_text(&rendered);
        let tool_idx = text.find("Edit [").expect("tool block missing");
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
                started_at: None,
                ended_at: None,
                visible: false,
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
        assert_eq!(
            s.messages[1].thinking_segments[0].content,
            "Let me run the tool first."
        );
        assert_eq!(
            s.messages[1].thinking_segments[1].content,
            "Good, that worked."
        );
    }

    /// End-to-end: a tool call that lands between two reasoning
    /// bursts must produce two `+--- Thinking ---+` boxes in the
    /// rendered chat — one anchored at the pre-tool offset, one
    /// anchored at the post-tool offset — with the tool block in
    /// between. This is the regression for "all thinking crammed
    /// into one block at the bottom of the message".
    #[test]
    fn thinking_flanking_tool_call_renders_two_boxes_in_correct_order() {
        let mut s = Session::default();
        s.push(Message::new(Role::User, "do it"));
        s.push(Message::new(Role::Assistant, ""));
        s.messages[1].streaming = true;
        s.streaming_id = Some(1);

        // 1. Pre-tool reasoning burst.
        s.append_thinking_to_last("Let me run the tool first.");

        // 2. Tool call + result arrive (in real life via
        //    `update_last_tool_content` and a `ContentBlockStart`
        //    for the next block). The tool_results.len() growth is
        //    what triggers the auto-close on the next reasoning
        //    delta.
        s.messages[1]
            .tool_results
            .push(crate::session::ToolResultBlock {
                name: "bash".to_string(),
                title: "Bash".to_string(),
                content: "ok".to_string(),
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

        // 3. Post-tool reasoning burst. The auto-close from Layer 1
        //    should land this in a fresh segment at the post-tool
        //    offset.
        s.append_thinking_to_last("Good, that worked.");
        assert_eq!(s.messages[1].thinking_segments.len(), 2);

        s.messages[1].thinking_visible = true;
        s.display = crate::config::ThinkingDisplay::Show;
        s.tool_preview_lines = 10;

        let text = lines_to_text(&build_message_lines(&s, 1, 80));
        let label_count = text.matches("+--- Thinking").count();
        assert_eq!(
            label_count, 2,
            "expected two Thinking boxes (one per reasoning burst), got {label_count}.\nRendered:\n{text}"
        );

        // The pre-tool thinking box must come BEFORE the tool block
        // in the rendered output, and the post-tool thinking box
        // must come AFTER it.
        let first_thinking = text.find("+--- Thinking").expect("first Thinking box");
        let tool_marker = text.find("Bash").expect("tool block");
        let second_thinking = text.rfind("+--- Thinking").expect("second Thinking box");
        assert!(
            first_thinking < tool_marker,
            "pre-tool thinking must render before the tool block, but thinking at {first_thinking} came after tool at {tool_marker}.\nRendered:\n{text}"
        );
        assert!(
            tool_marker < second_thinking,
            "post-tool thinking must render after the tool block, but tool at {tool_marker} came after thinking at {second_thinking}.\nRendered:\n{text}"
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
            context_path: "C:/Users/me/.agents/skills/commit-and-push-all/SKILL.md".to_string(),
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
        let expected = user_lines + asst_lines + s.messages.len();
        assert_eq!(
            total,
            expected,
            "compute_total_lines returned {total} but actual rendered lines = \
             user({user_lines}) + asst({asst_lines}) + gaps({}) = {expected}",
            s.messages.len()
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
        assert_eq!(total, user_lines + asst_lines + s.messages.len());
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
        // `lines_before` must also count the skill block rows and the
        // gap after the user message. Without the fix, the undercount
        // shifts the scroll target computed by `jump_to_message` and
        // `timeline`.
        let mut s = Session::default();
        s.push(user_with_skill(None));
        s.push(Message::new(Role::Assistant, ""));

        // Warm the per-message content cache that lines_before relies on.
        let _ = s.count_all_lines_with_width(120);
        let n = s.lines_before(1, 120);
        // lines_before(1) = user message lines + gap after user message.
        let user_lines = build_message_lines(&s, 0, 120).len() as u32;
        assert_eq!(
            n,
            user_lines + 1,
            "lines_before(1) = {n} but user message ({user_lines} lines) + 1 gap = {}",
            user_lines + 1
        );
    }

    /// Regression: `jump_to_message` must set `scroll` so the target
    /// message appears at the top of the viewport. Previously, when
    /// `cached_total_lines` was still valid, `count_all_lines_with_width`
    /// skipped `compute_total_lines` (which populates `line_offsets`),
    /// so `line_offsets.get(msg_idx)` returned None, unwrap_or(0)
    /// gave 0 = "scroll to top".
    #[test]
    fn jump_to_message_lands_on_correct_position() {
        let mut s = Session::default();
        // Message 0: user
        s.push(Message::new(Role::User, "first question"));
        // Message 1: assistant with lots of content (many lines)
        let mut asst1 = Message::new(Role::Assistant, "");
        asst1.content = (0..50)
            .map(|i| format!("Line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        asst1.display_cursor = usize::MAX;
        s.push(asst1);
        // Message 2: user
        s.push(Message::new(Role::User, "second question"));
        // Message 3: assistant with more content
        let mut asst2 = Message::new(Role::Assistant, "");
        asst2.content = (0..50)
            .map(|i| format!("More {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        asst2.display_cursor = usize::MAX;
        s.push(asst2);

        let width = 80u16;
        let viewport_h = 10u16;

        // Warm the cache (simulates a render frame having run).
        let total = s.count_all_lines_with_width(width as usize);

        // Jump to message 2 (the second user message).
        s.jump_to_message(2, None, viewport_h, width);

        // pending_scroll_top should be set with the correct lines_before.
        let msg_start = s.line_offsets.get(2).copied().unwrap_or(0);
        assert_eq!(
            s.pending_scroll_top,
            Some(msg_start),
            "pending_scroll_top should be line_offsets[2] = {msg_start}, got {:?}",
            s.pending_scroll_top
        );

        // Simulate render-time computation with the real inner_h.
        let real_inner_h = 20u32; // taller viewport (panel hidden)
        let lines_before = s.pending_scroll_top.take().unwrap();
        let scroll = total
            .saturating_sub(real_inner_h)
            .saturating_sub(lines_before);
        let actual_start = total.saturating_sub(real_inner_h).saturating_sub(scroll);
        assert_eq!(
            actual_start, msg_start,
            "render-time computation: expected start {msg_start}, got {actual_start}"
        );

        // Jump to message 3.
        s.jump_to_message(3, None, viewport_h, width);
        let msg_start = s.line_offsets.get(3).copied().unwrap_or(0);
        assert_eq!(
            s.pending_scroll_top,
            Some(msg_start),
            "pending_scroll_top should be line_offsets[3] = {msg_start}, got {:?}",
            s.pending_scroll_top
        );
    }

    /// Test that jump_to_message with tool_idx correctly positions the
    /// tool block at the top of the viewport.
    #[test]
    fn jump_to_message_with_tool_idx() {
        use crate::session::ToolResultBlock;
        let mut s = Session::default();
        s.push(Message::new(Role::User, "go"));
        let mut asst = Message::new(
            Role::Assistant,
            "Here is some text.\nMore text.\nEven more.",
        );
        asst.display_cursor = usize::MAX;
        asst.tool_results.push(ToolResultBlock {
            name: "shell_command".into(),
            title: "$ echo hi".into(),
            content: "hi".into(),
            metadata: String::new(),
            content_offset: 15, // after "Here is some te"
            visible: false,
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
        let viewport_h = 10u16;
        let _total = s.count_all_lines_with_width(width as usize);

        // Jump to tool 0 in message 1.
        s.jump_to_message(1, Some(0), viewport_h, width);

        let msg_start = s.line_offsets.get(1).copied().unwrap_or(0);
        // pending_scroll_top should be set.
        assert!(
            s.pending_scroll_top.is_some(),
            "pending_scroll_top should be set for tool jump"
        );
        let lines_before = s.pending_scroll_top.take().unwrap();
        // The tool offset within the message should be > 0 (there's
        // content before it), so lines_before > msg_start.
        assert!(
            lines_before >= msg_start,
            "tool jump: lines_before ({lines_before}) should be >= msg_start ({msg_start})"
        );
    }
}

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use super::*;
    use crate::config::ThinkingDisplay;
    use crate::session::{Message, Role, Session, ToolResultBlock};
    use ratatui::style::Color;

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
            tool_calls: Vec::new(),
            attachments: Vec::new(),
            display_cursor: usize::MAX,
            ts: chrono::Utc::now(),
            streaming: false,
            skill_ref: None,
            line_count: 0,
            cached_content_line_count: None,
            content_version: 0,
            prefix: false,
        });
        s
    }

    /// Regression: parallel tool calls (e.g. websearch + webfetch) used
    /// to create a cascade of duplicate empty blocks because
    /// `update_tool_input_delta` deduplicated by "last block name" only.
    /// With call_id routing, interleaved deltas for two tools must land
    /// in exactly two blocks, and final results must fill the matching
    /// block — never creating empty placeholder blocks.
    #[test]
    fn parallel_tool_calls_route_by_call_id_without_duplicates() {
        let mut s = Session::default();
        s.push(Message::new(Role::User, "search and fetch"));
        s.push(Message::new(Role::Assistant, ""));
        s.streaming_id = Some(1);

        // Interleave streaming deltas for two parallel tool calls.
        s.update_tool_input_delta(0, "callA", "websearch", r#"{"query":"a"}"#);
        s.update_tool_input_delta(1, "callB", "webfetch", r#"{"url":"x"}"#);
        s.update_tool_input_delta(0, "callA", "websearch", r#"{"query":"ab"}"#);
        s.update_tool_input_delta(1, "callB", "webfetch", r#"{"url":"xy"}"#);

        let m = &s.messages[1];
        assert_eq!(
            m.tool_results.len(),
            2,
            "expected exactly 2 blocks, got {}",
            m.tool_results.len()
        );
        assert_eq!(m.tool_results[0].call_id, "callA");
        assert_eq!(m.tool_results[1].call_id, "callB");
        assert_eq!(m.tool_results[0].streaming_input, r#"{"query":"ab"}"#);
        assert_eq!(m.tool_results[1].streaming_input, r#"{"url":"xy"}"#);

        // Final results must route to the matching block by call_id.
        s.update_last_tool_content(
            "websearch".into(),
            "search".into(),
            "result A".into(),
            "callA".into(),
            String::new(),
            false,
        );
        s.update_last_tool_content(
            "webfetch".into(),
            "fetch".into(),
            "result B".into(),
            "callB".into(),
            String::new(),
            false,
        );
        let m = &s.messages[1];
        assert_eq!(m.tool_results[0].content, "result A");
        assert_eq!(m.tool_results[1].content, "result B");
        assert!(!m.tool_results[0].running);
        assert!(!m.tool_results[1].running);

        // Regression: an empty placeholder block (no content and no
        // streaming input) must not consume any blank lines in the
        // total line count, otherwise the viewport grows with phantom
        // blank rows during parallel tool calls.
        s.messages[1].tool_results.push(ToolResultBlock {
            name: "webfetch".into(),
            title: String::new(),
            content: String::new(),
            metadata: String::new(),
            content_offset: 0,
            visible: true,
            running: true,
            failed: false,
            call_id: "stale".into(),
            pruned: false,
            streaming_input: String::new(),
            cached_line_count_visible: None,
            cached_line_count_collapsed: None,
            started_at: None,
        });
        let total_with_placeholder = s.count_all_lines_with_width(80);
        s.messages[1].tool_results.pop();
        let total_without_placeholder = s.count_all_lines_with_width(80);
        assert_eq!(
            total_with_placeholder, total_without_placeholder,
            "empty placeholder must not change total line count"
        );
    }

    /// Regression: a leftover empty placeholder block (content and
    /// streaming_input both empty) must not render as a blank bordered
    /// box. The render items builder skips such blocks.
    #[test]
    fn empty_placeholder_tool_block_is_not_rendered() {
        use crate::session::ToolResultBlock;
        let mut s = Session {
            display: ThinkingDisplay::Show,
            ..Session::default()
        };
        s.push(Message::new(Role::User, "go"));
        let mut asst = Message::new(Role::Assistant, "thinking…");
        // A real result block.
        asst.tool_results.push(ToolResultBlock {
            name: "websearch".into(),
            title: "search".into(),
            content: "real result".into(),
            metadata: String::new(),
            content_offset: 0,
            visible: true,
            running: false,
            failed: false,
            call_id: "real".into(),
            pruned: false,
            streaming_input: String::new(),
            cached_line_count_visible: None,
            cached_line_count_collapsed: None,
            started_at: None,
        });
        // A stray empty placeholder (the kind the old bug produced).
        asst.tool_results.push(ToolResultBlock {
            name: "webfetch".into(),
            title: String::new(),
            content: String::new(),
            metadata: String::new(),
            content_offset: 0,
            visible: true,
            running: true,
            failed: false,
            call_id: "stale".into(),
            pruned: false,
            streaming_input: String::new(),
            cached_line_count_visible: None,
            cached_line_count_collapsed: None,
            started_at: None,
        });
        s.push(asst);
        let (lines, _t) = build_lines(&s, 100);
        let text = lines_to_text(&lines);
        assert!(text.contains("real result"), "real block dropped:\n{text}");
        // The stray empty block should not produce a titled box header.
        assert!(
            !text.contains("webfetch"),
            "empty placeholder leaked:\n{text}"
        );
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
            metadata: String::new(),
            content_offset: 0,
            visible: true,
            running: false,
            failed: false,            call_id: String::new(),
            pruned: false,
            streaming_input: String::new(), cached_line_count_visible: None,
            cached_line_count_collapsed: None,
        started_at: None,
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
            metadata: String::new(),
            content_offset: 0,
            visible: true,
            running: false,
            failed: false,            call_id: String::new(),
            pruned: false,
            streaming_input: String::new(), cached_line_count_visible: None,
            cached_line_count_collapsed: None,
        started_at: None,
        };
        let rows = build_tool_block_rows(&tool, true, 10, 100);
        assert!(
            rows.is_empty(),
            "ask tool block must be empty, got {rows:?}"
        );
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
        let lines = render_ask_snapshot_message(body, 60, false, 0);
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
        };
        let rows = build_tool_block_rows(&tool, true, 10, 100);
        let text = lines_to_text(&rows);
        assert!(text.contains("hello"), "body missing:\n{text}");
        assert!(text.contains("body"), "body missing:\n{text}");
        assert!(!text.contains("{\"ok\":"), "json envelope leaked:\n{text}");
        assert!(
            !text.contains("\"kind\":\"plan\""),
            "raw inner JSON leaked:\n{text}"
        );
    }

    #[test]
    fn build_tool_block_renders_edit_diff() {
        let tool = ToolResultBlock {
            name: "edit".to_string(),
            title: "edit".to_string(),
            content: "Edit applied successfully.".to_string(),
            metadata: serde_json::json!({
                "kind": "edit_diff",
                "path": "src/demo.py",
                "old": "alpha\n    old_call()\nomega\n",
                "new": "alpha\n    new_call()\nomega\n",
                "output": "Edit applied successfully.",
            })
            .to_string(),
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
        };
        let rows = build_tool_block_rows(&tool, true, 10, 80);
        let text = lines_to_text(&rows);
        assert!(
            text.contains("Edit [src/demo.py +1/-1]"),
            "title missing:\n{text}"
        );
        assert!(
            text.contains("-  2 │     old_call()"),
            "removed line missing:\n{text}"
        );
        assert!(
            text.contains("+  2 │     new_call()"),
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
                tool_calls: Vec::new(),
                attachments: Vec::new(),
                display_cursor: usize::MAX,
                ts: chrono::Utc::now(),
                streaming: false,
                skill_ref: None,
                line_count: lines_per_msg as u32,
                cached_content_line_count: None,
                content_version: 0,
                prefix: false,
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
            warmup_us < 400_000,
            "warmup took {warmup_us}µs (expected <400ms)"
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
            asst_lines.len() + user_lines.len() + s.messages.len(),
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
        // total = rendered message lines + bottom gap
        assert_eq!(
            total as usize,
            rendered.len() + s.messages.len(),
            "viewport total ({total}) must equal rendered line count ({}) + bottom gap ({})",
            rendered.len(),
            s.messages.len()
        );
    }

    #[test]
    fn box_row_line_width_is_exact() {
        let bg = Color::Black;
        let width = 100;
        let line = box_row_line("hello", width, bg);
        let w: usize = line
            .spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        assert_eq!(w, width, "box_row_line width {w} != {width}");

        let long = "x".repeat(width - 3);
        let line2 = box_row_line(&long, width, bg);
        let w2: usize = line2
            .spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        assert_eq!(w2, width, "box_row_line long text width {w2} != {width}");

        let line3 = box_row_line("\u{4e2d}\u{6587}", width, bg);
        let w3: usize = line3
            .spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        assert_eq!(w3, width, "box_row_line CJK width {w3} != {width}");
    }
}

#[cfg(test)]
mod code_block_content_width_tests {
    use super::*;
    use crate::session::{Message, Role, Session};

    #[test]
    fn code_block_lines_in_user_message_have_same_width() {
        let short = "short";
        let long = "this_is_a_very_long_line_that_exceeds_normal_width";
        let content = format!("Look at this:\n\n```\n{short}\n{long}\n```\n\nAfter block.");

        let mut s = Session::default();
        s.push(Message::new(Role::User, content));

        let width = 80usize;
        let rendered = build_message_lines(&s, 0, width);

        let text = rendered
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Find code block lines: those that contain the pipe border
        let mut code_line_widths = Vec::new();
        let mut in_code = false;
        for line in rendered.iter() {
            let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if joined.contains('+') && joined.contains('-') && joined.contains("code") {
                in_code = true;
                continue;
            }
            if joined.contains('+') && joined.contains('-') && in_code {
                break;
            }
            if in_code && joined.contains('|') {
                let total_w: usize = line
                    .spans
                    .iter()
                    .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                    .sum();
                code_line_widths.push((joined.trim().to_string(), total_w));
            }
        }

        assert!(
            !code_line_widths.is_empty(),
            "no code block lines found in:\n{text}"
        );

        let first = code_line_widths[0].1;
        for (i, (content, w)) in code_line_widths.iter().enumerate() {
            assert_eq!(
                *w, first,
                "code line {i} width {w} != {first}\n  content: {content:?}\n  all: {code_line_widths:?}\n\nfull text:\n{text}"
            );
        }
    }
}

#[cfg(test)]
mod skill_output_block_width_tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn skill_output_block_lines_have_consistent_width() {
        let content = "\
<skill_content name=\"test\">
# Skill

Some text.

```xml
<validation>
<type>feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert</type>
<scope>optional</scope>
</validation>
```

</skill_content>";

        let width = 80;
        let bg = Color::Reset;
        let rows = output_row_lines(content, width, bg);

        assert!(!rows.is_empty(), "no output rows");

        let first_w = rows[0].width();
        for (i, row) in rows.iter().enumerate() {
            let joined: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            let line_w = row.width();
            assert_eq!(
                line_w, first_w,
                "line {i} width mismatch: {line_w} != {first_w}\n  content: {joined:?}"
            );
        }
    }

    #[test]
    fn full_build_output_block_rows_consistent_width() {
        let content = "\
<skill_content name=\"test\">
# Skill

Some text.

```xml
<validation>
<type>feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert</type>
<scope>optional</scope>
</validation>
```

</skill_content>";

        let width = 80;
        let bg = Color::Reset;
        let rows = build_output_block_rows(
            " Skill ", content, "", true, // visible
            10,   // preview_lines
            width, bg,
        );

        assert!(!rows.is_empty(), "no output rows");

        let first_w = rows[0].width();
        for (i, row) in rows.iter().enumerate() {
            let joined: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            let line_w = row.width();
            assert_eq!(
                line_w, first_w,
                "line {i} width mismatch: {line_w} != {first_w}\n  content: {joined:?}"
            );
        }
    }
}

#[cfg(test)]
mod border_fix_tests {
    use super::*;
    use ratatui::style::Color;

    /// Bug 3: shell command wrap — all body rows must have the same
    /// width (= the `width` parameter), including the second wrapped
    /// line of a long command.  Uses the user's exact command.
    #[test]
    fn shell_command_wrapped_rows_consistent_width() {
        let long_cmd = r#"$ git commit -m "fix(markdown): use saturating_sub to prevent underflow in code_line padding" -m "Replace `width - content_width` with `saturating_sub` to avoid underflow when content exceeds the available width. Add tests for code block and output block line width consistency.""#;
        for width in [40usize, 60, 80, 100, 120, 140, 150, 160] {
            let rows = build_shell_command_rows(
                long_cmd,
                "output line",
                "[Wall: 1.0s]",
                true,
                10,
                width,
                Color::Reset,
            );
            assert!(!rows.is_empty(), "no rows for width {width}");

            let first_w = rows[0].width();
            for (i, row) in rows.iter().enumerate() {
                let line_w = row.width();
                assert_eq!(
                    line_w,
                    first_w,
                    "width {width}: row {i} has width {line_w} != {first_w}\n  spans: {:?}",
                    row.spans
                        .iter()
                        .map(|s| (
                            s.content.as_ref(),
                            UnicodeWidthStr::width(s.content.as_ref())
                        ))
                        .collect::<Vec<_>>()
                );
            }
            assert_eq!(first_w, width, "row width {first_w} != {width}");
        }
    }

    /// Bug 2: diff_box_row_line — the total line width must equal
    /// `width`, not `width + 2`.
    #[test]
    fn diff_box_row_line_width_matches() {
        let diff = DiffLine {
            kind: DiffLineKind::Added,
            line_no: 42,
            content: "let x = some_long_variable_name_that_might_overflow;".to_string(),
        };
        for width in [30usize, 50, 80, 120] {
            let line = diff_box_row_line(
                &diff,
                3.max(diff.line_no.to_string().len()),
                width,
                Color::Reset,
                "rust",
            );
            let line_w = line.width();
            assert_eq!(
                line_w,
                width,
                "diff row width {line_w} != {width}\n  spans: {:?}",
                line.spans
                    .iter()
                    .map(|s| (
                        s.content.as_ref(),
                        UnicodeWidthStr::width(s.content.as_ref())
                    ))
                    .collect::<Vec<_>>()
            );
        }
    }

    /// Bug 4: render_content_segment — all output lines must have
    /// width == `width` (so the background fills the entire row).
    #[test]
    fn content_segment_lines_fill_full_width() {
        let content = "This is a paragraph with some text that is long enough to require wrapping at narrower widths.";
        for width in [30usize, 50, 80] {
            let mut out = Vec::new();
            render_content_segment(content, width, &mut out);
            assert!(!out.is_empty(), "no output for width {width}");
            for (i, line) in out.iter().enumerate() {
                let line_w = line.width();
                assert_eq!(
                    line_w, width,
                    "width {width}: line {i} has width {line_w} != {width}"
                );
            }
        }
    }

    /// Bug 4: code blocks inside content segments must also fill
    /// the full width.
    #[test]
    fn content_segment_code_block_fills_width() {
        let content = "```\nshort\n```\n";
        for width in [30usize, 50, 80] {
            let mut out = Vec::new();
            render_content_segment(content, width, &mut out);
            assert!(!out.is_empty(), "no output for width {width}");
            for (i, line) in out.iter().enumerate() {
                let line_w = line.width();
                assert_eq!(
                    line_w, width,
                    "width {width}: line {i} has width {line_w} != {width}"
                );
            }
        }
    }

    /// Bug 1: thinking block — long lines must be wrapped, not
    /// truncated, so all content is visible.
    #[test]
    fn thinking_block_wraps_long_lines() {
        let long_text = "a".repeat(200);
        for width in [30usize, 50, 80] {
            let rows = build_thinking_block_rows(
                &long_text,
                true, // visible
                10,
                width,
                Color::Reset,
                None,
            );
            assert!(
                rows.len() > 2,
                "expected more than 2 rows for width {width}"
            );

            // All body rows (excluding top/bottom borders) must have width == width
            for (i, row) in rows.iter().enumerate() {
                if i == 0 || i == rows.len() - 1 {
                    continue; // skip border rows
                }
                let line_w = row.width();
                assert_eq!(
                    line_w, width,
                    "width {width}: body row {i} has width {line_w} != {width}"
                );
            }

            // Check that we can see the end of the text (not truncated)
            let last_body = rows[rows.len() - 2]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>();
            assert!(
                last_body.contains('a'),
                "last body row should contain 'a', got: {last_body:?}"
            );
        }
    }

    /// Diagnostic: check if highlight_line preserves the exact text
    /// width of the input.  If the span widths don't match the input
    /// line width, box_row_line_spans will produce wrong padding.
    #[test]
    fn highlight_line_preserves_width() {
        let cmd = r#"cd D:\Code\rust\fish_coding_agent; git commit -m "feat(compaction): add context pruning, doom-loop detection, and tool metadata stripping- Add prune pass that clears old tool outputs exceeding 40k token budget- Add doom-loop detector breaking after 3 identical consecutive tool calls- Strip UI-only metadata from tool results before sending to LLM- Enforce token-budget output discipline in system prompt- Cover neologic with unit tests""#;

        for width in [80usize, 100, 120, 140, 150, 160] {
            let max_cmd_width = width.saturating_sub(6);
            let cmd_lines = wrap_line(cmd, max_cmd_width);
            for (i, line) in cmd_lines.iter().enumerate() {
                let line_w = visible_width(line);
                let spans = crate::session::markdown::highlight_line(line, "sh");
                let span_total: usize = spans
                    .iter()
                    .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                    .sum();
                let span_text: String = spans.iter().map(|s| s.content.as_ref()).collect();
                // Also verify the text is exactly preserved (no chars added/removed)
                assert_eq!(
                    &span_text, line,
                    "width {width}: cmd line {i}: span text differs from input!\n  input: {line:?}\n  spans: {span_text:?}"
                );
                assert_eq!(
                    span_total, line_w,
                    "width {width}: cmd line {i}: span total width {span_total} != line width {line_w}\n  line: {line:?}\n  span_text: {span_text:?}\n  spans: {:?}",
                    spans.iter().map(|s| (s.content.as_ref(), UnicodeWidthStr::width(s.content.as_ref()))).collect::<Vec<_>>()
                );

                // Also check: prefix + spans should fit in max_content
                let prefix_w = 2; // "$ " or "  "
                let total_content = prefix_w + span_total;
                let max_content = width.saturating_sub(4);
                assert!(
                    total_content <= max_content,
                    "width {width}: cmd line {i}: prefix({prefix_w}) + spans({span_total}) = {total_content} > max_content({max_content})\n  line: {line:?}"
                );
            }
        }
    }

    /// End-to-end: build a full session with a skill tool result,
    /// render through build_message_lines, render to buffer, and
    /// check the `|` is at the rightmost column for every body row.
    #[test]
    fn full_session_skill_tool_block_right_border() {
        use crate::session::{Message, Role, Session, ToolResultBlock};
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let skill_body = "\
6. Just execute this prompt and Copilot will handle the commit for you in the terminal.

### Commit Message Structure

```xml
<commit-message>
<type>feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert</type>
<scope>()</scope>
<description>A short, imperative summary of the change</description>
<body>(optional: more detailed explanation)</body>
<footer>(optional: e.g. BREAKING CHANGE: details, or issue references)</footer>
</commit-message>
```

### Examples

```xml
<examples>
<example>feat(parser): add ability to parse arrays</example>
<example>fix(ui): correct button alignment</example>
<example>docs: update README with usage instructions</example>
<example>refactor: improve performance of data processing</example>
<example>chore: update dependencies</example>
<example>feat!: send email on registration (BREAKING CHANGE: email service required)</example>
</examples>
```";

        for width in [80usize, 100, 120, 140, 150] {
            let mut session = Session::default();
            let mut msg = Message::new(Role::Assistant, "Running skill.");
            msg.tool_results.push(ToolResultBlock {
                name: "skill".to_string(),
                title: " Skill ".to_string(),
                content: skill_body.to_string(),
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
            session.push(msg);

            let lines = build_message_lines(&session, 0, width);
            let lines_vec: Vec<Line<'static>> = lines.iter().cloned().collect();

            let area = Rect::new(0, 0, width as u16, lines_vec.len() as u16);
            let mut buf = Buffer::empty(area);
            let p = ratatui::widgets::Paragraph::new(lines_vec.clone())
                .style(ratatui::style::Style::reset());
            p.render(area, &mut buf);

            for (i, line) in lines_vec.iter().enumerate() {
                let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                // Skip blank lines and border rows
                if content.is_empty() || content.starts_with('+') {
                    continue;
                }
                // Only check rows that have | borders (tool block body rows)
                if !content.starts_with('|') {
                    continue;
                }

                let line_w = line.width();
                if line_w != width {
                    // Print detailed span info for debugging
                    let span_info: Vec<(String, usize)> = line
                        .spans
                        .iter()
                        .map(|s| {
                            (
                                s.content.to_string(),
                                UnicodeWidthStr::width(s.content.as_ref()),
                            )
                        })
                        .collect();
                    panic!(
                        "width {width}: row {i} Line::width() = {line_w}, expected {width}\n  content[:80]: {:?}\n  spans: {span_info:?}",
                        &content[..content.len().min(80)]
                    );
                }

                let cell = buf.cell((width as u16 - 1, i as u16));
                assert!(
                    cell.is_some(),
                    "width {width}: no cell at ({}, {i})",
                    width - 1
                );
                let cell_symbol = cell.unwrap().symbol();
                assert_eq!(
                    cell_symbol, "|",
                    "width {width}: row {i} rightmost cell is {:?}, expected \"|\"\n  content[:80]: {:?}",
                    cell_symbol,
                    &content[..content.len().min(80)]
                );
            }
        }
    }

    /// Bug 3 deep diagnostic: render the EXACT shell command from
    /// the user's screenshot into a ratatui Buffer and check the
    /// rightmost 5 cells of every body row. This will reveal exactly
    /// where the `|` ends up.
    #[test]
    fn shell_command_buffer_rightmost_cells() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let cmd = r#"$ cd D:\Code\rust\fish_coding_agent; git commit -m "feat(compaction): add context pruning, doom-loop detection, and tool metadata stripping- Add prune pass that clears old tool outputs exceeding 40k token budget- Add doom-loop detector breaking after 3 identical consecutive tool calls- Strip UI-only metadata from tool results before sending to LLM- Enforce token-budget output discipline in system prompt- Cover neologic with unit tests""#;

        for width in [80usize, 100, 120, 140, 150] {
            let rows = build_shell_command_rows(
                cmd,
                "output",
                "[Wall: 1.0s]",
                true,
                10,
                width,
                Color::Reset,
            );

            let area = Rect::new(0, 0, width as u16, rows.len() as u16);
            let mut buf = Buffer::empty(area);
            let p = ratatui::widgets::Paragraph::new(rows.clone())
                .style(ratatui::style::Style::reset());
            p.render(area, &mut buf);

            for (i, row) in rows.iter().enumerate() {
                let content: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
                if content.starts_with('+') || content.is_empty() {
                    continue;
                }

                // Check the rightmost 5 cells
                let mut tail = String::new();
                for x in (width.saturating_sub(5) as u16)..width as u16 {
                    if let Some(cell) = buf.cell((x, i as u16)) {
                        tail.push_str(cell.symbol());
                    } else {
                        tail.push('?');
                    }
                }

                let line_w = row.width();
                let has_pipe_at_edge = buf
                    .cell((width as u16 - 1, i as u16))
                    .map(|c| c.symbol() == "|")
                    .unwrap_or(false);

                assert!(
                    has_pipe_at_edge,
                    "width {width}: row {i} NO pipe at edge!\n  Line::width()={line_w}\n  tail cells: {tail:?}\n  content tail: {:?}",
                    &content[content.len().saturating_sub(20)..]
                );
            }
        }
    }

    /// Bug 3: Use the EXACT command from the user's latest screenshot.
    /// The commit message contains `|` characters which might affect
    /// rendering. Also check with ratatui's Line::width() (v0.2) vs
    /// our visible_width (v0.1) to detect version mismatches.
    #[test]
    fn shell_command_with_pipe_chars_in_content() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let cmd = r#"$ git commit -m "fix(render): strip control chars and pad lines to full width to prevent border overflowStray control characters (e.g. \r) caused width mismatches betweenunicode-width v0.1 (width 0) and v0.2 used by ratatui (width 1),pushing the right border | past the visible area. Addstrip_control_chars to normalize content before width calculationsand rendering.Additionally pad each rendered line to the full width so thebackground fill remains consistent across wrapped rows: render_content_segment: pad short and wrapped lines to inner_w buithinking_block_rows: wrap long markdown lines instead of truncating them out of view diff_box_row_line: fix layout math (inner_w = width - 6) and truncate content spans to max_content so the line width equals the box width box_row_line / box_row_line_two / box_row_line_spns: strip control chars before truncation/padding build_shell_command_rows / output_row_lines: strip control chars before wrappingA gression tests covering all four affected areas.""#;

        for width in [80usize, 100, 120, 140, 150, 160] {
            let rows = build_shell_command_rows(
                cmd,
                "output",
                "[Wall: 1.32s]",
                true,
                10,
                width,
                Color::Reset,
            );

            // Render into buffer
            let area = Rect::new(0, 0, width as u16, rows.len() as u16);
            let mut buf = Buffer::empty(area);
            let p = ratatui::widgets::Paragraph::new(rows.clone())
                .style(ratatui::style::Style::reset());
            p.render(area, &mut buf);

            for (i, row) in rows.iter().enumerate() {
                let content: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
                if content.starts_with('+') || content.is_empty() {
                    continue;
                }

                // Check ratatui's Line::width() (uses unicode-width v0.2)
                let ratatui_w = row.width();

                // Check our visible_width (uses unicode-width v0.1)
                let our_w = visible_width(&content);

                // Check the buffer cell at the rightmost column
                let cell = buf.cell((width as u16 - 1, i as u16));
                let edge_sym = cell.map(|c| c.symbol().to_string()).unwrap_or_default();

                // The rightmost cell should be "|"
                assert_eq!(
                    edge_sym, "|",
                    "width {width}: row {i} edge={edge_sym:?} expected \"|\"\n  ratatui_w={ratatui_w} our_w={our_w} width={width}\n  content tail: {:?}",
                    &content[content.len().saturating_sub(30)..]
                );

                // Also verify widths match
                assert_eq!(
                    ratatui_w, width,
                    "width {width}: row {i} ratatui Line::width()={ratatui_w} != {width}"
                );
            }
        }
    }
}
