# rum

a coding agent you can watch think. built in rust.

```bash
brew install KAJdev/rum/rum
```

<!-- TODO: asciinema demo -->

## why rum?

rum fuses a coding agent and a code editor into one terminal. one view is the agent chat feed. the other is a full editor with syntax highlighting, LSP, and fuzzy search. follow mode links them: as the agent reads and edits files, the editor tracks every operation in real time, jumping to each change with inline diffs. a sidebar on the editor shows agent progress without switching views.

you watch the agent work the way you'd watch a coworker's screen. except you can take the keyboard at any time.

## install

```bash
# homebrew
brew install KAJdev/rum/rum

# pre-built binary (via cargo-binstall)
cargo binstall rum-code

# from source
cargo install rum-code
```

## getting started

run `rum` in any project directory. first launch opens your browser to log in with Anthropic. you can also set `ANTHROPIC_API_KEY` if you prefer.

```bash
rum
rum "add error handling to the api routes"
rum -p "explain this codebase"
rum --model opus --thinking high "refactor the auth module"
```

## follow mode

`Ctrl+F`. the core of rum.

every file the agent touches is tracked. the editor opens each one automatically, jumps to the relevant section, and marks changes with inline diff markers. `Alt+Up/Down` walks the full history of file operations.

## editor

`Ctrl+E` switches between chat and editor. syntax highlighting, undo/redo, mouse support. `Ctrl+P` for fuzzy file search, `Ctrl+/` for project-wide text search, `Ctrl+G` for go-to-definition.

LSP starts automatically for rust, typescript, python, go, and c/c++. if a server isn't installed, rum downloads it.

## tools

the agent can read, edit, write, and search files. run shell commands (foreground or background). search the web. view images. spawn read-only sub-agents for deep investigation. query LSP for definitions and diagnostics. tools run in parallel when possible.

## sessions

conversations persist per-directory and restore on restart. `/compact` summarizes long conversations to free context. `/tree` opens a conversation branch navigator for forking and exploring different approaches.

## context

rum loads `AGENTS.md` and `CLAUDE.md` from every directory between the filesystem root and your cwd, plus `~/.config/rum/`, `~/.pi/agent/`, and `~/.claude/`. custom system prompts via `SYSTEM.md`. settings merge from config files.

## print mode

`rum -p` streams to stdout without the TUI. tool calls and thinking go to stderr. summary with tokens, cost, and timing at the end. useful for scripts and CI.

## license

MIT
