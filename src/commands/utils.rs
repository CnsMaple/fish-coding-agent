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

/// A periodic pattern must repeat this many times consecutively within a
/// single assistant text to count as a stuck within-text loop. The model's
/// idea: once the same text recurs, treat the text between occurrences as the
/// pattern and match it forward; five consecutive repeats is enough to call
/// it a loop and interrupt.
const WITHIN_MIN_COUNT: usize = 5;
/// Upper bound on the single assistant text analyzed. Mirrors the across-turn
/// guard: past this we skip the check.
const WITHIN_MAX_BYTES: usize = 512 * 1024;
/// Minimum byte length of a repeated period pattern to count as a stuck
/// loop. Short repeated fragments (e.g. a bare "现在重建。") reachable from
/// legitimately templated output never reach this length.
const MIN_REPEAT_BYTES: usize = 24;
/// A "small vocabulary" loop is declared only when the number of distinct
/// lines is at most this many.
const SMALL_VOCAB_MAX_DISTINCT: usize = 5;
/// A "small vocabulary" loop is declared only when the total line count is
/// at least this many times the distinct-line count.
const SMALL_VOCAB_MIN_RATIO: usize = 3;

/// Within-turn text repetition detector.
///
/// A model can loop by repeating the same sentence *within a single message*
/// many times — e.g. emitting "现在重建文件。" over and over before acting.
/// This detector hunts for a *periodic* line sequence: a pattern of `p`
/// consecutive lines that repeats WITHIN_MIN_COUNT times back-to-back. One
/// check therefore covers every stuck-loop shape:
///
/// - single-line loop (p = 1): "A A A A …"
/// - multi-line block loop (p >= 2): "A B C A B C …"
/// - alternating / oscillating loop (p = 2): "A B A B A B …"
///
/// Blank lines are dropped first (they carry no signal and would otherwise
/// fragment a repeated block). Progressive reasoning — where each line is
/// distinct even when sharing phrasing — never matches a period and so never
/// fires. The caller breaks the loop and pauses for user review when it fires.
pub(super) fn detect_within_turn_repetition(text: &str) -> Option<String> {
    if text.is_empty() || text.len() > WITHIN_MAX_BYTES {
        return None;
    }

    // Collapse to non-empty trimmed lines, preserving order.
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    if let Some(found) = detect_periodic_repeat(&lines) {
        return Some(found);
    }

    // Fixed-column loop: a periodic pattern whose *varying* lines change
    // each cycle but which keeps one column constant (e.g. the model re-
    // states "Let me delete the test config." every time while the Chinese
    // line wobbles). The constant column repeats >= WITHIN_MIN_COUNT times
    // back-to-back, so the exact-match periodic detector above misses it.
    if let Some(found) = detect_fixed_column_loop(&lines) {
        return Some(found);
    }

    // Small-vocabulary loop: the model rephrases the same few lines over
    // and over without ever repeating a line verbatim enough to hit the
    // periodic or fixed-column rules (e.g. "改用无重定向 Start-Process 启
    // 动独立进程。/Let me start the dev server detached. …" cycling through
    // a handful of near-synonyms). A tiny distinct set stretched over a
    // long output is a strong stuck-loop signal.
    if is_small_vocabulary_loop(&lines) {
        return Some(
            "small-vocabulary loop: only a few distinct lines repeated many times".to_string(),
        );
    }

    // The model may instead loop *within a single line*: repeating the same
    // sentence (or the same multi-sentence block) back-to-back with no
    // newlines at all. `lines()` above collapses such a burst to one line, so
    // the line-level pass cannot see it. Split the text into sentences and
    // re-run the same periodic detector on the sentence sequence to catch
    // this shape too.
    let sentences = split_sentences(text);
    if sentences.len() < WITHIN_MIN_COUNT {
        return None;
    }
    let refs: Vec<&str> = sentences.iter().map(String::as_str).collect();
    detect_periodic_repeat(&refs)
}

