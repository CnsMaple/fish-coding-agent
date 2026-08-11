use super::utils::{detect_within_turn_repetition, is_doom_loop};
#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use super::*;

    #[test]
    fn doom_loop_triggers_on_third_identical_call() {
        let mut h: Vec<(String, String)> = Vec::new();
        h.push(("read".to_string(), "{\"path\":\"a\"}".to_string()));
        assert!(
            !is_doom_loop(&h, "read", "{\"path\":\"a\"}"),
            "2nd call is fine"
        );
        h.push(("read".to_string(), "{\"path\":\"a\"}".to_string()));
        assert!(
            is_doom_loop(&h, "read", "{\"path\":\"a\"}"),
            "3rd identical call must trigger"
        );
    }

    #[test]
    fn doom_loop_ignores_different_args() {
        let h = vec![
            ("read".to_string(), "{\"path\":\"a\"}".to_string()),
            ("read".to_string(), "{\"path\":\"b\"}".to_string()),
        ];
        assert!(!is_doom_loop(&h, "read", "{\"path\":\"a\"}"));
    }

    #[test]
    fn doom_loop_ignores_different_tools() {
        let h = vec![
            ("read".to_string(), "x".to_string()),
            ("grep".to_string(), "x".to_string()),
        ];
        assert!(!is_doom_loop(&h, "read", "x"));
    }

    #[test]
    fn within_turn_triggers_on_repeated_lines() {
        // The model echoes the same line many times within one message.
        // This is invisible to the across-turn detector (which counts
        // distinct source texts), so it must be caught here. Line-level
        // detection fires when an identical line appears 10+ times.
        let text = "现在重建文件，并确认改动正确。\n".repeat(10)
            + "现在重建两个文件。\n"
            + "现在重建 run_bench.py 和 verifier.sh。\n";
        let found = detect_within_turn_repetition(&text);
        assert!(found.is_some(), "within-turn repeats must trigger");
        let found = found.unwrap();
        assert!(found.contains("现在重建文件"), "found snippet: {found:?}");
    }

    #[test]
    fn within_turn_triggers_on_exact_clone_loop() {
        // Ten identical multi-line blocks repeat back to back.
        let block = "现在改造 run_bench.py 跨平台。\n先读全文。\n";
        assert!(detect_within_turn_repetition(&block.repeat(10)).is_some());
    }

    #[test]
    fn within_turn_triggers_on_alternating_loop() {
        // The model oscillates between two short phrases (周期 p = 2). Each
        // line alone is too short / too few to trigger, but the alternating
        // pair looping back-to-back is a clear stuck loop and must fire.
        let text = "现在重建。\n现在重建文件。\n".repeat(10);
        assert!(
            detect_within_turn_repetition(&text).is_some(),
            "alternating loop must trigger"
        );
    }

    #[test]
    fn within_turn_triggers_on_single_line_sentence_loop() {
        // The model repeats the same sentence(s) back-to-back with NO
        // newlines at all — the whole burst is one line. The line-level
        // pass cannot see it (lines() collapses to a single line), so the
        // sentence-level pass must catch it. The repeated block is
        // "I'll do the edit now and run. Then interpret. Then fix
        // assertion and remove debug. Let me go." (4 sentences).
        let block = "I'll do the edit now and run. Then interpret. Then fix assertion and remove debug. Let me go. ";
        let text = block.repeat(8);
        assert_eq!(text.lines().count(), 1, "burst must be a single line");
        let found = detect_within_turn_repetition(&text);
        assert!(found.is_some(), "single-line sentence loop must trigger");
        let found = found.unwrap();
        assert!(found.contains("do the edit now and run") || found.contains("Let me go."));
    }

    #[test]
    fn within_turn_ignores_legit_templated_lists() {
        // A task list that reuses a short verb prefix with distinct nouns is
        // legitimate; the shared prefix never reaches the byte threshold.
        let list = "现在实现登录。现在实现注册。现在实现退出。现在实现个人中心。现在实现设置。"
            .to_string()
            + "现在实现消息。现在实现帮助。现在实现关于。现在实现搜索。现在实现收藏。";
        assert_eq!(detect_within_turn_repetition(&list), None);

        let imports = "import os\nimport sys\nimport json\nimport time\nimport re\nimport math\n";
        assert_eq!(detect_within_turn_repetition(imports), None);
    }

    #[test]
    fn within_turn_ignores_repeated_code_fragments() {
        // A model enumerating N interception sites naturally repeats the
        // same code statement (`ChatWarn + ChatDone + return`) many times
        // within one message. That is legitimate reasoning, not a stuck
        // text loop, so it must not trigger even though the code fragment
        // is long and appears far more than the count threshold.
        let text = "interception 1: send ChatWarn + ChatDone + return\n".to_string()
            + "interception 2: send ChatWarn + ChatDone + return\n"
            + "interception 3: send ChatWarn + ChatDone + return\n"
            + "interception 4: send ChatWarn + ChatDone + return\n"
            + "interception 5: send ChatWarn + ChatDone + return\n";
        assert_eq!(detect_within_turn_repetition(&text), None);
    }

    #[test]
    fn within_turn_ignores_short_or_sparse_input() {
        // Too short / too few repeats to be a stuck loop.
        assert_eq!(detect_within_turn_repetition("现在重建。"), None);
        let three_short = "现在重建。现在重建。现在重建。";
        assert_eq!(detect_within_turn_repetition(three_short), None);
    }

    #[test]
    fn dynamic_prompt_includes_todos_when_present() {
        let todos = vec![
            crate::session::TodoItem {
                content: "实现登录".to_string(),
                status: "in_progress".to_string(),
            },
            crate::session::TodoItem {
                content: "写测试".to_string(),
                status: "pending".to_string(),
            },
            crate::session::TodoItem {
                content: "收尾".to_string(),
                status: "completed".to_string(),
            },
        ];
        let s = crate::commands::system_prompt_dynamic_full(
            "",
            &todos,
            false,
            crate::function::AppMode::Yolo,
        );
        assert!(s.contains("Current todos:"), "missing todos header: {s}");
        assert!(
            s.contains("- [>] 实现登录"),
            "missing in_progress item: {s}"
        );
        assert!(s.contains("- [ ] 写测试"), "missing pending item: {s}");
        assert!(s.contains("- [x] 收尾"), "missing completed item: {s}");
        assert!(
            !s.contains("首次请求"),
            "hint must not show when not first: {s}"
        );
    }

    #[test]
    fn dynamic_prompt_omits_todos_when_empty() {
        let s = crate::commands::system_prompt_dynamic_full(
            "",
            &[],
            false,
            crate::function::AppMode::Yolo,
        );
        assert!(
            !s.contains("Current todos:"),
            "empty todos must be omitted: {s}"
        );
    }

    #[test]
    fn dynamic_prompt_first_turn_hint_only_when_flag_set() {
        let s = crate::commands::system_prompt_dynamic_full(
            "",
            &[],
            true,
            crate::function::AppMode::Yolo,
        );
        assert!(s.contains("update_title"), "first-turn hint missing: {s}");
        assert!(s.contains("首次请求"), "first-turn hint missing: {s}");
        let s2 = crate::commands::system_prompt_dynamic_full(
            "",
            &[],
            false,
            crate::function::AppMode::Yolo,
        );
        assert!(!s2.contains("首次请求"), "hint must be gated by flag: {s2}");
    }

    #[test]
    fn dynamic_prompt_includes_session_title() {
        let s = crate::commands::system_prompt_dynamic_full(
            "修复聊天标题",
            &[],
            false,
            crate::function::AppMode::Yolo,
        );
        assert!(
            s.contains("Current session title: 修复聊天标题"),
            "title missing: {s}"
        );
    }

    #[test]
    fn dynamic_prompt_injects_current_mode() {
        let plan = crate::commands::system_prompt_dynamic_full(
            "",
            &[],
            false,
            crate::function::AppMode::Plan,
        );
        assert!(plan.contains("当前模式：plan"), "plan mode missing: {plan}");
        assert!(
            plan.contains("默认禁用工具：edit, write, shell_command, python_command, webfetch, websearch, sub_agent, update_title"),
            "plan disabled tools missing: {plan}"
        );

        let yolo = crate::commands::system_prompt_dynamic_full(
            "",
            &[],
            false,
            crate::function::AppMode::Yolo,
        );
        assert!(yolo.contains("当前模式：yolo"), "yolo mode missing: {yolo}");
        assert!(
            yolo.contains("无默认禁用工具"),
            "yolo should have no disabled tools: {yolo}"
        );

        let loopd = crate::commands::system_prompt_dynamic_full(
            "",
            &[],
            false,
            crate::function::AppMode::Loop,
        );
        assert!(
            loopd.contains("当前模式：loop"),
            "loop mode missing: {loopd}"
        );
        assert!(
            loopd.contains("默认禁用工具：plan, ask"),
            "loop disabled plan/ask missing: {loopd}"
        );
    }
}
