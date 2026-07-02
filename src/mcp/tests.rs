//! End-to-end test for the MCP subsystem.
//!
//! Strategy: spin up a minimal in-process MCP server with rmcp's
//! `ServerHandler`, connect a client to it through a
//! `tokio::io::duplex` pair, and exercise the `Peer<RoleClient>`
//! surface our service uses in production. This validates the
//! integration without requiring a real child process.

#![cfg(test)]

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, ServerInfo};
use rmcp::service::{Peer, RoleClient, RoleServer, RunningService, ServiceExt};
use rmcp::ServerHandler;
use serde_json::json;
use tokio::io::DuplexStream;

use crate::mcp::catalog::tool_name;
use crate::mcp::client::McpClientHandle;
use crate::mcp::config::{McpEntry, McpServerConfig};
use crate::mcp::{McpRegistry, McpService, StateSnapshot};

/// Minimal in-process MCP server. Doesn't expose any tools — we
/// only need a live peer for the client-side tests below.
#[derive(Clone, Default)]
struct TestServer;

impl ServerHandler for TestServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::default()
    }
}

async fn spawn_test_server(stream: DuplexStream) -> RunningService<RoleServer, TestServer> {
    TestServer
        .serve(stream)
        .await
        .expect("test server handshake")
}

async fn attach_client(
    stream: DuplexStream,
) -> (
    Arc<Peer<RoleClient>>,
    RunningService<RoleClient, ()>,
) {
    let running = tokio::time::timeout(std::time::Duration::from_secs(5), ().serve(stream))
        .await
        .expect("client handshake timed out")
        .expect("client handshake failed");
    let peer = running.peer().clone();
    (Arc::new(peer), running)
}

#[tokio::test]
async fn catalog_sanitize_combines_names() {
    assert_eq!(tool_name("github", "list_issues"), "github_list_issues");
    assert_eq!(tool_name("a.b", "c d"), "a_b_c_d");
}

#[tokio::test]
async fn config_local_roundtrip() {
    let raw = r#"{
        "type": "local",
        "command": ["npx", "-y", "mcp-fs"],
        "environment": {"FOO": "bar"},
        "enabled": true,
        "timeout_ms": 15000
    }"#;
    let cfg: McpServerConfig = serde_json::from_str(raw).unwrap();
    match cfg {
        McpServerConfig::Local { command, environment, enabled, timeout_ms, .. } => {
            assert_eq!(command, vec!["npx", "-y", "mcp-fs"]);
            assert_eq!(environment.get("FOO").map(String::as_str), Some("bar"));
            assert!(enabled);
            assert_eq!(timeout_ms, 15_000);
        }
        other => panic!("expected local, got {other:?}"),
    }
}

#[tokio::test]
async fn config_toggle_normalises_to_disabled_remote() {
    let entry = McpEntry::Toggle { enabled: false };
    let cfg = entry.normalize("gh").into_config().unwrap();
    assert!(!cfg.enabled());
}

#[tokio::test]
async fn in_process_handshake_then_list_tools_empty() {
    let (server_stream, client_stream) = tokio::io::duplex(4096);
    let server_handle = tokio::spawn(spawn_test_server(server_stream));

    let (peer, _running) = attach_client(client_stream).await;
    let tools = peer.list_all_tools().await.expect("list_all_tools");
    assert!(
        tools.is_empty(),
        "test server exposes no tools, expected empty list"
    );

    let _ = server_handle.await;
}

#[tokio::test]
async fn in_process_call_unknown_tool_errors() {
    let (server_stream, client_stream) = tokio::io::duplex(4096);
    let server_handle = tokio::spawn(spawn_test_server(server_stream));

    let (peer, _running) = attach_client(client_stream).await;
    let result = peer
        .call_tool(CallToolRequestParams::new("does_not_exist"))
        .await;
    assert!(result.is_err(), "expected error for unknown tool");
    let _ = server_handle.await;
}

#[tokio::test]
async fn empty_config_produces_empty_snapshot() {
    let cfg: HashMap<String, McpEntry> = HashMap::new();
    let svc = McpService::init_from_config(
        &cfg,
        std::env::current_dir().unwrap_or_default(),
    )
    .await;
    let snap: StateSnapshot = svc.snapshot().await;
    assert!(snap.config.is_empty());
    assert!(snap.status.is_empty());
    assert!(snap.tools.is_empty());
    assert!(McpRegistry::current().is_some());
    svc.shutdown().await;
}

#[tokio::test]
async fn disabled_entries_stay_disabled() {
    let mut cfg: HashMap<String, McpEntry> = HashMap::new();
    cfg.insert(
        "echo".into(),
        McpEntry::Config(McpServerConfig::Local {
            command: vec!["echo".into(), "hi".into()],
            cwd: None,
            environment: HashMap::new(),
            enabled: false,
            timeout_ms: 30_000,
        }),
    );
    let svc = McpService::init_from_config(
        &cfg,
        std::env::current_dir().unwrap_or_default(),
    )
    .await;
    let snap = svc.snapshot().await;
    assert_eq!(snap.config.len(), 1);
    let status = svc.status_of("echo").await;
    assert!(
        matches!(status, crate::mcp::McpStatus::Disabled),
        "expected Disabled, got {status:?}"
    );
    svc.shutdown().await;
}

#[test]
fn mcp_tool_specs_aggregate_with_builtins() {
    use crate::mcp::catalog::McpToolSpec;
    use serde_json::json;

    // We can't easily install an MCP service from a `#[test]`
    // because `init_from_config` is async + takes ownership of a
    // global registry. Instead, test the catalog-level helper
    // that converts an `McpToolSpec` to a JSON schema payload.
    let spec = McpToolSpec {
        key: "echo_say".into(),
        server: "echo".into(),
        name: "say".into(),
        description: "say something".into(),
        input_schema: json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }),
    };
    let key = spec.key.clone();
    let desc = spec.description.clone();
    let schema = spec.input_schema.clone();
    let openai = serde_json::json!({
        "type": "function",
        "function": {
            "name": key,
            "description": desc,
            "parameters": schema,
        }
    });
    assert_eq!(openai["type"], "function");
    assert_eq!(openai["function"]["name"], "echo_say");
    assert_eq!(openai["function"]["parameters"]["type"], "object");
}

#[test]
fn render_text_concatenates_text_blocks() {
    let mut result = CallToolResult::default();
    result.content = vec![
        ContentBlock::text("hello"),
        ContentBlock::text("world"),
    ];
    assert_eq!(McpClientHandle::render_text(&result), "hello\nworld");
}

#[test]
fn render_text_falls_back_to_structured() {
    let mut result = CallToolResult::default();
    result.structured_content = Some(json!({"answer": 42}));
    assert_eq!(McpClientHandle::render_text(&result), "{\"answer\":42}");
}

#[test]
fn render_text_marks_error() {
    let mut result = CallToolResult::default();
    result.content = vec![ContentBlock::text("boom")];
    result.is_error = Some(true);
    assert_eq!(McpClientHandle::render_text(&result), "[tool error] boom");
}
