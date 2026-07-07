use std::ops::Range;

use crate::app::App;
use crate::function::SidebarTab;
use crate::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

/// Keep the cursor inside the visible window while scrolling.
pub fn ensure_cursor_visible(cursor: usize, scroll: &mut usize, visible_rows: usize) {
    if visible_rows == 0 {
        return;
    }
    if cursor < *scroll {
        *scroll = cursor;
    } else if cursor >= *scroll + visible_rows {
        *scroll = cursor + 1 - visible_rows;
    }
}

/// Calculate the range of visible rows, auto-scrolling if the cursor is
/// outside the visible window.
pub fn visible_window(
    cursor: usize,
    scroll: &mut usize,
    visible_rows: usize,
    total: usize,
) -> Range<usize> {
    ensure_cursor_visible(cursor, scroll, visible_rows);
    let start = (*scroll).min(total);
    let end = (start + visible_rows).min(total);
    start..end
}

pub fn render(area: Rect, buf: &mut Buffer, app: &mut App) {
    if area.width < 4 || area.height < 4 {
        return;
    }

    let mut title_spans: Vec<Span> = Vec::new();
    for (i, tab) in app.function.tabs.iter().enumerate() {
        if i > 0 {
            title_spans.push(Span::raw(" │ "));
        }
        let name = match tab {
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
            SidebarTab::Todo(_) => "todo",
            SidebarTab::PastePreview(_) => "paste",
            SidebarTab::Hotkey => "hotkey",
        };
        if i == app.function.active {
            title_spans.push(Span::styled(format!(" {name} "), Theme::bold()));
        } else {
            title_spans.push(Span::styled(format!(" {name} "), Theme::dim()));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(app.config.border_type.ratatui_set())
        .border_style(match app.focus_target {
            crate::function::FocusTarget::FunctionPanel => Theme::focused_border(),
            crate::function::FocusTarget::Input => Theme::unfocused_border(),
        })
        .title(Line::from(title_spans));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height < 2 {
        return;
    }

    let todo_items: Vec<crate::session::TodoItem> = app.session.todo_items.clone();
    if let Some(tab) = app.function.tabs.get_mut(app.function.active) {
        let cfg = &app.config;
        let cursor = match tab {
            SidebarTab::Notifications => {
                render_notifications(inner, buf, app);
                None
            }
            SidebarTab::Completion(s) => {
                render_completion(inner, buf, s);
                None
            }
            SidebarTab::Settings(s) => render_settings(inner, buf, cfg, s),
            SidebarTab::ModelPicker(s) => render_picker(inner, buf, s),
            SidebarTab::ProviderPicker(s) => render_provider_picker(inner, buf, s),
            SidebarTab::ThinkingPicker(s) => render_thinking_picker(inner, buf, s),
            SidebarTab::TimelinePicker(s) => render_timeline_picker(inner, buf, s),
            SidebarTab::SessionPicker(s) => render_session_picker(inner, buf, s),
            SidebarTab::SessionRename(s) => render_session_rename(inner, buf, s),
            SidebarTab::Plan(s) => render_plan(inner, buf, s),
            SidebarTab::Ask(s) => render_ask(inner, buf, s),
            SidebarTab::PastePreview(s) => {
                render_paste_preview(inner, buf, s);
                None
            }
            SidebarTab::Todo(s) => {
                render_todo(inner, buf, &todo_items, s);
                None
            }
            SidebarTab::Hotkey => {
                render_hotkey(inner, buf);
                None
            }
        };
        app.function_panel_cursor = cursor;
    }
}

fn render_notifications(area: Rect, buf: &mut Buffer, app: &mut App) {
    if area.height < 3 {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    crate::ui::picker_widget::render_search_row(
        rows[0],
        buf,
        &app.notifications.query,
        crate::function::PickerFocus::List,
        app.notifications.searching,
    );

    let filtered = app.notifications.filtered_indices();
    let list_area = rows[1];
    if filtered.is_empty() {
        let msg = if app.notifications.items.is_empty() {
            "  [no notifications]"
        } else {
            "  [no matches]"
        };
        Paragraph::new(Line::from(Span::styled(msg, Theme::dim())))
            .wrap(Wrap { trim: false })
            .render(list_area, buf);
    } else {
        let filtered_len = filtered.len();
        if filtered_len > 0 {
            ensure_cursor_visible(app.notifications.cursor, &mut app.notifications.scroll, list_area.height as usize);
        }
        let width = list_area.width.saturating_sub(2).max(8) as usize;
        let start = app.notifications.scroll.min(filtered_len);
        let mut visible = Vec::new();
        let mut row_count = 0u16;
        for (row, idx) in filtered.iter().enumerate().skip(start) {
            if row_count >= list_area.height {
                break;
            }
            let t = &app.notifications.items[*idx];
            let selected = row == app.notifications.cursor;
            let level_style = match t.level {
                crate::function::notifications::ToastLevel::Ok => Theme::status_ok(),
                crate::function::notifications::ToastLevel::Info => Theme::status_info(),
                crate::function::notifications::ToastLevel::Warn => Theme::status_warn(),
                crate::function::notifications::ToastLevel::Fail => Theme::status_fail(),
            };
            let prefix = if selected { "> " } else { "  " };
            let head = format!("{}[{}] {}  ", prefix, t.level.tag(), t.format_time());
            let text_width = width.saturating_sub(display_width(&head)).max(8);
            let wrapped = wrap_plain_text(&t.text, text_width);
            let mut lines = Vec::new();
            let first = wrapped.first().cloned().unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(
                    prefix.to_string(),
                    if selected {
                        Theme::bold()
                    } else {
                        Theme::base()
                    },
                ),
                Span::styled(format!("[{}]", t.level.tag()), level_style),
                Span::raw(" "),
                Span::styled(t.format_time(), Theme::dim()),
                Span::raw("  "),
                Span::raw(first),
            ]));
            for cont in wrapped.into_iter().skip(1) {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(display_width(&head))),
                    Span::raw(cont),
                ]));
            }
            let item_height = lines.len() as u16;
            if row_count + item_height > list_area.height && row_count > 0 {
                break;
            }
            row_count = row_count.saturating_add(item_height.max(1));
            visible.push(ListItem::new(lines));
        }
        List::new(visible).render(list_area, buf);
    }

    let hint = if app.notifications.searching {
        Line::from(Span::styled(
            " Up/Down: nav | type: filter | Backspace: edit | Esc: close ",
            Theme::dim(),
        ))
    } else {
        Line::from(Span::styled(
            " Up/Down: nav | Alt+i: search | Esc: close ",
            Theme::dim(),
        ))
    };
    Paragraph::new(hint).render(rows[2], buf);
}

