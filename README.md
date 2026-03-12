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
- **settings**: reads `~/.pi/agent/settings.json` for default provider, model, and thinking level
- **context files**: loads `AGENTS.md` / `CLAUDE.md` from the global config, parent directories, and cwd
- **system prompt**: uses `SYSTEM.md` / `APPEND_SYSTEM.md` from pi's config

authenticate via pi first (`pi` then `/login`), or set `ANTHROPIC_API_KEY` in your environment.

## usage

```bash
cargo install --path .

# interactive mode
rum

# with initial message
rum "list all the files in src/"

# override model
rum --model claude-sonnet-4-20250514

# set thinking level
rum --thinking high "solve this complex problem"

# different working directory
rum -C /path/to/project
```

## keybindings

| key | action |
|-----|--------|
| Enter | submit message |
| Ctrl+C | clear input / quit |
| Escape | quit |
| Up/Down | scroll activity feed |
| PageUp/PageDown | scroll by page |
| Ctrl+O | toggle diff expansion |

## tools

rum provides four tools to the model:

- **read**: read file contents with optional offset/limit
- **bash**: execute shell commands with timeout
- **edit**: surgical find-and-replace edits
- **write**: create or overwrite files

## architecture

```
src/
├── main.rs     - cli parsing, event loop, TUI/agent coordination
├── config.rs   - loads pi's auth.json, settings.json, context files
├── api.rs      - anthropic messages api with SSE streaming
├── agent.rs    - agent loop: streaming, tool execution, turn management
├── tools.rs    - tool definitions and implementations
└── tui.rs      - ratatui-based diff-centric layout
```
