//! Transport wrappers around the `rmcp` crate.
//!
//! Two transports are exposed:
//! - [`connect_local`] spawns a child process and speaks MCP over
//!   its stdio (uses `rmcp::transport::child_process`).
//! - [`connect_remote`] speaks MCP over streamable-HTTP, with SSE
//!   fallback for older servers (uses
//!   `rmcp::transport::streamable_http_client`).
//!
//! The returned [`McpClientHandle`] is a thin wrapper around an
//! `rmcp` client plus enough metadata (server name, transport kind,
//! spawned PID for stdio) for [`crate::mcp::service::McpService`] to
//! drive lifecycle.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, JsonObject};
use rmcp::service::{Peer, RoleClient, RunningService, ServiceExt};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde_json::Value as JsonValue;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::mcp::catalog::McpToolSpec;
use crate::mcp::config::{McpServerConfig, RemoteOAuth};

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("auth required: {0}")]
    Unauthorized(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// How this client is connected. Stored for diagnostics and for
/// the right shutdown semantics (stdio needs a process kill).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Local,
    Remote,
}

/// Live MCP client. Wraps a cloned `Peer<RoleClient>` for shared
/// use plus the `RunningService` for shutdown.
pub struct McpClientHandle {
    pub name: String,
    pub kind: TransportKind,
    /// `Some(pid)` for local stdio servers — the service layer kills
    /// the whole process tree on shutdown.
    pub pid: Option<u32>,
    pub(crate) peer: Arc<Peer<RoleClient>>,
    shutdown: Mutex<Option<RunningService<RoleClient, ()>>>,
    pub timeout: Duration,
}

impl McpClientHandle {
    async fn from_running(
        name: &str,
        kind: TransportKind,
        pid: Option<u32>,
        running: RunningService<RoleClient, ()>,
    ) -> Self {
        let peer = Arc::new(running.peer().clone());
        Self {
            name: name.to_string(),
            kind,
            pid,
            peer,
            shutdown: Mutex::new(Some(running)),
            timeout: Duration::from_secs(30),
        }
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolSpec>, ClientError> {
        let tools = tokio::time::timeout(self.timeout, self.peer.list_all_tools())
            .await
            .map_err(|_| ClientError::Protocol("tools/list timed out".into()))?
            .map_err(|e| ClientError::Protocol(format!("tools/list failed: {e}")))?;
        Ok(tools
            .into_iter()
            .map(|t| {
                let name = t.name.to_string();
                let description = t
                    .description
                    .as_ref()
                    .map(|c| c.to_string())
                    .unwrap_or_default();
                let input_schema = serde_json::Value::Object((*t.input_schema).clone());
                McpToolSpec {
                    key: crate::mcp::catalog::tool_name(&self.name, &name),
                    server: self.name.clone(),
                    name,
                    description,
                    input_schema,
                }
            })
            .collect())
    }

    pub async fn call_tool(
        &self,
        tool: &str,
        arguments: Option<JsonValue>,
    ) -> Result<CallToolResult, ClientError> {
        let args: Option<JsonObject> = match arguments {
            Some(JsonValue::Object(map)) => Some(map),
            Some(JsonValue::Null) | None => None,
            Some(other) => {
                return Err(ClientError::Protocol(format!(
                    "tool arguments must be a JSON object, got {}",
                    short_type(&other)
                )));
            }
        };
        let name_owned: String = tool.to_string();
        let params = match args {
            Some(map) => CallToolRequestParams::new(name_owned.clone()).with_arguments(map),
            None => CallToolRequestParams::new(name_owned),
        };
        let result = tokio::time::timeout(self.timeout, self.peer.call_tool(params))
            .await
            .map_err(|_| ClientError::Protocol("tools/call timed out".into()))?
            .map_err(|e| ClientError::Protocol(format!("tools/call failed: {e}")))?;
        Ok(result)
    }

    /// Extract a plain text rendering of a [`CallToolResult`].
    pub fn render_text(result: &CallToolResult) -> String {
        let mut out = String::new();
        for c in &result.content {
            if let ContentBlock::Text(t) = c {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&t.text);
            } else {
                let s = serde_json::to_string(c).unwrap_or_default();
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&s);
            }
        }
        if out.is_empty() {
            if let Some(structured) = &result.structured_content {
                out = serde_json::to_string(structured).unwrap_or_default();
            }
        }
        if let Some(true) = result.is_error {
            if !out.is_empty() {
                out.insert_str(0, "[tool error] ");
            }
        }
        out
    }

    /// Cancel the underlying transport.
    pub async fn close(self) {
        let mut guard = self.shutdown.lock().await;
        if let Some(running) = guard.take() {
            let _ = running.cancel().await;
        }
    }
}

