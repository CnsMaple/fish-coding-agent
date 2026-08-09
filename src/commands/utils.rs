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

/// Minimum length (in bytes) of an assistant-text snippet considered
/// "repeated". Shorter repetitions (e.g. a single tool name) are noise.
const SNIPPET_MIN_BYTES: usize = 24;
/// A snippet must appear in at least this many distinct recent assistant
/// texts (including the current one) to count as a stuck repetition.
const SNIPPET_MIN_COUNT: usize = 3;
/// How many recent assistant texts the detector considers. Older turns
/// are ignored so a legitimately reused phrase across a long session
/// does not false-positive.
const SNIPPET_WINDOW: usize = 8;
/// Upper bound on the total concatenated bytes analyzed per turn. Guards
/// against a pathological O(n log^2 n) suffix-array build on very long
/// single responses; past this we skip the check.
const SNIPPET_MAX_TOTAL_BYTES: usize = 512 * 1024;

/// Text-snippet repetition detector for assistant prose.
///
/// The doom-loop detector (`is_doom_loop`) only catches identical tool
/// *arguments* invoked 3x in a row. A model can instead loop by emitting
/// the same *text* every turn (e.g. "现在执行编辑." over and over) while
/// varying the tool args, so the args never match. This detector builds a
/// suffix array + LCP over the recent assistant contents and reports a
/// long-enough snippet that appears in SNIPPET_MIN_COUNT or more distinct
/// texts. The caller breaks the loop and pauses for user review when it
/// fires.
pub(super) fn detect_repeated_snippet(history: &[String], new: &str) -> Option<String> {
    // Only keep the most recent turns plus the current one.
    let mut texts: Vec<&str> = Vec::with_capacity(SNIPPET_WINDOW + 1);
    let start = history.len().saturating_sub(SNIPPET_WINDOW);
    texts.extend(history[start..].iter().map(String::as_str));
    texts.push(new);
    if texts.len() < SNIPPET_MIN_COUNT {
        return None;
    }
    repeated_snippet_in(&texts)
}

/// Core routine: concatenate `texts` with separator bytes, build a suffix
/// array + LCP, then find the longest substring that appears in at least
/// SNIPPET_MIN_COUNT distinct texts and is at least SNIPPET_MIN_BYTES long.
///
/// Uses binary search on the answer length: for a candidate length `L`, a
/// substring of length L appears in >= MIN_COUNT distinct texts iff some
/// bucket of consecutive suffixes (all sharing a prefix of length >= L)
/// covers >= MIN_COUNT distinct source texts. The separators guarantee a
/// match never spans two messages and that bucket counts are per-text.
fn repeated_snippet_in(texts: &[&str]) -> Option<String> {
    // Concatenate with separators and record each byte's source index.
    let mut combined: Vec<u8> = Vec::new();
    let mut source: Vec<usize> = Vec::new();
    for (ti, t) in texts.iter().enumerate() {
        if !combined.is_empty() {
            combined.push(0x00); // separator
            source.push(usize::MAX);
        }
        for &b in t.as_bytes() {
            combined.push(b);
            source.push(ti);
        }
    }
    let n = combined.len();
    if n == 0 || n > SNIPPET_MAX_TOTAL_BYTES {
        return None;
    }

    // Integer encoding: separators -> 0, bytes -> 1..=256.
    let s: Vec<usize> = combined
        .iter()
        .map(|&b| if b == 0x00 { 0 } else { b as usize + 1 })
        .collect();
    let sa = suffix_array(&s);
    let lcp = lcp_array(&s, &sa);

    // For a candidate length `len`, returns the start offset of the first
    // suffix of a bucket (group of consecutive suffixes sharing a prefix
    // of length >= `len`) that spans >= MIN_COUNT distinct source texts.
    // Returns None when no such bucket exists.
    fn find_for_len(
        lcp: &[usize],
        sa: &[usize],
        source: &[usize],
        num_texts: usize,
        n: usize,
        len: usize,
    ) -> Option<usize> {
        let mut seen = vec![false; num_texts];
        let mut distinct = 0usize;
        let mut active = Vec::<usize>::new();
        let mut bucket_start: Option<usize> = None;
        let mut i = 0usize;
        while i < n {
            let new_bucket = i == 0 || source[sa[i]] == usize::MAX || lcp[i - 1] < len;
            if new_bucket {
                if distinct >= SNIPPET_MIN_COUNT {
                    return bucket_start;
                }
                for &t in &active {
                    seen[t] = false;
                }
                distinct = 0;
                active.clear();
                bucket_start = Some(sa[i]);
            }
            let src = source[sa[i]];
            if src != usize::MAX && !seen[src] {
                seen[src] = true;
                distinct += 1;
                active.push(src);
            }
            i += 1;
        }
        if distinct >= SNIPPET_MIN_COUNT {
            bucket_start
        } else {
            None
        }
    }

    // Binary search for the longest valid length.
    let mut lo = SNIPPET_MIN_BYTES;
    let mut hi = n;
    let mut ans: Option<(usize, usize)> = None; // (len, start offset)
    while lo < hi {
        let mid = (lo + hi) / 2;
        if let Some(pos) = find_for_len(&lcp, &sa, &source, texts.len(), n, mid) {
            ans = Some((mid, pos));
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let (best_len, best_pos) = ans?;
    let bytes = &combined[best_pos..best_pos + best_len];
    String::from_utf8(bytes.to_vec()).ok()
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
