//! MCP (Model Context Protocol) client subsystem.
//!
//! Mirrors the design of the upstream opencode project
//! ([`anomalyco/opencode`](https://github.com/anomalyco/opencode),
//! `packages/opencode/src/mcp`): a single `McpService` owns the
//! lifecycle of every configured server (local stdio or remote
//! streamable-HTTP / SSE), exposes a per-server [`McpStatus`], and
//! aggregates the live tool list into the agent's tool surface via
//! [`catalog`].

pub mod auth;
pub mod catalog;
pub mod client;
pub mod config;
pub mod oauth_callback;
pub mod registry;
pub mod service;
pub mod status;

#[cfg(test)]
mod tests;

pub use catalog::{sanitize, tool_name, McpToolSpec};
pub use config::{McpEntry, McpServerConfig, RemoteOAuth};
pub use registry::McpRegistry;
pub use service::{McpEvent, McpEventSink, McpService, ServiceError, StateSnapshot};
pub use status::{AuthStatus, McpStatus};

/// Adapter that translates `McpEvent`s into `AppMsg`s on the
/// shared event channel. The runtime installs one of these when
/// [`crate::mcp::service::McpService::bind_event_sink`] is called
/// from `event::run`.
pub struct AppMsgEventSink {
    tx: tokio::sync::mpsc::UnboundedSender<crate::event::AppMsg>,
}

impl AppMsgEventSink {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<crate::event::AppMsg>) -> Self {
        Self { tx }
    }
}

impl McpEventSink for AppMsgEventSink {
    fn emit(&self, event: McpEvent) {
        let msg = match event {
            McpEvent::ToolsChanged { server } => crate::event::AppMsg::McpToolsChanged { server },
            McpEvent::StatusChanged { name, status } => {
                crate::event::AppMsg::McpStatusChanged { name, status }
            }
            McpEvent::AuthRequired { server, error } => {
                crate::event::AppMsg::McpAuthRequired {
                    server,
                    url: String::new(),
                    error,
                }
            }
            McpEvent::ClientClosed { server } => {
                crate::event::AppMsg::McpClientClosed { server }
            }
        };
        let _ = self.tx.send(msg);
    }
}

/// Completion candidates for the `/mcp:<name>` slash form. Reads
/// from the live [`McpRegistry`] if installed; falls back to empty
/// when the service has not been initialised yet (e.g. during
/// early-startup picker rendering).
///
/// The returned strings are `/mcp:<name>`, preserving the
/// `complete_focused_candidate` contract that the rest of the
/// input layer relies on.
pub fn completion_candidates(query: &str) -> Vec<String> {
    let q = query.trim();
    let Some(svc) = McpRegistry::current() else {
        return Vec::new();
    };
    // Best-effort: take a snapshot if no one else is holding the
    // write lock; otherwise yield empty so the picker doesn't block.
    let Ok(snapshot) = svc.try_snapshot() else {
        return Vec::new();
    };
    let mut scored: Vec<(u32, String, u8)> = snapshot
        .config
        .iter()
        .filter_map(|(name, _)| {
            let sc = crate::fuzzy::score(q, name)?;
            let rank = snapshot
                .status
                .get(name)
                .map(status_rank)
                .unwrap_or(status_rank(&McpStatus::Disabled));
            Some((sc, format!("/mcp:{name}"), rank))
        })
        .collect();
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.1.cmp(&b.1))
    });
    scored.into_iter().map(|(_, s, _)| s).collect()
}

fn status_rank(s: &McpStatus) -> u8 {
    match s {
        McpStatus::Connected => 0,
        McpStatus::NeedsAuth => 1,
        McpStatus::Disabled => 2,
        McpStatus::NeedsClientRegistration { .. } => 3,
        McpStatus::Failed { .. } => 4,
    }
}

/// Synchronous snapshot — fails if the lock is held by another
/// task. Used by the picker which must not block.
pub fn try_snapshot_or_empty() -> StateSnapshot {
    McpRegistry::current()
        .and_then(|s| s.try_snapshot().ok())
        .unwrap_or_default()
}

/// Back-compat shim: list the names of all configured MCP servers.
/// Replaces the old `BUILTIN_MCPS`-based `builtin_names` from the
/// pre-PR1 stub. Reads the live service; returns an empty Vec if
/// the service has not been initialised yet.
pub fn builtin_names() -> Vec<String> {
    McpRegistry::current()
        .and_then(|svc| svc.configured_names_sync().ok())
        .unwrap_or_default()
}

/// Back-compat shim: look up a configured MCP server by name.
/// Returns `Some(name)` for any configured server (regardless of
/// enabled/disabled), `None` otherwise. The old stub returned a
/// static struct; callers that need richer data should use
/// [`McpRegistry::current`] directly.
pub fn find(name: &str) -> Option<String> {
    let needle = name.trim();
    if needle.is_empty() {
        return None;
    }
    builtin_names().into_iter().find(|n| n == needle)
}
