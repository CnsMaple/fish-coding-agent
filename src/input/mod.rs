pub mod status;

use crate::function::notifications::ToastLevel;
use crate::function::SidebarTab;
use crate::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui::widgets::{Block, Borders, Paragraph};
use std::sync::atomic::{AtomicUsize, Ordering};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug)]
pub struct InputState {
    pub buffer: String,
    pub cursor: usize, // byte index
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
    /// Whether we are actively editing a model id in /model picker.
    pub busy_hint: Option<String>,
    /// Active text selection within the buffer (byte indices, end exclusive).
    /// None means no selection; the tuple is always stored as (start, end) with start <= end.
    pub selection: Option<(usize, usize)>,
}

impl InputState {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            busy_hint: None,
            selection: None,
        }
    }

    pub fn has_selection(&self) -> bool {
        self.selection.map(|(s, e)| e > s).unwrap_or(false)
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn selected_text(&self) -> Option<String> {
        self.selection.and_then(|(s, e)| {
            if e > s
                && e <= self.buffer.len()
                && self.buffer.is_char_boundary(s)
                && self.buffer.is_char_boundary(e)
            {
                Some(self.buffer[s..e].to_string())
            } else {
                None
            }
        })
    }

    pub fn set_selection(&mut self, start: usize, end: usize) {
        let len = self.buffer.len();
        let s = start.min(len);
        let e = end.min(len);
        if s == e {
            self.selection = None;
        } else {
            self.selection = Some((s.min(e), s.max(e)));
        }
    }

    pub fn is_command(&self) -> bool {
        self.buffer.starts_with('/')
    }

    pub fn command_name(&self) -> Option<&str> {
        if self.is_command() {
            let stripped = self.buffer.trim_end();
            let after = stripped.trim_start_matches('/');
            if after.contains(' ') {
                None
            } else {
                Some(after)
            }
        } else {
            None
        }
    }

    pub fn take(&mut self) -> String {
        let v = std::mem::take(&mut self.buffer);
        self.cursor = 0;
        self.history_idx = None;
        if !v.trim().is_empty() {
            self.history.push(v.clone());
            if self.history.len() > 200 {
                self.history.remove(0);
            }
        }
        v
    }

    pub fn insert_char(&mut self, c: char) {
        let idx = self.cursor;
        self.buffer.insert(idx, c);
        self.cursor = idx + c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        let idx = self.cursor;
        self.buffer.insert_str(idx, s);
        self.cursor = idx + s.len();
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // find prev char boundary
        let mut start = self.cursor - 1;
        while start > 0 && !self.buffer.is_char_boundary(start) {
            start -= 1;
        }
        self.buffer.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn delete_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut i = self.cursor;
        let prev_char = |pos: usize| -> Option<(usize, char)> {
            if pos == 0 {
                return None;
            }
            let mut start = pos - 1;
            while start > 0 && !self.buffer.is_char_boundary(start) {
                start -= 1;
            }
            self.buffer[start..pos].chars().next().map(|c| (start, c))
        };
        // skip trailing spaces
        while let Some((prev, c)) = prev_char(i) {
            if !c.is_whitespace() {
                break;
            }
            i = prev;
        }
        // skip word
        while let Some((prev, c)) = prev_char(i) {
            if c.is_whitespace() {
                break;
            }
            i = prev;
        }
        self.buffer.replace_range(i..self.cursor, "");
        self.cursor = i;
    }

    pub fn move_left(&mut self) {
        let mut i = self.cursor;
        if i == 0 {
            return;
        }
        i -= 1;
        while i > 0 && !self.buffer.is_char_boundary(i) {
            i -= 1;
        }
        self.cursor = i;
    }

    pub fn move_right(&mut self) {
        let mut i = self.cursor;
        if i >= self.buffer.len() {
            return;
        }
        i += 1;
        while i < self.buffer.len() && !self.buffer.is_char_boundary(i) {
            i += 1;
        }
        self.cursor = i;
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    pub fn move_up_line(&mut self) -> bool {
        let (line_start, col) = self.current_line_start_and_col();
        if line_start == 0 {
            return false;
        }
        let prev_end = line_start.saturating_sub(1);
        let prev_start = self.buffer[..prev_end]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor = byte_at_display_col(&self.buffer[prev_start..prev_end], col) + prev_start;
        true
    }

    pub fn move_down_line(&mut self) -> bool {
        let (line_start, col) = self.current_line_start_and_col();
        let line_end = self.buffer[line_start..]
            .find('\n')
            .map(|i| line_start + i)
            .unwrap_or(self.buffer.len());
        if line_end >= self.buffer.len() {
            return false;
        }
        let next_start = line_end + 1;
        let next_end = self.buffer[next_start..]
            .find('\n')
            .map(|i| next_start + i)
            .unwrap_or(self.buffer.len());
        self.cursor = byte_at_display_col(&self.buffer[next_start..next_end], col) + next_start;
        true
    }

    fn current_line_start_and_col(&self) -> (usize, usize) {
        let cursor = self.cursor.min(self.buffer.len());
        let line_start = self.buffer[..cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let col = UnicodeWidthStr::width(&self.buffer[line_start..cursor]);
        (line_start, col)
    }

    pub fn delete_forward(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let mut end = self.cursor + 1;
        while end < self.buffer.len() && !self.buffer.is_char_boundary(end) {
            end += 1;
        }
        self.buffer.replace_range(self.cursor..end, "");
    }

    /// Returns true if a selection was deleted.
    pub fn delete_selection(&mut self) -> bool {
        if let Some((s, e)) = self.selection {
            let (s, e) = if s <= e { (s, e) } else { (e, s) };
            if s < e
                && e <= self.buffer.len()
                && self.buffer.is_char_boundary(s)
                && self.buffer.is_char_boundary(e)
            {
                self.buffer.replace_range(s..e, "");
                self.cursor = s;
                self.selection = None;
                return true;
            }
        }
        false
    }

    /// Extend selection to the left by one character (Shift+Left).
    pub fn extend_selection_left(&mut self) {
        let anchor = match self.selection {
            Some((s, e)) => {
                if self.cursor == e {
                    s
                } else {
                    e
                }
            }
            None => self.cursor,
        };
        self.move_left();
        let new_start = anchor.min(self.cursor);
        let new_end = anchor.max(self.cursor);
        if new_start == new_end {
            self.selection = None;
        } else {
            self.selection = Some((new_start, new_end));
        }
    }

    /// Extend selection to the right by one character (Shift+Right).
    pub fn extend_selection_right(&mut self) {
        let anchor = match self.selection {
            Some((s, e)) => {
                if self.cursor == s {
                    e
                } else {
                    s
                }
            }
            None => self.cursor,
        };
        self.move_right();
        let new_start = anchor.min(self.cursor);
        let new_end = anchor.max(self.cursor);
        if new_start == new_end {
            self.selection = None;
        } else {
            self.selection = Some((new_start, new_end));
        }
    }

    /// Set selection from a screen (col, row) by translating to buffer index
    /// using the known prompt prefix width.
    pub fn select_from_screen(&mut self, col: u16, prefix_width: u16) {
        if col < prefix_width {
            self.selection = None;
            return;
        }
        let offset = (col - prefix_width) as usize;
        let mut acc = 0usize;
        for (i, c) in self.buffer.char_indices() {
            if acc >= offset {
                self.cursor = i;
                self.selection = None;
                return;
            }
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            acc += w;
            if acc >= offset {
                self.cursor = i + c.len_utf8();
                self.selection = None;
                return;
            }
        }
        self.cursor = self.buffer.len();
        self.selection = None;
    }

    /// Set selection (start, end) from screen columns (start_col, end_col).
    /// Uses prompt width to translate columns to byte indices.
    pub fn set_selection_from_screen(&mut self, start_col: u16, end_col: u16, prefix_width: u16) {
        let len = self.buffer.len();
        let start_byte = col_to_byte(&self.buffer, start_col.saturating_sub(prefix_width));
        let end_byte = col_to_byte(&self.buffer, end_col.saturating_sub(prefix_width));
        self.cursor = end_byte;
        if start_byte == end_byte || start_byte >= len && end_byte >= len {
            self.selection = None;
        } else {
            self.selection = Some((start_byte.min(len), end_byte.min(len)));
        }
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_idx {
            None => self.history.len() - 1,
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_idx = Some(idx);
        self.buffer = self.history[idx].clone();
        self.cursor = self.buffer.len();
    }

    pub fn history_next(&mut self) {
        let idx = match self.history_idx {
            None => return,
            Some(i) if i + 1 >= self.history.len() => {
                self.history_idx = None;
                self.buffer.clear();
                self.cursor = 0;
                return;
            }
            Some(i) => i + 1,
        };
        self.history_idx = Some(idx);
        self.buffer = self.history[idx].clone();
        self.cursor = self.buffer.len();
    }
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a column offset (0-based display columns) to a byte index in `s`.
fn col_to_byte(s: &str, col: u16) -> usize {
    byte_at_display_col(s, col as usize)
}

fn byte_at_display_col(s: &str, col: usize) -> usize {
    let mut acc = 0usize;
    for (i, c) in s.char_indices() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if acc + w > col {
            return i;
        }
        acc += w;
        if acc > col {
            return i + c.len_utf8();
        }
    }
    s.len()
}

/// Renders the input area. The status line (model | think | hit) lives
/// in the input block's title, not inside the box, so the body is just
/// the prompt row.
pub fn render(area: Rect, buf: &mut Buffer, app: &mut crate::app::App) {
    // Pick the title content: the unread-banner wins when the function
    // panel is hidden but has pending important toasts; otherwise the
    // regular status line.
    let show_banner = !app.function_visible && app.pending_events > 0;
    let mut title: Line = if show_banner {
        Line::from(vec![
            Span::styled("[", Theme::dim()),
            Span::styled(
                format!(
                    "{} unread in function panel - press Ctrl+N to view",
                    app.pending_events
                ),
                Theme::status_warn(),
            ),
            Span::styled("]", Theme::dim()),
        ])
    } else {
        let mode = input_mode_preview(&app.input.buffer, &app.status.mode);
        app.status.render_line_with_mode(mode)
    };
    // One space of padding on each side so the title text does not touch
    // the left/right border lines.
    title.spans.insert(0, Span::raw(" "));
    title.spans.push(Span::raw(" "));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(app.config.border_type.ratatui_set())
        .border_style(Theme::unfocused_border())
        .title(title);
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height < 1 {
        return;
    }

    let prompt = if app.inflight.is_some() {
        spinner_prompt()
    } else {
        " > ".to_string()
    };
    let prompt_width = UnicodeWidthStr::width(prompt.as_str());
    let buffer = &app.input.buffer;
    let cursor = app.input.cursor.min(buffer.len());
    let cursor_line_idx = buffer[..cursor].chars().filter(|&c| c == '\n').count();
    let all_lines: Vec<&str> = buffer.split('\n').collect();
    let visible_count = (all_lines.len() as u16).min(inner.height).max(1) as usize;
    let start_line = (cursor_line_idx + 1).saturating_sub(visible_count);
    let end_line = (start_line + visible_count).min(all_lines.len().max(1));

    let inner_w = inner.width as usize;
    let mut visual_lines: Vec<Line<'static>> = Vec::new();
    // Map each (\n-segment, visual_line_within_segment) -> global visual line index
    let mut seg_visual_starts: Vec<usize> = Vec::new(); // per segment: first global visual line
    let mut byte_pos = 0usize;
    for (idx, text) in all_lines.iter().enumerate() {
        let line_start = byte_pos;
        let line_end = line_start + text.len();
        byte_pos = line_end + 1;
        let text_width = UnicodeWidthStr::width(*text);
        let seg_total_w = prompt_width + text_width;
        let n_vis = if seg_total_w <= inner_w {
            1
        } else {
            seg_total_w.div_ceil(inner_w)
        };
        if idx < start_line || idx >= end_line {
            continue;
        }
        seg_visual_starts.push(visual_lines.len());

        // Manually split this segment into visual lines
        let mut text_byte_offset = 0usize;
        for vi in 0..n_vis {
            let is_first = vi == 0;
            let max_text_w = if is_first {
                inner_w.saturating_sub(prompt_width)
            } else {
                inner_w
            };

            // Find how many bytes of remaining text fit in max_text_w display width
            let remaining = &text[text_byte_offset..];
            let mut chunk_w = 0usize;
            let mut split_at = 0usize;
            for (bi, ch) in remaining.char_indices() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if chunk_w + cw > max_text_w {
                    break;
                }
                chunk_w += cw;
                split_at = bi + ch.len_utf8();
            }
            let chunk = &remaining[..split_at];
            text_byte_offset += split_at;

            let mut spans: Vec<Span<'static>> = Vec::new();
            // Prompt prefix only on first visual line of each segment
            if is_first {
                spans.push(Span::styled(prompt.clone(), Theme::bold()));
            } else {
                spans.push(Span::raw(" ".repeat(prompt_width)));
            }
            // Compute byte ranges for this chunk in absolute buffer coordinates
            let chunk_abs_start = line_start + text_byte_offset - split_at;
            let chunk_abs_end = line_start + text_byte_offset;
            let chunk_len = chunk.len();

            // Selection handling
            if let Some((s, e)) = app.input.selection {
                let (s, e) = if s <= e { (s, e) } else { (e, s) };
                let sel_start = s.max(chunk_abs_start).min(chunk_abs_end);
                let sel_end = e.max(chunk_abs_start).min(chunk_abs_end);
                let local_start = sel_start - chunk_abs_start;
                let local_end = sel_end - chunk_abs_start;
                if local_start > 0 {
                    spans.push(Span::raw(chunk[..local_start].to_string()));
                }
                if local_start < local_end {
                    spans.push(Span::styled(
                        chunk[local_start..local_end].to_string(),
                        Theme::reversed(),
                    ));
                }
                if local_end < chunk_len {
                    spans.push(Span::raw(chunk[local_end..].to_string()));
                }
            } else if app.inflight.is_none() && cursor >= chunk_abs_start && cursor <= chunk_abs_end
            {
                let local = cursor - chunk_abs_start;
                if local > 0 {
                    spans.push(Span::raw(chunk[..local].to_string()));
                }
                // Hardware cursor (shown via \x1B[?25h) handles the
                // visual cursor; no block character needed.
                if local < chunk_len {
                    spans.push(Span::raw(chunk[local..].to_string()));
                }
            } else {
                spans.push(Span::raw(chunk.to_string()));
            }
            visual_lines.push(Line::from(spans));
        }
    }

    let p = Paragraph::new(visual_lines);
    p.render(inner, buf);
    app.input_prompt_area = Some(inner);

    // Cursor position: find which visual_line the cursor is on
    if app.inflight.is_none() {
        let mut cursor_vis = 0usize;
        let mut cursor_col = 0u16;
        let mut found = false;
        byte_pos = 0usize;
        for (idx, text) in all_lines.iter().enumerate() {
            let line_start = byte_pos;
            let line_end = line_start + text.len();
            byte_pos = line_end + 1;
            if cursor < line_start || cursor > line_end {
                continue;
            }
            // cursor is in this segment
            let text_width = UnicodeWidthStr::width(*text);
            let seg_total_w = prompt_width + text_width;
            let n_vis = if seg_total_w <= inner_w {
                1
            } else {
                seg_total_w.div_ceil(inner_w)
            };
            let text_before = &text[..cursor - line_start];
            let width_before = prompt_width + UnicodeWidthStr::width(text_before);
            let vi = if n_vis <= 1 {
                0
            } else {
                width_before / inner_w
            };
            cursor_col = (width_before % inner_w) as u16;
            // Count visual lines from all earlier visible segments
            let mut vis_before = 0usize;
            for (j, t) in all_lines.iter().enumerate() {
                if j >= idx {
                    break;
                }
                if j < start_line || j >= end_line {
                    continue;
                }
                let tw = UnicodeWidthStr::width(*t);
                let stw = prompt_width + tw;
                let nv = if stw <= inner_w {
                    1
                } else {
                    stw.div_ceil(inner_w)
                };
                vis_before += nv;
            }
            cursor_vis = vis_before + vi;
            found = true;
            break;
        }
        if found {
            let cy = inner.y + cursor_vis as u16;
            let cx = inner.x + cursor_col;
            if cy < inner.y + inner.height {
                if let Some(cell) = buf.cell_mut((cx, cy)) {
                    cell.set_style(Theme::cursor());
                }
                app.input_cursor_screen = Some((cx, cy));
            } else {
                app.input_cursor_screen = None;
            }
        } else {
            app.input_cursor_screen = None;
        }
    } else {
        app.input_cursor_screen = None;
    }
}

fn input_mode_preview<'a>(buffer: &'a str, fallback: &'a str) -> &'a str {
    let trimmed = buffer.trim_start();
    if trimmed.starts_with("!!") {
        "shell_context"
    } else if trimmed.starts_with('!') {
        "shell"
    } else if trimmed.starts_with("$$") {
        "python_context"
    } else if trimmed.starts_with('$') {
        "python"
    } else {
        fallback
    }
}

static SPINNER_FRAME: AtomicUsize = AtomicUsize::new(0);

fn spinner_prompt() -> String {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let idx = SPINNER_FRAME.fetch_add(1, Ordering::Relaxed) % FRAMES.len();
    format!(" {} ", FRAMES[idx])
}

/// Heuristic for whether a "completion" sidebar should be visible based on input.
pub fn should_show_completion(input: &InputState) -> bool {
    if !input.is_command() || input.buffer.contains('\n') {
        return false;
    }
    !completion_candidates_for(&input.buffer).is_empty()
}

/// List of slash commands offered by completion.
pub const COMMAND_LIST: &[&str] = &[
    "/settings",
    "/model",
    "/hotkey",
    "/new",
    "/clear",
    "/provider",
    "/think",
    "/timeline",
    "/session",
    "/rename",
    "/fork",
    "/retry",
    "/continue",
    "/compact",
    "/plan",
    "/build",
    "/quit",
    "/exit",
    "/help",
    "/mcp",
    "/mcp-auth",
    "/mcp-logout",
    "/mcp-debug",
];

/// Returns the list of completion candidates that match the given prefix
/// (the buffer text, starting with `/`). Returns owned `String`s so the
/// state can live in `CompletionState` independently of the buffer.
pub fn completion_candidates_for(input: &str) -> Vec<String> {
    let trimmed = input.trim_start_matches('/').to_lowercase();
    if let Some((cmd, rest)) = trimmed.split_once(' ') {
        // Show sub-argument completions for known commands.
        // Sub-args are short, fixed vocabularies (`off`/`low`/...)
        // so we keep the exact prefix filter rather than fuzzy.
        let rest = rest.trim().to_lowercase();
        return match cmd {
            "think" | "thinking" => vec!["off", "low", "med", "high", "adaptive"]
                .into_iter()
                .filter(|c| c.starts_with(&rest) || rest.is_empty())
                .map(|s| format!("/think {s}"))
                .collect(),
            "plan" => vec!["exit"]
                .into_iter()
                .filter(|c| c.starts_with(&rest) || rest.is_empty())
                .map(|s| format!("/plan {s}"))
                .collect(),
            "mcp" | "mcp-auth" | "mcp-logout" | "mcp-debug" => {
                // List configured MCP server names.
                let names = crate::mcp::builtin_names();
                names
                    .into_iter()
                    .filter(|n| n.starts_with(&rest) || rest.is_empty())
                    .map(|n| format!("/{cmd} {n}"))
                    .collect()
            }
            _ => vec![],
        };
    }
    // Top-level: fuzzy-match against static command names and the
    // dynamic `/skill:<name>` / `/mcp:<name>` lists. The trimmed
    // prefix may be:
    //   - empty               -> list everything
    //   - `skill` / `mcp`     -> list every `/skill:<n>` / `/mcp:<n>`
    //   - `skill:foo` / `mcp:f` -> filter the dynamic list by `foo` / `f`
    //   - anything else       -> fuzzy against the static command list
    // We dedupe (a candidate may match both static and dynamic) and
    // sort by fuzzy score then alphabetically so the best matches
    // appear at the top of the picker.
    let mut scored: Vec<(u32, String)> = Vec::new();
    for cmd in COMMAND_LIST.iter().copied() {
        let stem = &cmd[1..]; // strip leading '/'
        if let Some(sc) = crate::fuzzy::score(&trimmed, stem) {
            scored.push((sc, cmd.to_string()));
        }
    }
    // Decide which dynamic list to query and with what arg.
    let (skill_q, mcp_q) = match trimmed.split_once(':') {
        Some(("skill", r)) => (r.trim().to_string(), None),
        Some(("mcp", r)) => (String::new(), Some(r.trim().to_string())),
        Some(_) => (String::new(), None),
        None => match trimmed.as_str() {
            // Bare `skill` / `mcp` -> list every entry.
            "skill" => (String::new(), None),
            "mcp" => (String::new(), Some(String::new())),
            // Otherwise, only feed dynamic lists when the trimmed
            // prefix is a prefix of the base name (`sk` -> skill,
            // `m` -> mcp). This avoids spamming `/skill:<n>` for
            // queries like `/think` that share no base name.
            _ if trimmed.starts_with("sk") || trimmed.starts_with("skill") => {
                (trimmed.clone(), None)
            }
            _ if trimmed.starts_with("mcp") => (String::new(), Some(trimmed.clone())),
            _ => (String::new(), None),
        },
    };
    for cand in crate::skill::completion_candidates(&skill_q) {
        // Re-score so cross-source ranking stays consistent: a skill
        // name that matches the query better than a static command
        // must rank higher.
        let stem = cand.trim_start_matches("/skill:").to_string();
        let sc = crate::fuzzy::score(&trimmed, &stem).unwrap_or(u32::MAX);
        push_unique(&mut scored, sc, cand);
    }
    if let Some(q) = mcp_q.as_deref() {
        for cand in crate::mcp::completion_candidates(q) {
            let stem = cand.trim_start_matches("/mcp:").to_string();
            let sc = crate::fuzzy::score(&trimmed, &stem).unwrap_or(u32::MAX);
            push_unique(&mut scored, sc, cand);
        }
    }
    // Sort: best score first, alphabetical tiebreak so the picker is
    // deterministic across renders.
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut out: Vec<String> = scored.into_iter().map(|(_, s)| s).collect();
    // Cap the picker at a sane size so a very broad fuzzy match
    // doesn't dump the entire skill library into the sidebar.
    const MAX_CANDIDATES: usize = 50;
    out.truncate(MAX_CANDIDATES);
    out
}

/// Helper: push `(score, cand)` into `scored` only if no candidate
/// with the same string is already there. Used by the top-level
/// completion builder to dedupe across static + dynamic sources.
fn push_unique(scored: &mut Vec<(u32, String)>, score: u32, cand: String) {
    if !scored.iter().any(|(_, c)| *c == cand) {
        scored.push((score, cand));
    }
}

/// Backwards-compatible shim for places that still want borrowed candidates.
pub fn completion_candidates(input: &str) -> Vec<&'static str> {
    let owned = completion_candidates_for(input);
    // Map owned strings back to the static set where possible; if anything
    // unmatched is present we just return an empty vec.
    let _ = owned;
    let after = input.trim_start_matches('/').to_lowercase();
    if after.contains(' ') {
        return vec![];
    }
    COMMAND_LIST
        .iter()
        .copied()
        .filter(|c| c[1..].starts_with(after.as_str()) || after.is_empty())
        .collect()
}

pub fn toast_level_label(l: ToastLevel) -> &'static str {
    l.tag()
}

pub fn sidebar_tab_name(t: &SidebarTab) -> &'static str {
    match t {
        SidebarTab::Notifications => "notifications",
        SidebarTab::Completion(_) => "completion",
        SidebarTab::Settings(_) => "settings",
        SidebarTab::ModelPicker(_) => "model picker",
        SidebarTab::ProviderPicker(_) => "provider",
        SidebarTab::ThinkingPicker(_) => "thinking",
        SidebarTab::TimelinePicker(_) => "timeline",
        SidebarTab::SessionPicker(_) => "sessions",
        SidebarTab::SessionRename(_) => "rename",
        SidebarTab::Plan(_) => "plan",
        SidebarTab::Ask(_) => "ask",
        SidebarTab::Hotkey => "hotkey",
    }
}
