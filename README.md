# fish-coding-agent

An AI CLI coding agent with a TUI written in Rust. Supports multiple
providers (OpenAI, Anthropic, Cursor, DeepSeek, MiniMax, Volcengine),
MCP servers, skills, plan/ask/todo workflows, session persistence with
auto-compaction, and a rich set of built-in tools.

## Build

```sh
cargo build --release
```

Binary is at `target/release/fish-coding-agent.exe` (Windows) or
`target/release/fish-coding-agent` (Unix).

## Run

```sh
./target/release/fish-coding-agent
```

Config is stored at `~/.config/fish-coding-agent/config.json` (Unix) or
`%APPDATA%\fish-coding-agent\config.json` (Windows). A starter
`config.example.json` is included; on first run a default config is
written automatically. The loader migrates from the old format and
self-heals bad fields (a single corrupt entry is dropped, not the whole
file).

## TUI layout

Top-to-bottom vertical stack:

1. **Agents splash** — ASCII logo + load-duration + checkboxes for
   discovered `agents.md` files (`~/.agents/agents.md` and `./agents.md`).
   Shown only on a fresh session before the first input.
2. **Session panel** — scrollable markdown transcript with thinking,
   tool, `[skill]`, and attachment blocks; right-edge scrollbar; mouse
   selection; click a block to collapse/expand.
3. **Function panel** — hidden by default (only the `Notifications` tab
   exists). Dynamic height = `min(content + overhead, 30% of remaining)`,
   min 4 rows. Title bar lists all open tabs (` | ` separated, active in
   bold). Fail-level toasts force-show the panel; other toasts bump the
   `[!N]` unread badge. Slash commands and completion auto-show it.
   `Ctrl+N` toggles it; closing via `Ctrl+N` clears the notification
   queue (transient toast model).
4. **Input block** — grows with wrapped content (capped so the session
   keeps ≥50% of the viewport). Hardware cursor; scrollbar when scrolled.
5. **CWD line** — `~ <path>` on the left; right-aligned live stats:
   token usage, context-window headroom %, cache-hit rate, token/sec,
   MCP summary (`2✓ 1✗`), and an elapsed timer with progressive
   Esc-cancel (`esc to interrupt` → `esc again`).

## Configuration

Config schema (new format; the loader auto-migrates the old one):

```json
{
  "active": "openai:key",
  "thinking": "off",
  "thinking_display": "show",
  "tool_display": "show",
  "enter_behavior": "enter_sends",
  "tool_preview_lines": 10,
  "border_type": "ascii",
  "theme": "default",
  "auto_compact": true,
  "compact_reserved": null,
  "entries": {
    "openai:key": {
      "api_key": "",
      "api_key_env": "OPENAI_API_KEY",
      "base_url": "https://api.openai.com/v1",
      "model": "gpt-4o-mini",
      "name": "OpenAI",
      "access_key": "",
      "secret_key": ""
    }
  },
  "mcp": {},
  "agents": { "entries": {}, "visible": true }
}
```

### Provider entries

A provider entry is identified by `<kind>:<mode>` — e.g. `openai:key`,
`anthropic:env`. Duplicate `(kind, mode)` pairs get a dedup suffix
(`openai:key-2`).

- **Kinds**: `openai`, `anthropic`, `cursor`, `deepseek`, `minimax`,
  `volcengine`.
- **Modes**: `key` (inline `api_key`), `env` (env var named by
  `api_key_env`), `oauth` (Cursor only — browser PKCE flow).
  `api_key` and `api_key_env` are an **OR**: if `api_key` is non-empty
  it wins; otherwise the env var is read. At least one must resolve.
- `base_url` is **required** (empty value rejected with an inline
  `[!] base_url is required` error on save).
- Cursor entries store the access token in `api_key` and the refresh
  token in `api_key_env`; `model == "auto"` is normalized to empty.
- Volcengine entries additionally need `access_key`/`secret_key` for
  HMAC-SHA256 model-list signing.
- `name` is the friendly label shown in the status bar as `name:model`;
  `model_display` is an optional friendlier model label.

### Settings (`/settings`)

Hierarchical menu — `Esc` returns to the previous level; only the top
level closes the tab. The current level's shortcuts are shown in dim
gray at the bottom.

| Level            | Items                                                                  |
|------------------|------------------------------------------------------------------------|
| top level        | set provider · thinking display · tool display · enter behavior · border type · theme · auto compact · tool preview lines |
| `set provider`   | + new provider  /  openai:key, ...                                     |
| `new`            | OpenAI (custom) / Anthropic (custom) / Cursor (oauth) / DeepSeek / MiniMax / Volcengine |
| existing entry   | edit  /  delete                                                        |
| edit / new form  | base url *  /  key or env  /  save  /  exit                            |

