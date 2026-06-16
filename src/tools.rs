use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::json;

const COMMAND_TIMEOUT_SECS: u64 = 30;
const COMMAND_OUTPUT_LIMIT: usize = 16_000;
const READ_OUTPUT_LIMIT: usize = 32_000;

pub fn openai_tool_specs() -> Vec<serde_json::Value> {
    tool_defs()
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.schema,
                }
            })
        })
        .collect()
}

pub fn anthropic_tool_specs() -> Vec<serde_json::Value> {
    tool_defs()
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.schema,
            })
        })
        .collect()
}

struct ToolDef {
    name: &'static str,
    description: &'static str,
    schema: serde_json::Value,
}

fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read_file",
            description: "Read a UTF-8 text file within the current workspace. Supports optional 1-based inclusive line ranges.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workspace-relative path to read." },
                    "start_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based line to start reading." },
                    "end_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based line to stop reading, inclusive." }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "write_file",
            description: "Write a UTF-8 text file within the current workspace. Without a range, overwrites or creates the file. With a range, replaces that 1-based inclusive line range in an existing file.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workspace-relative path to write." },
                    "content": { "type": "string", "description": "Content to write or insert as replacement." },
                    "start_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based line to start replacing." },
                    "end_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based line to stop replacing, inclusive." }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDef {
            name: "command",
            description: "Run a shell command in the current workspace and return stdout/stderr. On Windows uses pwsh first, then Windows PowerShell. Timeout is 30 seconds.",
            schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Command line to execute." }
                },
                "required": ["command"]
            }),
        },
    ]
}

pub async fn execute_tool(name: &str, args: &str, cwd: &Path) -> String {
    let result = match name {
        "read_file" => read_file(args, cwd).await,
        "write_file" => write_file(args, cwd).await,
        "command" => run_command(args, cwd).await,
        _ => Err(anyhow!("unknown tool: {name}")),
    };

    match result {
        Ok(value) => json!({ "ok": true, "result": value }).to_string(),
        Err(err) => json!({ "ok": false, "error": err.to_string() }).to_string(),
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[derive(Deserialize)]
struct CommandArgs {
    command: String,
}

async fn read_file(args: &str, cwd: &Path) -> Result<String> {
    let args: ReadArgs = serde_json::from_str(args)?;
    let path = resolve_workspace_path(cwd, &args.path)?;
    let text = tokio::fs::read_to_string(&path).await?;
    let selected = select_lines(&text, args.start_line, args.end_line)?;
    Ok(truncate(selected, READ_OUTPUT_LIMIT))
}

async fn write_file(args: &str, cwd: &Path) -> Result<String> {
    let args: WriteArgs = serde_json::from_str(args)?;
    let path = resolve_workspace_path(cwd, &args.path)?;
    match (args.start_line, args.end_line) {
        (Some(start), Some(end)) => {
            if start > end {
                return Err(anyhow!("start_line must be <= end_line"));
            }
            let original = tokio::fs::read_to_string(&path).await?;
            let updated = replace_lines(&original, start, end, &args.content)?;
            tokio::fs::write(&path, updated).await?;
            Ok(format!("replaced lines {start}-{end} in {}", args.path))
        }
        (None, None) => {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&path, args.content).await?;
            Ok(format!("wrote {}", args.path))
        }
        _ => Err(anyhow!("start_line and end_line must be provided together")),
    }
}

async fn run_command(args: &str, cwd: &Path) -> Result<String> {
    let args: CommandArgs = serde_json::from_str(args)?;
    if args.command.trim().is_empty() {
        return Err(anyhow!("command is empty"));
    }

    let output = tokio::time::timeout(
        Duration::from_secs(COMMAND_TIMEOUT_SECS),
        run_shell_command(&args.command, cwd),
    )
    .await
    .map_err(|_| anyhow!("command timed out after {COMMAND_TIMEOUT_SECS}s"))??;

    Ok(truncate(output, COMMAND_OUTPUT_LIMIT))
}

async fn run_shell_command(command: &str, cwd: &Path) -> Result<String> {
    #[cfg(windows)]
    {
        if let Ok(output) = run_shell("pwsh", &["-NoLogo", "-NoProfile", "-Command", command], cwd).await {
            return Ok(output);
        }
        return run_shell("powershell", &["-NoLogo", "-NoProfile", "-Command", command], cwd).await;
    }

    #[cfg(not(windows))]
    {
        run_shell("sh", &["-lc", command], cwd).await
    }
}

async fn run_shell(program: &str, args: &[&str], cwd: &Path) -> Result<String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(format!(
        "exit_code: {}\nstdout:\n{}\nstderr:\n{}",
        output.status.code().map(|c| c.to_string()).unwrap_or_else(|| "terminated".to_string()),
        stdout,
        stderr
    ))
}

fn resolve_workspace_path(cwd: &Path, path: &str) -> Result<PathBuf> {
    let requested = Path::new(path);
    if requested.is_absolute() {
        return Err(anyhow!("path must be relative to workspace"));
    }
    if requested.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(anyhow!("path must not contain .."));
    }
    Ok(cwd.join(requested))
}

fn select_lines(text: &str, start_line: Option<usize>, end_line: Option<usize>) -> Result<String> {
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

fn replace_lines(text: &str, start: usize, end: usize, replacement: &str) -> Result<String> {
    if start == 0 || end == 0 || start > end {
        return Err(anyhow!("invalid line range"));
    }
    let mut lines: Vec<&str> = text.lines().collect();
    if end > lines.len() {
        return Err(anyhow!("line range exceeds file length"));
    }
    let replacement_lines: Vec<&str> = replacement.lines().collect();
    lines.splice(start - 1..end, replacement_lines);
    let mut out = lines.join("\n");
    if text.ends_with('\n') || replacement.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

fn truncate(mut text: String, limit: usize) -> String {
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
