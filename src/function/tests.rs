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
        input_prompt_area: None,
        tui_selection: None,
        selected_text: None,
        tui_drag_start: None,
        last_mouse_event: None,
        model_cache_path: cache_file,
        thinking_toggle_rows: Vec::new(),
        tool_toggle_rows: Vec::new(),
        session_area: None,
        input_cursor_screen: None,
        function_panel_cursor: None,
        paste_blocks: VecDeque::new(),
        image_blocks: VecDeque::new(),
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

#[test]
fn open_ask_pushes_first_question() {
    let mut app = make_test_app();
    app.open_ask("Q?".to_string(), vec!["a".to_string(), "b".to_string()]);
    // Notifications tab at 0, Ask tab at 1.
    let state = match app.function.tabs.get(1) {
        Some(SidebarTab::Ask(s)) => s.clone(),
        _ => panic!("expected ask tab"),
    };
    assert_eq!(state.items.len(), 1);
    assert_eq!(state.items[0].question, "Q?");
    assert_eq!(state.items[0].options, vec!["a", "b"]);
    // The per-question cursor starts on the first option.
    assert_eq!(state.items[0].cursor, 0);
}

#[test]
fn open_ask_appends_to_existing_tab() {
    let mut app = make_test_app();
    app.open_ask("first".to_string(), vec!["a".to_string()]);
    app.open_ask("second".to_string(), vec!["x".to_string(), "y".to_string()]);
    let state = match app.function.tabs.get(1) {
        Some(SidebarTab::Ask(s)) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(state.items.len(), 2);
    // Adding a question makes it the active one so the user
    // answers it next.
    assert_eq!(state.active, 1);
    assert_eq!(state.items[1].question, "second");
}

#[test]
fn ask_row_count_includes_options_and_freeform() {
    let s = AskState::new("q".to_string(), vec!["a".into(), "b".into(), "c".into()]);
    // The picker for this question has 3 options + 1 implicit
    // "Type your own answer…" row.
    assert_eq!(s.items[0].row_count(), 4);
    assert_eq!(s.row_count(), 4);
}

#[test]
fn ask_all_answered_after_picking_last() {
    let mut s = AskState::new("q".to_string(), vec!["a".into()]);
    s.items[0].answered = Some("a".to_string());
    assert!(s.all_answered());
}

#[test]
fn ask_all_answered_false_when_pending() {
    let s = AskState::new("q".to_string(), vec!["a".into()]);
    assert!(!s.all_answered());
}

#[test]
fn ask_next_unanswered_wraps() {
    let mut s = AskState::new("q1".to_string(), vec!["a".into()]);
    s.push("q2".to_string(), vec!["b".into()]);
    s.push("q3".to_string(), vec!["c".into()]);
    s.items[0].answered = Some("a".to_string());
    s.items[2].answered = Some("c".to_string());
    // From index 1, the next unanswered is index 1 itself.
    assert_eq!(s.next_unanswered(1), Some(1));
    // From index 2 (answered), wrap and find index 1.
    assert_eq!(s.next_unanswered(2), Some(1));
    // From index 0 (answered), wrap and find index 1.
    assert_eq!(s.next_unanswered(0), Some(1));
}

#[test]
fn ask_build_summary_lists_all_pairs() {
    let mut s = AskState::new("Q1?".to_string(), vec!["a".into()]);
    s.push("Q2?".to_string(), vec!["x".into()]);
    s.items[0].answered = Some("a".to_string());
    s.items[1].answered = Some("x".to_string());
    let summary = s.build_summary();
    assert!(summary.contains("Q1"));
    assert!(summary.contains("Q2"));
    assert!(summary.contains("a"));
    assert!(summary.contains("x"));
    assert!(summary.contains("Proceed"));
}

#[test]
fn thinking_picker_ensure_cursor_visible_scrolls_down() {
    use crate::function::ThinkingPickerState;
    let mut s = ThinkingPickerState::new();
    s.cursor = 4;
    crate::ui::function_panel::ensure_cursor_visible(s.cursor, &mut s.scroll, 3);
    assert_eq!(s.scroll, 2, "scroll should jump so cursor is last visible row");
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

/// `push` places the new question at the end and makes it
/// active so the user can answer it next.
#[test]
fn ask_push_makes_new_question_active() {
    let mut s = AskState::new("q1".to_string(), vec!["a".into()]);
    s.items[0].cursor = 1; // user has scrolled within q1
    s.push("q2".to_string(), vec!["x".into()]);
    assert_eq!(s.active, 1);
    assert_eq!(s.items[1].cursor, 0);
}
