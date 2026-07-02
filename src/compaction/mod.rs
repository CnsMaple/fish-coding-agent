//! Auto-compaction.
//!
//! Mirrors opencode's `compaction.ts` + `overflow.ts` flow, but in
//! Rust and with a much smaller surface area. The goals are:
//!
//! 1. Decide **when** to compact: a `should_auto_compact` predicate
//!    matches opencode's `isOverflow` formula:
//!    ```text
//!    used >= ctx_window - reserved
//!    ```
//!    where `reserved` defaults to `COMPACTION_BUFFER` (20 000), or
//!    `Config::compact_reserved` if the user has overridden it.
//!
//! 2. Decide **what** to compact: `plan_cutoff` returns the `[start,
//!    end)` slice of `Session::messages` that will be replaced by a
//!    single `Role::System` summary message. The last
//!    `DEFAULT_TAIL_TURNS` user/assistant turns are preserved, matching
//!    opencode's `DEFAULT_TAIL_TURNS = 2`.
//!
//! 3. Generate the summary: `build_summary_prompt` is the prompt we
//!    send to the LLM. The actual stream task lives in
//!    `event::spawn_compaction_task` so the caller can reuse the same
//!    `ChatRequest` machinery as the normal send path.

use crate::session::Message;

/// Buffer reserved for the model's reply. Matches opencode's
/// `COMPACTION_BUFFER` in `overflow.ts`. Used as the default
/// `reserved` value when `Config::compact_reserved` is `None`.
pub const COMPACTION_BUFFER: u64 = 20_000;

/// Number of trailing user/assistant turns to keep verbatim.
/// Matches opencode's `DEFAULT_TAIL_TURNS = 2`. Each "turn" is a
/// pair of (user, assistant) messages; partial tails (e.g. a lone
/// streaming assistant) are also preserved.
pub const DEFAULT_TAIL_TURNS: usize = 2;

/// Prompt asking the model to summarize a chunk of history. Kept
/// as a `const` so the unit tests can pin it.
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

    // Collect every user-message index; the tail is the last
    // `turns` entries. Everything between the first message and
    // the start of the tail is the "head" we may compact.
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
    // Skip a leading summary block if the session is *just* a
    // summary already (re-compacting a summary is a no-op the
    // user does not need).
    if messages.len() == 1 && matches!(messages[0].role, crate::session::Role::System) {
        return None;
    }
    Some((0, messages.len()))
}

/// Build the user-prompt body for the LLM that will summarize the
/// dropped history. The output is plain text; the chat path will
/// pass it through `commands::send_chat` after the summary task
/// succeeds.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Message, Role};

    fn user(s: &str) -> Message {
        Message::new(Role::User, s)
    }
    fn assistant(s: &str) -> Message {
        Message::new(Role::Assistant, s)
    }

    #[test]
    fn usable_uses_buffer_when_output_unknown() {
        // 128k context, 0 max-output => reserved defaults to
        // COMPACTION_BUFFER (we treat 0 as "unknown", not "free").
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
        // Huge output cap is clamped down to BUFFER; the buffer
        // is the upper bound on the reservation.
        let inp = CompactionInputs {
            auto_enabled: true,
            ctx_window: 200_000,
            max_output_tokens: 16_384,
            reserved_override: None,
        };
        assert_eq!(usable(inp), 200_000 - 16_384);
        // Tiny output cap wins; we only reserve what the model
        // will actually emit.
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
        // 2 users, 2 assistants — exactly the tail; nothing to compact.
        let msgs = vec![user("a"), assistant("a"), user("b"), assistant("b")];
        assert_eq!(plan_cutoff(&msgs, 2), None);
    }

    #[test]
    fn plan_cutoff_keeps_last_two_turns() {
        // 3 user+assistant turns; with tail_turns=2 we keep the
        // last two (positions 2..6) and compact the first
        // (positions 0..2). The "turn" boundary is a user
        // message index, matching opencode's `turns()` helper.
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
        // Single user message, no history to compact.
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
        // A session that is *only* a single compaction summary is
        // already compact; re-compacting is a no-op.
        let msgs = vec![Message::new(Role::System, "summary")];
        assert_eq!(plan_cutoff_force(&msgs), None);
    }
}
