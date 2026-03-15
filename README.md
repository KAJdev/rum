# rum

a fast coding agent for the terminal. built in rust.

```
┌────────────────────────────────────────────────────────────────────────┐
│ rum  ~/dev/project  (main)  claude-sonnet-4    ▂▃▅▂ 120 tok/s  $0.04   │
├────────────────────────────────────────────────────────────────────────┤
│ › refactor the auth module to use jose                                 │
│                                                                        │
│   Read src/auth/jwt.ts                                                 │
│   Read src/auth/middleware.ts                                          │
│   Edit src/auth/jwt.ts          +84 -67                                │
│   Edit src/auth/middleware.ts   +12 -18                                │
│ ◌ editing src/auth/refresh.ts...                                       │
│                                                                        │
└────────────────────────────────────────────────────────────────────────┘
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

# print mode — no TUI, streams to stdout
rum -p "explain this codebase"

# override model or thinking level
rum --model opus --thinking high "refactor the auth module"

# different working directory
rum -C /path/to/project
```

### slash commands

type these in the input box:

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

prefix any input with `!` to run a shell command inline. the output streams to the feed and is injected into context so the model can see what you ran.

```
!git status
!cargo test --lib
!cat src/main.rs | head -20
```

### keybindings

#### chat view

| key | action |
|-----|--------|
| **Enter** | send message (queues if agent is running) |
| **Shift+Enter** | newline |
| **Ctrl+C** | cancel agent / clear input / quit |
| **Escape** | cancel agent / clear input |
| **Up/Down** | input history |
| **Shift+Up/Down** | scroll activity feed |
| **Mouse scroll** | scroll activity feed |
| **PageUp/PageDown** | scroll by page |
| **Ctrl+O** | toggle diff expansion |
| **Tab** | autocomplete slash commands |

#### editor view

| key | action |
|-----|--------|
| **Ctrl+E** | toggle between chat and editor |
| **Ctrl+F** | toggle follow mode (auto-tracks agent file operations) |
| **Ctrl+P** | fuzzy file finder |
| **Ctrl+/** | text search across files |
| **Ctrl+S** | save file |
| **Ctrl+Z** | undo |
| **Ctrl+Y** | redo |
| **Ctrl+K** | delete line |
| **Alt+Up/Down** | navigate between agent edits (follow mode) |
| **Shift+Up/Down** | half-page scroll |
| **PageUp/PageDown** | full page scroll |
| **Arrow keys** | cursor movement |
| **Mouse scroll** | scroll (3 lines per tick) |

## editor

`Ctrl+E` opens a built-in editor with syntax highlighting (via syntect). you can browse, edit, and save project files without leaving the TUI.

### follow mode

`Ctrl+F` enables follow mode, which tracks every file the agent reads or edits and automatically opens the file in the editor, jumping to the relevant location:

- **edits** show inline diff markers: green gutter and background for insertions, red for deletions
- **reads** jump to the line offset the agent requested
- **Alt+Up/Down** navigates through the full history of agent file operations

the status bar shows your position in the edit history (e.g. `follow 3/7`).

an activity sidebar on the right side of the editor shows a condensed live feed of agent activity so you can watch progress without switching back to chat.

## tools

the agent has seven tools, executed in parallel when possible:

| tool | what it does |
|------|-------------|
| **read** | read file contents (with optional line offset/limit) |
| **write** | create or overwrite files |
| **edit** | surgical find-and-replace (exact match of old text) |
| **bash** | run shell commands |
| **explore** | spawn a read-only sub-agent to investigate a topic |
| **web_search** | search the web via DuckDuckGo |
| **view_file** | view an image file and describe its contents |

edits and writes show inline diffs. bash output streams in real time.

## session persistence

conversations are saved per-directory and restored on restart. the full activity feed (user messages, assistant responses, tool calls with output) is reconstructed when you reopen a project. input history (up-arrow) is also restored from the loaded session.

use `/compact` to summarize long conversations and free up context window space. the context usage bar in the header shows how full your context is.

## context files

rum loads `AGENTS.md` and `CLAUDE.md` files from every directory between the filesystem root and your cwd, as well as from the global config dirs (`~/.config/rum/`, `~/.pi/agent/`, `~/.claude/`). use these to give the agent project-specific instructions.

custom system prompts are checked in priority order:

1. `.rum/SYSTEM.md` in the project
2. `.pi/SYSTEM.md` in the project
3. `.claude/SYSTEM.md` in the project
4. `~/.config/rum/SYSTEM.md`
5. `~/.pi/agent/SYSTEM.md`
6. built-in default

`APPEND_SYSTEM.md` files from all locations (`~/.config/rum/`, `~/.pi/agent/`, `.rum/`, `.pi/`, `.claude/`) are appended.

settings (default model, thinking level) are merged from `~/.config/rum/config.json` and `~/.pi/agent/settings.json`, with rum's config taking priority.

on startup rum shows which config files it loaded so you know exactly what's in context.

## notifications

a terminal bell (`BEL`) fires when the agent finishes a turn. most terminals can be configured to show a system notification, bounce the dock icon, or play a sound when they receive a bell.

## print mode

`rum -p` streams output to stdout without the TUI. tool calls and thinking go to stderr. prints a summary with tokens, cost, and timing when done. useful for scripting or piping into other tools.
