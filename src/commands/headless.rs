use crate::config::Config;
use crate::providers::{ChatMessage, ChatRequest};
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use tokio::sync::mpsc;

use super::system_prompt;

/// Result of a completed headless run.
pub struct HeadlessResult {
    /// Final assistant text reply (accumulated deltas).
    pub text: String,
    /// Total input tokens (0 if the provider did not report usage).
    pub input_tokens: u64,
    /// Total output tokens (0 if the provider did not report usage).
    pub output_tokens: u64,
    /// Number of tool-call round-trips executed before finishing.
    pub tool_rounds: usize,
}

/// Options for a single headless task invocation.
pub struct HeadlessOptions {
    /// Directory tools operate in (the agent's workspace).
    pub cwd: PathBuf,
    /// Maximum LLM tool-call round-trips before forcing a stop.
    /// Guards against runaway loops. `0` means unlimited.
    pub max_rounds: usize,
}

impl Default for HeadlessOptions {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            max_rounds: 0,
        }
    }
}

/// Run a single autonomous task against the active provider's harness,
/// without any TUI. Mirrors the interactive `send_message` request
/// construction but drives the loop directly until the model stops
/// calling tools (or emits `plan`/`ask`, which is excluded below).
///
/// `plan`/`ask` are interaction tools that would pause the loop to
/// hand control back to the user, so they are filtered out of the
/// advertised tool set. The built-in Doom-loop guard and the provider
/// retry logic inside `run_chat_stream` still apply.
pub async fn run_headless_task(prompt: String, opts: HeadlessOptions) -> Result<HeadlessResult> {
    let config_path = crate::config::paths::config_file_path()?;
    let cfg = Config::load_or_init(&config_path)
        .with_context(|| "could not load config for headless run")?;

    let active_id = cfg
        .active
        .clone()
        .ok_or_else(|| anyhow!("no active provider configured"))?;
    cfg.validate_provider(&active_id)
        .map_err(|e| anyhow!("active provider invalid: {e}"))?;
    let entry = cfg
        .entry(&active_id)
        .ok_or_else(|| anyhow!("active entry not found"))?;
    let provider = entry.kind;
    let base = entry.base_url.clone();
    let key = cfg
        .effective_api_key(&active_id)
        .ok_or_else(|| anyhow!("no api key for {active_id}"))?;
    let model = cfg.active_model().to_string();
    let thinking = cfg.thinking;

    // Build the system prompt from the same static core + enabled
    // agents.md files the interactive TUI uses.
    let agents_content = build_agents_content(&cfg);
    let core_sp = system_prompt(crate::permission::Agent::Build, &agents_content);

    // Tool specs for the active provider, minus the interaction tools
    // that would pause the loop for user input.
    let tools = Some(crate::tools::loop_tool_specs(provider));

    // Dynamic prompt (date/CWD/shell) goes as the first working
    // message when prefix caching is on; otherwise appended to system.
    let (system, messages) = if cfg.prefix_cache {
        let mut msgs = vec![ChatMessage {
            role: "user".to_string(),
            content: super::system_prompt_dynamic(crate::function::AppMode::Loop),
            content_parts: Vec::new(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }];
        msgs.push(ChatMessage {
            role: "user".to_string(),
            content: prompt,
            content_parts: Vec::new(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        });
        (core_sp, msgs)
    } else {
        let dynamic = super::system_prompt_dynamic(crate::function::AppMode::Loop);
        let msgs = vec![ChatMessage {
            role: "user".to_string(),
            content: prompt,
            content_parts: Vec::new(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }];
        (format!("{core_sp}\n\n{dynamic}"), msgs)
    };

    let req = ChatRequest {
        model,
        messages,
        thinking,
        system: Some(system),
        tools,
        prefix_messages: Vec::new(),
        cache_retention: cfg.cache_retention,
    };

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("build reqwest client")?;

    let (tx, mut rx) = mpsc::unbounded_channel::<crate::event::AppMsg>();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let _ = cancel_tx;

    let seq = 1u64;
    let mut text = String::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;

    // Drive the loop ourselves: spawn the chat stream, then consume
    // AppMsg events until a terminating ChatDone/ChatError arrives.
    let task = tokio::spawn(super::run_chat_stream(
        client,
        base,
        key,
        req,
        provider,
        crate::permission::Agent::Build,
        opts.cwd,
        cancel_rx,
        tx,
        seq,
    ));

    let mut tool_rounds = 0usize;
    let result = loop {
        let msg = rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("event channel closed before task finished"))?;
        match msg {
            crate::event::AppMsg::ChatDelta(s) => text.push_str(&s),
            crate::event::AppMsg::ChatUsage { usage, .. } => {
                input_tokens = usage.input_tokens;
                output_tokens = usage.output_tokens;
            }
            crate::event::AppMsg::ChatDone { .. } => {
                break Ok(HeadlessResult {
                    text,
                    input_tokens,
                    output_tokens,
                    tool_rounds,
                })
            }
            crate::event::AppMsg::ChatError { error, .. } => {
                break Err(anyhow!("chat stream error: {error}"));
            }
            // Count tool-call batches to enforce max_rounds.
            crate::event::AppMsg::AssistantToolCalls(_) => {
                tool_rounds += 1;
                if opts.max_rounds > 0 && tool_rounds >= opts.max_rounds {
                    break Err(anyhow!("max tool rounds ({}) reached", opts.max_rounds));
                }
            }
            _ => {}
        }
    };

    // Wait for the spawned stream to finish so it doesn't leak.
    let _ = task.await;
    result
}

/// Replicate `commands::build_agents_content` without an `App` handle:
/// read the enabled agents.md files from config.
fn build_agents_content(cfg: &Config) -> String {
    let mut out = String::new();
    for (path, &enabled) in &cfg.agents.entries {
        if !enabled {
            continue;
        }
        if let Ok(body) = std::fs::read_to_string(path) {
            if !body.trim().is_empty() {
                out.push_str(&format!(
                    "\n\n## User instructions from {}\n\n{}\n",
                    path, body
                ));
            }
        }
    }
    out
}
