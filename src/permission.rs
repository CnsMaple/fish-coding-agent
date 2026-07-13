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
    pub const GLOB: &str = "glob";
    pub const WRITE: &str = "write";
    pub const SKILL: &str = "skill";
    pub const WEB_FETCH: &str = "webfetch";
    pub const WEB_SEARCH: &str = "websearch";
    pub const SUB_AGENT: &str = "sub_agent";
}

/// Sub-agent types that can be spawned by the `sub_agent` tool.
/// Each type has its own tool permission set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgent {
    /// Full tool access except `sub_agent` (no recursion).
    General,
    /// Read-only exploration: read, grep, glob, list, webfetch, websearch.
    Explore,
}

impl SubAgent {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "general" => Some(SubAgent::General),
            "explore" => Some(SubAgent::Explore),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SubAgent::General => "general",
            SubAgent::Explore => "explore",
        }
    }

    /// Return the permission rules for this sub-agent type.
    pub fn rules(self) -> &'static [(&'static str, Action)] {
        match self {
            SubAgent::General => general_sub_agent_rules(),
            SubAgent::Explore => explore_sub_agent_rules(),
        }
    }
}

/// `sub_agent` tool: always allowed in build mode, denied in plan mode.
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
        (tool::GLOB, Action::Allow),
        (tool::WRITE, Action::Allow),
        (tool::SKILL, Action::Allow),
        (tool::WEB_FETCH, Action::Allow),
        (tool::WEB_SEARCH, Action::Allow),
        (tool::SUB_AGENT, Action::Allow),
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
        (tool::GLOB, Action::Allow),
        (tool::WRITE, Action::Deny),
        (tool::SKILL, Action::Allow),
        (tool::WEB_FETCH, Action::Deny),
        (tool::WEB_SEARCH, Action::Deny),
        (tool::SUB_AGENT, Action::Deny),
    ]
}

/// `general` sub-agent: all tools except `sub_agent` (no recursion).
fn general_sub_agent_rules() -> &'static [(&'static str, Action)] {
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
        (tool::GLOB, Action::Allow),
        (tool::WRITE, Action::Allow),
        (tool::SKILL, Action::Allow),
        (tool::WEB_FETCH, Action::Allow),
        (tool::WEB_SEARCH, Action::Allow),
        (tool::SUB_AGENT, Action::Deny),
    ]
}

/// `explore` sub-agent: read-only tools only.
fn explore_sub_agent_rules() -> &'static [(&'static str, Action)] {
    &[
        (tool::READ_FILE, Action::Allow),
        (tool::WRITE_FILE, Action::Deny),
        (tool::SHELL_COMMAND, Action::Deny),
        (tool::PYTHON_COMMAND, Action::Deny),
        (tool::GREP, Action::Allow),
        (tool::LIST, Action::Allow),
        (tool::PLAN, Action::Deny),
        (tool::ASK, Action::Deny),
        (tool::TODO_WRITE, Action::Deny),
        (tool::GLOB, Action::Allow),
        (tool::WRITE, Action::Deny),
        (tool::SKILL, Action::Deny),
        (tool::WEB_FETCH, Action::Allow),
        (tool::WEB_SEARCH, Action::Allow),
        (tool::SUB_AGENT, Action::Deny),
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
            match agent {
                Agent::Build => Action::Allow,
                Agent::Plan => Action::Deny,
            }
        })
}

pub fn check_sub_agent(agent: SubAgent, tool: &str) -> Action {
    agent
        .rules()
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(_, a)| *a)
        .unwrap_or(Action::Deny)
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
            tool::GLOB,
            tool::WRITE,
            tool::SKILL,
            tool::WEB_FETCH,
            tool::WEB_SEARCH,
            tool::SUB_AGENT,
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

    #[test]
    fn sub_agent_parse_roundtrip() {
        assert_eq!(SubAgent::parse("general"), Some(SubAgent::General));
        assert_eq!(SubAgent::parse("explore"), Some(SubAgent::Explore));
        assert_eq!(SubAgent::parse("nope"), None);
    }

    #[test]
    fn explore_denies_write_tools() {
        assert_eq!(check_sub_agent(SubAgent::Explore, tool::WRITE_FILE), Action::Deny);
        assert_eq!(check_sub_agent(SubAgent::Explore, tool::WRITE), Action::Deny);
        assert_eq!(check_sub_agent(SubAgent::Explore, tool::SHELL_COMMAND), Action::Deny);
    }

    #[test]
    fn explore_allows_read_tools() {
        assert_eq!(check_sub_agent(SubAgent::Explore, tool::READ_FILE), Action::Allow);
        assert_eq!(check_sub_agent(SubAgent::Explore, tool::GREP), Action::Allow);
        assert_eq!(check_sub_agent(SubAgent::Explore, tool::GLOB), Action::Allow);
        assert_eq!(check_sub_agent(SubAgent::Explore, tool::WEB_FETCH), Action::Allow);
    }

    #[test]
    fn explore_denies_interaction_tools() {
        assert_eq!(check_sub_agent(SubAgent::Explore, tool::PLAN), Action::Deny);
        assert_eq!(check_sub_agent(SubAgent::Explore, tool::ASK), Action::Deny);
        assert_eq!(check_sub_agent(SubAgent::Explore, tool::TODO_WRITE), Action::Deny);
        assert_eq!(check_sub_agent(SubAgent::Explore, tool::SKILL), Action::Deny);
    }

    #[test]
    fn no_sub_agent_allows_recursion() {
        assert_eq!(check_sub_agent(SubAgent::General, tool::SUB_AGENT), Action::Deny);
        assert_eq!(check_sub_agent(SubAgent::Explore, tool::SUB_AGENT), Action::Deny);
    }

    #[test]
    fn general_allows_write_tools() {
        assert_eq!(check_sub_agent(SubAgent::General, tool::WRITE_FILE), Action::Allow);
        assert_eq!(check_sub_agent(SubAgent::General, tool::SHELL_COMMAND), Action::Allow);
    }
}
