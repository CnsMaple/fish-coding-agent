use crate::app::App;
use crate::config::{parse_id, ProviderKind, ProviderMode};
use crate::function::notifications::ToastLevel;
use crate::function::SidebarTab;
use crate::providers::{ChatMessage, ChatRequest, ToolCall};
use crate::session::{Message, Role};

pub fn dispatch(app: &mut App, cmd: &str, arg: &str) {
    match cmd {
        "settings" => open_settings(app),
        "model" => open_model_picker(app),
        "hotkey" | "help" | "keys" => open_hotkey(app),
        "new" | "clear" => {
            app.start_new_session();
            app.notify(
                ToastLevel::Info,
                if cmd == "new" {
                    "new session"
                } else {
                    "session cleared"
                },
            );
        }
        "think" | "thinking" => {
            use crate::config::ReasoningMode;
            let arg = arg.trim();
            if arg.is_empty() {
                // Open a picker in the function panel.
                open_thinking_picker(app);
                return;
            }
            let next = match arg {
                "off" => ReasoningMode::Off,
                "low" => ReasoningMode::Low,
                "med" | "medium" => ReasoningMode::Med,
                "high" => ReasoningMode::High,
                "adaptive" => ReasoningMode::Adaptive,
                _ => {
                    app.notify(
                        ToastLevel::Fail,
                        format!("unknown thinking level: {arg} (off/low/med/high/adaptive)"),
                    );
                    return;
                }
            };
            app.config.thinking = next;
            app.status.set_thinking(next);
            app.save_config();
            app.notify(ToastLevel::Ok, format!("thinking: {}", next.as_str()));
        }
        "timeline" => {
            open_timeline_picker(app);
        }
        "session" | "sessions" => {
            open_session_picker(app, crate::function::SessionPickerMode::Manage)
        }
        "rename" => {
            let title = arg.trim();
            if title.is_empty() {
                open_session_rename(app, None, app.session_title.clone());
            } else {
                app.rename_session(None, title.to_string());
            }
        }
        "fork" => app.fork_session(None),
        "retry" => retry_last_prompt(app),
        "continue" => continue_response(app, arg),
        "plan" => {
            let arg = arg.trim().to_lowercase();
            if matches!(arg.as_str(), "exit" | "off" | "yolo" | "build") {
                app.set_mode(crate::function::AppMode::Yolo);
                app.notify(ToastLevel::Info, "mode: build");
            } else if arg.is_empty() {
                app.set_mode(crate::function::AppMode::Plan);
                app.notify(
                    ToastLevel::Info,
                    "mode: plan (read-only — use /build to switch back)",
                );
            } else {
                app.notify(
                    ToastLevel::Fail,
                    "unknown plan command: use /plan or /plan exit",
                );
            }
        }
        "build" => {
            app.set_mode(crate::function::AppMode::Yolo);
            app.notify(ToastLevel::Info, "mode: build");
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
            app.refresh_status_model_context();
            app.save_config();
            app.notify(ToastLevel::Ok, format!("provider switched to {id}"));
        }
        "skill" => dispatch_skill(app, arg, ""),
        "mcp" => open_mcp(app, arg),
        _ => {
            app.notify(ToastLevel::Fail, format!("unknown command: /{cmd}"));
        }
    }
}
/// Public entry used by `event::submit_input` for the colon form.
pub fn dispatch_skill(app: &mut App, name: &str, args: &str) {
    open_skill(app, name, args);
}

