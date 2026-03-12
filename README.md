# rum

a diff-centric coding agent TUI built in rust.

rum takes a different approach from chat-style agent interfaces. the user prompt sits at the top, with a live activity feed and inline diffs below it, and token/cost metrics in the header bar.

```
┌──────────────────────────────────────────────┐
│ rum  ~/dev/project  claude-sonnet-4  47k $0.1│
├──────────────────────────────────────────────┤
│ › refactor the auth module to use jose       │
│                                              │
│   Read src/auth/jwt.ts                       │
│   Read src/auth/middleware.ts                │
│   Edit src/auth/jwt.ts          +84 -67      │
│   Edit src/auth/middleware.ts   +12 -18      │
│ ◌ editing src/auth/refresh.ts...             │
│                                              │
└──────────────────────────────────────────────┘
```

## setup

rum reads configuration from pi's config directory (`~/.pi/agent/`):

- **auth**: uses `~/.pi/agent/auth.json` for api credentials (oauth tokens from `pi /login`, api keys)
- **settings**: reads `~/.pi/agent/settings.json` for default provider, model, and thinking level. per-project overrides can be placed in `.pi/settings.json` within the project directory.
- **context files**: loads `AGENTS.md` / `CLAUDE.md` from the global config, parent directories, and cwd
- **system prompt**: uses `SYSTEM.md` / `APPEND_SYSTEM.md` from `~/.pi/agent/` or `.pi/` within the project directory. project-level `SYSTEM.md` takes precedence over the global one. `APPEND_SYSTEM.md` files from both locations are appended.

authenticate via pi first (`pi` then `/login`), or set `ANTHROPIC_API_KEY` in your environment.

## usage

```bash
cargo install --path .

# interactive TUI mode
rum

# with initial message
rum "list all the files in src/"

# override model
rum --model claude-sonnet-4-20250514

# override provider
rum --provider anthropic

# set thinking level (off, minimal, low, medium, high, xhigh)
rum --thinking high "solve this complex problem"

# different working directory
rum -C /path/to/project

# print mode: stream output to stdout without the TUI
rum -p "explain this codebase"
```

## keybindings

| key | action |
|-----|--------|
| Enter | submit message |
| Ctrl+C | clear input / cancel running / quit |
| Escape | cancel running / quit |
| Up/Down | scroll activity feed |
| PageUp/PageDown | scroll by page |
| Left/Right | move cursor in input |
| Home/End | jump to start/end of input |
| Ctrl+O | toggle diff expansion |

scrolling up disables auto-scroll. scrolling back to the bottom re-engages it.

## tools

rum provides four tools to the model:

- **read**: read file contents with optional offset/limit
- **bash**: execute shell commands with timeout
- **edit**: surgical find-and-replace edits
- **write**: create or overwrite files

## architecture

```
src/
├── main.rs       - cli parsing, event loop, TUI/agent coordination
├── config.rs     - loads auth.json, settings.json, system prompts, context files
├── api.rs        - anthropic messages api types and SSE event parsing
├── agent.rs      - agent loop: streaming, tool execution, turn management
├── tools.rs      - tool definitions and implementations (read, bash, edit, write)
├── tui.rs        - ratatui-based diff-centric layout and input handling
├── markdown.rs   - markdown-to-styled-text rendering (ansi + ratatui spans)
└── print.rs      - non-interactive print mode, streams output to stdout
```
