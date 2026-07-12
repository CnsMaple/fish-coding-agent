use super::super::ChatEvent;
use super::proto::*;
use super::{cursor_debug, send_cursor_client_message, CursorServerOutcome};
use anyhow::Result;
use std::path::PathBuf;
use tokio::sync::mpsc;

pub(super) async fn handle_exec_server_message(
    exec: ExecServerMessage,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<CursorServerOutcome> {
    match exec.message {
        Some(exec_server_message::Message::RequestContextArgs(_)) => {
            cursor_debug(
                tx,
                format!(
                    "exec request_context id={} exec_id={}",
                    exec.id, exec.exec_id
                ),
            );
            let reply = AgentClientMessage {
                message: Some(agent_client_message::Message::ExecClientMessage(
                    ExecClientMessage {
                        id: exec.id,
                        exec_id: exec.exec_id,
                        message: Some(exec_client_message::Message::RequestContextResult(
                            RequestContextResult {
                                result: Some(request_context_result::Result::Success(
                                    RequestContextSuccess {
                                        request_context: Some(RequestContext::default()),
                                    },
                                )),
                            },
                        )),
                    },
                )),
            };
            send_cursor_client_message(body_tx, reply).await?;
            Ok(CursorServerOutcome::Meaningful)
        }
        Some(exec_server_message::Message::ShellArgs(args)) => {
            cursor_debug(
                tx,
                format!(
                    "exec shell_args id={} command={}",
                    exec.id,
                    args.command.trim()
                ),
            );
            handle_shell_exec(exec.id, exec.exec_id, args, false, tx, body_tx).await?;
            Ok(CursorServerOutcome::ToolOutput)
        }
        Some(exec_server_message::Message::ShellStreamArgs(args)) => {
            cursor_debug(
                tx,
                format!(
                    "exec shell_stream_args id={} command={}",
                    exec.id,
                    args.command.trim()
                ),
            );
            handle_shell_exec(exec.id, exec.exec_id, args, true, tx, body_tx).await?;
            Ok(CursorServerOutcome::ToolOutput)
        }
        Some(exec_server_message::Message::ReadArgs(args)) => {
            cursor_debug(
                tx,
                format!("exec read_args id={} path={}", exec.id, args.path),
            );
            handle_read_exec(exec.id, exec.exec_id, args, tx, body_tx).await?;
            Ok(CursorServerOutcome::ToolOutput)
        }
        Some(exec_server_message::Message::LsArgs(args)) => {
            cursor_debug(
                tx,
                format!("exec ls_args id={} path={}", exec.id, args.path),
            );
            handle_ls_exec(exec.id, exec.exec_id, args, tx, body_tx).await?;
            Ok(CursorServerOutcome::ToolOutput)
        }
        Some(exec_server_message::Message::GrepArgs(args)) => {
            cursor_debug(
                tx,
                format!("exec grep_args id={} pattern={}", exec.id, args.pattern),
            );
            handle_grep_exec(exec.id, exec.exec_id, args, tx, body_tx).await?;
            Ok(CursorServerOutcome::ToolOutput)
        }
        None => {
            cursor_debug(
                tx,
                format!(
                    "exec unsupported_unknown id={} exec_id={} (ignored)",
                    exec.id, exec.exec_id
                ),
            );
            Ok(CursorServerOutcome::Continue)
        }
    }
}

pub(super) async fn handle_read_exec(
    id: u32,
    exec_id: String,
    args: ReadArgs,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let path = resolve_cursor_path(&cwd, &args.path);
    let result = match tokio::fs::read_to_string(&path).await {
        Ok(content) => {
            let total_lines = content.lines().count() as i32;
            let file_size = content.len() as i64;
            read_result::Result::Success(ReadSuccess {
                path: args.path.clone(),
                total_lines,
                file_size,
                truncated: false,
                output_blob_id: None,
                output: Some(read_success::Output::Content(content)),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            read_result::Result::FileNotFound(ReadFileNotFound {
                path: args.path.clone(),
            })
        }
        Err(e) => read_result::Result::Error(ReadError {
            path: args.path.clone(),
            error: e.to_string(),
        }),
    };
    let display = match &result {
        read_result::Result::Success(s) => match &s.output {
            Some(read_success::Output::Content(c)) => c.clone(),
            Some(read_success::Output::Data(d)) => format!("[binary data: {} bytes]", d.len()),
            None => String::new(),
        },
        read_result::Result::Error(e) => format!("[read error] {}", e.error),
        read_result::Result::FileNotFound(_) => "[file not found]".to_string(),
        read_result::Result::Rejected(e) => format!("[read rejected] {}", e.reason),
        read_result::Result::PermissionDenied(_) => "[permission denied]".to_string(),
        read_result::Result::InvalidFile(e) => format!("[invalid file] {}", e.reason),
    };
    let _ = tx.send(ChatEvent::ToolResult {
        name: "read".to_string(),
        title: format!("[read] {}", args.path),
        content: display,
    });
    send_cursor_client_message(
        body_tx,
        AgentClientMessage {
            message: Some(agent_client_message::Message::ExecClientMessage(
                ExecClientMessage {
                    id,
                    exec_id,
                    message: Some(exec_client_message::Message::ReadResult(ReadResult {
                        result: Some(result),
                    })),
                },
            )),
        },
    )
    .await
}

pub(super) async fn handle_ls_exec(
    id: u32,
    exec_id: String,
    args: LsArgs,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let target = if args.path.trim().is_empty() {
        "."
    } else {
        args.path.trim()
    };
    let path = resolve_cursor_path(&cwd, target);
    let (result, display) = match build_ls_tree(&path, 2) {
        Ok(root) => {
            let display = format_ls_tree(&root, 0);
            (
                ls_result::Result::Success(LsSuccess {
                    directory_tree_root: Some(root),
                }),
                display,
            )
        }
        Err(e) => (
            ls_result::Result::Error(LsError {
                path: target.to_string(),
                error: e.to_string(),
            }),
            format!("[ls error] {e}"),
        ),
    };
    let _ = tx.send(ChatEvent::ToolResult {
        name: "list".to_string(),
        title: format!("[list] {}", target),
        content: display,
    });
    send_cursor_client_message(
        body_tx,
        AgentClientMessage {
            message: Some(agent_client_message::Message::ExecClientMessage(
                ExecClientMessage {
                    id,
                    exec_id,
                    message: Some(exec_client_message::Message::LsResult(LsResult {
                        result: Some(result),
                    })),
                },
            )),
        },
    )
    .await
}

pub(super) async fn handle_grep_exec(
    id: u32,
    exec_id: String,
    args: GrepArgs,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let tool_args = serde_json::json!({
        "pattern": args.pattern,
        "path": args.path.unwrap_or_else(|| ".".to_string()),
    })
    .to_string();
    let content = crate::tools::execute_tool("grep", &tool_args, &cwd).await;
    let _ = tx.send(ChatEvent::ToolResult {
        name: "grep".to_string(),
        title: "[grep]".to_string(),
        content: content.clone(),
    });
    let result = grep_result::Result::Error(GrepError { error: content });
    send_cursor_client_message(
        body_tx,
        AgentClientMessage {
            message: Some(agent_client_message::Message::ExecClientMessage(
                ExecClientMessage {
                    id,
                    exec_id,
                    message: Some(exec_client_message::Message::GrepResult(GrepResult {
                        result: Some(result),
                    })),
                },
            )),
        },
    )
    .await
}

pub(super) fn resolve_cursor_path(cwd: &std::path::Path, path: &str) -> PathBuf {
    let p = PathBuf::from(path.trim());
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}

pub(super) fn build_ls_tree(path: &std::path::Path, depth: usize) -> std::io::Result<LsDirectoryTreeNode> {
    let abs_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut node = LsDirectoryTreeNode {
        abs_path: abs_path.display().to_string(),
        children_dirs: Vec::new(),
        children_files: Vec::new(),
        children_were_processed: false,
        full_subtree_extension_counts: std::collections::HashMap::new(),
        num_files: 0,
    };
    if depth == 0 || !path.is_dir() {
        return Ok(node);
    }
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        if file_name == ".git" || file_name == "target" {
            continue;
        }
        let meta = entry.metadata()?;
        if meta.is_dir() {
            dirs.push(entry.path());
        } else if meta.is_file() {
            files.push(file_name);
        }
    }
    dirs.sort();
    files.sort();
    for dir in dirs.into_iter().take(64) {
        if let Ok(child) = build_ls_tree(&dir, depth.saturating_sub(1)) {
            node.num_files += child.num_files;
            node.children_dirs.push(child);
        }
    }
    for file in files.into_iter().take(256) {
        if let Some(ext) = std::path::Path::new(&file)
            .extension()
            .and_then(|e| e.to_str())
        {
            *node
                .full_subtree_extension_counts
                .entry(ext.to_string())
                .or_insert(0) += 1;
        }
        node.num_files += 1;
        node.children_files.push(LsDirectoryTreeNodeFile {
            name: file,
            terminal_metadata: None,
        });
    }
    node.children_were_processed = true;
    Ok(node)
}

pub(super) fn format_ls_tree(node: &LsDirectoryTreeNode, indent: usize) -> String {
    let mut out = String::new();
    let name = std::path::Path::new(&node.abs_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&node.abs_path);
    out.push_str(&format!("{}{}\n", "  ".repeat(indent), name));
    for dir in &node.children_dirs {
        out.push_str(&format_ls_tree(dir, indent + 1));
    }
    for file in &node.children_files {
        out.push_str(&format!("{}{}\n", "  ".repeat(indent + 1), file.name));
    }
    out
}

#[cfg(windows)]
pub(super) fn normalize_cursor_shell_command(command: &str) -> String {
    match command.trim() {
        "ls -la" | "ls -al" | "ls --all -l" | "ls -l -a" => "Get-ChildItem -Force".to_string(),
        "ls -a" | "ls --all" => "Get-ChildItem -Force".to_string(),
        "ls -l" => "Get-ChildItem".to_string(),
        other => other.to_string(),
    }
}

#[cfg(not(windows))]
pub(super) fn normalize_cursor_shell_command(command: &str) -> String {
    command.trim().to_string()
}

pub(super) async fn handle_shell_exec(
    id: u32,
    exec_id: String,
    args: ShellArgs,
    stream: bool,
    tx: &mpsc::UnboundedSender<ChatEvent>,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let command = normalize_cursor_shell_command(args.command.trim());
    let cwd = if args.working_directory.trim().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(args.working_directory.trim())
    };
    let tool_args = serde_json::json!({ "command": command }).to_string();
    let content = crate::tools::execute_tool("shell_command", &tool_args, &cwd).await;
    let shell_content = crate::session::unwrap_tool_result_content(&content);
    let _ = tx.send(ChatEvent::ToolResult {
        name: "shell_command".to_string(),
        title: format!("$ {}", command),
        content: content.clone(),
    });

    let parsed = ParsedShellOutput::parse(&shell_content);
    cursor_debug(
        tx,
        format!(
            "exec shell_result exit={} stdout={} stderr={} stream={}",
            parsed.exit_code,
            parsed.stdout.len(),
            parsed.stderr.len(),
            stream
        ),
    );
    if stream {
        send_shell_stream_event(
            id,
            exec_id.clone(),
            body_tx,
            shell_stream::Event::Start(ShellStreamStart {}),
        )
        .await?;
        if !parsed.stdout.is_empty() {
            send_shell_stream_event(
                id,
                exec_id.clone(),
                body_tx,
                shell_stream::Event::Stdout(ShellStreamStdout {
                    data: parsed.stdout.clone(),
                }),
            )
            .await?;
        }
        if !parsed.stderr.is_empty() {
            send_shell_stream_event(
                id,
                exec_id.clone(),
                body_tx,
                shell_stream::Event::Stderr(ShellStreamStderr {
                    data: parsed.stderr.clone(),
                }),
            )
            .await?;
        }
        send_shell_stream_event(
            id,
            exec_id.clone(),
            body_tx,
            shell_stream::Event::Exit(ShellStreamExit {
                code: parsed.exit_code.max(0) as u32,
                cwd: cwd.display().to_string(),
                aborted: false,
            }),
        )
        .await?;
    }

    let result = if parsed.exit_code == 0 && !content.starts_with("[Tool Error]") {
        shell_result::Result::Success(ShellSuccess {
            command,
            working_directory: cwd.display().to_string(),
            exit_code: parsed.exit_code,
            signal: String::new(),
            stdout: parsed.stdout,
            stderr: parsed.stderr,
            execution_time: parsed.execution_time_ms,
        })
    } else {
        shell_result::Result::Failure(ShellFailure {
            command,
            working_directory: cwd.display().to_string(),
            exit_code: parsed.exit_code,
            signal: String::new(),
            stdout: parsed.stdout,
            stderr: parsed.stderr,
            execution_time: parsed.execution_time_ms,
            aborted: false,
        })
    };
    send_cursor_client_message(
        body_tx,
        AgentClientMessage {
            message: Some(agent_client_message::Message::ExecClientMessage(
                ExecClientMessage {
                    id,
                    exec_id: exec_id.clone(),
                    message: Some(exec_client_message::Message::ShellResult(ShellResult {
                        result: Some(result),
                    })),
                },
            )),
        },
    )
    .await?;
    if stream {
        cursor_debug(tx, format!("exec stream_close id={id}"));
        send_exec_stream_close(id, body_tx).await?;
    }
    Ok(())
}

pub(super) async fn send_shell_stream_event(
    id: u32,
    exec_id: String,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    event: shell_stream::Event,
) -> Result<()> {
    send_cursor_client_message(
        body_tx,
        AgentClientMessage {
            message: Some(agent_client_message::Message::ExecClientMessage(
                ExecClientMessage {
                    id,
                    exec_id,
                    message: Some(exec_client_message::Message::ShellStream(ShellStream {
                        event: Some(event),
                    })),
                },
            )),
        },
    )
    .await
}

pub(super) async fn send_exec_stream_close(
    id: u32,
    body_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    send_cursor_client_message(
        body_tx,
        AgentClientMessage {
            message: Some(agent_client_message::Message::ExecClientControlMessage(
                ExecClientControlMessage {
                    message: Some(exec_client_control_message::Message::StreamClose(
                        ExecClientStreamClose { id },
                    )),
                },
            )),
        },
    )
    .await
}

struct ParsedShellOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
    execution_time_ms: i32,
}

impl ParsedShellOutput {
    fn parse(content: &str) -> Self {
        let exit_code = extract_header_value(content, "exit_code:")
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(-1);
        let execution_time_ms = extract_header_value(content, "wall_secs:")
            .and_then(|s| s.parse::<f64>().ok())
            .map(|s| (s * 1000.0).round() as i32)
            .unwrap_or(0);
        Self {
            exit_code,
            stdout: extract_section(
                content,
                "stdout:
",
                "
stderr:
",
            )
            .unwrap_or_default(),
            stderr: extract_after(
                content,
                "
stderr:
",
            )
            .unwrap_or_default(),
            execution_time_ms,
        }
    }
}

pub(super) fn extract_header_value<'a>(content: &'a str, key: &str) -> Option<&'a str> {
    content
        .lines()
        .find_map(|line| line.strip_prefix(key).map(str::trim))
}

pub(super) fn extract_section(content: &str, start: &str, end: &str) -> Option<String> {
    let rest = content.split_once(start)?.1;
    let value = rest.split_once(end).map(|(v, _)| v).unwrap_or(rest);
    Some(value.to_string())
}

pub(super) fn extract_after(content: &str, start: &str) -> Option<String> {
    Some(content.split_once(start)?.1.to_string())
}
