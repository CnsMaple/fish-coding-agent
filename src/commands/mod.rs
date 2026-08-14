mod chat;
mod headless;
#[cfg(test)]
mod tests;
mod utils;

use crate::app::App;
use crate::function::notifications::ToastLevel;
use crate::function::SidebarTab;
use crate::session::Role;
pub use chat::{run_chat_stream, run_compaction_stream, send_chat, send_message};
pub use headless::{run_headless_task, HeadlessOptions, HeadlessResult};
pub use utils::{extract_partial_json_field, extract_partial_json_u64};

pub(crate) const MSG_REQUEST_IN_FLIGHT: &str = "request in flight, please wait";
pub(crate) const MSG_MCP_NOT_INIT: &str = "mcp service not initialised";
pub(crate) const MSG_PROVIDER_INVALID: &str = "active provider id invalid";

/// Start the OAuth flow for a remote MCP server (invoked from the
/// command palette). Delegates to the async handler via the event
/// channel, which runs [`crate::event::mcp::run_mcp_oauth`].
pub fn open_mcp_auth_for(app: &mut App, name: &str) {
    let name = name.trim();
    if name.is_empty() {
        app.notify(ToastLevel::Fail, "mcp: no server name");
        return;
    }
    if crate::mcp::McpRegistry::current().is_none() {
        app.notify(ToastLevel::Fail, MSG_MCP_NOT_INIT);
        return;
    }
    let tx = match &app.msg_tx {
        Some(tx) => tx.clone(),
        None => {
            app.notify(ToastLevel::Fail, "no event channel available");
            return;
        }
    };
    let _ = tx.send(crate::event::AppMsg::McpStartAuth {
        server: name.to_string(),
    });
    app.notify(
        ToastLevel::Info,
        format!("starting OAuth for `{name}`... (see next notification)"),
    );
}

