use super::*;

/// Maximum number of MCP tools that may be advertised to the LLM.
/// Protects against a misconfigured server that exports tens of
/// thousands of tools from blowing the prompt budget.
const MCP_TOOL_LIMIT: usize = 256;

/// Maximum length of a tool description we'll send to the LLM.
/// Truncates with an ellipsis when longer; matches the opencode
/// behaviour in `McpCatalog.convertTool`.
const MCP_DESC_LIMIT: usize = 200;

/// Provider-specific tool-spec JSON shape.
#[derive(Clone, Copy)]
enum ToolFormat {
    OpenAi,
    Anthropic,
}

impl ToolFormat {
    /// Wrap a `(name, description, schema)` triple into the
    /// provider-specific tool-spec JSON object.
    fn wrap(&self, name: &str, description: &str, schema: &serde_json::Value) -> serde_json::Value {
        match self {
            ToolFormat::OpenAi => json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": schema,
                }
            }),
            ToolFormat::Anthropic => json!({
                "name": name,
                "description": description,
                "input_schema": schema,
            }),
        }
    }

    /// Extract the tool name from a provider-specific spec.
    fn name_of<'a>(&self, spec: &'a serde_json::Value) -> &'a str {
        match self {
            ToolFormat::OpenAi => spec["function"]["name"].as_str().unwrap_or(""),
            ToolFormat::Anthropic => spec["name"].as_str().unwrap_or(""),
        }
    }
}

pub fn openai_tool_specs() -> Vec<serde_json::Value> {
    tool_specs(ToolFormat::OpenAi)
}

pub fn anthropic_tool_specs() -> Vec<serde_json::Value> {
    tool_specs(ToolFormat::Anthropic)
}

fn tool_specs(fmt: ToolFormat) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = tool_defs()
        .into_iter()
        .map(|tool| fmt.wrap(tool.name, &tool.description, &tool.schema))
        .collect();
    out.extend(mcp_specs(&fmt));
    out
}

/// Return tool specs filtered for a sub-agent type. Sub-agents may
/// not have access to all tools (e.g. `explore` is read-only).
pub fn openai_tool_specs_for_sub_agent(
    sub_agent: crate::permission::SubAgent,
) -> Vec<serde_json::Value> {
    tool_specs_for_sub_agent(ToolFormat::OpenAi, sub_agent)
}

pub fn anthropic_tool_specs_for_sub_agent(
    sub_agent: crate::permission::SubAgent,
) -> Vec<serde_json::Value> {
    tool_specs_for_sub_agent(ToolFormat::Anthropic, sub_agent)
}

fn tool_specs_for_sub_agent(
    fmt: ToolFormat,
    sub_agent: crate::permission::SubAgent,
) -> Vec<serde_json::Value> {
    tool_specs(fmt)
        .into_iter()
        .filter(|spec| {
            let name = fmt.name_of(spec);
            matches!(
                crate::permission::check_sub_agent(sub_agent, name),
                crate::permission::Action::Allow
            )
        })
        .collect()
}

/// Read the current MCP tool list and convert it to the provider-specific
/// tool-spec shape. Returns an empty Vec when the service is not
/// installed or has no connected tools.
fn mcp_specs(fmt: &ToolFormat) -> Vec<serde_json::Value> {
    mcp_tool_iter()
        .into_iter()
        .map(|(key, description, schema)| fmt.wrap(&key, &description, &schema))
        .collect()
}

