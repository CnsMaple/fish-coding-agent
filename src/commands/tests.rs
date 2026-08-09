use super::utils::{detect_repeated_snippet, detect_within_turn_repetition, is_doom_loop};
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
    fn repeated_snippet_triggers_on_three_identical_texts() {
        let snippet = "现在实现 AppMode::Loop 变体。";
        let h: Vec<String> = vec![snippet.to_string(), snippet.to_string()];
        let found = detect_repeated_snippet(&h, &format!("{snippet} 现在执行。"));
        assert!(found.is_some(), "3rd identical text must trigger");
        let found = found.unwrap();
        assert!(found.contains(snippet), "found snippet: {found:?}");
    }

    #[test]
    fn repeated_snippet_ignores_distinct_texts() {
        let h: Vec<String> = vec![
            "现在读取 AppMode 定义。".to_string(),
            "现在实现 set_mode 逻辑。".to_string(),
        ];
        assert_eq!(detect_repeated_snippet(&h, "现在测试工具规格。"), None);
    }

    #[test]
    fn repeated_snippet_detects_non_contiguous_repeats() {
        // The snippet appears in turns 1, 3 and 5 but not 2 or 4; the
        // detector counts distinct source texts, so non-contiguous repeats
        // still fire once the 3rd distinct occurrence lands.
        let h: Vec<String> = vec![
            "前缀AAAA现在实现变体。".to_string(),
            "完全不同的中间内容。".to_string(),
            "前缀AAAA现在实现变体。".to_string(),
            "又是一段不同的内容。".to_string(),
        ];
        assert!(detect_repeated_snippet(&h, "前缀AAAA现在实现变体。").is_some());
    }

    #[test]
    fn within_turn_triggers_on_back_to_back_repeats() {
        // The model echoes the same sentence many times within one message.
        // This is invisible to the across-turn detector (which counts
        // distinct source texts), so it must be caught here.
        let text = "现在重建。现在重建文件。现在重建。现在重建文件。现在重建 run_bench.py "
            .to_string()
            + "和 verifier.sh。现在重建文件。现在重建。现在重建文件。现在重建两个文件。"
            + "现在重建。现在重建文件。现在重建。现在重建文件。现在重建。现在重建文件。"
            + "现在重建。";
        let found = detect_within_turn_repetition(&text);
        assert!(found.is_some(), "within-turn repeats must trigger");
        let found = found.unwrap();
        assert!(found.contains("现在重建"), "found snippet: {found:?}");
    }

    #[test]
    fn within_turn_triggers_on_exact_clone_loop() {
        let text = "现在改造 run_bench.py 跨平台。先读全文。".repeat(6);
        assert!(detect_within_turn_repetition(&text).is_some());
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
    fn within_turn_ignores_short_or_sparse_input() {
        // Too short / too few repeats to be a stuck loop.
        assert_eq!(detect_within_turn_repetition("现在重建。"), None);
        let three_short = "现在重建。现在重建。现在重建。";
        assert_eq!(detect_within_turn_repetition(three_short), None);
    }
}
