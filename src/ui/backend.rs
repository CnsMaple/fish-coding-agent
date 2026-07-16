use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// A wrapper backend that de-duplicates cursor commands to prevent
/// irregular cursor blinking.
///
/// ratatui's `apply_buffer_with_cursor` always calls `show_cursor()`
/// (or `hide_cursor()`) and `set_cursor_position()` on every draw
/// frame — even when the cursor state hasn't changed.  Repeatedly
/// sending `\x1B[?25h` or `MoveTo` resets the terminal emulator's
/// native cursor blink timer, producing irregular blinking (sometimes
/// fast, sometimes slow).
///
/// This wrapper tracks:
///
/// - **Cursor visibility** — only forwards `show_cursor` / `hide_cursor`
///   when the state actually changes.
/// - **Cursor position** — only forwards `set_cursor_position` when the
///   position changed OR the previous `draw` wrote cells (which moves
///   the cursor during cell writes).  When the buffer diff is empty
///   (no cells changed) and the target position is the same, the
///   `MoveTo` is skipped entirely, leaving the terminal's blink timer
///   undisturbed.
pub struct CursorTrackingBackend<B> {
    inner: B,
    cursor_visible: bool,
    cursor_pos: Option<Position>,
    /// Whether the last `draw` call wrote any cells.  If `true`, the
    /// cursor was moved during cell writes and must be repositioned
    /// even if the target position is unchanged.
    draw_wrote_cells: bool,
}

impl<B> CursorTrackingBackend<B> {
    pub fn new(inner: B) -> Self {
        Self {
            inner,
            cursor_visible: true,
            cursor_pos: None,
            draw_wrote_cells: false,
        }
    }
}

impl<B: Backend> Backend for CursorTrackingBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut peekable = content.peekable();
        self.draw_wrote_cells = peekable.peek().is_some();
        self.inner.draw(peekable)
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.draw_wrote_cells = false;
        if self.cursor_visible {
            self.inner.hide_cursor()?;
            self.cursor_visible = false;
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        if !self.cursor_visible {
            self.inner.show_cursor()?;
            self.cursor_visible = true;
        }
        Ok(())
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        let needs_move = self.draw_wrote_cells || self.cursor_pos != Some(position);
        if needs_move {
            self.inner.set_cursor_position(position)?;
            self.cursor_pos = Some(position);
        }
        self.draw_wrote_cells = false;
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}