fn display_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

fn wrap_plain_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut line = String::new();
        let mut used = 0usize;
        for ch in raw.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used > 0 && used + w > width {
                out.push(line);
                line = String::new();
                used = 0;
            }
            line.push(ch);
            used += w;
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn render_new_provider_picker(
    area: Rect,
    buf: &mut Buffer,
    s: &mut crate::function::NewProviderPickerState,
) -> Option<(u16, u16)> {
    if area.height < 3 {
        return None;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let search_cursor =
        crate::ui::picker_widget::render_search_row(rows[0], buf, &s.query, s.focus, false);
    let list_area = rows[1];
    if s.filtered.is_empty() {
        Paragraph::new(Line::from(Span::styled("  [no matches]", Theme::dim())))
            .wrap(Wrap { trim: false })
            .render(list_area, buf);
    } else {
        let range = visible_window(s.cursor, &mut s.scroll, list_area.height as usize, s.filtered.len());
        for row in range {
            let idx = s.filtered[row];
            let id = &s.entries[idx];
            let is_cursor = row == s.cursor;
            let y = list_area.y + (row - s.scroll) as u16;
            let picker_label = s.picker_label(id);
            let line = if is_cursor {
                Line::from(vec![
                    Span::styled("> ", Theme::bold()),
                    Span::raw(picker_label),
                ])
            } else {
                Line::from(Span::raw(format!("  {picker_label}")))
            };
            buf.set_line(list_area.x, y, &line, list_area.width);
        }
    }
    Paragraph::new(Line::from(Span::styled(
        " Enter: select | type: filter | Esc: back ",
        Theme::dim(),
    )))
    .render(rows[2], buf);
    search_cursor
}

fn render_completion(area: Rect, buf: &mut Buffer, s: &mut crate::function::CompletionState) {
    if s.candidates.is_empty() {
        Paragraph::new(Line::from(Span::styled("[no completion]", Theme::dim())))
            .wrap(Wrap { trim: false })
            .render(area, buf);
        return;
    }
    let range = visible_window(s.cursor, &mut s.scroll, area.height as usize, s.candidates.len());
    for row in range {
        let c = &s.candidates[row];
        let is_cursor = row == s.cursor;
        let y = area.y + (row - s.scroll) as u16;
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
}

fn render_settings(
    area: Rect,
    buf: &mut Buffer,
    cfg: &crate::config::Config,
    s: &mut crate::function::SettingsState,
) -> Option<(u16, u16)> {
    use crate::function::SettingsLevel;
    if area.height < 3 {
        return None;
    }

    // Layout: list/form on top, blank line, hint at the bottom (dim).
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    let mut body_lines: Vec<Line> = Vec::new();
    match &s.level {
        SettingsLevel::TopLevel => {
            body_lines.push(list_item(0 == s.cursor, "set provider", None));
            body_lines.push(list_item(1 == s.cursor, "thinking display", None));
            body_lines.push(list_item(2 == s.cursor, "tool display", None));
            body_lines.push(list_item(
                3 == s.cursor,
                "enter behavior",
                Some(cfg.enter_behavior.as_str().to_string()),
            ));
            body_lines.push(list_item(
                4 == s.cursor,
                "border type",
                Some(cfg.border_type.as_str().to_string()),
            ));
            body_lines.push(list_item(
                5 == s.cursor,
                "theme",
                Some(cfg.theme.as_str().to_string()),
            ));
            body_lines.push(list_item(
                6 == s.cursor,
                "auto compact",
                Some(if cfg.auto_compact { "on".to_string() } else { "off".to_string() }),
            ));
            body_lines.push(list_item(
                7 == s.cursor,
                "tool preview lines",
                Some(format!(
                    "{}",
                    cfg.tool_preview_lines
                        .clamp(
                            crate::config::TOOL_PREVIEW_LINES_MIN,
                            crate::config::TOOL_PREVIEW_LINES_MAX,
                        )
                )),
            ));
        }
        SettingsLevel::ProviderList => {
            body_lines.push(list_item(0 == s.cursor, "+ new provider", None));
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
                body_lines.push(list_item(s.cursor == i + 1, &label, None));
            }
        }
        SettingsLevel::NewProviderKind => {
            return render_new_provider_picker(area, buf, &mut s.new_provider);
        }
        SettingsLevel::ExistingActions(id) => {
            body_lines.push(list_item(s.cursor == 0, "edit", None));
            body_lines.push(list_item(s.cursor == 1, "delete", None));
            let _ = id;
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
                body_lines.push(list_item(s.cursor == i, &label, None));
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
                body_lines.push(list_item(s.cursor == i, &label, None));
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
                body_lines.push(list_item(s.cursor == i, &label, None));
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
                body_lines.push(list_item(s.cursor == i, &label, None));
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
                body_lines.push(list_item(s.cursor == i, &label, None));
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
                body_lines.push(list_item(s.cursor == i, &text, None));
            }
        }
        SettingsLevel::ToolPreviewLines => {
            use crate::config::{
                default_tool_preview_lines, TOOL_PREVIEW_LINES_MAX, TOOL_PREVIEW_LINES_MIN,
            };
            let cur = cfg
                .tool_preview_lines
                .clamp(TOOL_PREVIEW_LINES_MIN, TOOL_PREVIEW_LINES_MAX);
            let label = format!(
                "preview lines: {cur}  (min {TOOL_PREVIEW_LINES_MIN}, max {TOOL_PREVIEW_LINES_MAX}, default {})",
                default_tool_preview_lines()
            );
            body_lines.push(list_item(true, &label, None));
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
    }
    if let Some(err) = &s.load_error {
        body_lines.push(Line::from(""));
        body_lines.push(Line::from(Span::styled(
            format!("[config error] {err}"),
            Theme::status_fail(),
        )));
    }

    let list_area = rows[0];
    let total = body_lines.len();
    if total > 0 {
        let range = visible_window(s.cursor, &mut s.scroll, list_area.height as usize, total);
        for row in range {
            let y = list_area.y + (row - s.scroll) as u16;
            buf.set_line(list_area.x, y, &body_lines[row], list_area.width);
        }
    }

    // Spacer (rows[1]) is left empty.

    // Hint at the bottom in dim gray.
    Paragraph::new(Line::from(Span::styled(s.level.hint(), Theme::dim()))).render(rows[2], buf);
    None
}

fn list_item(focused: bool, label: &str, value: Option<String>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if focused {
        spans.push(Span::styled("> ", Theme::bold()));
        spans.push(Span::raw(label.to_string()));
    } else {
        spans.push(Span::raw("  "));
        spans.push(Span::raw(label.to_string()));
    }
    if let Some(v) = value {
        spans.push(Span::raw(":  "));
        if v.is_empty() {
            if focused {
                spans.push(Span::styled("<empty>", Theme::dim()));
                // Terminal cursor is positioned here, no drawn cursor needed
            } else {
                spans.push(Span::styled("<empty>".to_string(), Theme::dim()));
            }
        } else if focused {
            spans.push(Span::raw(v));
            // Terminal cursor is positioned at end of value
        } else {
            spans.push(Span::raw(v));
        }
    }
    Line::from(spans)
}

fn render_picker(
    area: Rect,
    buf: &mut Buffer,
    s: &mut crate::function::ModelPickerState,
) -> Option<(u16, u16)> {
    if area.height < 3 {
        return None;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // search row
            Constraint::Min(1),    // list
            Constraint::Length(1), // hint
        ])
        .split(area);

    // --- search row -----------------------------------------------------
    let search_cursor =
        crate::ui::picker_widget::render_search_row(rows[0], buf, &s.query, s.focus, false);

    // --- list -----------------------------------------------------------
    let list_area = rows[1];
    if s.fetching {
        let p = Paragraph::new(Line::from(Span::styled(
            "[loading...]",
            Theme::status_warn(),
        )))
        .wrap(Wrap { trim: false });
        p.render(list_area, buf);
    } else if let Some(err) = &s.fetch_error {
        let p = Paragraph::new(Line::from(Span::styled(err.clone(), Theme::status_fail())))
            .wrap(Wrap { trim: false });
        p.render(list_area, buf);
    } else if s.models.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "[no models - press Ctrl+R to fetch]",
            Theme::dim(),
        )))
        .wrap(Wrap { trim: false });
        p.render(list_area, buf);
    } else if s.filtered.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "[no matches - Ctrl+M to enter a manual model id]",
            Theme::dim(),
        )))
        .wrap(Wrap { trim: false });
        p.render(list_area, buf);
    } else {
        let range = visible_window(s.cursor, &mut s.scroll, list_area.height as usize, s.filtered.len());
        for row in range {
            let model_idx = s.filtered[row];
            let model = &s.models[model_idx];
            let is_cursor = row == s.cursor;
            let y = list_area.y + (row - s.scroll) as u16;
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
                spans.push(Span::styled(
                    format!("ctx:{}k", ctx / 1000),
                    Theme::dim(),
                ));
            } else if model.context_needs_pick {
                spans.push(Span::raw("  "));
                spans.push(Span::styled("[set context]", Theme::status_warn()));
            }
            let line = Line::from(spans);
            buf.set_line(list_area.x, y, &line, list_area.width);
        }
    }

    // --- context picker (overlays list when active) ---------------------
    if let Some(ref cp) = s.context_pick {
        let model = &s.models[cp.model_idx];

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

        let p = Paragraph::new(picker_lines).wrap(Wrap { trim: false });
        p.render(list_area, buf);

        // Override hint
        let hint = Line::from(Span::styled(
            " Enter: select | Esc: cancel | Tab: toggle custom input ",
            Theme::dim(),
        ));
        Paragraph::new(hint).render(rows[2], buf);
        return search_cursor;
    }

    // --- hint row -------------------------------------------------------
    let hint = Line::from(Span::styled(
        " Enter: select | Ctrl+R: refresh | Ctrl+M: manual | Ctrl+E: edit | Esc: close ",
        Theme::dim(),
    ));
    Paragraph::new(hint).render(rows[2], buf);
    search_cursor
}

