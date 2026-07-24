use super::utils::{
    section_after, section_between, strip_control_chars, truncate_str_to_width, value_after_prefix,
    visible_width, wrap_line,
};
use crate::session::{ImageAttachment, Message, SkillRef, ThinkingSegment, ToolResultBlock};
use crate::theme::active_colors;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

pub(super) fn ensure_gap_before_block(msg_lines: &mut Vec<Line<'static>>) {
    if msg_lines.is_empty() {
        return; // viewport-level gap handles spacing before first block
    }
    if msg_lines.last().map(|l| l.width() != 0).unwrap_or(true) {
        msg_lines.push(Line::from(""));
    }
}

pub(super) fn push_block_rows(out: &mut Vec<Line<'static>>, rows: Vec<Line<'static>>) {
    out.extend(rows);
}

fn block_colors_for_tool(tool: &ToolResultBlock) -> (Color, Option<Color>) {
    let colors = active_colors();
    if tool.running {
        return (colors.tool_pending_bg, None);
    }
    let failed = tool.failed
        || match tool.name.as_str() {
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
    let content = crate::session::unwrap_tool_result_content(content);
    value_after_prefix(&content, "exit_code: ")
        .map(|code| code != "0")
        .unwrap_or(false)
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
pub fn get_thinking_segments(m: &Message) -> Vec<ThinkingSegment> {
    if !m.thinking_segments.is_empty() {
        return m.thinking_segments.clone();
    }
    if !m.thinking.is_empty() {
        return vec![ThinkingSegment {
            offset: 0,
            content: m.thinking.clone(),
            closed: false,
            tool_results_len_at_open: 0,
            cached_line_count_expanded: None,
            cached_line_count_collapsed: None,
            started_at: None,
            ended_at: None,
            visible: m.thinking_visible,
        }];
    }
    vec![]
}

pub(super) fn build_thinking_block_rows(
    content: &str,
    visible: bool,
    preview_lines: usize,
    width: usize,
    bg: Color,
    duration: Option<std::time::Duration>,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let mut rows = Vec::new();
    rows.push(border_with_label_line(width, " Thinking ", bg));
    let inner_w = width.saturating_sub(4);
    let content = content.trim_end();

    // Render a single markdown line into the thinking box, wrapping
    // if it exceeds inner_w (box_row_line_spans would otherwise
    // truncate and content would disappear off the right edge).
    let push_md_line = |line: &Line<'static>, rows: &mut Vec<Line<'static>>| {
        if line.width() <= inner_w {
            let spans = spans_with_bg(&line.spans, bg);
            rows.push(box_row_line_spans(spans, width, bg));
        } else {
            let combined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            for w in &wrap_line(&combined, inner_w) {
                let spans = spans_with_bg(&[Span::raw(w.clone())], bg);
                rows.push(box_row_line_spans(spans, width, bg));
            }
        }
    };

    if visible {
        let md_lines = crate::session::markdown::render_with_width(content, inner_w);
        if md_lines.is_empty() {
            rows.extend(box_row_lines("[no thinking content]", width, bg));
        } else {
            for line in &md_lines {
                push_md_line(line, &mut rows);
            }
        }
    } else {
        let md_lines = crate::session::markdown::render_with_width(content, inner_w);
        if md_lines.is_empty() {
            rows.extend(box_row_lines("[no thinking content]", width, bg));
        } else {
            // Wrap all markdown lines (so content stays visible), then
            // keep only the last `preview_lines` body rows + a click
            // hint when content overflows. No padding when content is
            // shorter — the collapsed height matches the content.
            let mut body: Vec<Line<'static>> = Vec::new();
            for line in md_lines.iter() {
                push_md_line(line, &mut body);
            }
            if body.len() > preview_lines {
                let skip = body.len() - preview_lines;
                body = body.split_off(skip);
                body.push(click_hint_line(skip, width, bg));
            }
            rows.extend(body);
        }
    }
    let time_label = duration
        .map(|d| format!("[{}]", format_duration(d)))
        .unwrap_or_default();
    if time_label.is_empty() {
        rows.push(border_line(width, bg));
    } else {
        rows.push(border_line_with_right_label(width, &time_label, bg));
    }
    rows
}

/// Build the boxed rows for a `[skill]` marker block. The block
/// shows name, optional args, and the on-disk context path so the
/// user has a stable visual identifier for the skill they invoked.
/// The actual skill body lives in `Message::content` and is rendered
/// below the block as ordinary markdown.
pub(super) fn build_skill_block_rows(skill: &SkillRef, width: usize) -> Vec<Line<'static>> {
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
///   2. `[skill]` (wrapped)
///   3. `name: <name>` (wrapped)
///   4. `args: <args>` (wrapped, only when args is non-empty)
///   5. `context: <path>` (wrapped)
///   6. bottom border
///   7. trailing blank line (pushed by `build_message_lines`)
///
/// Uses `wrap_line` to count wrapped lines, matching what
/// `box_row_lines` in `build_skill_block_rows` produces. Previously
/// this ignored the `width` parameter and always counted each field
/// as 1 line, causing long fields (e.g. a curl pasted as skill args)
/// to undercount the block's display rows and hide the bottom of the
/// viewport.
pub fn skill_block_line_count(skill: &SkillRef, width: usize) -> u32 {
    let render_width = width.max(8);
    let content_width = render_width.saturating_sub(4).max(1);
    let mut rows = 2u32; // top + bottom borders
    rows += wrap_line("[skill]", content_width).len() as u32;
    rows += wrap_line(&format!("name: {}", skill.name), content_width).len() as u32;
    if skill
        .args
        .as_deref()
        .map(|a| !a.trim().is_empty())
        .unwrap_or(false)
    {
        let args_text = format!("args: {}", skill.args.as_deref().unwrap());
        rows += wrap_line(&args_text, content_width).len() as u32;
    }
    rows += wrap_line(&format!("context: {}", skill.context_path), content_width).len() as u32;
    rows += 1; // trailing blank after the block
    rows
}

/// Build placeholder rows for pasted image attachments.
/// Each image gets one row: `[image #K] png 1024x768 234KB`.
pub(super) fn build_attachment_block_rows(
    attachments: &[ImageAttachment],
    width: usize,
) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut rows = Vec::new();
    // Render the whole block in dim grey so borders and text stay
    // uniform and visually distinct from the message content.
    let bg = Color::Reset;
    rows.push(border_with_label_line(width, " images ", bg));
    for (i, att) in attachments.iter().enumerate() {
        let size_kb = (att.byte_size + 512) / 1024;
        let label = if att.width > 0 && att.height > 0 {
            format!(
                "[image #{}] {} {}x{} · {}KB",
                i + 1,
                att.media_type,
                att.width,
                att.height,
                size_kb
            )
        } else {
            format!("[image #{}] {} · {}KB", i + 1, att.media_type, size_kb)
        };
        rows.push(box_row_line_dim(&label, width));
    }
    rows.push(border_line(width, bg));
    rows
}

/// Number of rendered lines consumed by attachment blocks +
/// the trailing blank line that `build_message_lines` pushes.
pub fn attachment_block_line_count(attachments: &[ImageAttachment]) -> u32 {
    if attachments.is_empty() {
        return 0;
    }
    // top border + bottom border + 1 row per attachment + trailing blank
    2 + attachments.len() as u32 + 1
}

pub(super) fn build_tool_block_rows(
    tool: &ToolResultBlock,
    visible: bool,
    preview_lines: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let (bg, fg) = block_colors_for_tool(tool);

    let visible = if tool.name == "plan" { true } else { visible };

    // Still generating: no final content yet. Show a streaming
    // preview or nothing so the block doesn't render empty rows.
    if tool.running && tool.content.is_empty() {
        if !tool.streaming_input.is_empty() {
            let rows = build_streaming_tool_rows(tool, width, bg);
            if !rows.is_empty() {
                return rows;
            }
        }
        // No usable streaming input yet — render nothing so the
        // block occupies no vertical space until content arrives.
        return vec![];
    }

    let mut rows: Vec<Line<'static>> = if tool.name == "edit" {
        if let Some(r) = build_edit_diff_rows(tool, visible, preview_lines, width, bg) {
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
    } else if tool.name == "read" {
        if let Some(r) = build_read_rows(tool, visible, preview_lines, width, bg) {
            r
        } else {
            return vec![];
        }
    } else if tool.name == "ask" {
        vec![]
    } else if tool.name == "plan" || tool.name == "sub_agent" {
        let (output, footer) = tool_display_content(tool);
        build_markdown_block_rows(
            &tool.title,
            &output,
            &footer,
            visible,
            preview_lines,
            width,
            bg,
        )
    } else if tool.name == "todowrite" {
        build_todowrite_rows(&tool.content, visible, preview_lines, width, bg)
    } else {
        let (output, footer) = tool_display_content(tool);
        let title_highlighted = tool.name == "shell_command" || tool.name == "command";
        let footer = if tool.running && title_highlighted && footer.is_empty() {
            let elapsed = tool
                .started_at
                .map(|t| (chrono::Utc::now() - t).num_seconds().max(0))
                .unwrap_or(0);
            format!("[{elapsed}s|300s]")
        } else {
            footer
        };
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

/// Render a streaming tool block — the LLM is still generating the
/// tool-call arguments. Extract partial fields from
/// `streaming_input` (raw accumulated JSON) and show a live preview.
fn build_streaming_tool_rows(
    tool: &ToolResultBlock,
    width: usize,
    bg: Color,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let args = &tool.streaming_input;
    match tool.name.as_str() {
        "shell_command" | "command" => {
            let cmd =
                crate::commands::extract_partial_json_field(args, "command").unwrap_or_default();
            build_streaming_shell_rows(&cmd, width, bg, tool.started_at)
        }
        "python_command" => {
            let code =
                crate::commands::extract_partial_json_field(args, "code").unwrap_or_default();
            build_streaming_python_rows(&code, width, bg)
        }
        "edit" => {
            let file_path =
                crate::commands::extract_partial_json_field(args, "file_path").unwrap_or_default();
            let old_str =
                crate::commands::extract_partial_json_field(args, "old_string").unwrap_or_default();
            let new_str =
                crate::commands::extract_partial_json_field(args, "new_string").unwrap_or_default();
            build_streaming_edit_rows(&file_path, &old_str, &new_str, width, bg)
        }
        _ => {
            // For other tools, show a generic "generating..." block
            let mut rows = vec![border_line(width, bg)];
            rows.extend(box_row_lines(
                &format!("generating {} tool call…", tool.name),
                width,
                bg,
            ));
            rows.push(border_line(width, bg));
            rows
        }
    }
}

/// Streaming shell command preview — shows the command text as it
/// arrives from the LLM, with sh syntax highlighting.
fn build_streaming_shell_rows(
    cmd: &str,
    width: usize,
    bg: Color,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let mut rows = Vec::new();
    rows.push(border_line(width, bg));

    let max_cmd_width = width.saturating_sub(6); // | $  |
    let cmd_lines = wrap_line(cmd, max_cmd_width);
    let cmd_refs: Vec<&str> = cmd_lines.iter().map(|s| s.as_str()).collect();
    let all_hl = crate::session::markdown::highlight_lines(&cmd_refs, "sh");

    for (i, line) in cmd_lines.iter().enumerate() {
        let prefix = if i == 0 { "$ " } else { "  " };
        let content = format!("{prefix}{line}");
        let base = box_row_line(&content, width, bg);
        let base_str: String = base.spans.iter().map(|s| s.content.as_ref()).collect();
        let content_start = 2;
        let cmd_start = content_start + prefix.len();
        let cmd_end = cmd_start + line.len();

        let hl_raw = &all_hl[i];
        let hl_spans = spans_with_bg(hl_raw, bg);
        let hl_total: usize = hl_spans.iter().map(|s| s.content.len()).sum();
        let hl_spans = if hl_total != line.len() {
            vec![Span::styled(line.to_string(), bg_style(bg))]
        } else {
            hl_spans
        };

        let mut parts: Vec<Span<'static>> = Vec::new();
        parts.push(Span::styled(
            base_str[..content_start].to_string(),
            dim_bg_style(bg),
        ));
        parts.push(Span::styled(
            base_str[content_start..cmd_start].to_string(),
            bg_style(bg),
        ));
        for span in &hl_spans {
            parts.push(span.clone());
        }
        let tail = &base_str[cmd_end..];
        if tail.len() >= 2 {
            let (pad_part, border_part) = tail.split_at(tail.len() - 2);
            if !pad_part.is_empty() {
                parts.push(Span::styled(pad_part.to_string(), bg_style(bg)));
            }
            parts.push(Span::styled(border_part.to_string(), dim_bg_style(bg)));
        } else {
            parts.push(Span::styled(tail.to_string(), dim_bg_style(bg)));
        }
        rows.push(Line::from(parts));
    }

    rows.push(border_with_label_line(width, " Output ", bg));
    rows.extend(box_row_lines("…", width, bg));
    if let Some(start) = started_at {
        let elapsed = (chrono::Utc::now() - start).num_seconds().max(0);
        let label = format!("[{elapsed}s|300s]");
        rows.push(border_line_with_right_label(width, &label, bg));
    } else {
        rows.push(border_line(width, bg));
    }
    rows
}

/// Streaming python code preview — shows the code as it arrives.
fn build_streaming_python_rows(code: &str, width: usize, bg: Color) -> Vec<Line<'static>> {
    let width = width.max(4);
    let mut rows = vec![border_with_label_line(width, " python ", bg)];

    let inner_w = width.saturating_sub(4);
    for line in code.lines() {
        let cleaned = strip_control_chars(line);
        let wrapped = wrap_line(&cleaned, inner_w);
        let refs: Vec<&str> = wrapped.iter().map(|s| s.as_str()).collect();
        let all_hl = crate::session::markdown::highlight_lines(&refs, "python");
        for (i, w) in wrapped.iter().enumerate() {
            let hl = spans_with_bg(&all_hl[i], bg);
            let hl_total: usize = hl.iter().map(|s| s.content.len()).sum();
            let hl = if hl_total != w.len() {
                vec![Span::styled(w.clone(), bg_style(bg))]
            } else {
                hl
            };
            let base = box_row_line(w, width, bg);
            let base_str: String = base.spans.iter().map(|s| s.content.as_ref()).collect();
            let content_start = 2;
            let content_end = content_start + w.len();
            let mut parts: Vec<Span<'static>> = Vec::new();
            parts.push(Span::styled(
                base_str[..content_start].to_string(),
                dim_bg_style(bg),
            ));
            for span in &hl {
                parts.push(span.clone());
            }
            let tail = &base_str[content_end..];
            if tail.len() >= 2 {
                let (pad_part, border_part) = tail.split_at(tail.len() - 2);
                if !pad_part.is_empty() {
                    parts.push(Span::styled(pad_part.to_string(), bg_style(bg)));
                }
                parts.push(Span::styled(border_part.to_string(), dim_bg_style(bg)));
            } else {
                parts.push(Span::styled(tail.to_string(), dim_bg_style(bg)));
            }
            rows.push(Line::from(parts));
        }
    }

    rows.push(border_with_label_line(width, " Output ", bg));
    rows.extend(box_row_lines("…", width, bg));
    rows.push(border_line(width, bg));
    rows
}

/// Streaming edit preview — shows old_string as red removed lines
/// and new_string as green added lines as they arrive from the LLM.
fn build_streaming_edit_rows(
    file_path: &str,
    old_str: &str,
    new_str: &str,
    width: usize,
    bg: Color,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let mut rows = Vec::new();

    // Title: Edit [file_path]
    let title = if file_path.is_empty() {
        " Edit ".to_string()
    } else {
        format!(" Edit [{file_path}] ")
    };
    rows.push(border_with_label_line(width, &title, bg));

    let inner_w = width.saturating_sub(4);

    // Show old_string lines as removed (red bg, `-` prefix)
    for line in old_str.lines() {
        let cleaned = strip_control_chars(line);
        let sign = "-";
        let (line_bg, sign_color) = (
            crate::theme::Theme::diff_removed_bg_color(),
            crate::theme::Theme::diff_removed_fg(),
        );
        let content = format!("{sign} {cleaned}");
        let wrapped = wrap_line(&content, inner_w.saturating_sub(2));
        for w in &wrapped {
            let prefix_str = format!("{} ", sign);
            let base = box_row_line(&format!("{prefix_str}{w}"), width, bg);
            let base_str: String = base.spans.iter().map(|s| s.content.as_ref()).collect();
            let content_start = 2;
            let sign_end = content_start + prefix_str.len();
            let w_end = sign_end + w.len();
            let mut parts: Vec<Span<'static>> = Vec::new();
            parts.push(Span::styled(
                base_str[..content_start].to_string(),
                dim_bg_style(bg),
            ));
            parts.push(Span::styled(
                base_str[content_start..sign_end].to_string(),
                Style::default().fg(sign_color).bg(bg),
            ));
            parts.push(Span::styled(
                base_str[sign_end..w_end].to_string(),
                bg_style(line_bg),
            ));
            let tail = &base_str[w_end..];
            if tail.len() >= 2 {
                let (pad_part, border_part) = tail.split_at(tail.len() - 2);
                if !pad_part.is_empty() {
                    parts.push(Span::styled(pad_part.to_string(), bg_style(line_bg)));
                }
                parts.push(Span::styled(border_part.to_string(), dim_bg_style(bg)));
            } else {
                parts.push(Span::styled(tail.to_string(), dim_bg_style(bg)));
            }
            rows.push(Line::from(parts));
        }
    }

    // Show new_string lines as added (green bg, `+` prefix)
    for line in new_str.lines() {
        let cleaned = strip_control_chars(line);
        let sign = "+";
        let (line_bg, sign_color) = (
            crate::theme::Theme::diff_added_bg_color(),
            crate::theme::Theme::diff_added_fg(),
        );
        let content = format!("{sign} {cleaned}");
        let wrapped = wrap_line(&content, inner_w.saturating_sub(2));
        for w in &wrapped {
            let prefix_str = format!("{} ", sign);
            let base = box_row_line(&format!("{prefix_str}{w}"), width, bg);
            let base_str: String = base.spans.iter().map(|s| s.content.as_ref()).collect();
            let content_start = 2;
            let sign_end = content_start + prefix_str.len();
            let w_end = sign_end + w.len();
            let mut parts: Vec<Span<'static>> = Vec::new();
            parts.push(Span::styled(
                base_str[..content_start].to_string(),
                dim_bg_style(bg),
            ));
            parts.push(Span::styled(
                base_str[content_start..sign_end].to_string(),
                Style::default().fg(sign_color).bg(bg),
            ));
            parts.push(Span::styled(
                base_str[sign_end..w_end].to_string(),
                bg_style(line_bg),
            ));
            let tail = &base_str[w_end..];
            if tail.len() >= 2 {
                let (pad_part, border_part) = tail.split_at(tail.len() - 2);
                if !pad_part.is_empty() {
                    parts.push(Span::styled(pad_part.to_string(), bg_style(line_bg)));
                }
                parts.push(Span::styled(border_part.to_string(), dim_bg_style(bg)));
            } else {
                parts.push(Span::styled(tail.to_string(), dim_bg_style(bg)));
            }
            rows.push(Line::from(parts));
        }
    }

    rows.push(border_line(width, bg));
    rows
}

pub(super) fn build_shell_command_rows(
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

    // Highlight the shell command with multi-line wrapping
    if let Some(cmd) = title.strip_prefix("$ ") {
        let cmd = strip_control_chars(cmd);
        let max_cmd_width = width.saturating_sub(6); // | $  |
        let cmd_lines = wrap_line(&cmd, max_cmd_width);
        // Highlight all wrapped lines with a single highlighter so
        // syntax state (e.g. open string literals) carries across.
        let cmd_refs: Vec<&str> = cmd_lines.iter().map(|s| s.as_str()).collect();
        let all_hl = crate::session::markdown::highlight_lines(&cmd_refs, "sh");
        for (i, line) in cmd_lines.iter().enumerate() {
            let prefix = if i == 0 { "$ " } else { "  " };
            let content = format!("{prefix}{line}");
            // Use box_row_line to get correct borders/padding string,
            // then overlay syntax highlighting by splitting at content
            // boundaries.
            let base = box_row_line(&content, width, bg);
            let base_str: String = base.spans.iter().map(|s| s.content.as_ref()).collect();
            let content_start = 2; // after "| "
            let cmd_start = content_start + prefix.len();
            let cmd_end = cmd_start + line.len();
            // Get highlighted spans for this line (from the multi-line pass)
            let hl_raw = &all_hl[i];
            let hl_spans = spans_with_bg(hl_raw, bg);
            // Verify hl_spans cover exactly the command text
            let hl_total: usize = hl_spans.iter().map(|s| s.content.len()).sum();
            let hl_spans = if hl_total != line.len() {
                vec![Span::styled(line.to_string(), bg_style(bg))]
            } else {
                hl_spans
            };
            let mut parts: Vec<Span<'static>> = Vec::new();
            parts.push(Span::styled(
                base_str[..content_start].to_string(),
                dim_bg_style(bg),
            ));
            parts.push(Span::styled(
                base_str[content_start..cmd_start].to_string(),
                bg_style(bg),
            ));
            for span in &hl_spans {
                parts.push(span.clone());
            }
            let tail = &base_str[cmd_end..];
            if tail.len() >= 2 {
                let (pad_part, border_part) = tail.split_at(tail.len() - 2);
                if !pad_part.is_empty() {
                    parts.push(Span::styled(pad_part.to_string(), bg_style(bg)));
                }
                parts.push(Span::styled(border_part.to_string(), dim_bg_style(bg)));
            } else {
                parts.push(Span::styled(tail.to_string(), dim_bg_style(bg)));
            }
            rows.push(Line::from(parts));
        }
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
    } else {
        let (preview, skipped) = collapsed_output_lines(output, preview_lines, width, bg);
        rows.extend(preview);
        if skipped > 0 {
            rows.push(click_hint_line(skipped, width, bg));
        }
    }

    if footer.is_empty() {
        rows.push(border_line(width, bg));
    } else {
        rows.push(border_line_with_right_label(width, footer, bg));
    }
    rows
}

/// Render a `read` tool result with syntax highlighting based on the
/// file extension extracted from the title (`read [path ...]`).
#[allow(clippy::too_many_arguments)]
fn build_read_rows(
    tool: &ToolResultBlock,
    visible: bool,
    preview_lines: usize,
    width: usize,
    bg: Color,
) -> Option<Vec<Line<'static>>> {
    let width = width.max(4);
    let inner_w = width.saturating_sub(4).max(1);
    let (output, footer) = tool_display_content(tool);
    let output = output.trim_end();

    // Extract file path from the title to determine the syntax language.
    // Title format: "read [path]" or "read [path start:end]".
    let lang = extract_read_lang(&tool.title);

    let mut rows = Vec::new();
    rows.push(border_with_label_line(width, &tool.title, bg));

    let body_lines: Vec<&str> = output.lines().collect();
    let (skip, show) = if visible || body_lines.len() <= preview_lines {
        (0, body_lines.len())
    } else {
        (body_lines.len() - preview_lines, preview_lines)
    };

    if body_lines.is_empty() {
        rows.extend(box_row_lines("[no output]", width, bg));
    } else if let Some(lang) = lang {
        // Syntax-highlighted rendering.
        let visible_refs: Vec<&str> = body_lines.iter().skip(skip).take(show).copied().collect();
        let all_hl = crate::session::markdown::highlight_lines(&visible_refs, lang);
        for (i, line) in visible_refs.iter().enumerate() {
            let hl = spans_with_bg(&all_hl[i], bg);
            let hl_total: usize = hl.iter().map(|s| s.content.len()).sum();
            if hl_total == line.len() {
                for wrapped in wrap_line(line, inner_w) {
                    if wrapped == *line {
                        rows.push(box_row_line_spans(hl.clone(), width, bg));
                    } else {
                        rows.extend(box_row_lines(&wrapped, width, bg));
                    }
                }
            } else {
                rows.extend(box_row_lines(line, width, bg));
            }
        }
    } else {
        // No language detected — plain rendering.
        for line in body_lines.iter().skip(skip).take(show) {
            rows.extend(box_row_lines(line, width, bg));
        }
    }

    if !visible && skip > 0 {
        rows.push(click_hint_line(skip, width, bg));
    }
    if !footer.is_empty() {
        rows.extend(box_row_lines(&footer, width, bg));
    }

    rows.push(border_line(width, bg));
    Some(rows)
}

/// Extract the file extension from a `read [path ...]` title and map
/// it to a syntax language name understood by `highlight_lines`.
fn extract_read_lang(title: &str) -> Option<&'static str> {
    // Title format: "read [path]" or "read [path start:end]".
    // Extract the path between "[" and the first space or "]".
    let open = title.find('[')?;
    let rest = &title[open + 1..];
    let path_end = rest
        .find(|c: char| c.is_whitespace() || c == ']')
        .unwrap_or(rest.len());
    let path = rest[..path_end].trim();
    if path.is_empty() {
        return None;
    }
    let ext = path.rsplit('.').next()?;
    if ext.is_empty() || ext.len() > 10 || ext == path {
        return None;
    }
    // `find_syntax_cached` matches by extension, but we return the
    // extension itself and let it resolve the syntax.
    // Use a static cache to avoid repeated lookups for the same ext.
    Some(static_ext_lang(ext))
}

