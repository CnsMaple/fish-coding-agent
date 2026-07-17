use super::file::{
    ask_question, glob_search, grep_text, list_path, plan_review, read_file, skill_load,
    split_edit_diff, todowrite, write_file, write_new_file,
};
use super::web::{run_command, run_python_command, strip_ansi, webfetch, websearch};
use super::*;

pub async fn execute_tool(name: &str, args: &str, cwd: &Path) -> String {
    execute_tool_with_agent(crate::permission::Agent::Build, name, args, cwd).await
}

pub async fn execute_tool_with_agent(
    agent: crate::permission::Agent,
    name: &str,
    args: &str,
    cwd: &Path,
) -> String {
    use crate::permission::{tool as t, Action, Agent};
    if matches!(crate::permission::check(agent, name), Action::Deny) {
        let hint = match agent {
            Agent::Plan => " (plan mode is read-only; switch to /yolo to edit or run commands)",
            _ => "",
        };
        return json!({
            "ok": false,
            "error": format!("tool `{name}` is not allowed in {} mode{}", agent.as_str(), hint),
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
        t::SUB_AGENT => Err(anyhow!(
            "sub_agent must be executed from within the chat stream loop"
        )),
        _ => Err(anyhow!("unknown tool: {name}")),
    };

    match result {
        Ok(value) => {
            let (ai, metadata) = split_edit_diff(name, &value);
            let mut obj = json!({ "ok": true, "result": ai });
            if !metadata.is_empty() {
                obj["metadata"] = json!(metadata);
            }
            obj.to_string()
        }
        Err(err) => json!({ "ok": false, "error": err.to_string() }).to_string(),
    }
}

/// Execute a tool with streaming output support.
/// For shell/python commands, output is streamed via ToolDelta messages.
/// For other tools, falls back to non-streaming execution.
/// `call_id` routes ToolDelta to the correct block during parallel execution.
pub async fn execute_tool_streaming(
    name: &str,
    args: &str,
    cwd: &Path,
    call_id: &str,
    tx: UnboundedSender<AppMsg>,
) -> String {
    execute_tool_streaming_with_agent(
        crate::permission::Agent::Build,
        name,
        args,
        cwd,
        call_id,
        tx,
    )
    .await
}

pub async fn execute_tool_streaming_with_agent(
    agent: crate::permission::Agent,
    name: &str,
    args: &str,
    cwd: &Path,
    call_id: &str,
    tx: UnboundedSender<AppMsg>,
) -> String {
    use crate::permission::{tool as t, Action, Agent};
    if matches!(crate::permission::check(agent, name), Action::Deny) {
        let hint = match agent {
            Agent::Plan => " (plan mode is read-only; switch to /yolo to edit or run commands)",
            _ => "",
        };
        return json!({
            "ok": false,
            "error": format!("tool `{name}` is not allowed in {} mode{}", agent.as_str(), hint),
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
                    let _ = tx.send(AppMsg::ToolDelta {
                        call_id: call_id.to_string(),
                        content: rendered.clone(),
                    });
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
        t::SHELL_COMMAND | "command" => run_command_streaming(args, cwd, call_id, tx)
            .await
            .unwrap_or_else(|e| json!({ "ok": false, "error": e.to_string() }).to_string()),
        t::PYTHON_COMMAND => run_python_streaming(args, cwd, call_id, tx)
            .await
            .unwrap_or_else(|e| json!({ "ok": false, "error": e.to_string() }).to_string()),
        _ => execute_tool_with_agent(agent, name, args, cwd).await,
    };

    // Intentionally NOT truncated here. Streaming results are shown
    // in full in the TUI; callers truncate the AI-facing payload
    // themselves before adding it to the LLM context.
    result
}

/// Heuristic: a tool name is treated as an MCP tool when the live
/// service knows it. Falls back to the built-in list otherwise.
pub(super) fn is_mcp_tool_name(name: &str) -> bool {
    if let Some(svc) = crate::mcp::McpRegistry::current() {
        if let Ok(snap) = svc.try_snapshot() {
            return snap.tools.contains_key(name);
        }
    }
    false
}

pub(super) async fn run_command_streaming(
    args: &str,
    cwd: &Path,
    call_id: &str,
    tx: UnboundedSender<AppMsg>,
) -> Result<String> {
    let cmd_args: CommandArgs = serde_json::from_str(args)?;
    if cmd_args.command.trim().is_empty() {
        return Err(anyhow!("command is empty"));
    }

    let timeout_secs = cmd_args.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let output = run_shell_streaming(&cmd_args.command, cwd, call_id, tx, timeout_secs).await?;

    Ok(truncate(output, COMMAND_OUTPUT_LIMIT))
}

pub(super) async fn run_python_streaming(
    args: &str,
    cwd: &Path,
    call_id: &str,
    tx: UnboundedSender<AppMsg>,
) -> Result<String> {
    let py_args: PythonArgs = serde_json::from_str(args)?;
    if py_args.code.trim().is_empty() {
        return Err(anyhow!("python code is empty"));
    }
    let timeout_secs = py_args.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let output = run_python_streaming_inner(&py_args.code, cwd, call_id, tx, timeout_secs).await?;

    Ok(json!({
        "kind": "python_command_result",
        "code": py_args.code,
        "output": truncate(output, COMMAND_OUTPUT_LIMIT),
    })
    .to_string())
}

pub(super) async fn run_shell_streaming(
    command: &str,
    cwd: &Path,
    call_id: &str,
    tx: UnboundedSender<AppMsg>,
    timeout_secs: u64,
) -> Result<String> {
    let (shell, args) = build_shell_args(command);
    run_shell_streaming_impl(&shell, &args, cwd, call_id, tx, timeout_secs).await
}

pub(super) async fn run_python_streaming_inner(
    code: &str,
    cwd: &Path,
    call_id: &str,
    tx: UnboundedSender<AppMsg>,
    timeout_secs: u64,
) -> Result<String> {
    let invocations = python_invocations(code);
    let mut last_err = None;
    for (program, args) in &invocations {
        match run_shell_streaming_impl(program, args, cwd, call_id, tx.clone(), timeout_secs).await
        {
            Ok(output) => return Ok(output),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no python invocations")))
}

/// Core streaming shell implementation.
/// Spawns a process with piped stdout/stderr, reads lines as they arrive,
/// sends them via ToolDelta, and returns the full accumulated output.
pub(super) async fn run_shell_streaming_impl(
    program: &str,
    args: &[String],
    cwd: &Path,
    call_id: &str,
    tx: UnboundedSender<AppMsg>,
    timeout_secs: u64,
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
        .kill_on_drop(true)
        .spawn()?;

    // Take stdout/stderr handles
    let stdout_reader = child.stdout.take().map(tokio::io::BufReader::new);
    let stderr_reader = child.stderr.take().map(tokio::io::BufReader::new);
    let call_id = call_id.to_string();

    // Buffers shared between the read tasks and the timeout guard.
    // `Arc<Mutex<String>>` so the timeout branch can read what was
    // already streamed before killing the child.
    let stdout_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));

    // Read stdout and stderr concurrently
    let stdout_task = {
        let stdout_buf = stdout_buf.clone();
        let call_id = call_id.clone();
        let tx = tx.clone();
        async move {
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
                                call_id: call_id.clone(),
                                content: clean,
                            });
                        }
                        Err(_) => break,
                    }
                }
            }
            if let Ok(mut shared) = stdout_buf.lock() {
                shared.push_str(&buf);
            }
        }
    };

    let stderr_task = {
        let stderr_buf = stderr_buf.clone();
        let call_id = call_id.clone();
        async move {
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
                                call_id: call_id.clone(),
                                content: format!("{tag}{clean}"),
                            });
                        }
                        Err(_) => break,
                    }
                }
            }
            if let Ok(mut shared) = stderr_buf.lock() {
                shared.push_str(&buf);
            }
        }
    };

    // Race the read tasks against the wall-clock timeout. On
    // timeout, kill the child so the outer future resolves; the
    // buffers still hold whatever was streamed before the deadline.
    let timed_out = tokio::select! {
        _ = async { tokio::join!(stdout_task, stderr_task); } => false,
        _ = tokio::time::sleep(Duration::from_secs(timeout_secs)) => true,
    };

    let mut exit_code;
    if timed_out {
        // Best-effort kill; ignore errors (process may have exited
        // on its own between the deadline and the kill).
        let _ = child.kill().await;
        exit_code = "124".to_string();
    } else {
        let status = child.wait().await?;
        exit_code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "terminated".to_string());
    }

    // Flush any trailing bytes the read tasks pushed into the shared
    // buffers before the deadline fired (select! may have cut them
    // off mid-read).
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Ok(shared) = stdout_buf.lock() {
        stdout.push_str(&shared);
    }
    if let Ok(shared) = stderr_buf.lock() {
        stderr.push_str(&shared);
    }
    let stdout = strip_ansi(&stdout);
    let mut stderr = strip_ansi(&stderr);
    if timed_out {
        // Make it obvious in the output that the deadline was hit,
        // not a normal exit-124.
        stderr.push_str(&format!("\n[command timed out after {timeout_secs}s]"));
        if exit_code == "124" {
            exit_code = "timeout".to_string();
        }
    }
    Ok(format_command_output(
        &exit_code,
        started.elapsed(),
        timeout_secs,
        &stdout,
        &stderr,
    ))
}
