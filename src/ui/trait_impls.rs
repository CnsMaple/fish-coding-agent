// =====================================================================
// TabWidget implementations
// =====================================================================

use crate::theme::Theme;
use crate::ui::function_panel::{
    ask_active_question_lines, ask_review_lines, ensure_cursor_visible, list_item,
    render_new_provider_picker, visible_window,
};
use crate::ui::tab_widget::{TabCtx, TabWidget};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    }
}

impl TabWidget for crate::function::ModelPickerState {
    fn title(&self) -> &str {
        "model picker"
    }
    fn hint(&self) -> &str {
        if self.context_pick.is_some() {
            " Enter: select | Esc: cancel | Tab: toggle custom input "
        } else {
            " Enter: select | Ctrl+R: refresh | Ctrl+M: manual | Ctrl+E: edit | Esc: close "
        }
    }
    fn has_search(&self) -> bool {
        true
    }
    fn content_height(&self, _ctx: &TabCtx) -> usize {
        if self.fetching || self.fetch_error.is_some() || self.models.is_empty() {
            1
        } else {
            self.filtered.len().max(1)
        }
    }
    fn render_search(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        crate::ui::picker_widget::render_search_row(area, buf, &self.query, self.focus, false)
    }
    fn render_body(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        if area.height < 1 {
            return None;
        }
        if self.fetching {
            Paragraph::new(Line::from(Span::styled(
                "[loading...]",
                Theme::status_warn(),
            )))
            .wrap(Wrap { trim: false })
            .render(area, buf);
        } else if let Some(err) = &self.fetch_error {
            Paragraph::new(Line::from(Span::styled(err.clone(), Theme::status_fail())))
                .wrap(Wrap { trim: false })
                .render(area, buf);
        } else if self.models.is_empty() {
            Paragraph::new(Line::from(Span::styled(
                "[no models - press Ctrl+R to fetch]",
                Theme::dim(),
            )))
            .wrap(Wrap { trim: false })
            .render(area, buf);
        } else if self.filtered.is_empty() {
            Paragraph::new(Line::from(Span::styled(
                "[no matches - Ctrl+M to enter a manual model id]",
                Theme::dim(),
            )))
            .wrap(Wrap { trim: false })
            .render(area, buf);
        } else {
            let range = visible_window(
                self.cursor,
                &mut self.scroll,
                area.height as usize,
                self.filtered.len(),
            );
            for row in range {
                let model_idx = self.filtered[row];
                let model = &self.models[model_idx];
                let is_cursor = row == self.cursor;
                let y = area.y + (row - self.scroll) as u16;
                let mut spans: Vec<Span<'static>> = Vec::new();
                if is_cursor {
                    spans.push(Span::styled("> ", Theme::bold()));
                } else {
                    spans.push(Span::raw("  "));
                }
                spans.push(Span::raw(model.display.clone()));
                if model.display != model.id {
                    spans.push(Span::styled("  ", Theme::dim()));
                    spans.push(Span::styled(model.id.clone(), Theme::dim()));
                }
                if let Some(ctx) = model.context_window_tokens {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(format!("ctx:{}k", ctx / 1000), Theme::dim()));
                } else if model.context_needs_pick {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled("[set context]", Theme::status_warn()));
                }
                buf.set_line(area.x, y, &Line::from(spans), area.width);
            }
        }
        if let Some(ref cp) = self.context_pick {
            let model = &self.models[cp.model_idx];
            let mut picker_lines: Vec<Line<'static>> = Vec::new();
            picker_lines.push(Line::from(Span::styled(
                format!(" Context window for: {}", model.id),
                Theme::bold(),
            )));
            picker_lines.push(Line::from(""));
            let picker_cursor = if cp.focus == crate::function::ContextPickerFocus::Options {
                cp.cursor
            } else {
                usize::MAX
            };
            for (i, opt) in cp.options.iter().enumerate() {
                let prefix = if i == picker_cursor { "> " } else { "  " };
                let mods_str = if opt.modalities.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", opt.modalities.join("+"))
                };
                let label = format!("{}{}k{}", prefix, opt.context / 1000, mods_str);
                let style = if i == picker_cursor {
                    Theme::bold()
                } else {
                    Theme::base()
                };
                picker_lines.push(Line::from(Span::styled(label, style)));
            }
            picker_lines.push(Line::from(""));
            let custom_prefix = if cp.focus == crate::function::ContextPickerFocus::CustomInput {
                "> "
            } else {
                "  "
            };
            let custom_label = if cp.custom_input.is_empty() {
                format!("{}Custom: [____]", custom_prefix)
            } else {
                format!("{}Custom: [{}]", custom_prefix, cp.custom_input)
            };
            let custom_style = if cp.focus == crate::function::ContextPickerFocus::CustomInput {
                Theme::bold()
            } else {
                Theme::base()
            };
            picker_lines.push(Line::from(Span::styled(custom_label, custom_style)));
            Paragraph::new(picker_lines)
                .wrap(Wrap { trim: false })
                .render(area, buf);
        }
        None
    }
}

