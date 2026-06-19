use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::json;

const COMMAND_TIMEOUT_SECS: u64 = 300;
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
    description: String,
    schema: serde_json::Value,
}

fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read_file",
            description: "Read a UTF-8 text file within the current workspace. Supports optional 1-based inclusive line ranges.".to_string(),
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
            description: "Write a UTF-8 text file within the current workspace. Without a range, overwrites or creates the file. With a range, replaces that 1-based inclusive line range in an existing file.".to_string(),
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
            name: "shell_command",
            description: format!(
                "Run a shell command in the current workspace using {} and return stdout/stderr. Timeout is 300 seconds.",
                shell_description()
            ),
            schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Command line to execute." }
                },
                "required": ["command"]
            }),
        },
        ToolDef {
            name: "python_command",
            description: "Run Python code in the current workspace and return stdout/stderr. Use this for exact file inspection, small scripts, and deterministic local analysis. Timeout is 300 seconds.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Python source code to execute." }
                },
                "required": ["code"]
            }),
        },
        ToolDef {
            name: "grep",
            description: "Search for literal text in UTF-8 files under a workspace path and return matching file/line snippets.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Literal text to search for." },
                    "path": { "type": "string", "description": "Optional workspace-relative file or directory. Defaults to current workspace." }
                },
                "required": ["pattern"]
            }),
        },
        ToolDef {
            name: "list",
            description: "List files and directories directly under a workspace-relative directory.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Optional workspace-relative directory. Defaults to current workspace." }
                }
            }),
        },
        ToolDef {
            name: "ask",
            description: "Ask the user a question in the function panel. Use when you need a decision before proceeding.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string" },
                    "options": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["question"]
            }),
        },
        ToolDef {
            name: "todo",
            description: "Publish or update a todo list in the function panel.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string" },
                                "status": { "type": "string" }
                            },
                            "required": ["content"]
                        }
                    }
                },
                "required": ["items"]
            }),
        },
        ToolDef {
            name: "plan",
            description: "Present a plan for user confirmation in the function panel before executing it.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "content": { "type": "string" },
                    "steps": { "type": "array", "items": { "type": "string" } }
                }
            }),
        },
    ]
}

pub async fn execute_tool(name: &str, args: &str, cwd: &Path) -> String {
    let result = match name {
        "read_file" => read_file(args, cwd).await,
        "write_file" => write_file(args, cwd).await,
        "shell_command" | "command" => run_command(args, cwd).await,
        "python_command" => run_python_command(args, cwd).await,
        "grep" => grep_text(args, cwd).await,
        "list" => list_path(args, cwd).await,
        "ask" => ask_user(args).await,
        "todo" => todo_items(args).await,
        "plan" => plan_review(args).await,
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

#[derive(Deserialize)]
struct PythonArgs {
    code: String,
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
}

#[derive(Deserialize)]
struct ListArgs {
    path: Option<String>,
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
            tokio::fs::write(&path, &updated).await?;
            Ok(write_diff_result(&args.path, &original, &updated))
        }
        (None, None) => {
            let original = match tokio::fs::read_to_string(&path).await {
                Ok(text) => text,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(err) => return Err(err.into()),
            };
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&path, &args.content).await?;
            Ok(write_diff_result(&args.path, &original, &args.content))
        }
        _ => Err(anyhow!("start_line and end_line must be provided together")),
    }
}

fn write_diff_result(path: &str, old: &str, new: &str) -> String {
    json!({
        "kind": "write_file_diff",
        "path": path,
        "old": old,
        "new": new,
    })
    .to_string()
}

async fn grep_text(args: &str, cwd: &Path) -> Result<String> {
    let args: GrepArgs = serde_json::from_str(args)?;
    if args.pattern.is_empty() {
        return Err(anyhow!("pattern is empty"));
    }
    let rel = args.path.unwrap_or_else(|| ".".to_string());
    let root = resolve_workspace_path(cwd, &rel)?;
    let mut out = Vec::new();
    grep_path(&root, &args.pattern, cwd, &mut out, 200)?;
    if out.is_empty() {
        Ok(format!("no matches for {:?} in {}", args.pattern, rel))
    } else {
        Ok(truncate(out.join("\n"), READ_OUTPUT_LIMIT))
    }
}

