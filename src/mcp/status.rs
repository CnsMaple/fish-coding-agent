//! Status of a configured MCP server. Mirrors the discriminated
//! `Status` union in opencode's `packages/opencode/src/mcp/index.ts`:
//! connected / disabled / failed / needs_auth /
//! needs_client_registration.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum McpStatus {
    /// Transport is up; the cached tool list is in use.
    Connected,
    /// Server is configured but `enabled: false` — never connect.
    Disabled,
    /// Transport failed to start or crashed. Includes the last error.
    Failed { error: String },
    /// Server returned 401 and we have not yet completed OAuth.
    NeedsAuth,
    /// Server requires pre-registered client credentials we don't have.
    NeedsClientRegistration { error: String },
}

impl McpStatus {
    pub fn icon(&self) -> &'static str {
        match self {
            McpStatus::Connected => "✓",
            McpStatus::Disabled => "○",
            McpStatus::NeedsAuth => "⚠",
            McpStatus::NeedsClientRegistration { .. } => "⚠",
            McpStatus::Failed { .. } => "✗",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            McpStatus::Connected => "connected",
            McpStatus::Disabled => "disabled",
            McpStatus::NeedsAuth => "needs auth",
            McpStatus::NeedsClientRegistration { .. } => "needs client registration",
            McpStatus::Failed { .. } => "failed",
        }
    }
}

/// Stored OAuth credential state for a single MCP server. Mirrors
/// `AuthStatus` in opencode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStatus {
    /// Tokens are present and unexpired.
    Authenticated,
    /// Tokens are present but expired; a refresh should be tried.
    Expired,
    /// No tokens are stored for this server.
    NotAuthenticated,
}

/// Error type returned by [`crate::mcp::service::McpService`].
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("mcp server `{0}` is not configured")]
    NotFound(String),
    #[error("mcp server `{0}` is currently `{1:?}`; cannot perform this action")]
    BadState(String, McpStatus),
    #[error("mcp server `{0}` is not a remote server")]
    NotRemote(String),
    #[error("mcp transport error: {0}")]
    Transport(String),
    #[error("mcp protocol error: {0}")]
    Protocol(String),
    #[error("mcp auth error: {0}")]
    Auth(String),
    #[error("mcp config error: {0}")]
    Config(String),
}
