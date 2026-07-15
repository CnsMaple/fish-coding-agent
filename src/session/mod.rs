pub mod lru;
pub mod markdown;
pub mod render;
pub mod store;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAttachment {
    /// Absolute path to the saved image file on disk.
    pub asset_path: PathBuf,
    /// MIME type, e.g. "image/png", "image/jpeg".
    pub media_type: String,
    /// File size in bytes.
    pub byte_size: u64,
    /// Image width in pixels (0 if unknown).
    pub width: u32,
    /// Image height in pixels (0 if unknown).
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentPart {
    Text(String),
    Image(ImageAttachment),
}

/// Width-keyed cached line count. Used to cache the rendered line count
/// of message content (markdown + wrapping) per viewport width, since the
/// exact number of display lines depends on the terminal width.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct CachedLineCount {
    pub width: u16,
    pub count: u32,
    /// Byte length of `Message::content` when this count was computed.
    /// Used for fast incremental line-count estimation during streaming.
    pub content_len: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingSegment {
    pub offset: usize,
    pub content: String,
    /// `true` once a non-thinking content block has begun after this
    /// segment, or `begin_thinking_segment` was called explicitly.
    /// `append_thinking_to_last` only appends into the most recent
    /// segment when `closed == false`, so consecutive thinking deltas
    /// that belong to the same Anthropic content block land in the
    /// same rendered box instead of being fragmented into one box per
    /// delta.
    #[serde(default)]
    pub closed: bool,
    /// Snapshot of `Message::tool_results.len()` when this segment was
    /// opened. `append_thinking_to_last` auto-closes the segment as
    /// soon as a tool call is appended (`tool_results.len()` grows),
    /// so reasoning chunks that flank a tool call land in distinct
    /// segments — and therefore in distinct rendered boxes at the
    /// correct offsets — even when no `ContentBlockStart` event was
    /// emitted (which is the case for OpenAI-style providers that
    /// only signal reasoning↔text transitions).
    #[serde(default)]
    pub tool_results_len_at_open: usize,
    /// Cached rendered line count for the expanded thinking block.
    /// `None` means "needs (re)compute"; populated on first render
    /// or by `Session::recompute_layout_caches`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_line_count_expanded: Option<u32>,
    /// Cached rendered line count for the collapsed (single toggle) line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_line_count_collapsed: Option<u32>,
    /// Wall-clock timestamp when this thinking segment started
    /// streaming. Used by the TUI to show `[12s]` elapsed time on
    /// the bottom border of the thinking block.
    #[serde(default)]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Wall-clock timestamp when this thinking segment was closed
    /// (via `begin_thinking_segment` or auto-close). When `None`
    /// the segment is still streaming.
    #[serde(default)]
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultBlock {
    pub name: String,
    pub title: String,
    pub content: String,
    /// UI-only payload that must never be sent to the AI. For
    /// `edit`/`write` tools this holds the `edit_diff` JSON (full
    /// old/new file contents) so the TUI can render a rich diff,
    /// while `content` carries only the short AI-facing success
    /// message. Empty for tools without structured metadata.
    #[serde(default)]
    pub metadata: String,
    pub content_offset: usize,
    pub visible: bool,
    #[serde(default)]
    pub running: bool,
    /// Tool call id that produced this result. Used to reconstruct
    /// the conversation context for the LLM in follow-up turns.
    #[serde(default)]
    pub call_id: String,
    /// When true, the AI-facing `content` has been logically cleared
    /// (replaced with a placeholder) by the prune pass to reclaim
    /// context budget. The original content is still on disk/in the
    /// session for the TUI; only the value sent to the LLM is
    /// swapped. Matches opencode's `part.state.time.compacted`.
    #[serde(default)]
    pub pruned: bool,
    /// Raw accumulated JSON arguments string from the LLM stream.
    /// While `running` is true and this is non-empty, the renderer
    /// shows a streaming preview (command text / code / diff) by
    /// extracting fields from this partial JSON. Cleared when
    /// `ChatToolResult` arrives (the final `title`/`content`/
    /// `metadata` take over).
    #[serde(default)]
    pub streaming_input: String,
    /// Cached rendered line count for the expanded tool block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_line_count_visible: Option<u32>,
    /// Cached rendered line count for the collapsed preview form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_line_count_collapsed: Option<u32>,
    /// `true` when the tool returned `{"ok": false, ...}`. Drives
    /// the error background color in the TUI regardless of tool
    /// type (not just shell/python commands).
    #[serde(default)]
    pub failed: bool,
}

impl ThinkingSegment {
    pub fn cached_line_count(&self, expanded: bool) -> Option<u32> {
        if expanded {
            self.cached_line_count_expanded
        } else {
            self.cached_line_count_collapsed
        }
    }
}

impl ToolResultBlock {
    pub fn cached_line_count(&self, visible: bool) -> Option<u32> {
        if visible {
            self.cached_line_count_visible
        } else {
            self.cached_line_count_collapsed
        }
    }
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
    /// Tool calls emitted by the assistant in this turn.
    /// Used to reconstruct the conversation context for the LLM
    /// in follow-up turns (e.g. after plan/ask interaction tools).
    #[serde(default)]
    pub tool_calls: Vec<SessionToolCall>,
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
    /// Image attachments (screenshots etc.) pasted with this message.
    /// Stored as references to disk-backed files; the raw bytes are not
    /// inlined in the JSONL. Empty vec for plain-text messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ImageAttachment>,

    /// Pre-computed line count (content.split('\n').count()).
    /// Updated when content changes to avoid re-scanning on every frame.
    /// `u32` to support 10M+ token sessions.
    pub line_count: u32,
    /// Cached rendered line count for the content portion (after
    /// markdown rendering and wrapping), keyed by the viewport width.
    /// `None` means "needs (re)compute". Used by
    /// `Session::compute_total_lines` / `lines_before` /
    /// `build_lines_viewport` so the viewport math reflects the actual
    /// number of display lines, not just the raw newline count.
    /// Critical for content with markdown tables / fenced code blocks /
    /// long lines that wrap — `line_count` alone is a strict
    /// undercount and causes the bottom of such messages to be hidden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_content_line_count: Option<CachedLineCount>,
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
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            attachments: Vec::new(),
            ts: Utc::now(),
            streaming: false,
            display_cursor: len, // non-streaming → fully visible
            skill_ref: None,
            line_count,
            cached_content_line_count: None,
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

    /// Invalidate all cached render state for this message.
    /// Call this whenever the message's content, thinking, or tool
    /// blocks change.
    pub fn invalidate_caches(&mut self) {
        self.cached_content_line_count = None;
        self.bump_version();
        for seg in &mut self.thinking_segments {
            seg.cached_line_count_expanded = None;
            seg.cached_line_count_collapsed = None;
        }
        for t in &mut self.tool_results {
            t.cached_line_count_visible = None;
            t.cached_line_count_collapsed = None;
        }
    }

    /// Invalidate render caches but keep `cached_content_line_count`.
    /// Used during streaming appends: the render output changes (so
    /// `content_version` must bump and block caches must clear), but
    /// the content line count stays approximately the same (at most
    /// 1 frame stale) — `compute_total_lines` will use the cached count
    /// and `build_message_lines` will update it naturally when it
    /// re-renders. This avoids a full markdown re-parse on every delta.
    pub fn invalidate_render_caches(&mut self) {
        self.bump_version();
        for seg in &mut self.thinking_segments {
            seg.cached_line_count_expanded = None;
            seg.cached_line_count_collapsed = None;
        }
        for t in &mut self.tool_results {
            t.cached_line_count_visible = None;
            t.cached_line_count_collapsed = None;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    pub messages: Vec<Message>,
    #[serde(default)]
    pub todo_items: Vec<TodoItem>,
    /// scroll offset from bottom; 0 = follow tail
    pub scroll: u32,
    /// id of the message currently being edited/streamed
    #[serde(skip)]
    pub streaming_id: Option<usize>,
    /// Thinking display mode, set from App config on each render.
    #[serde(skip)]
    pub display: crate::config::ThinkingDisplay,
    /// Tool result display mode, set from App config on each render.
    #[serde(skip)]
    pub tool_display: crate::config::ToolResultDisplay,
    /// Number of output lines shown in a collapsed tool block before
    /// the Ctrl+O hint is offered. Mirrors
    /// `Config::tool_preview_lines`; `ui::render` keeps this in sync.
    #[serde(skip)]
    pub tool_preview_lines: usize,
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
    /// Last `(width, total)` observed by the UI render. Used to
    /// pin the viewport start when the user is scrolled up: when
    /// `scroll > 0`, new content height is absorbed into `scroll` so
    /// the view doesn't drift back toward the tail. Reset whenever
    /// the viewport geometry changes (width change, session clear).
    #[serde(skip)]
    pub last_rendered_total: Option<(u16, u32)>,
    /// When `true`, newly created tool result blocks default to
    /// expanded. Set by `toggle_all_tool_results` when the user
    /// presses Ctrl+O to expand all blocks; subsequent tool calls
    /// during the same streaming turn inherit this state.
    #[serde(skip)]
    pub expand_new_tool_results: bool,
    /// Prefix-sum of message line counts. `line_offsets[i]` is the
    /// global line index where message `i` starts. `line_offsets[N]`
    /// (where N = messages.len()) is the total rendered line count.
    /// Populated by `compute_total_lines` and consumed by
    /// `build_lines_viewport` and the toggle-row walk for O(log N)
    /// viewport lookups.
    #[serde(skip)]
    pub line_offsets: Vec<u32>,
    /// When set, the next render will compute `scroll` so that this
    /// line index appears at the top of the viewport, using the
    /// actual `inner_h` known only at render time. This avoids the
    /// stale-height bug where `jump_to_message` computed scroll with
    /// a panel-visible height but the panel is then hidden, making
    /// the viewport taller and the clamp resetting scroll to max.
    #[serde(skip)]
    pub pending_scroll_top: Option<u32>,
}

impl Default for Session {
    /// `tool_preview_lines` needs a non-zero default so freshly
    /// constructed sessions (tests, restores, etc.) render a useful
    /// preview instead of empty boxes. The UI keeps this in sync
    /// with `Config::tool_preview_lines` on every render.
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            todo_items: Vec::new(),
            scroll: 0,
            streaming_id: None,
            display: crate::config::ThinkingDisplay::default(),
            tool_display: crate::config::ToolResultDisplay::default(),
            tool_preview_lines: 10,
            line_cache: std::sync::Mutex::new(Vec::new()),
            message_lines_cache: std::sync::Mutex::new(crate::session::lru::BoundedCache::default()),
            cached_total_lines: None,
            last_rendered_total: None,
            expand_new_tool_results: false,
            line_offsets: Vec::new(),
            pending_scroll_top: None,
        }
    }
}

impl Session {
    /// Mark the layout-derived caches as dirty. Cheap O(1) call. All
    /// write paths (`push`, `append_to_last`, `append_thinking_to_last`,
    /// `start_tool_in_last`, `append_tool_to_last`, `append_tool_delta_to_last`,
    /// `update_last_tool_content`, `finish_streaming`, `toggle_all_tool_results`,
    /// `clear`, resume/fork) MUST call this.
    pub fn invalidate_layout_cache(&mut self) {
        self.cached_total_lines = None;
        self.line_offsets.clear();
    }

    /// Drop any per-message render-LRU entries whose index is
    /// `>= from_idx`. Call this immediately after truncating or
    /// removing messages so a later `push` cannot reuse a stale
    /// render for a now-different (or removed) message slot.
    pub fn invalidate_message_cache_from(&mut self, from_idx: usize) {
        if let Ok(mut lru) = self.message_lines_cache.lock() {
            let stale: Vec<usize> = lru
                .iter_keys()
                .filter(|&&k| k >= from_idx)
                .copied()
                .collect();
            for k in stale {
                lru.remove(&k);
            }
        }
    }

    /// Absorb new streamed content height into `scroll` so the rendered
    /// `start = total - inner_h - scroll` stays constant when the user
    /// has scrolled up. No-op when at tail (`scroll == 0`) so the view
    /// keeps following the latest output. `width` keys the internal
    /// `last_rendered_total` cache so a resize resets the comparison
    /// instead of spuriously subtracting across widths.
    pub fn pin_scroll_for_total(&mut self, width: u16, new_total: u32) {
        let old_total = self
            .last_rendered_total
            .filter(|(w, _)| *w == width)
            .map(|(_, n)| n)
            .unwrap_or(new_total);
        if self.scroll > 0 && new_total > old_total {
            let delta = new_total - old_total;
            let room = u32::MAX - self.scroll;
            self.scroll = self.scroll.saturating_add(delta.min(room));
        }
        self.last_rendered_total = Some((width, new_total));
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
            if let Some(m) = self.messages.get_mut(id) {
                m.content.push_str(chunk);
                if let Some(last) = m.thinking_segments.last_mut() {
                    if !last.closed {
                        last.closed = true;
                    }
                }
                m.line_count = m.content.split('\n').count().max(1) as u32;
                m.invalidate_render_caches();
                if let Ok(mut c) = self.line_cache.lock() {
                    if id < c.len() {
                        c[id] = None;
                    }
                }
                // Keep cursor up-to-date so all content is immediately visible.
                m.display_cursor = m.content.len();
            }
            self.invalidate_layout_cache();
        }
    }

    /// Update the last tool block's content (for streaming: replace placeholder with final content).
    /// If no tool block exists yet (non-streaming path), falls back to appending.
    /// Matches the block by `call_id` (stable identity for parallel tool
    /// calls); falls back to the most recent running block with the same
    /// `name` when `call_id` is empty (legacy / direct-tool-input path).
    pub fn update_last_tool_content(
        &mut self,
        name: String,
        title: String,
        content: String,
        call_id: String,
        metadata: String,
        failed: bool,
    ) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                if let Some(last) = m.thinking_segments.last_mut() {
                    if !last.closed {
                        last.closed = true;
                    }
                }
                // Prefer the stable `call_id` so parallel tool calls
                // with the same name (or interleaved deltas) route
                // to the correct block. Fall back to the old
                // name+running heuristic only when no call_id is set.
                let pos = if !call_id.is_empty() {
                    m.tool_results.iter().rposition(|t| t.call_id == call_id)
                } else {
                    m.tool_results
                        .iter()
                        .rposition(|t| t.name == name && t.running)
                };
                if let Some(pos) = pos {
                    let tool = &mut m.tool_results[pos];
                    tool.content = content;
                    tool.metadata = metadata;
                    tool.running = false;
                    tool.failed = failed;
                    tool.title = title;
                    if tool.call_id.is_empty() {
                        tool.call_id = call_id;
                    }
                    tool.streaming_input.clear();
                    tool.cached_line_count_visible = None;
                    tool.cached_line_count_collapsed = None;
                    m.invalidate_render_caches();
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
        self.append_tool_to_last(name, title, content, metadata, failed);
    }

    pub fn append_tool_to_last(
        &mut self,
        name: String,
        title: String,
        content: String,
        metadata: String,
        failed: bool,
    ) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                if let Some(last) = m.thinking_segments.last_mut() {
                    if !last.closed {
                        last.closed = true;
                    }
                }
                let content_offset = m.content.len();
                let visible = name == "plan" || self.expand_new_tool_results;
                m.tool_results.push(ToolResultBlock {
                    name,
                    title,
                    content,
                    metadata,
                    content_offset,
                    visible,
                    running: false,
                    failed,
                    call_id: String::new(),
                    pruned: false,
                    streaming_input: String::new(),
                    cached_line_count_visible: None,
                    cached_line_count_collapsed: None,
                });
                m.invalidate_render_caches();
                self.invalidate_layout_cache();
            }
        }
    }

    pub fn start_tool_in_last(&mut self, call_id: String, name: String, title: String) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                if let Some(last) = m.thinking_segments.last_mut() {
                    if !last.closed {
                        last.closed = true;
                    }
                }
                // Match an existing running block by the stable
                // `call_id` (the streaming placeholder created during
                // ToolInputDelta) so a ToolStarted arriving for a
                // parallel tool call updates the right block instead
                // of pushing a duplicate.
                let pos = if !call_id.is_empty() {
                    m.tool_results.iter().rposition(|t| t.call_id == call_id)
                } else {
                    m.tool_results
                        .iter()
                        .rposition(|t| t.running && t.name == name)
                };
                if let Some(pos) = pos {
                    let tool = &mut m.tool_results[pos];
                    tool.title = title;
                    tool.running = true;
                    if tool.call_id.is_empty() {
                        tool.call_id = call_id;
                    }
                    tool.cached_line_count_visible = None;
                    tool.cached_line_count_collapsed = None;
                    m.invalidate_render_caches();
                    self.invalidate_layout_cache();
                    return;
                }
                let content_offset = m.content.len();
                let visible = name == "plan" || self.expand_new_tool_results;
                m.tool_results.push(ToolResultBlock {
                    name,
                    title,
                    content: String::new(),
                    metadata: String::new(),
                    content_offset,
                    visible,
                    running: true,
                    failed: false,
                    call_id,
                    pruned: false,
                    streaming_input: String::new(),
                    cached_line_count_visible: None,
                    cached_line_count_collapsed: None,
                });
                m.invalidate_render_caches();
                self.invalidate_layout_cache();
            }
        }
    }

    pub fn append_tool_delta_to_last(&mut self, call_id: &str, delta: &str) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                // Route to the block matching `call_id` (parallel-safe);
                // fall back to the last running block when call_id is empty.
                let pos = if !call_id.is_empty() {
                    m.tool_results.iter().rposition(|t| t.call_id == call_id)
                } else {
                    m.tool_results.iter().rposition(|t| t.running)
                };
                if let Some(pos) = pos {
                    let tool = &mut m.tool_results[pos];
                    tool.content.push_str(delta);
                    tool.cached_line_count_visible = None;
                    tool.cached_line_count_collapsed = None;
                    m.invalidate_render_caches();
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

    /// Update the streaming input (raw accumulated JSON args) on the
    /// tool block identified by `call_id` (or `index` when `call_id` is
    /// empty). If no running block exists for this identity, creates one
    /// first. This is called during LLM streaming so the user sees the
    /// command/code/edit text appear character by character before the
    /// tool executes. Routing by `call_id` (rather than "the last block")
    /// is what makes parallel tool calls render correctly: each tool
    /// call always updates its own block instead of pushing duplicates.
    pub fn update_tool_input_delta(
        &mut self,
        _index: usize,
        call_id: &str,
        name: &str,
        args: &str,
    ) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                let pos = if !call_id.is_empty() {
                    m.tool_results.iter().rposition(|t| t.call_id == call_id)
                } else {
                    m.tool_results
                        .iter()
                        .rposition(|t| t.running && t.name == name)
                };
                if let Some(pos) = pos {
                    let tool = &mut m.tool_results[pos];
                    tool.streaming_input = args.to_string();
                    if tool.name.is_empty() {
                        tool.name = name.to_string();
                    }
                    if tool.call_id.is_empty() && !call_id.is_empty() {
                        tool.call_id = call_id.to_string();
                    }
                    tool.cached_line_count_visible = None;
                    tool.cached_line_count_collapsed = None;
                } else {
                    if let Some(last) = m.thinking_segments.last_mut() {
                        if !last.closed {
                            last.closed = true;
                        }
                    }
                    let content_offset = m.content.len();
                    let visible = name == "plan" || self.expand_new_tool_results;
                    m.tool_results.push(ToolResultBlock {
                        name: name.to_string(),
                        title: String::new(),
                        content: String::new(),
                        metadata: String::new(),
                        content_offset,
                        visible,
                        running: true,
                        failed: false,
                        call_id: call_id.to_string(),
                        pruned: false,
                        streaming_input: args.to_string(),
                        cached_line_count_visible: None,
                        cached_line_count_collapsed: None,
                    });
                }
                m.invalidate_render_caches();
                if let Ok(mut c) = self.line_cache.lock() {
                    if id < c.len() {
                        c[id] = None;
                    }
                }
                self.invalidate_layout_cache();
            }
        }
    }

    pub fn push_tool_result_message(
        &mut self,
        name: String,
        title: String,
        content: String,
        metadata: String,
        failed: bool,
    ) {
        let visible = name == "plan" || self.expand_new_tool_results;
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
                metadata,
                content_offset: 0,
                visible,
                running: false,
                failed,
                call_id: String::new(),
                pruned: false,
                streaming_input: String::new(),
                cached_line_count_visible: None,
                cached_line_count_collapsed: None,
            }],
            tool_calls: Vec::new(),
            attachments: Vec::new(),
            ts: Utc::now(),
            streaming: false,
            display_cursor: 0,
            skill_ref: None,
            line_count: 0,
            cached_content_line_count: None,
            content_version: 0,
        };
        self.push(msg);
    }

    pub fn toggle_all_tool_results(&mut self) {
        let tool_should_expand = self
            .messages
            .iter()
            .flat_map(|m| m.tool_results.iter())
            .any(|tool| !tool.visible && tool.name != "plan");

        let think_should_expand = self
            .messages
            .iter()
            .any(|m| !m.thinking_visible && crate::session::render::message_has_thinking(m));

        let should_expand = tool_should_expand || think_should_expand;

        self.expand_new_tool_results = should_expand;

        for msg in &mut self.messages {
            let mut changed = false;
            for tool in &mut msg.tool_results {
                if tool.name == "plan" {
                    continue;
                }
                if tool.visible != should_expand {
                    tool.visible = should_expand;
                    changed = true;
                }
            }
            if crate::session::render::message_has_thinking(msg)
                && msg.thinking_visible != should_expand
            {
                msg.thinking_visible = should_expand;
                changed = true;
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
                // If the most recent thinking segment is still open
                // (no non-thinking content block has begun since),
                // append into it. Otherwise open a fresh segment.
                let mut extended = false;
                if let Some(last) = m.thinking_segments.last_mut() {
                    if !last.closed {
                        // If content has grown past the segment's offset,
                        // auto-close it so the new thinking lands at the
                        // current content position rather than extending
                        // an older block that visually sits above the
                        // already-streamed text.
                        if m.content.len() > last.offset {
                            last.closed = true;
                        } else if m.tool_results.len() > last.tool_results_len_at_open {
                            // A tool call was appended to this message
                            // since this segment opened. Close it so
                            // the next thinking delta starts a fresh
                            // segment at the new content position
                            // (i.e. alongside the just-completed tool
                            // result), instead of being glued onto the
                            // old pre-tool reasoning block.
                            last.closed = true;
                        } else {
                            last.content.push_str(chunk);
                            last.cached_line_count_expanded = None;
                            last.cached_line_count_collapsed = None;
                            extended = true;
                        }
                    }
                }
                if !extended {
                    let content_len = m.content.len();
                    let tool_results_len = m.tool_results.len();
                    m.thinking_segments.push(ThinkingSegment {
                        offset: content_len,
                        content: chunk.to_string(),
                        closed: false,
                        tool_results_len_at_open: tool_results_len,
                        cached_line_count_expanded: None,
                        cached_line_count_collapsed: None,
                        started_at: Some(chrono::Utc::now()),
                        ended_at: None,
                    });
                }
                m.invalidate_render_caches();
                self.invalidate_layout_cache();
            }
        }
    }

    /// Mark that a fresh thinking content block has begun in the
    /// upstream stream. The next `append_thinking_to_last` will
    /// start a new segment; if there is an in-flight segment from
    /// the previous content block, this call closes it off so the
    /// renderer treats it as a complete block.
    ///
    /// The Anthropic provider fires this on every `content_block_start`
    /// (for thinking, text, or tool_use) so deltas that belong to
    /// the same content block land in the same segment while deltas
    /// from different content blocks are kept separate.
    pub fn begin_thinking_segment(&mut self) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                // Drop the in-flight open segment if it is empty
                // (it was created by a previous content_block_start
                // and has not received any deltas yet). Otherwise
                // close the most recent segment so subsequent
                // thinking deltas land in a fresh one.
                if let Some(last) = m.thinking_segments.last_mut() {
                    if !last.closed && last.content.is_empty() {
                        m.thinking_segments.pop();
                    } else if !last.closed {
                        last.closed = true;
                        last.ended_at = Some(chrono::Utc::now());
                    }
                }
            }
        }
    }

    pub fn finish_streaming(&mut self) {
        if let Some(id) = self.streaming_id {
            if let Some(m) = self.messages.get_mut(id) {
                m.streaming = false;
                // Mark any still-running tools as finished and clear
                // streaming input (the final content/title takes over).
                for t in &mut m.tool_results {
                    t.running = false;
                    t.streaming_input.clear();
                }
                // Strip text-based tool call JSON fallback lines from
                // content so they don't appear in the rendered chat.
                m.content = strip_text_tool_calls(&m.content);
                m.line_count = m.content.split('\n').count().max(1) as u32;
                m.invalidate_caches();
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
                // is ShowWhileStreaming. Plan blocks are never folded.
                if matches!(
                    self.tool_display,
                    crate::config::ToolResultDisplay::ShowWhileStreaming
                ) {
                    for t in &mut m.tool_results {
                        if t.name != "plan" {
                            t.visible = false;
                        }
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
        if let Ok(mut c) = self.message_lines_cache.lock() {
            c.clear();
        }
        self.last_rendered_total = None;
        self.invalidate_layout_cache();
    }

    /// Replace the message slice `messages[start..end]` with a single
    /// `Role::System` summary message. Used by auto-compaction and
    /// the `/compact` slash command. The summary content is stored
    /// verbatim in `Message::content`; the LLM path already turns
    /// `Role::System` into a user-role ChatMessage, which lands the
    /// summary at the top of the next chat turn.
    ///
    /// `start..end` must be a valid range. Returns the index of the
    /// newly-inserted summary message, or `None` when the range is
    /// empty / out of bounds (caller should treat that as "nothing
    /// to do").
    ///
    /// Caches are invalidated: line cache for every removed /
    /// touched message index is dropped, the layout cache is
    /// flushed, and the streaming id is reset (a compaction can
    /// never run while a chat stream is live).
    pub fn apply_compaction(&mut self, start: usize, end: usize, summary: String) -> Option<usize> {
        if start >= end || end > self.messages.len() {
            return None;
        }
        let summary_msg = Message::new(Role::System, summary);
        // Drop everything in [start, end), then insert the summary
        // at `start`. Indices >= end shift by `(end - start) - 1`.
        self.messages
            .splice(start..end, std::iter::once(summary_msg));
        // The splice moved every index >= end down by
        // `(end - start) - 1`. Invalidate their render caches so
        // a stale LRU entry cannot be reused.
        self.invalidate_message_cache_from(start);
        // A streaming id pointing into the removed range is now
        // dangling; clear it.
        if let Some(id) = self.streaming_id {
            if id >= start && id < end {
                self.streaming_id = None;
            } else if id >= end {
                self.streaming_id = Some(id - (end - start) + 1);
            }
        }
        self.invalidate_layout_cache();
        Some(start)
    }

    /// Rough count of rendered lines up to (but not including) `msg_idx`,
    /// mirroring the same logic used by `build_lines` in `render.rs`.
    /// Only thinking-mode `Show` counts expanded expanded blocks; `Hide` and
    /// `ShowWhileStreaming` count collapsed toggles.
    pub fn count_lines_before(&mut self, _msg_idx: usize, viewport: u16) -> u32 {
        if self.messages.is_empty() {
            return 0;
        }
        let inner_h = viewport.saturating_sub(2) as u32;

        // Compute total lines the same way render.rs does.
        let total = self.count_all_lines();
        let scroll = self.scroll.min(total.saturating_sub(inner_h));
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
    ///
    /// The line count must match what `build_message_lines` actually
    /// produces:
    ///   1. Content lines (post-markdown, cached by width).
    ///   2. For each thinking block: the block rows + 1 trailing blank.
    ///   3. For each tool block: the block rows + 1 trailing blank.
    ///   4. If content precedes the first thinking/tool block: 1
    ///      leading gap line (inserted by `ensure_gap_before_block`).
    ///   5. For user messages: 2 extra background-fill lines (one
    ///      inserted above content, one pushed below content) so the
    ///      user-bg block visually wraps the message.
    ///   6. Inter-message gaps and bottom gap are added at the
    ///      session level (below the loop).
    ///
    /// Previously this function (and `build_lines_viewport` /
    /// `count_lines_estimate`) added 1 for a phantom "role prefix" line
    /// that is never actually rendered, and never accounted for the
    /// blank line `build_message_lines` pushes after every block.
    /// That mismatch made the viewport's last 1–N lines of tool /
    /// thinking blocks invisible (the bottom border of a long
    /// write_file diff was the most common casualty).
    fn compute_total_lines(&mut self, width: u16) -> u32 {
        let mut n: u32 = 0;
        let mut offsets = Vec::with_capacity(self.messages.len() + 1);
        offsets.push(0); // line_offsets[0] = 0

        // Snapshot valid `content_line_count` values from the render
        // cache (`message_lines_cache`) before borrowing `&mut
        // self.messages`. When a cache entry is valid (same
        // content_version, width, display_cursor, content_len), the
        // `build_message_lines` call that wrote it already computed
        // the content-only line count — so we can skip the full
        // markdown re-parse that `render_cached_content_lines` would
        // otherwise do. This eliminates the dominant streaming-time
        // CPU hotspot: each frame no longer parses the growing
        // message twice (once for counting, once for rendering).
        let cached_content_counts: Vec<Option<u32>> = {
            let lru = self.message_lines_cache.lock().unwrap();
            self.messages
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    lru.get(&i).and_then(|c| {
                        if c.content_version == m.content_version
                            && c.width == width
                            && c.display_cursor == m.display_cursor
                            && c.content_len == m.content.len()
                        {
                            Some(c.content_line_count)
                        } else {
                            None
                        }
                    })
                })
                .collect()
        };

        for (msg_idx, m) in self.messages.iter_mut().enumerate() {
            // Fast path: use the render cache's content_line_count if
            // the entry is valid. Write it to `cached_content_line_count`
            // so `read_cached_content_count_at` (used by `ui::render`
            // for toggle-row tracking) also hits the fast path.
            let content_lines = if let Some(count) = cached_content_counts[msg_idx] {
                m.cached_content_line_count = Some(crate::session::CachedLineCount {
                    width,
                    count,
                    content_len: m.content.len(),
                });
                count
            } else {
                render_cached_content_lines(m, width)
            };
            n += content_lines;

            // Attachment blocks.
            if !m.attachments.is_empty() {
                n += crate::session::render::attachment_block_line_count(&m.attachments);
            }

            // Thinking blocks.
            let show_thinking = m.role == Role::Assistant
                && crate::session::render::message_has_thinking(m)
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
                                    self.tool_preview_lines,
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
                                    self.tool_preview_lines,
                                    width as usize,
                                ) as u32);
                        }
                        n += seg.cached_line_count_collapsed.unwrap_or(0);
                    }
                    n += 1; // trailing blank after this thinking block
                }
            }

            // Tool result blocks.
            if self.tool_display != crate::config::ToolResultDisplay::Hide {
                for t in m.tool_results.iter_mut() {
                    // Skip placeholder blocks that have no renderable
                    // content. These are stale duplicates left over
                    // from parallel tool-call streaming before
                    // call-id routing; they must not consume a blank
                    // line in the total.
                    if t.content.is_empty() && t.streaming_input.is_empty() {
                        continue;
                    }
                    let t_vis = t.name == "plan"
                        || match self.tool_display {
                            crate::config::ToolResultDisplay::Show => t.visible,
                            crate::config::ToolResultDisplay::ShowWhileStreaming => {
                                m.streaming || t.visible
                            }
                            _ => false,
                        };
                    if t_vis {
                        if t.cached_line_count_visible.is_none() {
                            t.cached_line_count_visible =
                                Some(crate::session::render::tool_block_line_count(
                                    t,
                                    true,
                                    self.tool_preview_lines,
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
                                    self.tool_preview_lines,
                                    width as usize,
                                ) as u32);
                        }
                        n += t.cached_line_count_collapsed.unwrap_or(0);
                    }
                    n += 1; // trailing blank after this tool block
                }
            }

            // Gaps before thinking/tool blocks.
            let segments = crate::session::render::get_thinking_segments(m);
            let gap_count = crate::session::render::count_block_gaps(&segments, &m.tool_results);
            n += gap_count;

            // User messages: background padding + skill block.
            if m.role == Role::User {
                if let Some(skill_ref) = &m.skill_ref {
                    n += crate::session::render::skill_block_line_count(skill_ref, width as usize);
                }
                n += 2;
            }

            // Inter-message gap (also serves as bottom gap for last message).
            n += 1;
            offsets.push(n);
        }
        // If no messages, n == 0 and offsets == [0].
        if self.messages.is_empty() {
            offsets.push(0);
        }
        self.line_offsets = offsets;
        n
    }

    /// Count rendered lines estimating block widths at 120 columns
    /// (less accurate but doesn't require the viewport width).
    pub fn count_all_lines(&mut self) -> u32 {
        self.count_all_lines_with_width(120)
    }

    /// Set `scroll` so that the last `user` message appears at the top
    /// of the viewport.  Lines after the message will fill the viewport.
    pub fn timeline(&mut self, viewport_height: u16, viewport_width: u16) {
        let inner_h = viewport_height.saturating_sub(2) as u32;
        if inner_h == 0 {
            return;
        }

        // Find the last user message.
        let last_user = match self.messages.iter().rposition(|m| m.role == Role::User) {
            Some(i) => i,
            None => return,
        };

        let w = (viewport_width as usize).min(u16::MAX as usize);
        // Ensure line_offsets is populated (see jump_to_message).
        let total = if self.line_offsets.len() <= self.messages.len() {
            self.invalidate_layout_cache();
            self.count_all_lines_with_width(w)
        } else {
            self.count_all_lines_with_width(w)
        };
        let lines_before = self.line_offsets.get(last_user).copied().unwrap_or(0);
        let target = total.saturating_sub(lines_before + inner_h);
        self.scroll = target;
    }

    /// Set `scroll` so the message at index `msg_idx` appears at the
    /// top of the viewport. No-op if `msg_idx` is out of range.
    ///
    /// Uses `line_offsets` (populated by `compute_total_lines` during
    /// the last render) as the source of truth instead of recomputing
    /// `lines_before`, which diverged from the actual render layout
    /// because it didn't interleave thinking/tool blocks with content
    /// the way `build_message_lines` does.
    pub fn jump_to_message(
        &mut self,
        msg_idx: usize,
        tool_idx: Option<usize>,
        viewport_height: u16,
        viewport_width: u16,
    ) {
        if msg_idx >= self.messages.len() {
            return;
        }
        let w = (viewport_width as usize).min(u16::MAX as usize);
        let inner_h = viewport_height.max(1) as u32;

        // Force compute_total_lines so line_offsets is populated even
        // when cached_total_lines is still valid (which skips the
        // compute path in count_all_lines_with_width). line_offsets
        // may have been cleared by invalidate_layout_cache after a
        // tool-toggle or other mutation.
        let total = if self.line_offsets.len() <= self.messages.len() {
            self.invalidate_layout_cache();
            self.count_all_lines_with_width(w)
        } else {
            self.count_all_lines_with_width(w)
        };
        let msg_start = self.line_offsets.get(msg_idx).copied().unwrap_or(0);

        // If a tool index is specified, compute the line offset of that
        // tool block within the message — the number of rendered lines
        // before the tool's top border — so the jump lands on the tool,
        // not just the message start.
        let tool_offset: u32 = if let Some(tool_idx) = tool_idx {
            self.tool_line_offset_within_message(msg_idx, tool_idx, viewport_width)
        } else {
            0
        };

        let lines_before = msg_start + tool_offset;
        self.pending_scroll_top = Some(lines_before);
        self.scroll = total.saturating_sub(inner_h).saturating_sub(lines_before);
    }

    /// Compute the rendered line offset of a tool block's top border
    /// within message `msg_idx` — i.e. how many lines precede the
    /// tool block in the message's rendered output. This includes
    /// content segments before the tool, leading gaps, and any
    /// thinking/tool blocks that sort before it.
    #[allow(unused_assignments)]
    fn tool_line_offset_within_message(&self, msg_idx: usize, tool_idx: usize, width: u16) -> u32 {
        let Some(m) = self.messages.get(msg_idx) else {
            return 0;
        };
        let Some(target_tool) = m.tool_results.get(tool_idx) else {
            return 0;
        };
        if target_tool.content.is_empty() && target_tool.streaming_input.is_empty() {
            return 0;
        }

        let raw = m.visible_content();
        use crate::session::render::{clamp_char_boundary, count_md_segment, strip_legacy_markers};

        // Build sorted items matching build_message_lines.
        enum Item {
            Thinking(usize),
            Tool(usize),
        }
        let mut items: Vec<(usize, Item)> = Vec::new();
        let segments = crate::session::render::get_thinking_segments(m);
        for (si, seg) in segments.iter().enumerate() {
            let offset = clamp_char_boundary(raw, seg.offset.min(raw.len()));
            items.push((offset, Item::Thinking(si)));
        }
        for (ti, t) in m.tool_results.iter().enumerate() {
            if t.content.is_empty() && t.streaming_input.is_empty() {
                continue;
            }
            let offset = clamp_char_boundary(raw, t.content_offset.min(raw.len()));
            items.push((offset, Item::Tool(ti)));
        }
        // Sort with the same tiebreaker as build_message_lines.
        items.sort_by(|(off_a, a), (off_b, b)| {
            off_a.cmp(off_b).then_with(|| match (a, b) {
                (Item::Tool(ti), Item::Thinking(si)) => {
                    let seg = &segments[*si];
                    if *ti >= seg.tool_results_len_at_open {
                        std::cmp::Ordering::Greater
                    } else {
                        std::cmp::Ordering::Less
                    }
                }
                (Item::Thinking(si), Item::Tool(ti)) => {
                    let seg = &segments[*si];
                    if *ti >= seg.tool_results_len_at_open {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
                }
                _ => std::cmp::Ordering::Equal,
            })
        });

        let mut line_count: u32 = 0;
        let mut cursor: usize = 0;
        let mut prev_blank = false;
        let mut has_lines = false;
        let _ = prev_blank;

        // User messages: a blank padding line is inserted at the top.
        if m.role == crate::session::Role::User {
            line_count += 1;
            prev_blank = true;
            has_lines = true;
        }

        for (offset, item) in &items {
            let offset = *offset;
            if offset < cursor {
                continue;
            }

            // Content before this item.
            if offset > cursor {
                let seg_text = strip_legacy_markers(&raw[cursor..offset]);
                let seg_lines = count_md_segment(&seg_text, width as usize);
                line_count += seg_lines;
                cursor = offset;
                has_lines = true;
                // A non-empty content segment ends with a non-blank line,
                // so the next block needs a leading gap.
                prev_blank = false;
            }

            // ensure_gap_before_block.
            if has_lines && !prev_blank {
                line_count += 1;
                prev_blank = true;
            }

            match item {
                Item::Thinking(si) => {
                    let seg = &segments[*si];
                    let expanded = (self.display == crate::config::ThinkingDisplay::Show
                        && m.thinking_visible)
                        || (self.display == crate::config::ThinkingDisplay::ShowWhileStreaming
                            && (m.streaming || m.thinking_visible));
                    let lines = if expanded {
                        seg.cached_line_count_expanded.unwrap_or(0)
                    } else {
                        seg.cached_line_count_collapsed.unwrap_or(0)
                    };
                    line_count += lines;
                    line_count += 1; // trailing blank
                    has_lines = true;
                    prev_blank = true;
                }
                Item::Tool(ti) => {
                    if *ti == tool_idx {
                        return line_count;
                    }
                    let t = &m.tool_results[*ti];
                    let t_vis = t.name == "plan"
                        || match self.tool_display {
                            crate::config::ToolResultDisplay::Show => t.visible,
                            crate::config::ToolResultDisplay::ShowWhileStreaming => {
                                m.streaming || t.visible
                            }
                            _ => false,
                        };
                    let lines = if t_vis {
                        t.cached_line_count_visible.unwrap_or(0)
                    } else {
                        t.cached_line_count_collapsed.unwrap_or(0)
                    };
                    line_count += lines;
                    line_count += 1; // trailing blank
                    has_lines = true;
                    prev_blank = true;
                }
            }
        }

        // Tool not found in the sorted items — return 0 as fallback.
        0
    }

    /// Number of rendered lines from the top of the buffer up to (but
    /// not including) the message at `msg_idx`. Uses `viewport_width`
    /// so the per-segment wrapping matches what the renderer actually
    /// produces at that width.
    ///
    /// Mirrors `compute_total_lines` so the per-block counts plus
    /// trailing blanks plus leading gap plus spacer all add up to the
    /// same number `build_message_lines` would produce.
    pub fn lines_before(&mut self, msg_idx: usize, viewport_width: u16) -> u32 {
        // Make sure the per-block caches are populated so we can sum
        // without re-rendering.
        let _ = self.count_all_lines_with_width(viewport_width as usize);
        let mut n: u32 = 0;
        for (i, m) in self.messages.iter().enumerate() {
            if i >= msg_idx {
                break;
            }
            let content_lines = read_cached_content_lines(m, viewport_width);
            n += content_lines;
            if !m.attachments.is_empty() {
                n += crate::session::render::attachment_block_line_count(&m.attachments);
            }
            let mut thinking_blocks: u32 = 0;
            let show =
                m.role == Role::Assistant && self.display != crate::config::ThinkingDisplay::Hide;
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
                    n += 1; // trailing blank after thinking block
                    thinking_blocks += 1;
                }
            }
            let mut tool_blocks: u32 = 0;
            if self.tool_display != crate::config::ToolResultDisplay::Hide {
                for t in &m.tool_results {
                    let t_vis = t.name == "plan"
                        || match self.tool_display {
                            crate::config::ToolResultDisplay::Show => t.visible,
                            crate::config::ToolResultDisplay::ShowWhileStreaming => {
                                m.streaming || t.visible
                            }
                            _ => false,
                        };
                    let v = if t_vis {
                        t.cached_line_count_visible.unwrap_or(0)
                    } else {
                        t.cached_line_count_collapsed.unwrap_or(0)
                    };
                    n += v;
                    n += 1; // trailing blank after tool block
                    tool_blocks += 1;
                }
            }
            let first_offset = m
                .thinking_segments
                .iter()
                .map(|s| s.offset)
                .chain(m.tool_results.iter().map(|t| t.content_offset))
                .min();
            if first_offset.is_some_and(|off| off > 0) && (thinking_blocks > 0 || tool_blocks > 0) {
                n += 1; // leading gap
            }
            if m.role == Role::User {
                // Count the `[skill]` marker block rows when present
                // (5 rows + 1 trailing blank, or 6 + 1 with non-empty
                // args). Uses the same width as the content count
                // above to match the rendered block width.
                if let Some(skill_ref) = &m.skill_ref {
                    n += crate::session::render::skill_block_line_count(
                        skill_ref,
                        viewport_width as usize,
                    );
                }
                n += 2; // user-bg padding above and below
            }
            n += 1; // gap after this message
        }
        n
    }
}

