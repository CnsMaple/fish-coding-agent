mod exec;
mod file;
mod specs;
mod web;

pub use exec::*;
pub use file::*;
pub use specs::*;
pub use web::*;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::Datelike;
use futures_util::StreamExt;
use glob::Pattern;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;

use crate::event::AppMsg;
use crate::mcp::McpRegistry;

pub(super) const DEFAULT_TIMEOUT_SECS: u64 = 300;
pub(super) const COMMAND_OUTPUT_LIMIT: usize = 16_000;
pub(super) const READ_OUTPUT_LIMIT: usize = 32_000;

/// Build the shell program name and argument list for a command string.
/// On Windows, prepends a UTF-8 preamble and uses pwsh/powershell.
/// On Unix, uses `$SHELL -lc`.
pub(super) fn build_shell_args(command: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        let utf8_preamble = "\
$OutputEncoding = [Console]::OutputEncoding = \
[System.Text.UTF8Encoding]::UTF8; \
$env:PYTHONIOENCODING='utf-8'; ";
        let full_cmd = format!("{utf8_preamble}{command}");
        let shell = windows_shell_program().to_string();
        (
            shell,
            vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-Command".into(),
                full_cmd,
            ],
        )
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
        (shell, vec!["-lc".into(), command.to_string()])
    }
}

/// Return the list of (program, args) invocations to try for running Python
/// code, in fallback order. On Windows: `python` → `py -3`. On Unix:
/// `python3` → `python`.
pub(super) fn python_invocations(code: &str) -> Vec<(String, Vec<String>)> {
    #[cfg(windows)]
    {
        vec![
            (
                "python".into(),
                vec!["-X".into(), "utf8".into(), "-c".into(), code.into()],
            ),
            (
                "py".into(),
                vec![
                    "-3".into(),
                    "-X".into(),
                    "utf8".into(),
                    "-c".into(),
                    code.into(),
                ],
            ),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![
            ("python3".into(), vec!["-c".into(), code.into()]),
            ("python".into(), vec!["-c".into(), code.into()]),
        ]
    }
}

/// Format the output of a command execution into the standard envelope
/// `exit_code / wall_secs / timeout_secs / stdout / stderr`.
pub(super) fn format_command_output(
    exit_code: &str,
    elapsed: std::time::Duration,
    timeout_secs: u64,
    stdout: &str,
    stderr: &str,
) -> String {
    format!(
        "exit_code: {}\nwall_secs: {:.2}\ntimeout_secs: {}\nstdout:\n{}\nstderr:\n{}",
        exit_code,
        elapsed.as_secs_f64(),
        timeout_secs,
        stdout,
        stderr
    )
}

/// Directory names that should be skipped when traversing the workspace
/// (build artifacts, VCS metadata, or heavy dependency trees).
pub fn should_skip_dir(name: &str) -> bool {
    matches!(name, ".git" | "target" | "node_modules")
}

/// Resolve a possibly-relative path against `cwd`. Absolute paths are
/// returned as-is; relative paths are joined to `cwd`.
pub fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}

#[derive(Deserialize)]
pub(super) struct ReadArgs {
    pub(super) path: String,
    pub(super) start_line: Option<usize>,
    pub(super) end_line: Option<usize>,
}

#[derive(Deserialize)]
pub(super) struct WriteArgs {
    pub(super) path: String,
    #[serde(alias = "newString")]
    pub(super) content: Option<String>,
    #[serde(rename = "oldString")]
    pub(super) old_string: Option<String>,
    #[serde(rename = "replaceAll")]
    pub(super) replace_all: Option<bool>,
    pub(super) start_line: Option<usize>,
    pub(super) end_line: Option<usize>,
}

#[derive(Deserialize)]
pub(super) struct WriteNewArgs {
    #[serde(rename = "path")]
    pub(super) path: String,
    pub(super) content: String,
}