pub fn retry_last_prompt(app: &mut App) {
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

/// `/undo` — remove the most recently answered prompt (its user
/// message plus the assistant reply and any tool blocks) from the
/// session and archive it so `/redo` can restore it. Walks back to the
/// last real user prompt that is not a synthetic context marker.
/// Index of the last real user prompt (skips synthetic `Context from `
/// markers like the dynamic system-prompt suffix), or `None` if there
/// is no such prompt.
fn last_user_prompt_index(app: &App) -> Option<usize> {
    app.session
        .messages
        .iter()
        .rposition(|m| matches!(m.role, Role::User) && !m.content.starts_with("Context from "))
}

pub fn undo_last_response(app: &mut App) {
    if app.inflight.is_some() {
        app.notify(ToastLevel::Warn, MSG_REQUEST_IN_FLIGHT);
        return;
    }
    let Some(idx) = last_user_prompt_index(app) else {
        app.notify(ToastLevel::Warn, "nothing to undo");
        return;
    };
    // Archive everything from the prompt onward (indices are relative
    // to the current messages vec before truncation).
    let snapshot: Vec<_> = app.session.messages[idx..].to_vec();
    app.session.messages.truncate(idx);
    app.session.invalidate_message_cache_from(idx);
    app.session.invalidate_layout_cache();
    app.redo_turn_stack.clear();
    app.undo_turn_stack.push_back(snapshot);
    app.notify(ToastLevel::Info, "undo: last response removed");
}

/// `/redo` — restore the most recently undone prompt+response. Only
/// works immediately after an `/undo` (the stack is cleared the moment
/// a new prompt is sent, so stale turns cannot be re-applied).
pub fn redo_last_response(app: &mut App) {
    if app.inflight.is_some() {
        app.notify(ToastLevel::Warn, MSG_REQUEST_IN_FLIGHT);
        return;
    }
    let Some(snapshot) = app.undo_turn_stack.pop_back() else {
        app.notify(ToastLevel::Warn, "nothing to redo");
        return;
    };
    let start = app.session.messages.len();
    app.session.messages.extend(snapshot.clone());
    app.session.invalidate_message_cache_from(start);
    app.session.invalidate_layout_cache();
    app.redo_turn_stack.push_back(snapshot);
    app.notify(ToastLevel::Info, "redo: response restored");
}

pub fn continue_response(app: &mut App, arg: &str) {
    if app.inflight.is_some() {
        app.notify(ToastLevel::Warn, MSG_REQUEST_IN_FLIGHT);
        return;
    }
    // The continuation cue is sent to the model but not shown in the
    // session, so the assistant's response appears to continue the
    // interrupted turn. We still push a synthetic user message so the
    // provider gets a real prompt, then remove it from the UI log.
    let prompt = if arg.is_empty() {
        "Continue from where you left off.".to_string()
    } else {
        format!("Continue from where you left off.\n\n{arg}")
    };
    crate::commands::send_chat(app, prompt, Vec::new());
    // Remove the synthetic user message from the session log. The
    // assistant placeholder was pushed after it, so its index shifts
    // down by one; update streaming_id so deltas target the right slot.
    if app.inflight.is_some() && app.session.messages.len() >= 2 {
        let idx = app.session.messages.len() - 2;
        if app.session.messages[idx].role == Role::User {
            app.session.messages.remove(idx);
            app.session.invalidate_message_cache_from(idx);
            app.session.invalidate_layout_cache();
            app.session.streaming_id = Some(idx);
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
            "no active provider; configure one in settings",
        );
        open_settings(app);
        return;
    };
    if let Err(e) = app.config.validate_provider(&active_id) {
        app.notify(ToastLevel::Fail, e.clone());
        return;
    }
    let provider = match app.config.entry(&active_id).map(|e| e.kind) {
        Some(k) => k,
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
            format!(
                "trimming {} oldest messages to fit compaction limit",
                adjusted - start
            ),
        );
        start = adjusted;
    }
    if start >= end {
        app.notify(
            ToastLevel::Fail,
            "compaction prompt too large — try a shorter session",
        );
        return;
    }
    // Compute the kept-window boundary: messages after this index
    // are preserved verbatim after the summary, so the cache prefix
    // from before compaction partially overlaps.
    let raw_keep_start = crate::compaction::select_keep_boundary(
        &app.session.messages[start..end],
        crate::compaction::DEFAULT_KEEP_TOKENS,
    )
    .map(|offset| start + offset);
    let keep_start = match raw_keep_start {
        Some(ks) if ks > start && ks < end => ks,
        _ => end,
    };
    let history: Vec<crate::session::Message> = app.session.messages[start..keep_start].to_vec();
    let key = match app.config.effective_api_key(&active_id) {
        Some(k) if !k.is_empty() => k,
        _ => {
            app.notify(ToastLevel::Fail, format!("missing api key for {active_id}"));
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
        client, base, key, provider, model, history, cancel_rx, tx, start, end, keep_start,
    ));
}

/// Open a fresh Settings tab and jump to `initial_level`. Used by
/// `open_model_picker` so the user lands directly on ProviderList (skipping
/// the redundant TopLevel) when they are routed here because no model is
/// configured.
pub fn open_settings_at(app: &mut App, initial_level: crate::function::SettingsLevel) {
    let cache_parent = app
        .model_cache_path
        .parent()
        .unwrap_or(&app.model_cache_path);
    let mut state = crate::function::SettingsState::with_cache(&app.config, Some(cache_parent));
    state.level = initial_level;
    state.clamp_cursor(&app.config);
    app.function.push(SidebarTab::Settings(Box::new(state)));
    app.show_panel();
    app.acknowledge_panel();
}

