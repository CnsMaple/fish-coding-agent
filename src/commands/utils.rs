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

/// Minimum byte length of a within-turn repeated substring to count as a
/// stuck text loop. Short prefixes shared by legit templated output (e.g.
/// "现在实现登录。现在实现注册。…") never reach this length, while a model
/// echoing the same full sentence repeatedly does.
const WITHIN_MIN_BYTES: usize = 24;
/// A substring must appear this many times within a single assistant text
/// to count as a stuck within-text loop.
const WITHIN_MIN_COUNT: usize = 10;
/// Upper bound on the single assistant text analyzed. Mirrors the across-turn
/// guard: past this we skip the check.
const WITHIN_MAX_BYTES: usize = 512 * 1024;

/// Within-turn text repetition detector.
///
/// A model can loop by repeating the same sentence *within a single message*
/// many times — e.g. emitting "现在重建文件。" over and over before acting.
/// This detector builds a suffix array + LCP over that one text and reports a
/// long-enough substring that appears WITHIN_MIN_COUNT or more times. The
/// caller breaks the loop and pauses for user review when it fires.
pub(super) fn detect_within_turn_repetition(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() > WITHIN_MAX_BYTES {
        return None;
    }
    let n = bytes.len();
    let s: Vec<usize> = bytes.iter().map(|&b| b as usize + 1).collect();
    let sa = suffix_array(&s);
    let lcp = lcp_array(&s, &sa);

    // Binary search for the longest length L such that some set of
    // WITHIN_MIN_COUNT consecutive suffixes shares a common prefix of
    // length >= L.
    let mut lo = WITHIN_MIN_BYTES;
    let mut hi = n;
    let mut best: Option<usize> = None;
    while lo <= hi {
        let mid = (lo + hi) / 2;
        if longest_lcp_run(&lcp, mid) >= WITHIN_MIN_COUNT {
            best = Some(mid);
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    let len = best?;

    // Find a qualifying group of consecutive suffixes sharing a prefix of
    // length >= `len`. A group whose repeated snippet is purely code (no
    // natural-language prose) is legitimate — a model enumerating code
    // sites naturally repeats the same identifiers/statements within one
    // message. Only snippets carrying prose (CJK characters) count as a
    // stuck text loop; skip any code-only group and keep scanning.
    let mut run = 1usize;
    let mut run_start = 0usize;
    for (i, &v) in lcp.iter().enumerate() {
        if v >= len {
            if run == 1 {
                run_start = i;
            }
            run += 1;
            if run >= WITHIN_MIN_COUNT {
                let pos = sa[run_start];
                let sub = &bytes[pos..pos + len];
                if let Ok(s) = String::from_utf8(sub.to_vec()) {
                    if contains_cjk(&s) {
                        return Some(s);
                    }
                }
            }
        } else {
            run = 1;
        }
    }
    None
}

/// True when `s` contains at least one CJK character. The within-turn
/// detector only fires on prose snippets (repeated sentences), never on
/// code-only fragments (identifiers, paths, statements) that a model
/// legitimately reuses while enumerating code sites.
fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c,
            '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
            | '\u{3400}'..='\u{4DBF}' // CJK Extension A
            | '\u{F900}'..='\u{FAFF}' // CJK Compatibility Ideographs
        )
    })
}

/// Longest run of consecutive LCP entries that are each >= `min_len`. That
/// run length equals the maximum number of times a `min_len`-long substring
/// can appear consecutively in the suffix-array ordering.
fn longest_lcp_run(lcp: &[usize], min_len: usize) -> usize {
    let mut best = 1usize;
    let mut run = 1usize;
    for &v in lcp {
        if v >= min_len {
            run += 1;
            if run > best {
                best = run;
            }
        } else {
            run = 1;
        }
    }
    best
}

