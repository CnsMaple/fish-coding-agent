pub mod status;

use crate::function::notifications::ToastLevel;
use crate::function::SidebarTab;
use crate::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui::widgets::{Block, Borders, Paragraph};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Tab width in display columns.
const TAB_WIDTH: usize = 4;

/// Display width of a single character. Tab is treated as `TAB_WIDTH`
/// columns instead of `unicode-width`'s 0, so cursor positioning and
/// line wrapping work correctly for pasted text containing tabs.
fn char_width(c: char) -> usize {
    if c == '\t' {
        TAB_WIDTH
    } else {
        UnicodeWidthChar::width(c).unwrap_or(0)
    }
}

/// Display width of a string, treating tab as `TAB_WIDTH` columns.
fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Maximum number of undo snapshots retained in `InputState::undo_stack`.
/// Each snapshot clones the full buffer, so the cap bounds memory usage.
const UNDO_LIMIT: usize = 100;

/// Snapshot of input state used for undo/redo.
pub type InputSnapshot = (String, usize, Option<(usize, usize)>);

#[derive(Debug)]
pub struct InputState {
    pub buffer: String,
    pub cursor: usize, // byte index
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
    /// Draft of the buffer preserved when the user first starts browsing
    /// history (Up). Restored when they navigate back to the newest entry
    /// (Down) instead of being cleared, so an unsent draft survives a
    /// history round-trip.
    pub history_draft: String,
    /// Whether we are actively editing a model id in /model picker.
    pub busy_hint: Option<String>,
    /// Active text selection within the buffer (byte indices, end exclusive).
    /// None means no selection; the tuple is always stored as (start, end) with start <= end.
    pub selection: Option<(usize, usize)>,
    /// Undo history: each entry is a snapshot captured *before* a mutating
    /// operation. Ctrl+Z pops the top entry and restores it.
    pub undo_stack: VecDeque<InputSnapshot>,
    /// Redo history: entries that were undone and can be re-applied with
    /// Ctrl+Y. Cleared whenever a new mutation occurs.
    pub redo_stack: VecDeque<InputSnapshot>,
}

