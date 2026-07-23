use super::mcp::start_cursor_oauth;
use super::paste::handle_paste_preview_key;
use super::AppMsg;
use crate::app::App;
use crate::function::notifications::ToastLevel;
use crossterm::event::KeyModifiers;
/// Dispatch a key event to the per-tab handler for the currently active
/// sidebar tab. The dispatch follows a "move out → call handler → decide
/// what to do with the original" pattern:
///
/// 1. The active tab is moved out and replaced with a `Notifications`
///    placeholder. This lets the per-tab handler freely mutate
///    `app.function.tabs` (e.g. close the active tab, push a new one)
///    without any borrow conflict on the moved-out tab.
/// 2. The handler runs.
/// 3. The original tab is restored to the `active` slot IFF the handler
///    did not touch it. We detect "handler touched the slot" by checking
///    whether the placeholder is still there. If the placeholder is
///    gone, the handler must have replaced/removed it — we drop the
///    moved-out copy so we don't overwrite the handler's work (this
///    was the bug that resurrected the provider picker after the user
///    pressed Enter).
///
/// Returns `true` if the handler consumed the key.
pub(super) async fn dispatch_to_active_tab(k: crossterm::event::KeyEvent, app: &mut App) -> bool {
    let active = app.function.active;
    if active >= app.function.tabs.len() {
        return false;
    }
    let mut tab = std::mem::replace(
        &mut app.function.tabs[active],
        crate::function::SidebarTab::Notifications,
    );
    let consumed = match &mut tab {
        crate::function::SidebarTab::Notifications => handle_notifications_key(k, app),
        crate::function::SidebarTab::ModelPicker(state) => handle_picker_key(k, app, state),
        crate::function::SidebarTab::ProviderPicker(state) => {
            handle_provider_picker_key(k, app, state)
        }
        crate::function::SidebarTab::Settings(state) => handle_settings_key(k, app, state),
        crate::function::SidebarTab::ThinkingPicker(state) => handle_thinking_key(k, app, state),
        crate::function::SidebarTab::TimelinePicker(state) => handle_timeline_key(k, app, state),
        crate::function::SidebarTab::SessionPicker(state) => {
            handle_session_picker_key(k, app, state)
        }
        crate::function::SidebarTab::SessionRename(state) => {
            handle_session_rename_key(k, app, state)
        }
        crate::function::SidebarTab::PastePreview(state) => handle_paste_preview_key(k, app, state),
        crate::function::SidebarTab::Plan(state) => handle_plan_key(k, app, state).await,
        crate::function::SidebarTab::Ask(state) => handle_ask_key(k, app, state).await,
        crate::function::SidebarTab::Todo(state) => handle_todo_key(k, app, state).await,
        crate::function::SidebarTab::ToolPicker(state) => handle_tool_picker_key(k, app, state),
        crate::function::SidebarTab::CommandPalette(state) => {
            handle_command_palette_key(k, app, state)
        }
        _ => false,
    };
    if active < app.function.tabs.len()
        && matches!(
            app.function.tabs[active],
            crate::function::SidebarTab::Notifications
        )
    {
        // If the Plan tab was approved or rejected, close it instead
        // of restoring it. The handler already set state.approved
        // and switched the app mode.
        if matches!(&tab, crate::function::SidebarTab::Plan(state) if state.approved.is_some()) {
            close_active_function_tab(app);
        } else if matches!(&tab, crate::function::SidebarTab::SessionPicker(state) if state.consumed)
        {
            // The handler already closed the tab and resumed the session.
            // Don't restore it.
            close_active_function_tab(app);
        } else {
            app.function.tabs[active] = tab;
        }
    }
    consumed
}

/// Handle keys for the CommandPalette tab (Ctrl+P).
pub(super) fn handle_command_palette_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::CommandPaletteState,
) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    match k.code {
        KeyCode::Enter => {
            if state.entries.is_empty() || state.cursor >= state.entries.len() {
                return true;
            }
            let entry = &state.entries[state.cursor].clone();
            match entry {
                crate::function::PaletteEntry::Command { name, .. } => {
                    // Close the palette tab first, then execute the command.
                    close_active_function_tab(app);
                    match *name {
                        "model" => crate::commands::open_model_picker(app),
                        "settings" => crate::commands::open_settings(app),
                        "session" => crate::commands::open_session_picker(
                            app,
                            crate::function::SessionPickerMode::Manage,
                        ),
                        "timeline" => crate::commands::open_timeline_picker(app),
                        "think" => crate::commands::open_thinking_picker(app),
                        "tool" => crate::commands::open_tool_picker(app),
                        "hotkey" => crate::commands::open_hotkey(app),
                        "retry" => crate::commands::retry_last_prompt(app),
                        "continue" => crate::commands::continue_response(app, ""),
                        "compact" => crate::commands::compact_now(app, ""),
                        "new" => {
                            app.start_new_session();
                            app.notify(ToastLevel::Info, "new session");
                        }
                        "plan" => {
                            app.set_mode(crate::function::AppMode::Plan);
                            app.notify(ToastLevel::Info, "mode: plan");
                        }
                        "yolo" => {
                            app.set_mode(crate::function::AppMode::Yolo);
                            app.notify(ToastLevel::Info, "mode: yolo");
                        }
                        "clear" => {
                            app.start_new_session();
                            app.notify(ToastLevel::Info, "session cleared");
                        }
                        _ => {}
                    }
                    true
                }
                crate::function::PaletteEntry::Skill { .. } => {
                    // Insert all selected skills (or just the focused one) as markers.
                    let selected: Vec<String> = state
                        .entries
                        .iter()
                        .filter_map(|e| match e {
                            crate::function::PaletteEntry::Skill {
                                name,
                                selected: true,
                                ..
                            } => Some(name.clone()),
                            _ => None,
                        })
                        .collect();
                    let names = if selected.is_empty() {
                        // Nothing selected → insert just the focused skill.
                        vec![state.entries[state.cursor]
                            .clone()
                            .name()
                            .unwrap_or_default()
                            .to_string()]
                    } else {
                        selected
                    };
                    if !names.is_empty() {
                        app.push_input_undo();
                        for name in &names {
                            if app.input.has_selection() {
                                app.input.delete_selection();
                            }
                            app.input.insert_str(&format!("[skill:{}]", name));
                        }
                    }
                    close_active_function_tab(app);
                    true
                }
            }
        }
        KeyCode::Char(' ') if k.modifiers.is_empty() => {
            state.toggle_selected();
            true
        }
        KeyCode::Esc => {
            // Before closing, insert all selected skills as markers.
            let selected: Vec<String> = state
                .entries
                .iter()
                .filter_map(|e| match e {
                    crate::function::PaletteEntry::Skill {
                        name,
                        selected: true,
                        ..
                    } => Some(name.clone()),
                    _ => None,
                })
                .collect();
            if !selected.is_empty() {
                app.push_input_undo();
                for name in &selected {
                    if app.input.has_selection() {
                        app.input.delete_selection();
                    }
                    app.input.insert_str(&format!("[skill:{}]", name));
                }
            }
            close_active_function_tab(app);
            true
        }
        KeyCode::Up => {
            state.move_up();
            true
        }
        KeyCode::Down => {
            state.move_down();
            true
        }
        KeyCode::Backspace => {
            state.query.pop();
            state.rebuild_filter();
            true
        }
        KeyCode::Char(c)
            if !k
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.query.push(c);
            state.rebuild_filter();
            true
        }
        KeyCode::Tab | KeyCode::BackTab => {
            // Tab/BackTab are handled by the panel-level dispatch,
            // not consumed here.
            false
        }
        _ => false,
    }
}