/// Collect `(key, description, schema)` triples from the live MCP
/// service. Strips to the first `MCP_TOOL_LIMIT` entries; bounds
/// the description length.
pub(super) fn mcp_tool_iter() -> Vec<(String, String, serde_json::Value)> {
    let Some(svc) = McpRegistry::current() else {
        return Vec::new();
    };
    let snap = match svc.try_snapshot() {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<(String, String, serde_json::Value)> = snap
        .tools
        .values()
        .map(|t| {
            let mut desc = t.description.clone();
            if desc.chars().count() > MCP_DESC_LIMIT {
                desc = desc.chars().take(MCP_DESC_LIMIT).collect::<String>() + "…";
            }
            if desc.is_empty() {
                desc = format!(
                    "MCP tool `{name}` (server: {server})",
                    name = t.name,
                    server = t.server
                );
            } else {
                desc = format!("[mcp:{server}] {desc}", server = t.server);
            }
            (t.key.clone(), desc, t.input_schema.clone())
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.truncate(MCP_TOOL_LIMIT);
    out
}

pub(super) struct ToolDef {
    name: &'static str,
    description: String,
    schema: serde_json::Value,
}

pub(super) fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read",
            description: "Read a UTF-8 text file within the current workspace. Supports optional 1-based inclusive line ranges. Call this tool in parallel (multiple calls in one turn) when you know there are several files to read. Avoid tiny repeated slices — read a larger window once instead of re-reading several times.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workspace-relative path to read." },
                    "start_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based line to start reading." },
                    "end_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based line to stop reading, inclusive." }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "edit",
            description: "Write or edit a UTF-8 text file within the current workspace. Use this tool for all file modifications including creating new files and editing existing ones. To edit, provide oldString (the exact text to find and replace) with the replacement content (use `content` or `newString` — both accepted). When oldString is provided but content is null/omitted, the matched text is deleted. CRLF line endings are automatically normalized so you can use plain \n for oldString even on Windows files. When oldString matches multiple locations, use start_line/end_line to narrow the search scope, or use replaceAll: true. If exact match fails, a fuzzy fallback strips trailing whitespace from each line and retries — useful when editors added/removed trailing spaces. To create or overwrite a file, omit oldString. The edit FAILS if oldString is not found or matches more than one location — provide more surrounding context to make the match unique. You MUST read the file first before editing it.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workspace-relative path to write." },
                    "content": { "type": "string", "description": "Content to write, or replacement text when oldString is provided. Alias: newString." },
                    "oldString": { "type": "string", "description": "Exact text to find and replace in the file. Must be unique within the search scope (whole file or specified line range). Omit to create/overwrite the entire file." },
                    "replaceAll": { "type": "boolean", "description": "Replace all occurrences of oldString. Default false (requires unique match)." },
                    "start_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based line to start searching for oldString. Must be used with end_line." },
                    "end_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based line to stop searching for oldString, inclusive. Must be used with start_line." }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "shell_command",
            description: {
                let shell = super::web::shell_description();
                let guidance = super::web::shell_guidance();
                format!(
                    "Execute a shell command in the current workspace using {shell} and return stdout/stderr. Default timeout is 300 seconds; pass `timeout_secs` to override.\n\n{guidance}",
                    shell = shell,
                    guidance = guidance,
                )
            },
            schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Command line to execute. Use && to chain commands that must all succeed, ; to chain commands where failures are acceptable. Quote paths with spaces." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "default": 300, "description": "Timeout in seconds. Default 300." }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "python_command",
            description: "Run Python code in the current workspace and return stdout/stderr. Use this for exact file inspection, small scripts, and deterministic local analysis. Default timeout is 300 seconds; pass `timeout_secs` to override.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Python source code to execute." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "default": 300, "description": "Timeout in seconds. Default 300." }
                },
                "required": ["code"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "grep",
            description: "Search for a regex pattern in UTF-8 files under a workspace path and return matching file/line snippets.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern to search for." },
                    "path": { "type": "string", "description": "Optional workspace-relative file or directory. Defaults to current workspace." }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "list",
            description: "List files and directories directly under a workspace-relative directory.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Optional workspace-relative directory. Defaults to current workspace." }
                },
                "required": [],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "plan",
            description: "Present a plan for user confirmation in the function panel before executing it.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short plan title. Defaults to 'Plan'." },
                    "content": { "type": "string", "description": "Full plan text. Provide this or steps." },
                    "steps": { "type": "array", "items": { "type": "string" }, "description": "Optional list of step strings, rendered as a numbered list. Used when content is not provided." }
                },
                "required": [],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "ask",
            description: "Ask the user a clarifying question. The question is shown in the session and as a toast. The user types their answer into the main input; the conversation resumes when they submit. Use this in plan mode to confirm tradeoffs before drafting a plan, and in build mode when a single decision blocks the next step.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question to present to the user." },
                    "options": { "type": "array", "items": { "type": "string" }, "description": "Optional list of suggested answers; rendered as bullets under the question." }
                },
                "required": ["question"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "todowrite",
            description: "Create and maintain a structured task list for the current coding session. Tracks progress, organizes multi-step work, and surfaces status to the user.\n\nMandatory usage rules:\n1. Every turn: before finishing your response, call `todowrite` once with the full current list so the user sees up-to-date status. Do not skip a turn.\n2. Update on completion: the moment a single todo item is done (or its status changes), immediately call `todowrite` with the updated full list.\n3. Clear when all done: when every item is `completed`, call `todowrite` with an empty `todos` array `[]` to clear the list; the todo tab closes automatically.\n4. Always send ALL items (existing + new/changed) in each call — never send a diff.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "Description of the task." },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"], "description": "Task status." }
                            },
                            "required": ["content", "status"]
                        },
                        "description": "Full list of todo items to replace the current task list. Each call must send ALL items (existing + new/changed), not just the diff. Pass an empty array to clear the list when all tasks are completed."
                    }
                },
                "required": ["todos"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "glob",
            description: "Fast file pattern matching tool. Supports glob patterns like \"**/*.rs\" or \"src/**/*.ts\". Returns matching file paths sorted by modification time. Use this tool when you need to find files by name patterns. It is always better to speculatively perform multiple searches as a batch that are potentially useful.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "The glob pattern to match files against." },
                    "path": { "type": "string", "description": "Optional workspace-relative directory to search in. Defaults to current workspace." }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },

        ToolDef {
            name: "skill",
            description: "Load a specialized skill when the task at hand matches one of the skills listed in the system prompt.\n\nUse this tool to inject the skill's instructions and resources into current conversation. The output may contain detailed workflow guidance as well as references to scripts, files, etc in the same directory as the skill.\n\nThe skill name must match one of the skills listed in your system prompt.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The name of the skill from available_skills" }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "webfetch",
            description: "Fetch content from an HTTP or HTTPS URL and return it as text, markdown, or HTML. Markdown is the default.\n\nUse a more targeted tool when one is available. This tool is read-only. Large text results may be replaced with a preview while the complete output is retained in managed storage.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The HTTP or HTTPS URL to fetch content from" },
                    "format": { "type": "string", "enum": ["text", "markdown", "html"], "description": "The format to return the content in. Defaults to markdown." },
                    "timeout": { "type": "integer", "minimum": 1, "maximum": 120, "description": "Optional timeout in seconds (maximum: 120)" }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "websearch",
            description: format!("Search the web using the session's web search provider - performs real-time web searches and can scrape content from specific URLs\n\nProvides up-to-date information for current events and recent data\nSupports configurable result counts and returns the content from the most relevant websites\nUse this tool for accessing information beyond knowledge cutoff\nSearches are performed automatically within a single API call\n\nUsage notes:\n  - Supports live crawling modes when available: 'fallback' (backup if cached unavailable) or 'preferred' (prioritize live crawling)\n  - Search types when available: 'auto' (balanced), 'fast' (quick results), 'deep' (comprehensive search)\n  - Configurable context length for optimal LLM integration\n  - Domain filtering and advanced search options available\n\nThe current year is {year}. You MUST use this year when searching for recent information or current events\n- Example: If the current year is {year} and the user asks for \"latest AI news\", search for \"AI news {year}\", NOT \"AI news {prev}\"", year = chrono::Utc::now().year(), prev = chrono::Utc::now().year() - 1),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Websearch query" },
                    "numResults": { "type": "integer", "minimum": 1, "maximum": 20, "description": "Number of search results to return (default: 8, maximum: 20)" },
                    "livecrawl": { "type": "string", "enum": ["fallback", "preferred"], "description": "Live crawl mode - 'fallback': use live crawling as backup if cached content unavailable, 'preferred': prioritize live crawling (default: 'fallback')" },
                    "type": { "type": "string", "enum": ["auto", "fast", "deep"], "description": "Search type - 'auto': balanced search (default), 'fast': quick results, 'deep': comprehensive search" },
                    "contextMaxCharacters": { "type": "integer", "minimum": 1, "maximum": 50000, "description": "Maximum characters for context string optimized for models (default: 10000, maximum: 50000)" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "sub_agent",
            description: "Launch a new agent to handle complex, multistep tasks autonomously.\n\nWhen using the sub_agent tool, you must specify a subagent_type parameter to select which agent type to use.\n\nWhen NOT to use the sub_agent tool:\n- If you want to read a specific file path, use the Read or Glob tool instead\n- If you are searching for a specific class definition, use the Grep tool instead\n- If you are searching for code within a specific file or set of 2-3 files, use the Read tool instead\n- If no available agent is a good fit for the task, use other tools directly\n\nUsage notes:\n1. Launch multiple agents concurrently whenever possible\n2. Once you have delegated work to an agent, do not duplicate that work yourself\n3. When the agent is done, it will return a single message back to you\n4. Each agent invocation starts with a fresh context\n5. The agent's outputs should generally be trusted\n6. Clearly tell the agent whether you expect it to write code or just to do research\n7. If the agent description mentions it should be used proactively, use your best judgement\n\nAvailable agent types:\n- general: General-purpose agent for complex questions and multi-step tasks. Has full tool access.\n- explore: Fast agent specialized for exploring codebases. Use this when you need to quickly find files by patterns, search code for keywords, or answer questions about the codebase. When calling this agent, specify the desired thoroughness level: \"quick\" for basic searches, \"medium\" for moderate exploration, or \"very thorough\" for comprehensive analysis.".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string", "description": "A short (3-5 words) description of the task" },
                    "prompt": { "type": "string", "description": "The task for the agent to perform" },
                    "subagent_type": { "type": "string", "enum": ["general", "explore"], "description": "The type of specialized agent to use for this task" },
                    "max_steps": { "type": "integer", "minimum": 1, "maximum": 100, "default": 15, "description": "Maximum number of steps the sub-agent may take." },
                    "task_id": { "type": "string", "description": "Optional: resume a previous sub-agent session" }
                },
                "required": ["description", "prompt", "subagent_type"],
                "additionalProperties": false
            }),
        },
    ]
}
