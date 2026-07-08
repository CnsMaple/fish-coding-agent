use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::Datelike;
use glob::Pattern;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;

use crate::event::AppMsg;
use crate::mcp::McpRegistry;

const COMMAND_TIMEOUT_SECS: u64 = 300;
const COMMAND_OUTPUT_LIMIT: usize = 16_000;
const READ_OUTPUT_LIMIT: usize = 32_000;

/// Maximum number of MCP tools that may be advertised to the LLM.
/// Protects against a misconfigured server that exports tens of
/// thousands of tools from blowing the prompt budget.
const MCP_TOOL_LIMIT: usize = 256;

/// Maximum length of a tool description we'll send to the LLM.
/// Truncates with an ellipsis when longer; matches the opencode
/// behaviour in `McpCatalog.convertTool`.
const MCP_DESC_LIMIT: usize = 200;

pub fn openai_tool_specs() -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = tool_defs()
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
        .collect();
    out.extend(mcp_specs_for_openai());
    out
}

pub fn anthropic_tool_specs() -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = tool_defs()
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.schema,
            })
        })
        .collect();
    out.extend(mcp_specs_for_anthropic());
    out
}

/// Return tool specs filtered for a sub-agent type. Sub-agents may
/// not have access to all tools (e.g. `explore` is read-only).
pub fn openai_tool_specs_for_sub_agent(
    sub_agent: crate::permission::SubAgent,
) -> Vec<serde_json::Value> {
    openai_tool_specs()
        .into_iter()
        .filter(|spec| {
            let name = spec["function"]["name"].as_str().unwrap_or("");
            matches!(
                crate::permission::check_sub_agent(sub_agent, name),
                crate::permission::Action::Allow
            )
        })
        .collect()
}

pub fn anthropic_tool_specs_for_sub_agent(
    sub_agent: crate::permission::SubAgent,
) -> Vec<serde_json::Value> {
    anthropic_tool_specs()
        .into_iter()
        .filter(|spec| {
            let name = spec["name"].as_str().unwrap_or("");
            matches!(
                crate::permission::check_sub_agent(sub_agent, name),
                crate::permission::Action::Allow
            )
        })
        .collect()
}

/// Read the current MCP tool list and convert it to the OpenAI
/// tool-spec shape. Returns an empty Vec when the service is not
/// installed or has no connected tools.
fn mcp_specs_for_openai() -> Vec<serde_json::Value> {
    mcp_tool_iter()
        .into_iter()
        .map(|(key, description, schema)| {
            json!({
                "type": "function",
                "function": {
                    "name": key,
                    "description": description,
                    "parameters": schema,
                }
            })
        })
        .collect()
}

fn mcp_specs_for_anthropic() -> Vec<serde_json::Value> {
    mcp_tool_iter()
        .into_iter()
        .map(|(key, description, schema)| {
            json!({
                "name": key,
                "description": description,
                "input_schema": schema,
            })
        })
        .collect()
}

