use std::path::PathBuf;

use anyhow::Result;
use crossterm::cursor::{RestorePosition, SavePosition};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use fish_coding_agent::app::App;
use fish_coding_agent::{config, event};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    install_panic_hook();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            // Default: fish-coding-agent at info, everything else at warn.
            // Users can override via RUST_LOG env var.
            "warn,fish_coding_agent=info".parse().unwrap()
        }))
        .with_writer(std::io::stderr)
        .init();

    let config_path = config::paths::config_file_path()?;
    let cfg = match config::Config::load_or_init(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: could not load config: {e:#}");
            config::Config::default()
        }
    };

    let _guard = TerminalGuard::enter()?;

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    terminal.hide_cursor()?;

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // Initialize theme from config
    fish_coding_agent::theme::init_theme(cfg.theme);
    // Initialize the MCP service. This reads the config's `mcp`
    // section, spawns supervisor tasks for each enabled server,
    // and installs itself into `McpRegistry` so the rest of the
    // app can call `McpRegistry::current()`.
    {
        let mcp_cfg = cfg.mcp.clone();
        let cwd_for_mcp = cwd.clone();
        // `.await` is critical — without it the future is dropped
        // and the service never initialises.
        fish_coding_agent::mcp::McpService::init_from_config(&mcp_cfg, cwd_for_mcp).await;
    }
    let mut app = App::new(cfg, config_path, cwd);

    let res = event::run(&mut terminal, &mut app).await;

    // Tear down MCP clients (kills stdio child trees) before
    // returning so child processes don't outlive the TUI.
    if let Some(svc) = fish_coding_agent::mcp::McpRegistry::current() {
        svc.shutdown().await;
    }

    if let Err(e) = res {
        eprintln!("error: {e:#}");
    }
    Ok(())
}

/// Install a panic hook so that, instead of flashing a backtrace to stderr
/// and instantly dropping back to the shell, we:
///   1. Best-effort disable raw mode + clear the TUI area so the terminal
///      is usable after the crash.
///   2. Print the panic message and (if `RUST_BACKTRACE=1|full`) the
///      backtrace to stderr.
///   3. Persist the same info to `fish-coding-agent-panic.log` in the
///      current directory so the user can read it after the program exits.
///   4. Block on stdin so the user has time to read the output before the
///      process terminates.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        // Best-effort TUI teardown so the user can see the message.
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            DisableMouseCapture
        );

        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "non-string panic payload".to_string()
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());

        let mut output = String::new();
        output.push_str("\n\n[!!!] PANIC at ");
        output.push_str(&location);
        output.push('\n');
        output.push_str("[!!!] Message: ");
        output.push_str(&payload);
        output.push('\n');

        if matches!(
            std::env::var("RUST_BACKTRACE").as_deref(),
            Ok("1") | Ok("full")
        ) {
            let bt = std::backtrace::Backtrace::force_capture();
            output.push_str("[!!!] Backtrace:\n");
            output.push_str(&format!("{bt}\n"));
        }

        output.push_str("\n[!!!] Press Enter to exit.\n");

        use std::io::Write;
        let _ = std::io::stderr().write_all(output.as_bytes());
        let _ = std::io::stderr().flush();

        // Persist so the user can read it later even if the terminal
        // disappears. We put the log in the same config directory as the
        // app so the cwd is not polluted, and use a timestamp suffix so
        // repeat panics don't clobber previous logs.
        let log_path = config::paths::config_dir()
            .map(|d| {
                let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                d.join(format!("panic-{ts}.log"))
            })
            .unwrap_or_else(|_| PathBuf::from("fish-coding-agent-panic.log"));
        match std::fs::write(&log_path, &output) {
            Ok(_) => eprintln!("[!!!] Panic log written to: {}", log_path.display()),
            Err(e) => eprintln!("[!!!] Failed to write panic log: {e}"),
        }
        let _ = std::io::stderr().flush();

        // Block so the user can read the output before the process exits.
        let mut input = String::new();
        let _ = std::io::stdin().read_line(&mut input);
    }));
}

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> Result<Self> {
        execute!(std::io::stdout(), SavePosition)?;
        enable_raw_mode()?;
        execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        Ok(Self)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            crossterm::cursor::Show,
            crossterm::terminal::LeaveAlternateScreen,
            RestorePosition,
        );
        let _ = disable_raw_mode();
    }
}
