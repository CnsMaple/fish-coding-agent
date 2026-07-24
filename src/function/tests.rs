use super::*;

fn make_test_app() -> App {
    use crate::config::{make_id, Config, ProviderConfig, ProviderKind, ProviderMode};
    use crate::function::notifications::Notifications;
    let mut cfg = Config::default();
    let kind = ProviderKind::Openai;
    let id = make_id(kind, ProviderMode::Key);
    cfg.entries.entry(id).or_insert_with(|| ProviderConfig {
        api_key: String::new(),
        api_key_env: String::new(),
        base_url: crate::config::default_base_url(kind).to_string(),
        model: String::new(),
        model_display: String::new(),
        name: String::new(),
        access_key: String::new(),
        secret_key: String::new(),
        provider_id: String::new(),
    });
    cfg.active = Some(make_id(ProviderKind::Openai, ProviderMode::Key));
    let tmp = std::env::temp_dir().join("fish-coding-agent-fns-test.json");
    let _ = std::fs::remove_file(&tmp);
    let cache_file = tmp.parent().unwrap_or(&tmp).join("model-cache.json");
    App {
        config: cfg,
        config_path: tmp,
        session: Session::default(),
        session_id: crate::session::store::new_session_id(),
        session_title: "test".to_string(),
        mode: AppMode::Yolo,
        previous_mode: AppMode::Yolo,
        active_agent: crate::permission::Agent::Build,
        function: FunctionPanel::new(),
        input: crate::input::InputState::new(),
        status: crate::input::status::StatusBar::new(),
        function_visible: false,
        pending_events: 0,
        notifications: Notifications::default(),
        model_cache: crate::function::notifications::ModelCache::default(),
        hit_rate: crate::function::notifications::HitRate::new(50),
        token_rate: crate::function::notifications::TokenRate::new(50),
        response_started_at: None,
        response_accumulated: std::time::Duration::ZERO,
        response_output_chars: 0,
        response_output_tokens: None,
        reqwest: reqwest::Client::new(),
        stream_client: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("stream client"),
        inflight: None,
        cancel_state: CancelState::Idle,
        focus_target: FocusTarget::Input,
        current_request_seq: 0,
        pending_request: None,
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        should_quit: false,
        msg_tx: None,
        mcp_tools_dirty: true,
        disabled_tools: std::collections::HashSet::new(),
        input_prompt_area: None,
        tui_selection: None,
        selected_text: None,
        tui_drag_start: None,
        pending_tool_toggle: None,
        last_mouse_event: None,
        model_cache_path: cache_file,
        thinking_toggle_rows: Vec::new(),
        tool_toggle_rows: Vec::new(),
        session_area: None,
        agents_area: None,
        function_panel_area: None,
        input_cursor_screen: None,
        function_panel_cursor: None,
        paste_blocks: VecDeque::new(),
        image_blocks: VecDeque::new(),
        block_undo_stack: VecDeque::new(),
        block_redo_stack: VecDeque::new(),
        last_paste_text: None,
        last_paste_at: None,
        paste_key_quota: 0,
        burst_buf: String::new(),
        burst_snapshot: None,
        pending_ask_snapshot: String::new(),
        session_scroll: crate::event::ScrollAnimator::default(),
        input_scroll: crate::event::ScrollAnimator::default(),
        input_scroll_decoupled: false,
        force_full_repaint: false,
        compacting: false,
        pending_post_compaction_prompt: None,
        agents_visible: false,
        agents_cursor: 0,
        load_duration: std::time::Duration::ZERO,
    }
}

#[test]
fn plan_state_is_dirty_on_open_and_clears_after_save() {
    let mut app = make_test_app();
    app.open_plan("t".to_string(), "body".to_string());
    let state = match app.function.tabs.first().unwrap() {
        SidebarTab::Plan(s) => s.clone(),
        _ => panic!("expected plan tab"),
    };
    assert!(state.dirty, "open_plan must start dirty");
    assert!(state.path.is_none(), "open_plan must NOT auto-save");

    // save_active_plan writes to the user's real config dir. We
    // accept either true (write succeeded) or false (sandbox
    // blocks disk), but if it returned true the path must be
    // populated and dirty must be false.
    let ok = app.save_active_plan();
    if ok {
        let state = match app.function.tabs.first().unwrap() {
            SidebarTab::Plan(s) => s.clone(),
            _ => panic!(),
        };
        assert!(!state.dirty);
        assert!(state.path.is_some());
    }
}

/// After the AI opens a plan, the panel must be visible (so the user
/// can read the plan) but focus stays on the input box, because the
/// user still needs to type args / follow-up text. This is the
/// distinguishing behaviour vs `jump_to_plan` (the `/plan` command),
/// which focuses the panel itself.
#[test]
fn open_plan_keeps_input_focus_while_showing_panel() {
    let mut app = make_test_app();
    app.open_plan("t".to_string(), "body".to_string());
    assert!(
        app.function_visible,
        "panel must be visible to read the plan"
    );
    assert_eq!(
        app.focus_target,
        FocusTarget::Input,
        "input box must keep focus so the user can type args"
    );
    assert!(
        app.function_panel_cursor.is_none(),
        "panel cursor must be cleared so it does not steal focus"
    );
}

#[test]
fn thinking_picker_ensure_cursor_visible_scrolls_down() {
    use crate::function::ThinkingPickerState;
    let mut s = ThinkingPickerState::new();
    s.cursor = 4;
    crate::ui::function_panel::ensure_cursor_visible(s.cursor, &mut s.scroll, 3);
    assert_eq!(
        s.scroll, 2,
        "scroll should jump so cursor is last visible row"
    );
}

#[test]
fn thinking_picker_ensure_cursor_visible_scrolls_up() {
    use crate::function::ThinkingPickerState;
    let mut s = ThinkingPickerState::new();
    s.scroll = 4;
    s.cursor = 0;
    crate::ui::function_panel::ensure_cursor_visible(s.cursor, &mut s.scroll, 3);
    assert_eq!(s.scroll, 0, "scroll should follow cursor up to top");
}

#[test]
fn thinking_picker_no_scroll_when_fits() {
    use crate::function::ThinkingPickerState;
    let mut s = ThinkingPickerState::new();
    s.scroll = 0;
    s.cursor = 1;
    crate::ui::function_panel::ensure_cursor_visible(s.cursor, &mut s.scroll, 3);
    assert_eq!(s.scroll, 0, "no scroll needed when total fits visible");
}