/// Map common file extensions to syntax language names.
/// Returns a static str so the caller can use it as a cache key.
fn static_ext_lang(ext: &str) -> &'static str {
    // A small lookup for the most common extensions; everything else
    // falls through to the ext itself which `find_syntax_cached` will
    // try to match by extension (e.g. "rs", "py", "go", "ts").
    // The `Box::leak` here is intentional: these are short-lived per
    // call and the alternative (returning owned String) would force
    // `highlight_lines` to take `&str` vs `&'static str`.
    // But we avoid leaks by using a static table instead.
    match ext.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        "sh" | "bash" => "sh",
        "go" => "go",
        "rb" => "ruby",
        "cpp" | "cc" | "cxx" => "cpp",
        "hpp" | "h" | "hh" => "cpp",
        "cs" => "c#",
        "java" => "java",
        "kt" => "kotlin",
        "swift" => "swift",
        "php" => "php",
        "lua" => "lua",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "md" => "markdown",
        "c" => "c",
        _ => "",
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_output_block_rows(
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
    rows.push(border_with_label_line(width, title, bg));

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
            rows.push(click_hint_line(skipped, width, bg));
        }
    }

    rows.push(border_line(width, bg));
    rows
}

/// Render a tool result block whose body is Markdown (plan, sub_agent).
/// Mirrors `build_output_block_rows` but parses the body through the
/// Markdown renderer so headings, lists, code blocks, tables, etc.
/// are styled the same way as assistant message content and thinking
/// blocks.
#[allow(clippy::too_many_arguments)]
fn build_markdown_block_rows(
    title: &str,
    body: &str,
    footer: &str,
    visible: bool,
    preview_lines: usize,
    width: usize,
    bg: Color,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let mut rows = Vec::new();
    rows.push(border_with_label_line(width, title, bg));
    let inner_w = width.saturating_sub(4).max(1);

    let md_lines = crate::session::markdown::render_with_width(body, inner_w);
    let mut body_rows: Vec<Line<'static>> = Vec::new();
    for line in &md_lines {
        if line.width() <= inner_w {
            let spans = spans_with_bg(&line.spans, bg);
            body_rows.push(box_row_line_spans(spans, width, bg));
        } else {
            let combined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            for w in &wrap_line(&combined, inner_w) {
                let spans = spans_with_bg(&[Span::raw(w.clone())], bg);
                body_rows.push(box_row_line_spans(spans, width, bg));
            }
        }
    }

    if visible {
        if body_rows.is_empty() {
            rows.extend(box_row_lines("[no output]", width, bg));
        } else {
            rows.extend(body_rows);
        }
        if !footer.is_empty() {
            rows.extend(box_row_lines(footer, width, bg));
        }
    } else {
        if body_rows.len() > preview_lines {
            let skip = body_rows.len() - preview_lines;
            body_rows = body_rows.split_off(skip);
            body_rows.push(click_hint_line(skip, width, bg));
        }
        rows.extend(body_rows);
    }

    rows.push(border_line(width, bg));
    rows
}

