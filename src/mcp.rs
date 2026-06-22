//! MCP (Model Context Protocol) server registry: named tool providers
//! the user can enable via `/mcp <name>`. Listing follows the same
//! shape as [`crate::skill`] so the completion UI treats them
//! uniformly.
//!
//! Picking an entry from the completion list fills the input with
//! `/mcp <name>` directly (the standard `complete_focused_candidate`
//! contract). Pressing Enter then activates the chosen server, which
//! adds its tool specs to the agent's tool surface for the rest of
//! the session.

#[derive(Debug, Clone)]
pub struct McpServer {
    /// Slash-friendly id, e.g. `"filesystem"`. Lowercased.
    pub name: &'static str,
    /// One-line description shown in the picker.
    pub description: &'static str,
    /// Whether the server is enabled in this session. Disabled servers
    /// still appear in the picker so the user can opt in.
    pub enabled_by_default: bool,
}

/// Built-in MCPs shipped with the agent. A real deployment would
/// load these from the user's config; for now we hardcode a small
/// illustrative set so the picker has something to show.
pub const BUILTIN_MCPS: &[McpServer] = &[
    McpServer {
        name: "filesystem",
        description: "Sandboxed file read/write/list/grep over the workspace.",
        enabled_by_default: true,
    },
    McpServer {
        name: "github",
        description: "Read repos, issues, and PRs from GitHub.",
        enabled_by_default: false,
    },
    McpServer {
        name: "shell",
        description: "Run shell commands in the workspace with a timeout.",
        enabled_by_default: true,
    },
    McpServer {
        name: "python",
        description: "Execute Python snippets for quick local analysis.",
        enabled_by_default: true,
    },
    McpServer {
        name: "web",
        description: "Fetch and summarize public web pages.",
        enabled_by_default: false,
    },
];

/// Returns the names of all built-in MCPs, in declaration order.
pub fn builtin_names() -> Vec<&'static str> {
    BUILTIN_MCPS.iter().map(|m| m.name).collect()
}

/// Look up an MCP by name (case-insensitive).
pub fn find(name: &str) -> Option<&'static McpServer> {
    let needle = name.trim().to_ascii_lowercase();
    BUILTIN_MCPS.iter().find(|m| m.name == needle.as_str())
}

/// Completion candidates for the `/mcp:<name>` form. Performs
/// fuzzy subsequence matching (see [`crate::fuzzy`]) so partial
/// queries like `gh` still surface `github`. Empty query returns
/// every server, alphabetically sorted.
///
/// Returned strings are the full slash form (`/mcp:<name>`) so they
/// can be inserted verbatim by `complete_focused_candidate`.
pub fn completion_candidates(query: &str) -> Vec<String> {
    let q = query.trim();
    let mut scored: Vec<(u32, String)> = BUILTIN_MCPS
        .iter()
        .filter_map(|m| {
            crate::fuzzy::score(q, m.name).map(|sc| (sc, format!("/mcp:{}", m.name)))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, s)| s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_names_are_unique() {
        let names = builtin_names();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate mcp names");
    }

    #[test]
    fn find_is_case_insensitive() {
        assert!(find("Filesystem").is_some());
        assert!(find("FILESYSTEM").is_some());
        assert!(find("nope").is_none());
    }

    #[test]
    fn completion_filters_by_prefix() {
        let all = completion_candidates("");
        assert!(all.len() >= 5);
        assert!(all.iter().all(|s| s.starts_with("/mcp:")));

        let fi = completion_candidates("fi");
        assert!(fi.contains(&"/mcp:filesystem".to_string()));
        assert!(!fi.contains(&"/mcp:github".to_string()));
    }
}