/// Collect `(key, description, schema)` triples from the live MCP
/// service. Strips to the first `MCP_TOOL_LIMIT` entries; bounds
/// the description length.
fn mcp_tool_iter() -> Vec<(String, String, serde_json::Value)> {
    let Some(svc) = McpRegistry::current() else {
        return Vec::new();
    };
    let snap = match svc.try_snapshot() {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<(String, String, serde_json::Value)> = snap
        .tools
        .values()
        .map(|t| {
            let mut desc = t.description.clone();
            if desc.chars().count() > MCP_DESC_LIMIT {
                desc = desc.chars().take(MCP_DESC_LIMIT).collect::<String>() + "…";
            }
            if desc.is_empty() {
                desc = format!("MCP tool `{name}` (server: {server})", name = t.name, server = t.server);
            } else {
                desc = format!("[mcp:{server}] {desc}", server = t.server);
            }
            (t.key.clone(), desc, t.input_schema.clone())
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.truncate(MCP_TOOL_LIMIT);
    out
}

struct ToolDef {
    name: &'static str,
    description: String,
    schema: serde_json::Value,
}

fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read",
            description: "Read a UTF-8 text file within the current workspace. Supports optional 1-based inclusive line ranges.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workspace-relative path to read." },
                    "start_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based line to start reading." },
                    "end_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based line to stop reading, inclusive." }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "edit",
            description: "Write or edit a UTF-8 text file within the current workspace. Use this tool for all file modifications including creating new files and editing existing ones. To edit, provide oldString (the exact text to find and replace) with the replacement content. When oldString matches multiple locations, use start_line/end_line to narrow the search scope, or use replaceAll: true. To create or overwrite a file, omit oldString.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workspace-relative path to write." },
                    "content": { "type": "string", "description": "Content to write, or replacement text when oldString is provided." },
                    "oldString": { "type": "string", "description": "Exact text to find and replace in the file. Must be unique within the search scope (whole file or specified line range). Omit to create/overwrite the entire file." },
                    "replaceAll": { "type": "boolean", "description": "Replace all occurrences of oldString. Default false (requires unique match)." },
                    "start_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based line to start searching for oldString. Must be used with end_line." },
                    "end_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based line to stop searching for oldString, inclusive. Must be used with start_line." }
                },
                "required": ["path", "content"],
                "additionalProperties": false
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
                "required": ["command"],
                "additionalProperties": false
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
                "required": ["code"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "grep",
            description: "Search for a regex pattern in UTF-8 files under a workspace path and return matching file/line snippets.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern to search for." },
                    "path": { "type": "string", "description": "Optional workspace-relative file or directory. Defaults to current workspace." }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "list",
            description: "List files and directories directly under a workspace-relative directory.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Optional workspace-relative directory. Defaults to current workspace." }
                },
                "required": [],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "plan",
            description: "Present a plan for user confirmation in the function panel before executing it.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short plan title. Defaults to 'Plan'." },
                    "content": { "type": "string", "description": "Full plan text. Provide this or steps." },
                    "steps": { "type": "array", "items": { "type": "string" }, "description": "Optional list of step strings, rendered as a numbered list. Used when content is not provided." }
                },
                "required": [],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "ask",
            description: "Ask the user a clarifying question. The question is shown in the session and as a toast. The user types their answer into the main input; the conversation resumes when they submit. Use this in plan mode to confirm tradeoffs before drafting a plan, and in build mode when a single decision blocks the next step.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question to present to the user." },
                    "options": { "type": "array", "items": { "type": "string" }, "description": "Optional list of suggested answers; rendered as bullets under the question." }
                },
                "required": ["question"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "todowrite",
            description: "Create and maintain a structured task list for the current coding session. Tracks progress, organizes multi-step work, and surfaces status to the user.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "Description of the task." },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"], "description": "Task status." }
                            },
                            "required": ["content", "status"]
                        },
                        "description": "Full list of todo items to replace the current task list. Each call must send ALL items (existing + new/changed), not just the diff."
                    }
                },
                "required": ["todos"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "glob",
            description: "Fast file pattern matching tool. Supports glob patterns like \"**/*.rs\" or \"src/**/*.ts\". Returns matching file paths sorted by modification time. Use this tool when you need to find files by name patterns. It is always better to speculatively perform multiple searches as a batch that are potentially useful.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "The glob pattern to match files against." },
                    "path": { "type": "string", "description": "Optional workspace-relative directory to search in. Defaults to current workspace." }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "write",
            description: "Writes a file to the local filesystem.\n\nUsage:\n- This tool will overwrite the existing file if there is one at the provided path.\n- If this is an existing file, you MUST use the Read tool first to read the file's contents.\n- ALWAYS prefer editing existing files in the codebase. NEVER write new files unless explicitly required.\n- NEVER proactively create documentation files (*.md) or README files. Only create documentation files if explicitly requested by the User.\n- Only use emojis if the user explicitly requests it. Avoid writing emojis to files unless asked.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "filePath": { "type": "string", "description": "The absolute path to the file to write (must be absolute, not relative)." },
                    "content": { "type": "string", "description": "The content to write to the file." }
                },
                "required": ["filePath", "content"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "skill",
            description: "Load a specialized skill when the task at hand matches one of the skills listed in the system prompt.\n\nUse this tool to inject the skill's instructions and resources into current conversation. The output may contain detailed workflow guidance as well as references to scripts, files, etc in the same directory as the skill.\n\nThe skill name must match one of the skills listed in your system prompt.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The name of the skill from available_skills" }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "webfetch",
            description: "Fetches content from a specified URL and returns it in the requested format.\n\nUsage notes:\n- The URL must be a fully-formed valid URL\n- HTTP URLs will be automatically upgraded to HTTPS\n- Format options: \"markdown\" (default), \"text\", or \"html\"\n- This tool is read-only and does not modify any files\n- Results may be summarized if the content is very large".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to fetch content from" },
                    "format": { "type": "string", "enum": ["text", "markdown", "html"], "description": "The format to return the content in (text, markdown, or html). Defaults to markdown." }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "websearch",
            description: "Search the web for information. Provides up-to-date information for current events and recent data. Use this tool for accessing information beyond knowledge cutoff.\n\nUsage notes:\n- Supports configurable result counts\n- Returns the content from the most relevant websites\n- Searches are performed automatically within a single API call".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The search query" },
                    "numResults": { "type": "integer", "minimum": 1, "maximum": 20, "description": "Number of search results to return (default 8)" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "sub_agent",
            description: "Launch a new agent to handle complex, multistep tasks autonomously.\n\nWhen using the sub_agent tool, you must specify a subagent_type parameter to select which agent type to use.\n\nWhen NOT to use the sub_agent tool:\n- If you want to read a specific file path, use the Read or Glob tool instead\n- If you are searching for a specific class definition, use the Grep tool instead\n- If you are searching for code within a specific file or set of 2-3 files, use the Read tool instead\n- If no available agent is a good fit for the task, use other tools directly\n\nUsage notes:\n1. Launch multiple agents concurrently whenever possible\n2. Once you have delegated work to an agent, do not duplicate that work yourself\n3. When the agent is done, it will return a single message back to you\n4. Each agent invocation starts with a fresh context\n5. The agent's outputs should generally be trusted\n6. Clearly tell the agent whether you expect it to write code or just to do research\n7. If the agent description mentions that it should be used proactively, use your best judgement\n\nAvailable agent types:\n- general: General-purpose agent for complex questions and multi-step tasks. Has full tool access.\n- explore: Fast agent specialized for exploring codebases. Use this when you need to quickly find files by patterns, search code for keywords, or answer questions about the codebase. When calling this agent, specify the desired thoroughness level: \"quick\" for basic searches, \"medium\" for moderate exploration, or \"very thorough\" for comprehensive analysis.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string", "description": "A short (3-5 words) description of the task" },
                    "prompt": { "type": "string", "description": "The task for the agent to perform" },
                    "subagent_type": { "type": "string", "enum": ["general", "explore"], "description": "The type of specialized agent to use for this task" },
                    "task_id": { "type": "string", "description": "Optional: resume a previous sub-agent session" }
                },
                "required": ["description", "prompt", "subagent_type"],
                "additionalProperties": false
            }),
        },
    ]
}

