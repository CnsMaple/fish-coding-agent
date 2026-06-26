pub mod lru;
pub mod markdown;
pub mod render;
pub mod store;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingSegment {
    pub offset: usize,
    pub content: String,
    /// Cached rendered line count for the expanded thinking block.
    /// `None` means "needs (re)compute"; populated on first render
    /// or by `Session::recompute_layout_caches`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_line_count_expanded: Option<u32>,
    /// Cached rendered line count for the collapsed (single toggle) line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_line_count_collapsed: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultBlock {
    pub name: String,
    pub title: String,
    pub content: String,
    pub content_offset: usize,
    pub visible: bool,
    #[serde(default)]
    pub running: bool,
    /// Cached rendered line count for the expanded tool block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_line_count_visible: Option<u32>,
    /// Cached rendered line count for the collapsed preview form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_line_count_collapsed: Option<u32>,
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
    /// Thinking segments with content offsets for interleaved rendering.
    /// Each segment represents thinking received when content was at that offset.
    #[serde(default)]
    pub thinking_segments: Vec<ThinkingSegment>,
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
    /// `u32` to support 10M+ token sessions.
    pub line_count: u32,
    /// Per-message version counter. Bumped whenever this message's
    /// content, thinking, or tool results change. The render LRU is
    /// keyed on this so changing one message does not invalidate
    /// cached render output for the others.
    #[serde(default)]
    pub content_version: u64,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        let content = content.into();
        let len = content.len();
        let line_count = content.matches('\n').count() as u32 + 1;
        Self {
            role,
            content,
            thinking: String::new(),
            thinking_segments: Vec::new(),
            thinking_visible: false,
            tool_results: Vec::new(),
            ts: Utc::now(),
            streaming: false,
            display_cursor: len, // non-streaming → fully visible
            skill_ref: None,
            line_count,
            content_version: 0,
        }
    }

    /// Bump the per-message version counter. Call this whenever the
    /// message's content, thinking, or tool blocks change so the
    /// render LRU can detect staleness.
    pub fn bump_version(&mut self) {
        self.content_version = self.content_version.wrapping_add(1);
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
    /// Bounded LRU of fully-rendered `Vec<Line>` per message, keyed by
    /// `msg_idx` and validated against `Message.content_version`. Used
    /// by the viewport-aware render path so we only re-parse Markdown
    /// for messages that actually intersect the visible window.
    #[serde(skip)]
    pub message_lines_cache: std::sync::Mutex<
        crate::session::lru::BoundedCache<crate::session::render::CachedMessageLines>,
    >,
    /// Cached total rendered line count across all messages. Width
    /// dependent — the cache is keyed by the viewport width and the
    /// per-block visibility state in `display` / `tool_display`. We
    /// store the most recently computed value plus the version of the
    /// session state it was computed against. `None` means "needs
    /// compute on next read".
    #[serde(skip)]
    pub cached_total_lines: Option<(u16, u32)>,
    /// Monotonically increasing version counter. Bumped on every write
    /// so callers can detect stale cached values.
    #[serde(skip)]
    pub layout_version: u64,
    /// Cache of the last rendered viewport buffer. When nothing changed,
    /// `render()` skips all work and just blits this buffer.
    #[serde(skip)]
    pub render_cache: std::sync::Mutex<Option<crate::session::render::RenderCache>>,
}

impl Session {
    /// Mark the layout-derived caches as dirty. Cheap O(1) call. All
    /// write paths (`push`, `append_to_last`, `append_thinking_to_last`,
    /// `start_tool_in_last`, `append_tool_to_last`, `append_tool_delta_to_last`,
    /// `update_last_tool_content`, `finish_streaming`, `toggle_all_tool_results`,
    /// `clear`, resume/fork) MUST call this.
    pub fn invalidate_layout_cache(&mut self) {
        self.cached_total_lines = None;
        self.layout_version = self.layout_version.wrapping_add(1);
        // Clear the viewport render cache too so the next frame
        // re-renders from scratch.
        if let Ok(mut c) = self.render_cache.lock() {
            *c = None;
        }
    }

    /// Read the cached total line count for a specific width, if
    /// available. `None` means the caller must compute it first via
    /// `count_all_lines_with_width(width)` (which needs `&mut self`).
    /// Read-only renderers (those that already have a `&mut App` in
    /// the caller) should call `count_all_lines_with_width` to warm
    /// the cache, then use this for cheap lookups.
    pub fn cached_total_lines_for(&self, width: usize) -> Option<u32> {
        let w = width.min(u16::MAX as usize) as u16;
        self.cached_total_lines
            .as_ref()
            .filter(|(cw, _)| *cw == w)
            .map(|(_, n)| *n)
    }

