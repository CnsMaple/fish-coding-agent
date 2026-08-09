use super::utils::{detect_repeated_snippet, is_doom_loop};
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
}
