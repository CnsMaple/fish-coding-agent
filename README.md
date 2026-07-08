# fish-coding-agent

An AI CLI agent with a TUI written in Rust.

## Features

- Three-panel TUI: session / function panel / input + status bar
- Function panel starts hidden with only the `Notifications` tab; auto-shows on toast or slash command, `Ctrl+N` toggles it. Closing the panel via `Ctrl+N` clears the notification queue (transient toast model)
- Input area is always 4 rows tall; the saved space when the function panel is hidden goes to the session, not the input
- Sidebar tabs (Shift+Tab to cycle) for notifications, completion, settings, model picker, hotkey reference
- `/settings` opens a fresh hierarchical settings tab every time (each invocation is a new view; old tabs are kept and closable with Esc)
- `/model` to browse cached models (fetched via `GET /v1/models`) or enter a manual model id
- `/hotkey` for the keyboard reference
- `/clear` to reset the conversation
- Streaming chat responses for both OpenAI and Anthropic
- Live cache-hit rate parsed from API usage (`prompt_tokens_details.cached_tokens` / `cache_read_input_tokens`)
- ASCII-only style with the `system` theme (terminal default colors)

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

Config is stored at `~/.config/fish-coding-agent/config.json` (or
`%APPDATA%\fish-coding-agent\config.json` on Windows). A starter
`config.example.json` is included.

### Configuration rules

- A provider is identified by `(kind, mode)` — for example `openai:key`,
  `anthropic:env`. The config may contain up to four entries
  (`openai:key`, `openai:env`, `anthropic:key`, `anthropic:env`).
- `base_url` is **required** for every entry. Saving the form with an
  empty `base_url` is rejected with an inline `[!] base_url is required`
  error.
- `api_key` and `api_key_env` are an **OR**: if `api_key` is non-empty it
  is used; otherwise the env var named by `api_key_env` is read. At least
  one of them must resolve for chat and model-list requests to succeed.
- Pick the active entry from the `/model` picker.

### Settings navigation

`/settings` is a hierarchical menu (no more inner tabs). `Esc` returns
to the previous level; only the top level closes the tab. The current
level's shortcuts are always shown in dim gray at the bottom.

| Level            | Items                                              |
|------------------|----------------------------------------------------|
| `settings`       | set provider                                       |
| `set provider`   | + new provider  /  openai:key, ...                 |
| `new`            | OpenAI (key) / Anthropic (key) / OpenAI (env) / Anthropic (env) |
| existing entry   | edit  /  delete                                    |
| edit / new form  | base url *  /  key or env  /  save  /  exit        |

## Key bindings

| Key            | Action                                          |
|----------------|-------------------------------------------------|
| Tab            | Cycle inner tabs                                |
| Shift+Tab      | Cycle sidebar tabs                              |
| Enter          | Complete focused command + send / confirm       |
| Esc            | Clear selection / close sidebar tab / clear input |
| Up / Down      | Navigate completion candidates / history        |
| Shift+Left/Right | Extend text selection                         |
| Mouse drag     | Select text in the input box                    |
| Ctrl+Q         | Quit                                            |
| Ctrl+C         | Copy selection to clipboard / clear input       |
| Ctrl+I         | Focus input (close any open sidebar tab)        |
| Ctrl+L         | Clear session                                   |
| Ctrl+N         | Toggle notifications panel (dedicated to notify) |
| /              | Open completion                                 |