pub async fn execute_tool(name: &str, args: &str, cwd: &Path) -> String {
    execute_tool_with_agent(crate::permission::Agent::Build, name, args, cwd).await
}

pub async fn execute_tool_with_agent(
    agent: crate::permission::Agent,
    name: &str,
    args: &str,
    cwd: &Path,
) -> String {
    use crate::permission::{tool as t, Action};
    if matches!(crate::permission::check(agent, name), Action::Deny) {
        return json!({
            "ok": false,
            "error": format!("tool `{name}` is not allowed in {} mode", agent.as_str()),
        })
        .to_string();
    }
    let result = match name {
        t::READ_FILE => read_file(args, cwd).await,
t::WRITE_FILE => write_file(args, cwd).await,
        t::SHELL_COMMAND | "command" => run_command(args, cwd).await,
        t::PYTHON_COMMAND => run_python_command(args, cwd).await,
        t::GREP => grep_text(args, cwd).await,
        t::LIST => list_path(args, cwd).await,
        t::PLAN => plan_review(args).await,
        t::ASK => ask_question(args).await,
        t::TODO_WRITE => todowrite(args).await,
        t::GLOB => glob_search(args, cwd).await,
        t::WRITE => write_new_file(args, cwd).await,
        t::SKILL => skill_load(args).await,
        t::WEB_FETCH => webfetch(args).await,
        t::WEB_SEARCH => websearch(args).await,
        t::SUB_AGENT => Err(anyhow!("sub_agent must be executed from within the chat stream loop")),
        _ => Err(anyhow!("unknown tool: {name}")),
    };

    match result {
        Ok(value) => json!({ "ok": true, "result": value }).to_string(),
        Err(err) => json!({ "ok": false, "error": err.to_string() }).to_string(),
    }
}