fn render_provider_picker(
    area: Rect,
    buf: &mut Buffer,
    s: &mut crate::function::ProviderPickerState,
) -> Option<(u16, u16)> {
    if area.height < 3 {
        return None;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // search row
            Constraint::Min(1),    // list
            Constraint::Length(1), // hint
        ])
        .split(area);

    // --- search row -----------------------------------------------------
    let search_cursor =
        crate::ui::picker_widget::render_search_row(rows[0], buf, &s.query, s.focus, false);

    // --- list -----------------------------------------------------------
    let list_area = rows[1];
    if s.entries.is_empty() {
        Paragraph::new(Line::from(Span::styled(
            "  [no providers configured - open /settings]",
            Theme::dim(),
        )))
        .wrap(Wrap { trim: false })
        .render(list_area, buf);
    } else if s.filtered.is_empty() {
        Paragraph::new(Line::from(Span::styled("  [no matches]", Theme::dim())))
            .wrap(Wrap { trim: false })
            .render(list_area, buf);
    } else {
        let range = visible_window(s.cursor, &mut s.scroll, list_area.height as usize, s.filtered.len());
        for row in range {
            let entry_idx = s.filtered[row];
            let entry = &s.entries[entry_idx];
            let is_cursor = row == s.cursor;
            let is_active = s.active.as_deref() == Some(entry.id.as_str());
            let y = list_area.y + (row - s.scroll) as u16;
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
            buf.set_line(list_area.x, y, &Line::from(spans), list_area.width);
        }
    }

    // --- hint row -------------------------------------------------------
    let hint = Line::from(Span::styled(
        " Enter: pick | Up/Down: nav | type to filter | Ctrl+E: edit | Esc: close ",
        Theme::dim(),
    ));
    Paragraph::new(hint).render(rows[2], buf);
    search_cursor
}

