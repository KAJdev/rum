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

the header bar shows a colored sparkline of token throughput, tokens/sec, cost, and a context window usage bar.

## setup

rum reads configuration from pi's config directory (`~/.pi/agent/`), or from `PI_CODING_AGENT_DIR` if set.

**auth**: authenticate via one of:
- set `ANTHROPIC_API_KEY` in your environment
- run `pi` then `/login` to store oauth credentials in `~/.pi/agent/auth.json`

**settings**: reads `~/.pi/agent/settings.json` for default provider, model, and thinking level. per-project overrides can be placed in `.pi/settings.json` within the project directory. supported fields:

```json
{
  "defaultProvider": "anthropic",
  "defaultModel": "claude-sonnet-4-20250514",
  "defaultThinkingLevel": "high"
}
```

**context files**: loads `AGENTS.md` / `CLAUDE.md` from the global config dir, all ancestor directories from root to cwd, and cwd itself.

**system prompt**: uses `SYSTEM.md` / `APPEND_SYSTEM.md` from `~/.pi/agent/` or `.pi/` within the project directory. project-level `SYSTEM.md` takes precedence over the global one. `APPEND_SYSTEM.md` files from both locations are appended. if no custom system prompt is found, a built-in default is used.

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

### print mode

`rum -p` runs without the TUI, streaming markdown-rendered output directly to stdout. tool calls, diffs, and thinking are shown on stderr. prints a summary line with token count, cost, tool count, throughput, and elapsed time when done. exits with code 1 if any errors occurred.

### message queuing

you can type and submit messages while the agent is running. queued messages are sent automatically when the current turn finishes.

## keybindings

### general

| key | action |
|-----|--------|
| Enter | submit message (or queue if agent is running) |
| Shift+Enter / Alt+Enter / Ctrl+Enter | insert newline |
| Ctrl+J | insert newline |
| Ctrl+C | clear input / cancel running / quit (in that priority) |
| Escape | cancel running / quit |
| Ctrl+O | toggle diff expansion |

### navigation

| key | action |
|-----|--------|
| Up/Down | move cursor in multi-line input, or scroll activity feed |
| PageUp/PageDown | scroll activity feed by page |
| Left/Right | move cursor |
| Alt+Left / Alt+Right | move cursor by word |
| Cmd+Left / Cmd+Right | jump to line start/end |
| Home/End | jump to line start/end |
| Ctrl+A / Ctrl+E | jump to line start/end |

### editing

| key | action |
|-----|--------|
| Ctrl+U | delete to line start |
| Ctrl+K | delete to line end |
| Ctrl+W / Alt+Backspace | delete word backward |
| Alt+D | delete word forward |
| Cmd+Backspace | delete to line start |

scrolling up disables auto-scroll. scrolling back to the bottom re-engages it.

## tools

rum provides five tools to the model:

- **read**: read file contents with optional line offset/limit (defaults to 2000 lines, truncates at ~50KB)
- **bash**: execute shell commands with configurable timeout (default 120s)
- **edit**: surgical find-and-replace edits (requires a unique exact match of `oldText`)
- **write**: create or overwrite files, creating parent directories as needed
- **web_search**: search the web via DuckDuckGo, returning titles, URLs, and snippets

tool results are displayed inline in the activity feed. edit and write tools show inline diffs with addition/deletion counts. bash output is shown truncated to the first 8 lines.

## thinking

thinking level controls extended thinking budget. for most models, this sets a token budget:

| level | budget |
|-------|--------|
| off | disabled |
| minimal | 1,024 tokens |
| low | 4,096 tokens |
| medium | 10,240 tokens |
| high | 32,768 tokens |
| xhigh | 65,536 tokens |

for opus 4.6+ models, thinking uses adaptive mode with an effort parameter instead of a fixed budget.

## architecture

```
src/
├── main.rs       - cli parsing, event loop, TUI/print mode dispatch
├── config.rs     - auth, settings, system prompts, context file resolution
├── api.rs        - anthropic messages api client, request types, SSE parsing
├── agent.rs      - agentic loop: streaming, tool execution, multi-turn management
├── tools.rs      - tool definitions (read, bash, edit, write) and diff computation
├── tui.rs        - ratatui-based layout, input handling, sparkline, diff rendering
├── markdown.rs   - markdown-to-styled-text rendering (ansi for print, ratatui spans for TUI)
└── print.rs      - non-interactive streaming mode with incremental json field tracking
```