/// Split text into sentence-sized units on sentence/phrase-ending
/// punctuation. Each unit keeps its trailing marker so the sequence carries
/// the same shape as the emitted text. Used to detect single-line stuck
/// loops where the repeated period is a sentence (or a block of sentences)
/// rather than a newline-delimited line.
fn split_sentences(text: &str) -> Vec<String> {
    const SEP: &[char] = &['.', '。', '!', '！', '?', '？', ';', '；'];
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        cur.push(ch);
        if SEP.contains(&ch) {
            let t = cur.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
            cur.clear();
        }
    }
    let t = cur.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
    out
}

/// Detect a periodic run in the line sequence: a pattern of `p` consecutive
/// lines that repeats WITHIN_MIN_COUNT times back-to-back. Returns the
/// repeated pattern (joined with newlines) when found.
fn detect_periodic_repeat(lines: &[&str]) -> Option<String> {
    let n = lines.len();
    if n < WITHIN_MIN_COUNT {
        return None;
    }
    let max_p = n / WITHIN_MIN_COUNT;
    for p in 1..=max_p {
        // Longest consecutive chain of period-p matches ending at each index.
        let mut run = 0usize;
        let mut best_run = 0usize;
        let mut best_end = 0usize;
        for i in 0..n {
            run = if i >= p && lines[i] == lines[i - p] {
                run + 1
            } else {
                0
            };
            if run > best_run {
                best_run = run;
                best_end = i;
            }
        }
        // `best_run` matches span [best_end - best_run + 1, best_end]; the
        // full periodic region begins one period earlier. Full periods k =
        // best_run / p + 1.
        let periods = best_run / p + 1;
        if periods < WITHIN_MIN_COUNT {
            continue;
        }
        let block_start = best_end - best_run + 1 - p;
        let pattern = lines[block_start..block_start + p].join("\n");
        if pattern.len() < MIN_REPEAT_BYTES {
            continue;
        }
        return Some(pattern);
    }
    None
}

/// Detect a fixed-column loop: a periodic pattern of period `p` whose
/// varying lines change each cycle but which keeps one column identical
/// every cycle. For example:
///
///   A₁ / X / A₂ / X / A₃ / X …   (period 2, column 1 = X constant)
///
/// The exact-match periodic detector never fires here because the varying
/// column (A₁, A₂, …) differs each period. Returns the constant line when
/// found.
fn detect_fixed_column_loop(lines: &[&str]) -> Option<String> {
    let n = lines.len();
    if n < WITHIN_MIN_COUNT {
        return None;
    }
    let max_p = n / WITHIN_MIN_COUNT;
    for p in 1..=max_p {
        for col in 0..p {
            // Longest consecutive run of equal lines within this column.
            let mut run = 0usize;
            let mut best_run = 0usize;
            let mut prev: Option<&str> = None;
            let mut k = col;
            while k < n {
                let cur = lines[k];
                if let Some(pv) = prev {
                    run = if cur == pv { run + 1 } else { 0 };
                    if run > best_run {
                        best_run = run;
                    }
                }
                prev = Some(cur);
                k += p;
            }
            // `best_run` equality matches span best_run + 1 full periods.
            let periods = best_run + 1;
            if periods >= WITHIN_MIN_COUNT {
                let fixed = lines[col];
                if fixed.len() >= MIN_REPEAT_BYTES {
                    return Some(fixed.to_string());
                }
            }
        }
    }
    None
}

/// Detect a small-vocabulary loop: the whole output is drawn from a tiny
/// set of distinct lines (<= SMALL_VOCAB_MAX_DISTINCT) stretched over many
/// lines (total >= SMALL_VOCAB_MIN_RATIO x distinct). The model rephrases
/// the same few near-synonyms without ever repeating a line verbatim enough
/// to hit the periodic or fixed-column rules.
fn is_small_vocabulary_loop(lines: &[&str]) -> bool {
    let n = lines.len();
    if n < SMALL_VOCAB_MIN_RATIO * SMALL_VOCAB_MAX_DISTINCT {
        return false;
    }
    let distinct: std::collections::HashSet<&&str> = lines.iter().collect();
    let d = distinct.len();
    d > 0 && d <= SMALL_VOCAB_MAX_DISTINCT && n >= SMALL_VOCAB_MIN_RATIO * d
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