/// `/skill:<name> [args...]` - dispatch immediately. The skill's
/// template body goes to the AI as the user prompt; the chat UI
/// renders a clean `[skill]` block (name / args / context path) so
/// the user sees what was invoked without scrolling through the
/// raw template.
fn open_skill(app: &mut App, name: &str, args: &str) {
    let name = name.trim();
    let args = args.trim();
    if name.is_empty() {
        let names = crate::skill::list_names();
        let preview = names
            .iter()
            .take(8)
            .map(|n| format!("/skill:{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        let more = names.len().saturating_sub(8);
        let msg = if more == 0 {
            format!("skills: {preview}")
        } else {
            format!("skills: {preview} (+{more} more)")
        };
        app.notify(ToastLevel::Info, msg);
        return;
    }
    let Some(skill) = crate::skill::find(name) else {
        let known = crate::skill::list_names().join(", ");
        app.notify(
            ToastLevel::Fail,
            format!("unknown skill '{name}'. try: {known}"),
        );
        return;
    };
    if app.inflight.is_some() {
        app.notify(ToastLevel::Warn, "request in flight, please wait");
        return;
    }
    let context_path = crate::skill::skill_path(name)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| format!("<unknown skill path: {name}>"));
    // Body sent to the AI: the skill template alone, or template +
    // user's trailing instruction. The `[skill]` block is purely a
    // UI artifact (`Message::skill_ref`) and never reaches the model.
    let prompt_body = if args.is_empty() {
        skill.template.clone()
    } else {
        format!("{}\n\n{}", skill.template, args)
    };
    let mut msg = Message::new(Role::User, prompt_body);
    msg.skill_ref = Some(crate::session::SkillRef {
        name: name.to_string(),
        context_path,
        args: if args.is_empty() {
            None
        } else {
            Some(args.to_string())
        },
    });
    send_message(app, msg);
}

/// `/mcp:<name>` - with no arg, list the available MCP servers; with a
/// name, switch the session to that server's tool surface. Skill
/// completion follows the same shape: Tab on a focused MCP
/// candidate fills the input directly.
fn open_mcp(app: &mut App, arg: &str) {
    let name = arg.trim();
    if name.is_empty() {
        let names = crate::mcp::builtin_names();
        let preview = names
            .iter()
            .take(8)
            .map(|n| format!("/mcp:{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        let more = names.len().saturating_sub(8);
        let msg = if more == 0 {
            format!("mcps: {preview}")
        } else {
            format!("mcps: {preview} (+{more} more)")
        };
        app.notify(ToastLevel::Info, msg);
        return;
    }
    match crate::mcp::find(name) {
        Some(server) => {
            app.notify(
                ToastLevel::Ok,
                format!("mcp '{}' ready: {}", server.name, server.description),
            );
        }
        None => {
            let known = crate::mcp::builtin_names().join(", ");
            app.notify(
                ToastLevel::Fail,
                format!("unknown mcp '{name}'. try: {known}"),
            );
        }
    }
}

fn retry_last_prompt(app: &mut App) {
    if app.inflight.is_some() {
        app.notify(ToastLevel::Warn, "request in flight, please wait");
        return;
    }
    let Some(idx) = app
        .session
        .messages
        .iter()
        .rposition(|m| matches!(m.role, Role::User) && !m.content.starts_with("Context from "))
    else {
        app.notify(ToastLevel::Warn, "no prompt to retry");
        return;
    };
    let prompt = app.session.messages[idx].content.clone();
    app.session.messages.truncate(idx);
    app.session.invalidate_message_cache_from(idx);
    app.session.invalidate_layout_cache();
    crate::commands::send_chat(app, prompt);
}

fn continue_response(app: &mut App, arg: &str) {
    if app.inflight.is_some() {
        app.notify(ToastLevel::Warn, "request in flight, please wait");
        return;
    }
    let prompt = if arg.is_empty() {
        String::new()
    } else {
        arg.to_string()
    };
    crate::commands::send_chat(app, prompt);
    // Remove the user message from session (kept in API request)
    if app.inflight.is_some() && app.session.messages.len() >= 2 {
        let idx = app.session.messages.len() - 2;
        if app.session.messages[idx].role == Role::User {
            app.session.messages.remove(idx);
            app.session.invalidate_message_cache_from(idx);
            app.session.invalidate_layout_cache();
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
pub fn open_settings_at(app: &mut App, initial_level: crate::function::SettingsLevel) {
    let mut state = crate::function::SettingsState::new(&app.config);
    state.level = initial_level;
    state.clamp_cursor(&app.config);
    app.function.push(SidebarTab::Settings(Box::new(state)));
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
    if let Some(idx) = app
        .function
        .tabs
        .iter()
        .position(|t| matches!(t, SidebarTab::ModelPicker(s) if s.provider == provider))
    {
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
    app.function.push(SidebarTab::ThinkingPicker(
        crate::function::ThinkingPickerState::new(),
    ));
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

pub fn open_session_picker(app: &mut App, mode: crate::function::SessionPickerMode) {
    app.save_current_session();
    if let Some(idx) = app
        .function
        .tabs
        .iter()
        .position(|t| matches!(t, SidebarTab::SessionPicker(_)))
    {
        app.function.active = idx;
        if let Some(SidebarTab::SessionPicker(state)) = app.function.tabs.get_mut(idx) {
            state.mode = mode;
            state.reload(&app.cwd);
        }
    } else {
        app.function.push(SidebarTab::SessionPicker(
            crate::function::SessionPickerState::new(mode, &app.cwd),
        ));
    }
    app.function_visible = true;
    app.acknowledge_panel();
}

pub fn open_session_rename(app: &mut App, target_id: Option<String>, title: String) {
    app.function
        .push(SidebarTab::SessionRename(match target_id {
            Some(id) => crate::function::SessionRenameState::new_target(id, title),
            None => crate::function::SessionRenameState::new_current(&title),
        }));
    app.function_visible = true;
    app.acknowledge_panel();
}

/// System prompt instructing the model about available tools.
/// Stresses using the structured tool_calls API, and provides a
/// text-based fallback format for providers that don't support it.
fn system_prompt(agent: crate::permission::Agent) -> String {
    match agent {
        crate::permission::Agent::Build => format!(
            "\
You are a coding assistant with access to the following tools in the user's workspace:

  - read_file(path, start_line?, end_line?)
  - write_file(path, content, start_line?, end_line?)
  - shell_command(command) - runs in {shell}
    Current shell details: {shell_details}
  - python_command(code) - runs Python source code directly
  - grep(pattern, path?) - search text in files
  - list(path?) - list files under a directory
  - plan(title?, content, steps?) - present a plan for user confirmation

When a task requires one of these actions you MUST invoke the appropriate tool via the API's structured tool_calls mechanism. Never describe using a tool without actually calling it.

If your API does not support structured tool_calls, describe each tool call as a single-line JSON object on its own line in the following format:
  >>> {{\"name\": \"tool_name\", \"arguments\": {{...}}}} <<<

Do NOT claim a tool was used unless you actually see its result.",
            shell = crate::tools::shell_description(),
            shell_details = crate::tools::shell_guidance()
        ),
        crate::permission::Agent::Plan => String::from(
            "\
## Responsibility

You are operating in **plan mode**, a read-only research and planning role. \
Your job is to understand the user's task, gather only the evidence you need, \
and present a concrete plan the user can approve before any code is written.

## What you can do

Read-only exploration:

  - read_file(path, start_line?, end_line?)
  - grep(pattern, path?) — search text in files
  - list(path?) — list files under a directory

Communication with the user:

  - ask(question, options?) — ask a clarifying question. Use this when \
    weighing tradeoffs, when the request is ambiguous, or when a single \
    decision is blocking the plan. The question is shown in the session; \
    the user types their answer into the main input and the conversation \
    resumes automatically. Don't overuse it — batch independent questions.
  - plan(title?, content, steps?) — present a plan for approval. The plan \
    body is rendered in the session; the user approves / rejects / closes \
    in the plan tab. Call this exactly once when you have enough \
    information to act.

## What you must NOT do

The runtime will reject (with an error) any attempt to:

  - write_file (no file edits)
  - shell_command (no arbitrary shell)
  - python_command (no code execution)

If a task truly requires running a command or mutating a file, hand it back \
to the user — they can switch to build mode with `/build` and re-send. Do \
not pretend to invoke these tools; never claim a tool ran unless you saw \
its result.

## Important

1. **Always use the structured tool_calls API** (or the `>>> {{\"name\":...}} \
   <<<` text fallback). Never describe a plan in prose without actually \
   calling the `plan` tool — the user only sees your plan when the tool \
   result is rendered.
2. **Explore before you plan.** If the request touches code you have not \
   read, use read_file/grep/list to ground the plan in the actual \
   repository. Do not invent file paths, function names, or behaviour.
3. **Be concise.** The plan body should be actionable: what changes, where, \
   and why. Numbered steps are good. Skip preamble and apologies.
4. **Prefer asking over guessing.** When two reasonable interpretations \
   exist and the choice meaningfully changes the plan, call `ask`. When \
   the choice is cosmetic, pick one and note it in the plan.
5. **Stop after the plan tool.** Do not call additional tools after `plan`; \
   wait for the user's decision.",
        ),
    }
}
pub fn send_chat(app: &mut App, user_text: String) {
    send_message(app, Message::new(Role::User, user_text));
}

/// Dispatch a pre-built user message to the active provider. Used by
/// `/skill:<name>` to send the skill's body (rather than a literal
/// `[skill]` marker) as the user prompt, while the chat UI still
/// renders the marker block via `Message::skill_ref`.
pub fn send_message(app: &mut App, user_msg: Message) {
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
                format!("[no api key for {active_id}: set it via /settings or env {env_name}]"),
            ));
            app.notify(ToastLevel::Fail, format!("missing api key for {active_id}"));
            return;
        }
    };
    let model = app.config.active_model().to_string();
    let thinking = app.config.thinking;

    app.maybe_title_from_first_prompt(&user_msg.content);
    app.session.push(user_msg);
    let assistant = Message {
        role: Role::Assistant,
        content: String::new(),
        thinking: String::new(),
        thinking_segments: Vec::new(),
        thinking_visible: false,
        tool_results: Vec::new(),
        display_cursor: 0,
        line_count: 0,
        cached_content_line_count: None,
        ts: chrono::Utc::now(),
        streaming: true,
        skill_ref: None,
        content_version: 0,
    };
    let id = app.session.push(assistant);
    app.session.streaming_id = Some(id);
    app.response_started_at = None;
    app.response_output_chars = 0;
    app.response_output_tokens = None;

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
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    app.inflight = Some(crate::app::InflightHandle {
        cancel: cancel_tx,
        label: format!("chat:{active_id}:{model}"),
    });

    let req = ChatRequest {
        model,
        messages,
        thinking,
        system: Some(system_prompt(app.active_agent)),
    };

    if let Some(tx) = app.msg_tx.clone() {
        let client = app.reqwest.clone();
        let cwd = app.cwd.clone();
        let agent = app.active_agent;
        tokio::spawn(async move {
            let mut req = req;
            let mut stream_retries = 0u32;
            loop {
                if *cancel_rx.borrow() {
                    let _ = tx.send(crate::event::AppMsg::ChatDebug(
                        "user cancelled".to_string(),
                    ));
                    let _ = tx.send(crate::event::AppMsg::ChatDone);
                    return;
                }
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
                    p.chat_stream(
                        &client_for_call,
                        &base_for_call,
                        &key_for_call,
                        req_for_call,
                        chat_tx,
                    )
                    .await
                });

                let mut assistant_content = String::new();
                let mut tool_calls: Vec<crate::providers::ToolCall> = Vec::new();
                let mut stream_done = false;
                while let Some(ev) = chat_rx.recv().await {
                    match ev {
                        crate::providers::ChatEvent::Delta(s) => {
                            assistant_content.push_str(&s);
                            let _ = tx.send(crate::event::AppMsg::ChatDelta(s));
                        }
                        crate::providers::ChatEvent::ThinkingDelta(s) => {
                            let _ = tx.send(crate::event::AppMsg::ChatThinkingDelta(s));
                        }
                        crate::providers::ChatEvent::Debug(s) => {
                            let _ = tx.send(crate::event::AppMsg::ChatDebug(s));
                        }
                        crate::providers::ChatEvent::Usage(u) => {
                            let _ = tx.send(crate::event::AppMsg::ChatUsage(u));
                        }
                        crate::providers::ChatEvent::ToolResult {
                            name,
                            title,
                            content,
                        } => {
                            let _ = tx.send(crate::event::AppMsg::ChatToolResult {
                                name,
                                title,
                                content,
                            });
                        }
                        crate::providers::ChatEvent::ToolCalls(calls) => {
                            tool_calls = calls;
                        }
                        crate::providers::ChatEvent::Done => {
                            stream_done = true;
                            break;
                        }
                        crate::providers::ChatEvent::Error(e) => {
                            let _ = tx.send(crate::event::AppMsg::ChatError(e));
                            return;
                        }
                        crate::providers::ChatEvent::ContentBlockStart(kind) => {
                            let _ = tx.send(crate::event::AppMsg::ChatContentBlockStart(kind));
                        }
                    }
                }

                if !stream_done {
                    let err = match call.await {
                        Ok(Ok(())) => None,
                        Ok(Err(e)) => Some(format!("{e}")),
                        Err(e) => Some(format!("chat task failed: {e}")),
                    };
                    if let Some(e) = err {
                        stream_retries += 1;
                        if stream_retries >= 3 {
                            let _ = tx.send(crate::event::AppMsg::ChatError(e));
                            return;
                        }
                        let _ = tx.send(crate::event::AppMsg::ChatDebug(format!(
                            "stream retry {stream_retries}/3: {e}"
                        )));
                        // If an assistant message was pushed to req (we got tool calls),
                        // pop it so the retry starts clean.
                        if !tool_calls.is_empty() {
                            req.messages.pop(); // assistant
                        }
                        continue;
                    }
                }

                if tool_calls.is_empty() && !assistant_content.is_empty() {
                    let parsed = parse_text_tool_calls(&assistant_content);
                    if !parsed.is_empty() {
                        tool_calls = parsed;
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

                let _ = tx.send(crate::event::AppMsg::ChatTimerPause);
                for call in &tool_calls {
                    let title = tool_result_title(call);
                    let _ = tx.send(crate::event::AppMsg::ToolStarted {
                        name: call.name.clone(),
                        title: title.clone(),
                    });
                    let result = crate::tools::execute_tool_streaming_with_agent(
                        agent,
                        &call.name,
                        &call.arguments,
                        &cwd,
                        tx.clone(),
                    )
                    .await;
                    req.messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: result.clone(),
                        tool_call_id: Some(call.id.clone()),
                        tool_calls: Vec::new(),
                    });
                    let display_text = parse_tool_result_display(&result);
                    let _ = tx.send(crate::event::AppMsg::ChatToolResult {
                        name: call.name.clone(),
                        title,
                        content: display_text,
                    });
                }
                // If the model emitted an interaction tool (plan or
                // ask), stop the auto-continue loop and let the user
                // respond. The plan agent surfaces the question in the
                // session; the user types the answer in the input
                // prompt and the conversation resumes.
                let has_interaction_tool = tool_calls
                    .iter()
                    .any(|c| c.name == "plan" || c.name == "ask");
                if has_interaction_tool {
                    let _ = tx.send(crate::event::AppMsg::ChatDone);
                    return;
                }
                let _ = tx.send(crate::event::AppMsg::ChatTimerResume);
            }
            // unreachable
        });
    }
}

/// Extract the human-readable display content from a tool result JSON string.
/// Strips the `{"ok":true,"result":"..."}` wrapper to show just the inner content.
fn parse_tool_result_display(result: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(result) {
        if val.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            val.get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        } else {
            val.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or(result)
                .to_string()
        }
    } else {
        result.to_string()
    }
}

fn tool_result_title(call: &ToolCall) -> String {
    if call.name == "shell_command" || call.name == "command" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(command) = val.get("command").and_then(|v| v.as_str()) {
                return format!("$ {}", command.trim());
            }
        }
    }
    if call.name == "python_command" {
        return "python".to_string();
    }
    if call.name == "plan" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(title) = val.get("title").and_then(|v| v.as_str()) {
                if !title.trim().is_empty() {
                    return format!("Plan: {}", title.trim());
                }
            }
        }
        return "Plan".to_string();
    }
    if call.name == "ask" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(q) = val.get("question").and_then(|v| v.as_str()) {
                let q = q.trim();
                if !q.is_empty() {
                    return format!("Ask: {}", q);
                }
            }
        }
        return "Ask".to_string();
    }

    format!("[tool:{}]", call.name)
}