/// Execute a tool with streaming output support.
/// For shell/python commands, output is streamed via ToolDelta messages.
/// For other tools, falls back to non-streaming execution.
pub async fn execute_tool_streaming(
    name: &str,
    args: &str,
    cwd: &Path,
    tx: UnboundedSender<AppMsg>,
) -> String {
    execute_tool_streaming_with_agent(
        crate::permission::Agent::Build,
        name,
        args,
        cwd,
        tx,
    )
    .await
}

pub async fn execute_tool_streaming_with_agent(
    agent: crate::permission::Agent,
    name: &str,
    args: &str,
    cwd: &Path,
    tx: UnboundedSender<AppMsg>,
) -> String {
    use crate::permission::{tool as t, Action};
    if matches!(crate::permission::check(agent, name), Action::Deny) {
        return json!({
            "ok": false,
            "error": format!("tool `{name}` is not allowed in {} mode", agent.as_str()),
        })
        .to_string();
    }
    // MCP tool dispatch. The tool name is `<server>_<tool>`; if
    // the live service knows it, run it through the MCP client.
    if is_mcp_tool_name(name) {
        if let Some(svc) = crate::mcp::McpRegistry::current() {
            let arguments = if args.trim().is_empty() {
                serde_json::Value::Null
            } else {
                match serde_json::from_str::<serde_json::Value>(args) {
                    Ok(v) => v,
                    Err(e) => {
                        return json!({
                            "ok": false,
                            "error": format!("mcp tool arguments must be JSON: {e}"),
                        })
                        .to_string();
                    }
                }
            };
            return match svc.call_tool(name, arguments).await {
                Ok(rendered) => {
                    let _ = tx.send(AppMsg::ToolDelta { content: rendered.clone() });
                    json!({ "ok": true, "result": rendered }).to_string()
                }
                Err(e) => json!({ "ok": false, "error": format!("mcp error: {e}") }).to_string(),
            };
        }
        return json!({
            "ok": false,
            "error": format!("mcp service is not initialised; tool `{name}` cannot run"),
        })
        .to_string();
    }
    let result = match name {
        t::SHELL_COMMAND | "command" => run_command_streaming(args, cwd, tx)
            .await
            .unwrap_or_else(|e| json!({ "ok": false, "error": e.to_string() }).to_string()),
        t::PYTHON_COMMAND => run_python_streaming(args, cwd, tx)
            .await
            .unwrap_or_else(|e| json!({ "ok": false, "error": e.to_string() }).to_string()),
        _ => execute_tool_with_agent(agent, name, args, cwd).await,
    };

    result
}

/// Heuristic: a tool name is treated as an MCP tool when the live
/// service knows it. Falls back to the built-in list otherwise.
fn is_mcp_tool_name(name: &str) -> bool {
    if let Some(svc) = crate::mcp::McpRegistry::current() {
        if let Ok(snap) = svc.try_snapshot() {
            return snap.tools.contains_key(name);
        }
    }
    false
}

async fn run_command_streaming(
    args: &str,
    cwd: &Path,
    tx: UnboundedSender<AppMsg>,
) -> Result<String> {
    let cmd_args: CommandArgs = serde_json::from_str(args)?;
    if cmd_args.command.trim().is_empty() {
        return Err(anyhow!("command is empty"));
    }

    let output = tokio::time::timeout(
        Duration::from_secs(COMMAND_TIMEOUT_SECS),
        run_shell_streaming(&cmd_args.command, cwd, tx),
    )
    .await
    .map_err(|_| anyhow!("command timed out after {COMMAND_TIMEOUT_SECS}s"))??;

    Ok(truncate(output, COMMAND_OUTPUT_LIMIT))
}

async fn run_python_streaming(
    args: &str,
    cwd: &Path,
    tx: UnboundedSender<AppMsg>,
) -> Result<String> {
    let py_args: PythonArgs = serde_json::from_str(args)?;
    if py_args.code.trim().is_empty() {
        return Err(anyhow!("python code is empty"));
    }
    let output = tokio::time::timeout(
        Duration::from_secs(COMMAND_TIMEOUT_SECS),
        run_python_streaming_inner(&py_args.code, cwd, tx),
    )
    .await
    .map_err(|_| anyhow!("python command timed out after {COMMAND_TIMEOUT_SECS}s"))??;

    Ok(json!({
        "kind": "python_command_result",
        "code": py_args.code,
        "output": truncate(output, COMMAND_OUTPUT_LIMIT),
    })
    .to_string())
}