impl TabWidget for crate::function::ProviderPickerState {
    fn title(&self) -> &str {
        "provider"
    }
    fn hint(&self) -> &str {
        " Enter: pick | Up/Down: nav | type to filter | Ctrl+E: edit | Esc: close "
    }
    fn has_search(&self) -> bool {
        true
    }
    fn content_height(&self, _ctx: &TabCtx) -> usize {
        if self.entries.is_empty() || self.filtered.is_empty() {
            1
        } else {
            self.filtered.len()
        }
    }
    fn render_search(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        crate::ui::picker_widget::render_search_row(area, buf, &self.query, self.focus, false)
    }
    fn render_body(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        if area.height < 1 {
            return None;
        }
        if self.entries.is_empty() {
            Paragraph::new(Line::from(Span::styled(
                "  [no providers configured - open /settings]",
                Theme::dim(),
            )))
            .wrap(Wrap { trim: false })
            .render(area, buf);
        } else if self.filtered.is_empty() {
            Paragraph::new(Line::from(Span::styled("  [no matches]", Theme::dim())))
                .wrap(Wrap { trim: false })
                .render(area, buf);
        } else {
            let range = visible_window(
                self.cursor,
                &mut self.scroll,
                area.height as usize,
                self.filtered.len(),
            );
            for row in range {
                let entry_idx = self.filtered[row];
                let entry = &self.entries[entry_idx];
                let is_cursor = row == self.cursor;
                let is_active = self.active.as_deref() == Some(entry.id.as_str());
                let y = area.y + (row - self.scroll) as u16;
                let mut spans: Vec<Span<'static>> = Vec::new();
                if is_cursor {
                    spans.push(Span::styled("> ", Theme::bold()));
                    spans.push(Span::raw(entry.display.clone()));
                } else {
                    spans.push(Span::raw("  "));
                    spans.push(Span::raw(entry.display.clone()));
                }
                if is_active {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled("[active]", Theme::status_ok()));
                }
                buf.set_line(area.x, y, &Line::from(spans), area.width);
            }
        }
        None
    }
}

impl TabWidget for crate::function::ThinkingPickerState {
    fn title(&self) -> &str {
        "thinking"
    }
    fn has_search(&self) -> bool {
        true
    }
    fn content_height(&self, _ctx: &TabCtx) -> usize {
        if self.filtered.is_empty() {
            1
        } else {
            self.filtered.len()
        }
    }
    fn render_search(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        crate::ui::picker_widget::render_search_row(
            area,
            buf,
            &self.query,
            crate::function::PickerFocus::Search,
            false,
        )
    }
    fn render_body(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        if area.height < 1 {
            return None;
        }
        if self.filtered.is_empty() {
            Paragraph::new(Line::from(Span::styled("  [no matches]", Theme::dim())))
                .wrap(Wrap { trim: false })
                .render(area, buf);
        } else {
            let range = visible_window(
                self.cursor,
                &mut self.scroll,
                area.height as usize,
                self.filtered.len(),
            );
            for row in range {
                let model_idx = self.filtered[row];
                let level = crate::function::ThinkingPickerState::LEVELS[model_idx];
                let is_cursor = row == self.cursor;
                let y = area.y + (row - self.scroll) as u16;
                let line = if is_cursor {
                    Line::from(vec![
                        Span::styled("> ", Theme::bold()),
                        Span::raw(level.to_string()),
                    ])
                } else {
                    Line::from(Span::raw(format!("  {level}")))
                };
                buf.set_line(area.x, y, &line, area.width);
            }
        }
        None
    }
}