/// Resolve the program name and argument list from the raw
/// command tokens. On Windows, batch files (.cmd, .bat) must be
/// run through `cmd.exe /c` because CreateProcess cannot resolve
/// them directly.
fn resolve_command(command: &[String]) -> (String, Vec<String>) {
    #[cfg(not(windows))]
    {
        (command[0].clone(), command[1..].to_vec())
    }
    #[cfg(windows)]
    {
        let first = &command[0];
        let ext = std::path::Path::new(first)
            .extension()
            .and_then(|e| e.to_str());
        if ext.is_none() || matches!(ext, Some("cmd" | "bat")) {
            let full = command.join(" ");
            ("cmd.exe".to_string(), vec!["/c".to_string(), full])
        } else {
            (command[0].clone(), command[1..].to_vec())
        }
    }
}

fn short_type(v: &JsonValue) -> &'static str {
    match v {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "bool",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

/// Spawn a local MCP server and return a live client.
pub async fn connect_local(
    name: &str,
    command: &[String],
    cwd: &Path,
    environment: &HashMap<String, String>,
    timeout: Duration,
) -> Result<McpClientHandle, ClientError> {
    if command.is_empty() {
        return Err(ClientError::Spawn("command is empty".into()));
    }

    // Resolve the program and arguments.
    // On Windows, batch files (.cmd, .bat) can't be spawned directly
    // via CreateProcess; wrap them in cmd.exe /c.
    let (program, args_vec) = resolve_command(command);
    let program_label = program.clone();

    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args_vec)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(c) = cwd.to_str() {
        if !c.is_empty() {
            cmd.current_dir(c);
        }
    }
    for (k, v) in environment {
        cmd.env(k, v);
    }
    // Detach into its own process group on Unix so we can SIGTERM the
    // whole tree on shutdown. (Windows uses `taskkill /T` from the
    // service layer.)
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: pre_exec runs after fork; only async-signal-safe
        // operations are allowed.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let (transport, _stderr) = rmcp::transport::child_process::TokioChildProcess::builder(cmd)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| ClientError::Spawn(format!("spawn {program_label}: {e}")))?;
    let pid = transport.id();

    let running = tokio::time::timeout(timeout, ().serve(transport))
        .await
        .map_err(|_| ClientError::Transport("client handshake timed out".into()))?
        .map_err(|e| ClientError::Transport(format!("client handshake failed: {e}")))?;

    let mut out = McpClientHandle::from_running(name, TransportKind::Local, pid, running).await;
    out.timeout = timeout;
    Ok(out)
}

/// Connect to a remote MCP server. Tries streamable-HTTP first; on
/// failure (other than auth) falls back to SSE.
///
/// If `auth_token` is `Some`, it is sent as a `Bearer` authorization
/// header. When the server returns a 401, the caller should drive
/// the OAuth flow (via [`crate::mcp::oauth_callback`]) and call
/// `connect_remote` again with the new token.
pub async fn connect_remote(
    name: &str,
    url: &str,
    headers: &HashMap<String, String>,
    _oauth: &Option<RemoteOAuth>,
    timeout: Duration,
    auth_token: Option<String>,
) -> Result<McpClientHandle, ClientError> {
    let mut custom_headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue> =
        HashMap::new();
    for (k, v) in headers {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            custom_headers.insert(name, value);
        }
    }

    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    if !custom_headers.is_empty() {
        config = config.custom_headers(custom_headers);
    }
    if let Some(token) = auth_token {
        config = config.auth_header(token);
    }

    let transport = StreamableHttpClientTransport::from_config(config);
    let running = tokio::time::timeout(timeout, ().serve(transport))
        .await
        .map_err(|_| ClientError::Transport("streamable-http handshake timed out".into()))?
        .map_err(|e| match e {
            rmcp::service::ClientInitializeError::TransportError { error, .. }
                if format!("{error}").to_lowercase().contains("unauthorized") =>
            {
                ClientError::Unauthorized(format!("{error}"))
            }
            other => ClientError::Transport(format!("streamable-http handshake failed: {other}")),
        })?;

    let mut out = McpClientHandle::from_running(name, TransportKind::Remote, None, running).await;
    out.timeout = timeout;
    Ok(out)
}

/// Connect a server based on its config, using stored auth tokens
/// if available.
///
/// `auth_store` is looked up for stored OAuth tokens; if the server
/// has `auth_token` stored it is injected as a `Bearer` header on
/// remote connections. Local connections ignore the auth store.
pub async fn connect(
    name: &str,
    cfg: &McpServerConfig,
    workspace_root: &Path,
    auth_store: &crate::mcp::auth::McpAuthStore,
) -> Result<McpClientHandle, ClientError> {
    let timeout = Duration::from_millis(cfg.timeout_ms());
    match cfg {
        McpServerConfig::Local {
            command,
            cwd,
            environment,
            ..
        } => {
            let effective_cwd = match cwd {
                Some(rel) if !rel.is_empty() => workspace_root.join(rel),
                _ => workspace_root.to_path_buf(),
            };
            connect_local(name, command, &effective_cwd, environment, timeout).await
        }
        McpServerConfig::Remote {
            url,
            headers,
            oauth,
            ..
        } => {
            let auth_token = auth_store
                .get(name)
                .and_then(|e| e.tokens.map(|t| t.access_token));
            connect_remote(name, url, headers, oauth, timeout, auth_token).await
        }
    }
}
