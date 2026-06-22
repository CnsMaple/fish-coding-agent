pub mod markdown;
pub mod render;
pub mod store;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultBlock {
    pub name: String,
    pub title: String,
    pub content: String,
    pub content_offset: usize,
    pub visible: bool,

}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

impl Role {
    pub fn prefix(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        }
    }
}

/// Marker for a user message inserted by `/skill:<name>` dispatch.
/// The renderer uses this to show a clean `[skill]` block in the
/// chat. The AI sees only the actual skill content (stored in
/// `Message::content`); the path is purely a UI hint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRef {
    pub name: String,
    pub context_path: String,
    #[serde(default)]
    pub args: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Thinking content (Anthropic "thinking_delta"), shown in dim and
    /// optionally collapsed.
    pub thinking: String,
    /// Whether the thinking block is currently expanded.
    pub thinking_visible: bool,
    /// Tool result blocks, each with its own visibility,
    /// rendered as collapsible code sections.
    pub tool_results: Vec<ToolResultBlock>,
    pub ts: DateTime<Utc>,
    /// true while a streaming response is still in flight
    pub streaming: bool,
    /// Byte offset into `content` up to which the text has been visually
    /// revealed.  Advances by a few bytes per frame during streaming so
    /// that bursts from the API don't all appear at once.
    pub display_cursor: usize,
    /// `Some` when this message was inserted by a `/skill:<name>`
    /// dispatch. Drives the `[skill]` block rendering and tells the
    /// API path that the content is already the skill body, no extra
    /// prompt assembly is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_ref: Option<SkillRef>,
    /// Pre-computed line count (content.split('\n').count()).
    /// Updated when content changes to avoid re-scanning on every frame.
    pub line_count: u16,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        let content = content.into();
        let len = content.len();
        let line_count = content.matches('\n').count() as u16 + 1;
        Self {
            role,
            content,
            thinking: String::new(),
            thinking_visible: false,
            tool_results: Vec::new(),
            ts: Utc::now(),
            streaming: false,
            display_cursor: len, // non-streaming → fully visible
            skill_ref: None,
            line_count,
        }
    }

    /// The portion of `content` that should be displayed this frame.
    pub fn visible_content(&self) -> &str {
        let end = self.display_cursor.min(self.content.len());
        // Clamp to a valid char boundary so we never panic on a split
        // multi-byte character.
        let end = self.content.floor_char_boundary(end);
        &self.content[..end]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Session {
    pub messages: Vec<Message>,
    #[serde(default)]
    pub todo_items: Vec<TodoItem>,
    /// scroll offset from bottom; 0 = follow tail
    pub scroll: u16,
    /// id of the message currently being edited/streamed
    #[serde(skip)]
    pub streaming_id: Option<usize>,
    /// Thinking display mode, set from App config on each render.
    #[serde(skip)]
    pub display: crate::config::ThinkingDisplay,
    /// Tool result display mode, set from App config on each render.
    #[serde(skip)]
    pub tool_display: crate::config::ToolResultDisplay,
    /// Cache of rendered `Line`s per message index.
    /// `None` = uncached; `Some(lines)` = cached until content changes.
    #[serde(skip)]
    pub line_cache: std::sync::Mutex<Vec<Option<Vec<ratatui::text::Line<'static>>>>>,
}

impl Session {
    pub fn push(&mut self, msg: Message) -> usize {
        let id = self.messages.len();
        self.messages.push(msg);
        // Keep user's scroll offset if they manually scrolled away from
        // the bottom. Only auto-scroll (scroll = 0) when they were
        // already at the bottom.
        id
    }

    pub fn append_to_last(&mut self, chunk: &str) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                m.content.push_str(chunk);
                m.line_count = m.content.split('\n').count().max(1) as u16;
                if let Ok(mut c) = self.line_cache.lock() {
                    if id < c.len() { c[id] = None; }
                }
                // Reveal streamed content immediately; providers already
                // deliver deltas in small chunks, so no artificial delay.
                m.display_cursor = m.content.len();
            }
        }
    }

    pub fn append_tool_to_last(&mut self, name: String, title: String, content: String) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                let content_offset = m.content.len();
                let visible = name != "write_file" && !is_long_tool_content(&content);
                m.tool_results.push(ToolResultBlock {
                    name,
                    title,
                    content,
                    content_offset,
                    visible,
                });
            }
        }
    }

    pub fn push_tool_result_message(&mut self, name: String, title: String, content: String) {
        let visible = name != "write_file" && !is_long_tool_content(&content);
        let msg = Message {
            role: Role::Assistant,
            content: String::new(),
            thinking: String::new(),
            thinking_visible: false,
            tool_results: vec![ToolResultBlock {
                name,
                title,
                content,
                content_offset: 0,
                visible,
            }],
            ts: Utc::now(),
            streaming: false,
            display_cursor: 0,
            skill_ref: None,
            line_count: 0,
        };
        self.push(msg);
    }

    pub fn toggle_all_tool_results(&mut self) {
        let should_expand = self
            .messages
            .iter()
            .flat_map(|m| m.tool_results.iter())
            .any(|tool| !tool.visible);

        for msg in &mut self.messages {
            for tool in &mut msg.tool_results {
                tool.visible = should_expand;
            }
        }
    }

    pub fn append_thinking_to_last(&mut self, chunk: &str) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                m.thinking.push_str(chunk);
            }
        }
    }

    pub fn finish_streaming(&mut self) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                m.streaming = false;
                // Strip text-based tool call JSON fallback lines from
                // content so they don't appear in the rendered chat.
                m.content = strip_text_tool_calls(&m.content);
                m.line_count = m.content.split('\n').count().max(1) as u16;
                if let Ok(mut c) = self.line_cache.lock() {
                    if id < c.len() { c[id] = None; }
                }
                // Reveal all remaining content immediately.
                m.display_cursor = m.content.len();
                // Auto-fold thinking when streaming finishes and mode
                // is ShowWhileStreaming.
                if matches!(
                    self.display,
                    crate::config::ThinkingDisplay::ShowWhileStreaming
                ) {
                    m.thinking_visible = false;
                }
                // Auto-fold tool results when streaming finishes and mode
                // is ShowWhileStreaming.
                if matches!(
                    self.tool_display,
                    crate::config::ToolResultDisplay::ShowWhileStreaming
                ) {
                    for t in &mut m.tool_results {
                        t.visible = false;
                    }
                }
            }
        }
        self.streaming_id = None;
    }

    pub fn clear(&mut self) {
        self.messages.clear();
self.streaming_id = None;
        self.scroll = 0;
        self.todo_items.clear();
    }

    /// Rough count of rendered lines up to (but not including) `msg_idx`,
    /// mirroring the same logic used by `build_lines` in `render.rs`.
    /// Only thinking-mode `Show` counts expanded blocks; `Hide` and
    /// `ShowWhileStreaming` count collapsed toggles.
    pub fn count_lines_before(&self, _msg_idx: usize, viewport: u16) -> u16 {
        if self.messages.is_empty() {
            return 0;
        }
        let inner_h = viewport.saturating_sub(2);

        // Compute total lines the same way render.rs does.
        let total = self.count_all_lines();
        let scroll = self.scroll.min(total.saturating_sub(inner_h));
        let offset_from_bottom = inner_h as u16 + scroll;
        total.saturating_sub(offset_from_bottom)
    }

    /// Count rendered lines for every message (content + thinking toggle +
    /// thinking expanded + spacer) using the same rules as `build_lines`.
    pub fn count_all_lines(&self) -> u16 {
        let mut n = 0u16;
        for m in &self.messages {
            n += 1; // role prefix line
            let show = m.role == Role::Assistant
                && !m.thinking.trim().is_empty()
                && self.display != crate::config::ThinkingDisplay::Hide;
            if show {
                let expanded = (self.display == crate::config::ThinkingDisplay::Show
                    && m.thinking_visible)
                    || (self.display == crate::config::ThinkingDisplay::ShowWhileStreaming
                        && (m.streaming || m.thinking_visible));
                n += crate::session::render::thinking_block_line_count(&m.thinking, expanded, 120)
                    as u16;
            }
            n += m.line_count;
            if self.tool_display != crate::config::ToolResultDisplay::Hide {
                for t in &m.tool_results {
                    let t_vis = match self.tool_display {
                        crate::config::ToolResultDisplay::Show => t.visible,
                        crate::config::ToolResultDisplay::ShowWhileStreaming => {
                            m.streaming || t.visible
                        }
                        _ => false,
                    };
                    n += crate::session::render::tool_block_line_count(t, t_vis, 120) as u16;
                }
            }
            n += 1; // spacer
        }
        if !self.messages.is_empty() {
            n += 1; // trailing gap line at the bottom
        }
        n
    }

    /// Set `scroll` so that the last `user` message appears at the top
    /// of the viewport.  Lines after the message will fill the viewport.
    pub fn timeline(&mut self, viewport_height: u16) {
        let inner_h = viewport_height.saturating_sub(2) as u16;
        if inner_h == 0 {
            return;
        }

        // Find the last user message.
        let last_user = match self.messages.iter().rposition(|m| m.role == Role::User) {
            Some(i) => i,
            None => return,
        };

        let lines_before = self.lines_before(last_user);
        let total = self.count_all_lines();
        let target = total.saturating_sub(lines_before + inner_h);
        self.scroll = target;
    }

    /// Set `scroll` so the message at index `msg_idx` appears at the
    /// top of the viewport. No-op if `msg_idx` is out of range.
    pub fn jump_to_message(&mut self, msg_idx: usize, viewport_height: u16) {
        if msg_idx >= self.messages.len() {
            return;
        }
        let inner_h = viewport_height.max(1);
        let lines_before = self.lines_before(msg_idx);
        let total = self.count_all_lines();
        self.scroll = total.saturating_sub(inner_h).saturating_sub(lines_before);
    }

    /// Number of rendered lines from the top of the buffer up to (but
    /// not including) the message at `msg_idx`.
    fn lines_before(&self, msg_idx: usize) -> u16 {
        let mut n = 0u16;
        for (i, m) in self.messages.iter().enumerate() {
            if i >= msg_idx {
                break;
            }
            n += 1; // role prefix line
            let show = m.role == Role::Assistant
                && !m.thinking.trim().is_empty()
                && self.display != crate::config::ThinkingDisplay::Hide;
            if show {
                let expanded = (self.display == crate::config::ThinkingDisplay::Show
                    && m.thinking_visible)
                    || (self.display == crate::config::ThinkingDisplay::ShowWhileStreaming
                        && (m.streaming || m.thinking_visible));
                n += crate::session::render::thinking_block_line_count(&m.thinking, expanded, 120)
                    as u16;
            }
            n += m.line_count;
            if self.tool_display != crate::config::ToolResultDisplay::Hide {
                for t in &m.tool_results {
                    let t_vis = match self.tool_display {
                        crate::config::ToolResultDisplay::Show => t.visible,
                        crate::config::ToolResultDisplay::ShowWhileStreaming => {
                            m.streaming || t.visible
                        }
                        _ => false,
                    };
                    n += crate::session::render::tool_block_line_count(t, t_vis, 120) as u16;
                }
            }
            n += 1; // spacer
        }
        n
    }
}

