mod chat;
mod utils;
#[cfg(test)]
mod tests;

pub use chat::{send_chat, send_message, run_chat_stream, run_compaction_stream};
pub use utils::extract_partial_json_field;
use crate::app::App;
use crate::config::parse_id;
use crate::function::notifications::ToastLevel;
use crate::function::SidebarTab;
use crate::session::{Message, Role};

pub(crate) const MSG_REQUEST_IN_FLIGHT: &str = "request in flight, please wait";
pub(crate) const MSG_MCP_NOT_INIT: &str = "mcp service not initialised";
pub(crate) const MSG_PROVIDER_INVALID: &str = "active provider id invalid";

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
                app.notify(ToastLevel::Info, "mode: yolo");
            } else if arg.is_empty() {
                app.set_mode(crate::function::AppMode::Plan);
                app.notify(
                    ToastLevel::Info,
                    "mode: plan (read-only — use /yolo to switch back)",
                );
            } else {
                app.notify(
                    ToastLevel::Fail,
                    "unknown plan command: use /plan or /plan exit",
                );
            }
        }
        "yolo" | "build" => {
            app.set_mode(crate::function::AppMode::Yolo);
            app.notify(ToastLevel::Info, "mode: yolo");
        }
        "quit" | "exit" | "q" => {
            app.should_quit = true;
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
        app.notify(ToastLevel::Warn, MSG_REQUEST_IN_FLIGHT);
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
        app.notify(ToastLevel::Fail, MSG_MCP_NOT_INIT);
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
        app.notify(ToastLevel::Fail, MSG_MCP_NOT_INIT);
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
        app.notify(ToastLevel::Warn, MSG_REQUEST_IN_FLIGHT);
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
        app.notify(ToastLevel::Warn, MSG_REQUEST_IN_FLIGHT);
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
            app.notify(ToastLevel::Fail, MSG_PROVIDER_INVALID);
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
    let Some((mut start, end)) = plan else {
        app.notify(ToastLevel::Fail, "session is too short to compact");
        return;
    };
    let adjusted = crate::compaction::trim_to_size(
        &app.session.messages,
        start,
        end,
        crate::compaction::MAX_COMPACTION_PROMPT_CHARS,
    );
    if adjusted > start {
        app.notify(
            ToastLevel::Info,
            format!("trimming {} oldest messages to fit compaction limit", adjusted - start),
        );
        start = adjusted;
    }
    if start >= end {
        app.notify(ToastLevel::Fail, "compaction prompt too large — try a shorter session");
        return;
    }
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
        started_at: std::time::Instant::now(),
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
/// Build a string containing the content of all enabled agents.md files.
pub(super) fn build_agents_content(app: &App) -> String {
    let mut out = String::new();
    for (path, &enabled) in &app.config.agents.entries {
        if !enabled {
            continue;
        }
        if let Ok(body) = std::fs::read_to_string(path) {
            if !body.trim().is_empty() {
                out.push_str(&format!("\n\n## User instructions from {}\n\n{}\n", path, body));
            }
        }
    }
    out
}

pub(super) fn system_prompt(agent: crate::permission::Agent, agents_content: &str) -> String {
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
You are an AI coding assistant with access to the tools listed below. Your job is to \
complete software engineering tasks efficiently and correctly.

## Output discipline (token budget)

You MUST minimize output tokens as much as possible while remaining helpful and accurate. \
Answer concisely with fewer than 4 lines unless the user asks for detail. One-word answers \
are best. Avoid introductions, conclusions, and explanations. Do NOT emit text before or \
after your response such as \"Here is the content of the file...\" or \"Based on the \
information provided...\". After working on a file, just stop — do NOT summarize what you \
did unless asked. DO NOT add comments to code unless explicitly asked. Prefer tool calls \
over prose narration.

You operate in the following environment:
- Current date: {date}
- OS: {os}
- Shell: {shell} ({shell_details})
- Workspace: {workspace}

All file paths are relative to the workspace unless noted otherwise. Use `list`, `grep`, \
and `glob` to discover files — never invent or guess paths.

{skills}

## Tool usage

You communicate with the workspace through these tools. When a task requires one, you \
MUST invoke it via the API's structured `tool_calls` mechanism. Never describe a tool \
call in prose — actually call it. If your API does not support structured tool_calls, \
emit each call as a single-line JSON object on its own line:

  >>> {{\"name\": \"tool_name\", \"arguments\": {{...}}}} <<<

Do NOT claim a tool was used unless you actually see its result. Do NOT invent tool \
output — always wait for the real result.

### read(path, start_line?, end_line?)

Read a file from the workspace. When reading a file you've never seen before, start \
without line limits to understand the full context. For large files you already \
understand, use `start_line` and `end_line` to focus on the relevant section. Call this \
tool in parallel (multiple calls in one turn) when you know there are multiple files to \
read. Avoid tiny repeated slices (e.g. 30-line chunks) — if you need more context, read \
a larger window in one call rather than re-reading several times.

### edit(path, content, oldString?, replaceAll?, start_line?, end_line?)

Perform exact string replacements in a file. `oldString` must match the file content \
exactly (including indentation and whitespace). The edit will FAIL if `oldString` is not \
found, and will fail with a multiple-match error if it matches more than one location — \
in that case provide a larger `oldString` with more surrounding context to make the match \
unique, or set `replaceAll` to replace every occurrence. You MUST read the file first \
before editing it. ALWAYS prefer editing existing files over creating new ones.


would suffice — prefer surgical edits over full rewrites.

### shell_command(command)

Execute a command in {shell}. Shell guidance: {shell_details}

Important rules:
- Use `&&` to chain commands that must succeed sequentially.
- Use `;` only when you don't care if earlier commands fail.
- Quote paths containing spaces with double quotes.
- Do NOT use `cd` — use the `workdir` parameter or pass the full path directly.
- Avoid aliases (e.g. use `Get-ChildItem` not `ls` on Windows).
- Commands time out after 300 seconds.

### python_command(code)

Run Python source code directly. Use for computations, file inspection, data \
processing, or anything better done in Python than shell. Timeout is 300 seconds.

### grep(pattern, path?)

Search file contents with a regular expression. Use this to find function definitions, \
usage sites, error messages, or configuration keys. `pattern` supports full regex \
syntax. `path` can be a directory or file pattern (e.g. `\"src/**/*.rs\"`).

### glob(pattern, path?)

Find files by name pattern. Supports glob patterns like `\"**/*.ts\"` or `\"src/**/*.rs\"`. \
Results are sorted by modification time (newest first).

### list(path?)

List files and directories under a path. Useful for exploring project structure.

### plan(title?, content, steps?)

Present a plan for user confirmation. The plan is shown in the session; the user can \
approve, reject, or close it. Use this when the task is complex or destructive and \
you want confirmation before executing.

### ask(question, options?)

Ask the user a clarifying question. Use when the task is ambiguous, a tradeoff needs \
a decision, or you're blocked on missing information. Batch independent questions into \
one call. The user's answer appears as the next chat message.

### todowrite(todos)

Create and maintain a structured task list for the current coding session. Tracks \
progress, organizes multi-step work, and surfaces status to the user.

Mandatory usage rules:
1. Every turn: before finishing your response, call `todowrite` once with the full \
current list so the user sees up-to-date status. Do not skip a turn.
2. Update on completion: the moment a single todo item is done (or its status \
changes), immediately call `todowrite` with the updated full list.
3. Clear when all done: when every item is `completed`, call `todowrite` with an \
empty `todos` array `[]` to clear the list; the todo tab closes automatically.
4. Always send ALL items (existing + new/changed) in each call — never send a diff.

### skill(name)

Load a skill's instructions. Skills provide specialized workflows and domain knowledge.

### webfetch(url, format?, timeout?)

Fetch a web page and return its content as text, markdown, or HTML (default \
markdown). `timeout` is seconds (max 120). Use for reading documentation, API \
references, or any public web resource relevant to the task.

### websearch(query, numResults?, livecrawl?, type?, contextMaxCharacters?)

Search the web for information. Use when you need up-to-date knowledge beyond your \
training data, or when the task references technologies or APIs you're unsure about.

### sub_agent(description, prompt, subagent_type, task_id?)

Delegate a complex, multi-step subtask to a sub-agent. The sub-agent runs independently \
and returns a single result. Use `\"general\"` for broad tasks and `\"explore\"` for \
codebase search/analysis. The sub-agent cannot spawn further sub-agents.

## Workflow

When you receive a task, follow this general pattern:

1. **Understand** — If the task references code, use `read` and `grep` to ground \
your understanding in the actual codebase. Do NOT guess file paths, function names, \
or behaviour.

2. **Plan** — For complex tasks, call `plan` to present your approach before writing \
code. For simple, well-understood fixes, you may skip this.

3. **Execute** — Make surgical changes with `edit`. Only create new files when \
necessary. Run verification commands (`shell_command` for builds/tests) after changes.

4. **Verify** — Run relevant tests, linters, or type-checkers to confirm your changes \
are correct. If the task mentioned specific verification commands, run them.

## Code conventions

- BEFORE writing code, inspect the surrounding files to understand the project's \
conventions: naming, formatting, library choices, and patterns.
- NEVER assume a library is available unless you've confirmed it in the project's \
dependency manifest (Cargo.toml, package.json, etc.).
- Use the same libraries, frameworks, and patterns already present in the codebase.
- Follow existing code style: indentation, quoting, error handling, import ordering.

## Tone and style

- Keep responses short and direct. Aim for 1-3 sentences when possible.
- Skip preamble, greetings, and explanations — get straight to the point.
- Do not summarize what you already did or what you are about to do.
- Only elaborate when the user explicitly asks for detail.
- Do NOT add comments to code unless the user explicitly asks or the codebase \
convention demands it.

## Language

- Respond in the same language as the user's first prompt. If the user explicitly \
requests a different language in a later message, switch to that language.
{agents}",
            date = date,
            workspace = cwd,
            os = os,
            shell = shell,
            shell_details = crate::tools::shell_guidance(),
            skills = crate::skill::skills_for_system_prompt(),
            agents = agents_content,
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

  - read(path, start_line?, end_line?)
  - grep(pattern, path?) — search text in files
  - glob(pattern, path?) — find files by name pattern
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
  - skill(name) — load a skill's instructions and resources

## What you must NOT do

The runtime will reject (with an error) any attempt to:

  - edit (no file edits)
  - write (no file creation)
  - shell_command (no arbitrary shell)
  - python_command (no code execution)
  - webfetch (no web fetching)
  - websearch (no web searching)

If a task truly requires running a command or mutating a file, hand it back \
to the user — they can switch to yolo mode with `/yolo` and re-send. Do \
not pretend to invoke these tools; never claim a tool ran unless you saw \
its result.

## Important

1. **Always use the structured tool_calls API** (or the `>>> {{\"name\":...}} \
   <<<` text fallback). Never describe a plan in prose without actually \
   calling the `plan` tool — the user only sees your plan when the tool \
   result is rendered.
2. **Explore before you plan.** If the request touches code you have not \
   read, use read/grep/list to ground the plan in the actual \
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
{skills}
{agents}",
            date = date,
            workspace = cwd,
            os = os,
            shell = shell,
            skills = crate::skill::skills_for_system_prompt(),
            agents = agents_content,
        ),
    }
}