    pub fn push(&mut self, msg: Message) -> usize {
        let id = self.messages.len();
        self.messages.push(msg);
        self.invalidate_layout_cache();
        id
    }

    pub fn append_to_last(&mut self, chunk: &str) {
        if let Some(id) = self.streaming_id {
            let needs_invalidate = if let Some(m) = self.messages.get_mut(id) {
                m.content.push_str(chunk);
                m.line_count = m.content.split('\n').count().max(1) as u32;
                m.bump_version();
                if let Ok(mut c) = self.line_cache.lock() {
                    if id < c.len() {
                        c[id] = None;
                    }
                }
                // Streaming: invalidate any pre-computed block counts
                // for this message so count_all_lines_* stays accurate.
                for seg in m.thinking_segments.iter_mut() {
                    seg.cached_line_count_expanded = None;
                    seg.cached_line_count_collapsed = None;
                }
                for t in m.tool_results.iter_mut() {
                    t.cached_line_count_visible = None;
                    t.cached_line_count_collapsed = None;
                }
                // Keep cursor up-to-date so all content is immediately visible.
                m.display_cursor = m.content.len();
                true
            } else {
                false
            };
            if needs_invalidate {
                self.invalidate_layout_cache();
            }
        }
    }

    /// Update the last tool block's content (for streaming: replace placeholder with final content).
    /// If no tool block exists yet (non-streaming path), falls back to appending.
    pub fn update_last_tool_content(&mut self, name: String, title: String, content: String) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                if let Some(tool) = m.tool_results.last_mut() {
                    tool.content = content;
                    tool.running = false;
                    tool.title = title;
                    tool.cached_line_count_visible = None;
                    tool.cached_line_count_collapsed = None;
                    m.bump_version();
                    if let Ok(mut c) = self.line_cache.lock() {
                        if id < c.len() {
                            c[id] = None;
                        }
                    }
                    self.invalidate_layout_cache();
                    return;
                }
            }
        }
        // Fallback: no existing block → append as normal
        self.append_tool_to_last(name, title, content);
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
                    running: false,
                    cached_line_count_visible: None,
                    cached_line_count_collapsed: None,
                });
                m.bump_version();
                self.invalidate_layout_cache();
            }
        }
    }

    pub fn start_tool_in_last(&mut self, name: String, title: String) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                let content_offset = m.content.len();
                m.tool_results.push(ToolResultBlock {
                    name,
                    title,
                    content: String::new(),
                    content_offset,
                    visible: true,
                    running: true,
                    cached_line_count_visible: None,
                    cached_line_count_collapsed: None,
                });
                m.bump_version();
                self.invalidate_layout_cache();
            }
        }
    }

    pub fn append_tool_delta_to_last(&mut self, delta: &str) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                if let Some(tool) = m.tool_results.last_mut() {
                    tool.content.push_str(delta);
                    tool.cached_line_count_visible = None;
                    tool.cached_line_count_collapsed = None;
                    m.bump_version();
                    if let Ok(mut c) = self.line_cache.lock() {
                        if id < c.len() {
                            c[id] = None;
                        }
                    }
                    self.invalidate_layout_cache();
                }
            }
        }
    }

    pub fn push_tool_result_message(&mut self, name: String, title: String, content: String) {
        let visible = name != "write_file" && !is_long_tool_content(&content);
        let msg = Message {
            role: Role::Assistant,
            content: String::new(),
            thinking: String::new(),
            thinking_segments: Vec::new(),
            thinking_visible: false,
            tool_results: vec![ToolResultBlock {
                name,
                title,
                content,
                content_offset: 0,
                visible,
                running: false,
                cached_line_count_visible: None,
                cached_line_count_collapsed: None,
            }],
            ts: Utc::now(),
            streaming: false,
            display_cursor: 0,
            skill_ref: None,
            line_count: 0,
            content_version: 0,
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
            let mut changed = false;
            for tool in &mut msg.tool_results {
                if tool.visible != should_expand {
                    tool.visible = should_expand;
                    changed = true;
                }
            }
            if changed {
                msg.bump_version();
            }
        }
        self.invalidate_layout_cache();
    }

    pub fn append_thinking_to_last(&mut self, chunk: &str) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                m.thinking.push_str(chunk);
                let content_len = m.content.len();
                if let Some(last) = m.thinking_segments.last_mut() {
                    if last.offset == content_len {
                        // Same phase (content hasn't grown since last thinking) → extend
                        last.content.push_str(chunk);
                        last.cached_line_count_expanded = None;
                        last.cached_line_count_collapsed = None;
                    } else {
                        // Content has grown → new phase, new segment
                        m.thinking_segments.push(ThinkingSegment {
                            offset: content_len,
                            content: chunk.to_string(),
                            cached_line_count_expanded: None,
                            cached_line_count_collapsed: None,
                        });
                    }
                } else {
                    m.thinking_segments.push(ThinkingSegment {
                        offset: content_len,
                        content: chunk.to_string(),
                        cached_line_count_expanded: None,
                        cached_line_count_collapsed: None,
                    });
                }
                m.bump_version();
                self.invalidate_layout_cache();
            }
        }
    }

    pub fn finish_streaming(&mut self) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                m.streaming = false;
                // Mark any still-running tools as finished.
                for t in &mut m.tool_results {
                    t.running = false;
                }
                // Strip text-based tool call JSON fallback lines from
                // content so they don't appear in the rendered chat.
                m.content = strip_text_tool_calls(&m.content);
                m.line_count = m.content.split('\n').count().max(1) as u32;
                m.bump_version();
                if let Ok(mut c) = self.line_cache.lock() {
                    if id < c.len() {
                        c[id] = None;
                    }
                }
                // Invalidate any per-segment / per-tool counts.
                for seg in m.thinking_segments.iter_mut() {
                    seg.cached_line_count_expanded = None;
                    seg.cached_line_count_collapsed = None;
                }
                for t in m.tool_results.iter_mut() {
                    t.cached_line_count_visible = None;
                    t.cached_line_count_collapsed = None;
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
        if let Ok(mut c) = self.line_cache.lock() {
            c.clear();
        }
        self.invalidate_layout_cache();
    }

    /// Rough count of rendered lines up to (but not including) `msg_idx`,
    /// mirroring the same logic used by `build_lines` in `render.rs`.
    /// Only thinking-mode `Show` counts expanded blocks; `Hide` and
    /// `ShowWhileStreaming` count collapsed toggles.
    pub fn count_lines_before(&mut self, _msg_idx: usize, viewport: u16) -> u32 {
        if self.messages.is_empty() {
            return 0;
        }
        let inner_h = viewport.saturating_sub(2) as u32;

        // Compute total lines the same way render.rs does.
        let total = self.count_all_lines();
        let scroll = (self.scroll as u32).min(total.saturating_sub(inner_h));
        let offset_from_bottom = inner_h + scroll;
        total.saturating_sub(offset_from_bottom)
    }

    /// Count rendered lines for every message using pre-cached per-block
    /// counts. O(N) over messages, but each message is O(1) (sum of
    /// already-cached thinking / tool block counts). The cache is
    /// populated lazily on first call and invalidated by every write
    /// path (see `invalidate_layout_cache`).
    ///
    /// Returns `u32` because 10M-token sessions can easily exceed `u16`.
    /// `width` is the viewport width used for the original cached values.
    pub fn count_all_lines_with_width(&mut self, width: usize) -> u32 {
        let w = width.min(u16::MAX as usize) as u16;
        if let Some((cached_w, n)) = self.cached_total_lines {
            if cached_w == w {
                return n;
            }
        }
        let n = self.compute_total_lines(w);
        self.cached_total_lines = Some((w, n));
        n
    }

    /// Internal: walks the session, populates per-block line caches, and
    /// returns the total. Called by `count_all_lines_with_width` only
    /// when the cached value is stale.
    fn compute_total_lines(&mut self, width: u16) -> u32 {
        let mut n: u32 = 0;
        for m in &mut self.messages {
            // Role prefix line.
            n += 1;

            // Thinking blocks.
            let show_thinking = m.role == Role::Assistant
                && !m.thinking.trim().is_empty()
                && self.display != crate::config::ThinkingDisplay::Hide;
            if show_thinking {
                let expanded = (self.display == crate::config::ThinkingDisplay::Show
                    && m.thinking_visible)
                    || (self.display == crate::config::ThinkingDisplay::ShowWhileStreaming
                        && (m.streaming || m.thinking_visible));
                for seg in m.thinking_segments.iter_mut() {
                    if expanded {
                        if seg.cached_line_count_expanded.is_none() {
                            seg.cached_line_count_expanded =
                                Some(crate::session::render::thinking_block_line_count(
                                    &seg.content,
                                    true,
                                    width as usize,
                                ) as u32);
                        }
                        n += seg.cached_line_count_expanded.unwrap_or(0);
                    } else {
                        if seg.cached_line_count_collapsed.is_none() {
                            seg.cached_line_count_collapsed =
                                Some(crate::session::render::thinking_block_line_count(
                                    &seg.content,
                                    false,
                                    width as usize,
                                ) as u32);
                        }
                        n += seg.cached_line_count_collapsed.unwrap_or(0);
                    }
                }
            }

            // Content lines (raw newline count from `line_count`).
            n += m.line_count;

            // Tool result blocks.
            if self.tool_display != crate::config::ToolResultDisplay::Hide {
                for t in m.tool_results.iter_mut() {
                    let t_vis = match self.tool_display {
                        crate::config::ToolResultDisplay::Show => t.visible || t.running,
                        crate::config::ToolResultDisplay::ShowWhileStreaming => {
                            m.streaming || t.visible || t.running
                        }
                        _ => false,
                    };
                    if t_vis {
                        if t.cached_line_count_visible.is_none() {
                            t.cached_line_count_visible =
                                Some(crate::session::render::tool_block_line_count(
                                    t,
                                    true,
                                    width as usize,
                                ) as u32);
                        }
                        n += t.cached_line_count_visible.unwrap_or(0);
                    } else {
                        if t.cached_line_count_collapsed.is_none() {
                            t.cached_line_count_collapsed =
                                Some(crate::session::render::tool_block_line_count(
                                    t,
                                    false,
                                    width as usize,
                                ) as u32);
                        }
                        n += t.cached_line_count_collapsed.unwrap_or(0);
                    }
                }
            }

            // Spacer.
            n += 1;
        }
        if !self.messages.is_empty() {
            n += 1; // trailing gap line at the bottom
        }
        n
    }

    /// Count rendered lines estimating block widths at 120 columns
    /// (less accurate but doesn't require the viewport width).
    pub fn count_all_lines(&mut self) -> u32 {
        self.count_all_lines_with_width(120)
    }

    /// Set `scroll` so that the last `user` message appears at the top
    /// of the viewport.  Lines after the message will fill the viewport.
    pub fn timeline(&mut self, viewport_height: u16) {
        let inner_h = viewport_height.saturating_sub(2) as u32;
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
        self.scroll = target.min(u16::MAX as u32) as u16;
    }

    /// Set `scroll` so the message at index `msg_idx` appears at the
    /// top of the viewport. No-op if `msg_idx` is out of range.
    pub fn jump_to_message(&mut self, msg_idx: usize, viewport_height: u16) {
        if msg_idx >= self.messages.len() {
            return;
        }
        let inner_h = viewport_height.max(1) as u32;
        let lines_before = self.lines_before(msg_idx);
        let total = self.count_all_lines();
        self.scroll = total
            .saturating_sub(inner_h)
            .saturating_sub(lines_before)
            .min(u16::MAX as u32) as u16;
    }

    /// Number of rendered lines from the top of the buffer up to (but
    /// not including) the message at `msg_idx`. Uses a fixed width of
    /// 120 columns to match the previous (pre-cache) behavior.
    pub fn lines_before(&mut self, msg_idx: usize) -> u32 {
        // Make sure the per-block caches are populated so we can sum
        // without re-rendering.
        let _ = self.count_all_lines_with_width(120);
        let mut n: u32 = 0;
        for (i, m) in self.messages.iter().enumerate() {
            if i >= msg_idx {
                break;
            }
            // Role prefix.
            n += 1;
            // Thinking blocks.
            let show = m.role == Role::Assistant
                && !m.thinking.trim().is_empty()
                && self.display != crate::config::ThinkingDisplay::Hide;
            if show {
                let expanded = (self.display == crate::config::ThinkingDisplay::Show
                    && m.thinking_visible)
                    || (self.display == crate::config::ThinkingDisplay::ShowWhileStreaming
                        && (m.streaming || m.thinking_visible));
                for seg in &m.thinking_segments {
                    let v = if expanded {
                        seg.cached_line_count_expanded.unwrap_or(0)
                    } else {
                        seg.cached_line_count_collapsed.unwrap_or(0)
                    };
                    n += v;
                }
            }
            n += m.line_count;
            if self.tool_display != crate::config::ToolResultDisplay::Hide {
                for t in &m.tool_results {
                    let t_vis = match self.tool_display {
                        crate::config::ToolResultDisplay::Show => t.visible || t.running,
                        crate::config::ToolResultDisplay::ShowWhileStreaming => {
                            m.streaming || t.visible || t.running
                        }
                        _ => false,
                    };
                    let v = if t_vis {
                        t.cached_line_count_visible.unwrap_or(0)
                    } else {
                        t.cached_line_count_collapsed.unwrap_or(0)
                    };
                    n += v;
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
    content.lines().count() > 200 || content.len() > 10_000
}