pub(super) fn close_active_function_tab(app: &mut App) {
    let active = app.function.active;
    if active < app.function.tabs.len() {
        app.function.tabs.remove(active);
        if app.function.active >= app.function.tabs.len() {
            app.function.active = app.function.tabs.len().saturating_sub(1);
        }
    }
    app.maybe_hide_panel();
}
pub(super) fn handle_notifications_key(k: crossterm::event::KeyEvent, app: &mut App) -> bool {
    use crossterm::event::KeyCode;
    match k.code {
        KeyCode::Up => {
            app.notifications.move_up();
            true
        }
        KeyCode::Down => {
            app.notifications.move_down();
            true
        }
        KeyCode::Backspace => {
            if app.notifications.searching {
                app.notifications.backspace_query()
            } else {
                false
            }
        }
        KeyCode::Esc => {
            if app.notifications.searching {
                app.notifications.exit_search_mode();
                true
            } else if !app.notifications.query.is_empty() {
                app.notifications.query.clear();
                app.notifications.cursor = 0;
                app.notifications.scroll = 0;
                true
            } else {
                close_active_function_tab(app);
                true
            }
        }
        KeyCode::Char('i') | KeyCode::Char('I') if k.modifiers.contains(KeyModifiers::ALT) => {
            if !app.notifications.searching {
                app.notifications.enter_search_mode();
                true
            } else {
                false
            }
        }
        KeyCode::Char(c) => {
            if app.notifications.searching {
                app.notifications.insert_query_char(c);
                true
            } else {
                false
            }
        }
        _ => false,
    }
}
pub(super) async fn handle_plan_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::PlanState,
) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    match k.code {
        KeyCode::Enter => {
            // If the Enter variant the user invoked should insert a
            // newline (e.g. Shift+Enter under "Enter sends"), do that
            // in the input buffer instead of approving the plan.
            let modified = k.modifiers.intersects(
                KeyModifiers::SHIFT
                    | KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::META,
            );
            if matches!(
                super::enter_action(app.config.enter_behavior, modified),
                super::EnterAction::Newline
            ) {
                app.input.insert_newline();
                app.sync_completion();
                return true;
            }
            state.approved = Some(true);
            let mut prompt = format!(
                "Plan approved. Please proceed with the following plan:\n\n{}",
                state.content
            );
            // If the user typed something into the input buffer before
            // approving, append it as additional args/instructions.
            let extra = app.input.buffer.trim().to_string();
            if !extra.is_empty() {
                prompt.push_str("\n\nAdditional args from user:\n");
                prompt.push_str(&extra);
                app.input.buffer.clear();
                app.input.cursor = 0;
            }
            // send_chat -> send_message pushes the user message into
            // the session; do NOT push it here too, otherwise the
            // message appears twice in the session.
            // close_active_function_tab is intentionally NOT called
            // here: dispatch_to_active_tab swapped the Plan state out
            // of tabs and will close it after we return.
            app.set_mode(crate::function::AppMode::Yolo);
            app.notify(
                crate::function::notifications::ToastLevel::Ok,
                "plan approved",
            );
            crate::commands::send_chat(app, prompt, Vec::new());
            true
        }
        KeyCode::Char('r') | KeyCode::Char('R') if k.modifiers.contains(KeyModifiers::ALT) => {
            state.approved = Some(false);
            let prompt = "Plan rejected. Please revise or ask a follow-up question.".to_string();
            app.set_mode(crate::function::AppMode::Yolo);
            app.notify(
                crate::function::notifications::ToastLevel::Warn,
                "plan rejected",
            );
            crate::commands::send_chat(app, prompt, Vec::new());
            true
        }
        KeyCode::Char('s') | KeyCode::Char('S') if k.modifiers.contains(KeyModifiers::ALT) => {
            if state.dirty {
                app.save_active_plan();
            } else {
                app.notify(
                    crate::function::notifications::ToastLevel::Info,
                    "plan already saved",
                );
            }
            true
        }
        KeyCode::Esc => {
            close_active_function_tab(app);
            true
        }

        _ => false,
    }
}
pub(super) async fn handle_todo_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::TodoTabState,
) -> bool {
    use crate::session::TodoItem;
    use crossterm::event::KeyCode;
    let total = app.session.todo_items.len();

    // If editing, handle Enter (confirm) and Esc (cancel)
    if let Some(edit_idx) = state.editing {
        match k.code {
            KeyCode::Enter => {
                let text = app.input.buffer.trim().to_string();
                if edit_idx < app.session.todo_items.len() {
                    app.session.todo_items[edit_idx].content = text;
                }
                state.editing = None;
                app.session.invalidate_layout_cache();
                return true;
            }
            KeyCode::Esc => {
                // Remove the item if content is still empty
                if edit_idx < app.session.todo_items.len()
                    && app.session.todo_items[edit_idx].content.trim().is_empty()
                {
                    app.session.todo_items.remove(edit_idx);
                    if state.cursor > 0 && state.cursor >= app.session.todo_items.len() {
                        state.cursor = state.cursor.saturating_sub(1);
                    }
                }
                state.editing = None;
                app.session.invalidate_layout_cache();
                return true;
            }
            _ => {}
        }
        return true;
    }

    match k.code {
        KeyCode::Up => {
            if state.cursor > 0 {
                state.cursor -= 1;
            }
            true
        }
        KeyCode::Down => {
            if total > 0 {
                state.cursor = (state.cursor + 1).min(total.saturating_sub(1));
            }
            true
        }
        KeyCode::Enter => {
            if total == 0 {
                return true;
            }
            let item = &mut app.session.todo_items[state.cursor];
            item.status = match item.status.as_str() {
                "pending" => "in_progress".to_string(),
                "in_progress" => "completed".to_string(),
                _ => "pending".to_string(),
            };
            app.session.invalidate_layout_cache();
            true
        }
        KeyCode::Delete => {
            if total == 0 {
                return true;
            }
            app.session.todo_items.remove(state.cursor);
            if state.cursor > 0 && state.cursor >= app.session.todo_items.len() {
                state.cursor = state.cursor.saturating_sub(1);
            }
            app.session.invalidate_layout_cache();
            true
        }
        _ => {
            if k.modifiers.contains(crossterm::event::KeyModifiers::ALT) {
                match k.code {
                    KeyCode::Char('i') => {
                        let text = app.input.buffer.trim().to_string();
                        let insert_at = (state.cursor + 1).min(total);
                        app.session.todo_items.insert(
                            insert_at,
                            TodoItem {
                                content: text,
                                status: "pending".to_string(),
                            },
                        );
                        state.cursor = insert_at;
                        state.editing = Some(insert_at);
                        app.session.invalidate_layout_cache();
                        true
                    }
                    KeyCode::Char('I') => {
                        let text = app.input.buffer.trim().to_string();
                        let insert_at = state.cursor.min(total);
                        app.session.todo_items.insert(
                            insert_at,
                            TodoItem {
                                content: text,
                                status: "pending".to_string(),
                            },
                        );
                        state.cursor = insert_at;
                        state.editing = Some(insert_at);
                        app.session.invalidate_layout_cache();
                        true
                    }
                    KeyCode::Char('e') | KeyCode::Char('E') => {
                        if total == 0 {
                            return true;
                        }
                        // Copy current todo content to input buffer for editing
                        let content = app.session.todo_items[state.cursor].content.clone();
                        app.input.buffer = content;
                        app.input.cursor = app.input.buffer.len();
                        state.editing = Some(state.cursor);
                        app.session.invalidate_layout_cache();
                        true
                    }
                    KeyCode::Char('c') | KeyCode::Char('C') => {
                        app.session.todo_items.clear();
                        state.cursor = 0;
                        app.session.invalidate_layout_cache();
                        true
                    }
                    _ => false,
                }
            } else {
                false
            }
        }
    }
}
pub(super) async fn handle_ask_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::AskState,
) -> bool {
    use crate::function::AskPhase;
    use crossterm::event::KeyCode;

    let total_rows = match state.phase {
        AskPhase::Asking => state.row_count(),
        AskPhase::Reviewing => 0,
    };

    match k.code {
        KeyCode::Up => {
            if state.phase == AskPhase::Reviewing {
                if state.active > 0 {
                    state.active -= 1;
                }
                return true;
            }
            if let Some(it) = state.items.get_mut(state.active) {
                if it.cursor == 0 {
                    it.cursor = total_rows.saturating_sub(1);
                } else {
                    it.cursor -= 1;
                }
            }
            true
        }
        KeyCode::Down => {
            if state.phase == AskPhase::Reviewing {
                if state.active + 1 < state.items.len() {
                    state.active += 1;
                }
                return true;
            }
            if let Some(it) = state.items.get_mut(state.active) {
                it.cursor = (it.cursor + 1) % total_rows;
            }
            true
        }
        KeyCode::Left => {
            if state.phase == AskPhase::Reviewing {
                return true;
            }
            if state.active > 0 {
                state.active -= 1;
            }
            true
        }
        KeyCode::Right => {
            if state.phase == AskPhase::Reviewing {
                return true;
            }
            if state.active + 1 < state.items.len() {
                state.active += 1;
            } else if state.all_answered() {
                // Past the last question and everything is answered:
                // jump to the review step.
                state.phase = AskPhase::Reviewing;
            }
            true
        }
        KeyCode::Enter => {
            if state.phase == AskPhase::Reviewing {
                // Whole batch approved. Send a single summary turn
                // and close the tab.
                let summary = state.build_summary();
                close_active_function_tab(app);
                crate::commands::send_chat(app, summary, Vec::new());
                return true;
            }

            // Asking phase: dispatch on the cursor row.
            let q_idx = state.active;
            let cursor = state.items[q_idx].cursor;
            let is_freeform = cursor >= state.items[q_idx].options.len();

            if is_freeform {
                let custom = state.items[q_idx].custom_input.clone();
                if custom.trim().is_empty() {
                    // No text typed yet — tell the LLM to wait.
                    let question = state.items[q_idx].question.clone();
                    let prompt = format!(
                        "(Question: {question})\nPlease wait — the user is typing a free-form answer."
                    );
                    crate::commands::send_chat(app, prompt, Vec::new());
                    return true;
                }
                // Use the typed custom input as the answer.
                state.items[q_idx].answered = Some(custom);
                state.items[q_idx].custom_input.clear();
                if state.all_answered() {
                    state.phase = AskPhase::Reviewing;
                } else if let Some(next) = state.next_unanswered(q_idx + 1) {
                    state.active = next;
                    state.items[next].cursor = 0;
                }
                return true;
            }

            // Picked a model-supplied option. Write the answer,
            // advance to the next unanswered question, or flip to
            // the review step if everything is answered.
            let answer = state.items[q_idx].options[cursor].clone();
            state.items[q_idx].answered = Some(answer);
            if state.all_answered() {
                state.phase = AskPhase::Reviewing;
            } else if let Some(next) = state.next_unanswered(q_idx + 1) {
                state.active = next;
                state.items[next].cursor = 0;
            }
            true
        }
        KeyCode::Backspace => {
            if state.phase == AskPhase::Asking {
                if let Some(it) = state.items.get_mut(state.active) {
                    if it.cursor >= it.options.len() && !it.custom_input.is_empty() {
                        it.custom_input.pop();
                        return true;
                    }
                }
            }
            false
        }
        KeyCode::Char(c) => {
            if state.phase == AskPhase::Asking {
                if let Some(it) = state.items.get_mut(state.active) {
                    if it.cursor >= it.options.len() {
                        it.custom_input.push(c);
                        return true;
                    }
                }
            }
            false
        }
        KeyCode::Esc => {
            if state.phase == AskPhase::Reviewing {
                // Esc in Reviewing goes back to Asking so the user
                // can fix an answer.
                state.phase = AskPhase::Asking;
                if let Some(idx) = state.next_unanswered(0) {
                    state.active = idx;
                }
                return true;
            }
            // Esc in Asking dismisses the entire ask round.
            let summary = state.build_dismiss_summary();
            close_active_function_tab(app);
            crate::commands::send_chat(app, summary, Vec::new());
            true
        }
        _ => false,
    }
}
pub(super) fn handle_session_picker_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::SessionPickerState,
) -> bool {
    use crossterm::event::KeyCode;
    match k.code {
        KeyCode::Tab => {
            state.toggle_scope(&app.cwd);
            true
        }
        KeyCode::Esc => {
            close_active_function_tab(app);
            app.set_mode(crate::function::AppMode::Yolo);
            true
        }
        KeyCode::Up => {
            if state.cursor > 0 {
                state.cursor -= 1;
            }
            true
        }
        KeyCode::Down => {
            if state.cursor + 1 < state.filtered.len() {
                state.cursor += 1;
            }
            true
        }
        KeyCode::Backspace => {
            state.query.pop();
            state.rebuild_filter();
            true
        }
        KeyCode::Enter => {
            if let Some(id) = state.selected_id() {
                state.consumed = true;
                close_active_function_tab(app);
                app.resume_session(&id);
            }
            true
        }
        KeyCode::Char(c) => {
            match c {
                'r' | 'R' if state.mode == crate::function::SessionPickerMode::Manage => {
                    if let (Some(id), Some(title)) = (state.selected_id(), state.selected_title()) {
                        crate::commands::open_session_rename(app, Some(id), title);
                    }
                }
                'd' | 'D' if state.mode == crate::function::SessionPickerMode::Manage => {
                    if let Some(id) = state.selected_id() {
                        match crate::session::store::delete(&id) {
                            Ok(()) => {
                                app.notify(
                                    crate::function::notifications::ToastLevel::Ok,
                                    "session deleted",
                                );
                                state.reload(&app.cwd);
                            }
                            Err(e) => app.notify(
                                crate::function::notifications::ToastLevel::Fail,
                                format!("delete session: {e}"),
                            ),
                        }
                    }
                }
                'f' | 'F' if state.mode == crate::function::SessionPickerMode::Manage => {
                    if let Some(id) = state.selected_id() {
                        app.fork_session(Some(id));
                        state.reload(&app.cwd);
                    }
                }
                _ => {
                    state.query.push(c);
                    state.rebuild_filter();
                }
            }
            true
        }
        _ => false,
    }
}
pub(super) fn handle_session_rename_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::SessionRenameState,
) -> bool {
    use crossterm::event::KeyCode;
    match k.code {
        KeyCode::Esc => {
            close_active_function_tab(app);
            app.set_mode(crate::function::AppMode::Yolo);
            true
        }
        KeyCode::Enter => {
            app.rename_session(state.target_id.clone(), state.title.clone());
            close_active_function_tab(app);
            true
        }
        KeyCode::Left => {
            if let Some(idx) = state.title[..state.cursor].char_indices().last() {
                state.cursor = idx.0;
            } else {
                state.cursor = 0;
            }
            true
        }
        KeyCode::Right => {
            if state.cursor < state.title.len() {
                state.cursor = state.title[state.cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| state.cursor + i)
                    .unwrap_or(state.title.len());
            }
            true
        }
        KeyCode::Backspace => {
            if state.cursor > 0 {
                if let Some(idx) = state.title[..state.cursor].char_indices().last() {
                    let start = idx.0;
                    state.title.replace_range(start..state.cursor, "");
                    state.cursor = start;
                }
            }
            true
        }
        KeyCode::Delete => {
            if state.cursor < state.title.len() {
                let end = state.title[state.cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| state.cursor + i)
                    .unwrap_or(state.title.len());
                state.title.replace_range(state.cursor..end, "");
            }
            true
        }
        KeyCode::Char(c) => {
            state.title.insert(state.cursor, c);
            state.cursor += c.len_utf8();
            true
        }
        _ => false,
    }
}
/// First step of the `/model` flow: pick a provider. On Enter, a
/// `ModelPicker` tab for the selected entry's kind is PUSHED on top of
/// the ProviderPicker — the ProviderPicker stays behind so the user
/// can press Esc on the ModelPicker to return to provider selection
/// (matches the Settings-level back-stack pattern). On Esc (with empty
/// query) the whole flow closes.
/// Mirrors the model picker's search-row + list pattern so the user
/// can type to filter, then Up/Down + Enter to confirm.
pub(super) fn handle_provider_picker_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::ProviderPickerState,
) -> bool {
    use crossterm::event::KeyCode;
    let open_model_picker_for_selected =
        |app: &mut App, state: &crate::function::ProviderPickerState| {
            if let Some(id) = state.selected_id() {
                // Push the model picker for the chosen entry. Bind it to
                // the exact entry id (not just its kind) so fetches and
                // commits target the right endpoint even when several
                // configured entries share a kind. Do NOT remove the
                // ProviderPicker — keeping it in the tab stack means
                // the user can Esc back to provider selection.
                crate::commands::open_model_picker_for_entry(app, &id);
            }
        };
    match state.focus {
        crate::function::PickerFocus::Search => {
            use crate::function::states::FilterablePicker;
            match state.handle_search_key(k) {
                Some(consumed) => consumed,
                None => match k.code {
                    KeyCode::Enter => {
                        open_model_picker_for_selected(app, state);
                        true
                    }
                    _ => false,
                },
            }
        }
        crate::function::PickerFocus::List => {
            use crate::function::states::FilterablePicker;
            match state.handle_list_key(k) {
                Some(consumed) => consumed,
                None => match k.code {
                    KeyCode::Enter => {
                        open_model_picker_for_selected(app, state);
                        true
                    }
                    _ => false,
                },
            }
        }
    }
}
pub(super) fn handle_picker_key(
    k: crossterm::event::KeyEvent,
    _app: &mut App,
    state: &mut crate::function::ModelPickerState,
) -> bool {
    // If context picker is active, handle its keys first.
    if state.context_pick.is_some() {
        return handle_context_picker_key(k, _app, state);
    }
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

    // Global shortcuts (work in any focus mode).
    if ctrl {
        match k.code {
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if !state.fetching {
                    trigger_picker_fetch(_app, state);
                }
                return true;
            }
            KeyCode::Char('m') | KeyCode::Char('M') => {
                // Switch to search input so the user can type a model id.
                state.focus = crate::function::PickerFocus::Search;
                return true;
            }
            _ => {}
        }
    }

    if state.fetching {
        if matches!(k.code, KeyCode::Esc) {
            state.fetching = false;
            state.fetch_error = Some("[cancelled]".to_string());
            return true;
        }
        return false;
    }

    match state.focus {
        crate::function::PickerFocus::Search => {
            use crate::function::states::FilterablePicker;
            match state.handle_search_key(k) {
                Some(consumed) => consumed,
                None => match k.code {
                    KeyCode::Enter => {
                        if let Some(&idx) = state.filtered.get(state.cursor) {
                            let model = &state.models[idx];
                            if model.context_needs_pick && model.context_window_tokens.is_none() {
                                open_context_picker(_app, state, idx);
                            } else {
                                let id = model.id.clone();
                                commit_model_with_entry(
                                    _app,
                                    state.provider,
                                    state.entry_id.as_deref(),
                                    id,
                                    false,
                                );
                            }
                        } else {
                            let id = state.query.trim();
                            if !id.is_empty() {
                                commit_model_with_entry(
                                    _app,
                                    state.provider,
                                    state.entry_id.as_deref(),
                                    id.to_string(),
                                    true,
                                );
                            }
                        }
                        true
                    }
                    _ => false,
                },
            }
        }
        crate::function::PickerFocus::List => {
            use crate::function::states::FilterablePicker;
            match state.handle_list_key(k) {
                Some(consumed) => consumed,
                None => match k.code {
                    KeyCode::Enter => {
                        if let Some(&idx) = state.filtered.get(state.cursor) {
                            let model = &state.models[idx];
                            if model.context_needs_pick && model.context_window_tokens.is_none() {
                                open_context_picker(_app, state, idx);
                            } else {
                                let id = model.id.clone();
                                commit_model_with_entry(
                                    _app,
                                    state.provider,
                                    state.entry_id.as_deref(),
                                    id,
                                    false,
                                );
                            }
                        }
                        true
                    }
                    _ => false,
                },
            }
        }
    }
}
/// Search / navigate / select for the thinking-level picker.  Mirrors the
/// model-picker's pattern (search bar + filtered list) even though there
/// are only four possible levels.
pub(super) fn handle_thinking_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::ThinkingPickerState,
) -> bool {
    use crossterm::event::KeyCode;
    match k.code {
        KeyCode::Up => {
            if state.cursor > 0 {
                state.cursor -= 1;
            }
            true
        }
        KeyCode::Down => {
            if state.cursor + 1 < state.filtered.len() {
                state.cursor += 1;
            }
            true
        }
        KeyCode::Enter => {
            if let Some(level) = state.selected() {
                use crate::config::ReasoningMode;
                let next = match level {
                    "off" => ReasoningMode::Off,
                    "minimal" => ReasoningMode::Minimal,
                    "low" => ReasoningMode::Low,
                    "medium" => ReasoningMode::Medium,
                    "high" => ReasoningMode::High,
                    "xhigh" => ReasoningMode::XHigh,
                    "adaptive" => ReasoningMode::Adaptive,
                    "max" => ReasoningMode::Max,
                    _ => unreachable!(),
                };
                app.config.thinking = next;
                app.status.set_thinking(next);
                app.save_config();
            }
            // Close the picker tab.
            close_active_function_tab(app);
            true
        }
        KeyCode::Esc => {
            close_active_function_tab(app);
            true
        }
        KeyCode::Char(c) => {
            state.query.push(c);
            state.rebuild_filter();
            true
        }
        KeyCode::Backspace => {
            state.query.pop();
            state.rebuild_filter();
            true
        }
        _ => true,
    }
}
/// Search / navigate / jump-to-message for the timeline picker.
/// Mirrors the model picker's search-row + list pattern.
pub(super) fn handle_timeline_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::TimelinePickerState,
) -> bool {
    use crate::function::states::FilterablePicker;
    match state.focus {
        crate::function::PickerFocus::Search => match state.handle_search_key(k) {
            Some(consumed) => consumed,
            None => match k.code {
                crossterm::event::KeyCode::Enter => {
                    commit_timeline_jump(app, state);
                    true
                }
                _ => false,
            },
        },
        crate::function::PickerFocus::List => match state.handle_list_key(k) {
            Some(consumed) => consumed,
            None => match k.code {
                crossterm::event::KeyCode::Enter => {
                    commit_timeline_jump(app, state);
                    true
                }
                _ => false,
            },
        },
    }
}
/// Jump the session scroll to the focused entry and close the
/// timeline picker tab.
pub(super) fn commit_timeline_jump(app: &mut App, state: &crate::function::TimelinePickerState) {
    use crate::function::notifications::ToastLevel;
    let Some((msg_idx, tool_idx)) = state.selected_entry() else {
        return;
    };
    let viewport_h = app.session_area.map(|r| r.height).unwrap_or(20);
    let viewport_w = app.session_area.map(|r| r.width).unwrap_or(120);
    app.session
        .jump_to_message(msg_idx, tool_idx, viewport_h, viewport_w);
    close_active_function_tab(app);
    let label = if tool_idx.is_some() {
        "jumped to tool call"
    } else {
        &format!("jumped to message #{}", msg_idx + 1)
    };
    app.notify(ToastLevel::Info, label);
}
pub(super) fn trigger_picker_fetch(app: &mut App, state: &mut crate::function::ModelPickerState) {
    use crate::config::parse_id;
    let p = state.provider;
    // Resolve the entry to fetch from. Prefer the picker's bound entry id
    // (multiple entries can share a kind — e.g. two OpenAI endpoints —
    // so the kind alone is not enough). Fall back to kind-based
    // resolution only when no entry id is bound (legacy paths) — same
    // precedence as `commit_model`.
    let target_id: Option<String> = match state.entry_id.as_deref() {
        Some(id) if app.config.entry(id).is_some() => Some(id.to_string()),
        _ => match app.config.active.as_deref() {
            Some(id) if parse_id(id).map(|(k, _)| k == p).unwrap_or(false) => Some(id.to_string()),
            _ => app
                .config
                .entries
                .keys()
                .find(|id| parse_id(id).map(|(k2, _)| k2 == p).unwrap_or(false))
                .cloned(),
        },
    };
    let target_id = match target_id {
        Some(id) => id,
        None => {
            app.notify(
                ToastLevel::Fail,
                "no provider configured for this kind; add one in /settings",
            );
            return;
        }
    };
    if let Err(e) = app.config.validate_provider(&target_id) {
        app.notify(ToastLevel::Fail, e);
        return;
    }
    state.fetching = true;
    state.fetch_error = None;
    state.no_endpoint = false;
    state.models.clear();
    state.filtered.clear();
    state.cursor = 0;
    if let Some(tx) = app.msg_tx.clone() {
        let base = app
            .config
            .entry(&target_id)
            .map(|c| c.base_url.clone())
            .unwrap_or_default();
        let key = app.config.effective_api_key(&target_id).unwrap_or_default();
        let access_key = app
            .config
            .entry(&target_id)
            .map(|c| c.access_key.clone())
            .unwrap_or_default();
        let secret_key = app
            .config
            .entry(&target_id)
            .map(|c| c.secret_key.clone())
            .unwrap_or_default();
        let client = app.reqwest.clone();
        let provider_name = app
            .config
            .entry(&target_id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let provider_id = app
            .config
            .entry(&target_id)
            .map(|c| c.provider_id.clone())
            .unwrap_or_default();
        let cache_path = app
            .model_cache_path
            .parent()
            .unwrap_or(&app.model_cache_path)
            .to_path_buf();
        tokio::spawn(async move {
            match crate::providers::list_models(crate::providers::ListModelsArgs {
                client: &client,
                kind: p,
                base_url: &base,
                api_key: &key,
                access_key: &access_key,
                secret_key: &secret_key,
                cache_path: &cache_path,
                provider_name: &provider_name,
                provider_id: &provider_id,
            })
            .await
            {
                Ok(models) => {
                    let _ = tx.send(AppMsg::ModelsFetched {
                        provider: p,
                        base_url: base,
                        api_key: key,
                        models,
                    });
                }
                Err(e) => {
                    let no_endpoint = matches!(
                        e.downcast_ref::<crate::providers::ProviderError>(),
                        Some(crate::providers::ProviderError::NoModelsEndpoint)
                    );
                    let _ = tx.send(AppMsg::ModelsFetchFailed {
                        provider: p,
                        error: format!("{e}"),
                        no_endpoint,
                    });
                }
            }
        });
    }
}
pub(super) fn handle_settings_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::SettingsState,
) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if matches!(state.level, crate::function::SettingsLevel::NewProviderKind) {
        if matches!(k.code, KeyCode::Enter) {
            handle_settings_enter(app, state);
            return true;
        }
        return handle_new_provider_key(k, state);
    }
    // Tool-preview-lines is a single-row stepper: Up/Down adjust the
    // value rather than navigate.
    if matches!(
        state.level,
        crate::function::SettingsLevel::ToolPreviewLines
    ) {
        match k.code {
            KeyCode::Up => {
                if app.config.tool_preview_lines > crate::config::TOOL_PREVIEW_LINES_MIN {
                    app.config.tool_preview_lines -= 1;
                    app.save_config();
                }
                return true;
            }
            KeyCode::Down => {
                if app.config.tool_preview_lines < crate::config::TOOL_PREVIEW_LINES_MAX {
                    app.config.tool_preview_lines += 1;
                    app.save_config();
                }
                return true;
            }
            KeyCode::Esc | KeyCode::Enter => {
                handle_settings_back(app, state);
                return true;
            }
            _ => return true,
        }
    }
    // Navigation keys are level-agnostic.
    match k.code {
        KeyCode::Up => {
            if state.cursor > 0 {
                state.cursor -= 1;
            }
            sync_form_focus_to_cursor(state);
            return true;
        }
        KeyCode::Down => {
            let len = state.list_len(&app.config);
            if state.cursor + 1 < len {
                state.cursor += 1;
            }
            sync_form_focus_to_cursor(state);
            return true;
        }
        KeyCode::Esc => {
            handle_settings_back(app, state);
            return true;
        }
        KeyCode::Enter => {
            handle_settings_enter(app, state);
            return true;
        }
        _ => {}
    }

    // Per-level handlers.
    let level = std::mem::replace(&mut state.level, crate::function::SettingsLevel::TopLevel);
    let mut taken = level;
    let handled = match &mut taken {
        crate::function::SettingsLevel::ConfigForm(form) => handle_form_text(k, ctrl, form),
        _ => false,
    };
    state.level = taken;
    if handled {
        return true;
    }
    false
}
/// In a `ConfigForm` level, keep `form.focused` in sync with `state.cursor`
/// so Up/Down move the actual text-input focus, not just the visual
/// highlight. Otherwise the user navigates with Up/Down but typing still
/// goes to the previously-Tabbed field.
pub(super) fn sync_form_focus_to_cursor(state: &mut crate::function::SettingsState) {
    use crate::function::SettingsLevel;
    if let SettingsLevel::ConfigForm(form) = &mut state.level {
        let fields = form.active_fields();
        form.focused = match state.cursor {
            i if i < fields.len() => fields[i],
            _ => *fields.last().unwrap_or(&crate::function::ConfigField::Exit),
        };
    }
}
/// Esc behavior: pop one level. Only at TopLevel does Esc close the tab.
pub(super) fn handle_settings_back(app: &mut App, state: &mut crate::function::SettingsState) {
    use crate::function::SettingsLevel;
    match &state.level {
        SettingsLevel::ConfigForm(form) => {
            if form.is_new {
                state.level = SettingsLevel::NewProviderKind;
            } else {
                state.level = SettingsLevel::ProviderList;
            }
            state.cursor = 0;
            state.clamp_cursor(&app.config);
        }
        SettingsLevel::NewProviderKind | SettingsLevel::ExistingActions(_) => {
            state.level = SettingsLevel::ProviderList;
            state.cursor = 0;
            state.clamp_cursor(&app.config);
        }
        SettingsLevel::ThinkingDisplayList
        | SettingsLevel::ToolResultDisplayList
        | SettingsLevel::EnterBehaviorList
        | SettingsLevel::BorderTypeList
        | SettingsLevel::ThemeList
        | SettingsLevel::AutoCompact
        | SettingsLevel::ToolPreviewLines => {
            state.level = SettingsLevel::TopLevel;
            state.cursor = 0;
            state.clamp_cursor(&app.config);
        }
        SettingsLevel::ProviderList => {
            state.level = SettingsLevel::TopLevel;
            state.cursor = 0;
        }
        SettingsLevel::TopLevel => {
            // close the settings tab entirely
            close_active_function_tab(app);
        }
    }
}
/// Enter behavior depends on the current level.
///
/// Implementation note: we move `state.level` out with `mem::replace`, work on
/// the owned value, and put it back. This avoids nested `&state.level` /
/// `&mut state.level` patterns that NLL lets through but that are hard to
/// reason about and easy to break with future edits.
///
/// Also: Enter on a text field in the form does **not** auto-advance focus.
/// The user moves between fields with Up/Down/Tab.
pub(super) fn handle_settings_enter(app: &mut App, state: &mut crate::function::SettingsState) {
    use crate::config::parse_id;
    use crate::function::{ConfigField, SettingsLevel};

    let cursor = state.cursor;
    let level = std::mem::replace(&mut state.level, SettingsLevel::TopLevel);

    let new_level = match level {
        SettingsLevel::TopLevel => match cursor {
            0 => SettingsLevel::ProviderList,
            1 => SettingsLevel::ThinkingDisplayList,
            2 => SettingsLevel::ToolResultDisplayList,
            3 => SettingsLevel::EnterBehaviorList,
            4 => SettingsLevel::BorderTypeList,
            5 => SettingsLevel::ThemeList,
            6 => SettingsLevel::AutoCompact,
            _ => SettingsLevel::ToolPreviewLines,
        },
        SettingsLevel::ProviderList => {
            if cursor == 0 {
                // Transitioning to NewProviderKind — reload models.dev
                // providers from cache (background fetch may have
                // completed since SettingsState was created).
                let cache_parent = app
                    .model_cache_path
                    .parent()
                    .unwrap_or(&app.model_cache_path)
                    .to_path_buf();
                state.new_provider.load_model_dev_providers(&cache_parent);
                SettingsLevel::NewProviderKind
            } else {
                let keys = app.config.configured_provider_ids();
                match keys.get(cursor - 1) {
                    Some(id) => SettingsLevel::ExistingActions(id.clone()),
                    None => SettingsLevel::ProviderList,
                }
            }
        }
        SettingsLevel::NewProviderKind => {
            // Reload models.dev providers from cache every time we
            // enter this level (the background fetch may have completed
            // since the SettingsState was created).
            let cache_parent = app
                .model_cache_path
                .parent()
                .unwrap_or(&app.model_cache_path)
                .to_path_buf();
            state.new_provider.load_model_dev_providers(&cache_parent);

            let selected = state.new_provider.selected_id();
            match selected {
                Some(id) if id.starts_with("__md__/") => {
                    // models.dev provider — extract provider_id from
                    // the `__md__/{name}/{provider_id}` format.
                    let provider_id = id.rsplit('/').next().unwrap_or("").to_string();
                    // Load ModelData to get base_url and display name.
                    let cache_path = app
                        .model_cache_path
                        .parent()
                        .unwrap_or(&app.model_cache_path)
                        .to_path_buf();
                    let model_data_path = cache_path.join("model-data.json");
                    let model_data = crate::model_data::ModelData::load(&model_data_path);
                    let provider_meta = model_data
                        .as_ref()
                        .and_then(|d| d.providers.get(&provider_id));
                    let base_url = provider_meta
                        .map(|m| m.base_url.clone())
                        .unwrap_or_default();
                    let name = provider_meta
                        .map(|m| m.name.clone())
                        .unwrap_or_else(|| provider_id.clone());

                    let kind = crate::config::ProviderKind::Openai;
                    let mode = crate::config::ProviderMode::Key;
                    let mut form = crate::function::ConfigFormState::new_for_create(kind, mode);
                    form.name = name;
                    form.base_url = base_url;
                    form.provider_id = provider_id;
                    SettingsLevel::ConfigForm(form)
                }
                Some(id) => match parse_id(&id) {
                    Some((kind, mode)) => SettingsLevel::ConfigForm(
                        crate::function::ConfigFormState::new_for_create(kind, mode),
                    ),
                    None => SettingsLevel::NewProviderKind,
                },
                None => SettingsLevel::NewProviderKind,
            }
        }
        SettingsLevel::ExistingActions(id) => {
            if cursor == 0 {
                // edit
                if let Some((_kind, mode)) = parse_id(&id) {
                    let cfg = app.config.entry(&id).cloned().unwrap_or_default();
                    SettingsLevel::ConfigForm(crate::function::ConfigFormState::new_for_edit(
                        id, &cfg, mode,
                    ))
                } else {
                    SettingsLevel::ProviderList
                }
            } else {
                // delete
                if let Some(cfg) = app.config.entry(&id).cloned() {
                    app.config.entries.remove(&id);
                    if app.config.active.as_deref() == Some(id.as_str()) {
                        app.config.active = app.config.configured_provider_ids().into_iter().next();
                    }
                    if let Err(e) = app.config.save(&app.config_path) {
                        use crate::function::notifications::ToastLevel;
                        app.notify(ToastLevel::Fail, format!("delete: {e}"));
                        // restore
                        app.config.entries.insert(id, cfg);
                    } else {
                        use crate::function::notifications::ToastLevel;
                        app.notify(ToastLevel::Ok, format!("deleted {id}"));
                        // The active provider may have changed or been
                        // removed — refresh the status bar so it does not
                        // show a stale `name:(no model)` from a deleted
                        // entry.
                        app.status.set_provider_name(&app.config.active_name());
                        app.status.set_model(&app.config.active_model_display());
                        app.refresh_status_model_context();
                    }
                }
                SettingsLevel::ProviderList
            }
        }
        SettingsLevel::ThinkingDisplayList => {
            use crate::config::ThinkingDisplay;
            use crate::function::notifications::ToastLevel;
            let modes = [
                ThinkingDisplay::Show,
                ThinkingDisplay::Hide,
                ThinkingDisplay::ShowWhileStreaming,
            ];
            if let Some(&mode) = modes.get(cursor) {
                app.config.thinking_display = mode;
                app.save_config();
                app.notify(
                    ToastLevel::Ok,
                    format!("thinking display: {}", mode.as_str()),
                );
            }
            SettingsLevel::TopLevel
        }
        SettingsLevel::ToolResultDisplayList => {
            use crate::config::ToolResultDisplay;
            use crate::function::notifications::ToastLevel;
            let modes = [
                ToolResultDisplay::Show,
                ToolResultDisplay::Hide,
                ToolResultDisplay::ShowWhileStreaming,
            ];
            if let Some(&mode) = modes.get(cursor) {
                app.config.tool_display = mode;
                app.save_config();
                app.notify(ToastLevel::Ok, format!("tool display: {}", mode.as_str()));
            }
            SettingsLevel::TopLevel
        }
        SettingsLevel::EnterBehaviorList => {
            use crate::config::EnterBehavior;
            use crate::function::notifications::ToastLevel;
            let modes = [EnterBehavior::EnterSends, EnterBehavior::EnterNewline];
            if let Some(&mode) = modes.get(cursor) {
                app.config.enter_behavior = mode;
                app.save_config();
                app.notify(ToastLevel::Ok, format!("enter behavior: {}", mode.as_str()));
            }
            SettingsLevel::TopLevel
        }
        SettingsLevel::BorderTypeList => {
            use crate::function::notifications::ToastLevel;
            use crate::ui::border_type::BorderType;
            let modes = [BorderType::Ascii, BorderType::Rounded];
            if let Some(&mode) = modes.get(cursor) {
                app.config.border_type = mode;
                app.save_config();
                app.notify(ToastLevel::Ok, format!("border type: {}", mode.as_str()));
            }
            SettingsLevel::TopLevel
        }
        SettingsLevel::ThemeList => {
            use crate::function::notifications::ToastLevel;
            use crate::theme::ThemeVariant;
            let themes = ThemeVariant::all();
            if let Some(variant) = themes.get(cursor) {
                app.config.theme = *variant;
                app.save_config();
                crate::theme::init_theme(*variant);
                app.notify(ToastLevel::Ok, format!("theme: {}", variant.as_str()));
                // Clear all render caches so blocks re-render with new colors
                app.session.invalidate_all_render_caches();
            }
            SettingsLevel::TopLevel
        }
        SettingsLevel::AutoCompact => {
            use crate::function::notifications::ToastLevel;
            // 0 = on, 1 = off. `auto_compact` defaults to `true` in
            // `Config`, so picking the first row turns it on, the
            // second row turns it off.
            let enabled = matches!(cursor, 0);
            if app.config.auto_compact != enabled {
                app.config.auto_compact = enabled;
                app.status.set_auto_compact(enabled);
                app.save_config();
                app.notify(
                    ToastLevel::Ok,
                    format!("auto compact: {}", if enabled { "on" } else { "off" }),
                );
            }
            SettingsLevel::TopLevel
        }
        SettingsLevel::ToolPreviewLines => {
            // The single-row stepper is purely Up/Down driven; Enter
            // pops back to the top level without changing the value.
            SettingsLevel::TopLevel
        }
        SettingsLevel::ConfigForm(form) => {
            match form.focused {
                ConfigField::Name
                | ConfigField::BaseUrl
                | ConfigField::Key
                | ConfigField::Env
                | ConfigField::AccessKey
                | ConfigField::SecretKey => {
                    // No auto-advance. User moves fields with Up/Down/Tab.
                    SettingsLevel::ConfigForm(form)
                }
                ConfigField::Save => {
                    if form.base_url.trim().is_empty() {
                        use crate::function::notifications::ToastLevel;
                        let mut f = form;
                        f.form_error = Some("[!] base_url is required".to_string());
                        app.notify(ToastLevel::Fail, "base_url is required");
                        SettingsLevel::ConfigForm(f)
                    } else if !form.is_cursor()
                        && form.api_key.trim().is_empty()
                        && form.api_key_env.trim().is_empty()
                    {
                        use crate::function::notifications::ToastLevel;
                        let mut f = form;
                        f.form_error = Some("[!] api_key or env name is required".to_string());
                        app.notify(ToastLevel::Fail, "api_key or env name is required");
                        SettingsLevel::ConfigForm(f)
                    } else {
                        settings_save_form(app, form);
                        SettingsLevel::ProviderList
                    }
                }
                ConfigField::Exit => SettingsLevel::ProviderList,
            }
        }
    };

    // Restore level and reset cursor to 0 (a new level, no inherited cursor).
    state.level = new_level;
    state.cursor = 0;
    state.clamp_cursor(&app.config);
    let _ = cursor;
}
/// Commit a form into the config. The form's focused field must be Save
/// (caller is responsible). Updates `app.config`, writes to disk, refreshes
/// status, and pushes a toast.
pub(super) fn settings_save_form(app: &mut App, form: crate::function::ConfigFormState) {
    use crate::config::{parse_id, ProviderKind, ProviderMode};
    use crate::function::notifications::ToastLevel;

    let mut id = form.id.clone();
    let (_kind, _mode) = parse_id(&id).unwrap_or((ProviderKind::Openai, ProviderMode::Key));
    let base_url = form.base_url.trim().to_string();
    let was_new = form.is_new;

    // Deduplicate: if the base ID already exists, append -2, -3, etc.
    if was_new {
        let mut n = 2;
        while app.config.entries.contains_key(&id) {
            id = format!("{}-{}", form.id, n);
            n += 1;
        }
    }

    // Preserve existing model and api_key if the user did not touch
    // the corresponding fields.
    let existing = app.config.entry(&id).cloned();
    let model = existing
        .as_ref()
        .map(|c| c.model.clone())
        .unwrap_or_default();
    let model_display = existing
        .as_ref()
        .map(|c| c.model_display.clone())
        .unwrap_or_default();

    let mut new_cfg = crate::config::ProviderConfig {
        api_key: existing
            .as_ref()
            .map(|c| c.api_key.clone())
            .unwrap_or_default(),
        api_key_env: existing
            .as_ref()
            .map(|c| c.api_key_env.clone())
            .unwrap_or_default(),
        access_key: form.access_key.trim().to_string(),
        secret_key: form.secret_key.trim().to_string(),
        base_url,
        model,
        model_display,
        name: String::new(),
        provider_id: form.provider_id.clone(),
    };
    new_cfg.name = form.name.trim().to_string();

    // api_key: for edit, only apply if user modified the field
    if was_new || form.key_modified {
        new_cfg.api_key = form.api_key.trim().to_string();
    }
    // api_key_env: for edit, only apply if user modified the field
    if was_new || form.env_modified {
        new_cfg.api_key_env = form.api_key_env.trim().to_string();
    }

    app.config.entries.insert(id.clone(), new_cfg);
    app.config.active = Some(id.clone());
    app.config.sanitize_entries();

    if let Err(e) = app.config.save(&app.config_path) {
        app.notify(ToastLevel::Fail, format!("save: {e}"));
        return;
    }

    if was_new {
        app.notify(ToastLevel::Ok, format!("added {id}"));
    } else {
        app.notify(ToastLevel::Ok, format!("saved {id}"));
    }

    // refresh status bar
    app.status.set_provider_name(&app.config.active_name());
    app.status.set_model(&app.config.active_model_display());
    app.refresh_status_model_context();

    // Open the model picker so the user can pick a model. Validate first
    // so we can set the picker's initial state correctly (fetching vs
    // idle with an error message).
    if let Some(k) = app.config.active_kind() {
        if k == ProviderKind::Cursor {
            start_cursor_oauth(app);
            return;
        }
        let active_id = match app.config.active.clone() {
            Some(id) => id,
            None => return,
        };

        let mut state = crate::function::ModelPickerState::new(k);
        state.entry_id = Some(active_id.clone());
        match app.config.validate_provider(&active_id) {
            Ok(_) => state.fetching = true,
            Err(e) => state.fetch_error = Some(e),
        }
        let should_fetch = state.fetching;
        app.function
            .push(crate::function::SidebarTab::ModelPicker(state));
        app.show_panel();
        app.acknowledge_panel();

        if should_fetch {
            let base = app
                .config
                .entry(&active_id)
                .map(|c| c.base_url.clone())
                .unwrap_or_default();
            let key = app.config.effective_api_key(&active_id).unwrap_or_default();
            let access_key = app
                .config
                .entry(&active_id)
                .map(|c| c.access_key.clone())
                .unwrap_or_default();
            let secret_key = app
                .config
                .entry(&active_id)
                .map(|c| c.secret_key.clone())
                .unwrap_or_default();
            let client = app.reqwest.clone();
            let provider_name = app
                .config
                .entry(&active_id)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            let provider_id = app
                .config
                .entry(&active_id)
                .map(|c| c.provider_id.clone())
                .unwrap_or_default();
            let cache_path = app
                .model_cache_path
                .parent()
                .unwrap_or(&app.model_cache_path)
                .to_path_buf();
            if let Some(tx) = app.msg_tx.clone() {
                tokio::spawn(async move {
                    match crate::providers::list_models(crate::providers::ListModelsArgs {
                        client: &client,
                        kind: k,
                        base_url: &base,
                        api_key: &key,
                        access_key: &access_key,
                        secret_key: &secret_key,
                        cache_path: &cache_path,
                        provider_name: &provider_name,
                        provider_id: &provider_id,
                    })
                    .await
                    {
                        Ok(models) => {
                            let _ = tx.send(AppMsg::ModelsFetched {
                                provider: k,
                                base_url: base,
                                api_key: key,
                                models,
                            });
                        }
                        Err(e) => {
                            let no_endpoint = matches!(
                                e.downcast_ref::<crate::providers::ProviderError>(),
                                Some(crate::providers::ProviderError::NoModelsEndpoint)
                            );
                            let _ = tx.send(AppMsg::ModelsFetchFailed {
                                provider: k,
                                error: format!("{e}"),
                                no_endpoint,
                            });
                        }
                    }
                });
            }
        }
    }
}
pub(super) fn handle_new_provider_key(
    k: crossterm::event::KeyEvent,
    state: &mut crate::function::SettingsState,
) -> bool {
    use crossterm::event::KeyCode;
    let picker = &mut state.new_provider;
    match k.code {
        KeyCode::Esc => {
            if picker.query.is_empty() {
                state.level = crate::function::SettingsLevel::ProviderList;
                state.cursor = 0;
            } else {
                picker.query.clear();
                picker.rebuild_filter();
                picker.focus = crate::function::PickerFocus::List;
            }
            true
        }
        KeyCode::Enter => false,
        KeyCode::Up => {
            picker.focus = crate::function::PickerFocus::List;
            picker.cursor = picker.cursor.saturating_sub(1);
            true
        }
        KeyCode::Down => {
            picker.focus = crate::function::PickerFocus::List;
            if picker.cursor + 1 < picker.filtered.len() {
                picker.cursor += 1;
            }
            true
        }
        KeyCode::Backspace => {
            picker.query.pop();
            picker.focus = crate::function::PickerFocus::Search;
            picker.rebuild_filter();
            true
        }
        KeyCode::Char(c) => {
            picker.query.push(c);
            picker.focus = crate::function::PickerFocus::Search;
            picker.rebuild_filter();
            true
        }
        _ => false,
    }
}
/// Text editing inside a `ConfigForm` level. Returns true if the key was used.
pub(super) fn handle_form_text(
    k: crossterm::event::KeyEvent,
    ctrl: bool,
    form: &mut crate::function::ConfigFormState,
) -> bool {
    use crate::function::ConfigField;
    if ctrl {
        return false;
    }
    if matches!(k.code, crossterm::event::KeyCode::Tab) {
        // Tab cycles fields within the form using active_fields.
        let fields = form.active_fields();
        let idx = fields.iter().position(|f| *f == form.focused).unwrap_or(0);
        form.focused = fields[(idx + 1) % fields.len()];
        return true;
    }
    match form.focused {
        ConfigField::Name => match k.code {
            crossterm::event::KeyCode::Char(c) => {
                form.name.push(c);
                true
            }
            crossterm::event::KeyCode::Backspace => {
                form.name.pop();
                true
            }
            _ => false,
        },
        ConfigField::BaseUrl => match k.code {
            crossterm::event::KeyCode::Char(c) => {
                form.base_url.push(c);
                true
            }
            crossterm::event::KeyCode::Backspace => {
                form.base_url.pop();
                true
            }
            _ => false,
        },
        ConfigField::Key => {
            // First edit clears the saved (masked) value so the user can
            // type a new key. If they don't touch the field, the original
            // is preserved on save.
            if !form.key_modified && !form.api_key.is_empty() {
                form.api_key.clear();
            }
            form.key_modified = true;
            match k.code {
                crossterm::event::KeyCode::Char(c) => {
                    form.api_key.push(c);
                    true
                }
                crossterm::event::KeyCode::Backspace => {
                    form.api_key.pop();
                    true
                }
                _ => false,
            }
        }
        ConfigField::Env => {
            if !form.env_modified && !form.api_key_env.is_empty() {
                form.api_key_env.clear();
            }
            form.env_modified = true;
            match k.code {
                crossterm::event::KeyCode::Char(c) => {
                    form.api_key_env.push(c);
                    true
                }
                crossterm::event::KeyCode::Backspace => {
                    form.api_key_env.pop();
                    true
                }
                _ => false,
            }
        }
        ConfigField::AccessKey => match k.code {
            crossterm::event::KeyCode::Char(c) => {
                form.access_key.push(c);
                true
            }
            crossterm::event::KeyCode::Backspace => {
                form.access_key.pop();
                true
            }
            _ => false,
        },
        ConfigField::SecretKey => match k.code {
            crossterm::event::KeyCode::Char(c) => {
                form.secret_key.push(c);
                true
            }
            crossterm::event::KeyCode::Backspace => {
                form.secret_key.pop();
                true
            }
            _ => false,
        },
        _ => false,
    }
}
pub(super) fn open_context_picker(
    app: &mut App,
    state: &mut crate::function::ModelPickerState,
    model_idx: usize,
) {
    use crate::config::parse_id;
    // Resolve the entry by the picker's bound entry id when available
    // (multiple entries can share a kind), else by kind. Uses the right
    // provider name for the models.dev lookup instead of the global
    // active entry, which may be a different same-kind endpoint.
    let p = state.provider;
    let target_id: Option<String> = match state.entry_id.as_deref() {
        Some(id) if app.config.entry(id).is_some() => Some(id.to_string()),
        _ => match app.config.active.as_deref() {
            Some(id) if parse_id(id).map(|(k, _)| k == p).unwrap_or(false) => Some(id.to_string()),
            _ => app
                .config
                .entries
                .keys()
                .find(|id| parse_id(id).map(|(k2, _)| k2 == p).unwrap_or(false))
                .cloned(),
        },
    };
    let provider_name = target_id
        .as_deref()
        .and_then(|id| app.config.entry(id))
        .map(|c| c.name.clone())
        .unwrap_or_default()
        .to_lowercase();

    let cache_path = app
        .model_cache_path
        .parent()
        .unwrap_or(&app.model_cache_path);
    let model_data_path = cache_path.join("model-data.json");
    let model_data = crate::model_data::ModelData::load(&model_data_path).unwrap_or_else(|| {
        crate::model_data::ModelData {
            models: std::collections::HashMap::new(),
            providers: std::collections::HashMap::new(),
            fetched_at: chrono::Utc::now(),
        }
    });

    let options = model_data.context_options_for_provider(&provider_name);

    state.context_pick = Some(crate::function::ContextPickerState {
        model_idx,
        options,
        cursor: 0,
        custom_input: String::new(),
        focus: crate::function::ContextPickerFocus::Options,
    });
}
pub(super) fn handle_context_picker_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::ModelPickerState,
) -> bool {
    use crossterm::event::KeyCode;
    let Some(cp) = &mut state.context_pick else {
        return false;
    };

    match k.code {
        KeyCode::Esc => {
            state.context_pick = None;
            true
        }
        KeyCode::Tab => {
            cp.focus = match cp.focus {
                crate::function::ContextPickerFocus::Options => {
                    crate::function::ContextPickerFocus::CustomInput
                }
                crate::function::ContextPickerFocus::CustomInput => {
                    crate::function::ContextPickerFocus::Options
                }
            };
            true
        }
        KeyCode::Up => {
            if cp.focus == crate::function::ContextPickerFocus::Options && cp.cursor > 0 {
                cp.cursor -= 1;
            }
            true
        }
        KeyCode::Down => {
            if cp.focus == crate::function::ContextPickerFocus::Options
                && cp.cursor + 1 < cp.options.len()
            {
                cp.cursor += 1;
            }
            true
        }
        KeyCode::Enter => {
            match cp.focus {
                crate::function::ContextPickerFocus::Options => {
                    if let Some(opt) = cp.options.get(cp.cursor) {
                        let ctx = opt.context;
                        let model_idx = cp.model_idx;
                        state.models[model_idx].context_window_tokens = Some(ctx);
                        state.models[model_idx].context_needs_pick = false;
                        // Save to custom cache
                        let cache_path = app
                            .model_cache_path
                            .parent()
                            .unwrap_or(&app.model_cache_path);
                        let custom_cache_path = cache_path.join("context-cache.json");
                        let mut custom_cache =
                            crate::model_data::CustomContextCache::load(&custom_cache_path);
                        custom_cache.set(
                            state.models[model_idx].id.clone(),
                            ctx,
                            &custom_cache_path,
                        );
                        let id = state.models[model_idx].id.clone();
                        let provider = state.provider;
                        let entry_id = state.entry_id.clone();
                        state.context_pick = None;
                        commit_model_with_entry(app, provider, entry_id.as_deref(), id, false);
                    }
                }
                crate::function::ContextPickerFocus::CustomInput => {
                    let input = cp.custom_input.trim().to_string();
                    if let Ok(ctx) = input.parse::<u64>() {
                        let ctx = ctx * 1000; // user enters k, store as tokens
                        let model_idx = cp.model_idx;
                        state.models[model_idx].context_window_tokens = Some(ctx);
                        state.models[model_idx].context_needs_pick = false;
                        // Save to custom cache
                        let cache_path = app
                            .model_cache_path
                            .parent()
                            .unwrap_or(&app.model_cache_path);
                        let custom_cache_path = cache_path.join("context-cache.json");
                        let mut custom_cache =
                            crate::model_data::CustomContextCache::load(&custom_cache_path);
                        custom_cache.set(
                            state.models[model_idx].id.clone(),
                            ctx,
                            &custom_cache_path,
                        );
                        let id = state.models[model_idx].id.clone();
                        let provider = state.provider;
                        let entry_id = state.entry_id.clone();
                        state.context_pick = None;
                        commit_model_with_entry(app, provider, entry_id.as_deref(), id, false);
                    }
                }
            }
            true
        }
        KeyCode::Backspace => {
            if cp.focus == crate::function::ContextPickerFocus::CustomInput {
                cp.custom_input.pop();
            }
            true
        }
        KeyCode::Char(c) => {
            if cp.focus == crate::function::ContextPickerFocus::CustomInput && c.is_ascii_digit() {
                cp.custom_input.push(c);
            }
            true
        }
        _ => false,
    }
}
#[allow(dead_code)]
pub fn commit_model(
    app: &mut App,
    provider: crate::config::ProviderKind,
    model_id: String,
    manual: bool,
) {
    commit_model_with_entry(app, provider, None, model_id, manual)
}