impl InputState {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            history_draft: String::new(),
            busy_hint: None,
            selection: None,
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
        }
    }

    /// Snapshot the current state onto the undo stack *before* a mutation.
    /// Call this at the start of every method that modifies `buffer`/`cursor`/
    /// `selection`. Also clears the redo stack so Ctrl+Y cannot re-apply
    /// stale state after a new edit.
    pub fn push_undo(&mut self) {
        self.redo_stack.clear();
        if self.undo_stack.len() >= UNDO_LIMIT {
            self.undo_stack.pop_front();
        }
        self.undo_stack
            .push_back((self.buffer.clone(), self.cursor, self.selection));
    }

    /// Restore the most recent undo snapshot. Returns `true` if an undo
    /// was performed (i.e. the stack was non-empty).
    pub fn undo(&mut self) -> bool {
        let Some(entry) = self.undo_stack.pop_back() else {
            return false;
        };
        self.redo_stack
            .push_back((self.buffer.clone(), self.cursor, self.selection));
        if self.redo_stack.len() > UNDO_LIMIT {
            self.redo_stack.pop_front();
        }
        self.buffer = entry.0;
        self.cursor = entry.1;
        self.selection = entry.2;
        true
    }

    /// Re-apply the most recently undone snapshot. Returns `true` if a
    /// redo was performed (i.e. the redo stack was non-empty).
    pub fn redo(&mut self) -> bool {
        let Some(entry) = self.redo_stack.pop_back() else {
            return false;
        };
        self.undo_stack
            .push_back((self.buffer.clone(), self.cursor, self.selection));
        if self.undo_stack.len() > UNDO_LIMIT {
            self.undo_stack.pop_front();
        }
        self.buffer = entry.0;
        self.cursor = entry.1;
        self.selection = entry.2;
        true
    }

    /// Clear all undo/redo history (e.g. when starting a fresh session).
    pub fn clear_undo(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
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

    pub fn take(&mut self) -> String {
        let v = std::mem::take(&mut self.buffer);
        self.cursor = 0;
        self.history_idx = None;
        self.history_draft.clear();
        if !v.trim().is_empty() {
            self.history.push(v.clone());
            if self.history.len() > 200 {
                self.history.remove(0);
            }
        }
        v
    }

    pub fn insert_char(&mut self, c: char) {
        self.snap_cursor();
        let idx = self.cursor;
        self.buffer.insert(idx, c);
        self.cursor = idx + c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.snap_cursor();
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
        let col = str_width(&self.buffer[line_start..cursor]);
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

    /// Snap the cursor to a valid char boundary, clamped to
    /// `[0, buffer.len()]`. If the cursor is already valid, it is
    /// left unchanged; otherwise it is moved backward to the
    /// nearest preceding char boundary.
    pub fn snap_cursor(&mut self) {
        if self.cursor > self.buffer.len() {
            self.cursor = self.buffer.len();
            return;
        }
        while self.cursor > 0 && !self.buffer.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
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
            let w = char_width(c);
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
            None => {
                // First Up press: stash the current draft so it can be
                // restored when navigating back down.
                self.history_draft = self.buffer.clone();
                self.history.len() - 1
            }
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
                // Restore the draft captured on the first Up press
                // instead of clearing, so an unsent draft survives a
                // history round-trip.
                self.buffer = std::mem::take(&mut self.history_draft);
                self.cursor = self.buffer.len();
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
        let w = char_width(c);
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

/// Find the segment index where the cumulative visual line count first reaches
/// or exceeds `target_visual`. Returns the segment index.
fn find_segment_at_visual(seg_vis: &[usize], target_visual: usize) -> usize {
    let mut accumulated = 0usize;
    for (i, &v) in seg_vis.iter().enumerate() {
        accumulated += v;
        if accumulated > target_visual {
            return i;
        }
    }
    seg_vis.len().saturating_sub(1)
}

/// Render a text chunk for display, expanding tabs to `TAB_WIDTH` spaces.
/// Returns a borrowed slice when there are no tabs so the common no-tab
/// case avoids allocating a new String per span.
fn render_tab(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains('\t') {
        std::borrow::Cow::Owned(s.replace('\t', "    "))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Byte offset of the `\n`-segment containing byte offset `cursor`, i.e. the
/// byte right after the previous `\n` (or 0 for the first segment).
fn byte_at_line_start(cursor: usize, buffer: &str) -> usize {
    buffer[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Visual position `(line_index_within_segment, display_column)` of the
/// cursor at byte offset `text_before.len()` inside a segment whose
/// wrapped lines each start with `prompt_width` columns of prefix.
fn segment_cursor_position(
    text_before: &str,
    prompt_width: usize,
    inner_w: usize,
) -> (usize, usize) {
    let mut vi = 0usize;
    let mut col = prompt_width;
    let mut line_w = prompt_width;
    for (_, ch) in text_before.char_indices() {
        let cw = char_width(ch);
        if line_w + cw > inner_w {
            vi += 1;
            line_w = prompt_width;
        }
        line_w += cw;
        col = line_w;
    }
    if col >= inner_w {
        vi += 1;
        col = prompt_width;
    }
    (vi, col)
}

fn render_input_scrollbar(
    area: Rect,
    buf: &mut Buffer,
    total_visual: usize,
    visible: usize,
    scroll: usize,
) {
    if area.width == 0 || area.height == 0 || total_visual <= visible || visible == 0 {
        return;
    }
    let x = area.right().saturating_sub(1);
    let track_height = area.height as usize;
    let max_start = total_visual.saturating_sub(visible);
    let thumb_height = ((visible * track_height) / total_visual).clamp(1, track_height);
    let available = track_height.saturating_sub(thumb_height);
    let thumb_top = if max_start == 0 {
        0
    } else {
        (scroll * available + max_start / 2) / max_start
    };
    for row in 0..track_height {
        let y = area.y + row as u16;
        if let Some(cell) = buf.cell_mut((x, y)) {
            if row >= thumb_top && row < thumb_top + thumb_height {
                cell.set_symbol("█");
                cell.set_style(Theme::bold());
            } else {
                cell.set_symbol("│");
                cell.set_style(Theme::dim());
            }
        }
    }
}

/// Renders the input area. The status line (model | think | hit) lives
/// in the input block's title, not inside the box, so the body is just
/// the prompt row. When there are unread notifications while the
/// function panel is hidden, a small badge `(!N)` is prepended to the
/// status line instead of replacing it.
pub fn render(area: Rect, buf: &mut Buffer, app: &mut crate::app::App) {
    let mode = input_mode_preview(&app.input.buffer, &app.status.mode);
    let mut title = app.status.render_line_with_mode(mode);
    // Show a compact unread badge when the panel is hidden and there
    // are pending toasts, rather than replacing the entire status line.
    if !app.function_visible && app.pending_events > 0 {
        title.spans.insert(
            0,
            Span::styled(
                format!("[!{}] | ", app.pending_events),
                Theme::status_warn(),
            ),
        );
    }
    title.spans.insert(0, Span::raw("-- "));
    title.spans.push(Span::raw(" "));

    let border_set = ratatui::symbols::border::Set {
        top_left: "-",
        top_right: "-",
        bottom_left: "-",
        bottom_right: "-",
        vertical_left: " ",
        vertical_right: " ",
        horizontal_top: "-",
        horizontal_bottom: "-",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border_set)
        .border_style(match app.focus_target {
            crate::function::FocusTarget::Input => Theme::focused_border(),
            crate::function::FocusTarget::FunctionPanel => Theme::unfocused_border(),
            crate::function::FocusTarget::AgentsCheckbox => Theme::unfocused_border(),
        })
        .title(title);
    let mut inner = block.inner(area);
    // Keep 1 extra space on the right (border already gives 1, total = 2).
    inner.width = inner.width.saturating_sub(1);
    block.render(area, buf);
    if inner.height < 1 {
        return;
    }

    let prompt = " ".to_string();
    let prompt_width = UnicodeWidthStr::width(prompt.as_str());
    // Padding for wrapped continuation lines (reused across segments).
    let pad_line = " ".repeat(prompt_width);
    let buffer = &app.input.buffer;
    let cursor = app.input.cursor.min(buffer.len());
    let cursor_line_idx = buffer[..cursor].chars().filter(|&c| c == '\n').count();
    let all_lines: Vec<&str> = buffer.split('\n').collect();
    let inner_w = inner.width as usize;

    // Pre-compute visual line count per segment (for wrapping)
    let seg_vis: Vec<usize> = all_lines
        .iter()
        .map(|text| {
            let text_width = str_width(text);
            let seg_total_w = prompt_width + text_width;
            if seg_total_w <= inner_w {
                1
            } else {
                seg_total_w.div_ceil(inner_w)
            }
        })
        .collect();
    let total_visual: usize = seg_vis.iter().sum();

    let visible_vis = (inner.height as usize).min(total_visual).max(1);
    let start_line = if app.input_scroll_decoupled {
        // Convert raw segment offset to visual offset, then find start segment
        let scroll = app.input_scroll.current.round() as usize;
        let max_scroll = all_lines.len().saturating_sub(1);
        let scroll = scroll.min(max_scroll);
        let mut vis_before = 0usize;

        for (i, &v) in seg_vis.iter().enumerate() {
            if i >= scroll {
                break;
            }
            vis_before += v;
        }
        let max_vis_start = total_visual.saturating_sub(visible_vis);
        let target_vis = vis_before.min(max_vis_start);
        find_segment_at_visual(&seg_vis, target_vis)
    } else {
        // Place cursor at the bottom of the visible area
        let mut vis_before_cursor = 0usize;
        for (i, &v) in seg_vis.iter().enumerate() {
            if i >= cursor_line_idx {
                break;
            }
            vis_before_cursor += v;
        }
        // cursor visual line = vis_before_cursor + vi (where vi is the visual line within the segment)
        // We want the cursor to be at the bottom, so we need enough visual lines above
        let cursor_vis = vis_before_cursor
            + seg_vis
                .get(cursor_line_idx)
                .copied()
                .unwrap_or(1)
                .saturating_sub(1);
        let target_vis = cursor_vis.saturating_sub(visible_vis.saturating_sub(1));
        find_segment_at_visual(&seg_vis, target_vis)
    };

    // Walk forward from start_line, accumulating visual lines until we fill visible_vis
    let mut end_line = start_line;
    let mut accumulated_vis = 0usize;
    for (i, &v) in seg_vis.iter().enumerate() {
        if i < start_line {
            continue;
        }
        if accumulated_vis + v > visible_vis && accumulated_vis > 0 {
            break;
        }
        accumulated_vis += v;
        end_line = i + 1;
    }
    let end_line = end_line.min(all_lines.len().max(1));

    let mut visual_lines = Vec::new();
    // Map each (\n-segment) -> global visual line index (first visual line
    // of that segment). Computed for all segments; used both to render the
    // visible window and to translate the cursor to a screen position.
    let mut seg_visual_starts: Vec<usize> = Vec::new();
    let mut global_visual = 0usize;
    for &v in seg_vis.iter() {
        seg_visual_starts.push(global_visual);
        global_visual += v;
    }
    // Cursor's visual position within its segment, computed once here and
    // reused for scroll positioning and screen translation below.
    let cursor_vi = {
        let cursor_line = all_lines[cursor_line_idx];
        let text_before = &cursor_line[..cursor - byte_at_line_start(cursor, buffer)];
        segment_cursor_position(text_before, prompt_width, inner_w).0
    };

    let mut byte_pos = 0usize;
    for (idx, text) in all_lines.iter().enumerate() {
        let line_start = byte_pos;
        let line_end = line_start + text.len();
        byte_pos = line_end + 1;
        let n_vis = seg_vis[idx];
        if idx < start_line || idx >= end_line {
            continue;
        }

        // Manually split this segment into visual lines
        let mut text_byte_offset = 0usize;
        for vi in 0..n_vis {
            let is_first = vi == 0;
            let max_text_w = inner_w.saturating_sub(prompt_width);

            // Find how many bytes of remaining text fit in max_text_w display width
            let remaining = &text[text_byte_offset..];
            let mut chunk_w = 0usize;
            let mut split_at = 0usize;
            for (bi, ch) in remaining.char_indices() {
                let cw = char_width(ch);
                if chunk_w + cw > max_text_w {
                    break;
                }
                chunk_w += cw;
                split_at = bi + ch.len_utf8();
            }
            let chunk = &remaining[..split_at];
            text_byte_offset += split_at;

            let mut spans: Vec<Span<'_>> = Vec::new();
            // Prompt prefix only on first visual line of each segment
            if is_first {
                spans.push(Span::styled(prompt.clone(), Theme::bold()));
            } else {
                // Continuation lines use `prompt_width` spaces of padding.
                spans.push(Span::raw(pad_line.clone()));
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
                    spans.push(Span::raw(render_tab(&chunk[..local_start])));
                }
                if local_start < local_end {
                    spans.push(Span::styled(
                        render_tab(&chunk[local_start..local_end]),
                        Theme::reversed(),
                    ));
                }
                if local_end < chunk_len {
                    spans.push(Span::raw(render_tab(&chunk[local_end..])));
                }
            } else if cursor >= chunk_abs_start && cursor <= chunk_abs_end {
                let local = cursor - chunk_abs_start;
                if local > 0 {
                    spans.push(Span::raw(render_tab(&chunk[..local])));
                }
                // Hardware cursor (shown via \x1B[?25h) handles the
                // visual cursor; no block character needed.
                if local < chunk_len {
                    spans.push(Span::raw(render_tab(&chunk[local..])));
                }
            } else {
                spans.push(Span::raw(render_tab(chunk)));
            }
            visual_lines.push(Line::from(spans));
        }
    }

    let p = Paragraph::new(visual_lines);
    p.render(inner, buf);
    app.input_prompt_area = Some(inner);
    // Record the visual-row → byte-range map so mouse clicks anywhere in
    // the input area (including wrapped continuation rows) can be mapped
    // back to a buffer byte offset. Each rendered visual line occupies one
    // screen row starting at `inner.y`.
    {
        app.input_visual_rows.clear();
        let mut byte_pos = 0usize;
        let mut screen_row = inner.y;
        for (idx, text) in all_lines.iter().enumerate() {
            let line_start = byte_pos;
            let line_end = line_start + text.len();
            byte_pos = line_end + 1;
            let n_vis = seg_vis[idx];
            if idx < start_line || idx >= end_line {
                continue;
            }
            let mut text_byte_offset = 0usize;
            for _vi in 0..n_vis {
                let remaining = &text[text_byte_offset..];
                let mut chunk_w = 0usize;
                let mut split_at = 0usize;
                for (bi, ch) in remaining.char_indices() {
                    let cw = char_width(ch);
                    if chunk_w + cw > inner_w.saturating_sub(prompt_width) {
                        break;
                    }
                    chunk_w += cw;
                    split_at = bi + ch.len_utf8();
                }
                let chunk_abs_start = line_start + text_byte_offset;
                text_byte_offset += split_at;
                let chunk_abs_end = line_start + text_byte_offset;
                app.input_visual_rows
                    .push((screen_row, chunk_abs_start, chunk_abs_end));
                screen_row += 1;
            }
        }
    }

    // Render input scrollbar when scrolled away from cursor.
    if app.input_scroll_decoupled {
        let visible = (inner.height as usize).max(1);
        if total_visual > visible {
            let scroll_visual: usize = seg_vis.iter().take(start_line).sum();
            render_input_scrollbar(inner, buf, total_visual, visible, scroll_visual);
        }
    }

    // Cursor position: translate the precomputed segment + in-segment
    // visual position to a screen location, offset by the visible window.
    {
        let cursor_seg = seg_visual_starts.get(cursor_line_idx).copied().unwrap_or(0);
        let start_seg = seg_visual_starts.first().copied().unwrap_or(0);
        let cursor_vis = cursor_seg
            .saturating_add(cursor_vi)
            .saturating_sub(start_seg);
        let cursor_col = segment_cursor_position(
            &all_lines[cursor_line_idx][..cursor - byte_at_line_start(cursor, buffer)],
            prompt_width,
            inner_w,
        )
        .1 as u16;
        if cursor_vis < inner.height as usize {
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

/// Wall-clock epoch (ms since UNIX_EPOCH) captured on first spinner
/// draw. Stored in an atomic so the value is stable across calls and
/// threads without needing a `OnceLock`/`Once` wrapper around `Instant`
/// (which is not `Sync`).
static SPINNER_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Milliseconds per spinner frame. The render loop ticks at 100ms
/// while inflight, so matching that here yields one frame advance per
/// tick — a steady ~1s per full rotation regardless of how often
/// intermediate draws are triggered by arriving API chunks.
const SPINNER_FRAME_MS: u64 = 100;

pub(crate) fn spinner_prompt() -> String {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let epoch = SPINNER_EPOCH.load(Ordering::Relaxed);
    let epoch = if epoch == 0 {
        // Lazily install the epoch on first call; subsequent callers
        // fall through with whichever value won the race.
        let _ = SPINNER_EPOCH.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
        now
    } else {
        epoch
    };
    let elapsed = now.saturating_sub(epoch);
    let idx = ((elapsed / SPINNER_FRAME_MS) as usize) % FRAMES.len();
    format!(" {} ", FRAMES[idx])
}

pub fn toast_level_label(l: ToastLevel) -> &'static str {
    l.tag()
}

pub fn sidebar_tab_name(t: &SidebarTab) -> &'static str {
    match t {
        SidebarTab::Notifications => "notifications",
        SidebarTab::PastePreview(_) => "paste",
        SidebarTab::Settings(_) => "settings",
        SidebarTab::ModelPicker(_) => "model picker",
        SidebarTab::ProviderPicker(_) => "provider",
        SidebarTab::ThinkingPicker(_) => "thinking",
        SidebarTab::TimelinePicker(_) => "timeline",
        SidebarTab::SessionPicker(_) => "sessions",
        SidebarTab::SessionRename(_) => "rename",
        SidebarTab::Plan(_) => "plan",
        SidebarTab::Todo(_) => "todo",
        SidebarTab::ToolPicker(_) => "tools",
        SidebarTab::Hotkey => "hotkey",
        SidebarTab::CommandPalette(_) => "command palette",
    }
}

#[cfg(test)]
mod tests {
    /// A prefix match must outrank a non-prefix subsequence match even
    /// when both have the same gap score. The motivating bug: typing
    /// `/tool` ranked `/skill:ecommerce-rpa-toolkit` (contiguous
    /// substring "tool" -> gap 0) ahead of the literal `/tool` command
    /// because the alphabetical tiebreak put "skill" before "tool".
    #[test]
    fn prefix_match_outranks_same_score_subsequence() {
        // Sanity: both stems match `tool` with score 0.
        let tool_score = crate::fuzzy::score("tool", "tool").unwrap();
        let ecomm_score = crate::fuzzy::score("tool", "ecommerce-rpa-toolkit").unwrap();
        assert_eq!(tool_score, 0);
        assert_eq!(ecomm_score, 0);

        // The new prefix_match key separates them: "tool" starts_with
        // "tool" (true), "ecommerce-rpa-toolkit" does not (false).
        let tool_prefix = "tool".starts_with("tool");
        let ecomm_prefix = "ecommerce-rpa-toolkit".starts_with("tool");
        assert!(tool_prefix);
        assert!(!ecomm_prefix);

        // Simulate the sort key tuple (prefix_match desc, score asc,
        // alpha asc): true (1) > false (0), so /tool ranks first.
        let a = (tool_prefix, tool_score, "/tool".to_string());
        let b = (
            ecomm_prefix,
            ecomm_score,
            "/skill:ecommerce-rpa-toolkit".to_string(),
        );
        assert!(
            b.0.cmp(&a.0) == std::cmp::Ordering::Less,
            "prefix must rank first"
        );
    }

    /// Empty query degrades gracefully: every stem `starts_with("")`
    /// is true, so all candidates are at the same tier and the order
    /// falls back to alphabetical (existing behaviour).
    #[test]
    fn empty_query_all_prefix_tier() {
        assert!("tool".starts_with(""));
        assert!("skill".starts_with(""));
        assert!("mcp".starts_with(""));
    }
}