async fn run_shell_streaming(
    command: &str,
    cwd: &Path,
    tx: UnboundedSender<AppMsg>,
) -> Result<String> {
    #[cfg(windows)]
    {
        let utf8_preamble = "\
$OutputEncoding = [Console]::OutputEncoding = \
[System.Text.UTF8Encoding]::UTF8; \
$env:PYTHONIOENCODING='utf-8'; ";
        let full_cmd = format!("{utf8_preamble}{command}");
        let shell = windows_shell_program();
        run_shell_streaming_impl(
            shell,
            &["-NoLogo", "-NoProfile", "-Command", &full_cmd],
            cwd,
            tx,
        )
        .await
    }

    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
        run_shell_streaming_impl(&shell, &["-lc", command], cwd, tx).await
    }
}

async fn run_python_streaming_inner(
    code: &str,
    cwd: &Path,
    tx: UnboundedSender<AppMsg>,
) -> Result<String> {
    #[cfg(windows)]
    {
        match run_shell_streaming_impl("python", &["-X", "utf8", "-c", code], cwd, tx.clone()).await
        {
            Ok(output) => Ok(output),
            Err(_) => {
                run_shell_streaming_impl("py", &["-3", "-X", "utf8", "-c", code], cwd, tx).await
            }
        }
    }

    #[cfg(not(windows))]
    {
        match run_shell_streaming_impl("python3", &["-c", code], cwd, tx.clone()).await {
            Ok(output) => Ok(output),
            Err(_) => run_shell_streaming_impl("python", &["-c", code], cwd, tx).await,
        }
    }
}

/// Core streaming shell implementation.
/// Spawns a process with piped stdout/stderr, reads lines as they arrive,
/// sends them via ToolDelta, and returns the full accumulated output.
async fn run_shell_streaming_impl(
    program: &str,
    args: &[&str],
    cwd: &Path,
    tx: UnboundedSender<AppMsg>,
) -> Result<String> {
    use std::process::Stdio;
    use tokio::io::AsyncBufReadExt;

    let started = Instant::now();
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();

    // Take stdout/stderr handles
    let stdout_reader = child.stdout.take().map(tokio::io::BufReader::new);
    let stderr_reader = child.stderr.take().map(tokio::io::BufReader::new);

    // Read stdout and stderr concurrently
    let stdout_task = async {
        let mut buf = String::new();
        if let Some(mut reader) = stdout_reader {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        buf.push_str(&line);
                        let clean = strip_ansi(&line);
                        let _ = tx.send(AppMsg::ToolDelta {
                            content: clean,
                        });
                    }
                    Err(_) => break,
                }
            }
        }
        buf
    };

    let stderr_task = async {
        let mut buf = String::new();
        if let Some(mut reader) = stderr_reader {
            let mut line = String::new();
            loop {
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let tag = "stderr: ";
                        buf.push_str(&line);
                        let clean = strip_ansi(&line);
                        let _ = tx.send(AppMsg::ToolDelta {
                            content: format!("{tag}{clean}"),
                        });
                    }
                    Err(_) => break,
                }
            }
        }
        buf
    };

    let (stdout, stderr) = tokio::join!(stdout_task, stderr_task);
    stdout_buf.push_str(&stdout);
    stderr_buf.push_str(&stderr);

    let status = child.wait().await?;
    let stdout = strip_ansi(&stdout_buf);
    let stderr = strip_ansi(&stderr_buf);

    Ok(format!(
        "exit_code: {}\nwall_secs: {:.2}\ntimeout_secs: {}\nstdout:\n{}\nstderr:\n{}",
        status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "terminated".to_string()),
        started.elapsed().as_secs_f64(),
        COMMAND_TIMEOUT_SECS,
        stdout,
        stderr
    ))
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
    #[serde(rename = "oldString")]
    old_string: Option<String>,
    #[serde(rename = "replaceAll")]
    replace_all: Option<bool>,
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

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
}

#[derive(Deserialize)]
struct WriteNewArgs {
    #[serde(rename = "filePath")]
    file_path: String,
    content: String,
}

#[derive(Deserialize)]
struct SkillArgs {
    name: String,
}

#[derive(Deserialize)]
struct WebFetchArgs {
    url: String,
    format: Option<String>,
}