/// Read or compute the rendered content line count for `m` at `width`,
/// writing the result back to `m.cached_content_line_count`. Used by
/// the `&mut self` paths (`compute_total_lines`) that have write access
/// to the message and want to amortize the markdown-rendering cost
/// across frames.
fn render_cached_content_lines(m: &mut Message, width: u16) -> u32 {
    if let Some(c) = m.cached_content_line_count {
        if c.width == width && m.content.len() == c.content_len {
            return c.count;
        }
    }
    let n = if m.content.trim_start().starts_with("---ask---") {
        crate::session::render::ask_snapshot_line_count(&m.content, width as usize)
    } else {
        let segments = crate::session::render::get_thinking_segments(m);
        crate::session::render::content_line_count_segmented(
            &m.content,
            width as usize,
            &segments,
            &m.tool_results,
        )
    };
    m.cached_content_line_count = Some(CachedLineCount {
        width,
        count: n,
        content_len: m.content.len(),
    });
    n
}

/// Read-only variant used by callers that only have `&Message` (e.g.
/// `Session::lines_before`). Falls back to a live compute when the
/// cache is missing or stale, but does not write back — the next
/// `&mut` pass will populate the cache.
fn read_cached_content_lines(m: &Message, width: u16) -> u32 {
    if let Some(c) = m.cached_content_line_count {
        if c.width == width {
            return c.count;
        }
    }
    if m.content.trim_start().starts_with("---ask---") {
        crate::session::render::ask_snapshot_line_count(&m.content, width as usize)
    } else {
        crate::session::render::content_line_count_segmented(
            &m.content,
            width as usize,
            &m.thinking_segments,
            &m.tool_results,
        )
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

/// Unwrap tool result JSON envelope `{"ok":true,"result":"..."}`.
/// Falls back to the original content string if parsing fails or
/// the envelope is absent.
pub fn unwrap_tool_result_content(content: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin: when scrolled up, the rendered `start` (total - inner_h -
    /// scroll) must stay constant as content grows.
    #[test]
    fn pin_scroll_freezes_view_during_stream() {
        let mut s = Session {
            scroll: 5,
            ..Default::default()
        };
        // Render frame N: total = 100 at width 80.
        s.pin_scroll_for_total(80, 100);
        assert_eq!(s.last_rendered_total, Some((80, 100)));
        // Frame N+1: 7 lines streamed in. scroll must grow by 7 so
        // start = (100+7) - inner_h - (5+7) = 100 - inner_h - 5
        // stays equal to the previous frame's start.
        s.pin_scroll_for_total(80, 107);
        assert_eq!(s.scroll, 12, "scroll must absorb the 7 new lines");
        // Frame N+2: another 3 lines streamed in.
        s.pin_scroll_for_total(80, 110);
        assert_eq!(s.scroll, 15, "scroll must keep absorbing the new height");
    }

    /// Tail: when at the bottom (scroll == 0), no pin; the user
    /// wants to follow the latest output.
    #[test]
    fn pin_scroll_does_not_pin_at_tail() {
        let mut s = Session {
            scroll: 0,
            ..Default::default()
        };
        s.pin_scroll_for_total(80, 100);
        s.pin_scroll_for_total(80, 120);
        assert_eq!(s.scroll, 0, "scroll must remain 0 at the tail");
        assert_eq!(s.last_rendered_total, Some((80, 120)));
    }

    /// Resize: last_rendered_total is keyed by width, so a width
    /// change resets the comparison rather than spuriously dragging
    /// the view across unrelated widths.
    #[test]
    fn pin_scroll_resets_on_width_change() {
        let mut s = Session {
            scroll: 5,
            ..Default::default()
        };
        s.pin_scroll_for_total(80, 100);
        // Width change — treat the new total as the baseline, no
        // delta applied.
        s.pin_scroll_for_total(120, 90);
        assert_eq!(s.scroll, 5, "width change must not adjust scroll");
        assert_eq!(s.last_rendered_total, Some((120, 90)));
        // Subsequent same-width frame applies the delta normally.
        s.pin_scroll_for_total(120, 95);
        assert_eq!(s.scroll, 10, "delta resumes on the new width");
    }

    /// Overflow: scroll saturates at u32::MAX instead of wrapping.
    #[test]
    fn pin_scroll_saturates_at_u32_max() {
        let mut s = Session {
            scroll: u32::MAX - 2,
            ..Default::default()
        };
        s.pin_scroll_for_total(80, 100);
        s.pin_scroll_for_total(80, 200);
        assert_eq!(s.scroll, u32::MAX, "scroll must saturate, never overflow");
    }

    /// Width-keyed baseline: a stale entry at a different width must
    /// not influence the new comparison.
    #[test]
    fn pin_scroll_does_not_cross_widths() {
        let mut s = Session {
            scroll: 5,
            ..Default::default()
        };
        s.pin_scroll_for_total(80, 50);
        // Big jump but at a different width: must be treated as the
        // first observation at that width.
        s.pin_scroll_for_total(120, 1000);
        assert_eq!(s.scroll, 5, "first observation at new width is neutral");
        s.pin_scroll_for_total(120, 1005);
        assert_eq!(s.scroll, 10, "delta applies normally at the new width");
    }
}

#[cfg(test)]
mod compaction_tests {
    use crate::session::{Message, Role, Session};

    #[test]
    fn apply_compaction_drops_replaced_messages() {
        let mut s = Session::default();
        s.push(Message::new(Role::User, "u1"));
        s.push(Message::new(Role::Assistant, "a1"));
        s.push(Message::new(Role::User, "u2"));
        s.push(Message::new(Role::Assistant, "a2"));
        let idx = s.apply_compaction(0, 2, "summary".to_string()).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(s.messages.len(), 3);
        assert_eq!(s.messages[0].role, Role::System);
        assert_eq!(s.messages[0].content, "summary");
        assert_eq!(s.messages[1].content, "u2");
        assert_eq!(s.messages[2].content, "a2");
    }

    #[test]
    fn apply_compaction_rejects_empty_range() {
        let mut s = Session::default();
        s.push(Message::new(Role::User, "u1"));
        assert_eq!(s.apply_compaction(0, 0, "x".to_string()), None);
        assert_eq!(s.apply_compaction(2, 3, "x".to_string()), None);
    }
}