pub(super) fn output_row_lines(output: &str, width: usize, bg: Color) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for line in output.lines() {
        let line = strip_control_chars(line);
        for wrapped in wrap_line(&line, width.saturating_sub(4)) {
            rows.push(box_row_line(&wrapped, width, bg));
        }
    }
    rows
}

/// Render the last `preview_lines` logical lines of `output` as a
/// collapsed preview block. While the output is shorter than
/// `preview_lines`, the preview grows naturally as content streams in.
/// Once the output reaches `preview_lines` logical lines, the preview
/// height is fixed so the block stops jittering. Returns the rendered
/// rows plus the number of hidden logical lines.
fn collapsed_output_lines(
    output: &str,
    preview_lines: usize,
    width: usize,
    bg: Color,
) -> (Vec<Line<'static>>, usize) {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return (Vec::new(), 0);
    }

    let shown_logical = preview_lines.min(lines.len());
    let skip_logical = lines.len().saturating_sub(shown_logical);

    let mut rows = Vec::new();
    for line in lines.iter().skip(skip_logical) {
        rows.extend(box_row_lines(line, width, bg));
    }

    let mut skipped = lines.len().saturating_sub(shown_logical);

    if rows.len() > preview_lines {
        // Keep the last `preview_lines` display rows so the collapsed
        // block height stays fixed and does not jitter.
        let excess = rows.len() - preview_lines;
        rows.drain(0..excess);
        // Recalculate skipped: count logical lines that are completely
        // hidden after the display-row truncation.
        let mut shown_rows = 0;
        for line in lines.iter().skip(skip_logical).rev() {
            let line_rows = wrap_line(line, width.saturating_sub(4)).len().max(1);
            if shown_rows + line_rows <= preview_lines {
                shown_rows += line_rows;
            } else {
                skipped += 1;
                break;
            }
        }
    } else if lines.len() >= preview_lines {
        while rows.len() < preview_lines {
            rows.push(box_row_line("", width, bg));
        }
    }
    (rows, skipped)
}

