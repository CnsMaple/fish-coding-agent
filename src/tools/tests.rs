use super::*;

#[test]
fn truncate_tool_output_keeps_small_envelope() {
    let env = r#"{"ok":true,"result":"small"}"#;
    assert_eq!(truncate_tool_output(env), env);
}

#[test]
fn truncate_tool_output_preserves_metadata_field() {
    // edit/write envelope: result is short, metadata must survive
    // untouched so the TUI can still render the diff.
    let env = r#"{"ok":true,"result":"Edit applied successfully.","metadata":"{\"kind\":\"edit_diff\",\"old\":\"x\",\"new\":\"y\"}"}"#;
    let out = truncate_tool_output(env);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v.get("result").and_then(|r| r.as_str()),
        Some("Edit applied successfully.")
    );
    assert!(
        v.get("metadata").is_some(),
        "metadata must be preserved for the TUI"
    );
}

#[test]
fn truncate_tool_output_truncates_large_result() {
    let big = "x\n".repeat(5000);
    let env = json!({ "ok": true, "result": big }).to_string();
    let out = truncate_tool_output(&env);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let result = v.get("result").and_then(|r| r.as_str()).unwrap();
    assert!(
        result.contains("truncated"),
        "expected truncation marker, got tail: {}",
        &result[result.len().saturating_sub(200)..]
    );
    assert!(
        result.contains("grep"),
        "expected grep hint in truncated output"
    );
    // The kept body must be under the line limit.
    assert!(
        result.lines().count() <= TOOL_OUTPUT_MAX_LINES + 10,
        "truncated body too large"
    );
}

#[test]
fn truncate_tool_output_passes_through_non_json() {
    let raw = "not json at all";
    assert_eq!(truncate_tool_output(raw), raw);
}

/// Plan agent must be denied any tool that could mutate the
/// user's tree, even when the tool name is well-formed.
#[tokio::test]
async fn plan_mode_denies_write_file() {
    let result = execute_tool_with_agent(
        crate::permission::Agent::Plan,
        "edit",
        r#"{"path":"x","content":"y"}"#,
        Path::new("."),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v.get("ok").and_then(|s| s.as_bool()), Some(false));
    let err = v.get("error").and_then(|s| s.as_str()).unwrap_or("");
    assert!(err.contains("not allowed"), "got: {err}");
}

#[tokio::test]
async fn plan_mode_denies_shell_command() {
    let result = execute_tool_with_agent(
        crate::permission::Agent::Plan,
        "shell_command",
        r#"{"command":"echo hi"}"#,
        Path::new("."),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v.get("ok").and_then(|s| s.as_bool()), Some(false));
}

#[tokio::test]
async fn build_mode_allows_write_file() {
    let dir = std::env::temp_dir().join("fish-coding-agent-perm-test");
    let _ = std::fs::create_dir_all(&dir);
    let target = dir.join("perm_test.txt");
    let _ = std::fs::remove_file(&target);
    let args = serde_json::json!({
        "path": target.file_name().unwrap().to_string_lossy(),
        "content": "ok"
    })
    .to_string();
    let result =
        execute_tool_with_agent(crate::permission::Agent::Build, "edit", &args, &dir).await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v.get("ok").and_then(|s| s.as_bool()), Some(true));
    let _ = std::fs::remove_file(&target);
}

#[tokio::test]
async fn write_file_old_string_with_null_content_deletes_match() {
    let dir = std::env::temp_dir().join("fish-coding-agent-no-content-test");
    let _ = std::fs::create_dir_all(&dir);
    let target = dir.join("no_content.txt");
    std::fs::write(
        &target,
        "line1
line2
",
    )
    .unwrap();
    // oldString provided but content is missing (null) — should
    // delete the matched text (treat null as empty string).
    let args = serde_json::json!({
        "path": "no_content.txt",
        "oldString": "line1
    "
    })
    .to_string();
    let result =
        execute_tool_with_agent(crate::permission::Agent::Build, "edit", &args, &dir).await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v.get("ok").and_then(|s| s.as_bool()), Some(true));
    // line1 + newline should be deleted
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "line2
"
    );
    let _ = std::fs::remove_file(&target);
}

#[tokio::test]
async fn write_file_new_string_alias_works() {
    let dir = std::env::temp_dir().join("fish-coding-agent-newstring-test");
    let _ = std::fs::create_dir_all(&dir);
    let target = dir.join("newstring.txt");
    std::fs::write(
        &target, "foo
bar
",
    )
    .unwrap();
    // Use `newString` instead of `content` — should work as alias.
    let args = serde_json::json!({
        "path": "newstring.txt",
        "oldString": "foo",
        "newString": "baz"
    })
    .to_string();
    let result =
        execute_tool_with_agent(crate::permission::Agent::Build, "edit", &args, &dir).await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v.get("ok").and_then(|s| s.as_bool()), Some(true));
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "baz
bar
"
    );
    let _ = std::fs::remove_file(&target);
}