#[derive(Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(rename = "numResults")]
    num_results: Option<usize>,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
struct SubAgentArgs {
    description: String,
    prompt: String,
    #[serde(rename = "subagent_type")]
    subagent_type: String,
    #[serde(rename = "task_id")]
    task_id: Option<String>,
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
    if let Some(old_string) = &args.old_string {
        if old_string.is_empty() {
            return Err(anyhow!("oldString must not be empty"));
        }
        let original = tokio::fs::read_to_string(&path).await?;
        let updated = replace_string(
            &original,
            old_string,
            &args.content,
            args.replace_all.unwrap_or(false),
            args.start_line,
            args.end_line,
        )?;
        tokio::fs::write(&path, &updated).await?;
        Ok(write_diff_result(&args.path, &original, &updated))
    } else {
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
}

fn write_diff_result(path: &str, old: &str, new: &str) -> String {
    json!({
        "kind": "edit_diff",
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
    let re = Regex::new(&args.pattern).map_err(|e| anyhow!("invalid regex pattern: {e}"))?;
    let rel = args.path.unwrap_or_else(|| ".".to_string());
    let root = resolve_workspace_path(cwd, &rel)?;
    let mut out = Vec::new();
    grep_path(&root, &re, cwd, &mut out, 200)?;
    if out.is_empty() {
        Ok(format!("no matches for {:?} in {}", args.pattern, rel))
    } else {
        Ok(truncate(out.join("\n"), READ_OUTPUT_LIMIT))
    }
}

fn grep_path(
    path: &Path,
    re: &Regex,
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
            grep_path(&p, re, cwd, out, limit)?;
            if out.len() >= limit {
                break;
            }
        }
    } else if path.is_file() {
        if let Ok(text) = std::fs::read_to_string(path) {
            let rel = path.strip_prefix(cwd).unwrap_or(path).display().to_string();
            for (idx, line) in text.lines().enumerate() {
                if re.is_match(line) {
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
        .or_else(|| {
            value.get("content").map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
        })
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
        return Err(anyhow!("plan content or steps must be non-empty. Provide 'content' (a string describing the plan) or 'steps' (an array of step strings)."));
    }
    Ok(json!({
        "kind": "plan",
        "title": title,
        "content": content,
        "status": "pending",
        "instruction": "Do not call this tool again. The plan is now shown to the user in the function panel. Stop and wait for the user to approve, reject, or request changes. The user will submit their decision and the conversation will resume automatically -- you will be re-prompted with the user's response."
    }).to_string())
}

async fn ask_question(args: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(args)?;
    let question = value
        .get("question")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if question.is_empty() {
        return Err(anyhow!("question is empty"));
    }
    let options: Vec<String> = value
        .get("options")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "kind": "ask",
        "question": question,
        "options": options,
        "status": "pending",
        "instruction": "Do not call this tool again. The question is now shown to the user in the session. Stop and wait for the user to type their answer into the main input. Their reply will be sent back to you automatically -- you will be re-prompted with the user's response."
    })
    .to_string())
}

async fn todowrite(args: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(args)?;
    let todos = value
        .get("todos")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("todowrite: missing or invalid `todos` array"))?;
    if todos.is_empty() {
        return Ok(json!({
            "kind": "todowrite",
            "action": "clear",
            "todos": [],
            "status": "ok",
            "summary": "Todo list cleared."
        })
        .to_string());
    }
    let mut validated = Vec::new();
    for (i, item) in todos.iter().enumerate() {
        let content = item
            .get("content")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("todowrite: todos[{}] missing or empty `content`", i))?;
        let status = item
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("pending");
        let status = match status {
            "pending" | "in_progress" | "completed" => status,
            _ => return Err(anyhow!(
                "todowrite: todos[{}] invalid status `{status}` (must be pending, in_progress, or completed)", i
            )),
        };
        validated.push(json!({
            "content": content,
            "status": status,
        }));
    }
    let pending = validated.iter().filter(|v| v["status"] == "pending").count();
    let in_progress = validated.iter().filter(|v| v["status"] == "in_progress").count();
    let completed = validated.iter().filter(|v| v["status"] == "completed").count();
    Ok(json!({
        "kind": "todowrite",
        "action": "replace",
        "todos": validated,
        "status": "ok",
        "summary": format!("{} pending, {} in progress, {} completed", pending, in_progress, completed),
    })
    .to_string())
}

