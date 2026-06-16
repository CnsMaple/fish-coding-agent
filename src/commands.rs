use crate::app::App;
use crate::config::{parse_id, ProviderKind, ProviderMode};
use crate::function::notifications::ToastLevel;
use crate::function::SidebarTab;
use crate::providers::{ChatMessage, ChatRequest};
use crate::session::{Message, Role};

pub fn dispatch(app: &mut App, cmd: &str, arg: &str) {
    match cmd {
        "settings" => open_settings(app),
        "model" => open_model_picker(app),
        "hotkey" | "help" | "keys" => open_hotkey(app),
        "clear" => {
            app.session.clear();
            app.notify(ToastLevel::Info, "session cleared");
        }
        "think" | "thinking" => {
            use crate::config::ReasoningMode;
            let arg = arg.trim();
            if arg.is_empty() {
                // Open a picker in the function panel.
                open_thinking_picker(app);
                return;
            } else {
                let next = match arg {
                    "off" => ReasoningMode::Off,
                    "low" => ReasoningMode::Low,
                    "med" | "medium" => ReasoningMode::Med,
                    "high" => ReasoningMode::High,
                    "adaptive" => ReasoningMode::Adaptive,
                    _ => {
                        app.notify(ToastLevel::Fail, format!("unknown thinking level: {arg} (off/low/med/high/adaptive)"));
                        return;
                    }
                };
                app.config.thinking = next;
                app.status.set_thinking(next);
                app.save_config();
                app.notify(ToastLevel::Ok, format!("thinking: {}", next.as_str()));
            }
        }
        "timeline" => {
            // Open (or focus) the timeline picker in the function panel.
            // The picker lists every message in the session with a search
            // box; pressing Enter on an entry jumps the session scroll to
            // that message.
            open_timeline_picker(app);
        }
        "quit" | "exit" | "q" => {
            app.should_quit = true;
        }
        "provider" => {
            // /provider <kind>[:<mode>]   (defaults to key mode)
            let arg = arg.trim();
            if arg.is_empty() {
                let id = app.config.active.clone().unwrap_or_else(|| "-".to_string());
                app.notify(ToastLevel::Info, format!("current provider: {id}"));
                return;
            }
            let id = if arg.contains(':') {
                arg.to_string()
            } else if let Some(k) = ProviderKind::from_str_opt(arg) {
                crate::config::make_id(k, ProviderMode::Key)
            } else {
                app.notify(ToastLevel::Fail, format!("unknown provider: {arg}"));
                return;
            };
            if !app.config.entries.contains_key(&id) {
                app.notify(
                    ToastLevel::Fail,
                    format!("provider {id} not configured; open /settings"),
                );
                return;
            }
            app.config.active = Some(id.clone());
            app.status.set_provider_name(&app.config.active_name());
            app.status.set_model(&app.config.active_model_display());
            app.save_config();
            app.notify(ToastLevel::Ok, format!("provider switched to {id}"));
        }
        _ => {
            app.notify(ToastLevel::Fail, format!("unknown command: /{cmd}"));
        }
    }
}

pub fn open_settings(app: &mut App) {
    open_settings_at(app, crate::function::SettingsLevel::TopLevel);
}

/// Open a fresh Settings tab and jump to `initial_level`. Used by
/// `open_model_picker` so the user lands directly on ProviderList (skipping
/// the redundant TopLevel) when they are routed here because no model is
/// configured.
pub fn open_settings_at(
    app: &mut App,
    initial_level: crate::function::SettingsLevel,
) {
    let mut state = crate::function::SettingsState::new(&app.config);
    state.level = initial_level;
    state.clamp_cursor(&app.config);
    app.function.push(SidebarTab::Settings(state));
    app.function_visible = true;
    app.acknowledge_panel();
}

pub fn open_model_picker(app: &mut App) {
    // /model is now a two-step flow: first pick a configured provider
    // entry (by name, not just by kind), then pick a model for that
    // entry's kind. If the user has only one entry we skip straight to
    // the model list. If they have none, route to /settings like
    // before.

    // Count configured entries (one per row in the picker). The picker
    // shows one row per entry — not per kind — so multiple entries of
    // the same kind (e.g. "prod-openai" and "dev-openai") each get
    // their own line.
    let entry_count = app.config.entries.len();

    // If a ModelPicker is already open, just focus it — the user is
    // continuing from where they left off. They can Esc out and re-run
    // /model if they want to switch providers.
    if let Some(idx) = app
        .function
        .tabs
        .iter()
        .position(|t| matches!(t, SidebarTab::ModelPicker(_)))
    {
        app.function.active = idx;
        app.function_visible = true;
        app.acknowledge_panel();
        return;
    }

    match entry_count {
        0 => {
            app.notify(
                ToastLevel::Warn,
                "no active provider; configure one via /settings",
            );
            // Land on ProviderList directly (skip TopLevel's "set provider"
            // step) so the user can pick a kind/mode right away.
            open_settings_at(app, crate::function::SettingsLevel::ProviderList);
        }
        1 => {
            // Only one configured entry — skip the chooser and jump
            // straight to the model list for its kind.
            let kind = app
                .config
                .entries
                .keys()
                .next()
                .and_then(|id| parse_id(id).map(|(k, _)| k));
            if let Some(kind) = kind {
                open_model_picker_for_kind(app, kind);
            }
        }
        _ => {
            // Multiple entries — show the provider picker. The user
            // picks one, then the model picker for its kind replaces
            // this tab.
            open_provider_picker(app);
        }
    }
}

