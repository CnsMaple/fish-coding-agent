//! Tool name sanitization and the `<server>_<tool>` naming scheme.
//!
//! Mirrors opencode's `McpCatalog.sanitize` and
//! `McpCatalog.toolName`. The combined key is what we expose to the
//! LLM as the tool name (e.g. `github_list_issues`), so it has to
//! match what the permission system and the picker key off of.

/// Replace every character outside `[A-Za-z0-9_-]` with `_`.
///
/// `[^a-zA-Z0-9_-]` matches the opencode `sanitize` regex verbatim.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Build the LLM-facing tool name as `<sanitized_server>_<sanitized_tool>`.
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("{}_{}", sanitize(server), sanitize(tool))
}

/// One tool spec as returned by the upstream server. We keep this as
/// a thin `serde_json::Value` envelope — at the catalog boundary we
/// convert it to the existing `serde_json::Value` tool-spec shape
/// used by `tools::openai_tool_specs` / `anthropic_tool_specs`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpToolSpec {
    /// Combined `<server>_<tool>` key.
    pub key: String,
    /// Original server name.
    pub server: String,
    /// Original tool name.
    pub name: String,
    /// Human-readable description for the LLM.
    pub description: String,
    /// JSON Schema for the tool's input.
    pub input_schema: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_special_chars() {
        assert_eq!(sanitize("github.com"), "github_com");
        assert_eq!(sanitize("my server"), "my_server");
        assert_eq!(sanitize("a-b_c"), "a-b_c");
    }

    #[test]
    fn tool_name_combines() {
        assert_eq!(tool_name("github", "list_issues"), "github_list_issues");
        assert_eq!(tool_name("a.b", "c d"), "a_b_c_d");
    }
}
