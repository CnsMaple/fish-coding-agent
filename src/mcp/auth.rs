//! Persistent OAuth credential store for remote MCP servers.
//!
//! Mirrors opencode's `packages/opencode/src/mcp/auth.ts`. The
//! actual OAuth flow is implemented in
//! [`crate::mcp::oauth_callback`]; this module just persists the
//! resulting tokens at
//! `~/.config/fish-coding-agent/mcp-auth.json` (mode 0o600).
//!
//! Stays a small module in this PR — only the read/write API
//! needed by the rest of the service is exposed. The full OAuth
//! flow is wired up in PR4.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "mcp-auth.json";
#[cfg(unix)]
const FILE_MODE: u32 = 0o600;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tokens {
    #[serde(default)]
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientInfo {
    #[serde(default)]
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id_issued_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ClientInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct File {
    #[serde(default)]
    pub(crate) entries: HashMap<String, Entry>,
}

#[derive(Debug, Clone)]
pub struct McpAuthStore {
    path: PathBuf,
}

// `Clone` is derived above; nothing else needed.

impl McpAuthStore {
    pub fn load_or_default() -> Self {
        let path = match crate::mcp::auth::default_path() {
            Some(p) => p,
            None => return Self::disabled(),
        };
        Self { path }
    }

    /// Construct a no-op store (no on-disk persistence). Used
    /// when the config dir can't be resolved.
    pub fn disabled() -> Self {
        Self {
            path: PathBuf::from(""),
        }
    }

    pub fn is_enabled(&self) -> bool {
        !self.path.as_os_str().is_empty()
    }

    pub(crate) fn load(&self) -> File {
        if !self.is_enabled() {
            return File::default();
        }
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => File::default(),
        }
    }

    pub(crate) fn save(&self, file: &File) {
        if !self.is_enabled() {
            return;
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(raw) = serde_json::to_string_pretty(file) {
            if std::fs::write(&self.path, raw).is_ok() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &self.path,
                        std::fs::Permissions::from_mode(FILE_MODE),
                    );
                }
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<Entry> {
        self.load().entries.get(name).cloned()
    }

    pub fn set(&self, name: &str, entry: Entry) {
        let mut file = self.load();
        file.entries.insert(name.to_string(), entry);
        self.save(&file);
    }

    pub fn remove(&self, name: &str) {
        let mut file = self.load();
        file.entries.remove(name);
        self.save(&file);
    }

    /// List all stored entries by name.
    pub fn list(&self) -> Vec<String> {
        self.load().entries.into_keys().collect()
    }
}

fn default_path() -> Option<PathBuf> {
    crate::config::paths::config_dir()
        .ok()
        .map(|p| p.join(FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_entry() {
        let entry = Entry {
            tokens: Some(Tokens {
                access_token: "abc".into(),
                refresh_token: Some("xyz".into()),
                expires_at: Some(1_700_000_000),
                scope: Some("read".into()),
            }),
            client_info: None,
            server_url: Some("https://example.com/mcp".into()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: Entry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }
}