/// Open (or focus) a ModelPicker tab for a specific provider kind.
/// Used by the two-step /model flow after the user has chosen a
/// provider, and also directly when there's only one provider
/// configured (so the chooser step is skipped).
pub fn open_model_picker_for_kind(app: &mut App, provider: crate::config::ProviderKind) {
    // If a picker for this exact provider is already open, focus it.
    if let Some(idx) = app.function.tabs.iter().position(|t| {
        matches!(t, SidebarTab::ModelPicker(s) if s.provider == provider)
    }) {
        app.function.active = idx;
        app.function_visible = true;
        app.acknowledge_panel();
        return;
    }
    let mut state = crate::function::ModelPickerState::new(provider);
    if let Some(c) = app.model_cache.get(provider) {
        state.models = c.models.clone();
        state.rebuild_filter();
    }
    app.function.push(SidebarTab::ModelPicker(state));
    app.function_visible = true;
    app.acknowledge_panel();
}

pub fn open_provider_picker(app: &mut App) {
    if let Some(idx) = app
        .function
        .tabs
        .iter()
        .position(|t| matches!(t, SidebarTab::ProviderPicker(_)))
    {
        app.function.active = idx;
    } else {
        let state = crate::function::ProviderPickerState::new(&app.config);
        app.function.push(SidebarTab::ProviderPicker(state));
    }
    app.function_visible = true;
    app.acknowledge_panel();
}

pub fn open_hotkey(app: &mut App) {
    if let Some(idx) = app
        .function
        .tabs
        .iter()
        .position(|t| matches!(t, SidebarTab::Hotkey))
    {
        app.function.active = idx;
    } else {
        app.function.push(SidebarTab::Hotkey);
    }
    app.function_visible = true;
    app.acknowledge_panel();
}

pub fn open_thinking_picker(app: &mut App) {
    app.function
        .push(SidebarTab::ThinkingPicker(crate::function::ThinkingPickerState::new()));
    app.function_visible = true;
    app.acknowledge_panel();
}

pub fn open_timeline_picker(app: &mut App) {
    if let Some(idx) = app
        .function
        .tabs
        .iter()
        .position(|t| matches!(t, SidebarTab::TimelinePicker(_)))
    {
        app.function.active = idx;
    } else {
        let state = crate::function::TimelinePickerState::new(&app.session);
        app.function.push(SidebarTab::TimelinePicker(state));
    }
    app.function_visible = true;
    app.acknowledge_panel();
}

