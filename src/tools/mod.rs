mod specs;
mod exec;
mod file;
mod web;

pub use specs::*;
pub use exec::*;
pub use file::*;
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

#[derive(Deserialize)]
pub(super) struct ReadArgs {
    pub(super) path: String,
    pub(super) start_line: Option<usize>,
    pub(super) end_line: Option<usize>,
}

#[derive(Deserialize)]
pub(super) struct WriteArgs {
    pub(super) path: String,
    pub(super) content: Option<String>,
    #[serde(rename = "oldString")]
    pub(super) old_string: Option<String>,
    #[serde(rename = "replaceAll")]
    pub(super) replace_all: Option<bool>,
    pub(super) start_line: Option<usize>,
    pub(super) end_line: Option<usize>,
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
pub(super) struct WriteNewArgs {
    #[serde(rename = "filePath")]
    pub(super) file_path: String,
    pub(super) content: String,
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
    #[serde(rename = "task_id")]
    pub(super) task_id: Option<String>,
}

pub(super) fn resolve_workspace_path(cwd: &Path, path: &str) -> Result<PathBuf> {
    let requested = Path::new(path);
    if requested.is_absolute() {
        return Err(anyhow!("path must be relative to workspace (got absolute path: {})", path));
    }
    if requested
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(anyhow!("path must not contain .."));
    }
    Ok(cwd.join(requested))
}

pub(super) fn select_lines(text: &str, start_line: Option<usize>, end_line: Option<usize>) -> Result<String> {
    match (start_line, end_line) {
        (None, None) => Ok(text.to_string()),
        (Some(start), Some(end)) => {
            if start == 0 || end == 0 || start > end {
                return Err(anyhow!("invalid line range"));
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
        _ => Err(anyhow!("start_line and end_line must be provided together")),
    }
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
    let had_crlf = text.contains("\r\n");
    let normalized_text: String;
    let normalized_old: String;
    let normalized_new: String;
    let (text_ref, old_ref, new_ref) = if had_crlf {
        normalized_text = text.replace("\r\n", "\n");
        normalized_old = old_string.replace("\r\n", "\n");
        normalized_new = new_string.replace("\r\n", "\n");
        (normalized_text.as_str(), normalized_old.as_str(), normalized_new.as_str())
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
        let offset = lines[..start - 1].iter().map(|l| l.len() + 1).sum::<usize>();
        let range_text = lines[start - 1..end].join("\n");
        (range_text, offset)
    } else {
        (text_ref.to_string(), 0)
    };

    let matches: Vec<usize> = search_text
        .match_indices(old_ref)
        .map(|(idx, _)| idx)
        .collect();
    if matches.is_empty() {
        if let (Some(s), Some(e)) = (start_line, end_line) {
            return Err(anyhow!(
                "oldString not found in lines [{}, {}]",
                s,
                e
            ));
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
        result.replace_range(
            search_offset..search_offset + search_text.len(),
            &search_text.replace(old_ref, new_ref),
        );
        result
    } else {
        text_ref.replace(old_ref, new_ref)
    };

    // Restore CRLF if the original file used it.
    if had_crlf {
        Ok(result.replace("\n", "\r\n"))
    } else {
        Ok(result)
    }
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
