use crate::app::App;
use crate::function::SidebarTab;
use crate::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs, Wrap};

pub fn render(area: Rect, buf: &mut Buffer, app: &mut App) {
    if area.width < 4 || area.height < 4 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(app.config.border_type.ratatui_set())
        .border_style(Theme::unfocused_border())
        .title(Span::styled(" function ", Theme::dim()));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height < 2 {
        return;
    }

    // All tabs (including Notifications) are shown in the row. The earlier
    // model that hid "notif" was too confusing — the user wants to see
    // every tab, even the passive ones.
    let function_tab_indices: Vec<usize> = app
        .function
        .tabs
        .iter()
        .enumerate()
        .map(|(i, _)| i)
        .collect();

    if function_tab_indices.is_empty() {
        // No function tab is open. The body is the Notifications content
        // (or empty if there are no toasts). No tabs row.
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
                SidebarTab::Ask(s) => render_ask(inner, buf, s),
                SidebarTab::Todo(s) => render_todo(inner, buf, s),
                SidebarTab::Plan(s) => render_plan(inner, buf, s),
                SidebarTab::Hotkey => {
                    render_hotkey(inner, buf);
                    None
                }
            };
            app.function_panel_cursor = cursor;
        }
        return;
    }

    // Function tab is active. Show the tabs row (function tabs only) above
    // the body.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    let active_idx = app.function.active;
    let titles: Vec<Line> = function_tab_indices
        .iter()
        .map(|&orig_idx| {
            let name = match app.function.tabs[orig_idx] {
                SidebarTab::Notifications => "notifications",
                SidebarTab::Completion(_) => "completion",
                SidebarTab::Settings(_) => "settings",
                SidebarTab::ModelPicker(_) => "model picker",
                SidebarTab::ProviderPicker(_) => "provider",
                SidebarTab::ThinkingPicker(_) => "thinking",
                SidebarTab::TimelinePicker(_) => "timeline",
                SidebarTab::SessionPicker(_) => "sessions",
                SidebarTab::SessionRename(_) => "rename",
                SidebarTab::Ask(_) => "ask",
                SidebarTab::Todo(_) => "todo",
                SidebarTab::Plan(_) => "plan",
                SidebarTab::Hotkey => "hotkey",
            };
            if orig_idx == active_idx {
                Line::from(Span::styled(
                    format!(" {name} "),
                    Theme::underlined().add_modifier(ratatui::style::Modifier::BOLD),
                ))
            } else {
                Line::from(Span::styled(format!(" {name} "), Theme::dim()))
            }
        })
        .collect();
    let active_filtered = function_tab_indices
        .iter()
        .position(|&i| i == active_idx)
        .unwrap_or(0);
    let tabs = Tabs::new(titles)
        .select(active_filtered)
        .highlight_style(Theme::underlined());
    tabs.render(rows[0], buf);

    // Body
    if let Some(tab) = app.function.tabs.get_mut(active_idx) {
        let cfg = &app.config;
        let cursor = match tab {
            SidebarTab::Notifications => {
                render_notifications(rows[1], buf, app);
                None
            }
            SidebarTab::Completion(s) => {
                render_completion(rows[1], buf, s);
                None
            }
            SidebarTab::Settings(s) => {
                render_settings(rows[1], buf, cfg, s);
                None
            }
            SidebarTab::ModelPicker(s) => render_picker(rows[1], buf, s),
            SidebarTab::ProviderPicker(s) => render_provider_picker(rows[1], buf, s),
            SidebarTab::ThinkingPicker(s) => render_thinking_picker(rows[1], buf, s),
            SidebarTab::TimelinePicker(s) => render_timeline_picker(rows[1], buf, s),
            SidebarTab::SessionPicker(s) => render_session_picker(rows[1], buf, s),
            SidebarTab::SessionRename(s) => render_session_rename(rows[1], buf, s),
            SidebarTab::Ask(s) => render_ask(rows[1], buf, s),
            SidebarTab::Todo(s) => render_todo(rows[1], buf, s),
            SidebarTab::Plan(s) => render_plan(rows[1], buf, s),
            SidebarTab::Hotkey => {
                render_hotkey(rows[1], buf);
                None
            }
        };
        app.function_panel_cursor = cursor;
    }
}