fn render_hotkey(area: Rect, buf: &mut Buffer) {
    let rows: Vec<(&str, &str)> = vec![
        ("Alt+L", "Toggle focus: input ↔ panel"),
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
        ("/build", "Switch back to build mode"),
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
}

fn render_paste_preview(area: Rect, buf: &mut Buffer, state: &crate::function::PastePreviewState) {
    use ratatui::widgets::{Paragraph, Wrap};
    use ratatui::text::{Line, Span};
    use ratatui::layout::{Constraint, Direction, Layout};
    use crate::theme::Theme;

    let content_lines = if state.image.is_some() {
        2
    } else if let Some(ref text) = state.text {
        text.lines().count().min(5) as u16
    } else {
        1
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(content_lines), Constraint::Length(1)])
        .split(area);

    let mut lines = Vec::new();

    if let Some(ref image) = state.image {
        let size_kb = (image.byte_size + 512) / 1024;
        let dim = if image.width > 0 && image.height > 0 {
            format!("{}x{} ", image.width, image.height)
        } else {
            String::new()
        };
        lines.push(Line::from(Span::styled(
            format!("image {} {dim}· {size_kb}KB", image.media_type),
            Theme::bold(),
        )));
        lines.push(Line::from(Span::styled(
            image.asset_path.display().to_string(),
            Theme::dim(),
        )));
    } else if let Some(ref text) = state.text {
        let preview_lines: Vec<&str> = text.lines().take(5).collect();
        for &line_str in &preview_lines {
            lines.push(Line::from(Span::raw(line_str)));
        }
    } else {
        lines.push(Line::from(Span::styled("clipboard is empty", Style::default().dim())));
    }

    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    p.render(rows[0], buf);

    // Hint row
    let hint_text = if let Some(ref text) = state.text {
        let overflow = text.lines().count().saturating_sub(5);
        if overflow > 0 {
            format!(" ... ({overflow} more lines, {} chars)   Enter: paste | Esc: cancel ", text.len())
        } else {
            format!(" {} chars   Enter: paste | Esc: cancel ", text.len())
        }
    } else {
        String::from(" Enter: paste | Esc: cancel ")
    };
    let hint = Line::from(Span::styled(hint_text, Theme::dim()));
    Paragraph::new(hint).render(rows[1], buf);
}

