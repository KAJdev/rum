# rum

a coding agent and editor fused into one terminal UI. built in rust.

rum is a dual-pane TUI where one side is an AI agent that can read, write, and run commands across your codebase, and the other side is a full editor with syntax highlighting. follow mode links the two together: as the agent works through files, the editor tracks every read and edit in real time, jumping to each change with inline diff markers.

```
┌─────────────────────────────────────────────────────────────────────────┐
│ rum  ~/dev/project  (main)  claude-sonnet-4   ▂▃▅▂ 120 tok/s   $0.04  │
├────────────────────────────────────────┬────────────────────────────────┤
│  1  use std::path::Path;              │ › refactor auth to use jose    │
│  2  use std::time::Instant;           │                                │
│  3                                    │   Read src/auth/jwt.ts         │
│  4+ use jose::jwt::JwtClaims;         │   Read src/auth/middleware.ts  │
│  5+ use jose::jws::JsonWebSignature;  │   Edit src/auth/jwt.ts  +84   │
│  6                                    │ ◌ editing middleware.ts...      │
│  7  pub fn verify(token: &str) {      │                                │
│  8      let claims = JwtClaims::new() │                                │
│                                       │                                │
├───────────────────────────────────────┴─────────────────────────────────┤
│ >                                                                      │
└────────────────────────────────────────────────────────────────────────-┘
```

## install

```bash
# homebrew
brew install KAJdev/rum/rum

# cargo binstall (pre-built binary)
cargo binstall rum

# cargo from source
cargo install --path .
```

## getting started

run `rum` in any project directory. if you don't have credentials, it will open your browser to log in with your Anthropic account automatically -- paste the code back and you're in.

you can also set `ANTHROPIC_API_KEY` in your environment if you prefer API key auth.

## usage

```bash
# start the TUI
rum

# start with a message
rum "add error handling to the api routes"

# print mode -- streams to stdout, no TUI
rum -p "explain this codebase"

# override model or thinking level
rum --model opus --thinking high "refactor the auth module"

# different working directory
rum -C /path/to/project
```

## the editor

`Ctrl+E` switches between chat and editor. the editor has syntax highlighting (syntect), undo/redo, and file saving. `Ctrl+P` opens a fuzzy file finder, `Ctrl+/` searches text across the project.

### follow mode

`Ctrl+F` turns on follow mode, which is the core of how rum connects the agent to the editor. every file the agent touches (reads or edits) is tracked. the editor automatically opens each file and jumps to the relevant line:

- edits get diff markers in the gutter -- green for insertions, red for deletions
- reads jump to the offset the agent requested
- `Alt+Up/Down` walks through the full history of file operations
- the status bar shows your position (e.g. `follow 3/7`)

a condensed activity sidebar on the right side of the editor shows agent progress without needing to switch back to chat.

## tools

seven tools, executed in parallel when possible:

| tool | what it does |
|------|-------------|
| **read** | read file contents (with optional line offset/limit) |
| **write** | create or overwrite files |
| **edit** | surgical find-and-replace (exact match) |
| **bash** | run shell commands (streams in real time) |
| **explore** | spawn a read-only sub-agent to investigate a topic |
| **web_search** | search the web via DuckDuckGo |
| **view_file** | view and describe an image file |

edits and writes show inline diffs in the chat feed. click to expand/collapse.

## keybindings

### chat

| key | action |
|-----|--------|
| **Enter** | send message (queues if agent is running) |
| **Shift+Enter** | newline |
| **Ctrl+C** | cancel agent / clear input / quit |
| **Escape** | cancel agent / clear input |
| **Up/Down** | input history |
| **Shift+Up/Down** | scroll feed |
| **PageUp/PageDown** | scroll by page |
| **Ctrl+O** | toggle diff expansion |
| **Tab** | autocomplete slash commands |

### editor

| key | action |
|-----|--------|
| **Ctrl+E** | toggle chat / editor |
| **Ctrl+F** | toggle follow mode |
| **Ctrl+P** | fuzzy file finder |
| **Ctrl+/** | text search across files |
| **Ctrl+S** | save |
| **Ctrl+Z / Ctrl+Y** | undo / redo |
| **Ctrl+K** | delete line |
| **Alt+Up/Down** | previous / next agent edit (follow mode) |
| **Shift+Up/Down** | half-page scroll |
| **PageUp/PageDown** | full page scroll |

### slash commands

| command | description |
|---------|-------------|
| `/model [name]` | switch model (opus, sonnet, haiku, etc.) |
| `/thinking [level]` | set thinking (off, minimal, low, medium, high, xhigh) |
| `/compact` | summarize and compress conversation context |
| `/cd <path>` | change working directory |
| `/new` | clear conversation and start fresh |
| `/login` | log in with anthropic oauth |
| `/logout` | log out |
| `/help` | show all commands |
| `/quit` | exit |

### `!` bash commands

prefix any input with `!` to run a shell command inline. output streams to the feed and is injected into context.

```
!git status
!cargo test --lib
```

## sessions

conversations are saved per-directory and restored on restart, including the full activity feed and input history. use `/compact` to summarize long conversations and free up context. the header shows context usage.

## context files

rum loads `AGENTS.md` and `CLAUDE.md` from every directory between the filesystem root and your cwd, plus global config dirs (`~/.config/rum/`, `~/.pi/agent/`, `~/.claude/`).

custom system prompts are checked in order: `.rum/SYSTEM.md`, `.pi/SYSTEM.md`, `.claude/SYSTEM.md`, `~/.config/rum/SYSTEM.md`, `~/.pi/agent/SYSTEM.md`, then the built-in default. `APPEND_SYSTEM.md` files from all locations are appended.

settings (default model, thinking level) merge from `~/.config/rum/config.json` and `~/.pi/agent/settings.json`.

## notifications

a terminal bell fires when the agent finishes a turn. configure your terminal to show a notification, bounce the dock icon, or play a sound on bell.

## print mode

`rum -p` streams to stdout without the TUI. tool calls and thinking go to stderr. prints a summary with tokens, cost, and timing at the end.
