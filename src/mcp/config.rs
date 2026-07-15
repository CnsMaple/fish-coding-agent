//! Configuration schema for MCP (Model Context Protocol) servers.
//!
//! Mirrors opencode's `packages/core/src/v1/config/mcp.ts`:
//! - `Local`  — spawns a child process; speaks MCP over stdio.
//! - `Remote` — speaks MCP over streamable-HTTP, with optional
//!   per-server OAuth config.
//!
//! The top-level `mcp` key on the user config is a
//! `HashMap<String, McpEntry>`. Each entry is either a full server
//! config or a `{ "enabled": false }` toggle used to disable a
//! remote default the user does not want.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;

pub(crate) fn default_enabled() -> bool {
    true
}

pub(crate) fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// OAuth sub-config for remote servers. Mirrors opencode's
/// `McpOAuthConfig`. All fields are optional; when omitted the
/// client uses dynamic client registration if the server supports it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteOAuth {
    /// Pre-registered OAuth client id. Skips dynamic client
    /// registration when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Pre-registered OAuth client secret. Skipped when the
    /// authorization server supports public clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// OAuth scopes to request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Local callback port. Default: 19876.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
    /// Full redirect URI override. Default:
    /// `http://127.0.0.1:{callback_port}/mcp/oauth/callback`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
}

/// Discriminated server config (mirrors opencode `McpLocalConfig |
/// McpRemoteConfig`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerConfig {
    /// Spawn a local process and speak MCP over its stdio.
    Local {
        /// Command + args, e.g. `["npx", "-y", "@modelcontextprotocol/server-filesystem"]`.
        command: Vec<String>,
        /// Optional working directory for the child, relative to the
        /// workspace root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// Extra environment variables passed to the child.
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        environment: HashMap<String, String>,
        #[serde(default = "default_enabled")]
        enabled: bool,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    /// Speak MCP over streamable-HTTP (with SSE fallback).
    Remote {
        url: String,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        headers: HashMap<String, String>,
        /// `Some(false)` disables OAuth auto-detection. `None` enables
        /// it (default). `Some(config)` provides pre-registered
        /// client credentials.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth: Option<RemoteOAuth>,
        #[serde(default = "default_enabled")]
        enabled: bool,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
}

impl McpServerConfig {
    pub fn enabled(&self) -> bool {
        match self {
            McpServerConfig::Local { enabled, .. } => *enabled,
            McpServerConfig::Remote { enabled, .. } => *enabled,
        }
    }

    pub fn timeout_ms(&self) -> u64 {
        match self {
            McpServerConfig::Local { timeout_ms, .. } => *timeout_ms,
            McpServerConfig::Remote { timeout_ms, .. } => *timeout_ms,
        }
    }

    /// Set the enabled flag in place.
    pub fn set_enabled(&mut self, value: bool) {
        match self {
            McpServerConfig::Local { enabled, .. } => *enabled = value,
            McpServerConfig::Remote { enabled, .. } => *enabled = value,
        }
    }
}

/// A single entry under the top-level `mcp` config key.
///
/// The `{ "enabled": false }` shorthand lets users turn off a
/// remote default without redefining the whole server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum McpEntry {
    /// Full server configuration.
    Config(McpServerConfig),
    /// Toggle-only entry. `enabled = false` is the only meaningful
    /// value (a `true` toggle is just normalised to a default
    /// [`McpServerConfig::Remote`] in [`McpEntry::normalize`]).
    Toggle { enabled: bool },
}

impl McpEntry {
    /// Collapse toggle entries into a concrete server config. A
    /// `Toggle { enabled: true }` is treated as a remote server
    /// with no URL — which is invalid; we keep the original and
    /// surface a validation error in the service layer.
    pub fn normalize(self, name: &str) -> McpEntry {
        match self {
            McpEntry::Config(cfg) => McpEntry::Config(cfg),
            McpEntry::Toggle { enabled: false } => McpEntry::Config(McpServerConfig::Remote {
                url: String::new(),
                headers: HashMap::new(),
                oauth: None,
                enabled: false,
                timeout_ms: DEFAULT_TIMEOUT_MS,
            }),
            McpEntry::Toggle { enabled: true } => {
                tracing::warn!(
                    "mcp entry `{name}` is `{{\"enabled\": true}}` without a server config; \
                     ignoring"
                );
                McpEntry::Toggle { enabled: true }
            }
        }
    }

    pub fn is_enabled(&self) -> bool {
        match self {
            McpEntry::Config(cfg) => cfg.enabled(),
            McpEntry::Toggle { enabled } => *enabled,
        }
    }
}

impl McpEntry {
    /// Borrow the inner config if this is a `Config` variant.
    pub fn as_config(&self) -> Option<&McpServerConfig> {
        match self {
            McpEntry::Config(cfg) => Some(cfg),
            McpEntry::Toggle { .. } => None,
        }
    }

    /// Owned inner config if this is a `Config` variant.
    pub fn into_config(self) -> Option<McpServerConfig> {
        match self {
            McpEntry::Config(cfg) => Some(cfg),
            McpEntry::Toggle { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_roundtrip() {
        let raw = r#"{
            "type": "local",
            "command": ["npx", "-y", "mcp-fs"],
            "environment": {"FOO": "bar"},
            "enabled": true,
            "timeout_ms": 15000
        }"#;
        let cfg: McpServerConfig = serde_json::from_str(raw).unwrap();
        match cfg {
            McpServerConfig::Local {
                command,
                environment,
                enabled,
                timeout_ms,
                ..
            } => {
                assert_eq!(command, vec!["npx", "-y", "mcp-fs"]);
                assert_eq!(environment.get("FOO").map(String::as_str), Some("bar"));
                assert!(enabled);
                assert_eq!(timeout_ms, 15_000);
            }
            other => panic!("expected local, got {other:?}"),
        }
    }

    #[test]
    fn remote_with_oauth() {
        let raw = r#"{
            "type": "remote",
            "url": "https://example.com/mcp",
            "headers": {"X-Token": "abc"},
            "oauth": {"client_id": "cid", "scope": "read write"},
            "timeout_ms": 30000
        }"#;
        let cfg: McpServerConfig = serde_json::from_str(raw).unwrap();
        match cfg {
            McpServerConfig::Remote {
                url,
                headers,
                oauth,
                enabled,
                timeout_ms,
            } => {
                assert_eq!(url, "https://example.com/mcp");
                assert_eq!(headers.get("X-Token").map(String::as_str), Some("abc"));
                let oauth = oauth.unwrap();
                assert_eq!(oauth.client_id.as_deref(), Some("cid"));
                assert_eq!(oauth.scope.as_deref(), Some("read write"));
                assert!(enabled, "enabled should default to true");
                assert_eq!(timeout_ms, 30_000);
            }
            other => panic!("expected remote, got {other:?}"),
        }
    }

    #[test]
    fn toggle_entry_disables_remote() {
        let raw = r#"{"enabled": false}"#;
        let entry: McpEntry = serde_json::from_str(raw).unwrap();
        let normalized = entry.normalize("gh").into_config().unwrap();
        assert!(!normalized.enabled());
    }

    #[test]
    fn toggle_entry_invalid_keeps_shape() {
        let raw = r#"{"enabled": true}"#;
        let entry: McpEntry = serde_json::from_str(raw).unwrap();
        assert!(matches!(entry, McpEntry::Toggle { enabled: true }));
    }
}