#[tokio::test]
async fn plan_tool_payload_contains_kind() {
    let result = execute_tool_with_agent(
        crate::permission::Agent::Plan,
        "plan",
        r#"{"title":"t","content":"hello"}"#,
        Path::new("."),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v.get("ok").and_then(|s| s.as_bool()), Some(true));
    let inner: serde_json::Value =
        serde_json::from_str(v.get("result").and_then(|s| s.as_str()).unwrap()).unwrap();
    assert_eq!(inner.get("kind").and_then(|s| s.as_str()), Some("plan"));
    assert_eq!(inner.get("title").and_then(|s| s.as_str()), Some("t"));
}

#[tokio::test]
async fn ask_tool_payload_contains_kind_and_questions() {
    let result = execute_tool_with_agent(
        crate::permission::Agent::Plan,
        "ask",
        r#"{"questions":[{"question":"which API?","options":["v1","v2"]}]}"#,
        Path::new("."),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v.get("ok").and_then(|s| s.as_bool()), Some(true));
    let inner: serde_json::Value =
        serde_json::from_str(v.get("result").and_then(|s| s.as_str()).unwrap()).unwrap();
    assert_eq!(inner.get("kind").and_then(|s| s.as_str()), Some("ask"));
    let questions = inner.get("questions").and_then(|s| s.as_array()).unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(
        questions[0].get("question").and_then(|s| s.as_str()),
        Some("which API?")
    );
    let options = questions[0]
        .get("options")
        .and_then(|s| s.as_array())
        .unwrap();
    assert_eq!(options.len(), 2);
}

#[tokio::test]
async fn ask_tool_rejects_empty_questions() {
    let result = execute_tool_with_agent(
        crate::permission::Agent::Build,
        "ask",
        r#"{"questions":[]}"#,
        Path::new("."),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v.get("ok").and_then(|s| s.as_bool()), Some(false));
    assert!(v
        .get("error")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .contains("empty"));
}

// ── replace_string unit tests ──

#[test]
fn replace_string_basic() {
    let input = "line1\nline2\nline3\n";
    let result = replace_string(input, "line2\n", "new\n", false, None, None).unwrap();
    assert_eq!(result, "line1\nnew\nline3\n");
}

#[test]
fn replace_string_crlf() {
    let input = "line1\r\nline2\r\nline3\r\n";
    let result = replace_string(input, "line2", "new", false, None, None).unwrap();
    assert_eq!(result, "line1\r\nnew\r\nline3\r\n");
}

#[test]
fn replace_string_multiple_lines() {
    let input = "a\nb\nc\nd\n";
    let result = replace_string(input, "b\nc\n", "X\nY\n", false, None, None).unwrap();
    assert_eq!(result, "a\nX\nY\nd\n");
}

#[test]
fn replace_string_multiple_lines_crlf() {
    let input = "a\r\nb\r\nc\r\nd\r\n";
    let result = replace_string(input, "b\r\nc", "X\r\nY", false, None, None).unwrap();
    assert_eq!(result, "a\r\nX\r\nY\r\nd\r\n");
}

#[test]
fn replace_string_not_found() {
    let input = "a\nb\nc\n";
    assert!(replace_string(input, "X", "Y", false, None, None).is_err());
}

#[test]
fn replace_string_multiple_matches_without_replace_all() {
    let input = "a\nb\na\n";
    assert!(replace_string(input, "a", "X", false, None, None).is_err());
}

#[test]
fn replace_string_replace_all() {
    let input = "a\nb\na\nc\n";
    let result = replace_string(input, "a", "X", true, None, None).unwrap();
    assert_eq!(result, "X\nb\nX\nc\n");
}

#[test]
fn replace_string_empty_old_string() {
    let input = "a\nb\n";
    let result = replace_string(input, "a", "X", false, None, None).unwrap();
    assert_eq!(result, "X\nb\n");
}

#[test]
fn replace_string_with_line_range() {
    let input = "a\nb\nc\nd\n";
    let result = replace_string(input, "b", "X", false, Some(2), Some(3)).unwrap();
    assert_eq!(result, "a\nX\nc\nd\n");
}

#[test]
fn replace_string_with_line_range_not_found() {
    let input = "a\nb\nc\n";
    assert!(replace_string(input, "a", "X", false, Some(2), Some(3)).is_err());
}

#[test]
fn replace_string_with_line_range_multiple_matches() {
    let input = "a\na\na\na\n";
    assert!(replace_string(input, "a", "X", false, Some(1), Some(3)).is_err());
}