Hot-switchable options:

- **Thinking display** — `show` / `hide` / `while streaming`
- **Tool result display** — `show` / `hide` / `while streaming`
- **Enter behavior** — `enter_sends` (Enter sends, Shift+Enter newline) / `enter_newline` (Enter newline, Shift+Enter sends)
- **Border type** — `ascii` / `rounded`
- **Theme** — `default` (terminal defaults) / `light-eucalyptus` / `dark-eucalyptus`
- **Tool preview lines** — 3–50 (default 10): collapsed tool block lines before the `Ctrl+O` expand hint
- **Auto-compact** — on/off

## Slash commands

| Command                     | Action                                                                  |
|-----------------------------|-------------------------------------------------------------------------|
| `/settings`                 | Open a fresh hierarchical settings tab                                  |
| `/model`                    | Pick configured entry → pick cached model (or `Ctrl+M` for a manual id) |
| `/think` [`/thinking`]      | Set reasoning level (off/minimal/low/med/high/xhigh/adaptive/max); no arg opens a picker |
| `/timeline`                 | Message-timeline picker; Enter jumps the session viewport              |
| `/session` [`/sessions`]   | Session manager: `Enter` resume · `r` rename · `d` delete · `f` fork · `Tab` toggle local/global scope |
| `/rename [title]`           | Rename the current (or target) session                                  |
| `/fork`                     | Fork the current session into a new one                                 |
| `/retry`                    | Re-send the last user prompt (drops the prior assistant turn)           |
| `/continue [text]`          | Continue an interrupted response (synthetic prompt, hidden from the log)|
| `/compact`                  | Manually trigger compaction (ignores `auto_compact`)                    |
| `/plan` [`exit`/`off`/`yolo`/`build`] | Switch to read-only plan mode (`/plan exit` returns to yolo)   |
| `/yolo` [`/build`]          | Switch to yolo (build) mode                                            |
| `/tool` [`/tools`]          | Open the tool toggle picker (enable/disable tools for this session)    |
| `/skill`                    | No arg: list skills. `/skill:<name> [args]`: invoke a skill             |
| `/mcp`                      | No arg: list MCP servers. `/mcp:<name>`: report a server's status       |
| `/mcp-auth <name>`          | Start the OAuth flow for a remote MCP server                            |
| `/mcp-logout <name>`        | Remove stored OAuth tokens for a remote MCP server                      |
| `/mcp-debug <name>`         | Print diagnostics: status, auth state, tool count                       |
| `/new` [`/clear`]           | Reset the conversation                                                   |
| `/hotkey` [`/help` `/keys`] | Keyboard reference                                                       |
| `/quit` [`/exit` `/q`]      | Quit                                                                     |

### Prefix tool inputs (typed directly, no chat round-trip)

| Prefix  | Runs                                  |
|---------|---------------------------------------|
| `!<cmd>`   | `shell_command` (no chat context)   |
| `!!<cmd>`  | `shell_command`, output fed to chat |
| `$<code>`  | `python_command` (no chat context)  |
| `$$<code>` | `python_command`, output fed to chat |

## Providers

| Provider   | Chat                                   | Model list                          | Notes                                                                 |
|------------|----------------------------------------|-------------------------------------|-----------------------------------------------------------------------|
| OpenAI     | `/chat/completions` (OpenAI-compat)    | `/models`                           | Default `https://api.openai.com/v1`; env `OPENAI_API_KEY`            |
| Anthropic  | `/v1/messages` (native)               | `/v1/models`                        | Default `https://api.anthropic.com`; env `ANTHROPIC_API_KEY`         |
| Cursor     | Connect-RPC protobuf over HTTP/2       | protobuf `GetUsableModels`          | OAuth PKCE, browser flow; default `https://api2.cursor.sh`            |
| DeepSeek   | OpenAI-compat                          | `/models`                           | Default `https://api.deepseek.com`; env `DEEPSEEK_API_KEY`            |
| MiniMax    | OpenAI-compat                          | `/models`                           | Default `https://api.minimaxi.com`; env `MINIMAX_API_KEY`             |
| Volcengine | OpenAI-compat (via `OpenAiProvider`)   | `/models` + V4 HMAC-SHA256 signing  | Default `https://ark.cn-beijing.volces.com/api/plan/v3`; env `VOLCENGINE_API_KEY`; needs `access_key`/`secret_key` |