#[derive(Deserialize)]
pub(super) struct CommandArgs {
    pub(super) command: String,
    #[serde(default)]
    pub(super) timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
pub(super) struct PythonArgs {
    pub(super) code: String,
    #[serde(default)]
    pub(super) timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
pub(super) struct GrepArgs {
    pub(super) pattern: String,
    pub(super) path: Option<String>,
    #[serde(default)]
    pub(super) glob: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ListArgs {
    pub(super) path: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct GlobArgs {
    pub(super) pattern: String,
    pub(super) path: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct SkillArgs {
    pub(super) name: String,
}

#[derive(Deserialize)]
pub(super) struct WebFetchArgs {
    pub(super) url: String,
    pub(super) format: Option<String>,
    pub(super) timeout: Option<u32>,
}

#[derive(Deserialize)]
pub(super) struct WebSearchArgs {
    pub(super) query: String,
    #[serde(rename = "numResults", default)]
    pub(super) num_results: Option<u32>,
    pub(super) livecrawl: Option<String>,
    #[serde(rename = "type", default)]
    pub(super) search_type: Option<String>,
    #[serde(rename = "contextMaxCharacters", default)]
    pub(super) context_max_chars: Option<u32>,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
pub(super) struct SubAgentArgs {
    pub(super) description: String,
    pub(super) prompt: String,
    #[serde(rename = "subagent_type")]
    pub(super) subagent_type: String,
    #[serde(rename = "max_steps", default)]
    pub(super) max_steps: Option<u64>,
    #[serde(rename = "task_id")]
    pub(super) task_id: Option<String>,
}

pub(super) fn resolve_workspace_path(cwd: &Path, path: &str) -> Result<PathBuf> {
    Ok(resolve_path(cwd, path))
}

pub(super) fn select_lines(
    text: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<String> {
    // Neither bound given: return the whole file unchanged.
    if start_line.is_none() && end_line.is_none() {
        return Ok(text.to_string());
    }
    let total = text.lines().count();
    // An empty file has no selectable lines; the only valid request is
    // "the whole file" (handled above), so any partial request is an error.
    if total == 0 {
        return Err(anyhow!("line range requested but file is empty (0 lines)"));
    }
    // Missing bounds default to the file's first/last line, so callers can
    // pass only one of the two instead of always needing both.
    let start = start_line.unwrap_or(1);
    let end = end_line.unwrap_or(total);
    if start == 0 || end == 0 || start > end {
        return Err(anyhow!(
            "invalid line range: start_line must be <= end_line (got {}:{})",
            start,
            end
        ));
    }
    Ok(text
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let line_no = idx + 1;
            (line_no >= start && line_no <= end).then_some(line)
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

pub(super) fn replace_string(
    text: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<String> {
    // Normalize CRLF -> LF so that the caller can write old_string with
    // plain \n even when the file uses Windows \r\n line endings.
    // Also normalize bare \r (old Mac) -> \n for consistency.
    let had_crlf = text.contains("\r\n");
    let had_bare_cr = !had_crlf && text.contains('\r');
    let mut normalized_text: String;
    let mut normalized_old: String;
    let mut normalized_new: String;
    let (text_ref, old_ref, new_ref) = if had_crlf || had_bare_cr {
        normalized_text = text.replace("\r\n", "\n");
        normalized_old = old_string.replace("\r\n", "\n");
        normalized_new = new_string.replace("\r\n", "\n");
        if had_bare_cr {
            normalized_text = normalized_text.replace('\r', "\n");
            normalized_old = normalized_old.replace('\r', "\n");
            normalized_new = normalized_new.replace('\r', "\n");
        }
        (
            normalized_text.as_str(),
            normalized_old.as_str(),
            normalized_new.as_str(),
        )
    } else {
        (text, old_string, new_string)
    };

    let (search_text, search_offset) = if let (Some(start), Some(end)) = (start_line, end_line) {
        if start == 0 || end == 0 || start > end {
            return Err(anyhow!("start_line must be <= end_line and >= 1"));
        }
        let lines: Vec<&str> = text_ref.lines().collect();
        if end > lines.len() {
            return Err(anyhow!(
                "line range [{}, {}] exceeds file length ({})",
                start,
                end,
                lines.len()
            ));
        }
        let offset = lines[..start - 1]
            .iter()
            .map(|l| l.len() + 1)
            .sum::<usize>();
        let range_text = lines[start - 1..end].join("\n");
        (range_text, offset)
    } else {
        (text_ref.to_string(), 0)
    };

    // Try exact match first.
    let matches: Vec<usize> = search_text
        .match_indices(old_ref)
        .map(|(idx, _)| idx)
        .collect();

    // If exact match fails, try a whitespace-tolerant fallback: strip
    // trailing whitespace from each line in both the search text and
    // old_string, then match. If found, locate the corresponding region
    // in the original (non-stripped) text by scanning line by line.
    let (matches, fuzzy) = if matches.is_empty() && !old_ref.is_empty() {
        let fuzzy_search = strip_trailing_ws(&search_text);
        let fuzzy_old = strip_trailing_ws(old_ref);
        if fuzzy_search != search_text || fuzzy_old != old_ref {
            let fuzzy_matches: Vec<usize> = fuzzy_search
                .match_indices(&fuzzy_old)
                .map(|(idx, _)| idx)
                .collect();
            if !fuzzy_matches.is_empty() {
                // Map each fuzzy offset back to original coordinates by
                // walking through both strings line by line.
                let orig_offsets: Vec<usize> = fuzzy_matches
                    .iter()
                    .map(|&fo| fuzzy_to_orig_offset(&search_text, &fuzzy_search, fo))
                    .collect();
                (orig_offsets, true)
            } else {
                (Vec::new(), false)
            }
        } else {
            (Vec::new(), false)
        }
    } else {
        (matches, false)
    };

    if matches.is_empty() {
        if let (Some(s), Some(e)) = (start_line, end_line) {
            return Err(anyhow!("oldString not found in lines [{}, {}]", s, e));
        }
        return Err(anyhow!("oldString not found in file"));
    }
    if !replace_all && matches.len() > 1 {
        let hint = if start_line.is_some() {
            "provide more context to make oldString unique within the range"
        } else {
            "use replaceAll=true to replace all, or provide start_line/end_line to narrow the scope"
        };
        let mut ctx = String::new();
        let lines: Vec<&str> = text_ref.lines().collect();
        for (i, &offset) in matches.iter().take(5).enumerate() {
            let mut char_pos = 0;
            let mut line_no: usize = 1;
            for (idx, line) in lines.iter().enumerate() {
                char_pos += line.len() + 1;
                if char_pos > offset {
                    line_no = idx + 1;
                    break;
                }
            }
            let start = line_no.saturating_sub(1);
            let end = (line_no + 1).min(lines.len());
            let snippet = lines[start..end].join("\n");
            ctx.push_str(&format!(
                "  match {} at line {}: ...{}\n",
                i + 1,
                line_no,
                snippet
            ));
        }
        if matches.len() > 5 {
            ctx.push_str(&format!("  ... and {} more matches\n", matches.len() - 5));
        }
        return Err(anyhow!(
            "oldString found {} times; {}\n{}",
            matches.len(),
            hint,
            ctx,
        ));
    }

    let result = if start_line.is_some() {
        let mut result = text_ref.to_string();
        if fuzzy {
            // For fuzzy matches, we need to replace the original-text
            // region that corresponds to the fuzzy match. The match
            // offset in the fuzzy domain maps back to an original offset;
            // the length to replace is the length of the original (non-
            // stripped) text at that position. Since fuzzy matching only
            // strips trailing whitespace, the original text at the match
            // position starts at the same offset and extends through the
            // matched region plus any trailing whitespace that was stripped.
            // We find the end by searching for the original old_ref in a
            // whitespace-tolerant way.
            let orig_len = original_match_len(&search_text, matches[0], old_ref);
            result.replace_range(
                search_offset + matches[0]..search_offset + matches[0] + orig_len,
                new_ref,
            );
        } else {
            result.replace_range(
                search_offset..search_offset + search_text.len(),
                &search_text.replace(old_ref, new_ref),
            );
        }
        result
    } else if fuzzy {
        // Whole-file fuzzy match: replace the first (or all) matched
        // region in the original text.
        let mut result = text_ref.to_string();
        for &offset in matches.iter().rev() {
            let orig_len = original_match_len(&result, offset, old_ref);
            result.replace_range(offset..offset + orig_len, new_ref);
        }
        result
    } else {
        text_ref.replace(old_ref, new_ref)
    };

    // Restore CRLF if the original file used it. For bare \r files,
    // keep the normalized \n (treating them as LF files).
    if had_crlf {
        Ok(result.replace("\n", "\r\n"))
    } else {
        Ok(result)
    }
}

/// Strip trailing whitespace from each line (but preserve the newline
/// characters themselves). Used for fuzzy fallback matching.
fn strip_trailing_ws(s: &str) -> String {
    s.lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Map a character offset in the fuzzy (trailing-ws-stripped) text back
/// to the corresponding offset in the original text. Both strings have
/// the same number of lines; the only difference is that some lines in
/// the original have extra trailing whitespace.
fn fuzzy_to_orig_offset(original: &str, fuzzy: &str, fuzzy_offset: usize) -> usize {
    let orig_lines: Vec<&str> = original.lines().collect();
    let fuzzy_lines: Vec<&str> = fuzzy.lines().collect();

    // Walk through both, accumulating positions.
    let mut orig_pos = 0usize;
    let mut fuzzy_pos = 0usize;

    for (i, fuzzy_line) in fuzzy_lines.iter().enumerate() {
        let orig_line = orig_lines.get(i).copied().unwrap_or("");

        // If the fuzzy offset falls within this line's range, the offset
        // within the line is the same in both strings (trailing whitespace
        // is at the end).
        if fuzzy_pos + fuzzy_line.len() >= fuzzy_offset {
            let within_line = fuzzy_offset - fuzzy_pos;
            return orig_pos + within_line.min(orig_line.len());
        }

        // Advance past this line + its newline in both strings.
        orig_pos += orig_line.len() + 1; // +1 for \n
        fuzzy_pos += fuzzy_line.len() + 1; // +1 for \n
    }

    // Fallback: return the original position at the end.
    orig_pos
}

/// Find the length of the original text that matches `old_ref` starting
/// at `offset` in `original`, accounting for trailing whitespace
/// differences. Returns the number of characters to replace.
fn original_match_len(original: &str, offset: usize, old_ref: &str) -> usize {
    let old_lines: Vec<&str> = old_ref.lines().collect();
    if old_lines.is_empty() {
        return 0;
    }
    let suffix = &original[offset..];
    let orig_lines: Vec<&str> = suffix.lines().collect();
    if orig_lines.len() < old_lines.len() {
        return old_ref.len();
    }

    let mut total = 0usize;
    let n = old_lines.len();
    for i in 0..n {
        let orig_line = orig_lines[i];
        let is_last = i == n - 1;

        if orig_line.trim_end() == old_lines[i].trim_end() {
            if is_last && !old_ref.ends_with('\n') {
                // Last line, no trailing newline in old_ref: only replace
                // the content characters, not trailing whitespace.
                total += old_lines[i]
                    .trim_end()
                    .len()
                    .min(orig_line.trim_end().len());
            } else {
                // Include the original line's trailing whitespace.
                total += orig_line.len();
            }
        } else {
            total += old_lines[i]
                .trim_end()
                .len()
                .min(orig_line.trim_end().len());
        }

        // Add newline between lines.
        if i < n - 1 {
            total += 1; // \n
        } else if old_ref.ends_with('\n') {
            total += 1; // trailing \n in old_ref
        }
    }
    total
}

pub fn is_valid_tool(name: &str) -> bool {
    matches!(
        name,
        "read"
            | "edit"
            | "shell_command"
            | "python_command"
            | "grep"
            | "list"
            | "plan"
            | "ask"
            | "todowrite"
            | "command"
            | "glob"
            | "write"
            | "skill"
            | "webfetch"
            | "websearch"
            | "sub_agent"
    )
}

pub(super) fn truncate(mut text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    text.truncate(limit);
    while !text.is_char_boundary(text.len()) {
        text.pop();
    }
    text.push_str("\n[truncated]");
    text
}

/// Maximum number of lines a tool's AI-facing output may keep
/// before being truncated. Matches opencode's `MAX_LINES`.
pub const TOOL_OUTPUT_MAX_LINES: usize = 2000;

/// Maximum byte size of a tool's AI-facing output before being
/// truncated. Matches opencode's `MAX_BYTES` (50 KiB).
pub const TOOL_OUTPUT_MAX_BYTES: usize = 50 * 1024;

/// Counter used to disambiguate truncation files written in the same
/// millisecond.
static TRUNCATION_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Directory under the system temp folder where truncated tool
/// outputs are saved so the AI can re-read them with `read`/`grep`.
pub(super) fn truncation_dir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push("fish_coding_agent_tool_output");
    p
}

/// Persist the full (untruncated) tool output to a temp file and
/// return the display path. Failures are non-fatal: we fall back to
/// an inline note so the AI still gets a truncation hint.
pub(super) fn save_tool_output(text: &str) -> String {
    let dir = truncation_dir();
    let _ = std::fs::create_dir_all(&dir);
    let seq = TRUNCATION_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let file = dir.join(format!("tool_{stamp}_{seq}.txt"));
    match std::fs::write(&file, text) {
        Ok(()) => file.display().to_string(),
        Err(_) => "(temp file write failed)".to_string(),
    }
}

/// Truncate a raw tool output string to the line/byte limits. When
/// it fits, returns it unchanged. When it exceeds, saves the full
/// text to a temp file and returns a head preview plus a hint that
/// guides the AI to use `grep` / `read` (with offset/limit) instead
/// of re-reading the whole thing.
pub(super) fn truncate_output_str(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let total_bytes = text.len();
    if lines.len() <= TOOL_OUTPUT_MAX_LINES && total_bytes <= TOOL_OUTPUT_MAX_BYTES {
        return text.to_string();
    }

    let mut out: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for line in &lines {
        let size = line.len() + if out.is_empty() { 0 } else { 1 };
        if out.len() >= TOOL_OUTPUT_MAX_LINES || bytes + size > TOOL_OUTPUT_MAX_BYTES {
            break;
        }
        out.push(line);
        bytes += size;
    }

    let removed_lines = lines.len() - out.len();
    let removed_bytes = total_bytes - bytes;
    let preview = out.join("\n");
    let path = save_tool_output(text);
    let unit = if removed_bytes > 0 && out.len() == TOOL_OUTPUT_MAX_LINES {
        "lines"
    } else {
        "bytes"
    };
    let amount = if unit == "lines" {
        removed_lines
    } else {
        removed_bytes
    };
    format!(
        "{preview}\n\n...{amount} {unit} truncated...\n\nThe tool call succeeded but the output was truncated. Full output saved to: {path}\nUse grep to search the full content or read with offset/limit to view specific sections."
    )
}

/// Apply the unified truncation layer to a tool result envelope of
/// the form `{"ok":true,"result":...,"metadata"?...}`. Only the
/// AI-facing `result` string is truncated; the UI-only `metadata`
/// field (e.g. `edit_diff`) is left untouched. Non-JSON envelopes
/// are returned unchanged.
pub fn truncate_tool_output(envelope: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(envelope) else {
        return envelope.to_string();
    };
    let Some(obj) = v.as_object_mut() else {
        return envelope.to_string();
    };
    if obj.get("ok").and_then(|b| b.as_bool()) != Some(true) {
        return envelope.to_string();
    }
    if let Some(result) = obj
        .get("result")
        .and_then(|r| r.as_str())
        .map(str::to_string)
    {
        let truncated = truncate_output_str(&result);
        obj.insert("result".to_string(), serde_json::Value::String(truncated));
    }
    serde_json::to_string(&v).unwrap_or_else(|_| envelope.to_string())
}

#[cfg(test)]
mod tests;