fn render_thinking_picker(
    area: Rect,
    buf: &mut Buffer,
    s: &mut crate::function::ThinkingPickerState,
) -> Option<(u16, u16)> {
    if area.height < 1 {
        return None;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // search row
            Constraint::Min(1),    // list
        ])
        .split(area);

    // Search row — ThinkingPicker is always in search-focus mode
    let search_cursor = crate::ui::picker_widget::render_search_row(
        rows[0],
        buf,
        &s.query,
        crate::function::PickerFocus::Search,
        false,
    );

    // List — scroll the visible window so the cursor row is always
    // in view (same pattern as the model / provider / session pickers).
    use crate::function::ThinkingPickerState as TPS;
    let list_area = rows[1];
    if s.filtered.is_empty() {
        Paragraph::new(Line::from(Span::styled("  [no matches]", Theme::dim())))
            .wrap(Wrap { trim: false })
            .render(list_area, buf);
    } else {
        let range = visible_window(s.cursor, &mut s.scroll, list_area.height as usize, s.filtered.len());
        for row in range {
            let model_idx = s.filtered[row];
            let level = TPS::LEVELS[model_idx];
            let is_cursor = row == s.cursor;
            let y = list_area.y + (row - s.scroll) as u16;
            let line = if is_cursor {
                Line::from(vec![
                    Span::styled("> ", Theme::bold()),
                    Span::raw(level.to_string()),
                ])
            } else {
                Line::from(Span::raw(format!("  {level}")))
            };
            buf.set_line(list_area.x, y, &line, list_area.width);
        }
    }
    search_cursor
}