async fn glob_search(args: &str, cwd: &Path) -> Result<String> {
    let args: GlobArgs = serde_json::from_str(args)?;
    if args.pattern.trim().is_empty() {
        return Err(anyhow!("pattern is empty"));
    }
    let rel = args.path.unwrap_or_else(|| ".".to_string());
    let root = resolve_workspace_path(cwd, &rel)?;
    if !root.is_dir() {
        return Err(anyhow!("glob path must be a directory: {}", rel));
    }
    let pattern = Pattern::new(&args.pattern).map_err(|e| anyhow!("invalid glob pattern: {e}"))?;
    let mut matches: Vec<(String, std::time::SystemTime)> = Vec::new();
    collect_glob_matches(&root, &root, &pattern, &mut matches, 100)?;
    matches.sort_by(|a, b| b.1.cmp(&a.1));
    let mut out = matches.into_iter().map(|(p, _)| p).collect::<Vec<_>>();
    if out.is_empty() {
        return Ok("No files found".to_string());
    }
    if out.len() >= 100 {
        out.push("[results truncated at 100 — narrow your search]".to_string());
    }
    Ok(out.join("\n"))
}

fn collect_glob_matches(
    search_root: &Path,
    current: &Path,
    pattern: &Pattern,
    out: &mut Vec<(String, std::time::SystemTime)>,
    limit: usize,
) -> Result<()> {
    if out.len() >= limit {
        return Ok(());
    }
    if current.is_dir() {
        let dir = std::fs::read_dir(current)?;
        for entry in dir {
            let entry = entry?;
            let p = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".git" || name == "target" || name == "node_modules" {
                continue;
            }
            let rel = p.strip_prefix(search_root).unwrap_or(&p).display().to_string();
            let rel_path = Path::new(&rel);
            if p.is_dir() {
                collect_glob_matches(search_root, &p, pattern, out, limit)?;
            } else if pattern.matches_path(rel_path) {
                if let Ok(meta) = p.metadata() {
                    out.push((rel, meta.modified().unwrap_or(std::time::UNIX_EPOCH)));
                } else {
                    out.push((rel, std::time::UNIX_EPOCH));
                }
                if out.len() >= limit {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

async fn write_new_file(args: &str, cwd: &Path) -> Result<String> {
    let args: WriteNewArgs = serde_json::from_str(args)?;
    if args.file_path.trim().is_empty() {
        return Err(anyhow!("filePath is empty"));
    }
    let path = resolve_workspace_path(cwd, &args.file_path)?;
    let original = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err.into()),
    };
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, &args.content).await?;
    Ok(write_diff_result(&args.file_path, &original, &args.content))
}

async fn skill_load(args: &str) -> Result<String> {
    let args: SkillArgs = serde_json::from_str(args)?;
    let name = args.name.trim();
    if name.is_empty() {
        return Err(anyhow!("skill name is empty"));
    }
    let Some(skill) = crate::skill::find(name) else {
        return Err(anyhow!(
            "skill not found: `{name}`. Available skills: {}",
            crate::skill::list_names().join(", ")
        ));
    };
    let skill_dir = crate::skill::skill_path(name)
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let mut file_list = Vec::new();
    if let Some(ref dir) = skill_dir {
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut files: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                .filter(|e| e.file_name().to_string_lossy() != "SKILL.md")
                .map(|e| e.path().display().to_string())
                .take(10)
                .collect();
            files.sort();
            file_list = files;
        }
    }
    let base_dir = skill_dir
        .as_ref()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| "(unknown)".to_string());
    let mut out = format!(
        "<skill_content name=\"{name}\">\n# Skill: {name}\n\n{content}\n\nBase directory for this skill: {base}\nRelative paths in this skill are relative to this base directory.\nNote: file list is sampled.\n",
        name = skill.name,
        content = skill.template,
        base = base_dir,
    );
    if !file_list.is_empty() {
        out.push_str("<skill_files>\n");
        for f in &file_list {
            out.push_str(&format!("<file>{f}</file>\n"));
        }
        out.push_str("</skill_files>\n");
    }
    out.push_str("</skill_content>");
    Ok(out)
}

