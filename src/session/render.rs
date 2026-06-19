use super::{Role, Session, ToolResultBlock};
use crate::config::{ThinkingDisplay, ToolResultDisplay};
use crate::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
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
    let start = total.saturating_sub(inner_h as u16 + scroll);
    let end = total.saturating_sub(scroll);

    tool_toggle_rows.clear();

    let visible: Vec<Line> = if start < end {
        lines[start as usize..end as usize].to_vec()
    } else {
        vec![]
    };

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
    for m in &session.messages {
        let role_style = match m.role {
            Role::User => Theme::role_user(),
            Role::Assistant => Theme::role_assistant(),
            Role::System => Theme::role_system(),
        };
        let arrow = Span::styled(" › ", role_style);
        let prefix = Span::styled(m.role.prefix(), role_style);

        // Role prefix on its own line; content and blocks start below it.
        out.push(Line::from(vec![prefix.clone(), arrow.clone()]));

        let show_thinking = m.role == Role::Assistant
            && !m.thinking.trim().is_empty()
            && match session.display {
                ThinkingDisplay::Hide => false,
                ThinkingDisplay::Show => true,
                ThinkingDisplay::ShowWhileStreaming => true,
            };
        if show_thinking {
            let visible = match session.display {
                ThinkingDisplay::Show => m.thinking_visible,
                ThinkingDisplay::ShowWhileStreaming => m.streaming || m.thinking_visible,
                _ => false,
            };
            let rows = build_thinking_block_rows(&m.thinking, visible, width);
            push_block_rows(&mut out, rows, block_style(m.streaming));
        }

        // Render tool blocks at the content offset where the tool result
        // arrived. This keeps command output near the assistant text that
        // triggered it instead of moving every block to the message tail.
        let raw = if m.streaming {
            m.visible_content()
        } else {
            &m.content
        };
        let mut cursor = 0usize;
        let mut tools: Vec<&ToolResultBlock> = m.tool_results.iter().collect();
        tools.sort_by_key(|tool| tool.content_offset);
        for tool in tools {
            let offset = clamp_char_boundary(raw, tool.content_offset.min(raw.len()));
            if offset < cursor {
                continue;
            }
            render_content_segment(&strip_legacy_markers(&raw[cursor..offset]), width, &mut out);
            cursor = offset;

            if session.tool_display != ToolResultDisplay::Hide {
                let t_vis = match session.tool_display {
                    ToolResultDisplay::Show => tool.visible,
                    ToolResultDisplay::ShowWhileStreaming => m.streaming || tool.visible,
                    _ => false,
                };
                let rows = build_tool_block_rows(tool, t_vis, width);
                push_block_rows(&mut out, rows, block_style_for_tool(tool));
            }
        }
        render_content_segment(&strip_legacy_markers(&raw[cursor..]), width, &mut out);

        if m.streaming {
            if let Some(last) = out.last_mut() {
                let mut s = last.spans.clone();
                s.push(Span::styled("▌", Theme::cursor()));
                *last = Line::from(s);
            } else {
                out.push(Line::from(Span::styled("▌", Theme::cursor())));
            }
        }
        out.push(Line::from(""));
    }
    while out.last().map(|l| l.width() == 0).unwrap_or(false) {
        out.pop();
    }
    if !out.is_empty() {
        out.push(Line::from(""));
    }
    (out, Vec::new())
}

/// Strip any remaining `[tool:...]` markers from old session content.
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
    build_thinking_block_rows(content, visible, width).len()
}

pub fn tool_block_line_count(tool: &ToolResultBlock, visible: bool, width: usize) -> usize {
    build_tool_block_rows(tool, visible, width).len()
}

fn push_block_rows(out: &mut Vec<Line<'static>>, rows: Vec<String>, style: Style) {
    for row in rows {
        out.push(Line::from(Span::styled(row, style)));
    }
}

fn block_style(running: bool) -> Style {
    if running {
        Theme::block_running()
    } else {
        Theme::block_done()
    }
}

fn block_style_for_tool(tool: &ToolResultBlock) -> Style {
    let failed = match tool.name.as_str() {
        "shell_command" | "command" => command_failed(&tool.content),
        "python_command" => python_command_failed(&tool.content),
        _ => false,
    };
    if failed {
        Theme::block_failed()
    } else {
        Theme::block_done()
    }
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

fn build_thinking_block_rows(content: &str, visible: bool, width: usize) -> Vec<String> {
    build_output_block_rows(
        "thinking",
        " Thinking ",
        content.trim_end(),
        "[click to collapse/expand]",
        visible,
        width,
        "click to expand",
    )
}

fn build_tool_block_rows(tool: &ToolResultBlock, visible: bool, width: usize) -> Vec<String> {
    if tool.name == "write_file" {
        if let Some(rows) = build_write_file_diff_rows(tool, visible, width) {
            return rows;
        }
    }
    if tool.name == "python_command" {
        if let Some(rows) = build_python_command_rows(tool, visible, width) {
            return rows;
        }
    }

    let (output, footer) = tool_display_content(tool);
    build_output_block_rows(
        &tool.title,
        " Output ",
        &output,
        &footer,
        visible,
        width,
        "ctrl+o to expand",
    )
}

fn build_output_block_rows(
    title: &str,
    label: &str,
    output: &str,
    footer: &str,
    visible: bool,
    width: usize,
    collapsed_hint: &str,
) -> Vec<String> {
    let width = width.max(4);
    let mut rows = Vec::new();
    rows.push(border(width));
    rows.extend(box_rows(title, width));
    rows.push(border_with_label(width, label));

    if visible {
        let mut body_rows = output_rows(output, width);
        if body_rows.is_empty() {
            body_rows.extend(box_rows("[no output]", width));
        }
        rows.extend(body_rows);
        if !footer.is_empty() {
            rows.extend(box_rows(footer, width));
        }
    } else {
        rows.extend(collapsed_output_rows(output, width, collapsed_hint));
        if !footer.is_empty() {
            rows.extend(box_rows(footer, width));
        }
    }

    rows.push(border(width));
    rows
}

fn output_rows(output: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for line in output.lines() {
        for wrapped in wrap_line(line, width.saturating_sub(4)) {
            rows.extend(box_rows(&wrapped, width));
        }
    }
    rows
}

fn collapsed_output_rows(output: &str, width: usize, hint: &str) -> Vec<String> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        let mut rows = box_rows("[no output]", width);
        rows.extend(box_rows(&format!("[collapsed; {hint}]"), width));
        return rows;
    }

    let total = lines.len();
    let shown = total.min(COLLAPSED_PREVIEW_LINES);
    let skipped = total.saturating_sub(shown);
    let mut rows = Vec::new();
    if skipped > 0 {
        rows.extend(box_rows(
            &format!("... ({skipped} earlier lines, showing {shown} of {total}) ({hint})"),
            width,
        ));
    } else {
        rows.extend(box_rows(
            &format!("... (showing {shown} of {total}) ({hint})"),
            width,
        ));
    }
    for line in lines.iter().skip(skipped) {
        rows.extend(box_rows(line, width));
    }
    rows
}