pub fn send_chat(app: &mut App, user_text: String) {
    if app.inflight.is_some() {
        app.notify(ToastLevel::Warn, "request in flight, please wait");
        return;
    }
    let Some(active_id) = app.config.active.clone() else {
        app.notify(
            ToastLevel::Fail,
            "no active provider; configure one via /settings",
        );
        open_settings(app);
        return;
    };
    if let Err(e) = app.config.validate_provider(&active_id) {
        app.notify(ToastLevel::Fail, e.clone());
        app.session.push(Message::new(
            Role::System,
            format!("[config error] {e} - open /settings to fix"),
        ));
        if !app
            .function
            .tabs
            .iter()
            .any(|t| matches!(t, SidebarTab::Settings(_)))
        {
            open_settings(app);
        }
        return;
    }
    let (provider, _mode) = match parse_id(&active_id) {
        Some(p) => p,
        None => {
            app.notify(ToastLevel::Fail, "active provider id invalid");
            return;
        }
    };
    let base = app
        .config
        .entry(&active_id)
        .map(|c| c.base_url.clone())
        .unwrap_or_default();
    let key: String = match app.config.effective_api_key(&active_id) {
        Some(k) if !k.is_empty() => k,
        _ => {
            let env_name = app
                .config
                .entry(&active_id)
                .map(|c| c.api_key_env.clone())
                .unwrap_or_default();
            app.session.push(Message::new(
                Role::System,
                format!(
                    "[no api key for {active_id}: set it via /settings or env {env_name}]"
                ),
            ));
            app.notify(ToastLevel::Fail, format!("missing api key for {active_id}"));
            return;
        }
    };
    let model = app.config.active_model().to_string();
    let thinking = app.config.thinking;

    app.session.push(Message::new(Role::User, user_text.clone()));
    let assistant = Message {
        role: Role::Assistant,
        content: String::new(),
        thinking: String::new(),
        thinking_visible: false,
        display_cursor: 0,
        ts: chrono::Utc::now(),
        streaming: true,
    };
    let id = app.session.push(assistant);
    app.session.streaming_id = Some(id);

    let messages: Vec<ChatMessage> = app
        .session
        .messages
        .iter()
        .filter(|m| !matches!(m.role, Role::System))
        .filter(|m| !(matches!(m.role, Role::Assistant) && m.content.is_empty()))
        .map(|m| ChatMessage {
            role: match m.role {
                Role::User => "user".to_string(),
                Role::Assistant => "assistant".to_string(),
                Role::System => "user".to_string(),
            },
            content: m.content.clone(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        })
        .collect();
    let (cancel_tx, _cancel_rx) = tokio::sync::watch::channel(false);
    app.inflight = Some(crate::app::InflightHandle {
        cancel: cancel_tx,
        label: format!("chat:{active_id}:{model}"),
    });

    let req = ChatRequest {
        model,
        messages,
        thinking,
        system: None,
    };

    if let Some(tx) = app.msg_tx.clone() {
        let client = app.reqwest.clone();
        let cwd = app.cwd.clone();
        tokio::spawn(async move {
            let mut req = req;
            for _ in 0..8 {
                let (chat_tx, mut chat_rx) =
                    tokio::sync::mpsc::unbounded_channel::<crate::providers::ChatEvent>();
                let p = crate::providers::provider(provider);
                let client_for_call = client.clone();
                let base_for_call = base.clone();
                let key_for_call = key.clone();
                let req_for_call = crate::providers::ChatRequest {
                    model: req.model.clone(),
                    messages: req.messages.clone(),
                    thinking: req.thinking,
                    system: req.system.clone(),
                };
                let call = tokio::spawn(async move {
                    p.chat_stream(&client_for_call, &base_for_call, &key_for_call, req_for_call, chat_tx).await
                });

                let mut assistant_content = String::new();
                let mut tool_calls: Vec<crate::providers::ToolCall> = Vec::new();
                while let Some(ev) = chat_rx.recv().await {
                    match ev {
                        crate::providers::ChatEvent::Delta(s) => {
                            assistant_content.push_str(&s);
                            let _ = tx.send(crate::event::AppMsg::ChatDelta(s));
                        }
                        crate::providers::ChatEvent::ThinkingDelta(s) => {
                            let _ = tx.send(crate::event::AppMsg::ChatThinkingDelta(s));
                        }
                        crate::providers::ChatEvent::Usage(u) => {
                            let _ = tx.send(crate::event::AppMsg::ChatUsage(u));
                        }
                        crate::providers::ChatEvent::ToolCalls(calls) => {
                            tool_calls = calls;
                        }
                        crate::providers::ChatEvent::Done => break,
                        crate::providers::ChatEvent::Error(e) => {
                            let _ = tx.send(crate::event::AppMsg::ChatError(e));
                            return;
                        }
                    }
                }

                match call.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        let _ = tx.send(crate::event::AppMsg::ChatError(format!("{e}")));
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send(crate::event::AppMsg::ChatError(format!("chat task failed: {e}")));
                        return;
                    }
                }

                if tool_calls.is_empty() {
                    let _ = tx.send(crate::event::AppMsg::ChatDone);
                    return;
                }

                req.messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: assistant_content,
                    tool_call_id: None,
                    tool_calls: tool_calls.clone(),
                });

                for call in tool_calls {
                    let _ = tx.send(crate::event::AppMsg::ChatDelta(format!(
                        "\n\n[tool:{}]\n",
                        call.name
                    )));
                    let result = crate::tools::execute_tool(&call.name, &call.arguments, &cwd).await;
                    req.messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: result.clone(),
                        tool_call_id: Some(call.id.clone()),
                        tool_calls: Vec::new(),
                    });
                    let _ = tx.send(crate::event::AppMsg::ChatDelta(format!(
                        "```json\n{}\n```\n",
                        result
                    )));
                }
            }
            let _ = tx.send(crate::event::AppMsg::ChatError(
                "tool loop exceeded maximum iterations".to_string(),
            ));
        });
    }
}
