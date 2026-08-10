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

use fish_coding_agent::ui::backend::CursorTrackingBackend;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing_subscriber::EnvFilter;

/// Thread-safe log writer that shares one file handle across all tracing events.
/// Cloning cheaply shares the same `Arc<Mutex<...>>`.
struct TracingLog(Arc<Mutex<Box<dyn Write + Send>>>);

impl Clone for TracingLog {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Write for TracingLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    install_panic_hook();

    let tracing_writer: Box<dyn Write + Send> = config::paths::config_dir()
        .ok()
        .and_then(|dir| {
            let path = dir.join("fish-coding-agent.log");
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .inspect(|_| {
                    // Bleat the path so the user knows where logs live.
                    let _ = writeln!(std::io::stderr(), "tracing log → {}", path.display());
                })
                .ok()
        })
        .map(|f| Box::new(f) as Box<dyn Write + Send>)
        .unwrap_or_else(|| {
            // Fallback: discard tracing output silently.
            Box::new(std::io::sink()) as Box<dyn Write + Send>
        });
    let tracing_log = TracingLog(Arc::new(Mutex::new(tracing_writer)));

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,fish_coding_agent=info".parse().unwrap()),
        )
        .with_writer(move || tracing_log.clone())
        .init();

    let load_start = std::time::Instant::now();

    let config_path = config::paths::config_file_path()?;
    let cfg = match config::Config::load_or_init(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: could not load config: {e:#}");
            config::Config::default()
        }
    };

    // Headless mode: run a single autonomous task and print the final
    // reply to stdout, skipping the TUI entirely. Used for benchmark
    // / HLR harness comparison.
    if let Some(headless) = parse_headless_args()? {
        let cwd = headless
            .cwd
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let opts = fish_coding_agent::commands::HeadlessOptions {
            cwd,
            max_rounds: headless.max_rounds,
        };
        match fish_coding_agent::commands::run_headless_task(headless.prompt, opts).await {
            Ok(res) => {
                println!("{}", res.text);
                eprintln!(
                    "[headless] done: input={} output={} tokens, tool_rounds={}",
                    res.input_tokens, res.output_tokens, res.tool_rounds
                );
            }
            Err(e) => {
                eprintln!("[headless] error: {e:#}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    let _guard = TerminalGuard::enter()?;

    let backend = CursorTrackingBackend::new(CrosstermBackend::new(std::io::stdout()));
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    terminal.hide_cursor()?;

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // Initialize theme from config without blocking startup on the
    // terminal-background probe (auto-eucalyptus uses a dark fallback
    // now; the real resolution happens in the background once the TUI
    // is up and adjusts colors via `AppMsg::ThemeAutoResolved`).
    fish_coding_agent::theme::init_theme_deferred(cfg.theme);
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
    let load_duration = load_start.elapsed();
    let mut app = App::new(cfg, config_path, cwd, load_duration);

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

/// Headless CLI arguments (parsed from `--headless` mode).
struct HeadlessArgs {
    prompt: String,
    cwd: Option<PathBuf>,
    max_rounds: usize,
}

/// Parse CLI args for `--headless` mode. Returns `None` when not in
/// headless mode. Hand-written parser (no new dependency).
fn parse_headless_args() -> Result<Option<HeadlessArgs>> {
    let mut args = std::env::args().skip(1);
    let mut headless = false;
    let mut prompt: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut max_rounds = 0usize;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--headless" => headless = true,
            "--cwd" => {
                cwd = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--cwd requires a path"))?,
                ));
            }
            "--max-rounds" => {
                let v = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--max-rounds requires a number"))?;
                max_rounds = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid --max-rounds value: {v}"))?;
            }
            other => {
                // First positional argument is the prompt; the rest are
                // joined so multi-word prompts work without quotes.
                if prompt.is_none() {
                    prompt = Some(other.to_string());
                } else if let Some(p) = prompt.as_mut() {
                    p.push(' ');
                    p.push_str(other);
                }
            }
        }
    }

    if !headless {
        return Ok(None);
    }
    let prompt = prompt.ok_or_else(|| anyhow::anyhow!("--headless requires a prompt argument"))?;
    Ok(Some(HeadlessArgs {
        prompt,
        cwd,
        max_rounds,
    }))
}

/// Print the CLI usage/help text to stdout.
fn print_help() {
    println!(
        "\
fish-coding-agent — terminal coding agent

USAGE:
    fish-coding-agent [OPTIONS]

OPTIONS:
    --headless <prompt...>   Run a single autonomous task with no TUI and
                             print the final reply to stdout. Used for
                             benchmark / HLR harness comparison.
    --cwd <dir>              Working directory for headless tools (default: cwd).
    --max-rounds <N>         Cap the number of LLM tool-call round-trips in
                             headless mode (0 = unlimited, default).
    -h, --help               Print this help and exit.

With no options, the interactive TUI is launched.

EXAMPLES:
    fish-coding-agent --headless \"list 当前目录文件\"
    fish-coding-agent --headless \"fix the failing test\" --cwd D:\\repo --max-rounds 20
"
    );
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
            EnableBracketedPaste,
        )?;
        // Set a steady (non-blinking) block cursor so that MoveTo / Print
        // commands during rendering never reset the terminal's blink timer
        // and cause irregular cursor blinking.
        use std::io::Write;
        let _ = write!(std::io::stdout(), "\x1B[2 q");
        let _ = std::io::stdout().flush();
        Ok(Self)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        use std::io::Write;
        // Restore the terminal's default cursor style (blinking).
        let _ = write!(std::io::stdout(), "\x1B[0 q");
        let _ = std::io::stdout().flush();
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