fn build_python_command_rows(
    tool: &ToolResultBlock,
    visible: bool,
    width: usize,
) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(&tool.content).ok()?;
    if value.get("kind").and_then(|v| v.as_str()) != Some("python_command_result") {
        return None;
    }
    let code = value.get("code")?.as_str()?.trim_end();
    let output_raw = value.get("output")?.as_str()?;
    let (output, footer) = command_display_content(output_raw);
    let width = width.max(4);
    let mut rows = Vec::new();
    rows.push(border_with_label(width, " python "));
    rows.extend(output_rows(code, width));
    rows.push(border_with_label(width, " Output "));
    if visible {
        let mut body_rows = output_rows(&output, width);
        if body_rows.is_empty() {
            body_rows.extend(box_rows("[no output]", width));
        }
        rows.extend(body_rows);
    } else {
        rows.extend(collapsed_output_rows(&output, width, "ctrl+o to expand"));
    }
    if !footer.is_empty() {
        rows.extend(box_rows(&footer, width));
    }
    rows.push(border(width));
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
        "[Ctrl+O to collapse/expand]".to_string(),
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
) -> Option<Vec<String>> {
    let (path, old, new) = parse_write_file_diff(&tool.content)?;
    let diff = unified_diff_rows(&old, &new);
    let added = diff
        .iter()
        .filter(|line| line.starts_with(" ") && line.contains("+│"))
        .count();
    let removed = diff.iter().filter(|line| line.starts_with('-')).count();
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("file");
    let title = format!(" ~ Edit: {ext} {path} [+{added}/-{removed}] ");

    let width = width.max(4);
    let mut rows = vec![border_with_label(width, &title)];
    let body = diff.join("\n");
    if visible {
        if diff.is_empty() {
            rows.extend(box_rows("[no changes]", width));
        } else {
            for line in diff {
                rows.extend(box_rows(&line, width));
            }
        }
    } else {
        rows.extend(collapsed_output_rows(&body, width, "ctrl+o to expand"));
    }
    rows.push(border(width));
    Some(rows)
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

fn border(width: usize) -> String {
    if width <= 1 {
        return "+".to_string();
    }
    format!("+{}+", "-".repeat(width.saturating_sub(2)))
}

fn border_with_label(width: usize, label: &str) -> String {
    if width <= 4 {
        return border(width);
    }
    let label_width = visible_width(label);
    let left = 3.min(width.saturating_sub(2));
    let used = 2 + left + label_width;
    if used >= width {
        return border(width);
    }
    format!(
        "+{}{}{}+",
        "-".repeat(left),
        label,
        "-".repeat(width - used)
    )
}

fn box_rows(text: &str, width: usize) -> Vec<String> {
    wrap_line(text, width.saturating_sub(4))
        .into_iter()
        .map(|line| {
            let pad = width.saturating_sub(4).saturating_sub(visible_width(&line));
            format!("| {}{} |", line, " ".repeat(pad))
        })
        .collect()
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

    fn session_with_table_table() -> Session {
        let mut s = Session::default();
        s.display = ThinkingDisplay::Show;
        s.push(Message::new(Role::User, "give me a table"));
        s.push(Message {
            role: Role::Assistant,
            content: "| 列 1 | 列 2 |\n|---|---|\n| A | B |".into(),
            thinking: String::new(),
            thinking_visible: false,
            tool_results: Vec::new(),
            display_cursor: usize::MAX,
            ts: chrono::Utc::now(),
            streaming: false,
        });
        s
    }

    #[test]
    fn build_lines_renders_table() {
        let session = session_with_table_table();
        let (lines, _toggles) = build_lines(&session, 100);
        // Join each line's spans into a string first, then join lines
        // with a space. This is the same shape the markdown tests use
        // and avoids inserting a space between every single-char span
        // (cells get wrapped into one span per char so the column
        // widths line up; flat-map+join would put phantom spaces
        // between "列" and "1" inside a cell).
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ");
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
        };
        let rows = build_tool_block_rows(&tool, true, 100);
        let text = rows.join("\n");
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
        };
        let rows = build_tool_block_rows(&tool, true, 80);
        let text = rows.join("\n");
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