/// Same as `commit_model` but, when the picker knows the exact entry it
/// was opened for, `entry_id` pins the commit to that entry. This matters
/// when several configured entries share a kind (e.g. two OpenAI
/// endpoints): without it, the kind-based fallback could activate the
/// wrong same-kind entry.
pub fn commit_model_with_entry(
    app: &mut App,
    provider: crate::config::ProviderKind,
    entry_id: Option<&str>,
    model_id: String,
    manual: bool,
) {
    use crate::config::parse_id;
    use crate::function::notifications::ToastLevel;

    // 1. Find target entry id:
    //    - If the picker was bound to a specific entry that still exists,
    //      use it (handles multiple entries of the same kind).
    //    - Else if the active entry's kind matches, use it.
    //    - Otherwise, find any existing entry with the same kind.
    //    - Otherwise, leave the target unset (no entry to attach the model to).
    let target_id: Option<String> = match entry_id {
        Some(id) if app.config.entry(id).is_some() => Some(id.to_string()),
        _ => match app.config.active.as_deref() {
            Some(id) if parse_id(id).map(|(k, _)| k == provider).unwrap_or(false) => {
                Some(id.to_string())
            }
            Some(_) | None => app
                .config
                .entries
                .keys()
                .find(|id| parse_id(id).map(|(k2, _)| k2 == provider).unwrap_or(false))
                .cloned(),
        },
    };

    let selected_model =
        app.model_cache
            .get(provider)
            .and_then(|cache| {
                cache.models.iter().find(|m| {
                    m.id == model_id || m.request_id.as_deref() == Some(model_id.as_str())
                })
            })
            .cloned();
    let request_model_id = selected_model
        .as_ref()
        .and_then(|m| m.request_id.clone())
        .unwrap_or_else(|| model_id.clone());
    let display_model = selected_model
        .as_ref()
        .map(|m| m.display.clone())
        .unwrap_or_else(|| model_id.clone());

    // 2. Update the target entry's request model id and make it active.
    if let Some(id) = target_id {
        app.config.active = Some(id.clone());
        if let Some(entry) = app.config.entry_mut(&id) {
            entry.model = request_model_id.clone();
            entry.model_display = display_model.clone();
        }
    }

    // 3. Refresh the status bar.
    app.status.set_provider_name(&app.config.active_name());
    app.status.set_model(&app.config.active_model_display());
    app.refresh_status_model_context();
    if let Some(tokens) = selected_model.and_then(|m| m.context_window_tokens) {
        app.status.set_context_window_tokens(tokens);
    }

    // 4. Close the picker tab.
    close_active_function_tab(app);
    // 4b. If the now-active tab is a ProviderPicker (the /model flow's
    // first step) the user has finished the flow — close it too,
    // otherwise the panel would still be open and the user would
    // have to Esc a second time to exit.
    if app.function.active < app.function.tabs.len()
        && matches!(
            app.function.tabs[app.function.active],
            crate::function::SidebarTab::ProviderPicker(_)
        )
    {
        close_active_function_tab(app);
    }

    // 5. Persist to disk (after tab cleanup so notify() doesn't
    //    shift indices when creating a Notifications tab).
    app.save_config();

    // 6. Toast.
    if manual {
        app.notify(ToastLevel::Ok, format!("manual model id set: {model_id}"));
    } else {
        app.notify(
            ToastLevel::Ok,
            format!("model set: {}:{model_id}", provider.as_str()),
        );
    }
}

