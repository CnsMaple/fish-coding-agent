use crate::app::App;
use crate::config::{parse_id, ProviderKind, ProviderMode};
use crate::function::notifications::ToastLevel;
use crate::function::SidebarTab;
use crate::providers::{ChatMessage, ChatRequest, ToolCall};
use crate::session::{Message, Role};
use std::time::Duration;

pub fn dispatch(app: &mut App, cmd: &str, arg: &str) {
    match cmd {
        "settings" => open_settings(app),
        "model" => open_model_picker(app),
        "compact" => compact_now(app, arg),
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
                "minimal" => ReasoningMode::Minimal,
                "low" => ReasoningMode::Low,
                "med" | "medium" => ReasoningMode::Medium,
                "high" => ReasoningMode::High,
                "xhigh" => ReasoningMode::XHigh,
                "adaptive" => ReasoningMode::Adaptive,
                "max" => ReasoningMode::Max,
                _ => {
                    app.notify(
                        ToastLevel::Fail,
                        format!("unknown thinking level: {arg} (off/minimal/low/medium/high/xhigh/adaptive/max)"),
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
        "mcp-auth" => open_mcp_auth(app, arg),
        "mcp-logout" => open_mcp_logout(app, arg),
        "mcp-debug" => open_mcp_debug(app, arg),
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
        if names.is_empty() {
            app.notify(
                ToastLevel::Warn,
                "no MCP servers configured. add to config.json `mcp` section, then restart.",
            );
            return;
        }
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
        Some(_server_name) => {
            let status_label = if let Some(svc) = crate::mcp::McpRegistry::current() {
                let s = svc.status_of_sync(name).ok();
                s.as_ref()
                    .map(|st| {
                        let base = format!("{} — {}", st.icon(), st.label());
                        match st {
                            crate::mcp::McpStatus::Failed { error } => {
                                format!("{base}: {error}")
                            }
                            _ => base,
                        }
                    })
                    .unwrap_or_else(|| "configured".to_string())
            } else {
                "configured".to_string()
            };
            app.notify(
                ToastLevel::Ok,
                format!("mcp '{name}' is {status_label}"),
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

/// `/mcp-auth <name>` — start the OAuth flow for a remote MCP
/// server. Opens a local callback server, constructs the
/// authorization URL, opens the browser, and waits for the
/// redirect. On success, stores the token and re-connects the
/// server.
fn open_mcp_auth(app: &mut App, arg: &str) {
    let name = arg.trim();
    if name.is_empty() {
        app.notify(ToastLevel::Fail, "usage: /mcp-auth <server-name>");
        return;
    }
    if crate::mcp::McpRegistry::current().is_none() {
        app.notify(ToastLevel::Fail, "mcp service not initialised");
        return;
    }
    // Delegate to the async handler via the event channel.
    let tx = match &app.msg_tx {
        Some(tx) => tx.clone(),
        None => {
            app.notify(ToastLevel::Fail, "no event channel available");
            return;
        }
    };
    let _ = tx.send(crate::event::AppMsg::McpStartAuth { server: name.to_string() });
    app.notify(
        ToastLevel::Info,
        format!("starting OAuth for `{name}`... (see next notification)"),
    );
}

/// `/mcp-debug <name>` — print diagnostics for a server: status,
/// auth state, tool count, and config preview.
fn open_mcp_debug(app: &mut App, arg: &str) {
    let name = arg.trim();
    if name.is_empty() {
        app.notify(ToastLevel::Fail, "usage: /mcp-debug <server-name>");
        return;
    }
    let Some(svc) = crate::mcp::McpRegistry::current() else {
        app.notify(ToastLevel::Fail, "mcp service not initialised");
        return;
    };
    let status = svc.status_of_sync(name).ok();
    let auth = crate::mcp::auth::McpAuthStore::load_or_default();
    let has_tokens = auth.get(name).is_some();
    let snap = svc.try_snapshot().ok();
    let tool_count = snap
        .as_ref()
        .map(|s| s.tools.values().filter(|t| t.server == name).count())
        .unwrap_or(0);
    let mut lines = vec![format!("MCP server: {name}")];
    let status_str = match status.as_ref() {
        Some(crate::mcp::McpStatus::Failed { error }) => {
            format!("✗ failed: {error}")
        }
        Some(s) => format!("{} {}", s.icon(), s.label()),
        None => "unknown".to_string(),
    };
    lines.push(format!("  status: {status_str}"));
    lines.push(format!("  has stored tokens: {has_tokens}"));
    lines.push(format!("  tool count: {tool_count}"));
    if let Some(crate::mcp::McpStatus::Connected) = status.as_ref() {
        lines.push("  ✓ ready to receive tool calls".to_string());
    }
    app.notify(ToastLevel::Info, lines.join(" | "));
}

/// `/mcp-logout <name>` — remove stored OAuth tokens for a
/// remote MCP server.
fn open_mcp_logout(app: &mut App, arg: &str) {
    let name = arg.trim();
    if name.is_empty() {
        app.notify(ToastLevel::Fail, "usage: /mcp-logout <server-name>");
        return;
    }
    // Remove from auth store.
    let auth = crate::mcp::auth::McpAuthStore::load_or_default();
    auth.remove(name);
    app.notify(ToastLevel::Ok, format!("OAuth tokens removed for `{name}`"));
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
    crate::commands::send_chat(app, prompt, Vec::new());
}

fn continue_response(app: &mut App, arg: &str) {
    if app.inflight.is_some() {
        app.notify(ToastLevel::Warn, "request in flight, please wait");
        return;
    }
    // Sent to the model, never shown in the session — we strip the
    // user message out of `session.messages` right below. An empty
    // user message confuses most providers (some reject it, others
    // stall waiting for real input), so always feed the model an
    // explicit continuation cue. If the user typed `/continue foo`
    // we just append their note to the cue.
    let prompt = if arg.is_empty() {
        "Continue from where you left off.".to_string()
    } else {
        format!("Continue from where you left off.\n\n{arg}")
    };
    crate::commands::send_chat(app, prompt, Vec::new());
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

/// Manually trigger a session compaction. `/compact` ignores the
/// `auto_compact` setting (the user asked for it explicitly) and
/// always runs the summary flow. We still refuse to start while a
/// chat request is in flight so the live session is not
/// concurrently mutated.
pub fn compact_now(app: &mut App, _arg: &str) {
    use crate::function::notifications::ToastLevel;
    if app.inflight.is_some() {
        app.notify(
            ToastLevel::Fail,
            "cannot compact while a request is in flight",
        );
        return;
    }
    if app.compacting {
        app.notify(ToastLevel::Fail, "compaction already in progress");
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
        return;
    }
    let (provider, _mode) = match crate::config::parse_id(&active_id) {
        Some(p) => p,
        None => {
            app.notify(ToastLevel::Fail, "active provider id invalid");
            return;
        }
    };
    if app.session.messages.is_empty() {
        app.notify(ToastLevel::Fail, "session is empty — nothing to compact");
        return;
    }
    // Try the conservative plan first (preserves `tail_turns` of
    // recent context). If there is not enough history for that
    // (e.g. the session has only 1–2 turns), fall back to a
    // full-session summary so `/compact` always does something
    // useful for the user.
    let plan = crate::compaction::plan_cutoff(
        &app.session.messages,
        crate::compaction::DEFAULT_TAIL_TURNS,
    )
    .or_else(|| crate::compaction::plan_cutoff_force(&app.session.messages));
    let Some((start, end)) = plan else {
        app.notify(ToastLevel::Fail, "session is too short to compact");
        return;
    };
    let history: Vec<crate::session::Message> = app.session.messages[start..end].to_vec();
    let key = match app.config.effective_api_key(&active_id) {
        Some(k) if !k.is_empty() => k,
        _ => {
            app.notify(
                ToastLevel::Fail,
                format!("missing api key for {active_id}"),
            );
            return;
        }
    };
    let base = app
        .config
        .entry(&active_id)
        .map(|c| c.base_url.clone())
        .unwrap_or_default();
    let model = app.config.active_model().to_string();
    let client = app.stream_client.clone();
    let tx = match app.msg_tx.clone() {
        Some(tx) => tx,
        None => {
            app.notify(ToastLevel::Fail, "internal: msg channel closed");
            return;
        }
    };
    app.compacting = true;
    app.status.mark_compact_triggered();
    app.notify(ToastLevel::Info, "compacting session...");
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    // Stash the cancel sender in `inflight` so the existing Esc-cancel
    // UI (which flips `inflight.cancel` to true) also cancels an
    // active compaction. This re-uses the existing field; we
    // distinguish the two via `compacting` so a chat cancel won't
    // also clobber a separate inflight later.
    app.inflight = Some(crate::app::InflightHandle {
        cancel: cancel_tx,
        label: format!("compact:{active_id}:{model}"),
        seq: app.current_request_seq,
    });
    app.cancel_state = crate::function::CancelState::Idle;
    tokio::spawn(run_compaction_stream(
        client, base, key, provider, model, history, cancel_rx, tx, start, end,
    ));
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
    app.show_panel();
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
        app.show_panel();
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
        app.show_panel();
        app.acknowledge_panel();
        return;
    }
    let mut state = crate::function::ModelPickerState::new(provider);
    if let Some(c) = app.model_cache.get(provider) {
        state.models = c.models.clone();
        state.rebuild_filter();
    }
    app.function.push(SidebarTab::ModelPicker(state));
    app.show_panel();
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
    app.show_panel();
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
    app.show_panel();
    app.acknowledge_panel();
}

pub fn open_thinking_picker(app: &mut App) {
    app.function.push(SidebarTab::ThinkingPicker(
        crate::function::ThinkingPickerState::new(),
    ));
    app.show_panel();
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
    app.show_panel();
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
    app.show_panel();
    app.acknowledge_panel();
}

pub fn open_session_rename(app: &mut App, target_id: Option<String>, title: String) {
    app.function
        .push(SidebarTab::SessionRename(match target_id {
            Some(id) => crate::function::SessionRenameState::new_target(id, title),
            None => crate::function::SessionRenameState::new_current(&title),
        }));
    app.show_panel();
    app.acknowledge_panel();
}

/// System prompt instructing the model about available tools.
/// Stresses using the structured tool_calls API, and provides a
/// text-based fallback format for providers that don't support it.
fn system_prompt(agent: crate::permission::Agent) -> String {
    let now = chrono::Local::now();
    let date = now.format("%Y-%m-%d %A").to_string();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let os = crate::tools::os_name();
    let shell = crate::tools::shell_description();
    match agent {
        crate::permission::Agent::Build => format!(
            "\
You are a coding assistant with access to the following tools. All file paths are relative to the current workspace directory; use `list` and `grep` to discover files within it. Never invent or guess file paths from outside the current workspace:

Current date: {date}
Current workspace: {workspace}
Current OS: {os}, shell: {shell}

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

Do NOT claim a tool was used unless you actually see its result.

## Tone and style
- Keep responses short and direct. Aim for 1-3 sentences when possible.
- Skip preamble, greetings, and explanations — get straight to the point.
- Do not summarize what you already did or what you are about to do.
- Only elaborate when the user explicitly asks for detail.

## Language
- Respond in the same language as the user's first prompt. If the user explicitly requests a different language in a later message, switch to that language.
{skills}",
            date = date,
            workspace = cwd,
            os = os,
            shell = shell,
            shell_details = crate::tools::shell_guidance(),
            skills = crate::skill::skills_for_system_prompt(),
        ),
        crate::permission::Agent::Plan => format!(
            "\
## Responsibility

You are operating in **plan mode**, a read-only research and planning role. \
Your job is to understand the user's task, gather only the evidence you need, \
and present a concrete plan the user can approve before any code is written.

Current date: {date}
Current workspace: {workspace}
Current OS: {os}, shell: {shell}

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
    wait for the user's decision.
6. **Handle interruptions directly.** If the user interrupts you and asks a \
    follow-up question (e.g. translation, clarification, summary), answer \
    using the information you already have. Do not re-explore the codebase \
    or call `plan` again.
{skills}",
            date = date,
            workspace = cwd,
            os = os,
            shell = shell,
            skills = crate::skill::skills_for_system_prompt(),
        ),
    }
}
pub fn send_chat(app: &mut App, user_text: String, image_parts: Vec<crate::session::ContentPart>) {
    let mut msg = Message::new(Role::User, user_text);
    // Extract ImageAttachment from ContentPart to store on the Message.
    for part in &image_parts {
        if let crate::session::ContentPart::Image(att) = part {
            msg.attachments.push(att.clone());
        }
    }
    send_message(app, msg);
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
        attachments: Vec::new(),
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
            content_parts: Vec::new(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        })
        .collect();
    // Bump the request generation. Anything stale from a previous
    // request that slips through after we re-enter (an old chat
    // task still draining, a queued `ChatDone`/`ChatError` from
    // before Esc cleared the inflight, etc.) carries the OLD seq
    // and is filtered out in `handle_msg`.
    app.current_request_seq = app.current_request_seq.wrapping_add(1);
    let seq = app.current_request_seq;

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    app.inflight = Some(crate::app::InflightHandle {
        cancel: cancel_tx,
        label: format!("chat:{active_id}:{model}"),
        seq,
    });
    app.cancel_state = crate::function::CancelState::Idle;

    let req = ChatRequest {
        model,
        messages,
        thinking,
        system: Some(system_prompt(app.active_agent)),
    };

    if let Some(tx) = app.msg_tx.clone() {
let client = app.stream_client.clone();
    let cwd = app.cwd.clone();
    let agent = app.active_agent;
    // Defer the actual `tokio::spawn` until after the next
    // `terminal.draw(...)` returns, so the freshly-pushed user
    // message is on screen before the HTTP request goes out. The
    // main event loop pulls this in `flush_pending_request`.
        app.pending_request = Some(crate::function::PendingRequest::Chat(
            crate::function::ChatPending {
                client,
                base,
                key,
                req,
                provider,
                cwd,
                agent,
                cancel_rx,
                tx,
                seq,
            },
        ));
    }
}

/// Run the chat stream retry loop. Extracted from `send_message` so
/// the same body can be invoked both inline (legacy path) and from
/// `event::flush_pending_request` after the user message has been
/// rendered.
///
/// `req` is consumed and may be mutated across retries (the assistant
/// turn is appended after a successful tool-call round, then popped
/// on a retry so the new request starts clean).
///
/// `seq` is `App::current_request_seq` at the time the request was
/// prepared. It's stamped onto the final `ChatDone`/`ChatError` so
/// `handle_msg` can tell a freshly-completed request from a stale
/// `ChatDone` left over from a previously cancelled request. While
/// the request is running we also gate every `tx.send(...)` on
/// `cancel_rx`: once the user hits Esc we no longer want any of
/// these events to mutate the new state (a partial `Delta`
/// landing in the new assistant message, or worse, a `ChatDone`
/// clearing the freshly-armed inflight during `/continue`).
#[allow(clippy::too_many_arguments)]
pub async fn run_chat_stream(
    client: reqwest::Client,
    base: String,
    key: String,
    mut req: ChatRequest,
    provider: ProviderKind,
    agent: crate::permission::Agent,
    cwd: std::path::PathBuf,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    tx: tokio::sync::mpsc::UnboundedSender<crate::event::AppMsg>,
    seq: u64,
) {
    // Wrap every outbound AppMsg in a cancel check so a stale chat
    // task cannot race with a follow-up request. See the field-level
    // comments in `run_chat_stream` above and `App::current_request_seq`.
    let send_msg = |msg: crate::event::AppMsg| {
        if !*cancel_rx.borrow() {
            let _ = tx.send(msg);
        }
    };
    let mut stream_retries = 0u32;
    let retry_delays = [3u64, 12, 60];
    loop {
        if *cancel_rx.borrow() {
            // Silent exit. We do NOT send `ChatDone` / `ChatError`
            // here — the Esc handler already cleared local state and
            // `seq` will reject any leftover event from this task if
            // a new request takes over.
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
            if *cancel_rx.borrow() {
                // Drop the event we just received and exit. The next
                // chat_rx.recv() would block forever on the dead
                // http stream anyway; returning here lets the
                // background `call` task finish on its own.
                return;
            }
            match ev {
                crate::providers::ChatEvent::Delta(s) => {
                    assistant_content.push_str(&s);
                    send_msg(crate::event::AppMsg::ChatDelta(s));
                }
                crate::providers::ChatEvent::ThinkingDelta(s) => {
                    send_msg(crate::event::AppMsg::ChatThinkingDelta(s));
                }
                crate::providers::ChatEvent::Debug(s) => {
                    send_msg(crate::event::AppMsg::ChatDebug(s));
                }
                crate::providers::ChatEvent::Usage(u) => {
                    send_msg(crate::event::AppMsg::ChatUsage(u));
                }
                crate::providers::ChatEvent::ToolResult {
                    name,
                    title,
                    content,
                } => {
                    send_msg(crate::event::AppMsg::ChatToolResult {
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
                    send_msg(crate::event::AppMsg::ChatError { seq, error: e });
                    return;
                }
                crate::providers::ChatEvent::ContentBlockStart(kind) => {
                    send_msg(crate::event::AppMsg::ChatContentBlockStart(kind));
                }
            }
        }

        if !stream_done {
            let err = match call.await {
                Ok(Ok(())) => None,
                // {e:#} shows the full anyhow error chain (surface
                // message + underlying cause like reqwest transport
                // errors) so retry/failure notifications carry enough
                // context to diagnose API or network issues.
                Ok(Err(e)) => Some(format!("{e:#}")),
                Err(e) => Some(format!("chat task failed: {e:#}")),
            };
            if let Some(e) = err {
                stream_retries += 1;
                if stream_retries >= 3 {
                    // Show the full error chain in the final failure so
                    // the user sees both the surface message and its
                    // underlying cause (e.g. reqwest transport errors).
                    send_msg(crate::event::AppMsg::ChatError { seq, error: e });
                    return;
                }
                let delay = retry_delays[(stream_retries - 1) as usize];
                // Use ChatWarn (Warn level) instead of ChatDebug
                // (Info level) so retry notifications are more visible
                // in the notification list. Use {e:#} to show the full
                // error chain including the underlying cause.
                send_msg(crate::event::AppMsg::ChatWarn(format!(
                    "stream retry {stream_retries}/3 ({delay}s): {e:#}"
                )));
                // If an assistant message was pushed to req (we got tool calls),
                // pop it so the retry starts clean.
                if !tool_calls.is_empty() {
                    req.messages.pop(); // assistant
                }
                tokio::time::sleep(Duration::from_secs(delay)).await;
                continue;
            }
        }
        // Stream completed (either via Done event or graceful EOF
        // without error). Reset the retry counter so a subsequent
        // failure starts from 1/3, not from the stale count.
        stream_retries = 0;

        if tool_calls.is_empty() && !assistant_content.is_empty() {
            let parsed = parse_text_tool_calls(&assistant_content);
            if !parsed.is_empty() {
                tool_calls = parsed;
            }
        }

        if *cancel_rx.borrow() {
            // User cancelled between the inner loop draining the
            // provider's `Done` event and us trying to close it out
            // here. Stay silent.
            return;
        }

        if tool_calls.is_empty() {
            send_msg(crate::event::AppMsg::ChatDone { seq });
            return;
        }

        req.messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: assistant_content,
            content_parts: Vec::new(),
            tool_call_id: None,
            tool_calls: tool_calls.clone(),
        });

        send_msg(crate::event::AppMsg::ChatTimerPause);
        for call in &tool_calls {
            let title = tool_result_title(call);
            send_msg(crate::event::AppMsg::ToolStarted {
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
                content_parts: Vec::new(),
                tool_call_id: Some(call.id.clone()),
                tool_calls: Vec::new(),
            });
            let display_text = parse_tool_result_display(&result);
            send_msg(crate::event::AppMsg::ChatToolResult {
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
            send_msg(crate::event::AppMsg::ChatDone { seq });
            return;
        }
        send_msg(crate::event::AppMsg::ChatTimerResume);
    }
}

/// System prompt used by the compaction stream. Distinct from the
/// normal agent's prompt because the summarizer should focus on
/// preserving intent, not on producing tool calls.
fn compaction_system_prompt() -> String {
    "You are a helpful assistant that summarizes conversations. \
Produce a single concise summary that preserves every decision, \
identifier, file path, and open question from the source. Do not \
use any tools. Reply with the summary only — no preamble, no \
closing remarks."
    .to_string()
}

/// Spawn a one-shot chat stream that summarizes `history`. Used by
/// both auto-compaction and the `/compact` command. The result is
/// delivered as `AppMsg::CompactionSummaryReady { start, end, summary }`
/// (or `AppMsg::CompactionFailed { error }` on error).
///
/// `history` must already be a clone of `Session::messages[start..end]`
/// — the compactor runs entirely on the snapshot, so the live
/// session can be mutated safely in parallel. The cancel channel is
/// independent from the chat inflight handle so the existing
/// inflight-cancel UI does not interfere.
#[allow(clippy::too_many_arguments)]
pub async fn run_compaction_stream(
    client: reqwest::Client,
    base: String,
    key: String,
    provider: ProviderKind,
    model: String,
    history: Vec<crate::session::Message>,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    tx: tokio::sync::mpsc::UnboundedSender<crate::event::AppMsg>,
    start: usize,
    end: usize,
) {
    let send_msg = |msg: crate::event::AppMsg| {
        if !*cancel_rx.borrow() {
            let _ = tx.send(msg);
        }
    };
    if *cancel_rx.borrow() {
        return;
    }
    let prompt = crate::compaction::build_summary_prompt(&history);
    let req = crate::providers::ChatRequest {
        model,
        messages: vec![crate::providers::ChatMessage {
            role: "user".to_string(),
            content: prompt,
            content_parts: Vec::new(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }],
        thinking: crate::config::ReasoningMode::Off,
        system: Some(compaction_system_prompt()),
    };

    let (chat_tx, mut chat_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::providers::ChatEvent>();
    let p = crate::providers::provider(provider);
    let client_for_call = client.clone();
    let base_for_call = base.clone();
    let key_for_call = key.clone();
    let call = tokio::spawn(async move {
        p.chat_stream(
            &client_for_call,
            &base_for_call,
            &key_for_call,
            req,
            chat_tx,
        )
        .await
    });

    let mut summary = String::new();
    let mut stream_done = false;
    while let Some(ev) = chat_rx.recv().await {
        if *cancel_rx.borrow() {
            return;
        }
        match ev {
            crate::providers::ChatEvent::Delta(s) => {
                summary.push_str(&s);
            }
            crate::providers::ChatEvent::Done => {
                stream_done = true;
                break;
            }
            crate::providers::ChatEvent::Error(e) => {
                send_msg(crate::event::AppMsg::CompactionFailed { error: e });
                return;
            }
            // We do not care about thinking deltas, tool calls, etc.
            // for a compaction summary — drop them.
            _ => {}
        }
    }

    if !stream_done {
        // The provider's task ended without emitting `Done`. Treat
        // any error as a compaction failure so the user gets a
        // meaningful toast.
        let err = match call.await {
            Ok(Ok(())) => "stream closed without Done".to_string(),
            Ok(Err(e)) => format!("{e}"),
            Err(e) => format!("compaction task failed: {e}"),
        };
        send_msg(crate::event::AppMsg::CompactionFailed { error: err });
        return;
    }
    let _ = call.await;
    if summary.trim().is_empty() {
        send_msg(crate::event::AppMsg::CompactionFailed {
            error: "summary was empty".to_string(),
        });
        return;
    }
    send_msg(crate::event::AppMsg::CompactionSummaryReady {
        start,
        end,
        summary,
    });
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

if call.name == "read_file" || call.name == "write_file" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            let start = val.get("start_line").and_then(|v| v.as_u64());
            let end = val.get("end_line").and_then(|v| v.as_u64());
            match (start, end) {
                (Some(s), Some(e)) => return format!("{} [{}:{}]", call.name, s, e),
                (Some(s), None) => return format!("{} [{}:]", call.name, s),
                (None, Some(e)) => return format!("{} [{}:]", call.name, e),
                (None, None) => {}
            }
        }
    }

    if call.name == "grep" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(pattern) = val.get("pattern").and_then(|v| v.as_str()) {
                let short = pattern.trim();
                let display = if short.len() > 40 {
                    format!("{}…", &short[..40])
                } else {
                    short.to_string()
                };
                return format!("grep [{}]", display);
            }
        }
    }

    if call.name == "list" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(path) = val.get("path").and_then(|v| v.as_str()) {
                let p = path.trim();
                if !p.is_empty() {
                    return format!("list [{}]", p);
                }
            }
        }
    }

    call.name.clone()
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
