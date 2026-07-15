use crate::config::Config;
use crate::session::TodoItem;
use crate::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

/// Shared context passed to every [`TabWidget`] render call.
/// Contains read-only references to app-level data that tabs may need.
pub struct TabCtx<'a> {
    pub config: &'a Config,
    pub todos: &'a [TodoItem],
}

/// A widget rendered inside the function panel's bordered area.
///
/// Each tab type (settings, model picker, etc.) implements this trait.
/// The generic [`render_tab`](Self::render_tab) method handles the
/// three-row vertical layout (search → body → hint); implementations
/// only need to provide the per-row pieces.
///
/// Adding a new tab:
/// 1. Define a state struct (e.g. `MyPickerState`).
/// 2. Add a `SidebarTab::MyPicker(MyPickerState)` variant.
/// 3. `impl TabWidget for MyPickerState { ... }`.
/// 4. Wire the variant in `function_panel.rs::render()`.
pub trait TabWidget {
    /// Tab title shown in the panel's top border.
    fn title(&self) -> &str;

    /// Footer hint text. Empty = no hint row. Override `render_hint`
    /// for dynamic hints.
    fn hint(&self) -> &str {
        ""
    }

    /// Whether this tab has a search/filter input row.
    fn has_search(&self) -> bool {
        false
    }

    /// Number of content lines the body needs. Used for dynamic height.
    fn content_height(&self, ctx: &TabCtx) -> usize;

    /// Render the search row into `buf`. Returns cursor position if focused.
    fn render_search(
        &mut self,
        _area: Rect,
        _buf: &mut Buffer,
        _ctx: &TabCtx,
    ) -> Option<(u16, u16)> {
        None
    }

    /// Render the body content into `buf`. Returns cursor position if focused.
    fn render_body(&mut self, _area: Rect, _buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        None
    }

    /// Render the hint row. Default implementation draws `self.hint()`
    /// in dim style. Override for dynamic hints.
    fn render_hint(&self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) {
        let h = self.hint();
        if !h.is_empty() {
            Paragraph::new(Line::from(Span::styled(h, Theme::dim()))).render(area, buf);
        }
    }

    /// Generic three-row layout: search (optional) → body → hint (optional).
    /// Calls `render_search`, `render_body`, and `render_hint`.
    /// Override only if your tab needs a non-standard layout.
    fn render_tab(&mut self, area: Rect, buf: &mut Buffer, ctx: &TabCtx) -> Option<(u16, u16)> {
        if area.height < 2 {
            return None;
        }

        let mut constraints: Vec<Constraint> = Vec::new();
        if self.has_search() {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Min(1));
        let has_hint = !self.hint().is_empty();
        if has_hint {
            constraints.push(Constraint::Length(1));
        }

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let mut idx = 0;
        let mut cursor = None;
        if self.has_search() {
            cursor = self.render_search(rows[idx], buf, ctx).or(cursor);
            idx += 1;
        }
        cursor = self.render_body(rows[idx], buf, ctx).or(cursor);
        idx += 1;
        if has_hint {
            self.render_hint(rows[idx], buf, ctx);
        }
        cursor
    }
}