/// Build a suffix array over an integer sequence using the doubling
/// algorithm (Manber–Myers). Values are >= 0; separators use 0.
fn suffix_array(s: &[usize]) -> Vec<usize> {
    let n = s.len();
    let mut sa: Vec<usize> = (0..n).collect();
    let mut rank: Vec<usize> = s.to_vec();
    let mut tmp = vec![0usize; n];
    let mut k = 1usize;
    while k < n {
        let key = |i: usize| -> (usize, usize) {
            let r2 = if i + k < n { rank[i + k] + 1 } else { 0 };
            (rank[i] + 1, r2)
        };
        sa.sort_by_key(|&i| key(i));
        tmp[sa[0]] = 0;
        for i in 1..n {
            tmp[sa[i]] = tmp[sa[i - 1]] + usize::from(key(sa[i - 1]) != key(sa[i]));
        }
        rank.copy_from_slice(&tmp);
        if rank[sa[n - 1]] == n - 1 {
            break;
        }
        k <<= 1;
    }
    sa
}

/// Kasai's algorithm: LCP array where lcp[i] = LCP(sa[i], sa[i+1]).
fn lcp_array(s: &[usize], sa: &[usize]) -> Vec<usize> {
    let n = s.len();
    let mut rank = vec![0usize; n];
    for (i, &v) in sa.iter().enumerate() {
        rank[v] = i;
    }
    let mut lcp = vec![0usize; n.saturating_sub(1)];
    let mut h = 0usize;
    for i in 0..n {
        if rank[i] > 0 {
            let j = sa[rank[i] - 1];
            while i + h < n && j + h < n && s[i + h] == s[j + h] {
                h += 1;
            }
            lcp[rank[i] - 1] = h;
            h = h.saturating_sub(1);
        }
    }
    lcp
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

/// Extract a `u64` field from potentially-partial JSON (e.g. the
/// `timeout_secs` argument of a shell command). Tries complete JSON
/// first, then scans for `"key": <digits>` in the partial stream.
pub fn extract_partial_json_u64(args: &str, key: &str) -> Option<u64> {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(args) {
        return val.get(key).and_then(|v| v.as_u64());
    }
    let needle = format!("\"{key}\"");
    let mut search_from = 0;
    while let Some(pos) = args[search_from..].find(&needle) {
        let abs_pos = search_from + pos;
        let after_key = abs_pos + needle.len();
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
        let num_abs = after_key + colon_offset + 1 + ws2;
        let digits: String = args[num_abs..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if digits.is_empty() {
            search_from = abs_pos + 1;
            continue;
        }
        return digits.parse().ok();
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
            let path = val.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let start = val.get("start_line").and_then(|v| v.as_u64());
            let end = val.get("end_line").and_then(|v| v.as_u64());
            let range = match (start, end) {
                (Some(s), Some(e)) => format!("{}:{}", s, e),
                (Some(s), None) => format!("{}:", s),
                (None, Some(e)) => format!("{}:", e),
                (None, None) => String::new(),
            };
            if !range.is_empty() {
                return format!("read [{} {}]", path, range);
            } else {
                return format!("read [{}]", path);
            }
        }
    }
    if call.name == "edit" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(old) = val.get("oldString").and_then(|v| v.as_str()) {
                let display = if old.chars().count() > 40 {
                    format!("{}…", old.chars().take(40).collect::<String>())
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
                let display = if short.chars().count() > 40 {
                    format!("{}…", short.chars().take(40).collect::<String>())
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
                let display = if short.chars().count() > 40 {
                    format!("{}…", short.chars().take(40).collect::<String>())
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
                let display = if n.chars().count() > 40 {
                    format!("{}…", n.chars().take(40).collect::<String>())
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
                let display = if u.chars().count() > 50 {
                    format!("{}…", u.chars().take(50).collect::<String>())
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
                let display = if q.chars().count() > 40 {
                    format!("{}…", q.chars().take(40).collect::<String>())
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
            let display = if short.chars().count() > 40 {
                format!("{}…", short.chars().take(40).collect::<String>())
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