/// Fallback: parse text-based tool call descriptions from assistant
/// content when the model did not emit structured tool_calls.
/// Looks for JSON objects `{"name": "...", "arguments": {...}}` in
/// the text and returns valid tool calls found.
fn parse_text_tool_calls(content: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut search_start = 0;
    let bytes = content.as_bytes();
    while search_start < bytes.len() {
        // Find the next '{'
        let brace = match content[search_start..].find('{') {
            Some(i) => search_start + i,
            None => break,
        };
        // Match braces to find the full JSON object
        let mut depth: u32 = 0;
        let mut end = brace;
        for (i, ch) in content[brace..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = brace + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        if depth != 0 {
            break;
        }
        let candidate = &content[brace..end];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate) {
            let name = v.get("name").and_then(|n| n.as_str());
            let args = v.get("arguments");
            if let (Some(name), Some(args)) = (name, args) {
                if crate::tools::is_valid_tool(name) {
                    let args_str = if let Some(s) = args.as_str() {
                        s.to_string()
                    } else {
                        serde_json::to_string(args).unwrap_or_default()
                    };
                    calls.push(ToolCall {
                        id: format!("text_{}", calls.len()),
                        name: name.to_string(),
                        arguments: args_str,
                    });
                }
            }
        }
        search_start = end;
    }
    calls
}