All providers stream via SSE. Live **cache-hit rate** parsed from usage:
OpenAI `prompt_tokens_details.cached_tokens`, Anthropic
`cache_read_input_tokens` + `cache_creation_input_tokens`. Reasoning
levels map to Anthropic `thinking` (off→omitted, adaptive→`adaptive`,
else `enabled` with a budget) or OpenAI `reasoning_effort`. Model list
is cached in `model-cache.json`; context-window data is augmented from
models.dev (`model-data.json`) with manual overrides in
`context-cache.json`. Rate-limit errors surface as Warn toasts that
auto-clear on success; context-overflow errors trigger compaction+retry.

## MCP

Client-only (via the `rmcp` crate). Two transports per server:

- **Local** — spawn a child process, speak MCP over stdio.
- **Remote** — streamable-HTTP (with SSE fallback), optional per-server
  OAuth (local callback server, browser-based).

Configured under the top-level `mcp` key (each entry is a full config
or a `{ "enabled": false }` toggle to disable a remote default). Tools
from connected servers are merged into the agent's tool surface (capped
at 256, 200-char description limit) and shown with a `[mcp:server]`
prefix. Status-bar summary like `2✓ 1✗`. Use `/mcp`, `/mcp:<name>`,
`/mcp-auth`, `/mcp-logout`, `/mcp-debug` to manage.

## Built-in tools

| Tool            | Notes                                                                  |
|-----------------|-------------------------------------------------------------------------|
| `read`          | Read a file with optional `start_line`/`end_line`                        |
| `edit` (`write`)| Exact-string replacement with fuzzy fallback, CRLF normalization, `replaceAll`, line scoping |
| `shell_command` (`command`) | Shell exec, 300s default timeout, 16 KB output cap, env-utf8 preamble on Windows |
| `python_command`| Python exec, 300s timeout                                              |
| `grep`          | Regex search with optional `path` and `glob` filter                     |
| `glob`          | File-name pattern matching, sorted by mtime                            |
| `list`          | Directory listing                                                       |
| `plan`          | Present a plan for approval in the function panel                       |
| `ask`           | Ask the user a clarifying question (with options)                       |
| `todowrite`     | Maintain a structured todo list (opens a Todo sidebar tab)              |
| `skill`         | Load a skill's instructions into the conversation                       |
| `webfetch`      | Fetch a URL as text/markdown/html (max 120 s)                           |
| `websearch`     | Web search with `numResults`/`livecrawl`/`type`/`contextMaxCharacters`  |
| `sub_agent`     | Delegate to a `general` or `explore` sub-agent; `max_steps` default 15 (cap 100); no recursion |

Tool output is uniformly truncated to 2000 lines / 50 KiB; overflow is
saved to a temp file under `fish_coding_agent_tool_output/` and the
model is told to use `grep`/`read`. MCP tools appear as
`mcp_<server>_<tool>`.

## Permission system

Tools are gated by agent role and sub-agent type:

- **Build (yolo)** — all tools.
- **Plan** — read-only (denies `edit`/`write`/`shell_command`/`python_command`/`webfetch`/`websearch`/`sub_agent`).
- **Sub-agent `general`** — all tools except `sub_agent` (no recursion).
- **Sub-agent `explore`** — read-only (`read`/`grep`/`glob`/`list`/`webfetch`/`websearch`).

`/plan` switches to Plan mode; `/yolo`/`/build` switches back. Per-session
`/tool` disabling also filters the LLM tool list.

## Skills

Named prompt templates loaded from `~/.agents/skills/<name>/SKILL.md`
(mirrors the `~/.claude/skills/` layout). The file is markdown with
optional YAML frontmatter (`name`/`description`/`license`); the body
after the closing `---` is the template. `/skill:<name> [args]` sends
the rendered template to the AI and renders a `[skill]` block in the
session (the block never reaches the model). `/skill` (no arg) lists up
to 8 skills + "N more". Available skills are advertised to the model in
the system prompt; the `skill` tool can load them at runtime.

## Session & compaction

Sessions persist as JSON under `<config_dir>/sessions/<id>/session.json`
(capturing messages, todos, provider/model/thinking, token totals,
context window, max output, auto_compact, MCP summary). Pasted images
are stored in `sessions/<id>/assets/<sha>.png`. Titles auto-derive from
the first user prompt. Input history is capped at 200 entries.

- **Auto-compact** (default on) fires when
  `used >= ctx_window - reserved`, where
  `reserved = compact_reserved ?? min(20000, max_output)`. Post-response
  and pre-flight triggers; on success a synthetic "Continue if you have
  next steps…" prompt is queued.