/// Single full-width click hint line for collapsed blocks that
/// don't pair the hint with a footer.
fn click_hint_line(skipped: usize, width: usize, bg: Color) -> Line<'static> {
    let line = format!("[click to expand/collapse {skipped} lines]");
    box_row_line(&line, width, bg)
}

/// One row inside a tool box with a left chunk (typically the
/// click hint) and a right chunk (typically the timing footer).
/// The middle is filled with the box background so it still looks
/// like a `box_row_line`. When the chunks would overflow the
/// available inner width, both are shown full-width stacked on
/// separate rows by the caller.
#[allow(dead_code)]
fn box_row_line_two(left: &str, right: &str, width: usize, bg: Color) -> Line<'static> {
    let max_content = width.saturating_sub(4);
    let right = strip_control_chars(right);
    let right_w = visible_width(&right);
    let left_max = max_content.saturating_sub(right_w);
    let left = strip_control_chars(left);
    let left = truncate_str_to_width(&left, left_max);
    let left_w = visible_width(&left);
    let pad = max_content.saturating_sub(left_w).saturating_sub(right_w);
    Line::from(vec![
        Span::styled("| ", dim_bg_style(bg)),
        Span::styled(left, bg_style(bg)),
        Span::styled(" ".repeat(pad), bg_style(bg)),
        Span::styled(right, bg_style(bg)),
        Span::styled(" |", dim_bg_style(bg)),
    ])
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

