use crate::app::App;
use crate::function::SidebarTab;
use crate::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, List, ListItem, Paragraph, Tabs, Wrap,
};
use ratatui::widgets::Widget;

pub fn render(area: Rect, buf: &mut Buffer, app: &mut App) {
    if area.width < 4 || area.height < 4 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
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
                SidebarTab::Notifications => { render_notifications(inner, buf, app); None }
                SidebarTab::Completion(s) => { render_completion(inner, buf, s); None }
                SidebarTab::Settings(s) => { render_settings(inner, buf, cfg, s); None }
                SidebarTab::ModelPicker(s) => render_picker(inner, buf, s),
                SidebarTab::ProviderPicker(s) => render_provider_picker(inner, buf, s),
                SidebarTab::ThinkingPicker(s) => render_thinking_picker(inner, buf, s),
                SidebarTab::TimelinePicker(s) => render_timeline_picker(inner, buf, s),
                SidebarTab::Hotkey => { render_hotkey(inner, buf); None }
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
            SidebarTab::Notifications => { render_notifications(rows[1], buf, app); None }
            SidebarTab::Completion(s) => { render_completion(rows[1], buf, s); None }
            SidebarTab::Settings(s) => { render_settings(rows[1], buf, cfg, s); None }
            SidebarTab::ModelPicker(s) => render_picker(rows[1], buf, s),
            SidebarTab::ProviderPicker(s) => render_provider_picker(rows[1], buf, s),
            SidebarTab::ThinkingPicker(s) => render_thinking_picker(rows[1], buf, s),
            SidebarTab::TimelinePicker(s) => render_timeline_picker(rows[1], buf, s),
            SidebarTab::Hotkey => { render_hotkey(rows[1], buf); None }
        };
        app.function_panel_cursor = cursor;
    }
}

fn render_notifications(area: Rect, buf: &mut Buffer, app: &App) {
    let items: Vec<ListItem> = app
        .notifications
        .items
        .iter()
        .rev()
        .map(|t| {
            let level_style = match t.level {
                crate::function::notifications::ToastLevel::Ok => Theme::status_ok(),
                crate::function::notifications::ToastLevel::Info => Theme::status_info(),
                crate::function::notifications::ToastLevel::Warn => Theme::status_warn(),
                crate::function::notifications::ToastLevel::Fail => Theme::status_fail(),
            };
            let tag = format!("[{}]", t.level.tag());
            let line = Line::from(vec![
                Span::styled(tag, level_style),
                Span::raw(" "),
                Span::styled(t.format_time(), Theme::dim()),
                Span::raw("  "),
                Span::raw(t.text.clone()),
            ]);
            ListItem::new(line)
        })
        .collect();

    List::new(items).render(area, buf);
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

fn render_settings(area: Rect, buf: &mut Buffer, cfg: &crate::config::Config, s: &crate::function::SettingsState) {
    use crate::function::SettingsLevel;
    if area.height < 3 {
        return;
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
            body_lines.push(list_item(2 == s.cursor, "enter behavior", Some(cfg.enter_behavior.as_str().to_string())));
        }
        SettingsLevel::ProviderList => {
            body_lines.push(list_item(0 == s.cursor, "+ new provider", None));
            let mut keys: Vec<String> = cfg.entries.keys().cloned().collect();
            keys.sort();
            for (i, id) in keys.iter().enumerate() {
                let is_active = cfg.active.as_deref() == Some(id.as_str());
                let mut label = crate::config::id_display(id);
                if let Some(entry) = cfg.entry(id) {
                    if !entry.model.is_empty() {
                        label.push_str(&format!(", {}", entry.model));
                    }
                }
                if is_active {
                    label.push_str("  [active]");
                }
                body_lines.push(list_item(s.cursor == i + 1, &label, None));
            }
        }
        SettingsLevel::NewProviderKind => {
            let ids = crate::config::Config::all_possible_ids();
            for (i, id) in ids.iter().enumerate() {
                let exists = cfg.entries.contains_key(id);
                let mut label = crate::config::id_display(id);
                if exists {
                    label.push_str("  [exists]");
                }
                body_lines.push(list_item(s.cursor == i, &label, None));
            }
        }
        SettingsLevel::ExistingActions(id) => {
            body_lines.push(list_item(s.cursor == 0, "edit", None));
            body_lines.push(list_item(s.cursor == 1, "delete", None));
            let _ = id;
        }
        SettingsLevel::ThinkingDisplayList => {
            use crate::config::ThinkingDisplay;
            let modes = [ThinkingDisplay::Show, ThinkingDisplay::Hide, ThinkingDisplay::ShowWhileStreaming];
            for (i, mode) in modes.iter().enumerate() {
                let is_current = *mode == cfg.thinking_display;
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
                            Some(crate::config::parse_id(&form.id)
                                .map(|(k, _)| k.as_str().to_string())
                                .unwrap_or_default())
                        } else {
                            Some(form.name.clone())
                        }
                    }
                    ConfigField::BaseUrl => Some(form.base_url.clone()),
                    ConfigField::KeyOrEnv => {
                        if key_is_secret && !form.key_modified && !form.key_or_env.is_empty() {
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
    Paragraph::new(Line::from(Span::styled(
        s.level.hint(),
        Theme::dim(),
    )))
    .render(rows[2], buf);
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
        let p = Paragraph::new(Line::from(Span::styled("[loading...]", Theme::status_warn())))
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
        ("Mouse wheel", "Scroll session"),
    ];
    let lines: Vec<Line> = rows
        .into_iter()
        .map(|(k, v)| Line::from(vec![Span::styled(format!("{k:<14}"), Theme::bold()), Span::raw("  "), Span::raw(v)]))
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
    let search_cursor =
        crate::ui::picker_widget::render_search_row(
            rows[0], buf, &s.query, crate::function::PickerFocus::Search,
        );

    // List
    use crate::function::ThinkingPickerState as TPS;
    let mut list_lines: Vec<Line> = Vec::new();
    if s.filtered.is_empty() {
        list_lines.push(Line::from(Span::styled(
            "  [no matches]",
            Theme::dim(),
        )));
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
        Paragraph::new(Line::from(Span::styled("[no messages in session]", Theme::dim())))
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
