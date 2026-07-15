//! Auto-compaction.
//!
//! Mirrors opencode's `compaction.ts` + `overflow.ts` flow. The goals are:
//!
//! 1. Decide **when** to compact: `should_auto_compact` (post-response)
//!    and `compact_if_needed` (pre-flight). Both use the same
//!    `isOverflow`-style formula:
//!    ```text
//!    used >= ctx_window - reserved
//!    ```
//!    where `reserved` defaults to `COMPACTION_BUFFER` (20 000), or
//!    `Config::compact_reserved` if the user has overridden it.
//!
//! 2. Decide **what** to compact: `select` uses a token budget
//!    (`DEFAULT_KEEP_TOKENS = 8 000`) to walk backward from the most
//!    recent messages and determine which ones to keep vs. summarise.
//!    `plan_cutoff` is a simpler turn-based fallback.
//!
//! 3. Generate the summary: `build_prompt` constructs a structured
//!    prompt with a `SUMMARY_TEMPLATE` that asks the LLM to produce
//!    a Markdown summary with `## Objective`, `## Important Details`,
//!    `## Work State`, and `## Next Move` sections. Supports
//!    incremental compaction via `previous_summary`.

use crate::session::Message;

/// Buffer reserved for the model's reply. Matches opencode's
/// `COMPACTION_BUFFER` in `overflow.ts`. Used as the default
/// `reserved` value when `Config::compact_reserved` is `None`.
pub const COMPACTION_BUFFER: u64 = 20_000;

/// Maximum characters for the compaction summary prompt. When the
/// prompt exceeds this, the oldest messages are trimmed so the
/// request stays well under the API's input-token limit (typically
/// 1 000 000 tokens). 500k chars ≈ 125k tokens is a conservative
/// upper bound.
pub const MAX_COMPACTION_PROMPT_CHARS: usize = 500_000;

/// Number of trailing user/assistant turns to keep verbatim.
/// Used by `plan_cutoff` as a fallback. Each "turn" is a
/// pair of (user, assistant) messages.
pub const DEFAULT_TAIL_TURNS: usize = 2;

/// Tokens to keep as recent context during compaction. Matches
/// opencode's `DEFAULT_KEEP_TOKENS = 8_000`.
pub const DEFAULT_KEEP_TOKENS: u64 = 8_000;

/// Maximum characters for tool output in the compaction prompt.
/// Tool results are truncated to avoid blowing up the prompt.
pub const TOOL_OUTPUT_MAX_CHARS: usize = 2_000;

/// Output tokens reserved for the summary. Matches opencode's
/// `SUMMARY_OUTPUT_TOKENS = 4_096`.
pub const SUMMARY_OUTPUT_TOKENS: u64 = 4_096;

/// Structured template for the compaction summary. The LLM is asked
/// to produce a Markdown summary with these sections. Matches
/// opencode's `SUMMARY_TEMPLATE`.
pub const SUMMARY_TEMPLATE: &str = "\
Output exactly the Markdown structure shown inside <template> and keep the section order unchanged. Do not include the <template> tags in your response.
<template>
## Objective
- [one or two brief sentences describing what the user is trying to accomplish]