pub(super) fn box_row_line(text: &str, width: usize, bg: Color) -> Line<'static> {
    let max_content = width.saturating_sub(4);
    let text = strip_control_chars(text);
    let text = truncate_str_to_width(&text, max_content);
    let pad = max_content.saturating_sub(visible_width(&text));
    let line = Line::from(vec![
        Span::styled("| ", dim_bg_style(bg)),
        Span::styled(text.clone(), bg_style(bg)),
        Span::styled(" ".repeat(pad), bg_style(bg)),
        Span::styled(" |", dim_bg_style(bg)),
    ]);
    if line.width() == width {
        line
    } else {
        eprintln!(
            "[box_row_line] width mismatch: Line::width()={} != width={}, pad={}, max_content={}",
            line.width(),
            width,
            pad,
            max_content
        );
        let mut flat = String::new();
        flat.push_str("| ");
        flat.push_str(&text);
        let pad_str = " ".repeat(pad);
        flat.push_str(&pad_str);
        flat.push_str(" |");
        let flat = truncate_str_to_width(&flat, width);
        let flat_pad = width.saturating_sub(visible_width(&flat));
        let flat_str = if flat_pad > 0 {
            let mut s = flat;
            if s.ends_with(" |") {
                let pos = s.len() - 2;
                s.insert_str(pos, &" ".repeat(flat_pad));
            } else {
                s.push_str(&" ".repeat(flat_pad));
            }
            s
        } else {
            flat
        };
        Line::from(Span::styled(flat_str, bg_style(bg)))
    }
}

