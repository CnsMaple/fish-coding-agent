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
        if matches!(tool.name.as_str(), "shell_command" | "command") {
            // Prefer the full command from the title (set by
            // `ToolStarted` via `tool_result_title`); fall back to the
            // partial streaming JSON while the LLM is still assembling
            // the arguments. Using the title guarantees the command
            // text stays visible once the tool starts running, even
            // when parallel streaming deltas are interleaved.
            let cmd = shell_command_title(tool);
            let timeout =
                crate::commands::extract_partial_json_u64(&tool.streaming_input, "timeout_secs")
                    .unwrap_or(300);
            let rows = build_streaming_shell_rows(&cmd, width, bg, tool.started_at, timeout);
            if !rows.is_empty() {
                return rows;
            }
        } else if !tool.streaming_input.is_empty() {
            let rows = build_streaming_tool_rows(tool, width, bg);
            if !rows.is_empty() {
                return rows;
            }
        }
        // No usable streaming input yet — render nothing so the
        // block occupies no vertical space until content arrives.
        return vec![];
    }

    let mut rows: Vec<Line<'static>> = if tool.name == "edit" || tool.name == "write" {
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
            let timeout =
                crate::commands::extract_partial_json_u64(&tool.streaming_input, "timeout_secs")
                    .unwrap_or(300);
            format!(
                "[{}|{}]",
                format_duration(std::time::Duration::from_secs(elapsed as u64)),
                format_duration(std::time::Duration::from_secs(timeout))
            )
        } else {
            footer
        };
        if title_highlighted {
            let shell_title = if tool.title.is_empty() {
                shell_command_title(tool)
            } else {
                tool.title.clone()
            };
            build_shell_command_rows(
                &shell_title,
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
            let timeout =
                crate::commands::extract_partial_json_u64(args, "timeout_secs").unwrap_or(300);
            build_streaming_shell_rows(&cmd, width, bg, tool.started_at, timeout)
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
    timeout_secs: u64,
) -> Vec<Line<'static>> {
    let width = width.max(4);
    let mut rows = Vec::new();
    rows.push(border_with_label_line(width, " shell ", bg));

    let max_cmd_width = width.saturating_sub(6); // | $  |
    let cmd_lines = wrap_line(cmd, max_cmd_width);
    let cmd_refs: Vec<&str> = cmd_lines.iter().map(|s| s.as_str()).collect();
    let all_hl = crate::session::markdown::highlight_lines(&cmd_refs, "sh");

    for (i, _line) in cmd_lines.iter().enumerate() {
        let prefix = if i == 0 { "$ " } else { "  " };
        let hl_raw = &all_hl[i];
        let hl_spans = spans_with_bg(hl_raw, bg);
        let mut content_spans: Vec<Span<'static>> = Vec::with_capacity(hl_spans.len() + 1);
        content_spans.push(Span::styled(prefix.to_string(), bg_style(bg)));
        content_spans.extend(hl_spans);
        rows.push(box_row_line_spans(content_spans, width, bg));
    }

    rows.push(border_with_label_line(width, " Output ", bg));
    rows.extend(box_row_lines("…", width, bg));
    if let Some(start) = started_at {
        let elapsed = (chrono::Utc::now() - start).num_seconds().max(0);
        let label = format!(
            "[{}|{}]",
            format_duration(std::time::Duration::from_secs(elapsed as u64)),
            format_duration(std::time::Duration::from_secs(timeout_secs))
        );
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
        for (i, _w) in wrapped.iter().enumerate() {
            let hl = spans_with_bg(&all_hl[i], bg);
            rows.push(box_row_line_spans(hl, width, bg));
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
            let content = w.strip_prefix(&prefix_str).unwrap_or(w);
            let spans = vec![
                Span::styled(prefix_str.clone(), Style::default().fg(sign_color).bg(bg)),
                Span::styled(content.to_string(), bg_style(line_bg)),
            ];
            rows.push(box_row_line_spans(spans, width, bg));
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
            let content = w.strip_prefix(&prefix_str).unwrap_or(w);
            let spans = vec![
                Span::styled(prefix_str.clone(), Style::default().fg(sign_color).bg(bg)),
                Span::styled(content.to_string(), bg_style(line_bg)),
            ];
            rows.push(box_row_line_spans(spans, width, bg));
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
    rows.push(border_with_label_line(width, " shell ", bg));

    // Highlight the shell command with multi-line wrapping
    if let Some(cmd) = title.strip_prefix("$ ") {
        let cmd = strip_control_chars(cmd);
        let max_cmd_width = width.saturating_sub(6); // | $  |
        let cmd_lines = wrap_line(&cmd, max_cmd_width);
        // Highlight all wrapped lines with a single highlighter so
        // syntax state (e.g. open string literals) carries across.
        let cmd_refs: Vec<&str> = cmd_lines.iter().map(|s| s.as_str()).collect();
        let all_hl = crate::session::markdown::highlight_lines(&cmd_refs, "sh");
        for (i, _line) in cmd_lines.iter().enumerate() {
            let prefix = if i == 0 { "$ " } else { "  " };
            let hl_raw = &all_hl[i];
            let hl_spans = spans_with_bg(hl_raw, bg);
            let mut content_spans: Vec<Span<'static>> = Vec::with_capacity(hl_spans.len() + 1);
            content_spans.push(Span::styled(prefix.to_string(), bg_style(bg)));
            content_spans.extend(hl_spans);
            rows.push(box_row_line_spans(content_spans, width, bg));
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

/// Resolve the command text to show for a shell tool block. Prefer the
/// full command from the block title (set by `ToolStarted` via
/// `tool_result_title`); fall back to the partial streaming JSON while
/// the LLM is still assembling the arguments. Used both while the block
/// is streaming (`tool.content` empty) and once output has arrived, so
/// the command stays visible across the whole run even when parallel
/// streaming deltas are interleaved.
fn shell_command_title(tool: &ToolResultBlock) -> String {
    if !tool.title.is_empty() {
        tool.title
            .strip_prefix("$ ")
            .unwrap_or(&tool.title)
            .to_string()
    } else {
        crate::commands::extract_partial_json_field(&tool.streaming_input, "command")
            .unwrap_or_default()
    }
}

/// Derive a `read [path start:end]` title from the raw streaming JSON
/// arguments when the block has no title yet (e.g. a placeholder block
/// created by `ToolInputDelta` before `ToolStarted` lands). Keeps the
/// block header labelled even during the streaming window.
fn read_title_from_input(args: &str) -> String {
    let path = crate::commands::extract_partial_json_field(args, "path")
        .unwrap_or_default()
        .trim()
        .to_string();
    let start = crate::commands::extract_partial_json_u64(args, "start_line");
    let end = crate::commands::extract_partial_json_u64(args, "end_line");
    let range = match (start, end) {
        (Some(s), Some(e)) => format!("{}:{}", s, e),
        (Some(s), None) => format!("{}:", s),
        (None, Some(e)) => format!(":{}", e),
        (None, None) => String::new(),
    };
    if path.is_empty() {
        "read".to_string()
    } else if range.is_empty() {
        format!("read [{}]", path)
    } else {
        format!("read [{} {}]", path, range)
    }
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
    let title = if tool.title.is_empty() {
        read_title_from_input(&tool.streaming_input)
    } else {
        tool.title.clone()
    };
    let lang = extract_read_lang(&title);

    let mut rows = Vec::new();
    rows.push(border_with_label_line(width, &title, bg));

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
            if hl.is_empty() {
                rows.extend(box_row_lines(line, width, bg));
                continue;
            }
            for wrapped in crate::session::markdown::wrap_cell(&hl, inner_w.max(1)) {
                rows.push(box_row_line_spans(wrapped, width, bg));
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
        tracing::warn!(
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
        tracing::warn!(
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
        tracing::warn!(
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
    // Normalise top-left box labels: trim, lowercase (all block headers
    // are displayed lowercase), then pad with a single space on each
    // side so the border reads `+--- label ---+` with consistent
    // spacing regardless of what each caller passed in.
    let label = format!(" {} ", label.trim().to_ascii_lowercase());
    let label_width = visible_width(&label);
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

/// Format a `Duration` as an incrementing timer string, omitting
/// zero leading components:
/// - < 60s → `12s`
/// - < 1h  → `2m12s` (or `2m` for exactly 2 minutes)
/// - ≥ 1h  → `1h2m3s`
pub(super) fn format_duration(d: std::time::Duration) -> String {
    let total_secs = d.as_secs();
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    let mut parts: Vec<String> = Vec::new();
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if h > 0 || m > 0 {
        parts.push(format!("{m}m"));
    }
    if h == 0 && m == 0 {
        parts.push(format!("{s}s"));
    }
    parts.join("")
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
        let cleaned = strip_control_chars(line);
        let spans = crate::session::markdown::highlight_line(&cleaned, "python");
        let spans = spans_with_bg(&spans, bg);
        for wrapped in crate::session::markdown::wrap_cell(&spans, width.saturating_sub(4).max(1)) {
            rows.push(box_row_line_spans(wrapped, width, bg));
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
    if tool.name == "sub_agent" {
        if let Some(body) = sub_agent_tool_display(&tool.content) {
            return (body, String::new());
        }
    }
    if tool.name == "update_title" {
        if let Some(title) = update_title_display(&tool.content) {
            return (title, String::new());
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

/// Render a `sub_agent` tool result in the session. The sub-agent's
/// final text reply is Markdown, so it is unwrapped from the
/// `{"ok":true,"result":"…"}` envelope and returned for Markdown block
/// rendering (headings, lists, tables, code blocks).
fn sub_agent_tool_display(content: &str) -> Option<String> {
    let inner = crate::session::unwrap_tool_result_content(content);
    if inner.is_empty() {
        return None;
    }
    Some(inner.trim().to_string())
}

fn update_title_display(content: &str) -> Option<String> {
    let inner = crate::session::unwrap_tool_result_content(content);
    let value: serde_json::Value = serde_json::from_str(&inner).ok()?;
    if value.get("kind").and_then(|v| v.as_str()) != Some("update_title") {
        return None;
    }
    value
        .get("title")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[derive(Debug, Clone)]
pub(super) enum DiffLineKind {
    Context,
    Removed,
    Added,
    /// A line present on both sides but with different content.
    Modified,
}

/// An aligned old/new row. Both sides present for `Context`/`Modified`;
/// `Added`/`Removed` have one side `None`.
#[derive(Debug, Clone)]
pub(super) struct DiffRow {
    pub(super) kind: DiffLineKind,
    pub(super) old_no: Option<usize>,
    pub(super) old_content: String,
    pub(super) new_no: Option<usize>,
    pub(super) new_content: String,
}

fn build_edit_diff_rows(
    tool: &ToolResultBlock,
    visible: bool,
    preview_lines: usize,
    width: usize,
    bg: Color,
) -> Option<Vec<Line<'static>>> {
    let (path, old, new) = parse_edit_diff(&tool.metadata)?;
    let rows = aligned_diff_rows(&old, &new);
    let added = rows
        .iter()
        .filter(|r| matches!(r.kind, DiffLineKind::Added | DiffLineKind::Modified))
        .count();
    let removed = rows
        .iter()
        .filter(|r| matches!(r.kind, DiffLineKind::Removed | DiffLineKind::Modified))
        .count();
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("file");
    let title = format!(" Edit [{path} +{added}/-{removed}] ");
    let lang = ext;

    let width = width.max(4);
    let nw = rows
        .iter()
        .filter_map(|r| r.old_no.or(r.new_no))
        .map(|n| n.to_string().len())
        .max()
        .unwrap_or(0)
        .max(3);
    let colors = crate::theme::active_colors();
    let mut body: Vec<Line<'static>> = Vec::new();
    let mut i = 0usize;
    while i < rows.len() {
        let big = matches!(rows[i].kind, DiffLineKind::Modified)
            && is_big_change(&rows[i].old_content, &rows[i].new_content);
        if big {
            // Group consecutive big-modified rows: emit all `-` sides then
            // all `+` sides so the output reads `---+++` instead of `-+-+-+`.
            let mut j = i;
            while j < rows.len()
                && matches!(rows[j].kind, DiffLineKind::Modified)
                && is_big_change(&rows[j].old_content, &rows[j].new_content)
            {
                j += 1;
            }
            for r in &rows[i..j] {
                body.push(diff_box_line(
                    &DiffSide {
                        sign: "-",
                        line_no: r.old_no.unwrap_or(r.new_no.unwrap_or(0)),
                        content: &r.old_content,
                        ranges: &[],
                    },
                    nw,
                    width,
                    bg,
                    lang,
                    &colors,
                ));
            }
            for r in &rows[i..j] {
                body.push(diff_box_line(
                    &DiffSide {
                        sign: "+",
                        line_no: r.new_no.unwrap_or(r.old_no.unwrap_or(0)),
                        content: &r.new_content,
                        ranges: &[],
                    },
                    nw,
                    width,
                    bg,
                    lang,
                    &colors,
                ));
            }
            i = j;
        } else {
            body.extend(diff_box_row_line(&rows[i], nw, width, bg, lang));
            i += 1;
        }
    }
    let mut out = vec![border_with_label_line(width, &title, bg)];
    if body.is_empty() {
        out.extend(box_row_lines("[no changes]", width, bg));
    } else if visible {
        out.extend(body);
    } else {
        let shown = preview_lines.min(body.len());
        let skip = body.len().saturating_sub(shown);
        for l in body.iter().skip(skip) {
            out.push(l.clone());
        }
        if skip > 0 {
            out.push(click_hint_line(skip, width, bg));
        }
    }
    out.push(border_line(width, bg));
    Some(out)
}

/// Render a unified (top/bottom) diff row as full-width lines.
/// `Modified` rows produce a single `~` line with removed/new characters
/// highlighted inline; `Added`/`Removed` rows produce a single whole-line
/// highlighted `+`/`-` row. `Context` rows produce a single plain line.
/// Unchanged characters use the plain background; changed characters are
/// painted with the deeper `diff_*_fg` color as background.
/// One side of a diff row to render in the unified layout.
struct DiffSide<'a> {
    sign: &'a str,
    line_no: usize,
    content: &'a str,
    ranges: &'a [(usize, usize)],
}

pub(super) fn diff_box_row_line(
    row: &DiffRow,
    number_width: usize,
    width: usize,
    bg: Color,
    lang: &str,
) -> Vec<Line<'static>> {
    let colors = crate::theme::active_colors();

    let sides: Vec<DiffSide<'_>> = match row.kind {
        DiffLineKind::Removed => vec![DiffSide {
            sign: "-",
            line_no: row.old_no.unwrap_or(0),
            content: &row.old_content,
            ranges: &[],
        }],
        DiffLineKind::Added => vec![DiffSide {
            sign: "+",
            line_no: row.new_no.unwrap_or(0),
            content: &row.new_content,
            ranges: &[],
        }],
        DiffLineKind::Modified => {
            let line_no = row.old_no.or(row.new_no).unwrap_or(0);
            return vec![diff_box_line_modified(
                line_no,
                &row.old_content,
                &row.new_content,
                number_width,
                width,
                bg,
                &colors,
            )];
        }
        DiffLineKind::Context => {
            let content = if !row.old_content.is_empty() {
                &row.old_content
            } else {
                &row.new_content
            };
            vec![DiffSide {
                sign: " ",
                line_no: row.old_no.or(row.new_no).unwrap_or(0),
                content,
                ranges: &[],
            }]
        }
    };

    sides
        .into_iter()
        .map(|side| diff_box_line(&side, number_width, width, bg, lang, &colors))
        .collect()
}

/// Render a single full-width diff line for one side of a diff row.
fn diff_box_line(
    side: &DiffSide<'_>,
    number_width: usize,
    width: usize,
    bg: Color,
    lang: &str,
    colors: &crate::theme::ThemeColors,
) -> Line<'static> {
    let (line_bg, hl_bg, hl_fg) = match side.sign {
        "-" => (
            colors.diff_removed_bg,
            colors.diff_removed_fg,
            colors.tool_error_fg,
        ),
        "+" => (
            colors.diff_added_bg,
            colors.diff_added_fg,
            colors.tool_success_bg,
        ),
        _ => (bg, bg, Color::Reset),
    };
    let number_str = format!("{:>width$} ", side.line_no, width = number_width);
    let prefix = format!("{}{}", side.sign, number_str);
    let prefix_width = unicode_width::UnicodeWidthStr::width(prefix.as_str());

    let inner_w = width.saturating_sub(6);
    let max_content = inner_w.saturating_sub(prefix_width);

    let content = strip_control_chars(side.content);
    let content = truncate_str_to_width(&content, max_content);

    // Constrain char ranges to the (possibly truncated) content length.
    let eff_ranges: Vec<(usize, usize)> = if side.sign == "-" || side.sign == "+" {
        side.ranges
            .iter()
            .filter(|(s, _)| *s < content.len())
            .map(|(s, e)| (*s, (*e).min(content.len())))
            .collect()
    } else {
        Vec::new()
    };

    let base_spans = crate::session::markdown::highlight_line(&content, lang);
    let base_spans = spans_with_bg(&base_spans, line_bg);
    let content_spans = content_spans_highlighted_ranges(base_spans, &eff_ranges, hl_bg, hl_fg);
    let content_width: usize = content_spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let pad = max_content.saturating_sub(content_width);

    let sign_fg = match side.sign {
        "-" => colors.diff_removed_fg,
        "+" => colors.diff_added_fg,
        _ => Color::Reset,
    };
    let mut spans = vec![Span::styled("| ", dim_bg_style(bg))];
    spans.push(Span::styled(
        side.sign.to_string(),
        Style::default().fg(sign_fg).bg(bg),
    ));
    spans.push(Span::styled(number_str, Style::default().bg(bg)));
    spans.push(Span::styled("│ ", bg_style(line_bg)));
    spans.extend(content_spans);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), bg_style(line_bg)));
    }
    spans.push(Span::styled(" |", dim_bg_style(bg)));
    Line::from(spans)
}

/// Render a single `Modified` line: unchanged characters plain, deleted
/// characters red and added characters green, all on one line prefixed
/// with `~`. Pure additions/removals are handled by [`diff_box_line`].
fn diff_box_line_modified(
    line_no: usize,
    old_content: &str,
    new_content: &str,
    number_width: usize,
    width: usize,
    bg: Color,
    colors: &crate::theme::ThemeColors,
) -> Line<'static> {
    let number_str = format!("{:>width$} ", line_no, width = number_width);
    let prefix = format!("~{number_str}");
    let prefix_width = unicode_width::UnicodeWidthStr::width(prefix.as_str());

    let inner_w = width.saturating_sub(6);
    let max_content = inner_w.saturating_sub(prefix_width);

    let old_content = strip_control_chars(old_content);
    let new_content = strip_control_chars(new_content);

    // Interleave unchanged, removed (red) and added (green) runs into a
    // single inline line, truncating to max_content.
    let spans = merge_inline_diff_spans(
        &old_content,
        &new_content,
        max_content,
        bg,
        colors.diff_removed_bg,
        colors.diff_removed_fg,
        colors.diff_added_bg,
        colors.diff_added_fg,
    );
    let content_w: usize = spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let pad = max_content.saturating_sub(content_w);

    let mut spans_out = vec![Span::styled("| ", dim_bg_style(bg))];
    spans_out.push(Span::styled(
        "~".to_string(),
        Style::default().fg(colors.diff_removed_fg).bg(bg),
    ));
    spans_out.push(Span::styled(number_str, Style::default().bg(bg)));
    spans_out.push(Span::styled("│ ", bg_style(bg)));
    spans_out.extend(spans);
    if pad > 0 {
        spans_out.push(Span::styled(" ".repeat(pad), bg_style(bg)));
    }
    spans_out.push(Span::styled(" |", dim_bg_style(bg)));
    Line::from(spans_out)
}

/// A segment of a word-level diff: either matching text on both sides,
/// text only present in the old line, or text only present in the new line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffSegKind {
    Same,
    Removed,
    Added,
}

/// Split a line into alternating whitespace and non-whitespace word tokens.
/// Diffing at word granularity means a common leading indent stays plain
/// while a changed first word is highlighted on its own.
fn tokenize_words(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut cur_ws = false;
    for ch in s.chars() {
        let ws = ch.is_whitespace();
        if cur.is_empty() {
            cur.push(ch);
            cur_ws = ws;
        } else if ws == cur_ws {
            cur.push(ch);
        } else {
            tokens.push(std::mem::take(&mut cur));
            cur.push(ch);
            cur_ws = ws;
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// LCS-align two lines word-by-word, collapsing adjacent segments of the
/// same kind. Common words come back as `Same`, old-only as `Removed`,
/// new-only as `Added`.
fn word_diff_segments(old: &str, new: &str) -> Vec<(DiffSegKind, String)> {
    let old_tokens = tokenize_words(old);
    let new_tokens = tokenize_words(new);
    let (n, m) = (old_tokens.len(), new_tokens.len());

    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old_tokens[i] == new_tokens[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut segs: Vec<(DiffSegKind, String)> = Vec::new();
    let push = |kind: DiffSegKind, text: &str, segs: &mut Vec<(DiffSegKind, String)>| {
        if text.is_empty() {
            return;
        }
        if let Some((last_kind, last_text)) = segs.last_mut() {
            if *last_kind == kind {
                last_text.push_str(text);
                return;
            }
        }
        segs.push((kind, text.to_string()));
    };

    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old_tokens[i] == new_tokens[j] {
            push(DiffSegKind::Same, &old_tokens[i], &mut segs);
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            push(DiffSegKind::Removed, &old_tokens[i], &mut segs);
            i += 1;
        } else {
            push(DiffSegKind::Added, &new_tokens[j], &mut segs);
            j += 1;
        }
    }
    while i < n {
        push(DiffSegKind::Removed, &old_tokens[i], &mut segs);
        i += 1;
    }
    while j < m {
        push(DiffSegKind::Added, &new_tokens[j], &mut segs);
        j += 1;
    }
    segs
}

/// Merge unchanged / removed / added word runs into one inline span list
/// (common words plain, removed words red, added words green), truncated to
/// `max_width` columns.
#[allow(clippy::too_many_arguments)]
fn merge_inline_diff_spans(
    old_content: &str,
    new_content: &str,
    max_width: usize,
    base_bg: Color,
    rm_bg: Color,
    rm_fg: Color,
    add_bg: Color,
    add_fg: Color,
) -> Vec<Span<'static>> {
    let segs = word_diff_segments(old_content, new_content);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut width_used = 0usize;
    for (kind, text) in segs {
        let style = match kind {
            DiffSegKind::Same => Style::default().bg(base_bg),
            DiffSegKind::Removed => Style::default()
                .bg(rm_bg)
                .fg(rm_fg)
                .add_modifier(Modifier::BOLD),
            DiffSegKind::Added => Style::default()
                .bg(add_bg)
                .fg(add_fg)
                .add_modifier(Modifier::BOLD),
        };
        // `flush_run` takes a mutable buffer and clears it; pass an owned
        // copy so the truncated remainder is emitted properly.
        let mut owned = text;
        flush_run(&mut owned, style, &mut spans, &mut width_used, max_width);
    }
    spans
}

/// Flush the buffered run into `spans`, truncating to `max_width`.
fn flush_run(
    buf: &mut String,
    style: Style,
    spans: &mut Vec<Span<'static>>,
    width_used: &mut usize,
    max_width: usize,
) {
    if buf.is_empty() || *width_used >= max_width {
        buf.clear();
        return;
    }
    let w = unicode_width::UnicodeWidthStr::width(buf.as_str());
    if *width_used + w > max_width {
        let room = max_width - *width_used;
        let cut = truncate_str_to_width(buf, room);
        if !cut.is_empty() {
            spans.push(Span::styled(cut, style));
        }
        *width_used = max_width;
    } else {
        spans.push(Span::styled(buf.clone(), style));
        *width_used += w;
    }
    buf.clear();
}

/// Overlay character-level highlight ranges on top of syntax-highlighted
/// spans. Each span is split so that bytes inside a range get `hl_bg`
/// (deeper color) with `hl_fg` text, while the rest keeps `line_bg`.
fn content_spans_highlighted_ranges(
    base_spans: Vec<Span<'static>>,
    ranges: &[(usize, usize)],
    hl_bg: Color,
    hl_fg: Color,
) -> Vec<Span<'static>> {
    if ranges.is_empty() {
        return base_spans;
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut pos = 0usize;
    for span in base_spans {
        let text = span.content.as_ref();
        let span_start = pos;
        let span_end = pos + text.len();
        pos = span_end;
        // Walk byte-by-byte, splitting at range boundaries.
        let mut cursor = span_start;
        for &(rs, re) in ranges {
            let rs = rs.max(span_start).min(span_end);
            let re = re.max(span_start).min(span_end);
            if rs >= re {
                continue;
            }
            if rs > cursor {
                out.push(Span::styled(
                    text[cursor - span_start..rs - span_start].to_string(),
                    span.style,
                ));
            }
            out.push(Span::styled(
                text[rs - span_start..re - span_start].to_string(),
                Style::default()
                    .bg(hl_bg)
                    .fg(hl_fg)
                    .add_modifier(Modifier::BOLD),
            ));
            cursor = re;
        }
        if cursor < span_end {
            out.push(Span::styled(
                text[cursor - span_start..].to_string(),
                span.style,
            ));
        }
    }
    out
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

/// Whether a modified line changed in more than 60% of its (longer-side)
/// characters. Big changes render as a `-`/`+` pair rather than one `~` row.
fn is_big_change(old: &str, new: &str) -> bool {
    let old_n = old.chars().count();
    let new_n = new.chars().count();
    if old_n == 0 && new_n == 0 {
        return false;
    }

    // LCS length over characters.
    let old_chars: Vec<char> = old.chars().collect();
    let new_chars: Vec<char> = new.chars().collect();
    let (n, m) = (old_chars.len(), new_chars.len());
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old_chars[i] == new_chars[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let lcs = dp[0][0];
    let longer = old_n.max(new_n);
    if longer == 0 {
        return false;
    }
    // Removed chars (old minus LCS) plus added chars (new minus LCS).
    let changed = old_n + new_n - 2 * lcs;
    changed * 100 >= longer * 60
}

/// Align old/new lines into paired rows for the unified renderer.
fn aligned_diff_rows(old: &str, new: &str) -> Vec<DiffRow> {
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
    for idx in context_start..prefix {
        rows.push(DiffRow {
            kind: DiffLineKind::Context,
            old_no: Some(idx + 1),
            old_content: old_lines[idx].to_string(),
            new_no: Some(idx + 1),
            new_content: new_lines[idx].to_string(),
        });
    }
    let pair_count = (old_change_end - prefix).max(new_change_end - prefix);
    for k in 0..pair_count {
        let o = (prefix + k < old_change_end).then(|| old_lines[prefix + k]);
        let n = (prefix + k < new_change_end).then(|| new_lines[prefix + k]);
        let no = (prefix + k + 1).to_string();
        let nn = (prefix + k + 1).to_string();
        match (o, n) {
            (Some(o), Some(n)) => {
                let kind = if o == n {
                    DiffLineKind::Context
                } else {
                    DiffLineKind::Modified
                };
                rows.push(DiffRow {
                    kind,
                    old_no: no.parse().ok(),
                    old_content: o.to_string(),
                    new_no: nn.parse().ok(),
                    new_content: n.to_string(),
                });
            }
            (Some(o), None) => rows.push(DiffRow {
                kind: DiffLineKind::Removed,
                old_no: no.parse().ok(),
                old_content: o.to_string(),
                new_no: None,
                new_content: String::new(),
            }),
            (None, Some(n)) => rows.push(DiffRow {
                kind: DiffLineKind::Added,
                old_no: None,
                old_content: String::new(),
                new_no: nn.parse().ok(),
                new_content: n.to_string(),
            }),
            (None, None) => {}
        }
    }
    for k in 0..context_after {
        let o_idx = old_change_end + k;
        let n_idx = new_change_end + k;
        rows.push(DiffRow {
            kind: DiffLineKind::Context,
            old_no: Some(o_idx + 1),
            old_content: old_lines[o_idx].to_string(),
            new_no: Some(n_idx + 1),
            new_content: new_lines[n_idx].to_string(),
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
