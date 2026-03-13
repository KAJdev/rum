# rum

a fast, diff-centric coding agent for the terminal. built in rust.

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

run `rum` in any project directory. if you don't have credentials, it will open your browser to log in with your Anthropic account automatically — paste the code back and you're in.

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
| `/new` | clear conversation and start fresh |
| `/login` | log in with anthropic oauth |
| `/logout` | log out |
| `/help` | show all commands |
| `/quit` | exit |

### keybindings

| key | action |
|-----|--------|
| **Enter** | send message (queues if agent is running) |
| **Shift+Enter** | newline |
| **Ctrl+C** | clear input → cancel agent → quit |
| **Escape** | cancel agent / quit |
| **Ctrl+O** | toggle diff expansion |
| **Tab** | autocomplete slash commands |
| **Up/Down** | cursor / scroll |
| **PageUp/PageDown** | scroll by page |

## tools

the agent has five tools:

| tool | what it does |
|------|-------------|
| **read** | read file contents (with optional line offset/limit) |
| **write** | create or overwrite files |
| **edit** | surgical find-and-replace (exact match of old text) |
| **bash** | run shell commands |
| **web_search** | search the web via DuckDuckGo |

edits and writes show inline diffs. bash output is shown inline too.

## context files

rum loads `AGENTS.md` and `CLAUDE.md` files from every directory between the filesystem root and your cwd. use these to give the agent project-specific instructions.

custom system prompts go in `~/.config/rum/SYSTEM.md` (global) or `.rum/SYSTEM.md` (project). `APPEND_SYSTEM.md` in either location is appended to the default prompt.

## print mode

`rum -p` streams output to stdout without the TUI. tool calls and thinking go to stderr. prints a summary with tokens, cost, and timing when done. useful for scripting or piping into other tools.