fn box_row_line_dim(text: &str, width: usize) -> Line<'static> {
    let max_content = width.saturating_sub(4);
    let text = strip_control_chars(text);
    let text = truncate_str_to_width(&text, max_content);
    let pad = max_content.saturating_sub(visible_width(&text));
    let line_str = format!("| {}{} |", text, " ".repeat(pad));
    let line = Line::from(Span::styled(
        line_str.clone(),
        Style::default().add_modifier(Modifier::DIM),
    ));
    if line.width() == width {
        line
    } else {
        eprintln!(
            "[box_row_line_dim] width mismatch: Line::width()={} != width={}, pad={}, max_content={}, text_len={}",
            line.width(),
            width,
            pad,
            max_content,
            visible_width(&text),
        );
        let flat = truncate_str_to_width(&line_str, width);
        let flat_pad = width.saturating_sub(visible_width(&flat));
        let flat_str = if flat_pad > 0 {
            let mut s = flat;
            if s.ends_with(" |") {
                let pos = s.len() - 2;
                s.insert_str(pos, &" ".repeat(flat_pad));
            } else {
                s.push_str(&" ".repeat(flat_pad));
            }
            s
        } else {
            flat
        };
        Line::from(Span::styled(
            flat_str,
            Style::default().add_modifier(Modifier::DIM),
        ))
    }
}