## Important Details
- [constraints/preferences, decisions and why, important facts/assumptions, exact context needed to continue, or \"(none)\"]

## Work State
- Completed: [finished work, verified facts, or changes made; otherwise \"(none)\"]
- Active: [current work, partial changes, or investigation state; otherwise \"(none)\"]
- Blocked: [blockers, failing commands, or unknowns; otherwise \"(none)\"]

## Next Move
1. [immediate concrete action, or \"(none)\"]
2. [next action if known, or \"(none)\"]
</template>

Rules:
- Keep every section, even when empty.
- Use terse bullets, not prose paragraphs.
- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers when known.
- Put relevant files and symbols inside the section where they matter; do not add extra sections.
- Do not mention the summary process or that context was compacted.";

/// Legacy prompt kept for backward compatibility with unit tests.
/// New callers should use `build_prompt` + `SUMMARY_TEMPLATE`.
pub const SUMMARY_PROMPT: &str = "Summarize the following conversation history so it can be \
used as a compact context for the next turn. Preserve all of the \
following:\n\
 - Decisions, plans, and conclusions the user and assistant reached.\n\
 - File paths, function names, and other concrete identifiers touched.\n\
 - Open questions and pending follow-ups.\n\
 - Tool outputs that materially affect later reasoning.\n\
Drop greetings, filler, and anything that is fully captured by the \
preserved tail. Reply with the summary only — no preamble, no \
explanation.";

/// Inputs that drive the `usable` / `should_auto_compact` math.
#[derive(Debug, Clone, Copy)]
pub struct CompactionInputs {
    pub auto_enabled: bool,
    pub ctx_window: u64,
    pub max_output_tokens: u64,
    pub reserved_override: Option<u64>,
}

/// Maximum number of input tokens usable for the conversation body
/// before auto-compaction should kick in. Matches opencode's
/// `usable()`:
///
/// ```text
/// reserved = cfg.compaction?.reserved ?? min(BUFFER, max_output)
/// usable   = ctx_window - reserved
/// ```
///
/// `max_output_tokens == 0` is treated as "unknown" and falls back
/// to `COMPACTION_BUFFER` so the math stays meaningful for models
/// that do not advertise a separate output cap. Returns 0 when the
/// model has no known context window — callers must treat that as
/// "no compaction possible" rather than "always compact".
pub fn usable(inp: CompactionInputs) -> u64 {
    if inp.ctx_window == 0 {
        return 0;
    }
    let reserved = inp.reserved_override.unwrap_or_else(|| {
        if inp.max_output_tokens == 0 {
            COMPACTION_BUFFER
        } else {
            inp.max_output_tokens.min(COMPACTION_BUFFER)
        }
    });
    inp.ctx_window.saturating_sub(reserved)
}

/// Returns true when the auto-compaction trigger should fire for the
/// current usage. Mirrors opencode's `isOverflow`.
pub fn should_auto_compact(used: u64, inp: CompactionInputs) -> bool {
    if !inp.auto_enabled {
        return false;
    }
    let u = usable(inp);
    if u == 0 {
        return false;
    }
    used >= u
}

/// `(usable - used) / usable`, clamped to `[0.0, 1.0]`. The status
/// bar shows this as a "headroom %" so the user can see how much room
/// is left before the next compaction.
/// `None` when the model has no known context window.
pub fn headroom_pct(used: u64, inp: CompactionInputs) -> Option<f64> {
    let u = usable(inp);
    if u == 0 {
        return None;
    }
    let remaining = u.saturating_sub(used) as f64;
    Some((remaining / u as f64).clamp(0.0, 1.0))
}

/// Coarse token estimation. Uses `chars / 4` (≈4 chars per token
/// for English text) as a conservative heuristic. CJK-heavy text
/// is closer to 1 char per token, so this overestimates for those
/// languages — which is safe for compaction purposes.
pub fn estimate_tokens(text: &str) -> u64 {
    ((text.len() as f64) / 4.0).ceil() as u64
}

/// Truncate a string to `max_chars`, appending "\n[truncated]" if
/// it was cut.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        let mut out = s[..max_chars].to_string();
        out.push_str("\n[truncated]");
        out
    }
}

/// Serialize a single message for the compaction prompt. Handles
/// different message types with appropriate formatting.
pub fn serialize_message(m: &Message) -> String {
    match m.role {
        crate::session::Role::User => {
            let mut out = format!("[User]: {}", m.content);
            if !m.tool_results.is_empty() {
                for tr in &m.tool_results {
                    let body = if tr.pruned {
                        PRUNE_PLACEHOLDER.to_string()
                    } else {
                        truncate(&tr.content, TOOL_OUTPUT_MAX_CHARS)
                    };
                    out.push_str(&format!("\n[Tool result {}]: {}", tr.name, body));
                }
            }
            out
        }
        crate::session::Role::Assistant => {
            let mut out = if m.content.is_empty() {
                "[Assistant]: (empty)".to_string()
            } else {
                format!("[Assistant]: {}", m.content)
            };
            // Assistant messages also carry tool results (e.g. the
            // streaming tool blocks). Surface them in the summary so
            // the compaction model retains what each tool produced,
            // with the same prune/truncate discipline as user messages.
            for tr in &m.tool_results {
                let body = if tr.pruned {
                    PRUNE_PLACEHOLDER.to_string()
                } else {
                    truncate(&tr.content, TOOL_OUTPUT_MAX_CHARS)
                };
                out.push_str(&format!("\n[Tool result {}]: {}", tr.name, body));
            }
            out
        }
        crate::session::Role::System => {
            format!("[System update]: {}", m.content)
        }
    }
}

/// Token budget protected for recent tool outputs. Older tool
/// results whose cumulative content exceeds this budget are pruned
/// (their AI-facing content is replaced with a placeholder).
/// Matches opencode's `PRUNE_PROTECT`.
pub const PRUNE_PROTECT_TOKENS: u64 = 40_000;

/// Placeholder substituted for a pruned tool's AI-facing content.
pub const PRUNE_PLACEHOLDER: &str = "[Old tool result content cleared]";

/// Tools whose output must never be pruned (e.g. `skill`, which
/// injects instructions the model relies on for the rest of the
/// session).
const PRUNE_PROTECTED_TOOLS: &[&str] = &["skill"];

/// Walk the session backward and mark old tool results as pruned
/// once their cumulative token estimate exceeds `PRUNE_PROTECT_TOKENS`.
/// Already-pruned tools stop the accumulation (their content is
/// already cleared). Protected tools (e.g. `skill`) are skipped.
///
/// This is an in-place, idempotent pass: running it repeatedly only
/// newly-overflows get marked. The TUI still shows the original
/// `content`; only the value sent to the LLM is swapped (see the
/// `pruned` flag consumer in `commands.rs`).
pub fn prune(messages: &mut [crate::session::Message]) {
    let mut accumulated: u64 = 0;
    // Iterate messages in reverse chronological order.
    for m in messages.iter_mut().rev() {
        for tr in m.tool_results.iter_mut().rev() {
            if tr.pruned {
                // Already cleared; doesn't add to the budget.
                continue;
            }
            if PRUNE_PROTECTED_TOOLS.contains(&tr.name.as_str()) {
                continue;
            }
            let tokens = estimate_tokens(&tr.content);
            accumulated += tokens;
            if accumulated > PRUNE_PROTECT_TOKENS {
                tr.pruned = true;
            }
        }
    }
}

/// Select which messages to keep as recent context and which to
/// compact. Walks backward from the most recent messages, accumulating
/// token estimates, until the `keep_tokens` budget is exhausted.
/// Returns `(head, recent)` where `head` is the serialized text of
/// messages to compact and `recent` is the serialized text to keep
/// verbatim.
pub fn select(messages: &[Message], keep_tokens: u64) -> Option<(String, String)> {
    if messages.is_empty() {
        return None;
    }
    let conversation: Vec<String> = messages
        .iter()
        .map(serialize_message)
        .filter(|s| !s.is_empty())
        .collect();
    if conversation.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    let mut split = conversation.len();
    for (i, item) in conversation.iter().enumerate().rev() {
        let next = total + estimate_tokens(item);
        if next > keep_tokens {
            split = i + 1;
            break;
        }
        total = next;
        split = i;
    }
    let head = conversation[..split].join("\n\n");
    let recent = conversation[split..].join("\n\n");
    if head.is_empty() && recent.is_empty() {
        return None;
    }
    Some((head, recent))
}

/// Build the compaction prompt. If `previous_summary` is provided,
/// the LLM is asked to update the existing summary rather than
/// create a new one. Matches opencode's `buildPrompt`.
pub fn build_prompt(previous_summary: Option<&str>, context: &[String]) -> String {
    let mut parts = Vec::new();
    if let Some(prev) = previous_summary {
        parts.push(format!(
            "Update the anchored summary below using the conversation history above.\n\
             Preserve still-true details, remove stale details, and merge in the new facts.\n\
             <previous-summary>\n{prev}\n</previous-summary>"
        ));
    } else {
        parts.push("Create a new anchored summary from the conversation history.".to_string());
    }
    parts.push(SUMMARY_TEMPLATE.to_string());
    for ctx in context {
        if !ctx.is_empty() {
            parts.push(ctx.clone());
        }
    }
    parts.join("\n\n")
}

/// Pre-flight check: estimate the token count of the given messages
/// and return `true` if compaction is needed before sending.
/// This mirrors opencode's `compactIfNeeded` pre-flight guard.
///
/// `messages` are the serialized model messages (the same ones that
/// will be sent to the API). Returns `false` when context window is
/// unknown or auto-compaction is disabled.
pub fn compact_if_needed(messages: &[String], system_prompt: &str, inp: CompactionInputs) -> bool {
    if !inp.auto_enabled || inp.ctx_window == 0 {
        return false;
    }
    let mut total: u64 = 0;
    total += estimate_tokens(system_prompt);
    for m in messages {
        total += estimate_tokens(m);
    }
    let u = usable(inp);
    if u == 0 {
        return false;
    }
    total > u
}

/// Locate the slice of `messages` that should be replaced with a
/// single summary message. Returns
/// `Some((start_inclusive, end_exclusive))` where everything in
/// `[start, end)` gets dropped, and everything before `start` plus
/// everything from `end` onwards is kept.
///
/// Returns `None` when there is not enough history to compact (i.e.
/// compacting would leave an empty "head" or empty "tail").
pub fn plan_cutoff(messages: &[Message], tail_turns: usize) -> Option<(usize, usize)> {
    if messages.is_empty() {
        return None;
    }
    let turns = tail_turns.max(1);

    let user_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m.role, crate::session::Role::User))
        .map(|(i, _)| i)
        .collect();
    if user_idxs.len() <= turns {
        return None;
    }
    let cut_start_user = user_idxs[user_idxs.len() - turns];
    if cut_start_user == 0 {
        return None;
    }
    Some((0, cut_start_user))
}

/// Same as [`plan_cutoff`] but always returns `Some` for any
/// non-empty session, even if there is only one turn (i.e. the
/// resulting head would otherwise be empty). Used by the manual
/// `/compact` command so the user can summarize a one-turn session
/// and start fresh with a single summary block as context.
pub fn plan_cutoff_force(messages: &[Message]) -> Option<(usize, usize)> {
    if messages.is_empty() {
        return None;
    }
    if messages.len() == 1 && matches!(messages[0].role, crate::session::Role::System) {
        return None;
    }
    Some((0, messages.len()))
}

/// Given a compaction range `[start, end)` and a maximum character
/// limit, walk backward from `end` and return the greatest `start`
/// index such that the content of `messages[adjusted..end]` fits
/// within `max_chars` when formatted by [`build_summary_prompt`].
/// Returns `min(start, end)` (i.e. `end`) when even a single message
/// exceeds the limit — the caller should treat that as "skip
/// compaction".
pub fn trim_to_size(messages: &[Message], start: usize, end: usize, max_chars: usize) -> usize {
    let overhead = SUMMARY_PROMPT.len() + 50;
    let max_content = max_chars.saturating_sub(overhead);
    let mut total = 0usize;
    let mut new_start = end;
    for i in (start..end).rev() {
        let m = &messages[i];
        let role_len = match m.role {
            crate::session::Role::User => 4,
            crate::session::Role::Assistant => 9,
            crate::session::Role::System => 6,
        };
        let body = if m.content.is_empty() {
            "<empty>"
        } else {
            &m.content
        };
        let msg_len = role_len + body.len() + 10;
        total += msg_len;
        if total > max_content {
            return new_start;
        }
        new_start = i;
    }
    start
}

/// Build the user-prompt body for the LLM that will summarize the
/// dropped history. Uses the legacy format; new callers should
/// prefer `build_prompt` + `select` for structured summaries.
pub fn build_summary_prompt(history: &[Message]) -> String {
    let mut out = String::new();
    out.push_str(SUMMARY_PROMPT);
    out.push_str("\n\n--- conversation history to summarize ---\n");
    for (i, m) in history.iter().enumerate() {
        let role = match m.role {
            crate::session::Role::User => "user",
            crate::session::Role::Assistant => "assistant",
            crate::session::Role::System => "system",
        };
        let body = if m.content.is_empty() {
            "<empty>".to_string()
        } else {
            m.content.clone()
        };
        out.push_str(&format!("[#{i} {role}] {body}\n"));
    }
    out
}

/// Check if an error message indicates a context-length overflow.
/// Used to detect API errors that should trigger auto-compaction.
pub fn is_context_overflow_error(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("context length")
        || lower.contains("token limit")
        || lower.contains("too long")
        || lower.contains("1000000")
        || lower.contains("input length")
        || lower.contains("max context")
        || lower.contains("maximum context")
        || lower.contains("reduce the length")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Message, Role, ToolResultBlock};

    fn user(s: &str) -> Message {
        Message::new(Role::User, s)
    }
    fn assistant(s: &str) -> Message {
        Message::new(Role::Assistant, s)
    }

    fn tool_block(name: &str, content: &str) -> ToolResultBlock {
        ToolResultBlock {
            name: name.to_string(),
            title: name.to_string(),
            content: content.to_string(),
            metadata: String::new(),
            content_offset: 0,
            visible: true,
            running: false,
            failed: false,
            call_id: String::new(),
            pruned: false,
            streaming_input: String::new(),
            cached_line_count_visible: None,
            cached_line_count_collapsed: None,
        }
    }

    #[test]
    fn prune_clears_old_tool_outputs_beyond_budget() {
        // Each tool block is ~2500 chars ≈ 625 tokens. With a 40k
        // token budget, ~64 blocks fit; the 65th and older get pruned.
        let big = "x".repeat(2500);
        let mut m = assistant("ran tools");
        for _ in 0..80 {
            m.tool_results.push(tool_block("read", &big));
        }
        let mut msgs = vec![user("hi"), m];
        prune(&mut msgs);
        let tools = &msgs[1].tool_results;
        // Most recent blocks (tail) must stay unpruned.
        assert!(!tools.last().unwrap().pruned, "newest tool must survive");
        // Some older ones must be pruned.
        assert!(
            tools.iter().filter(|t| t.pruned).count() > 0,
            "expected at least one pruned tool"
        );
    }

    #[test]
    fn prune_protects_skill_tool() {
        let big = "x".repeat(200_000); // well over budget
        let mut m = assistant("ran tools");
        m.tool_results.push(tool_block("skill", &big));
        m.tool_results.push(tool_block("read", &big));
        let mut msgs = vec![m];
        prune(&mut msgs);
        assert!(
            !msgs[0].tool_results[0].pruned,
            "skill output must never be pruned"
        );
        assert!(
            msgs[0].tool_results[1].pruned,
            "read output should be pruned"
        );
    }

    #[test]
    fn prune_is_idempotent() {
        let big = "x".repeat(200_000);
        let mut m = assistant("ran tools");
        m.tool_results.push(tool_block("read", &big));
        let mut msgs = vec![m];
        prune(&mut msgs);
        assert!(msgs[0].tool_results[0].pruned);
        // Running again must not panic or flip the flag back.
        prune(&mut msgs);
        assert!(msgs[0].tool_results[0].pruned);
    }

    #[test]
    fn usable_uses_buffer_when_output_unknown() {
        let inp = CompactionInputs {
            auto_enabled: true,
            ctx_window: 128_000,
            max_output_tokens: 0,
            reserved_override: None,
        };
        assert_eq!(usable(inp), 128_000 - COMPACTION_BUFFER);
    }

    #[test]
    fn usable_clamps_to_min_buffer_and_max_output() {
        let inp = CompactionInputs {
            auto_enabled: true,
            ctx_window: 200_000,
            max_output_tokens: 16_384,
            reserved_override: None,
        };
        assert_eq!(usable(inp), 200_000 - 16_384);
        let inp = CompactionInputs {
            auto_enabled: true,
            ctx_window: 200_000,
            max_output_tokens: 1_000,
            reserved_override: None,
        };
        assert_eq!(usable(inp), 200_000 - 1_000);
    }

    #[test]
    fn usable_zero_when_context_unknown() {
        let inp = CompactionInputs {
            auto_enabled: true,
            ctx_window: 0,
            max_output_tokens: 0,
            reserved_override: None,
        };
        assert_eq!(usable(inp), 0);
    }

    #[test]
    fn should_auto_compact_disabled_never_fires() {
        let inp = CompactionInputs {
            auto_enabled: false,
            ctx_window: 128_000,
            max_output_tokens: 0,
            reserved_override: None,
        };
        assert!(!should_auto_compact(u64::MAX, inp));
    }

    #[test]
    fn should_auto_compact_at_threshold() {
        let inp = CompactionInputs {
            auto_enabled: true,
            ctx_window: 128_000,
            max_output_tokens: 0,
            reserved_override: None,
        };
        let u = usable(inp);
        assert!(!should_auto_compact(u - 1, inp));
        assert!(should_auto_compact(u, inp));
        assert!(should_auto_compact(u + 1, inp));
    }

    #[test]
    fn headroom_pct_clamps_to_zero() {
        let inp = CompactionInputs {
            auto_enabled: true,
            ctx_window: 128_000,
            max_output_tokens: 0,
            reserved_override: None,
        };
        let u = usable(inp);
        assert!((headroom_pct(0, inp).unwrap() - 1.0).abs() < 1e-9);
        assert!((headroom_pct(u, inp).unwrap() - 0.0).abs() < 1e-9);
        assert!((headroom_pct(u + 1_000_000, inp).unwrap() - 0.0).abs() < 1e-9);
    }

    #[test]
    fn headroom_pct_none_when_ctx_unknown() {
        let inp = CompactionInputs {
            auto_enabled: true,
            ctx_window: 0,
            max_output_tokens: 0,
            reserved_override: None,
        };
        assert_eq!(headroom_pct(0, inp), None);
    }

    #[test]
    fn plan_cutoff_returns_none_for_empty() {
        let msgs: Vec<Message> = vec![];
        assert_eq!(plan_cutoff(&msgs, 2), None);
    }

    #[test]
    fn plan_cutoff_returns_none_when_too_short() {
        let msgs = vec![user("a"), assistant("a"), user("b"), assistant("b")];
        assert_eq!(plan_cutoff(&msgs, 2), None);
    }

    #[test]
    fn plan_cutoff_keeps_last_two_turns() {
        let msgs = vec![
            user("u1"),
            assistant("a1"),
            user("u2"),
            assistant("a2"),
            user("u3"),
            assistant("a3"),
        ];
        assert_eq!(plan_cutoff(&msgs, 2), Some((0, 2)));
    }

    #[test]
    fn plan_cutoff_returns_none_when_everything_is_tail() {
        let msgs = vec![user("u1")];
        assert_eq!(plan_cutoff(&msgs, 1), None);
    }

    #[test]
    fn build_summary_prompt_includes_messages() {
        let msgs = vec![user("hello"), assistant("hi")];
        let p = build_summary_prompt(&msgs);
        assert!(p.contains("hello"));
        assert!(p.contains("hi"));
        assert!(p.contains(SUMMARY_PROMPT));
    }

    #[test]
    fn plan_cutoff_force_works_for_one_turn() {
        let msgs = vec![user("hi"), assistant("hello")];
        assert_eq!(plan_cutoff_force(&msgs), Some((0, 2)));
    }

    #[test]
    fn plan_cutoff_force_rejects_empty() {
        let msgs: Vec<Message> = vec![];
        assert_eq!(plan_cutoff_force(&msgs), None);
    }

    #[test]
    fn plan_cutoff_force_rejects_pure_summary() {
        let msgs = vec![Message::new(Role::System, "summary")];
        assert_eq!(plan_cutoff_force(&msgs), None);
    }

    #[test]
    fn estimate_tokens_english() {
        // 48 chars of English text ≈ 12 tokens (48/4)
        let text = "This is a test of the token estimation function.";
        let tokens = estimate_tokens(text);
        assert_eq!(tokens, 12);
    }

    #[test]
    fn serialize_message_user() {
        let m = user("hello world");
        let s = serialize_message(&m);
        assert_eq!(s, "[User]: hello world");
    }

    #[test]
    fn serialize_message_assistant() {
        let m = assistant("hi there");
        let s = serialize_message(&m);
        assert_eq!(s, "[Assistant]: hi there");
    }

    #[test]
    fn serialize_message_empty_assistant() {
        let m = assistant("");
        let s = serialize_message(&m);
        assert_eq!(s, "[Assistant]: (empty)");
    }

    #[test]
    fn serialize_message_system() {
        let m = Message::new(Role::System, "config updated");
        let s = serialize_message(&m);
        assert_eq!(s, "[System update]: config updated");
    }

    #[test]
    fn select_returns_none_for_empty() {
        let msgs: Vec<Message> = vec![];
        assert_eq!(select(&msgs, 100), None);
    }

    #[test]
    fn select_splits_on_budget() {
        // 4 messages: ~3+4+3+4 = 14 tokens, budget of 5 keeps the last
        let msgs = vec![user("a"), assistant("bb"), user("c"), assistant("dd")];
        let result = select(&msgs, 5);
        assert!(result.is_some());
        let (head, recent) = result.unwrap();
        assert!(!head.is_empty());
        assert!(!recent.is_empty());
    }

    #[test]
    fn build_prompt_includes_template() {
        let prompt = build_prompt(None, &["head content".to_string()]);
        assert!(prompt.contains("## Objective"));
        assert!(prompt.contains("## Important Details"));
        assert!(prompt.contains("## Work State"));
        assert!(prompt.contains("## Next Move"));
        assert!(prompt.contains("head content"));
    }

    #[test]
    fn build_prompt_updates_previous_summary() {
        let prompt = build_prompt(Some("old summary"), &["new content".to_string()]);
        assert!(prompt.contains("Update the anchored summary"));
        assert!(prompt.contains("<previous-summary>"));
        assert!(prompt.contains("old summary"));
        assert!(prompt.contains("new content"));
    }

    #[test]
    fn compact_if_needed_returns_false_when_fits() {
        let inp = CompactionInputs {
            auto_enabled: true,
            ctx_window: 128_000,
            max_output_tokens: 0,
            reserved_override: None,
        };
        let messages = vec!["short message".to_string()];
        let system = "short system prompt".to_string();
        assert!(!compact_if_needed(&messages, &system, inp));
    }

    #[test]
    fn compact_if_needed_returns_false_when_disabled() {
        let inp = CompactionInputs {
            auto_enabled: false,
            ctx_window: 128_000,
            max_output_tokens: 0,
            reserved_override: None,
        };
        let messages = vec!["x".repeat(500_000)]; // huge message
        let system = "system".to_string();
        assert!(!compact_if_needed(&messages, &system, inp));
    }

    #[test]
    fn is_context_overflow_error_detects() {
        assert!(is_context_overflow_error("context_length_exceeded"));
        assert!(is_context_overflow_error("input length too long"));
        assert!(is_context_overflow_error("token limit exceeded"));
        assert!(is_context_overflow_error(
            "range of input length should be [1, 1000000]"
        ));
        assert!(is_context_overflow_error(
            "reduce the length of the messages"
        ));
        assert!(!is_context_overflow_error("network timeout"));
        assert!(!is_context_overflow_error("auth failed"));
    }
}