fn render_timeline_picker(
    area: Rect,
    buf: &mut Buffer,
    s: &mut crate::function::TimelinePickerState,
) -> Option<(u16, u16)> {
    if area.height < 3 {
        return None;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // search row
            Constraint::Min(1),    // list
            Constraint::Length(1), // hint
        ])
        .split(area);

    // --- search row ---
    let search_cursor =
        crate::ui::picker_widget::render_search_row(rows[0], buf, &s.query, s.focus, false);

    // --- list ---
    let list_area = rows[1];
    if s.entries.is_empty() {
        Paragraph::new(Line::from(Span::styled(
            "[no messages in session]",
            Theme::dim(),
        )))
        .wrap(Wrap { trim: false })
        .render(list_area, buf);
    } else if s.filtered.is_empty() {
        Paragraph::new(Line::from(Span::styled("[no matches]", Theme::dim())))
            .wrap(Wrap { trim: false })
            .render(list_area, buf);
    } else {
        let range = visible_window(s.cursor, &mut s.scroll, list_area.height as usize, s.filtered.len());
        for row in range {
            let entry_idx = s.filtered[row];
            let entry = &s.entries[entry_idx];
            let is_cursor = row == s.cursor;
            let y = list_area.y + (row - s.scroll) as u16;
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
                buf.set_line(list_area.x, y, &line, list_area.width);
            } else {
                let line = Line::from(vec![
                    Span::raw("  "),
                    tag_span,
                    Span::raw(entry.preview.clone()),
                ]);
                buf.set_line(list_area.x, y, &line, list_area.width);
            }
        }
    }

    // --- hint ---
    let hint = Line::from(Span::styled(
        " Enter: jump to message | Up/Down: nav | Ctrl+E: edit | Esc: close ",
        Theme::dim(),
    ));
    Paragraph::new(hint).render(rows[2], buf);
    search_cursor
}

fn render_session_picker(
    area: Rect,
    buf: &mut Buffer,
    s: &mut crate::function::SessionPickerState,
) -> Option<(u16, u16)> {
    if area.height < 3 {
        return None;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    let search_cursor =
        crate::ui::picker_widget::render_search_row(rows[0], buf, &s.query, s.focus, false);
    let list_area = rows[1];
    if s.entries.is_empty() {
        Paragraph::new(Line::from(Span::styled(
            format!("[no {} sessions]", s.scope.label()),
            Theme::dim(),
        )))
        .wrap(Wrap { trim: false })
        .render(list_area, buf);
    } else if s.filtered.is_empty() {
        Paragraph::new(Line::from(Span::styled("[no matches]", Theme::dim())))
            .wrap(Wrap { trim: false })
            .render(list_area, buf);
    } else {
        let range = visible_window(s.cursor, &mut s.scroll, list_area.height as usize, s.filtered.len());
        for row in range {
            let idx = s.filtered[row];
            let entry = &s.entries[idx];
            let y = list_area.y + (row - s.scroll) as u16;
            let active = row == s.cursor;
            let updated = entry.updated_at.format("%m-%d %H:%M").to_string();
            let cwd = std::path::Path::new(&entry.cwd)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(entry.cwd.as_str());
            let mut spans = Vec::new();
            spans.push(if active {
                Span::styled("> ", Theme::bold())
            } else {
                Span::raw("  ")
            });
            spans.push(Span::raw(entry.title.clone()));
            spans.push(Span::styled(
                format!("  {} msg  {}  {}", entry.message_count, updated, cwd),
                Theme::dim(),
            ));
            buf.set_line(list_area.x, y, &Line::from(spans), list_area.width);
        }
    }

    let hint_text =
        " Enter: resume | R: rename | D: delete | F: fork | Tab: local/global | Ctrl+E: edit | Esc: close ";
    let hint = Line::from(vec![
        Span::styled(format!(" [{}] ", s.scope.label()), Theme::bold()),
        Span::styled(hint_text, Theme::dim()),
    ]);
    Paragraph::new(hint).render(rows[2], buf);
    search_cursor
}

fn render_session_rename(
    area: Rect,
    buf: &mut Buffer,
    s: &crate::function::SessionRenameState,
) -> Option<(u16, u16)> {
    if area.height < 2 {
        return None;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);
    let line = Line::from(vec![
        Span::styled(" title: ", Theme::bold()),
        Span::raw(s.title.clone()),
    ]);
    buf.set_line(rows[0].x, rows[0].y, &line, rows[0].width);
    let cursor_x = rows[0].x + 8 + s.cursor.min(s.title.len()) as u16;
    let hint = Line::from(Span::styled(
        " Enter: save | Ctrl+E: edit | Esc: close ",
        Theme::dim(),
    ));
    Paragraph::new(hint).render(rows[1], buf);
    Some((cursor_x.min(rows[0].right().saturating_sub(1)), rows[0].y))
}

fn render_plan(area: Rect, buf: &mut Buffer, s: &crate::function::PlanState) -> Option<(u16, u16)> {
    // The plan body is shown in the session area. This tab is a slim
    // action bar: title, status, saved-to path, and the key hints.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let status = match s.approved {
        Some(true) => " [approved]",
        Some(false) => " [rejected]",
        None => " [pending]",
    };
    buf.set_line(
        rows[0].x,
        rows[0].y,
        &Line::from(vec![
            Span::styled("Plan: ", Theme::bold()),
            Span::raw(s.title.clone()),
            Span::styled(status, Theme::dim()),
        ]),
        rows[0].width,
    );
    let saved_line = if s.dirty {
        Line::from(vec![Span::styled(
            "not saved — press S to save",
            Theme::dim(),
        )])
    } else if let Some(p) = &s.path {
        Line::from(vec![
            Span::styled("saved: ", Theme::dim()),
            Span::raw(p.display().to_string()),
        ])
    } else {
        Line::from(Span::styled("not saved", Theme::dim()))
    };
    buf.set_line(rows[1].x, rows[1].y, &saved_line, rows[1].width);
    Paragraph::new(Line::from(Span::styled(
        "Read the plan in the session. Use the keys below to act on it.",
        Theme::dim(),
    )))
    .wrap(Wrap { trim: false })
    .render(rows[2], buf);
    Paragraph::new(Line::from(Span::styled(
        " Enter: approve | R: reject | S: save | Esc: close ",
        Theme::dim(),
    )))
    .render(rows[3], buf);
    None
}