fn box_row_line_spans(spans: Vec<Span<'static>>, width: usize, bg: Color) -> Line<'static> {
    let max_content = width.saturating_sub(4);
    let mut content_width: usize = 0;
    let mut result_spans: Vec<Span<'static>> = Vec::new();
    for span in spans {
        let cleaned = strip_control_chars(span.content.as_ref());
        let cleaned_span = Span::styled(cleaned, span.style);
        let sw = UnicodeWidthStr::width(cleaned_span.content.as_ref());
        if content_width + sw <= max_content {
            content_width += sw;
            result_spans.push(cleaned_span);
        } else {
            let remaining = max_content.saturating_sub(content_width);
            if remaining > 0 {
                let truncated = truncate_str_to_width(cleaned_span.content.as_ref(), remaining);
                if !truncated.is_empty() {
                    result_spans.push(Span::styled(truncated, span.style));
                    content_width +=
                        UnicodeWidthStr::width(result_spans.last().unwrap().content.as_ref());
                }
            }
            break;
        }
    }
    let pad = max_content.saturating_sub(content_width);

    // Build the entire line as a single string to avoid any multi-span
    // rendering discrepancies between unicode-width v0.1 (our crate)
    // and v0.2 (ratatui's crate). Each span's style is preserved by
    // emitting separate spans, but the PADDING and borders are
    // coalesced into the last content span / first border span to
    // minimize the number of span boundaries.
    let mut all_spans: Vec<Span<'static>> = Vec::with_capacity(result_spans.len() + 3);
    all_spans.push(Span::styled("| ", dim_bg_style(bg)));
    let result_spans_clone = result_spans.clone();
    all_spans.extend(result_spans);
    if pad > 0 {
        all_spans.push(Span::styled(" ".repeat(pad), bg_style(bg)));
    }
    all_spans.push(Span::styled(" |", dim_bg_style(bg)));

    // Safety net: if the produced Line::width() (ratatui v0.2) doesn't
    // match `width`, flatten everything into a single Span so ratatui
    // renders it as one atomic string with no grapheme-boundary
    // surprises.
    let line = Line::from(all_spans);
    if line.width() == width {
        line
    } else {
        // Fallback: flatten to a single span. We lose per-span styling
        // but guarantee the width is correct.
        eprintln!(
            "[box_row_line_spans] width mismatch: Line::width()={} != width={}, content_width={}, pad={}, max_content={}",
            line.width(), width, content_width, pad, max_content
        );
        let mut flat = String::new();
        flat.push_str("| ");
        for span in &result_spans_clone {
            flat.push_str(span.content.as_ref());
        }
        if pad > 0 {
            flat.push_str(&" ".repeat(pad));
        }
        flat.push_str(" |");
        // Truncate to exactly `width` chars (display width) as a final guard
        let flat = truncate_str_to_width(&flat, width);
        let flat_pad = width.saturating_sub(visible_width(&flat));
        let flat_str = if flat_pad > 0 {
            // Pad inside the string to reach exactly `width`
            let mut s = flat;
            // Insert padding before the final " |"
            if s.ends_with(" |") {
                let pos = s.len() - 2;
                s.insert_str(pos, &" ".repeat(flat_pad));
            } else {
                s.push_str(&" ".repeat(flat_pad));
            }
            s
        } else {
            flat
        };
        Line::from(Span::styled(flat_str, bg_style(bg)))
    }
}

/// Render an ask-snapshot message (content starts with `---ask---`)
/// as a single `+--- Ask ---+` block. One block per assistant turn,
/// regardless of how many ask tool calls the model emitted in
/// parallel. Each line is wrapped and clipped to the panel width.
pub(super) fn render_ask_snapshot_message(
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

/// Line count for an ask-snapshot message. Mirrors
/// `render_ask_snapshot_message` so the viewport math matches the
/// actual rendered output.
pub fn ask_snapshot_line_count(content: &str, width: usize) -> u32 {
    let width = width.max(8);
    let body = content
        .lines()
        .skip_while(|l| l.trim_start().starts_with("---ask---"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut n: u32 = 1; // top border
    for line in body.lines() {
        n += wrap_line(line, width.saturating_sub(4)).len() as u32;
    }
    n += 1; // bottom border
    n
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

/// Format a `Duration` as an incrementing timer string:
/// - < 60s → `12s`
/// - < 1h  → `2m12s`
/// - ≥ 1h  → `1h2m3s`
fn format_duration(d: std::time::Duration) -> String {
    let total_secs = d.as_secs();
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// Bottom border line with a right-aligned label, mirroring the
/// tool block's footer-in-border style. The label sits flush
/// against the right `+`, separated from the left dashes.
fn border_line_with_right_label(width: usize, label: &str, bg: Color) -> Line<'static> {
    if label.is_empty() || width <= 4 {
        return border_line(width, bg);
    }
    let label_width = visible_width(label);
    let inner = width.saturating_sub(2 + label_width);
    if inner < 3 {
        return border_line(width, bg);
    }
    let line_str = format!("+{}{}+", "-".repeat(inner), label);
    Line::from(Span::styled(line_str, dim_bg_style(bg)))
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
            rows.push(click_hint_line(skipped, width, bg));
        }
    }
    if !footer.is_empty() {
        rows.push(border_line_with_right_label(width, &footer, bg));
    } else {
        rows.push(border_line(width, bg));
    }
    Some(rows)
}

/// Render a `todowrite` tool result as a stylised todo list with
/// status icons and colours inside a boxed block.
fn build_todowrite_rows(
    content: &str,
    visible: bool,
    preview_lines: usize,
    width: usize,
    bg: Color,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let mut rows = Vec::new();

    // Parse todos from the inner JSON
    let todos: Vec<(String, String)> = serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .and_then(|v| {
            v.get("todos")?.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let content = item.get("content")?.as_str()?.to_string();
                        let status = item
                            .get("status")
                            .and_then(|s| s.as_str())
                            .unwrap_or("pending")
                            .to_string();
                        Some((content, status))
                    })
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default();

    let title = format!(" todowrite ({} items) ", todos.len());
    rows.push(border_with_label_line(width, &title, bg));

    let inner_w = width.saturating_sub(4).max(1);

    if todos.is_empty() {
        rows.extend(box_row_lines("(no tasks)", width, bg));
    } else if visible || todos.len() <= preview_lines {
        // Show all items
        for (todo_content, status) in &todos {
            let (icon, fg) = match status.as_str() {
                "completed" => (" \u{2713}", Color::Green),  // ✓
                "in_progress" => (" \u{25CF}", Color::Cyan), // ●
                _ => (" \u{25CB}", Color::Yellow),           // ○
            };
            let line_text = format!("{icon} {todo_content}");
            for wrapped in wrap_line(&line_text, inner_w) {
                let spans = vec![Span::styled(wrapped, Style::default().fg(fg).bg(bg))];
                rows.push(box_row_line_spans(spans, width, bg));
            }
        }
    } else {
        let skip = todos.len() - preview_lines;
        for (todo_content, status) in todos.iter().skip(skip) {
            let (icon, fg) = match status.as_str() {
                "completed" => (" \u{2713}", Color::Green),
                "in_progress" => (" \u{25CF}", Color::Cyan),
                _ => (" \u{25CB}", Color::Yellow),
            };
            let line_text = format!("{icon} {todo_content}");
            for wrapped in wrap_line(&line_text, inner_w) {
                let spans = vec![Span::styled(wrapped, Style::default().fg(fg).bg(bg))];
                rows.push(box_row_line_spans(spans, width, bg));
            }
        }
        rows.push(click_hint_line(skip, width, bg));
    }

    rows.push(border_line(width, bg));
    rows
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
    let inner = crate::session::unwrap_tool_result_content(content);
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

#[derive(Debug, Clone)]
pub(super) enum DiffLineKind {
    Context,
    Removed,
    Added,
}

#[derive(Debug, Clone)]
pub(super) struct DiffLine {
    pub(super) kind: DiffLineKind,
    pub(super) line_no: usize,
    pub(super) content: String,
}

fn build_edit_diff_rows(
    tool: &ToolResultBlock,
    visible: bool,
    preview_lines: usize,
    width: usize,
    bg: Color,
) -> Option<Vec<Line<'static>>> {
    let (path, old, new) = parse_edit_diff(&tool.metadata)?;
    let diff = unified_diff_rows(&old, &new);
    let added = diff
        .iter()
        .filter(|line| matches!(line.kind, DiffLineKind::Added))
        .count();
    let removed = diff
        .iter()
        .filter(|line| matches!(line.kind, DiffLineKind::Removed))
        .count();
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("file");
    let title = format!(" Edit [{path} +{added}/-{removed}] ");
    let lang = ext;

    let width = width.max(4);
    let max_line_width = diff
        .iter()
        .map(|l| l.line_no.to_string().len())
        .max()
        .unwrap_or(0)
        .max(3);
    let mut rows = vec![border_with_label_line(width, &title, bg)];
    if visible {
        if diff.is_empty() {
            rows.extend(box_row_lines("[no changes]", width, bg));
        } else {
            for line in &diff {
                rows.push(diff_box_row_line(line, max_line_width, width, bg, lang));
            }
        }
    } else {
        let shown = preview_lines.min(diff.len());
        let skip = diff.len().saturating_sub(shown);
        for line in diff.iter().skip(skip) {
            rows.push(diff_box_row_line(line, max_line_width, width, bg, lang));
        }
        if skip > 0 {
            rows.push(click_hint_line(skip, width, bg));
        }
    }
    rows.push(border_line(width, bg));
    Some(rows)
}

pub(super) fn diff_box_row_line(
    diff: &DiffLine,
    number_width: usize,
    width: usize,
    bg: Color,
    lang: &str,
) -> Line<'static> {
    let (line_bg, sign) = match diff.kind {
        DiffLineKind::Removed => (crate::theme::Theme::diff_removed_bg_color(), "-"),
        DiffLineKind::Added => (crate::theme::Theme::diff_added_bg_color(), "+"),
        DiffLineKind::Context => (bg, " "),
    };

    let sign_color = match diff.kind {
        DiffLineKind::Removed => crate::theme::Theme::diff_removed_fg(),
        DiffLineKind::Added => crate::theme::Theme::diff_added_fg(),
        DiffLineKind::Context => Color::Reset,
    };

    let number_str = format!("{:>width$} ", diff.line_no, width = number_width);
    let sign_str = sign.to_string();
    let prefix = format!("{sign_str}{number_str}");

    let content = &diff.content;
    let content = strip_control_chars(content);
    let content_spans = crate::session::markdown::highlight_line(&content, lang);
    let content_spans = spans_with_bg(&content_spans, line_bg);

    let prefix_width = unicode_width::UnicodeWidthStr::width(prefix.as_str());
    // Layout: "| " (2) + prefix + "│ " (2) + content + pad + " |" (2) = 6 + prefix + content + pad
    // So inner_w (space for prefix+content+pad) = width - 6
    let inner_w = width.saturating_sub(6);
    let max_content = inner_w.saturating_sub(prefix_width);

    // Truncate content_spans to max_content (mirrors box_row_line_spans logic)
    let mut truncated_spans: Vec<Span<'static>> = Vec::new();
    let mut content_width: usize = 0;
    for span in content_spans {
        let sw = unicode_width::UnicodeWidthStr::width(span.content.as_ref());
        if content_width + sw <= max_content {
            content_width += sw;
            truncated_spans.push(span);
        } else {
            let remaining = max_content.saturating_sub(content_width);
            if remaining > 0 {
                let truncated = truncate_str_to_width(span.content.as_ref(), remaining);
                if !truncated.is_empty() {
                    truncated_spans.push(Span::styled(truncated, span.style));
                    content_width += unicode_width::UnicodeWidthStr::width(
                        truncated_spans.last().unwrap().content.as_ref(),
                    );
                }
            }
            break;
        }
    }

    let pad = max_content.saturating_sub(content_width);

    let prefix_fg = match diff.kind {
        DiffLineKind::Added | DiffLineKind::Removed => line_bg, // diff bg color (green/red)
        DiffLineKind::Context => sign_color,                    // keep context visible
    };
    let mut spans = vec![Span::styled("| ", dim_bg_style(bg))];
    spans.push(Span::styled(
        sign_str,
        Style::default().fg(prefix_fg).bg(bg),
    ));
    spans.push(Span::styled(
        number_str,
        Style::default().fg(prefix_fg).bg(bg),
    ));
    spans.push(Span::styled("│ ", bg_style(line_bg)));
    spans.extend(truncated_spans);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), bg_style(line_bg)));
    }
    spans.push(Span::styled(" |", dim_bg_style(bg)));
    Line::from(spans)
}