async fn webfetch(args: &str) -> Result<String> {
    let args: WebFetchArgs = serde_json::from_str(args)?;
    let url = args.url.trim();
    if url.is_empty() {
        return Err(anyhow!("url is empty"));
    }
    let url = if url.starts_with("http://") {
        url.replace("http://", "https://")
    } else if !url.starts_with("https://") {
        return Err(anyhow!("URL must start with http:// or https://"));
    } else {
        url.to_string()
    };
    let format = args.format.as_deref().unwrap_or("markdown");
    if !["text", "markdown", "html"].contains(&format) {
        return Err(anyhow!("format must be text, markdown, or html"));
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36")
        .build()?;
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("HTTP {status}"));
    }
    let body = resp.text().await?;
    if body.len() > 5 * 1024 * 1024 {
        return Err(anyhow!("Response too large (exceeds 5MB limit)"));
    }
    match format {
        "html" => Ok(body),
        "text" => Ok(html_to_text(&body)),
        _ => Ok(html_to_markdown(&body)),
    }
}

fn html_to_text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 80)
}

fn html_to_markdown(html: &str) -> String {
    let text = html2text::from_read(html.as_bytes(), 0);
    if text == html || text.trim().is_empty() {
        return html.to_string();
    }
    text
}

async fn websearch(args: &str) -> Result<String> {
    let args: WebSearchArgs = serde_json::from_str(args)?;
    let query = args.query.trim();
    if query.is_empty() {
        return Err(anyhow!("query is empty"));
    }
    let num_results = args.num_results.unwrap_or(8).clamp(1, 20);
    let year = chrono::Utc::now().year();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        .build()?;
    let body = serde_json::json!({
        "method": "web_search_exa",
        "params": {
            "query": query,
            "numResults": num_results,
            "type": "auto",
            "contextMaxCharacters": 10000
        }
    });
    let resp = client
        .post("https://mcp.exa.ai/api")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow!("search failed (HTTP {status}): {body}"));
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let text = v
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or(&body);
    if text.trim().is_empty() {
        return Ok("No search results found. Please try a different query.".to_string());
    }
    Ok(format!("Search results for \"{query}\" ({year}):\n\n{text}"))
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

pub fn os_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "Windows"
    }
    #[cfg(target_os = "linux")]
    {
        "Linux"
    }
    #[cfg(target_os = "macos")]
    {
        "macOS"
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        std::env::consts::OS
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

fn replace_string(
    text: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<String> {
    let (search_text, search_offset) = if let (Some(start), Some(end)) = (start_line, end_line) {
        if start == 0 || end == 0 || start > end {
            return Err(anyhow!("start_line must be <= end_line and >= 1"));
        }
        let lines: Vec<&str> = text.lines().collect();
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
        (text.to_string(), 0)
    };

    let matches: Vec<usize> = search_text
        .match_indices(old_string)
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
        return Err(anyhow!(
            "oldString found {} times; {}",
            matches.len(),
            hint
        ));
    }

    if start_line.is_some() {
        let mut result = text.to_string();
        result.replace_range(
            search_offset..search_offset + search_text.len(),
            &search_text.replace(old_string, new_string),
        );
        Ok(result)
    } else {
        Ok(text.replace(old_string, new_string))
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

#[cfg(test)]
mod tests {
    use super::*;

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
            execute_tool_with_agent(crate::permission::Agent::Build, "edit", &args, &dir)
                .await;
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v.get("ok").and_then(|s| s.as_bool()), Some(true));
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
    async fn ask_tool_payload_contains_kind_and_question() {
        let result = execute_tool_with_agent(
            crate::permission::Agent::Plan,
            "ask",
            r#"{"question":"which API?","options":["v1","v2"]}"#,
            Path::new("."),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v.get("ok").and_then(|s| s.as_bool()), Some(true));
        let inner: serde_json::Value =
            serde_json::from_str(v.get("result").and_then(|s| s.as_str()).unwrap()).unwrap();
        assert_eq!(inner.get("kind").and_then(|s| s.as_str()), Some("ask"));
        assert_eq!(
            inner.get("question").and_then(|s| s.as_str()),
            Some("which API?")
        );
        let options = inner.get("options").and_then(|s| s.as_array()).unwrap();
        assert_eq!(options.len(), 2);
    }

    #[tokio::test]
    async fn ask_tool_rejects_empty_question() {
        let result = execute_tool_with_agent(
            crate::permission::Agent::Build,
            "ask",
            r#"{"question":"   "}"#,
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
}