impl TabWidget for crate::function::TimelinePickerState {
    fn title(&self) -> &str {
        "timeline"
    }
    fn hint(&self) -> &str {
        " Enter: jump to message | Up/Down: nav | Ctrl+E: edit | Esc: close "
    }
    fn has_search(&self) -> bool {
        true
    }
    fn content_height(&self, _ctx: &TabCtx) -> usize {
        if self.entries.is_empty() || self.filtered.is_empty() {
            1
        } else {
            self.filtered.len()
        }
    }
    fn render_search(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        crate::ui::picker_widget::render_search_row(area, buf, &self.query, self.focus, false)
    }
    fn render_body(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        if area.height < 1 {
            return None;
        }
        if self.entries.is_empty() {
            Paragraph::new(Line::from(Span::styled(
                "[no messages in session]",
                Theme::dim(),
            )))
            .wrap(Wrap { trim: false })
            .render(area, buf);
        } else if self.filtered.is_empty() {
            Paragraph::new(Line::from(Span::styled("[no matches]", Theme::dim())))
                .wrap(Wrap { trim: false })
                .render(area, buf);
        } else {
            let range = visible_window(
                self.cursor,
                &mut self.scroll,
                area.height as usize,
                self.filtered.len(),
            );
            for row in range {
                let entry_idx = self.filtered[row];
                let entry = &self.entries[entry_idx];
                let is_cursor = row == self.cursor;
                let y = area.y + (row - self.scroll) as u16;
                let tag = if entry.tool_idx.is_some() {
                    "tool"
                } else {
                    match entry.role {
                        crate::session::Role::User => "user",
                        crate::session::Role::Assistant => "asst",
                        crate::session::Role::System => "sys ",
                    }
                };
                let tag_span = Span::styled(
                    format!("{tag} "),
                    if entry.tool_idx.is_some() {
                        Theme::dim().add_modifier(Modifier::ITALIC)
                    } else {
                        Theme::dim()
                    },
                );
                if is_cursor {
                    let line = Line::from(vec![
                        Span::styled("> ", Theme::bold()),
                        tag_span,
                        Span::raw(entry.preview.clone()),
                    ]);
                    buf.set_line(area.x, y, &line, area.width);
                } else {
                    let line = Line::from(vec![
                        Span::raw("  "),
                        tag_span,
                        Span::raw(entry.preview.clone()),
                    ]);
                    buf.set_line(area.x, y, &line, area.width);
                }
            }
        }
        None
    }
}