#[test]
fn replace_string_with_line_range_replace_all() {
    let input = "a\na\na\na\n";
    let result = replace_string(input, "a", "X", true, Some(1), Some(3)).unwrap();
    assert_eq!(result, "X\nX\nX\na\n");
}

#[test]
fn replace_string_invalid_line_range() {
    assert!(replace_string("a\nb\n", "a", "X", false, Some(2), Some(1)).is_err());
    assert!(replace_string("a\nb\n", "a", "X", false, Some(0), Some(1)).is_err());
}

#[test]
fn replace_string_line_range_exceeds_length() {
    assert!(replace_string("a\nb\n", "a", "X", false, Some(1), Some(10)).is_err());
}

#[test]
fn replace_string_crlf_file_with_lf_old_string() {
    // Simulates the most common edit-tool issue: user writes oldString
    // with \n but the file on disk uses \r\n line endings.
    let input = "pub consumed: bool,\r\npub other: bool,\r\n";
    let result = replace_string(
        input,
        "pub consumed: bool,\n",
        "pub consumed: bool,\npub extra: bool,\n",
        false,
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        result,
        "pub consumed: bool,\r\npub extra: bool,\r\npub other: bool,\r\n"
    );
}

#[test]
fn replace_string_crlf_file_with_lf_old_string_start_line() {
    let input = "// header\r\nline1\r\nline2\r\nline3\r\n";
    let result = replace_string(input, "line2\n", "new\n", false, Some(2), Some(4)).unwrap();
    assert_eq!(result, "// header\r\nline1\r\nnew\r\nline3\r\n");
}

#[test]
fn replace_string_multi_match_shows_context() {
    let input = "a\nb\nc\na\nd\ne\n";
    let err = replace_string(input, "a", "X", false, None, None).unwrap_err();
    let msg = format!("{}", err);
    assert!(msg.contains("found 2 times"));
    assert!(msg.contains("match 1 at line 1"));
    assert!(msg.contains("match 2 at line 4"));
}

#[test]
fn replace_string_multi_match_shows_context_crlf() {
    let input = "a\r\nb\r\nc\r\na\r\nd\r\n";
    let err = replace_string(input, "a", "X", false, None, None).unwrap_err();
    let msg = format!("{}", err);
    assert!(msg.contains("found 2 times"));
    assert!(msg.contains("match 1 at line 1"));
    assert!(msg.contains("match 2 at line 4"));
}

// ── Fuzzy (trailing whitespace tolerant) matching tests ──

#[test]
fn replace_string_exact_match_preserves_trailing_ws() {
    // File has trailing spaces, oldString doesn't — exact match succeeds,
    // trailing spaces on the line are preserved (not part of oldString).
    let input = "line1   \nline2   \nline3\n";
    let result = replace_string(input, "line2", "new", false, None, None).unwrap();
    assert_eq!(result, "line1   \nnew   \nline3\n");
}

#[test]
fn replace_string_fuzzy_trailing_ws_in_old_string() {
    // oldString has trailing spaces, file doesn't — exact match fails,
    // fuzzy fallback strips trailing ws and matches.
    let input = "line1\nline2\nline3\n";
    let result = replace_string(input, "line2   ", "new", false, None, None).unwrap();
    assert_eq!(result, "line1\nnew\nline3\n");
}

#[test]
fn replace_string_fuzzy_multiline() {
    // Multi-line oldString with trailing ws differences.
    let input = "a   \nb   \nc\nd\n";
    let result = replace_string(input, "a\nb\n", "X\nY\n", false, None, None).unwrap();
    assert_eq!(result, "X\nY\nc\nd\n");
}

#[test]
fn replace_string_fuzzy_no_false_positive() {
    // When oldString genuinely doesn't exist, fuzzy shouldn't invent a match.
    let input = "line1\nline2\n";
    assert!(replace_string(input, "nonexistent", "X", false, None, None).is_err());
}

#[test]
fn replace_string_bare_cr_normalization() {
    // Bare \r (old Mac) is normalized to \n for matching, and stays
    // as \n in the output (not restored to \r).
    let input = "line1\rline2\rline3\r";
    let result = replace_string(input, "line2", "new", false, None, None).unwrap();
    assert_eq!(result, "line1\nnew\nline3\n");
}

#[test]
fn replace_string_fuzzy_crlf_with_trailing_ws() {
    // CRLF file with trailing spaces on line2, LF oldString.
    // oldString "line2\n" doesn't match "line2   \n" (after CRLF
    // normalization) so exact match fails. Fuzzy strips trailing ws.
    let input = "line1   \r\nline2   \r\nline3\r\n";
    let result = replace_string(input, "line2\n", "new\n", false, None, None).unwrap();
    assert_eq!(result, "line1   \r\nnew\r\nline3\r\n");
}