fn render_ask(area: Rect, buf: &mut Buffer, s: &mut crate::function::AskState) -> Option<(u16, u16)> {
    use crate::function::AskPhase;

    // Layout matches the screenshot the user picked:
    //
    //     q1 q2 q3 confirm         ← tab strip (every question + confirm)
    //     <question text>          ← current question header
    //      - <option>              ← options for the current question
    //      - <option>
    //      - Type your own answer… ← implicit freeform row
    //     ↑/↓: navigate | Enter: pick | Esc: cancel | ←/→: switch
    //
    // The function panel already has its outer border + title;
    // this tab is rendered inside that area.
    if area.height < 3 {
        return None;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // tab strip
            Constraint::Min(1),    // picker body
            Constraint::Length(1), // hint
        ])
        .split(area);

    // --- tab strip: Q1 Q2 Q3 Confirm (with ✓ / active highlight) ---
    let total = s.items.len();
    let active_idx = s.active.min(total.saturating_sub(1));
    let mut tab_spans: Vec<Span> = Vec::new();
    for (i, it) in s.items.iter().enumerate() {
        let answered = it.answered.is_some();
        let label = format!("q{}", i + 1);
        let (style, mark) = if s.phase == AskPhase::Asking && i == active_idx {
            (
                Theme::underlined().add_modifier(ratatui::style::Modifier::BOLD),
                " ",
            )
        } else if answered {
            (Theme::dim(), "✓")
        } else {
            (Theme::base(), " ")
        };
        tab_spans.push(Span::styled(format!(" {mark}{label} "), style));
    }
    if total > 0 && (s.all_answered() || s.phase == AskPhase::Reviewing) {
        let style = if s.phase == AskPhase::Reviewing {
            Theme::underlined().add_modifier(ratatui::style::Modifier::BOLD)
        } else {
            Theme::dim()
        };
        tab_spans.push(Span::styled(" confirm ", style));
    }
    if !tab_spans.is_empty() {
        buf.set_line(
            rows[0].x,
            rows[0].y,
            &Line::from(tab_spans),
            rows[0].width,
        );
    }

    // --- body ---
    let body_lines: Vec<Line> = match s.phase {
        AskPhase::Asking => ask_active_question_lines(s, active_idx),
        AskPhase::Reviewing => ask_review_lines(s),
    };
    let body_area = rows[1];
    let total = body_lines.len();
    if total > 0 {
        let cursor = match s.phase {
            AskPhase::Asking => s.items[active_idx].cursor,
            AskPhase::Reviewing => 0,
        };
        let range = visible_window(cursor, &mut s.scroll, body_area.height as usize, total);
        for row in range {
            let y = body_area.y + (row - s.scroll) as u16;
            buf.set_line(body_area.x, y, &body_lines[row], body_area.width);
        }
    }

    // --- hint ---
    let hint = match s.phase {
        AskPhase::Asking => {
            if total > 1 {
                " ↑/↓: navigate | ←/→: switch | Enter: pick | Esc: cancel "
            } else {
                " ↑/↓: navigate | Enter: pick | Esc: cancel "
            }
        }
        AskPhase::Reviewing => " ↑/↓: scroll | Enter: send all | Esc: cancel ",
    };
    Paragraph::new(Line::from(Span::styled(hint, Theme::dim()))).render(rows[2], buf);
    None
}