impl TabWidget for crate::function::SessionPickerState {
    fn title(&self) -> &str {
        "sessions"
    }
    fn hint(&self) -> &str {
        " "
    }
    fn has_search(&self) -> bool {
        true
    }
    fn content_height(&self, _ctx: &TabCtx) -> usize {
        if self.entries.is_empty() || self.filtered.is_empty() {
            1
        } else {
            self.filtered.len()
        }
    }
    fn render_search(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        crate::ui::picker_widget::render_search_row(area, buf, &self.query, self.focus, false)
    }
    fn render_body(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        if area.height < 1 {
            return None;
        }
        if self.entries.is_empty() {
            Paragraph::new(Line::from(Span::styled(
                format!("[no {} sessions]", self.scope.label()),
                Theme::dim(),
            )))
            .wrap(Wrap { trim: false })
            .render(area, buf);
        } else if self.filtered.is_empty() {
            Paragraph::new(Line::from(Span::styled("[no matches]", Theme::dim())))
                .wrap(Wrap { trim: false })
                .render(area, buf);
        } else {
            let range = visible_window(
                self.cursor,
                &mut self.scroll,
                area.height as usize,
                self.filtered.len(),
            );
            for row in range {
                let idx = self.filtered[row];
                let entry = &self.entries[idx];
                let y = area.y + (row - self.scroll) as u16;
                let active = row == self.cursor;
                let updated = entry.updated_at.format("%m-%d %H:%M").to_string();
                let last_msg = entry
                    .last_msg_at
                    .map(|t| t.format("%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "-".to_string());
                let cwd = std::path::Path::new(&entry.cwd)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(entry.cwd.as_str());
                let tokens = entry
                    .token_total
                    .map(fmt_tokens)
                    .unwrap_or_else(|| "-".to_string());
                let mut spans = Vec::new();
                spans.push(if active {
                    Span::styled("> ", Theme::bold())
                } else {
                    Span::raw("  ")
                });
                spans.push(Span::raw(entry.title.clone()));
                spans.push(Span::styled(
                    format!(
                        "  {}msg  {}  use:{}  msg:{}  {}",
                        entry.message_count, tokens, updated, last_msg, cwd
                    ),
                    Theme::dim(),
                ));
                buf.set_line(area.x, y, &Line::from(spans), area.width);
            }
        }
        None
    }
    fn render_hint(&self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) {
        let hint_text = " Enter: resume | R: rename | D: delete | F: fork | Tab: local/global | Ctrl+E: edit | Esc: close ";
        let hint = Line::from(vec![
            Span::styled(format!(" [{}] ", self.scope.label()), Theme::bold()),
            Span::styled(hint_text, Theme::dim()),
        ]);
        Paragraph::new(hint).render(area, buf);
    }
}

impl TabWidget for crate::function::SessionRenameState {
    fn title(&self) -> &str {
        "rename"
    }
    fn hint(&self) -> &str {
        " Enter: save | Ctrl+E: edit | Esc: close "
    }
    fn content_height(&self, _ctx: &TabCtx) -> usize {
        1
    }
    fn render_body(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        if area.height < 1 {
            return None;
        }
        let line = Line::from(vec![
            Span::styled(" title: ", Theme::bold()),
            Span::raw(self.title.clone()),
        ]);
        buf.set_line(area.x, area.y, &line, area.width);
        let cursor_byte = self.cursor.min(self.title.len());
        let cursor_w = unicode_width::UnicodeWidthStr::width(&self.title[..cursor_byte]);
        let cursor_x = area.x + 8 + cursor_w as u16;
        Some((cursor_x.min(area.right().saturating_sub(1)), area.y))
    }
}

impl TabWidget for crate::function::PlanState {
    fn title(&self) -> &str {
        "plan"
    }
    fn hint(&self) -> &str {
        " Enter: approve | R: reject | S: save | Esc: close "
    }
    fn content_height(&self, _ctx: &TabCtx) -> usize {
        3
    }
    fn render_body(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        if area.height < 1 {
            return None;
        }
        let status = match self.approved {
            Some(true) => " [approved]",
            Some(false) => " [rejected]",
            None => " [pending]",
        };
        buf.set_line(
            area.x,
            area.y,
            &Line::from(vec![
                Span::styled("Plan: ", Theme::bold()),
                Span::raw(self.title.clone()),
                Span::styled(status, Theme::dim()),
            ]),
            area.width,
        );
        if area.height >= 2 {
            let saved_line = if self.dirty {
                Line::from(vec![Span::styled(
                    "not saved \u{2014} press S to save",
                    Theme::dim(),
                )])
            } else if let Some(p) = &self.path {
                Line::from(vec![
                    Span::styled("saved: ", Theme::dim()),
                    Span::raw(p.display().to_string()),
                ])
            } else {
                Line::from(Span::styled("not saved", Theme::dim()))
            };
            buf.set_line(area.x, area.y + 1, &saved_line, area.width);
        }
        if area.height >= 3 {
            Paragraph::new(Line::from(Span::styled(
                "Read the plan in the session. Use the keys below to act on it.",
                Theme::dim(),
            )))
            .wrap(Wrap { trim: false })
            .render(
                Rect {
                    y: area.y + 2,
                    ..area
                },
                buf,
            );
        }
        None
    }
}

impl TabWidget for crate::function::AskState {
    fn title(&self) -> &str {
        "ask"
    }
    fn hint(&self) -> &str {
        " "
    }
    fn content_height(&self, _ctx: &TabCtx) -> usize {
        use crate::function::AskPhase;
        match self.phase {
            AskPhase::Asking => {
                let active = self.active.min(self.items.len().saturating_sub(1));
                self.items
                    .get(active)
                    .map(|it| 1 + it.options.len() + 1)
                    .unwrap_or(1)
            }
            AskPhase::Reviewing => self.items.len(),
        }
    }
    fn render_body(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        use crate::function::AskPhase;
        if area.height < 1 {
            return None;
        }
        let total = self.items.len();
        let active_idx = self.active.min(total.saturating_sub(1));
        if total > 0 {
            let mut tab_spans: Vec<Span> = Vec::new();
            for (i, it) in self.items.iter().enumerate() {
                let answered = it.answered.is_some();
                let label = format!("q{}", i + 1);
                let (style, mark) = if self.phase == AskPhase::Asking && i == active_idx {
                    (
                        Theme::underlined().add_modifier(ratatui::style::Modifier::BOLD),
                        " ",
                    )
                } else if answered {
                    (Theme::dim(), "\u{2713}")
                } else {
                    (Theme::base(), " ")
                };
                tab_spans.push(Span::styled(format!(" {mark}{label} "), style));
            }
            if total > 0 && (self.all_answered() || self.phase == AskPhase::Reviewing) {
                let style = if self.phase == AskPhase::Reviewing {
                    Theme::underlined().add_modifier(ratatui::style::Modifier::BOLD)
                } else {
                    Theme::dim()
                };
                tab_spans.push(Span::styled(" confirm ", style));
            }
            if !tab_spans.is_empty() {
                buf.set_line(area.x, area.y, &Line::from(tab_spans), area.width);
            }
        }
        let body_area = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
        // Use width-1 so wrapping happens BEFORE the clip boundary.
        // This ensures the next typed character immediately appears on
        // the continuation line instead of being silently clipped.
        let w = (body_area.width as usize).saturating_sub(1).max(8);
        let (body_lines, row_starts) = match self.phase {
            AskPhase::Asking => ask_active_question_lines(self, active_idx, w),
            AskPhase::Reviewing => ask_review_lines(self, w),
        };
        let total_display_lines = body_lines.len();
        if total_display_lines == 0 {
            return None;
        }
        // Convert logical cursor to display-line index via row_starts.
        let cursor_dl = match self.phase {
            AskPhase::Asking => {
                let logical = self.items[active_idx].cursor + 1; // +1 for question row
                row_starts.get(logical).copied().unwrap_or(0)
            }
            AskPhase::Reviewing => row_starts.get(self.active).copied().unwrap_or(0),
        };
        // Display-line-level scroll with cursor-visible enforcement.
        let vh = body_area.height as usize;
        let max_scroll = total_display_lines.saturating_sub(vh);
        let scroll = &mut self.scroll;
        if *scroll > max_scroll {
            *scroll = max_scroll;
        }
        if cursor_dl < *scroll {
            *scroll = cursor_dl;
        } else if cursor_dl >= *scroll + vh {
            *scroll = cursor_dl.saturating_add(1).saturating_sub(vh);
        }
        *scroll = (*scroll).min(max_scroll);
        // Clear the body area before rendering to prevent ghost
        // characters from previous frames.
        for cy in body_area.y..body_area.y + body_area.height {
            for cx in body_area.x..body_area.x + body_area.width {
                buf[(cx, cy)].set_symbol(" ");
            }
        }
        // Render visible display lines.
        let end = (*scroll + vh).min(total_display_lines);
        for (row, line) in body_lines.iter().enumerate().take(end).skip(*scroll) {
            let y = body_area.y + (row - *scroll) as u16;
            buf.set_line(body_area.x, y, line, body_area.width);
        }
        None
    }
    fn render_hint(&self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) {
        use crate::function::AskPhase;
        let hint = match self.phase {
            AskPhase::Asking => {
                if self.items.len() > 1 {
                    " \u{2191}/\u{2193}: navigate | \u{2190}/\u{2192}: switch | Enter: pick | Esc: cancel "
                } else {
                    " \u{2191}/\u{2193}: navigate | Enter: pick | Esc: cancel "
                }
            }
            AskPhase::Reviewing => " \u{2191}/\u{2193}: scroll | Enter: send all | Esc: cancel ",
        };
        Paragraph::new(Line::from(Span::styled(hint, Theme::dim()))).render(area, buf);
    }
}

impl TabWidget for crate::function::TodoTabState {
    fn title(&self) -> &str {
        "todo"
    }
    fn hint(&self) -> &str {
        " "
    }
    fn content_height(&self, ctx: &TabCtx) -> usize {
        ctx.todos.len().max(1)
    }
    fn render_body(&mut self, area: Rect, buf: &mut Buffer, ctx: &TabCtx) -> Option<(u16, u16)> {
        if area.height < 1 {
            return None;
        }
        let todos = ctx.todos;
        if todos.is_empty() {
            Paragraph::new(Line::from(Span::styled("  [no tasks]", Theme::dim())))
                .wrap(Wrap { trim: false })
                .render(area, buf);
        } else {
            ensure_cursor_visible(self.cursor, &mut self.scroll, area.height as usize);
            let range = visible_window(
                self.cursor,
                &mut self.scroll,
                area.height as usize,
                todos.len(),
            );
            for row in range {
                let item = &todos[row];
                let y = area.y + (row - self.scroll) as u16;
                let selected = row == self.cursor;
                let editing = self.editing == Some(row);
                let prefix = if editing || selected { ">" } else { " " };
                let status_style = match item.status.as_str() {
                    "pending" => Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                    "in_progress" => Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                    "completed" => Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                    _ => Theme::dim(),
                };
                let status_label = match item.status.as_str() {
                    "pending" => "\u{25CB}",
                    "in_progress" => "\u{25CC}",
                    "completed" => "\u{2713}",
                    _ => "?",
                };
                let content_display = if editing {
                    format!("{} [edit]", item.content)
                } else {
                    item.content.clone()
                };
                let line = Line::from(vec![
                    Span::styled(
                        format!("{} ", prefix),
                        if selected {
                            Theme::bold()
                        } else {
                            Theme::base()
                        },
                    ),
                    Span::styled(format!("{} ", status_label), status_style),
                    Span::styled(content_display, status_style),
                ]);
                buf.set_line(area.x, y, &line, area.width);
            }
        }
        None
    }
    fn render_hint(&self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) {
        let hint = if self.editing.is_some() {
            Line::from(Span::styled(" Enter:confirm | Esc:cancel ", Theme::dim()))
        } else {
            Line::from(Span::styled(" \u{2191}/\u{2193}:nav | Alt+I:add | Alt+Shift+I:add above | Del:delete | Enter:toggle | Alt+E:edit | Alt+C:clear | Esc:close ", Theme::dim()))
        };
        Paragraph::new(hint).render(area, buf);
    }
}

impl TabWidget for crate::function::ToolPickerState {
    fn title(&self) -> &str {
        "tools"
    }
    fn has_search(&self) -> bool {
        true
    }
    fn content_height(&self, _ctx: &TabCtx) -> usize {
        if self.filtered.is_empty() {
            1
        } else {
            self.filtered.len()
        }
    }
    fn render_search(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        crate::ui::picker_widget::render_search_row(
            area,
            buf,
            &self.query,
            crate::function::PickerFocus::Search,
            false,
        )
    }
    fn render_body(&mut self, area: Rect, buf: &mut Buffer, ctx: &TabCtx) -> Option<(u16, u16)> {
        if area.height < 1 {
            return None;
        }
        if self.filtered.is_empty() {
            Paragraph::new(Line::from(Span::styled("  [no matches]", Theme::dim())))
                .wrap(Wrap { trim: false })
                .render(area, buf);
        } else {
            let range = visible_window(
                self.cursor,
                &mut self.scroll,
                area.height as usize,
                self.filtered.len(),
            );
            for row in range {
                let tool_idx = self.filtered[row];
                let name = &self.tools[tool_idx];
                let mode_denied = matches!(
                    crate::permission::check(ctx.agent, name.as_str()),
                    crate::permission::Action::Deny
                );
                let user_disabled = ctx.disabled_tools.contains(name.as_str());
                let is_cursor = row == self.cursor;
                let (checkbox, style) = if mode_denied {
                    ("\u{1F512}", Theme::dim())
                } else if user_disabled {
                    ("\u{2717}", Theme::dim())
                } else {
                    ("\u{2713}", Theme::base())
                };
                let y = area.y + (row - self.scroll) as u16;
                let line = if is_cursor {
                    Line::from(vec![
                        Span::styled("> ", Theme::bold()),
                        Span::styled(format!("{checkbox} "), style),
                        Span::styled(name.clone(), style),
                    ])
                } else {
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("{checkbox} "), style),
                        Span::styled(name.clone(), style),
                    ])
                };
                buf.set_line(area.x, y, &line, area.width);
            }
        }
        None
    }
}