/// Remove text-based tool call JSON fallback lines from content.
/// These are lines containing `{"name":"...","arguments":...}` that
/// the model may output when the API doesn't support structured
/// tool_calls. Also strips optional `>>>` / `<<<` wrappers, including
/// Markdown-quoted wrapper fragments.
pub fn strip_text_tool_calls(s: &str) -> String {
    let mut out = Vec::new();
    let mut in_tool_block = false;

    for line in s.lines() {
        let mut current = line.to_string();
        let mut kept_prefix: Option<String> = None;

        if let Some(idx) = current.find(">>>") {
            let prefix = current[..idx].trim_end();
            if !prefix.is_empty() {
                kept_prefix = Some(prefix.to_string());
            }
            current = current[idx + 3..].to_string();
            in_tool_block = true;
        }

        let mut closes_block = false;
        if let Some(idx) = current.find("<<<") {
            current.truncate(idx);
            closes_block = true;
        }

        let normalized = normalize_tool_call_line(&current);
        let is_tool_call = is_text_tool_call_normalized(&normalized);
        let is_empty_quote = normalized.is_empty();

        if let Some(prefix) = kept_prefix {
            out.push(prefix);
        } else if !in_tool_block && !is_tool_call {
            out.push(line.to_string());
        } else if in_tool_block && !is_tool_call && !is_empty_quote {
            out.push(current.trim().to_string());
        }

        if closes_block {
            in_tool_block = false;
        }
    }

    out.join("\n")
}

fn normalize_tool_call_line(line: &str) -> String {
    let mut t = line.trim();
    while let Some(rest) = t.strip_prefix('>') {
        t = rest.trim_start();
    }
    t.trim_matches(|c: char| c.is_whitespace()).to_string()
}

fn is_text_tool_call_normalized(line: &str) -> bool {
    let inner = line
        .strip_prefix(">>>")
        .unwrap_or(line)
        .strip_suffix("<<<")
        .unwrap_or(line)
        .trim();
    inner.len() > 10
        && inner.starts_with('{')
        && inner.contains("\"name\"")
        && inner.contains("\"arguments\"")
}

fn is_long_tool_content(content: &str) -> bool {
    content.lines().count() > 12 || content.len() > 2_000
}