- **Manual `/compact`** ignores `auto_compact`; tries `plan_cutoff`
  (keeps the last 2 turns) then falls back to force-trim; output capped
  at 500 000 chars. The `skill` tool is never pruned.
- **Overflow recovery** — context-length errors trigger compaction+retry.

The summary template is structured Markdown
(`## Objective / Important Details / Work State / Next Move`) and
supports incremental updates via a `previous_summary`.

## Key bindings

### Global

| Key                | Action                                                                  |
|--------------------|-------------------------------------------------------------------------|
| Ctrl+Q             | Quit (cancels inflight)                                                 |
| Ctrl+C             | Copy selection (full-TUI > input) / clear input                          |
| Ctrl+I             | Focus input; closes all non-Notifications tabs                          |
| Ctrl+L             | Clear session                                                           |
| Ctrl+N             | Toggle the Notifications panel (dedicated)                              |
| Ctrl+Z / Ctrl+Y   | Undo / redo input (100-snapshot stack)                                  |
| Ctrl+W             | Delete word back                                                        |
| Ctrl+A / Ctrl+E   | Move cursor to start / end of line                                       |
| Ctrl+U             | Clear the entire input buffer                                            |
| Ctrl+K             | Truncate from cursor to end of line                                      |
| Alt+L              | Cycle focus: Input → FunctionPanel → AgentsCheckbox → Input             |
| Alt+V              | Open the paste-preview panel (clipboard image or text)                   |
| Tab                | Complete the focused candidate; else jump to/switch to the Plan tab     |
| Shift+Tab          | Cycle sidebar tabs (wraps last→first)                                    |
| Enter              | Submit; or insert newline (see `enter_behavior`)                         |
| Shift/Ctrl/Alt+Enter | Modifier-Enter — reliable send in EnterNewline mode (Windows consoles drop Shift for Enter) |
| Esc                | Progressive: first Esc → "esc again" hint (2 s timeout); second Esc → cancel inflight. Else close tab / clear selection / clear input |
| Home / End         | Scroll session to top / bottom                                           |
| PageUp / PageDown  | Scroll session one page                                                  |
| Up / Down          | In completion: move candidate; else move cursor / browse input history   |
| Shift+Left/Right   | Extend text selection in the input                                        |
| Mouse drag         | Select text anywhere in the TUI                                          |
| Mouse wheel        | Scroll the session (instant) or input area                               |
| Click on a block   | Toggle collapse/expand (tool / thinking)                                 |

### Per-tab (shown in dim gray at the bottom of each tab)

- **ModelPicker** — `Enter` select · `Ctrl+R` refresh · `Ctrl+M` manual id · `Ctrl+E` edit · `Esc` close
- **ProviderPicker** — `Enter` pick · `Up/Down` nav · type to filter · `Ctrl+E` edit · `Esc` close
- **TimelinePicker** — `Enter` jump · `Up/Down` nav · `Esc` close
- **SessionPicker** — `Tab` scope · `r` rename · `d` delete · `f` fork · `Enter` resume
- **SessionRename** — `Enter` save · `Esc` close
- **Plan** — `Enter` approve · `Alt+R` reject · `Alt+S` save · `Esc` close
- **ToolPicker** — `Space` toggle · `Enter` confirm · `Esc` close
- **PastePreview** — `Enter` paste · `Esc` cancel
- **Notifications** — `Alt+I` search · `Esc` exit/clear/close · `Up/Down` navigate
- **Todo** — `Enter` toggle status · `Delete` remove · `Alt+I` insert below · `Alt+Shift+I` insert above · `Alt+E` edit · `Alt+C` clear all · `Esc` cancel edit

## Function panel tabs

`Notifications` (always present, `Ctrl+N`) · `Completion` (auto on `/`) ·
`Settings` · `ModelPicker` · `ProviderPicker` · `ThinkingPicker` ·
`TimelinePicker` · `SessionPicker` · `SessionRename` · `Plan` · `Ask` ·
`Todo` · `ToolPicker` · `PastePreview` · `Hotkey`.

## agents.md

Per-file enabled/disabled checkboxes (discovered from `~/.agents/agents.md`
and `./agents.md`); enabled file bodies are injected into the system
prompt. The included `agents.md` enforces `cargo check`/`clippy`/`fmt`
after every modification.

## Logging

`tracing-subscriber` to stderr; default filter
`warn,fish_coding_agent=info`, overridable via `RUST_LOG`. On panic the
terminal is restored, the message + optional backtrace is printed and
persisted to `<config_dir>/panic-<ts>.log`.