impl TabWidget for crate::function::PastePreviewState {
    fn title(&self) -> &str {
        "paste"
    }
    fn hint(&self) -> &str {
        " Enter: paste | Esc: cancel "
    }
    fn content_height(&self, _ctx: &TabCtx) -> usize {
        if self.image.is_some() {
            2
        } else if let Some(ref text) = self.text {
            text.lines().count().min(5)
        } else {
            1
        }
    }
    fn render_body(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        if area.height < 1 {
            return None;
        }
        let mut lines = Vec::new();
        if let Some(ref image) = self.image {
            let size_kb = (image.byte_size + 512) / 1024;
            let dim = if image.width > 0 && image.height > 0 {
                format!("{}x{} ", image.width, image.height)
            } else {
                String::new()
            };
            lines.push(Line::from(Span::styled(
                format!("image {} {dim}\u{00B7} {size_kb}KB", image.media_type),
                Theme::bold(),
            )));
            lines.push(Line::from(Span::styled(
                image.asset_path.display().to_string(),
                Theme::dim(),
            )));
        } else if let Some(ref text) = self.text {
            for &line_str in text.lines().take(5).collect::<Vec<&str>>().iter() {
                lines.push(Line::from(Span::raw(line_str)));
            }
        } else {
            lines.push(Line::from(Span::styled(
                "clipboard is empty",
                Style::default().dim(),
            )));
        }
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
        None
    }
    fn render_hint(&self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) {
        let hint_text = if let Some(ref text) = self.text {
            let overflow = text.lines().count().saturating_sub(5);
            if overflow > 0 {
                format!(
                    " ... ({overflow} more lines, {} chars)   Enter: paste | Esc: cancel ",
                    text.len()
                )
            } else {
                format!(" {} chars   Enter: paste | Esc: cancel ", text.len())
            }
        } else {
            String::from(" Enter: paste | Esc: cancel ")
        };
        Paragraph::new(Line::from(Span::styled(hint_text, Theme::dim()))).render(area, buf);
    }
}