/// Search / navigate / toggle for the tool picker.  Space toggles
/// the focused tool's enabled/disabled state; Enter confirms and
/// closes the tab; Esc cancels without applying.
pub(super) fn handle_tool_picker_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::ToolPickerState,
) -> bool {
    use crossterm::event::KeyCode;
    match k.code {
        KeyCode::Up => {
            if state.cursor > 0 {
                state.cursor -= 1;
            }
            true
        }
        KeyCode::Down => {
            if state.cursor + 1 < state.filtered.len() {
                state.cursor += 1;
            }
            true
        }
        KeyCode::Char(' ') => {
            if let Some(name) = state.selected() {
                if matches!(
                    crate::permission::check(app.active_agent, name),
                    crate::permission::Action::Deny
                ) {
                    app.notify(
                        ToastLevel::Warn,
                        format!("`{name}` is locked in {} mode", app.active_agent.as_str()),
                    );
                } else if app.disabled_tools.contains(name) {
                    app.disabled_tools.remove(name);
                } else {
                    app.disabled_tools.insert(name.to_string());
                }
            }
            true
        }
        KeyCode::Enter => {
            app.sync_disabled_tools();
            let n = app.disabled_tools.len();
            close_active_function_tab(app);
            if n == 0 {
                app.notify(ToastLevel::Ok, "all tools enabled");
            } else {
                app.notify(ToastLevel::Info, format!("{n} tool(s) disabled"));
            }
            true
        }
        KeyCode::Esc => {
            close_active_function_tab(app);
            true
        }
        KeyCode::Char(c) => {
            state.query.push(c);
            state.rebuild_filter();
            true
        }
        KeyCode::Backspace => {
            state.query.pop();
            state.rebuild_filter();
            true
        }
        _ => true,
    }
}