fn parse_edit_diff(content: &str) -> Option<(String, String, String)> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    if value.get("kind").and_then(|v| v.as_str()) != Some("edit_diff") {
        return None;
    }
    Some((
        value.get("path")?.as_str()?.to_string(),
        value.get("old")?.as_str()?.to_string(),
        value.get("new")?.as_str()?.to_string(),
    ))
}

fn unified_diff_rows(old: &str, new: &str) -> Vec<DiffLine> {
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

    let mut rows = Vec::new();
    for (idx, line) in old_lines
        .iter()
        .enumerate()
        .take(prefix)
        .skip(context_start)
    {
        rows.push(DiffLine {
            kind: DiffLineKind::Context,
            line_no: idx + 1,
            content: line.to_string(),
        });
    }
    for (idx, line) in old_lines
        .iter()
        .enumerate()
        .take(old_change_end)
        .skip(prefix)
    {
        rows.push(DiffLine {
            kind: DiffLineKind::Removed,
            line_no: idx + 1,
            content: line.to_string(),
        });
    }
    for (idx, line) in new_lines
        .iter()
        .enumerate()
        .take(new_change_end)
        .skip(prefix)
    {
        rows.push(DiffLine {
            kind: DiffLineKind::Added,
            line_no: idx + 1,
            content: line.to_string(),
        });
    }
    for (idx, line) in old_lines
        .iter()
        .enumerate()
        .take(old_change_end.saturating_add(context_after))
        .skip(old_change_end)
    {
        rows.push(DiffLine {
            kind: DiffLineKind::Context,
            line_no: idx + 1,
            content: line.to_string(),
        });
    }
    rows
}

fn command_display_content(content: &str) -> (String, String) {
    let content = crate::session::unwrap_tool_result_content(content);
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

    (output, format_wall_timeout_label(wall, timeout))
}

fn format_wall_timeout_label(wall: &str, timeout: &str) -> String {
    let wall_secs = wall.parse::<f64>().map(|f| f.round() as u64).unwrap_or(0);
    let wall_dur = std::time::Duration::from_secs(wall_secs);
    let timeout_dur = std::time::Duration::from_secs(timeout.parse::<u64>().unwrap_or(300));
    format!(
        "[{}|{}]",
        format_duration(wall_dur),
        format_duration(timeout_dur)
    )
}