impl TabWidget for crate::function::CompletionState {
    fn title(&self) -> &str {
        "completion"
    }
    fn content_height(&self, _ctx: &TabCtx) -> usize {
        self.candidates.len()
    }
    fn render_body(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        if self.candidates.is_empty() {
            Paragraph::new(Line::from(Span::styled("[no completion]", Theme::dim())))
                .wrap(Wrap { trim: false })
                .render(area, buf);
            return None;
        }
        let range = visible_window(
            self.cursor,
            &mut self.scroll,
            area.height as usize,
            self.candidates.len(),
        );
        for row in range {
            let c = &self.candidates[row];
            let is_cursor = row == self.cursor;
            let y = area.y + (row - self.scroll) as u16;
            let line = if is_cursor {
                Line::from(vec![
                    Span::styled("> ", Theme::bold()),
                    Span::raw(c.clone()),
                ])
            } else {
                Line::from(Span::raw(format!("  {c}")))
            };
            buf.set_line(area.x, y, &line, area.width);
        }
        None
    }
}

pub struct HotkeyTab;
impl TabWidget for HotkeyTab {
    fn title(&self) -> &str {
        "hotkey"
    }
    fn content_height(&self, _ctx: &TabCtx) -> usize {
        18
    }
    fn render_body(&mut self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) -> Option<(u16, u16)> {
        let rows: Vec<(&str, &str)> = vec![
            ("Alt+L", "Toggle focus: input \u{2194} panel"),
            ("Tab", "Cycle sidebar tabs"),
            ("Shift+Tab", "Cycle sidebar tabs backwards"),
            ("Enter", "Send / confirm"),
            ("Esc", "Close sidebar tab / focus input"),
            ("Up / Down", "Navigate (focused area)"),
            ("Ctrl+C", "Quit"),
            ("Ctrl+L", "Clear session"),
            ("Ctrl+N", "Toggle notifications panel"),
            ("/", "Open completion"),
            ("/timeline", "Jump to latest prompt"),
            ("/session", "Manage and resume sessions"),
            ("/retry", "Retry previous prompt"),
            ("/continue", "Continue interrupted output"),
            ("/plan", "Switch to plan mode (read-only)"),
            ("/yolo", "Switch back to yolo mode"),
            ("/tool", "Toggle tools for current session"),
            ("Mouse wheel", "Scroll session"),
        ];
        let lines: Vec<Line> = rows
            .into_iter()
            .map(|(k, v)| {
                Line::from(vec![
                    Span::styled(format!("{k:<14}"), Theme::bold()),
                    Span::raw("  "),
                    Span::raw(v),
                ])
            })
            .collect();
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
        None
    }
}

