pub use crate::function::{App, InflightHandle};

impl App {
    /// Validate the config and surface the issues as a single toast.
    /// Returns true if there were no issues.
    ///
    /// We deliberately do NOT auto-open the settings tab here. The user's
    /// "default hidden" preference means we only push a toast (which auto-
    /// shows the panel because Fail is important). The user opens /settings
    /// when they are ready to fix things.
    ///
    /// Three cases:
    /// 1. All entries validate. -> No toast.
    /// 2. At least one entry is usable, but some are misconfigured. ->
    ///    Consolidated list of the specific errors so the user can fix
    ///    them.
    /// 3. No entry is usable (e.g. fresh install, both `openai:key` and
    ///    `anthropic:key` are placeholders). -> Single friendly prompt:
    ///    "set up openai or anthropic to start using".
    pub fn check_config(&mut self) -> bool {
        let errs = self.config.validate_all();
        if errs.is_empty() {
            return true;
        }
        use crate::function::notifications::ToastLevel;
        let has_usable = self
            .config
            .entries
            .keys()
            .any(|id| self.config.validate_provider(id).is_ok());
        let msg = if has_usable {
            if errs.len() == 1 {
                errs.into_iter().next().unwrap()
            } else {
                format!(
                    "config: {} issues, open /settings to fix: {}",
                    errs.len(),
                    errs.join(" | ")
                )
            }
        } else {
            "no provider configured; open /settings to set up openai or anthropic".to_string()
        };
        self.notify(ToastLevel::Fail, msg);
        false
    }

    /// Mark the LLM tool spec cache as dirty. The next chat request
    /// will re-read `tools::openai_tool_specs` / `anthropic_tool_specs`,
    /// picking up any new MCP tool definitions. The OpenAI / Anthropic
    /// provider calls are stateless w.r.t. the tool list, so a
    /// single bool is enough.
    pub fn invalidate_tool_specs(&mut self) {
        self.mcp_tools_dirty = true;
    }
}