/// Body lines for the Asking phase: the active question + its
/// options. Cursor is marked with `>` and rendered bold; the
/// implicit "Type your own answer…" row is appended as the last
/// row.
///
/// ```text
/// <question>
/// >  - <option>
///    - <option>
/// ```
fn ask_active_question_lines(
    s: &crate::function::AskState,
    active_idx: usize,
) -> Vec<Line<'static>> {
    let Some(it) = s.items.get(active_idx) else {
        return vec![Line::from(Span::styled("(no question)", Theme::dim()))];
    };
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::raw(it.question.clone())));
    for (j, opt) in it.options.iter().enumerate() {
        if j == it.cursor {
            lines.push(Line::from(vec![
                Span::styled(">  - ", Theme::bold()),
                Span::styled(opt.clone(), Theme::bold()),
            ]));
        } else {
            lines.push(Line::from(Span::raw(format!("   - {opt}"))));
        }
    }
    let freeform_idx = it.options.len();
    if freeform_idx == it.cursor {
        lines.push(Line::from(vec![
            Span::styled(">  - ", Theme::bold()),
            Span::styled("Type your own answer…", Theme::dim()),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "   - Type your own answer…",
            Theme::dim(),
        )));
    }
    lines
}

fn render_todo(area: Rect, buf: &mut Buffer, todos: &[crate::session::TodoItem], s: &mut crate::function::TodoTabState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let list_area = rows[0];
    if todos.is_empty() {
        Paragraph::new(Line::from(Span::styled("  [no tasks]", Theme::dim())))
            .wrap(Wrap { trim: false })
            .render(list_area, buf);
    } else {
        ensure_cursor_visible(s.cursor, &mut s.scroll, list_area.height as usize);
        let range = visible_window(s.cursor, &mut s.scroll, list_area.height as usize, todos.len());
        for row in range {
            let item = &todos[row];
            let y = list_area.y + (row - s.scroll) as u16;
            let selected = row == s.cursor;
            let editing = s.editing == Some(row);
            let prefix = if editing { ">" } else if selected { ">" } else { " " };
            let status_style = match item.status.as_str() {
                "pending" => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                "in_progress" => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                "completed" => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                _ => Theme::dim(),
            };
            let status_label = match item.status.as_str() {
                "pending" => "○",
                "in_progress" => "◌",
                "completed" => "✓",
                _ => "?",
            };
            let content_display = if editing { format!("{} [edit]", item.content) } else { item.content.clone() };
            let line = Line::from(vec![
                Span::styled(format!("{} ", prefix), if selected { Theme::bold() } else { Theme::base() }),
                Span::styled(format!("{} ", status_label), status_style),
                Span::styled(content_display, status_style),
            ]);
            buf.set_line(list_area.x, y, &line, list_area.width);
        }
    }
    let hint = if s.editing.is_some() {
        Line::from(Span::styled(
            " Enter:confirm | Esc:cancel ",
            Theme::dim(),
        ))
    } else {
        Line::from(Span::styled(
            " j/k:nav | Alt+I:add | Alt+Shift+I:add above | Del:delete | Enter:toggle | Alt+E:edit | Esc:close ",
            Theme::dim(),
        ))
    };
    Paragraph::new(hint).render(rows[1], buf);
}

/// Body lines for the Reviewing phase: one Q/A pair per question.
///
/// ```text
/// Q1. <question>
///    A. <answer>
/// Q2. <question>
///    A. <answer>
/// ```
fn ask_review_lines(s: &crate::function::AskState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    for (i, it) in s.items.iter().enumerate() {
        let ans = it.answered.as_deref().unwrap_or("(no answer)");
        lines.push(Line::from(Span::styled(
            format!("Q{}. {}", i + 1, it.question),
            Theme::bold(),
        )));
        lines.push(Line::from(Span::raw(format!("   A. {ans}"))));
    }
    lines
}