impl TabWidget for crate::function::SettingsState {
    fn title(&self) -> &str {
        "settings"
    }
    fn hint(&self) -> &str {
        " "
    }
    fn content_height(&self, ctx: &TabCtx) -> usize {
        crate::function::settings_body_lines(self, ctx.config).len()
    }
    fn render_body(&mut self, area: Rect, buf: &mut Buffer, ctx: &TabCtx) -> Option<(u16, u16)> {
        use crate::function::SettingsLevel;
        if area.height < 1 {
            return None;
        }
        if matches!(&self.level, SettingsLevel::NewProviderKind) {
            return render_new_provider_picker(area, buf, &mut self.new_provider);
        }
        let cfg = ctx.config;
        let mut body_lines: Vec<Line> = Vec::new();
        match &self.level {
            SettingsLevel::TopLevel => {
                body_lines.push(list_item(0 == self.cursor, "set provider", None));
                body_lines.push(list_item(1 == self.cursor, "thinking display", None));
                body_lines.push(list_item(2 == self.cursor, "tool display", None));
                body_lines.push(list_item(
                    3 == self.cursor,
                    "enter behavior",
                    Some(cfg.enter_behavior.as_str().to_string()),
                ));
                body_lines.push(list_item(
                    4 == self.cursor,
                    "border type",
                    Some(cfg.border_type.as_str().to_string()),
                ));
                body_lines.push(list_item(
                    5 == self.cursor,
                    "theme",
                    Some(cfg.theme.as_str().to_string()),
                ));
                body_lines.push(list_item(
                    6 == self.cursor,
                    "auto compact",
                    Some(if cfg.auto_compact {
                        "on".to_string()
                    } else {
                        "off".to_string()
                    }),
                ));
                body_lines.push(list_item(
                    7 == self.cursor,
                    "tool preview lines",
                    Some(format!(
                        "{}",
                        cfg.tool_preview_lines.clamp(
                            crate::config::TOOL_PREVIEW_LINES_MIN,
                            crate::config::TOOL_PREVIEW_LINES_MAX
                        )
                    )),
                ));
            }
            SettingsLevel::ProviderList => {
                body_lines.push(list_item(0 == self.cursor, "+ new provider", None));
                let keys = cfg.configured_provider_ids();
                for (i, id) in keys.iter().enumerate() {
                    let is_active = cfg.active.as_deref() == Some(id.as_str());
                    let name = cfg.entry(id).and_then(|e| {
                        if e.name.trim().is_empty() {
                            None
                        } else {
                            Some(e.name.clone())
                        }
                    });
                    let mut label = name.unwrap_or_else(|| crate::config::id_display(id));
                    if is_active {
                        label.push_str("  [active]");
                    }
                    body_lines.push(list_item(self.cursor == i + 1, &label, None));
                }
            }
            SettingsLevel::ExistingActions(_) => {
                body_lines.push(list_item(self.cursor == 0, "edit", None));
                body_lines.push(list_item(self.cursor == 1, "delete", None));
            }
            SettingsLevel::ConfigForm(form) => {
                use crate::function::ConfigField;
                let fields = form.active_fields();
                for f in fields.iter() {
                    let focused = form.focused == *f;
                    let label = form.field_label(*f);
                    let value: Option<String> = match f {
                        ConfigField::Name => Some(form.name.clone()),
                        ConfigField::BaseUrl => Some(form.base_url.clone()),
                        ConfigField::Key => {
                            if !form.key_modified && !form.api_key.is_empty() {
                                Some("(set, hidden)".to_string())
                            } else {
                                Some(form.api_key.clone())
                            }
                        }
                        ConfigField::Env => Some(form.api_key_env.clone()),
                        ConfigField::AccessKey => Some(form.access_key.clone()),
                        ConfigField::SecretKey => Some(form.secret_key.clone()),
                        _ => None,
                    };
                    body_lines.push(list_item(focused, label, value));
                }
                if let Some(err) = &form.form_error {
                    body_lines.push(Line::from(""));
                    body_lines.push(Line::from(Span::styled(
                        format!("[!] {err}"),
                        Theme::status_fail(),
                    )));
                }
            }
            SettingsLevel::ThinkingDisplayList => {
                use crate::config::ThinkingDisplay;
                let modes = [
                    ThinkingDisplay::Show,
                    ThinkingDisplay::Hide,
                    ThinkingDisplay::ShowWhileStreaming,
                ];
                for (i, mode) in modes.iter().enumerate() {
                    let is_current = *mode == cfg.thinking_display;
                    let mut label = mode.as_str().to_string();
                    if is_current {
                        label.push_str("  [current]");
                    }
                    body_lines.push(list_item(self.cursor == i, &label, None));
                }
            }
            SettingsLevel::ToolResultDisplayList => {
                use crate::config::ToolResultDisplay;
                let modes = [
                    ToolResultDisplay::Show,
                    ToolResultDisplay::Hide,
                    ToolResultDisplay::ShowWhileStreaming,
                ];
                for (i, mode) in modes.iter().enumerate() {
                    let is_current = *mode == cfg.tool_display;
                    let mut label = mode.as_str().to_string();
                    if is_current {
                        label.push_str("  [current]");
                    }
                    body_lines.push(list_item(self.cursor == i, &label, None));
                }
            }
            SettingsLevel::EnterBehaviorList => {
                use crate::config::EnterBehavior;
                let modes = [EnterBehavior::EnterSends, EnterBehavior::EnterNewline];
                for (i, mode) in modes.iter().enumerate() {
                    let is_current = *mode == cfg.enter_behavior;
                    let mut label = mode.as_str().to_string();
                    if is_current {
                        label.push_str("  [current]");
                    }
                    body_lines.push(list_item(self.cursor == i, &label, None));
                }
            }
            SettingsLevel::BorderTypeList => {
                use crate::ui::border_type::BorderType;
                let modes = [BorderType::Ascii, BorderType::Rounded];
                for (i, mode) in modes.iter().enumerate() {
                    let is_current = *mode == cfg.border_type;
                    let mut label = mode.as_str().to_string();
                    if is_current {
                        label.push_str("  [current]");
                    }
                    body_lines.push(list_item(self.cursor == i, &label, None));
                }
            }
            SettingsLevel::ThemeList => {
                use crate::theme::ThemeVariant;
                let themes = ThemeVariant::all();
                for (i, variant) in themes.iter().enumerate() {
                    let is_current = *variant == cfg.theme;
                    let mut label = variant.as_str().to_string();
                    if is_current {
                        label.push_str("  [current]");
                    }
                    body_lines.push(list_item(self.cursor == i, &label, None));
                }
            }
            SettingsLevel::AutoCompact => {
                let labels = ["on", "off"];
                for (i, label) in labels.iter().enumerate() {
                    let is_current = (i == 0) == cfg.auto_compact;
                    let mut text = (*label).to_string();
                    if is_current {
                        text.push_str("  [current]");
                    }
                    body_lines.push(list_item(self.cursor == i, &text, None));
                }
            }
            SettingsLevel::ToolPreviewLines => {
                use crate::config::{
                    default_tool_preview_lines, TOOL_PREVIEW_LINES_MAX, TOOL_PREVIEW_LINES_MIN,
                };
                let cur = cfg
                    .tool_preview_lines
                    .clamp(TOOL_PREVIEW_LINES_MIN, TOOL_PREVIEW_LINES_MAX);
                let label = format!("preview lines: {cur}  (min {TOOL_PREVIEW_LINES_MIN}, max {TOOL_PREVIEW_LINES_MAX}, default {})", default_tool_preview_lines());
                body_lines.push(list_item(true, &label, None));
            }
            SettingsLevel::NewProviderKind => {}
        }
        if let Some(err) = &self.load_error {
            body_lines.push(Line::from(""));
            body_lines.push(Line::from(Span::styled(
                format!("[config error] {err}"),
                Theme::status_fail(),
            )));
        }
        let total = body_lines.len();
        if total > 0 {
            let range = visible_window(self.cursor, &mut self.scroll, area.height as usize, total);
            for row in range {
                let y = area.y + (row - self.scroll) as u16;
                buf.set_line(area.x, y, &body_lines[row], area.width);
            }
        }
        None
    }
    fn render_hint(&self, area: Rect, buf: &mut Buffer, _ctx: &TabCtx) {
        Paragraph::new(Line::from(Span::styled(self.level.hint(), Theme::dim()))).render(area, buf);
    }
}
