pub mod markdown;
pub mod render;

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Thinking content (Anthropic "thinking_delta"), shown in dim and
    /// optionally collapsed.
    pub thinking: String,
    /// Whether the thinking block is currently expanded.
    pub thinking_visible: bool,
    pub ts: DateTime<Utc>,
    /// true while a streaming response is still in flight
    pub streaming: bool,
    /// Byte offset into `content` up to which the text has been visually
    /// revealed.  Advances by a few bytes per frame during streaming so
    /// that bursts from the API don't all appear at once.
    pub display_cursor: usize,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        let content = content.into();
        let len = content.len();
        Self {
            role,
            content,
            thinking: String::new(),
            thinking_visible: false,
            ts: Utc::now(),
            streaming: false,
            display_cursor: len, // non-streaming → fully visible
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

#[derive(Debug, Default)]
pub struct Session {
    pub messages: Vec<Message>,
    /// scroll offset from bottom; 0 = follow tail
    pub scroll: u16,
    /// id of the message currently being edited/streamed
    pub streaming_id: Option<usize>,
    /// Thinking display mode, set from App config on each render.
    pub display: crate::config::ThinkingDisplay,
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
                // display_cursor stays behind – the main loop advances it
                // by a few bytes each frame so the text appears smoothly.
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
                // Reveal all remaining content immediately.
                m.display_cursor = m.content.len();
                // Auto-fold thinking when streaming finishes and mode
                // is ShowWhileStreaming.
                if matches!(self.display, crate::config::ThinkingDisplay::ShowWhileStreaming) {
                    m.thinking_visible = false;
                }
            }
        }
        self.streaming_id = None;
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.streaming_id = None;
        self.scroll = 0;
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
                n += 1; // toggle (after prefix)
                let expanded = (self.display == crate::config::ThinkingDisplay::Show && m.thinking_visible)
                    || (self.display == crate::config::ThinkingDisplay::ShowWhileStreaming && (m.streaming || m.thinking_visible));
                if expanded {
                    n += m.thinking.split('\n').count() as u16 + 1;
                }
            }
            n += m.content.split('\n').count() as u16;
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
        if inner_h == 0 { return; }

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
                n += 1;
                let expanded = (self.display == crate::config::ThinkingDisplay::Show && m.thinking_visible)
                    || (self.display == crate::config::ThinkingDisplay::ShowWhileStreaming && (m.streaming || m.thinking_visible));
                if expanded {
                    n += m.thinking.split('\n').count() as u16 + 1;
                }
            }
            n += m.content.split('\n').count() as u16;
            n += 1; // spacer
        }
        n
    }
}
