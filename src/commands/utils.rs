use crate::providers::ToolCall;

/// Doom-loop detector: returns true when `name`/`args` match each of
/// the last two entries in `history`, i.e. this would be the 3rd
/// consecutive identical tool call. Matches opencode's
/// `DOOM_LOOP_THRESHOLD = 3`.
pub(super) fn is_doom_loop(history: &[(String, String)], name: &str, args: &str) -> bool {
    let n = history.len();
    if n < 2 {
        return false;
    }
    history[n - 1].0 == name
        && history[n - 1].1 == args
        && history[n - 2].0 == name
        && history[n - 2].1 == args
}

/// Extract the human-readable display content from a tool result JSON string.
/// Strips the `{"ok":true,"result":"..."}` wrapper to show just the inner content.
pub(super) fn parse_tool_result_display(result: &str) -> (String, bool) {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(result) {
        match val.get("ok").and_then(|v| v.as_bool()) {
            Some(true) => (
                val.get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                false,
            ),
            Some(false) => (
                val.get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or(result)
                    .to_string(),
                true,
            ),
            None => (result.to_string(), false),
        }
    } else {
        (result.to_string(), false)
    }
}

/// Extract a string field from potentially-partial JSON.
/// First tries `serde_json::from_str`. If that fails (because the
/// JSON is incomplete), falls back to a heuristic scanner that
/// finds `"key": "value` and extracts the partial value with
/// escape-sequence handling.
///
/// Returns `Some(value)` if the field is found (partial or complete),
/// `None` if the field is not present in the JSON at all.
pub fn extract_partial_json_field(args: &str, key: &str) -> Option<String> {
    // Fast path: complete JSON
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(args) {
        return val.get(key).and_then(|v| v.as_str()).map(|s| s.to_string());
    }
    // Heuristic: scan for `"key": "` and extract the partial string value
    let needle = format!("\"{key}\"");
    let mut search_from = 0;
    while let Some(pos) = args[search_from..].find(&needle) {
        let abs_pos = search_from + pos;
        let after_key = abs_pos + needle.len();
        // Skip whitespace and look for `:`
        let rest = &args[after_key..];
        let trimmed = rest.trim_start();
        let colon_offset = rest.len() - trimmed.len();
        if !trimmed.starts_with(':') {
            search_from = abs_pos + 1;
            continue;
        }
        let after_colon = &rest[colon_offset + 1..];
        let trimmed2 = after_colon.trim_start();
        let ws2 = after_colon.len() - trimmed2.len();
        if !trimmed2.starts_with('"') {
            search_from = abs_pos + 1;
            continue;
        }
        // Found `"key": "` — extract the string value
        let value_start_abs = after_key + colon_offset + 1 + ws2 + 1;
        let raw = &args[value_start_abs..];
        return Some(unescape_partial_json_string(raw));
    }
    None
}

/// Unescape a partial JSON string value (the text after the opening
/// `"`). Handles `\"`, `\\`, `\n`, `\t`, `\r`, `\/`, `\uXXXX`. Stops
/// at the first unescaped `"` (which would be the closing quote).
pub(super) fn unescape_partial_json_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if escaped {
            match ch {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{0008}'),
                'f' => out.push('\u{000C}'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Ok(code) = u32::from_str_radix(&hex, 16) {
                        if let Some(c) = char::from_u32(code) {
                            out.push(c);
                        }
                    }
                }
                _ => {
                    // Unknown escape — keep as-is
                    out.push('\\');
                    out.push(ch);
                }
            }
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            // Closing quote — value is complete
            break;
        } else {
            out.push(ch);
        }
    }
    out
}

pub(super) fn tool_result_title(call: &ToolCall) -> String {
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

if call.name == "read" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            let start = val.get("start_line").and_then(|v| v.as_u64());
            let end = val.get("end_line").and_then(|v| v.as_u64());
            match (start, end) {
                (Some(s), Some(e)) => return format!("read [{}:{}]", s, e),
                (Some(s), None) => return format!("read [{}:]", s),
                (None, Some(e)) => return format!("read [{}:]", e),
                (None, None) => {}
            }
        }
    }
    if call.name == "edit" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(old) = val.get("oldString").and_then(|v| v.as_str()) {
                let display = if old.len() > 40 {
                    format!("{}…", &old[..40])
                } else {
                    old.to_string()
                };
                return format!("edit [{}]", display);
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
    if call.name == "glob" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(pattern) = val.get("pattern").and_then(|v| v.as_str()) {
                let short = pattern.trim();
                let display = if short.len() > 40 {
                    format!("{}…", &short[..40])
                } else {
                    short.to_string()
                };
                return format!("glob [{}]", display);
            }
        }
    }

    if call.name == "todowrite" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(todos) = val.get("todos").and_then(|v| v.as_array()) {
                return format!("todowrite ({} items)", todos.len());
            }
        }
    }
    if call.name == "skill" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(name) = val.get("name").and_then(|v| v.as_str()) {
                let n = name.trim();
                let display = if n.len() > 40 {
                    format!("{}…", &n[..40])
                } else {
                    n.to_string()
                };
                return format!("skill [{}]", display);
            }
        }
    }
    if call.name == "webfetch" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(url) = val.get("url").and_then(|v| v.as_str()) {
                let u = url.trim();
                let display = if u.len() > 50 {
                    format!("{}…", &u[..50])
                } else {
                    u.to_string()
                };
                return format!("webfetch [{}]", display);
            }
        }
    }
    if call.name == "websearch" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(query) = val.get("query").and_then(|v| v.as_str()) {
                let q = query.trim();
                let display = if q.len() > 40 {
                    format!("{}…", &q[..40])
                } else {
                    q.to_string()
                };
                return format!("websearch [{}]", display);
            }
        }
    }
    if call.name == "sub_agent" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            let stype = val
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let desc = val
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let short = desc.trim();
            let display = if short.len() > 40 {
                format!("{}…", &short[..40])
            } else {
                short.to_string()
            };
            return format!("sub_agent [{stype}] {display}");
        }
    }

    call.name.clone()
}
/// Fallback: parse text-based tool call descriptions from assistant
/// content when the model did not emit structured tool_calls.
/// Looks for JSON objects `{"name": "...", "arguments": {...}}` in
/// the text and returns valid tool calls found.
pub(super) fn parse_text_tool_calls(content: &str) -> Vec<ToolCall> {
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