fn grep_path(
    path: &Path,
    pattern: &str,
    cwd: &Path,
    out: &mut Vec<String>,
    limit: usize,
) -> Result<()> {
    if out.len() >= limit {
        return Ok(());
    }
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let p = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".git" || name == "target" {
                continue;
            }
            grep_path(&p, pattern, cwd, out, limit)?;
            if out.len() >= limit {
                break;
            }
        }
    } else if path.is_file() {
        if let Ok(text) = std::fs::read_to_string(path) {
            let rel = path.strip_prefix(cwd).unwrap_or(path).display().to_string();
            for (idx, line) in text.lines().enumerate() {
                if line.contains(pattern) {
                    out.push(format!("{}:{}:{}", rel, idx + 1, line));
                    if out.len() >= limit {
                        out.push("[match limit reached]".to_string());
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

async fn list_path(args: &str, cwd: &Path) -> Result<String> {
    let args: ListArgs = serde_json::from_str(args)?;
    let rel = args.path.unwrap_or_else(|| ".".to_string());
    let path = resolve_workspace_path(cwd, &rel)?;
    if !path.is_dir() {
        return Err(anyhow!("path is not a directory"));
    }
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(&path)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let mut name = entry.file_name().to_string_lossy().to_string();
        if meta.is_dir() {
            name.push('/');
        }
        rows.push(name);
    }
    rows.sort();
    Ok(rows.join("\n"))
}

async fn ask_user(args: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(args)?;
    let question = value
        .get("question")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if question.is_empty() {
        return Err(anyhow!("question is empty"));
    }
    let options = value
        .get("options")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(json!({ "kind": "ask", "question": question, "options": options, "status": "pending_user_answer", "instruction": "Wait for the user answer shown in the function panel before assuming a choice." }).to_string())
}

async fn todo_items(args: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(args)?;
    let items = value
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(json!({ "kind": "todo", "items": items }).to_string())
}

async fn plan_review(args: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(args)?;
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("Plan");
    let content = value
        .get("content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            value
                .get("steps")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .enumerate()
                        .map(|(i, s)| format!("{}. {}", i + 1, s))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        });
    if content.trim().is_empty() {
        return Err(anyhow!("plan content is empty"));
    }
    Ok(json!({ "kind": "plan", "title": title, "content": content, "status": "pending_user_confirmation", "instruction": "Stop and wait for user approval before executing this plan." }).to_string())
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

async fn run_python_command(args: &str, cwd: &Path) -> Result<String> {
    let args: PythonArgs = serde_json::from_str(args)?;
    if args.code.trim().is_empty() {
        return Err(anyhow!("python code is empty"));
    }
    let output = tokio::time::timeout(
        Duration::from_secs(COMMAND_TIMEOUT_SECS),
        run_python(&args.code, cwd),
    )
    .await
    .map_err(|_| anyhow!("python command timed out after {COMMAND_TIMEOUT_SECS}s"))??;
    Ok(json!({
        "kind": "python_command_result",
        "code": args.code,
        "output": truncate(output, COMMAND_OUTPUT_LIMIT),
    })
    .to_string())
}

async fn run_python(code: &str, cwd: &Path) -> Result<String> {
    #[cfg(windows)]
    {
        match run_shell("python", &["-X", "utf8", "-c", code], cwd).await {
            Ok(output) => Ok(output),
            Err(_) => run_shell("py", &["-3", "-X", "utf8", "-c", code], cwd).await,
        }
    }

    #[cfg(not(windows))]
    {
        match run_shell("python3", &["-c", code], cwd).await {
            Ok(output) => Ok(output),
            Err(_) => run_shell("python", &["-c", code], cwd).await,
        }
    }
}

async fn run_shell_command(command: &str, cwd: &Path) -> Result<String> {
    #[cfg(windows)]
    {
        let utf8_preamble = "\
$OutputEncoding = [Console]::OutputEncoding = \
[System.Text.UTF8Encoding]::UTF8; \
$env:PYTHONIOENCODING='utf-8'; ";
        let full_cmd = format!("{utf8_preamble}{command}");
        let shell = windows_shell_program();
        return run_shell(
            shell,
            &["-NoLogo", "-NoProfile", "-Command", &full_cmd],
            cwd,
        )
        .await;
    }

    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
        run_shell(&shell, &["-lc", command], cwd).await
    }
}

pub fn shell_guidance() -> String {
    #[cfg(windows)]
    {
        format!(
            "OS is Windows; shell is {} (PowerShell syntax). `ls` is Get-ChildItem; do not use Unix flags like `ls -la`. Use `Get-ChildItem -Force` or `dir` for hidden/all files.",
            windows_shell_program()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "OS is Unix-like; shell is {}.",
            std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string())
        )
    }
}

pub fn shell_description() -> String {
    #[cfg(windows)]
    {
        windows_shell_program().to_string()
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string())
    }
}

#[cfg(windows)]
fn windows_shell_program() -> &'static str {
    static SHELL: OnceLock<&'static str> = OnceLock::new();
    SHELL.get_or_init(|| {
        if std::process::Command::new("pwsh")
            .arg("-NoLogo")
            .arg("-NoProfile")
            .arg("-Command")
            .arg("$PSVersionTable.PSVersion | Out-Null")
            .status()
            .is_ok()
        {
            "pwsh"
        } else {
            "powershell"
        }
    })
}

async fn run_shell(program: &str, args: &[&str], cwd: &Path) -> Result<String> {
    let started = Instant::now();
    let output = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = strip_ansi(&stdout);
    let stderr = strip_ansi(&stderr);
    Ok(format!(
        "exit_code: {}\nwall_secs: {:.2}\ntimeout_secs: {}\nstdout:\n{}\nstderr:\n{}",
        output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "terminated".to_string()),
        started.elapsed().as_secs_f64(),
        COMMAND_TIMEOUT_SECS,
        stdout,
        stderr
    ))
}

fn strip_ansi(s: &str) -> String {
    let bytes = strip_ansi_escapes::strip(s);
    String::from_utf8_lossy(&bytes).to_string()
}

fn resolve_workspace_path(cwd: &Path, path: &str) -> Result<PathBuf> {
    let requested = Path::new(path);
    if requested.is_absolute() {
        return Err(anyhow!("path must be relative to workspace"));
    }
    if requested
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
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

pub fn is_valid_tool(name: &str) -> bool {
    matches!(
        name,
        "read_file"
            | "write_file"
            | "shell_command"
            | "python_command"
            | "grep"
            | "list"
            | "ask"
            | "todo"
            | "plan"
            | "command"
    )
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