fn render_notifications(area: Rect, buf: &mut Buffer, app: &App) {
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
        let width = list_area.width.saturating_sub(2).max(8) as usize;
        let start = app.notifications.scroll.min(filtered.len());
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

    let hint = Line::from(Span::styled(
        " Up/Down: nav | type: filter | Backspace: edit | Esc: close ",
        Theme::dim(),
    ));
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
        crate::ui::picker_widget::render_search_row(rows[0], buf, &s.query, s.focus);
    let list_area = rows[1];
    if s.filtered.is_empty() {
        Paragraph::new(Line::from(Span::styled("  [no matches]", Theme::dim())))
            .wrap(Wrap { trim: false })
            .render(list_area, buf);
    } else {
        let visible_rows = list_area.height as usize;
        s.ensure_cursor_visible(visible_rows);
        let total = s.filtered.len();
        let start = s.scroll.min(total);
        let end = (start + visible_rows).min(total);
        for row in start..end {
            let idx = s.filtered[row];
            let id = &s.entries[idx];
            let is_cursor = row == s.cursor;
            let y = list_area.y + (row - start) as u16;
            let line = if is_cursor {
                Line::from(vec![
                    Span::styled("> ", Theme::bold()),
                    Span::raw(crate::config::id_display(id)),
                ])
            } else {
                Line::from(Span::raw(format!("  {}", crate::config::id_display(id))))
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

fn render_completion(area: Rect, buf: &mut Buffer, s: &crate::function::CompletionState) {
    let mut lines: Vec<Line> = Vec::new();
    if s.candidates.is_empty() {
        lines.push(Line::from(Span::styled("[no completion]", Theme::dim())));
    } else {
        for (i, c) in s.candidates.iter().enumerate() {
            if i == s.cursor {
                lines.push(Line::from(vec![
                    Span::styled("> ", Theme::bold()),
                    Span::raw(c.clone()),
                ]));
            } else {
                lines.push(Line::from(Span::raw(format!("  {c}"))));
            }
        }
    }
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(area, buf);
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
        SettingsLevel::ConfigForm(form) => {
            use crate::function::ConfigField;
            let fields = [
                ConfigField::Name,
                ConfigField::BaseUrl,
                ConfigField::KeyOrEnv,
                ConfigField::Save,
                ConfigField::Exit,
            ];
            // Determine whether the KeyOrEnv field holds an api_key (secret)
            // or an env name. For the saved api_key we show a placeholder so
            // the secret is not leaked into the terminal scrollback. The user
            // can clear-and-retype to change it; if they leave the field
            // untouched, the original key is preserved on save.
            let key_is_secret = crate::config::parse_id(&form.id)
                .map(|(_, m)| m == crate::config::ProviderMode::Key)
                .unwrap_or(false);
            for (_i, f) in fields.iter().enumerate() {
                // Highlight only the field that is actually focused. Up/Down
                // keeps `form.focused` in sync with `s.cursor` (see
                // `sync_form_focus_to_cursor`), and Tab cycles `form.focused`
                // directly. Using `s.cursor` here as well would make a stale
                // cursor cause two fields to appear focused at once.
                let focused = form.focused == *f;
                let label = form.field_label(*f);
                let value: Option<String> = match f {
                    ConfigField::Name => {
                        if form.name.is_empty() {
                            Some(
                                crate::config::parse_id(&form.id)
                                    .map(|(k, _)| k.as_str().to_string())
                                    .unwrap_or_default(),
                            )
                        } else {
                            Some(form.name.clone())
                        }
                    }
                    ConfigField::BaseUrl => Some(form.base_url.clone()),
                    ConfigField::KeyOrEnv => {
                        let is_oauth = crate::config::parse_id(&form.id)
                            .map(|(_, m)| m == crate::config::ProviderMode::Oauth)
                            .unwrap_or(false);
                        if is_oauth {
                            Some("browser auth on save".to_string())
                        } else if key_is_secret && !form.key_modified && !form.key_or_env.is_empty()
                        {
                            Some("(set, hidden)".to_string())
                        } else {
                            Some(form.key_or_env.clone())
                        }
                    }
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

    Paragraph::new(body_lines)
        .wrap(Wrap { trim: false })
        .render(rows[0], buf);

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
                spans.push(Span::styled("|", Theme::cursor_visible()));
            } else {
                spans.push(Span::styled("<empty>".to_string(), Theme::dim()));
            }
        } else if focused {
            spans.push(Span::raw(v));
            spans.push(Span::styled("|", Theme::cursor_visible()));
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
        crate::ui::picker_widget::render_search_row(rows[0], buf, &s.query, s.focus);

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
        let visible_rows = list_area.height as usize;
        // ensure the focused row is in the visible window
        s.ensure_cursor_visible(visible_rows);
        let total = s.filtered.len();
        let start = s.scroll.min(total);
        let end = (start + visible_rows).min(total);
        for row in start..end {
            let model_idx = s.filtered[row];
            let model = &s.models[model_idx];
            let is_cursor = row == s.cursor;
            let y = list_area.y + (row - start) as u16;
            let line = if is_cursor {
                Line::from(vec![
                    Span::styled("> ", Theme::bold()),
                    Span::raw(model.id.clone()),
                ])
            } else {
                Line::from(Span::raw(format!("  {}", model.id)))
            };
            buf.set_line(list_area.x, y, &line, list_area.width);
        }
    }

    // --- hint row -------------------------------------------------------
    let hint = Line::from(Span::styled(
        " Enter: select | Ctrl+R: refresh | Ctrl+M: manual | Esc: close ",
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
        crate::ui::picker_widget::render_search_row(rows[0], buf, &s.query, s.focus);

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
        let visible_rows = list_area.height as usize;
        s.ensure_cursor_visible(visible_rows);
        let total = s.filtered.len();
        let start = s.scroll.min(total);
        let end = (start + visible_rows).min(total);
        for row in start..end {
            let entry_idx = s.filtered[row];
            let entry = &s.entries[entry_idx];
            let is_cursor = row == s.cursor;
            let is_active = s.active.as_deref() == Some(entry.id.as_str());
            let y = list_area.y + (row - start) as u16;
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
        " Enter: pick | Up/Down: nav | type to filter | Esc: close ",
        Theme::dim(),
    ));
    Paragraph::new(hint).render(rows[2], buf);
    search_cursor
}

fn render_hotkey(area: Rect, buf: &mut Buffer) {
    let rows: Vec<(&str, &str)> = vec![
        ("Tab", "Cycle sidebar tabs"),
        ("Shift+Tab", "Cycle sidebar tabs backwards"),
        ("Enter", "Send / confirm"),
        ("Esc", "Close sidebar tab / clear input"),
        ("Up / Down", "Navigate list / history"),
        ("Ctrl+C", "Quit"),
        ("Ctrl+L", "Clear session"),
        ("Ctrl+N", "Toggle notifications panel"),
        ("/", "Open completion"),
        ("/timeline", "Jump to latest prompt"),
        ("/session", "Manage and resume sessions"),
        ("/retry", "Retry previous prompt"),
        ("/continue", "Continue interrupted output"),
        ("/plan", "Switch to plan mode"),
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

fn render_thinking_picker(
    area: Rect,
    buf: &mut Buffer,
    s: &crate::function::ThinkingPickerState,
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
    );

    // List
    use crate::function::ThinkingPickerState as TPS;
    let mut list_lines: Vec<Line> = Vec::new();
    if s.filtered.is_empty() {
        list_lines.push(Line::from(Span::styled("  [no matches]", Theme::dim())));
    } else {
        for (pos, &model_idx) in s.filtered.iter().enumerate() {
            let level = TPS::LEVELS[model_idx];
            if pos == s.cursor {
                list_lines.push(Line::from(vec![
                    Span::styled("> ", Theme::bold()),
                    Span::raw(level.to_string()),
                ]));
            } else {
                list_lines.push(Line::from(Span::raw(format!("  {level}"))));
            }
        }
    }
    Paragraph::new(list_lines).render(rows[1], buf);
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
        crate::ui::picker_widget::render_search_row(rows[0], buf, &s.query, s.focus);

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
        let visible_rows = list_area.height as usize;
        s.ensure_cursor_visible(visible_rows);
        let total = s.filtered.len();
        let start = s.scroll.min(total);
        let end = (start + visible_rows).min(total);
        for row in start..end {
            let entry_idx = s.filtered[row];
            let entry = &s.entries[entry_idx];
            let is_cursor = row == s.cursor;
            let y = list_area.y + (row - start) as u16;
            let tag = match entry.role {
                crate::session::Role::User => "user",
                crate::session::Role::Assistant => "asst",
                crate::session::Role::System => "sys ",
            };
            let tag_span = Span::styled(format!("{tag} "), Theme::dim());
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
        " Enter: jump to message | Up/Down: nav | Esc: close ",
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
        crate::ui::picker_widget::render_search_row(rows[0], buf, &s.query, s.focus);
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
        let visible_rows = list_area.height as usize;
        s.ensure_cursor_visible(visible_rows);
        let total = s.filtered.len();
        let start = s.scroll.min(total);
        let end = (start + visible_rows).min(total);
        for row in start..end {
            let idx = s.filtered[row];
            let entry = &s.entries[idx];
            let y = list_area.y + (row - start) as u16;
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
        " Enter: resume | R: rename | D: delete | F: fork | Tab: local/global | Esc: close ";
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
    let hint = Line::from(Span::styled(" Enter: save | Esc: close ", Theme::dim()));
    Paragraph::new(hint).render(rows[1], buf);
    Some((cursor_x.min(rows[0].right().saturating_sub(1)), rows[0].y))
}

fn render_ask(area: Rect, buf: &mut Buffer, s: &crate::function::AskState) -> Option<(u16, u16)> {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("? ", Theme::bold()),
        Span::raw(s.question.clone()),
    ]));
    lines.push(Line::from(""));
    if s.options.is_empty() {
        // Free-form mode: the user types into `s.input` directly in
        // this panel (no options to pick). Show a one-line input
        // box and return its cursor so the OS-level IME / cursor
        // placement still works.
        lines.push(Line::from(vec![
            Span::styled("> ", Theme::bold()),
            Span::raw(s.input.clone()),
        ]));
    } else {
        for (i, opt) in s.options.iter().enumerate() {
            if i == s.cursor {
                lines.push(Line::from(vec![
                    Span::styled("> ", Theme::bold()),
                    Span::raw(opt.clone()),
                ]));
            } else {
                lines.push(Line::from(Span::raw(format!("  {opt}"))));
            }
        }
    }
    if let Some(ans) = &s.answered {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("answered: ", Theme::status_ok()),
            Span::raw(ans.clone()),
        ]));
    }
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(rows[0], buf);
    let hint = if s.options.is_empty() {
        " Enter: submit | Backspace: delete | Esc: close "
    } else {
        " Enter: answer | Up/Down: nav | Esc: close "
    };
    Paragraph::new(Line::from(Span::styled(hint, Theme::dim()))).render(rows[1], buf);

    // For free-form mode, expose the text cursor so position_ime_cursor
    // can place the hardware cursor on top of the input. The visible
    // "> " prefix is 2 cells wide, and we add a small left margin of
    // 1 cell so the cursor lines up with the first character of input.
    if s.options.is_empty() {
        // Find the y coordinate of the input line — it is the
        // third Line we pushed (index 2: question, blank, input).
        let y = rows[0].y + 2;
        // Convert input_cursor (byte offset) to a display column.
        let prefix_cols: u16 = 2; // "> "
        let cursor_col = s.input[..s.input_cursor.min(s.input.len())]
            .chars()
            .count() as u16;
        let x = (rows[0].x + prefix_cols + cursor_col).min(rows[0].right().saturating_sub(1));
        return Some((x, y));
    }
    None
}

fn render_todo(area: Rect, buf: &mut Buffer, s: &crate::function::TodoState) -> Option<(u16, u16)> {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let mut lines = Vec::new();
    if s.items.is_empty() {
        lines.push(Line::from(Span::styled("[no todos]", Theme::dim())));
    } else {
        for (i, item) in s.items.iter().enumerate() {
            let mark = match item.status.as_str() {
                "completed" | "done" => "[x]",
                "in_progress" | "running" => "[>]",
                _ => "[ ]",
            };
            let prefix = if i == s.cursor { "> " } else { "  " };
            lines.push(Line::from(vec![
                Span::styled(
                    prefix,
                    if i == s.cursor {
                        Theme::bold()
                    } else {
                        Theme::base()
                    },
                ),
                Span::styled(format!("{mark} "), Theme::dim()),
                Span::raw(item.content.clone()),
            ]));
        }
    }
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(rows[0], buf);
    Paragraph::new(Line::from(Span::styled(
        " Up/Down: nav | Esc: close ",
        Theme::dim(),
    )))
    .render(rows[1], buf);
    None
}

fn render_plan(area: Rect, buf: &mut Buffer, s: &crate::function::PlanState) -> Option<(u16, u16)> {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
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
    let lines: Vec<Line> = s
        .content
        .lines()
        .map(|line| Line::from(Span::raw(line.to_string())))
        .collect();
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(rows[1], buf);
    Paragraph::new(Line::from(Span::styled(
        " Enter: approve | R: reject | Esc: close ",
        Theme::dim(),
    )))
    .render(rows[2], buf);
    None
}
