use super::utils::is_doom_loop;
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
}
