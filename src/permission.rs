use serde::{Deserialize, Serialize};

/// One tool invocation's permission verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// Tool may run, no user prompt.
    Allow,
    /// Tool is rejected outright; the model must find another way.
    Deny,
}

/// Names of tools the agent may invoke. Centralised so the rule tables
/// stay in sync with `crate::tools`.
pub mod tool {
    pub const READ_FILE: &str = "read";
    pub const WRITE_FILE: &str = "edit";
    pub const SHELL_COMMAND: &str = "shell_command";
    pub const PYTHON_COMMAND: &str = "python_command";
    pub const GREP: &str = "grep";
    pub const LIST: &str = "list";
    pub const PLAN: &str = "plan";
    pub const ASK: &str = "ask";
    pub const TODO_WRITE: &str = "todowrite";
}

/// `build` / `yolo` agent: every tool is allowed. Used in normal
/// (non-plan) mode where the user wants maximum autonomy.
fn yolo_rules() -> &'static [(&'static str, Action)] {
    &[
        (tool::READ_FILE, Action::Allow),
        (tool::WRITE_FILE, Action::Allow),
        (tool::SHELL_COMMAND, Action::Allow),
        (tool::PYTHON_COMMAND, Action::Allow),
        (tool::GREP, Action::Allow),
        (tool::LIST, Action::Allow),
        (tool::PLAN, Action::Allow),
        (tool::ASK, Action::Allow),
        (tool::TODO_WRITE, Action::Allow),
    ]
}

/// `plan` agent: read-only exploration plus the `plan` and `ask`
/// tools. The plan agent must NOT mutate the user's tree (no
/// `write_file`, no `shell_command`, no `python_command`). This
/// mirrors opencode's `plan` agent, which sets `edit: "*": "deny"`.
fn plan_rules() -> &'static [(&'static str, Action)] {
    &[
        (tool::READ_FILE, Action::Allow),
        (tool::WRITE_FILE, Action::Deny),
        (tool::SHELL_COMMAND, Action::Deny),
        (tool::PYTHON_COMMAND, Action::Deny),
        (tool::GREP, Action::Allow),
        (tool::LIST, Action::Allow),
        (tool::PLAN, Action::Allow),
        (tool::ASK, Action::Allow),
        (tool::TODO_WRITE, Action::Allow),
    ]
}

/// `agent` values that select a rule table. Kept as a tiny enum so
/// the call sites don't pass stray strings around.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Build,
    Plan,
}

impl Agent {
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Build => "build",
            Agent::Plan => "plan",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "build" | "yolo" => Some(Agent::Build),
            "plan" => Some(Agent::Plan),
            _ => None,
        }
    }
}

pub fn check(agent: Agent, tool: &str) -> Action {
    let rules = match agent {
        Agent::Build => yolo_rules(),
        Agent::Plan => plan_rules(),
    };
    rules
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(_, a)| *a)
        .unwrap_or_else(|| {
            // Unknown tools (e.g. MCP-discovered `<server>_<tool>`)
            // are allowed in build/yolo mode (max autonomy) and
            // denied in plan mode (read-only exploration).
            match agent {
                Agent::Build => Action::Allow,
                Agent::Plan => Action::Deny,
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_mode_blocks_mutation_tools() {
        assert_eq!(check(Agent::Plan, tool::WRITE_FILE), Action::Deny);
        assert_eq!(check(Agent::Plan, tool::SHELL_COMMAND), Action::Deny);
        assert_eq!(check(Agent::Plan, tool::PYTHON_COMMAND), Action::Deny);
    }

    #[test]
    fn plan_mode_allows_read_tools() {
        assert_eq!(check(Agent::Plan, tool::READ_FILE), Action::Allow);
        assert_eq!(check(Agent::Plan, tool::GREP), Action::Allow);
        assert_eq!(check(Agent::Plan, tool::LIST), Action::Allow);
    }

    #[test]
    fn plan_mode_allows_plan_tool() {
        assert_eq!(check(Agent::Plan, tool::PLAN), Action::Allow);
    }

    #[test]
    fn build_mode_allows_everything() {
        for t in [
            tool::READ_FILE,
            tool::WRITE_FILE,
            tool::SHELL_COMMAND,
            tool::PYTHON_COMMAND,
            tool::GREP,
            tool::LIST,
            tool::PLAN,
            tool::ASK,
        ] {
            assert_eq!(check(Agent::Build, t), Action::Allow, "{t} should allow");
        }
    }

    #[test]
    fn plan_mode_allows_ask_tool() {
        assert_eq!(check(Agent::Plan, tool::ASK), Action::Allow);
    }

    #[test]
    fn unknown_tool_in_build_is_allowed() {
        assert_eq!(check(Agent::Build, "no_such_tool"), Action::Allow);
    }

    #[test]
    fn unknown_tool_in_plan_is_denied() {
        assert_eq!(check(Agent::Plan, "no_such_tool"), Action::Deny);
    }

    #[test]
    fn agent_parse_roundtrip() {
        assert_eq!(Agent::parse("build"), Some(Agent::Build));
        assert_eq!(Agent::parse("yolo"), Some(Agent::Build));
        assert_eq!(Agent::parse("plan"), Some(Agent::Plan));
        assert_eq!(Agent::parse("nope"), None);
    }
}