pub fn open_model_picker(app: &mut App) {
    // The model picker is a two-step flow: first pick a configured provider
    // entry (by name, not just by kind), then pick a model for that
    // entry's kind. If the user has only one entry we skip straight to
    // the model list. If they have none, route to the settings panel like
    // before.

    // Count configured entries (one per row in the picker). The picker
    // shows one row per entry — not per kind — so multiple entries of
    // the same kind (e.g. "prod-openai" and "dev-openai") each get
    // their own line.
    let entry_count = app.config.entries.len();

    // If a ModelPicker is already open, just focus it — the user is
    // continuing from where they left off. They can Esc out and reopen
    // it (Ctrl+P → model) if they want to switch providers.
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
                "no active provider; configure one in settings",
            );
            // Land on ProviderList directly (skip TopLevel's "set provider"
            // step) so the user can pick a kind/mode right away.
            open_settings_at(app, crate::function::SettingsLevel::ProviderList);
        }
        1 => {
            // Only one configured entry — skip the chooser and jump
            // straight to that entry's model list.
            if let Some(id) = app.config.entries.first().map(|e| e.id.clone()) {
                open_model_picker_for_entry(app, &id);
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
/// Used by the two-step model flow after the user has chosen a
/// provider. For callers that know the exact configured entry,
/// prefer `open_model_picker_for_entry` (see its doc).
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
    let state = crate::function::ModelPickerState::new(provider);
    // NOTE: cannot use entry_id-based cache without knowing which entry.
    // The picker starts empty and will fetch on first Ctrl+R.
    app.function.push(SidebarTab::ModelPicker(state));
    app.show_panel();
    app.acknowledge_panel();
}

/// Open (or focus) a ModelPicker bound to a specific configured entry
/// id. Prefer this over `open_model_picker_for_kind` whenever the
/// caller knows the exact entry: multiple entries can share a kind
/// (e.g. two OpenAI endpoints), so resolving credentials/commits by
/// kind alone would hit the wrong one. Uses the per-entry-id model
/// cache so each endpoint's model list is cached independently.
pub fn open_model_picker_for_entry(app: &mut App, entry_id: &str) {
    use crate::config::ProviderKind;
    let Some(kind) = app.config.entry(entry_id).map(|e| e.kind) else {
        return;
    };
    let mut state = crate::function::ModelPickerState::new_for_entry(kind, entry_id);
    let provider = state.provider;
    // Dedup by the exact entry id, not just the kind — two pickers for
    // different same-kind entries are distinct tabs.
    if let Some(idx) = app.function.tabs.iter().position(|t| {
        matches!(t, SidebarTab::ModelPicker(s)
            if s.entry_id.as_deref() == Some(entry_id))
    }) {
        app.function.active = idx;
        app.show_panel();
        app.acknowledge_panel();
        return;
    }
    // Cursor never has a model list endpoint; skip cache lookup.
    if provider != ProviderKind::Cursor {
        if let Some(c) = app.model_cache.get(entry_id) {
            state.models = c.models.clone();
            state.rebuild_filter();
        }
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
    let mut state = crate::function::ThinkingPickerState::new();
    // Pre-select the current reasoning strength so the user sees which
    // one is active.
    let current = app.config.thinking.as_str();
    if let Some(fi) = state
        .filtered
        .iter()
        .position(|&i| crate::function::ThinkingPickerState::LEVELS[i] == current)
    {
        state.cursor = fi;
    }
    app.function.push(SidebarTab::ThinkingPicker(state));
    app.show_panel();
    app.acknowledge_panel();
}

pub fn open_tool_picker(app: &mut App) {
    app.function.push(SidebarTab::ToolPicker(
        crate::function::ToolPickerState::new(&app.disabled_tools),
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

/// Build a string containing the content of all enabled agents.md
/// files, each prefixed by its own "## User instructions from <path>"
/// header so the model can tell them apart.
pub(super) fn build_agents_content(app: &App) -> String {
    let mut out = String::new();
    for (path, &enabled) in &app.config.agents.entries {
        if !enabled {
            continue;
        }
        if let Ok(body) = std::fs::read_to_string(path) {
            if !body.trim().is_empty() {
                out.push_str(&format!(
                    "\n\n## User instructions from {}\n\n{}\n",
                    path, body
                ));
            }
        }
    }
    out
}

/// Build the dynamic suffix for the system prompt: date, OS, shell,
/// and workspace path. These change between or even during sessions,
/// so keeping them separate from the static core avoids invalidating
/// the prefix cache on every request.
pub(super) fn system_prompt_dynamic(mode: crate::function::AppMode) -> String {
    system_prompt_dynamic_full("", &[], false, mode)
}

/// Build the dynamic suffix for the system prompt. On top of the
/// date/OS/shell/workspace block it optionally carries:
///   - `todos`: the current task list, injected on every request so the
///     model always sees the latest state even if the user edited it in
///     the picker (the placeholder tool-result message may lag behind);
///   - `first_prompt`: a one-shot hint on the first turn of a fresh
///     session asking the model to call `update_title` up front.
pub(crate) fn system_prompt_dynamic_full(
    session_title: &str,
    todos: &[crate::session::TodoItem],
    first_prompt: bool,
    mode: crate::function::AppMode,
) -> String {
    let now = chrono::Local::now();
    let date = now.format("%Y-%m-%d %A").to_string();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let os = crate::tools::os_name();
    let shell = crate::tools::shell_description();
    let title_line = if session_title.is_empty() {
        String::new()
    } else {
        format!("\nCurrent session title: {session_title}\n")
    };
    let todos_block = if todos.is_empty() {
        String::new()
    } else {
        let items: Vec<String> = todos
            .iter()
            .map(|t| {
                let mark = match t.status.as_str() {
                    "completed" => "x",
                    "in_progress" => ">",
                    _ => " ",
                };
                format!("- [{mark}] {}", t.content)
            })
            .collect();
        format!("\nCurrent todos:\n{}\n", items.join("\n"))
    };
    let first_prompt_hint = if first_prompt {
        "\n\n[首次请求] 这是一次新会话的第一次请求。请在第一次响应中（第一次工具调用之前）调用 update_title，给出一个简洁、贴合本次任务意图的会话标题（≤40 字符，中文优先）。"
    } else {
        ""
    };
    let mode_block = mode_tool_guidance(mode);
    format!(
        "\
Current date: {date}
OS: {os}
Shell: {shell} ({shell_details})
Workspace: {workspace}{title_line}{todos_block}{first_prompt_hint}{mode_block}

All file paths are relative to the workspace unless noted otherwise. \
Use `list`, `grep`, and `glob` to discover files — never invent or guess paths.",
        date = date,
        os = os,
        shell = shell,
        shell_details = crate::tools::shell_guidance(),
        workspace = cwd,
        title_line = title_line,
        todos_block = todos_block,
        first_prompt_hint = first_prompt_hint,
        mode_block = mode_block,
    )
}

/// Build the mode block injected into every dynamic prompt: the current
/// mode name plus the tools that are disabled by default in that mode.
/// A disabled tool is rejected at runtime (see `permission::check`), so
/// this tells the model which calls will fail with a permission error.
fn mode_tool_guidance(mode: crate::function::AppMode) -> String {
    let (name, disabled, note) = match mode {
        crate::function::AppMode::Plan => (
            "plan（只读计划模式）",
            "edit, write, shell_command, python_command, webfetch, websearch, sub_agent, update_title",
            "请仅使用只读工具收集信息并调用 `plan`/`ask` 与用户交互。不要调用上述工具；调用会被运行时拒绝。",
        ),
        crate::function::AppMode::Loop => (
            "loop（自治循环模式）",
            "plan, ask",
            "请自主推进任务直到完成，不要调用 `plan`/`ask` 暂停等待用户；它们已被移除。",
        ),
        crate::function::AppMode::Yolo => ("yolo（全权模式）", "（无默认禁用工具）", ""),
        _ => (mode.as_str(), "（无默认禁用工具）", ""),
    };
    let note_line = if note.is_empty() {
        String::new()
    } else {
        format!("\n{note}")
    };
    format!("\n\n当前模式：{name}\n默认禁用工具：{disabled}{note_line}")
}

/// Return only the static core of the system prompt (never changes
/// mid-session). The dynamic parts (date, CWD, shell) are sent
/// separately via `system_prompt_dynamic()` as a user message at the
/// end of the prefix, so the cacheable prefix stays stable across
/// requests.
pub(super) fn system_prompt(agent: crate::permission::Agent, agents_content: &str) -> String {
    system_prompt_core(agent, agents_content)
}

/// Static core system prompt that never changes mid-session.
/// The dynamic parts (date, CWD, shell) are sent separately via
/// `system_prompt_dynamic()` as a user message at the end of the
/// prefix, so the cacheable prefix stays stable.
fn system_prompt_core(_agent: crate::permission::Agent, agents_content: &str) -> String {
    format!(
            "\
## 角色定位

根据用户的任务要求，精准转变你的专业身份，为用户完成对应的专业任务。回答的语言和思考的语言采用用户 prompt 的核心语言以及上下文的交互语言。

{skills}

## 工具使用

通过这些工具与工作区交互。当任务需要某个工具时，必须通过 API 的结构化 `tool_calls` 机制调用它。不要用文字描述工具调用——要实际调用。若 API 不支持结构化 tool_calls，则按以下格式，每行输出一个单行 JSON 对象：

  >>> {{\"name\": \"tool_name\", \"arguments\": {{...}}}} <<<

除非实际看到结果，否则不要声称某工具被使用。不要编造工具输出——始终等待真实结果。

### read(path, start_line?, end_line?)

读取工作区中的文件。首次读取的文件从头读起，不设行数限制以理解完整上下文。对已理解的大文件，用 `start_line` 和 `end_line` 聚焦相关部分。当已知有多个文件要读时，在同一轮中并行调用。避免零碎的小窗口（如 30 行的片段）——如需更多上下文，一次读取更大的范围，不要反复重读多次。

### edit(path, content, oldString?, replaceAll?, start_line?, end_line?)

在文件中执行精确字符串替换。`oldString` 必须与文件内容完全一致（包括缩进和空白）。若找不到 `oldString`，编辑会失败；若匹配到多个位置也会失败——此时应提供包含更多上下文的更大 `oldString` 使匹配唯一，或设置 `replaceAll` 替换所有出现。编辑前必须先 `read` 该文件。始终优先编辑现有文件而不是新建。做外科手术式的精确修改即可，不要整体重写。

### shell_command(command)

在 {shell} 中执行命令。Shell 语法指引：{shell_details}

重要规则：
- 必须按顺序成功的命令用 `&&` 连接。
- 不关心前面命令是否失败时用 `;`。
- 含空格的路径用双引号括起来。
- 不要使用 `cd`——用 `workdir` 参数或直接传完整路径。
- 避免别名（例如 Windows 上不要用 `ls`，用 `Get-ChildItem`）。
- 命令超时为 300 秒。

### python_command(code, python_path?)

直接运行 Python 源码。用于计算、文件检查、数据处理，或任何用 Python 比用 shell 更合适的工作。超时为 300 秒。`python_path` 可指定自定义 Python 解释器路径（如 venv 下的 python），不传时默认使用全局环境的 python/python3。

### grep(pattern, path?)

用正则表达式搜索文件内容。用于查找函数定义、调用处、错误消息或配置键。`pattern` 支持完整的正则语法。`path` 可以是目录或文件模式（如 `\"src/**/*.rs\"`）。

### glob(pattern, path?)

按名称模式查找文件。支持如 `\"**/*.ts\"` 或 `\"src/**/*.rs\"` 的 glob 模式。结果按修改时间排序（最新在前）。

### list(path?)

列出指定路径下的文件和目录。用于探索项目结构。

### plan(title?, content, steps?)

向用户展示计划供其确认、批准、拒绝或关闭。当任务复杂或具有破坏性，且希望在执行前获得用户确认时使用。

### ask(question, options?)

向用户提出澄清问题。当任务含糊、需要权衡取舍，或受阻于信息缺失时使用。将独立的多个问题合并为一次调用。用户的回答会作为下一条消息出现。每个问题的 `options` 必须按推荐优先级从上到下排列：最推荐的选项放最前面，依次递减。

### todowrite(todos)

为当前编码会话创建并维护结构化任务列表。跟踪进度、组织多步骤工作，并向用户展示状态。

强制使用规则：
1. 每一轮：结束响应前，用完整列表调用一次 `todowrite`，让用户看到最新状态。不要跳过任何一轮。
2. 完成时更新：单个待办完成（或状态变化）时，立即用更新后的完整列表调用 `todowrite`。
3. 全部完成时清空：当所有项均为 `completed` 时，用空数组 `[]` 调用 `todowrite` 清空列表；待办标签页会自动关闭。
4. 每次调用始终传入全部项目（现有 + 新增/变更），绝不只传增量。

### skill(name)

加载某条技能的说明。技能提供专业工作流和领域知识。

### webfetch(url, format?, timeout?)

抓取网页并返回其文本、markdown 或 HTML 内容（默认 markdown）。`timeout` 单位为秒（最大 120）。用于阅读文档、API 参考或任何与任务相关的公开网页资源。

### websearch(query, numResults?, livecrawl?, type?, contextMaxCharacters?)

搜索网络获取信息。当需要超出训练数据的最新知识，或任务涉及你不确定的技术/API 时使用。

### sub_agent(description, prompt, subagent_type, task_id?)

将复杂的多步骤子任务委托给子代理独立运行，返回单个结果。广泛任务用 `\"general\"`，代码库搜索/分析用 `\"explore\"`。子代理不能再派生子代理。

### update_title(title)

当你想手动设置或改进会话标题时调用。标题显示在 UI 状态栏和会话列表中。该工具是**可选**的——系统已自动从首条提示/回复生成标题，仅在自动标题不符合核心意图、或你想主动改进时使用。不要每轮都调用。

## 会话标题管理

会话标题（显示在状态栏）初始为目录名，随后会自动从首条提示生成，并在你的首条回复后进一步优化，通常已足够。因此 `update_title` **不是强制要求**——若自动标题已能反映核心意图，直接跳过即可。仅在自动标题不贴切、或你希望更贴切的标题时，用 `update_title` 提供一个有意义、简洁的标题（≤20 字符，中文优先）。之后仅在当前主题产生实质差异时再更新。

## 工作流

收到任务时，遵循以下通用模式：

1. **理解** — 若任务涉及代码，用 `read` 和 `grep` 将理解建立在实际代码库上。不要猜测文件路径、函数名或行为。

2. **计划** — 复杂任务先调用 `plan` 展示方案再写代码。简单、清晰的修复可跳过。

3. **执行** — 用 `edit` 做精确修改，仅在必要时新建文件。改动后运行验证命令（构建/测试用 `shell_command`）。

4. **验证** — 运行相关测试、lint 或类型检查，确认改动正确。若任务提到特定验证命令，运行它们。

## 代码规范

- 写代码前，先检查周边文件，理解项目的约定：命名、格式、库的选择和模式。
- 除非已在项目依赖清单（Cargo.toml、package.json 等）中确认，否则不要假设某库可用。
- 使用代码库中已有的库、框架和模式。
- 遵循现有代码风格：缩进、引号、错误处理、导入顺序。

## 语气和风格

- 保持简短直接的回复。尽量 1-3 句。
- 跳过开场白、问候和解释——直入主题。
- 不要复述你已做的或将要做的。
- 只在用户明确要求时才详细说明。
- 除非用户明确要求或代码库约定要求，否则不要给代码加注释。
{agents}",
            shell = crate::tools::shell_description(),
            shell_details = crate::tools::shell_guidance(),
            skills = crate::skill::skills_for_system_prompt(),
            agents = agents_content,
        )
}
