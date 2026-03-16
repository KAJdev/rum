use crate::agent::AgentEvent;
use crate::api::{ContentBlock, Message, MessageContent};
use crate::editor::{self, AgentEdit, EditorBuffer, SearchMode, SearchState};
use crate::diff::{DiffInfo, DiffLineTag};
use crate::tools::ToolResult;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame, Terminal,
};
use std::io::{self, Stdout};
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

const BG: Color = Color::Rgb(11, 14, 20);
const FG: Color = Color::Rgb(191, 189, 182);
const MUTED: Color = Color::Rgb(108, 115, 128);
const ACCENT: Color = Color::Rgb(230, 180, 80);
const GREEN: Color = Color::Rgb(170, 217, 76);
const RED: Color = Color::Rgb(240, 113, 120);
const YELLOW: Color = Color::Rgb(255, 180, 84);
const DIM: Color = Color::Rgb(86, 91, 102);
const SURFACE: Color = Color::Rgb(22, 27, 36);
const BAR_COLOR: Color = Color::Rgb(60, 65, 75);

const THINKING_COLOR: Color = Color::Rgb(180, 140, 255);
const TOOL_COLOR: Color = Color::Rgb(100, 200, 220);
const INPUT_BG: Color = Color::Rgb(16, 20, 28);
const BRANCH_COLOR: Color = Color::Rgb(120, 190, 148);
const SIDEBAR_WIDTH: u16 = 30;

const DEFAULT_CONTEXT: u32 = 200_000;

struct SlashDef {
    name: &'static str,
    args: &'static str,
    description: &'static str,
}

const SLASH_COMMANDS: &[SlashDef] = &[
    SlashDef {
        name: "/model",
        args: "[name]",
        description: "Switch model",
    },
    SlashDef {
        name: "/thinking",
        args: "[level]",
        description: "Set thinking level",
    },
    SlashDef {
        name: "/new",
        args: "",
        description: "Start new conversation",
    },
    SlashDef {
        name: "/compact",
        args: "",
        description: "Summarize context to free up space",
    },
    SlashDef {
        name: "/cd",
        args: "<path>",
        description: "Change working directory",
    },
    SlashDef {
        name: "/login",
        args: "",
        description: "Log in with Anthropic OAuth",
    },
    SlashDef {
        name: "/logout",
        args: "",
        description: "Log out",
    },
    SlashDef {
        name: "/help",
        args: "",
        description: "Show available commands",
    },
    SlashDef {
        name: "/quit",
        args: "",
        description: "Quit",
    },
    SlashDef {
        name: "/tree",
        args: "",
        description: "View and branch conversation tree",
    },
];

struct Suggestion {
    display: String,
    description: String,
    // full string placed in the input field when this suggestion is applied
    completion: String,
}

fn slash_suggestions(input: &str) -> Vec<Suggestion> {
    if !input.starts_with('/') {
        return vec![];
    }
    let parts: Vec<&str> = input.splitn(2, ' ').collect();
    let cmd_part = parts[0].to_lowercase();

    if parts.len() == 1 {
        // completing the command name
        return SLASH_COMMANDS
            .iter()
            .filter(|h| h.name.starts_with(cmd_part.as_str()))
            .map(|h| Suggestion {
                display: if h.args.is_empty() {
                    h.name.to_string()
                } else {
                    format!("{} {}", h.name, h.args)
                },
                description: h.description.to_string(),
                // commands with args get a trailing space so the next Tab starts arg completion
                completion: if h.args.is_empty() {
                    h.name.to_string()
                } else {
                    format!("{} ", h.name)
                },
            })
            .collect();
    }

    // completing a command argument
    let arg_partial = parts[1].to_lowercase();
    match cmd_part.as_str() {
        "/model" => crate::config::ANTHROPIC_MODELS
            .iter()
            .filter(|m| {
                m.id.to_lowercase().contains(arg_partial.as_str())
                    || m.name.to_lowercase().contains(arg_partial.as_str())
            })
            .map(|m| Suggestion {
                display: m.id.to_string(),
                description: m.name.to_string(),
                completion: format!("/model {}", m.id),
            })
            .collect(),
        "/thinking" => crate::config::THINKING_LEVELS
            .iter()
            .filter(|l| l.starts_with(arg_partial.as_str()))
            .map(|l| Suggestion {
                display: l.to_string(),
                description: String::new(),
                completion: format!("/thinking {}", l),
            })
            .collect(),
        _ => vec![],
    }
}

// tokens accumulated in a single time bucket, broken down by type
#[derive(Clone, Default)]
pub(crate) struct TokenBucket {
    text: u32,
    thinking: u32,
    tool: u32,
}

impl TokenBucket {
    fn total(&self) -> u64 {
        // input tokens are excluded: they're a usage count reported once
        // per api call, not streaming throughput
        (self.text + self.thinking + self.tool) as u64
    }
}

// the bar prefix string and its display width
const BAR_STR: &str = "\u{2502} ";
const BAR_WIDTH: u16 = 2;

#[derive(Debug, Clone)]
pub(crate) enum ActivityItem {
    // thinking text from the model, shown dim/italic
    Thinking(String),
    // text output from the model, rendered as markdown with bar prefix
    Text(String),
    // user-submitted message, shown with accent color
    UserMessage(String),
    // tool call entry
    Tool(ToolEntry),
    // system/slash-command output
    System(SystemKind, String),
    // /compact lifecycle: animated while running, static when done
    Compact(CompactStatus),
}

#[derive(Debug, Clone)]
pub(crate) enum CompactStatus {
    Running,
    Done(String),
    Cancelled,
}

#[derive(Debug, Clone)]
pub enum SystemKind {
    // muted, informational (default)
    Info,
    // green — positive outcome
    Success,
    // yellow — caution / non-fatal problem
    Warning,
    // red — something went wrong
    Error,
    // accent+bold — new version available, prominent announcements
    Update,
}

#[derive(Debug, Clone)]
pub enum QueuedItem {
    // a regular follow-up message waiting to be sent
    Message(String),
    // a slash command waiting to be dispatched when the current turn finishes
    Command(String),
}

// action returned by drain_next_queued() to tell the caller what to dispatch
#[derive(Debug)]
pub enum QueuedAction {
    // one or more messages combined, ready to send as a user turn
    SendMessage(String),
    // a slash command name to dispatch (e.g. "/compact")
    RunCommand(String),
}

// background job shown in the bottom status bar
#[derive(Debug, Clone)]
pub struct BackgroundJob {
    pub id: u64,
    pub label: String,
    pub detail: String,
    pub status: JobStatus,
    pub started_at: Instant,
    // hidden until there's something meaningful to show
    pub visible: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum JobStatus {
    Running,
    Passed,
    Failed(String),
}

// event sent from background job tasks to update the UI
#[derive(Debug)]
pub enum JobEvent {
    Show {
        id: u64,
    },
    Update {
        id: u64,
        detail: String,
    },
    Complete {
        id: u64,
        status: JobStatus,
        summary: String,
    },
    // silently remove a job without inserting a message
    Dismiss {
        id: u64,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ToolEntry {
    name: String,
    // argument portion (path, command, etc.) shown after the tool label
    arg: String,
    status: ToolStatus,
    diff: Option<DiffInfo>,
    output: Option<String>,
    expanded: bool,
    started_at: Instant,
}

#[derive(Debug, Clone)]
pub(crate) enum ToolStatus {
    Running,
    // exit_code is Some for bash commands
    Complete { exit_code: Option<i32> },
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ViewMode {
    Chat,
    Editor,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DiffMarker {
    Insert,
    // line immediately after a deleted block
    DeleteBoundary,
}

// cached rendered lines for a single activity item.
// invalidated when content length, terminal width, expand state, or
// tool status changes.
#[derive(Clone, Default)]
pub(crate) struct CachedRender {
    lines: Vec<Line<'static>>,
    content_len: usize,
    width: u16,
    expanded: bool,
    status_tag: u8,
}

pub struct InputState {
    pub text: String,
    pub cursor_pos: usize,
    // multi-line or long pastes stored here; the text string holds a single
    // placeholder char (private use area \u{E000}+index) per chunk
    pub paste_chunks: Vec<String>,
    pub history: Vec<String>,
    pub history_pos: Option<usize>,
    pub draft: String,
    // set when PasteFromClipboard already handled an image this tick,
    // so the subsequent Event::Paste("") doesn't duplicate it
    pub paste_handled: bool,
    // slash command tab-completion state
    pub slash_prefix: Option<String>,
    pub slash_selected: Option<usize>,
}

pub struct FeedState {
    pub items: Vec<ActivityItem>,
    // per-item cache of rendered lines
    pub render_cache: Vec<CachedRender>,
    // index where the current login flow started; used to
    // wipe login messages on success so only the result remains
    pub login_activity_start: Option<usize>,
    pub scroll_offset: u16,
    // when true, viewport follows new content to the bottom
    pub auto_scroll: bool,
    // set after TurnComplete so the next text/thinking block always starts fresh
    pub new_turn: bool,
    pub diffs_expanded: bool,
}

pub struct TokenState {
    // summed across all api calls (for cost calculation)
    pub total_input: u32,
    pub total_output: u32,
    pub total_cache_read: u32,
    pub total_cache_creation: u32,
    // from the most recent api call (for context window display).
    // each call's input_tokens already includes the full conversation
    // history, so these reflect actual context window usage.
    pub last_input: u32,
    pub last_output: u32,
    pub context_limit: u32,
    pub rate_samples: Vec<TokenBucket>,
    pub rate_bucket: TokenBucket,
    pub last_sample: Instant,
    pub start_time: Option<Instant>,
}

pub struct EditorViewState {
    pub mode: ViewMode,
    pub buffer: Option<EditorBuffer>,
    pub search: Option<SearchState>,
    pub autocomplete: Option<crate::autocomplete::AutocompleteState>,
    pub follow_mode: bool,
    pub agent_edits: Vec<AgentEdit>,
    pub agent_edit_index: usize,
    // maps line number (0-indexed) to diff tag for follow mode highlighting
    pub diff_markers: std::collections::HashMap<usize, DiffMarker>,
    pub highlighter: Option<editor::Highlighter>,
}

pub struct LspState {
    pub manager: Option<std::sync::Arc<tokio::sync::Mutex<crate::lsp::LspManager>>>,
    // diagnostics for the currently open file, cached from LspManager
    pub diagnostics: Vec<crate::lsp::DiagnosticInfo>,
    // queued LSP notifications (processed async in main loop)
    pub pending: Vec<LspNotify>,
    // timestamp to check for diagnostics after agent turn completes
    pub diag_check_at: Option<std::time::Instant>,
    // pending completion request (path, line, character)
    pub completion_request: Option<(std::path::PathBuf, u32, u32)>,
    // pending goto-definition request (path, line, character)
    pub goto_request: Option<(std::path::PathBuf, u32, u32)>,
}

pub struct App {
    pub input: InputState,
    pub feed: FeedState,
    pub tokens: TokenState,
    pub editor: EditorViewState,
    pub lsp: LspState,
    pub current_message: Option<String>,
    pub queued_messages: Vec<QueuedItem>,
    pub is_running: bool,
    pub should_quit: bool,
    pub model_name: String,
    pub thinking_level: String,
    pub cwd: String,
    pub git_branch: Option<String>,
    // accumulated tool input json for the current streaming tool call
    current_tool_input: String,
    // cached terminal width for manual line wrapping
    pub term_width: u16,
    // animation frame counter, incremented every render tick
    pub spin_frame: u64,
    // channel for injecting queued messages into a running turn
    pub inject_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    // background jobs shown in the bottom status bar
    pub background_jobs: Vec<BackgroundJob>,
    pub next_job_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    // set when a git push is detected; main.rs reads and clears this to spawn CI watch
    pub pending_ci_watch: Option<String>,
    // session tree for conversation branching
    pub session_tree: crate::persistence::SessionTree,
    pub tree_view: Option<crate::tree::TreeView>,
}

#[derive(Debug)]
pub enum LspNotify {
    Open(std::path::PathBuf),
    Change(std::path::PathBuf, String),
    Save(std::path::PathBuf),
}

impl App {
    pub fn new(model_name: &str, thinking_level: &str, cwd: &str) -> Self {
        let context_limit = guess_context_limit(model_name);
        let term_width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
        Self {
            input: InputState {
                text: String::new(),
                cursor_pos: 0,
                paste_chunks: Vec::new(),
                history: Vec::new(),
                history_pos: None,
                draft: String::new(),
                paste_handled: false,
                slash_prefix: None,
                slash_selected: None,
            },
            feed: FeedState {
                items: Vec::new(),
                render_cache: Vec::new(),
                login_activity_start: None,
                scroll_offset: 0,
                auto_scroll: true,
                new_turn: false,
                diffs_expanded: true,
            },
            tokens: TokenState {
                total_input: 0,
                total_output: 0,
                total_cache_read: 0,
                total_cache_creation: 0,
                last_input: 0,
                last_output: 0,
                context_limit,
                rate_samples: Vec::new(),
                rate_bucket: TokenBucket::default(),
                last_sample: Instant::now(),
                start_time: None,
            },
            editor: EditorViewState {
                mode: ViewMode::Chat,
                buffer: None,
                search: None,
                autocomplete: None,
                follow_mode: false,
                agent_edits: Vec::new(),
                agent_edit_index: 0,
                diff_markers: std::collections::HashMap::new(),
                highlighter: None,
            },
            lsp: LspState {
                manager: None,
                diagnostics: Vec::new(),
                pending: Vec::new(),
                diag_check_at: None,
                completion_request: None,
                goto_request: None,
            },
            current_message: None,
            queued_messages: Vec::new(),
            is_running: false,
            should_quit: false,
            model_name: model_name.to_string(),
            thinking_level: thinking_level.to_string(),
            cwd: cwd.to_string(),
            git_branch: detect_git_branch(cwd),
            current_tool_input: String::new(),
            term_width,
            spin_frame: 0,
            inject_tx: None,
            background_jobs: Vec::new(),
            next_job_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pending_ci_watch: None,
            session_tree: crate::persistence::SessionTree::new(),
            tree_view: None,
        }
    }

    pub fn set_inject_tx(&mut self, tx: tokio::sync::mpsc::UnboundedSender<String>) {
        self.inject_tx = Some(tx);
    }

    // fire an async LSP completion request for the current cursor position
    pub fn request_lsp_completion(&mut self) {
        if self.lsp.manager.is_some() {
            if let Some(ref buf) = self.editor.buffer {
                self.lsp.completion_request = Some((
                    buf.path.clone(),
                    buf.cursor_row as u32,
                    buf.cursor_col as u32,
                ));
            }
        }
    }

    pub fn tick_rate(&mut self) {
        self.spin_frame = self.spin_frame.wrapping_add(1);
        let now = Instant::now();
        if now.duration_since(self.tokens.last_sample).as_millis() >= 2000 {
            self.tokens.rate_samples.push(self.tokens.rate_bucket.clone());
            self.tokens.rate_bucket = TokenBucket::default();
            self.tokens.last_sample = now;
            if self.tokens.rate_samples.len() > 120 {
                self.tokens.rate_samples.remove(0);
            }
        }
        // refresh terminal width and git branch periodically
        if let Ok((w, _)) = crossterm::terminal::size() {
            self.term_width = w;
        }
        if self.spin_frame % 60 == 0 {
            self.git_branch = detect_git_branch(&self.cwd);
        }
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Thinking(t) => {
                let approx = (t.len() as u32 / 4).max(1);
                self.tokens.rate_bucket.thinking += approx;
                if !self.feed.new_turn {
                    if let Some(ActivityItem::Thinking(ref mut s)) = self.feed.items.last_mut() {
                        s.push_str(&t);
                        return;
                    }
                }
                self.feed.new_turn = false;
                self.feed.items.push(ActivityItem::Thinking(t));
            }
            AgentEvent::Text(t) => {
                let approx = (t.len() as u32 / 4).max(1);
                self.tokens.rate_bucket.text += approx;
                if !self.feed.new_turn {
                    if let Some(ActivityItem::Text(ref mut s)) = self.feed.items.last_mut() {
                        s.push_str(&t);
                        return;
                    }
                }
                self.feed.new_turn = false;
                self.feed.items.push(ActivityItem::Text(t));
            }
            AgentEvent::ToolStart { id: _, name } => {
                self.current_tool_input.clear();
                self.feed.items.push(ActivityItem::Tool(ToolEntry {
                    name: name.clone(),
                    arg: String::new(),
                    status: ToolStatus::Running,
                    diff: None,
                    output: None,
                    expanded: self.feed.diffs_expanded,
                    started_at: Instant::now(),
                }));
            }
            AgentEvent::ToolInputDelta(json) => {
                let approx = (json.len() as u32 / 4).max(1);
                self.tokens.rate_bucket.tool += approx;
                self.current_tool_input.push_str(&json);
                if let Some(ActivityItem::Tool(ref mut entry)) = self.feed.items.last_mut() {
                    if let Ok(partial) =
                        serde_json::from_str::<serde_json::Value>(&self.current_tool_input)
                    {
                        entry.arg = extract_tool_arg(&entry.name, &partial);
                    }
                }
            }
            AgentEvent::ToolOutputDelta { id: _, text } => {
                if let Some(ActivityItem::Tool(ref mut entry)) = self.feed.items.iter_mut().rev()
                    .find(|item| matches!(item, ActivityItem::Tool(e) if matches!(e.status, ToolStatus::Running)))
                {
                    let buf = entry.output.get_or_insert_with(String::new);
                    // cap the display buffer to keep re-renders cheap
                    if buf.len() < 10_000 {
                        buf.push_str(&strip_ansi(&text));
                    }
                }
            }
            AgentEvent::ToolComplete {
                id: _,
                name,
                result,
            } => {
                let mut tracked: Option<(String, Option<DiffInfo>, Option<usize>)> = None;
                if let Some(ActivityItem::Tool(ref mut entry)) = self.feed.items.iter_mut().rev()
                    .find(|item| matches!(item, ActivityItem::Tool(e) if matches!(e.status, ToolStatus::Running)))
                {
                    match &result {
                        ToolResult::Success { output, diff, read } => {
                            let exit_code = if name == "bash" {
                                parse_exit_code(output)
                            } else {
                                None
                            };

                            if let Some(d) = diff {
                                entry.arg = d.path.clone();
                                entry.diff = Some(d.clone());
                                tracked = Some((d.path.clone(), Some(d.clone()), None));
                            } else if let Some(r) = read {
                                tracked = Some((r.path.clone(), None, Some(r.offset)));
                            }

                            entry.expanded = self.feed.diffs_expanded;

                            // store output for display, truncated.
                            // skip when a diff is present (path+stats in the header is enough).
                            // for bash, streaming already built entry.output via ToolOutputDelta so
                            // we leave it alone. for all other tools (including explore) we always
                            // overwrite with the final result, replacing any live progress lines.
                            let trimmed = output.trim();
                            let keep_streamed = name == "bash" && entry.output.is_some();
                            if !keep_streamed && entry.diff.is_none() && !trimmed.is_empty() && trimmed != "(no output)" {
                                let clean = strip_ansi(trimmed);
                                let display_output = if clean.len() > 2000 {
                                    format!("{}...", &clean[..2000])
                                } else {
                                    clean
                                };
                                entry.output = Some(display_output);
                            }

                            entry.status = ToolStatus::Complete { exit_code };

                            // detect git push to trigger CI watch
                            if name == "bash" && exit_code == Some(0) {
                                let cmd = entry.arg.to_lowercase();
                                if cmd.contains("git push") {
                                    self.pending_ci_watch = Some(self.cwd.clone());
                                }
                            }
                        }
                        ToolResult::Error(e) => {
                            entry.status = ToolStatus::Error(e.clone());
                        }
                        ToolResult::Image { text, .. } => {
                            entry.output = Some(text.clone());
                            entry.status = ToolStatus::Complete { exit_code: None };
                        }
                    }
                }
                if let Some((path, diff, line)) = tracked {
                    self.track_agent_edit(path, diff, line);
                }
            }
            AgentEvent::TokenUsage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } => {
                self.tokens.total_input += input_tokens;
                self.tokens.total_output += output_tokens;
                self.tokens.total_cache_read += cache_read_tokens;
                self.tokens.total_cache_creation += cache_creation_tokens;
                // context meter: update each field only when nonzero so
                // incremental emissions (input first, output later) don't
                // reset each other
                let input_total = input_tokens + cache_read_tokens + cache_creation_tokens;
                if input_total > 0 {
                    self.tokens.last_input = input_total;
                }
                if output_tokens > 0 {
                    self.tokens.last_output = output_tokens;
                }
            }
            AgentEvent::TurnComplete => {
                self.is_running = false;
                self.feed.new_turn = true;
                // BEL character triggers terminal/OS notification
                print!("\x07");
                // schedule LSP diagnostic check after a short delay
                self.lsp.diag_check_at = Some(
                    std::time::Instant::now() + std::time::Duration::from_secs(3),
                );
                // cancel any in-progress compact animation
                for item in self.feed.items.iter_mut().rev() {
                    match item {
                        ActivityItem::Compact(CompactStatus::Running) => {
                            *item = ActivityItem::Compact(CompactStatus::Cancelled);
                            break;
                        }
                        _ => {}
                    }
                }
            }
            AgentEvent::UserMessage(msg) => {
                // the agent consumed queued messages at a tool break;
                // remove them from the queue display
                self.queued_messages
                    .retain(|q| !matches!(q, QueuedItem::Message(_)));
                self.feed.items.push(ActivityItem::UserMessage(msg.clone()));
                if let Some(ref mut current) = self.current_message {
                    current.push_str(&format!("\n{}", msg));
                }
                self.feed.auto_scroll = true;
            }
            AgentEvent::Status(msg) => {
                self.feed.items
                    .push(ActivityItem::System(SystemKind::Info, msg));
            }
            AgentEvent::Error(e) => {
                self.is_running = false;
                self.feed.items
                    .push(ActivityItem::Text(format!("[error] {e}")));
            }
            AgentEvent::CompactStart => {
                self.feed.items
                    .push(ActivityItem::Compact(CompactStatus::Running));
                self.feed.auto_scroll = true;
            }
            AgentEvent::CompactDone(msg) => {
                if let Some(ActivityItem::Compact(ref mut s)) = self
                    .feed.items
                    .iter_mut()
                    .rev()
                    .find(|i| matches!(i, ActivityItem::Compact(_)))
                {
                    *s = CompactStatus::Done(msg);
                }
            }
            AgentEvent::MessagesUpdated(messages) => {
                // sync the active branch in the session tree
                self.session_tree.branches[self.session_tree.active].messages = messages;
                let _ = crate::persistence::save_session(
                    std::path::Path::new(self.cwd()),
                    &self.session_tree,
                );
            }
        }
    }

    pub fn start_new_message(&mut self, message: &str) {
        self.feed.items
            .push(ActivityItem::UserMessage(message.to_string()));
        self.current_message = Some(message.to_string());
        self.is_running = true;
        self.feed.auto_scroll = true;
        self.current_tool_input.clear();
        if self.tokens.start_time.is_none() {
            self.tokens.start_time = Some(Instant::now());
        }
    }

    // immediately reflect cancellation in the UI without waiting for TurnComplete
    pub fn cancel_running(&mut self) {
        self.is_running = false;
        self.current_message = None;
        for item in self.feed.items.iter_mut().rev() {
            match item {
                ActivityItem::Compact(CompactStatus::Running) => {
                    *item = ActivityItem::Compact(CompactStatus::Cancelled);
                    break;
                }
                ActivityItem::Tool(ref mut e) if matches!(e.status, ToolStatus::Running) => {
                    e.status = ToolStatus::Error("cancelled".to_string());
                }
                _ => {}
            }
        }
    }

    // queue a followup message while the agent is running.
    // appears in the queued messages area; sent to the agent's inject
    // channel so it can be picked up at the next tool break.
    pub fn queue_message(&mut self) {
        if !self.input.text.is_empty() {
            let msg = self.expand_input();
            if let Some(ref tx) = self.inject_tx {
                let _ = tx.send(msg.clone());
            }
            self.queued_messages.push(QueuedItem::Message(msg));
            self.input.text.clear();
            self.input.cursor_pos = 0;
            self.input.paste_chunks.clear();
        }
    }

    // queue a slash command to be dispatched when the current turn finishes
    pub fn queue_command(&mut self, cmd: &str) {
        self.queued_messages
            .push(QueuedItem::Command(cmd.to_string()));
    }

    // queue an explicit message string (used by background jobs like CI watch)
    pub fn queue_message_str(&mut self, msg: String) {
        if let Some(ref tx) = self.inject_tx {
            let _ = tx.send(msg.clone());
        }
        self.queued_messages.push(QueuedItem::Message(msg));
    }

    // pop the next queued item for dispatch. commands are returned individually;
    // consecutive messages are combined into a single send.
    pub fn drain_next_queued(&mut self) -> Option<QueuedAction> {
        if self.queued_messages.is_empty() {
            return None;
        }
        match &self.queued_messages[0] {
            QueuedItem::Command(_) => {
                if let QueuedItem::Command(cmd) = self.queued_messages.remove(0) {
                    Some(QueuedAction::RunCommand(cmd))
                } else {
                    None
                }
            }
            QueuedItem::Message(_) => {
                let mut msgs = Vec::new();
                while !self.queued_messages.is_empty() {
                    if matches!(&self.queued_messages[0], QueuedItem::Message(_)) {
                        if let QueuedItem::Message(m) = self.queued_messages.remove(0) {
                            msgs.push(m);
                        }
                    } else {
                        break;
                    }
                }
                let combined = msgs.join("\n\n");
                self.current_message = Some(combined.clone());
                self.is_running = true;
                self.feed.auto_scroll = true;
                self.current_tool_input.clear();
                Some(QueuedAction::SendMessage(combined))
            }
        }
    }

    pub fn has_queued_items(&self) -> bool {
        !self.queued_messages.is_empty()
    }

    pub fn clear_queue(&mut self) {
        self.queued_messages.clear();
    }

    pub fn reset_slash_completion(&mut self) {
        self.input.slash_prefix = None;
        self.input.slash_selected = None;
    }

    // pop the last queued message back into the input for editing
    pub fn pop_queued_message(&mut self) -> bool {
        // find the last Message item (skip over any queued commands)
        let pos = self
            .queued_messages
            .iter()
            .rposition(|i| matches!(i, QueuedItem::Message(_)));
        if let Some(idx) = pos {
            if let QueuedItem::Message(msg) = self.queued_messages.remove(idx) {
                self.input.text = msg;
                self.input.cursor_pos = self.char_count();
                return true;
            }
        }
        false
    }

    pub fn toggle_diff(&mut self) {
        self.feed.diffs_expanded = !self.feed.diffs_expanded;
        for item in &mut self.feed.items {
            if let ActivityItem::Tool(ref mut entry) = item {
                entry.expanded = self.feed.diffs_expanded;
            }
        }
    }

    pub fn push_user_message(&mut self, msg: &str) {
        self.feed.items
            .push(ActivityItem::UserMessage(msg.to_string()));
        self.feed.auto_scroll = true;
    }

    pub fn push_system_message(&mut self, msg: String) {
        self.feed.items
            .push(ActivityItem::System(SystemKind::Info, msg));
        self.feed.auto_scroll = true;
    }

    pub fn push_success(&mut self, msg: String) {
        self.feed.items
            .push(ActivityItem::System(SystemKind::Success, msg));
        self.feed.auto_scroll = true;
    }

    pub fn push_warning(&mut self, msg: String) {
        self.feed.items
            .push(ActivityItem::System(SystemKind::Warning, msg));
        self.feed.auto_scroll = true;
    }

    pub fn push_error_msg(&mut self, msg: String) {
        self.feed.items
            .push(ActivityItem::System(SystemKind::Error, msg));
        self.feed.auto_scroll = true;
    }

    pub fn push_update_notice(&mut self, msg: String) {
        self.feed.items
            .push(ActivityItem::System(SystemKind::Update, msg));
        self.feed.auto_scroll = true;
    }

    pub fn start_background_job(&mut self, label: String, detail: String) -> u64 {
        let id = self.next_job_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.background_jobs.push(BackgroundJob {
            id,
            label,
            detail,
            status: JobStatus::Running,
            started_at: Instant::now(),
            visible: false,
        });
        id
    }

    pub fn handle_job_event(&mut self, event: JobEvent) {
        match event {
            JobEvent::Show { id } => {
                if let Some(job) = self.background_jobs.iter_mut().find(|j| j.id == id) {
                    job.visible = true;
                } else {
                    // job created externally (e.g. background bash tool)
                    self.background_jobs.push(BackgroundJob {
                        id,
                        label: "bash".to_string(),
                        detail: String::new(),
                        status: JobStatus::Running,
                        started_at: Instant::now(),
                        visible: true,
                    });
                }
            }
            JobEvent::Update { id, detail } => {
                if let Some(job) = self.background_jobs.iter_mut().find(|j| j.id == id) {
                    job.detail = detail;
                }
            }
            JobEvent::Complete {
                id,
                status,
                summary,
            } => {
                if let Some(job) = self.background_jobs.iter_mut().find(|j| j.id == id) {
                    job.status = status.clone();
                    job.detail = summary.clone();
                }
                let kind = match &status {
                    JobStatus::Passed => SystemKind::Success,
                    JobStatus::Failed(_) => SystemKind::Error,
                    JobStatus::Running => SystemKind::Info,
                };
                self.feed.items.push(ActivityItem::System(kind, summary));
                self.feed.auto_scroll = true;
            }
            JobEvent::Dismiss { id } => {
                self.background_jobs.retain(|j| j.id != id);
            }
        }
    }

    // remove completed background jobs older than the given duration
    pub fn gc_background_jobs(&mut self, max_age: std::time::Duration) {
        self.background_jobs
            .retain(|j| matches!(j.status, JobStatus::Running) || j.started_at.elapsed() < max_age);
    }

    // reconstruct the activity feed and input history from persisted messages
    pub fn hydrate_from_history(&mut self, messages: &[Message]) {
        use std::collections::HashMap;
        // tool_use_id -> index in self.feed.items for matching results
        let mut tool_map: HashMap<String, usize> = HashMap::new();

        for msg in messages {
            match (&msg.role.as_str(), &msg.content) {
                (&"user", MessageContent::Text(s)) => {
                    if !s.trim().is_empty() {
                        self.feed.items.push(ActivityItem::UserMessage(s.clone()));
                        self.push_history(s);
                    }
                }
                (&"user", MessageContent::Blocks(blocks)) => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text } => {
                                if !text.trim().is_empty() {
                                    self.feed.items.push(ActivityItem::UserMessage(text.clone()));
                                    self.push_history(text);
                                }
                            }
                            ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                            } => {
                                if let Some(&idx) = tool_map.get(tool_use_id) {
                                    if let ActivityItem::Tool(ref mut entry) = self.feed.items[idx] {
                                        let output = tool_result_display_text(content);
                                        if is_error == &Some(true) {
                                            entry.status = ToolStatus::Error(output);
                                        } else {
                                            let exit_code = if entry.name == "bash" {
                                                parse_exit_code(&output)
                                            } else {
                                                None
                                            };
                                            let trimmed = output.trim();
                                            if !trimmed.is_empty()
                                                && trimmed != "(no output)"
                                                && entry.diff.is_none()
                                            {
                                                let display = if trimmed.len() > 2000 {
                                                    format!("{}...", &trimmed[..2000])
                                                } else {
                                                    trimmed.to_string()
                                                };
                                                entry.output = Some(display);
                                            }
                                            entry.status = ToolStatus::Complete { exit_code };
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                (&"assistant", MessageContent::Text(s)) => {
                    if !s.trim().is_empty() {
                        self.feed.items.push(ActivityItem::Text(s.clone()));
                    }
                }
                (&"assistant", MessageContent::Blocks(blocks)) => {
                    for block in blocks {
                        match block {
                            ContentBlock::Thinking { thinking, .. } => {
                                if !thinking.trim().is_empty() {
                                    self.feed.items.push(ActivityItem::Thinking(thinking.clone()));
                                }
                            }
                            ContentBlock::Text { text } => {
                                if !text.trim().is_empty() {
                                    self.feed.items.push(ActivityItem::Text(text.clone()));
                                }
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                let display_name = crate::agent::from_cc_name(name).to_string();
                                let arg = extract_tool_arg(&display_name, input);
                                let idx = self.feed.items.len();
                                self.feed.items.push(ActivityItem::Tool(ToolEntry {
                                    name: display_name,
                                    arg,
                                    status: ToolStatus::Complete { exit_code: None },
                                    diff: None,
                                    output: None,
                                    expanded: self.feed.diffs_expanded,
                                    started_at: Instant::now(),
                                }));
                                tool_map.insert(id.clone(), idx);
                            }
                            ContentBlock::Compaction { .. } => {
                                // server-side compaction markers are internal
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // call just before showing login instructions; records the current
    // activity length so clear_login_activity can wipe everything since
    pub fn mark_login_start(&mut self) {
        self.feed.login_activity_start = Some(self.feed.items.len());
    }

    // truncate all activity added since mark_login_start, then push a
    // single clean result message in its place
    pub fn finish_login(&mut self, msg: String, success: bool) {
        if let Some(start) = self.feed.login_activity_start.take() {
            self.feed.items.truncate(start);
        }
        if success {
            self.push_success(msg);
        } else {
            self.push_error_msg(msg);
        }
    }

    pub fn update_model(&mut self, model_id: &str) {
        self.model_name = model_id.to_string();
        self.tokens.context_limit = guess_context_limit(model_id);
    }

    pub fn update_thinking(&mut self, level: &str) {
        self.thinking_level = level.to_string();
    }

    pub fn update_cwd(&mut self, cwd: &str) {
        self.cwd = cwd.to_string();
        self.git_branch = detect_git_branch(cwd);
    }

    // push a submitted message into the input history, avoiding adjacent duplicates.
    // resets history navigation state.
    pub fn push_history(&mut self, msg: &str) {
        if !msg.trim().is_empty() {
            if self.input.history.last().map(|s| s.as_str()) != Some(msg) {
                self.input.history.push(msg.to_string());
                if self.input.history.len() > 1000 {
                    self.input.history.remove(0);
                }
            }
        }
        self.input.history_pos = None;
        self.input.draft = String::new();
    }

    // navigate to the previous (older) history entry. returns true if handled
    // (so the caller knows not to fall through to scroll).
    pub fn navigate_history_up(&mut self) -> bool {
        if self.input.history.is_empty() {
            return false;
        }
        match self.input.history_pos {
            None => {
                self.input.draft = self.expand_input();
                let pos = self.input.history.len() - 1;
                self.input.history_pos = Some(pos);
                self.input.text = self.input.history[pos].clone();
                self.input.paste_chunks.clear();
                self.input.cursor_pos = self.char_count();
                true
            }
            Some(0) => true, // already at oldest entry, absorb the keypress
            Some(p) => {
                let new_pos = p - 1;
                self.input.history_pos = Some(new_pos);
                self.input.text = self.input.history[new_pos].clone();
                self.input.cursor_pos = self.char_count();
                true
            }
        }
    }

    // navigate to the next (newer) history entry, or back to the saved draft.
    // returns false when not in history mode so the caller can scroll instead.
    pub fn navigate_history_down(&mut self) -> bool {
        match self.input.history_pos {
            None => false,
            Some(p) if p + 1 >= self.input.history.len() => {
                self.input.text = self.input.draft.clone();
                self.input.paste_chunks.clear();
                self.input.cursor_pos = self.char_count();
                self.input.history_pos = None;
                true
            }
            Some(p) => {
                let new_pos = p + 1;
                self.input.history_pos = Some(new_pos);
                self.input.text = self.input.history[new_pos].clone();
                self.input.cursor_pos = self.char_count();
                true
            }
        }
    }

    pub fn reset_session(&mut self) {
        self.feed.items.clear();
        self.feed.render_cache.clear();
        self.tokens.total_input = 0;
        self.tokens.total_output = 0;
        self.tokens.total_cache_read = 0;
        self.tokens.total_cache_creation = 0;
        self.tokens.last_input = 0;
        self.tokens.last_output = 0;
        self.current_message = None;
        self.queued_messages.clear();
        self.tokens.rate_samples.clear();
        self.tokens.rate_bucket = TokenBucket::default();
        self.tokens.start_time = None;
        self.feed.scroll_offset = 0;
        self.feed.auto_scroll = true;
        self.feed.new_turn = false;
        self.input.paste_chunks.clear();
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    pub fn thinking_level(&self) -> &str {
        &self.thinking_level
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    fn context_used(&self) -> u32 {
        self.tokens.last_input + self.tokens.last_output
    }

    fn context_pct(&self) -> f64 {
        if self.tokens.context_limit == 0 {
            return 0.0;
        }
        (self.context_used() as f64 / self.tokens.context_limit as f64).min(1.0)
    }

    fn cost_usd(&self) -> f64 {
        let p = crate::config::model_pricing(&self.model_name);
        // cache writes cost 1.25x, cache reads cost 0.1x base input price
        self.tokens.total_input as f64 * p.input / 1_000_000.0
            + self.tokens.total_cache_creation as f64 * p.input * 1.25 / 1_000_000.0
            + self.tokens.total_cache_read as f64 * p.input * 0.1 / 1_000_000.0
            + self.tokens.total_output as f64 * p.output / 1_000_000.0
    }

    fn avg_rate(&self) -> f64 {
        let elapsed = self.tokens.start_time.map_or(0.0, |t| t.elapsed().as_secs_f64());
        if elapsed > 0.0 {
            self.tokens.total_output as f64 / elapsed
        } else {
            0.0
        }
    }

    // cursor_pos is a char-count offset. convert to byte index for
    // String insert/remove operations.
    fn cursor_byte_pos(&self) -> usize {
        self.input.text
            .char_indices()
            .nth(self.input.cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(self.input.text.len())
    }

    // insert text directly at the cursor position without any collapsing
    pub fn insert_text(&mut self, text: String) {
        let bp = self.cursor_byte_pos();
        self.input.text.insert_str(bp, &text);
        self.input.cursor_pos += text.chars().count();
    }

    // insert pasted text. multi-line or long pastes are collapsed to a single
    // placeholder char in the input string; the real content lives in paste_chunks.
    // on submit, expand_input() restores the full text.
    // very large pastes (> 50 lines or > 2000 chars) are written to a temp file
    // and the path is inserted instead, so the model can read them with the read tool.
    pub fn insert_paste(&mut self, text: String) {
        let multiline = text.contains('\n');
        let long = text.len() > 80;
        if !multiline && !long {
            let bp = self.cursor_byte_pos();
            self.input.text.insert_str(bp, &text);
            self.input.cursor_pos += text.chars().count();
            return;
        }

        let idx = self.input.paste_chunks.len();
        if idx > 15 {
            let bp = self.cursor_byte_pos();
            self.input.text.insert_str(bp, &text);
            self.input.cursor_pos += text.chars().count();
            return;
        }

        self.input.paste_chunks.push(text);
        let placeholder = char::from_u32(0xE000 + idx as u32).unwrap();
        let bp = self.cursor_byte_pos();
        self.input.text.insert(bp, placeholder);
        self.input.cursor_pos += 1;
    }

    // replace all paste placeholders with their real content
    pub fn expand_input(&self) -> String {
        let mut out = String::new();
        for c in self.input.text.chars() {
            if is_paste_placeholder(c) {
                let idx = paste_placeholder_index(c);
                if idx < self.input.paste_chunks.len() {
                    out.push_str(&self.input.paste_chunks[idx]);
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn char_count(&self) -> usize {
        self.input.text.chars().count()
    }

    // (line_number, column_in_chars) from cursor_pos
    fn cursor_line_col(&self) -> (usize, usize) {
        let text_before: String = self.input.text.chars().take(self.input.cursor_pos).collect();
        let line = text_before.matches('\n').count();
        let col = match text_before.rfind('\n') {
            Some(i) => text_before[i + 1..].chars().count(),
            None => text_before.chars().count(),
        };
        (line, col)
    }

    fn input_line_count(&self) -> usize {
        self.input.text.split('\n').count()
    }

    // visual line count after soft-wrapping to terminal width
    fn input_visual_line_count(&self) -> usize {
        let content_width = (self.term_width as usize).saturating_sub(2); // prefix width
        if content_width == 0 {
            return 1;
        }
        let display = make_display_input(&self.input.text, &self.input.paste_chunks);
        let mut count = 0;
        for line in display.split('\n') {
            let w = UnicodeWidthStr::width(line);
            if w == 0 {
                count += 1;
            } else {
                count += (w + content_width - 1) / content_width;
            }
        }
        count.max(1)
    }

    fn delete_to_line_start(&mut self) {
        let (_, col) = self.cursor_line_col();
        if col == 0 {
            if self.input.cursor_pos > 0 {
                let bp = self.cursor_byte_pos();
                let prev = self.input.text[..bp].char_indices().last().map(|(i, _)| i);
                if let Some(pb) = prev {
                    self.input.text.remove(pb);
                    self.input.cursor_pos -= 1;
                }
            }
        } else {
            let bp = self.cursor_byte_pos();
            let start = bp
                - self.input.text[..bp]
                    .chars()
                    .rev()
                    .take(col)
                    .map(|c| c.len_utf8())
                    .sum::<usize>();
            self.input.text.replace_range(start..bp, "");
            self.input.cursor_pos -= col;
        }
    }

    fn delete_to_line_end(&mut self) {
        let bp = self.cursor_byte_pos();
        let end = self.input.text[bp..]
            .find('\n')
            .map(|i| bp + i)
            .unwrap_or(self.input.text.len());
        self.input.text.replace_range(bp..end, "");
    }

    fn delete_word_backward(&mut self) {
        if self.input.cursor_pos == 0 {
            return;
        }
        let chars: Vec<char> = self.input.text.chars().collect();
        let mut new_pos = self.input.cursor_pos;
        while new_pos > 0 && chars[new_pos - 1].is_whitespace() {
            new_pos -= 1;
        }
        while new_pos > 0 && !chars[new_pos - 1].is_whitespace() {
            new_pos -= 1;
        }
        let byte_start = self
            .input.text
            .char_indices()
            .nth(new_pos)
            .map(|(i, _)| i)
            .unwrap_or(0);
        let byte_end = self.cursor_byte_pos();
        self.input.text.replace_range(byte_start..byte_end, "");
        self.input.cursor_pos = new_pos;
    }

    fn move_word_left(&mut self) {
        if self.input.cursor_pos == 0 {
            return;
        }
        let chars: Vec<char> = self.input.text.chars().collect();
        let mut pos = self.input.cursor_pos;
        while pos > 0 && chars[pos - 1].is_whitespace() {
            pos -= 1;
        }
        while pos > 0 && !chars[pos - 1].is_whitespace() {
            pos -= 1;
        }
        self.input.cursor_pos = pos;
    }

    fn move_word_right(&mut self) {
        let chars: Vec<char> = self.input.text.chars().collect();
        let len = chars.len();
        let mut pos = self.input.cursor_pos;
        while pos < len && !chars[pos].is_whitespace() {
            pos += 1;
        }
        while pos < len && chars[pos].is_whitespace() {
            pos += 1;
        }
        self.input.cursor_pos = pos;
    }

    fn move_line_start(&mut self) {
        let (_, col) = self.cursor_line_col();
        self.input.cursor_pos -= col;
    }

    fn move_line_end(&mut self) {
        let chars: Vec<char> = self.input.text.chars().collect();
        let mut pos = self.input.cursor_pos;
        while pos < chars.len() && chars[pos] != '\n' {
            pos += 1;
        }
        self.input.cursor_pos = pos;
    }

    // returns false if already on the first line
    fn move_cursor_up(&mut self) -> bool {
        let (line, col) = self.cursor_line_col();
        if line == 0 {
            return false;
        }
        let lines: Vec<&str> = self.input.text.split('\n').collect();
        let prev_len = lines[line - 1].chars().count();
        let new_col = col.min(prev_len);
        let mut new_pos = 0;
        for i in 0..line - 1 {
            new_pos += lines[i].chars().count() + 1;
        }
        new_pos += new_col;
        self.input.cursor_pos = new_pos;
        true
    }

    // returns false if already on the last line
    fn move_cursor_down(&mut self) -> bool {
        let (line, col) = self.cursor_line_col();
        let lines: Vec<&str> = self.input.text.split('\n').collect();
        if line >= lines.len() - 1 {
            return false;
        }
        let next_len = lines[line + 1].chars().count();
        let new_col = col.min(next_len);
        let mut new_pos = 0;
        for i in 0..=line {
            new_pos += lines[i].chars().count() + 1;
        }
        new_pos += new_col;
        self.input.cursor_pos = new_pos;
        true
    }
}

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// paste placeholder helpers — private use area \u{E000}..\u{E00F}
fn is_paste_placeholder(c: char) -> bool {
    (c as u32) >= 0xE000 && (c as u32) <= 0xE00F
}

fn paste_placeholder_index(c: char) -> usize {
    ((c as u32) - 0xE000) as usize
}

fn paste_display_str(chunk: &str) -> String {
    let lines = chunk.lines().count().max(1);
    if lines > 1 {
        format!("[{lines} lines]")
    } else {
        format!("[{} chars]", chunk.chars().count())
    }
}

// write paste content to a uniquely named temp file and return its path
// expand paste placeholders to their display summaries (e.g. "[3 lines]")
fn make_display_input(input: &str, paste_chunks: &[String]) -> String {
    let mut out = String::new();
    for c in input.chars() {
        if is_paste_placeholder(c) {
            let idx = paste_placeholder_index(c);
            if idx < paste_chunks.len() {
                out.push_str(&paste_display_str(&paste_chunks[idx]));
            }
        } else {
            out.push(c);
        }
    }
    out
}

// map a char-index in the real input to the equivalent char-index in the display string
fn remap_cursor(input: &str, paste_chunks: &[String], real_pos: usize) -> usize {
    let mut display_pos = 0;
    for (i, c) in input.chars().enumerate() {
        if i == real_pos {
            return display_pos;
        }
        if is_paste_placeholder(c) {
            let idx = paste_placeholder_index(c);
            if idx < paste_chunks.len() {
                display_pos += paste_display_str(&paste_chunks[idx]).chars().count();
            }
        } else {
            display_pos += 1;
        }
    }
    display_pos
}

// ~60fps ticks, slow down to ~8 transitions/sec
fn spinner_char(frame: u64) -> &'static str {
    let idx = (frame / 8) as usize % SPINNER_FRAMES.len();
    SPINNER_FRAMES[idx]
}

fn detect_git_branch(cwd: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", cwd, "symbolic-ref", "--short", "HEAD"])
        .output()
        .ok()?;
    if out.status.success() {
        let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if branch.is_empty() {
            None
        } else {
            Some(branch)
        }
    } else {
        None
    }
}

fn guess_context_limit(model: &str) -> u32 {
    if let Some(def) = crate::config::ANTHROPIC_MODELS
        .iter()
        .find(|m| m.id == model)
    {
        return def.context_window;
    }
    DEFAULT_CONTEXT
}

fn capitalize_tool(name: &str) -> &str {
    match name {
        "read" => "Read",
        "edit" => "Edit",
        "write" => "Write",
        "bash" => "Bash",
        "web_search" => "Search",
        "explore" => "Explore",
        _ => name,
    }
}

// extract the primary argument (path or command) from streaming tool input
fn extract_tool_arg(name: &str, input: &serde_json::Value) -> String {
    match name {
        "read" | "edit" | "write" => input
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("...")
            .to_string(),
        "bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("...")
            .to_string(),
        "web_search" => input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("...")
            .to_string(),
        "explore" => input
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("...")
            .to_string(),
        _ => "...".to_string(),
    }
}

// extract exit code from bash output text.
// tools.rs prefixes non-zero exits with "[exit code: N]\n"
fn parse_exit_code(output: &str) -> Option<i32> {
    if output.starts_with("[exit code: ") {
        output[12..]
            .split(']')
            .next()
            .and_then(|s| s.parse::<i32>().ok())
    } else {
        Some(0)
    }
}

// extract displayable text from a tool result content value.
// content is either a plain string or an array of {type, text} blocks.
fn tool_result_display_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|item| {
                if item.get("type")?.as_str()? == "text" {
                    item.get("text")?.as_str().map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// return the last paragraph from a block of text.
// paragraphs are separated by blank lines (\n\n).
fn last_paragraph(text: &str) -> &str {
    let trimmed = text.trim_end();
    if let Some(pos) = trimmed.rfind("\n\n") {
        let after = trimmed[pos + 2..].trim_start_matches('\n');
        if after.is_empty() {
            trimmed
        } else {
            after
        }
    } else {
        trimmed
    }
}

// remove ansi escape sequences and other terminal control codes from tool output.
// covers CSI sequences (\x1b[...X), OSC sequences (\x1b]...ST), character set
// designations (\x1b(F), and bare \x1b.
use crate::util::strip_ansi;

// strip the "[exit code: N]\n" prefix from bash output for display
fn strip_exit_prefix(s: &str) -> &str {
    if s.starts_with("[exit code: ") {
        if let Some(idx) = s.find("]\n") {
            return &s[idx + 2..];
        }
    }
    s
}

// indent for tool lines (no bar)
fn tool_line(spans: Vec<Span<'static>>) -> Line<'static> {
    let mut all = vec![Span::styled("  ", Style::default())];
    all.extend(spans);
    Line::from(all)
}

// wrap a plain text string to fit within `max_width` characters,
// returning one Line per visual row. each line gets the bar prefix.
fn wrap_text_with_bar(text: &str, max_width: u16, style: Style) -> Vec<Line<'static>> {
    let content_width = (max_width as usize).saturating_sub(BAR_WIDTH as usize);
    if content_width == 0 {
        return vec![];
    }

    let bar = Span::styled(BAR_STR, Style::default().fg(BAR_COLOR));

    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            lines.push(Line::from(vec![bar.clone()]));
            continue;
        }
        let mut remaining = raw_line;
        while !remaining.is_empty() {
            let w = UnicodeWidthStr::width(remaining);
            if w <= content_width {
                lines.push(Line::from(vec![
                    bar.clone(),
                    Span::styled(remaining.to_string(), style),
                ]));
                break;
            }
            // find a break point near content_width
            let mut split = 0;
            let mut cur_w = 0;
            for (i, ch) in remaining.char_indices() {
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if cur_w + cw > content_width {
                    break;
                }
                cur_w += cw;
                split = i + ch.len_utf8();
            }
            if split == 0 {
                split = remaining.len();
            }
            lines.push(Line::from(vec![
                bar.clone(),
                Span::styled(remaining[..split].to_string(), style),
            ]));
            remaining = &remaining[split..];
        }
    }
    lines
}

// wrap markdown-rendered lines so every visual row has the bar prefix.
// the TuiMarkdownRenderer produces Lines, but long ones get wrapped by
// ratatui without the bar. we do manual wrapping here instead.
fn wrap_md_lines_with_bar(md_lines: Vec<Line<'static>>, max_width: u16) -> Vec<Line<'static>> {
    let content_width = (max_width as usize).saturating_sub(BAR_WIDTH as usize);
    let bar = Span::styled(BAR_STR, Style::default().fg(BAR_COLOR));

    let mut out = Vec::new();
    for ml in md_lines {
        // estimate the display width of the line
        let line_width: usize = ml
            .spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        if line_width <= content_width {
            let mut spans = vec![bar.clone()];
            spans.extend(ml.spans);
            out.push(Line::from(spans));
        } else {
            // for long lines, flatten to styled chunks and re-wrap
            let mut cur_spans: Vec<Span<'static>> = vec![bar.clone()];
            let mut cur_width: usize = 0;

            for span in ml.spans {
                let span_text: &str = span.content.as_ref();
                let span_style = span.style;
                let mut remaining = span_text;

                while !remaining.is_empty() {
                    let avail = content_width.saturating_sub(cur_width);
                    if avail == 0 {
                        out.push(Line::from(std::mem::take(&mut cur_spans)));
                        cur_spans = vec![bar.clone()];
                        cur_width = 0;
                        continue;
                    }

                    let rw = UnicodeWidthStr::width(remaining);
                    if rw <= avail {
                        cur_spans.push(Span::styled(remaining.to_string(), span_style));
                        cur_width += rw;
                        break;
                    }

                    // split at avail
                    let mut split = 0;
                    let mut w = 0;
                    for (i, ch) in remaining.char_indices() {
                        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                        if w + cw > avail {
                            break;
                        }
                        w += cw;
                        split = i + ch.len_utf8();
                    }
                    if split == 0 && cur_width == 0 {
                        // single char wider than avail (shouldn't happen), force it
                        split = remaining.chars().next().map_or(0, |c| c.len_utf8());
                    }
                    if split > 0 {
                        cur_spans.push(Span::styled(remaining[..split].to_string(), span_style));
                    }
                    out.push(Line::from(std::mem::take(&mut cur_spans)));
                    cur_spans = vec![bar.clone()];
                    cur_width = 0;
                    remaining = &remaining[split..];
                }
            }
            if cur_spans.len() > 1 {
                out.push(Line::from(cur_spans));
            }
        }
    }
    out
}

impl App {
    #[allow(dead_code)]
    pub fn open_file(&mut self, path: &std::path::Path) {
        match EditorBuffer::open(path) {
            Ok(buf) => {
                self.lsp.pending.push(LspNotify::Open(path.to_path_buf()));
                self.editor.buffer = Some(buf);
                self.editor.diff_markers.clear();
                self.lsp.diagnostics.clear();
                if let Some(ref mut hl) = self.editor.highlighter { hl.invalidate(); }
                self.editor.mode = ViewMode::Editor;
            }
            Err(_) => {}
        }
    }

    pub fn open_agent_edit(&mut self, index: usize) {
        if index >= self.editor.agent_edits.len() {
            return;
        }
        let edit = self.editor.agent_edits[index].clone();
        let full_path = std::path::Path::new(&self.cwd).join(&edit.path);
        match EditorBuffer::open(&full_path) {
            Ok(mut buf) => {
                self.editor.diff_markers.clear();
                if let Some(ref diff) = edit.diff {
                    // build diff markers and track the changed line range
                    let mut first_change_line: Option<usize> = None;
                    let mut last_change_line: Option<usize> = None;
                    for hunk in &diff.hunks {
                        let mut new_line = hunk.new_start;
                        let mut pending_delete = false;
                        for dl in &hunk.lines {
                            match dl.tag {
                                DiffLineTag::Equal => {
                                    if pending_delete {
                                        self.editor.diff_markers.entry(new_line)
                                            .or_insert(DiffMarker::DeleteBoundary);
                                        pending_delete = false;
                                    }
                                    new_line += 1;
                                }
                                DiffLineTag::Insert => {
                                    if first_change_line.is_none() {
                                        first_change_line = Some(new_line);
                                    }
                                    last_change_line = Some(new_line);
                                    self.editor.diff_markers.insert(new_line, DiffMarker::Insert);
                                    pending_delete = false;
                                    new_line += 1;
                                }
                                DiffLineTag::Delete => {
                                    if first_change_line.is_none() {
                                        first_change_line = Some(new_line);
                                    }
                                    last_change_line = Some(new_line);
                                    pending_delete = true;
                                }
                            }
                        }
                        // if hunk ends with deletions, mark the next line
                        if pending_delete {
                            self.editor.diff_markers.entry(new_line)
                                .or_insert(DiffMarker::DeleteBoundary);
                        }
                    }
                    if let Some(first) = first_change_line {
                        let last = last_change_line.unwrap_or(first);
                        let h = crossterm::terminal::size()
                            .map(|(_, h)| h)
                            .unwrap_or(24) as usize;
                        buf.center_on_range(first, last, h.saturating_sub(2));
                    }
                } else if let Some(line) = edit.line {
                    // no diff (read tool) - center on the read offset
                    let target = line.saturating_sub(1);
                    let h = crossterm::terminal::size()
                        .map(|(_, h)| h)
                        .unwrap_or(24) as usize;
                    buf.center_on_range(target, target, h.saturating_sub(2));
                }
                self.editor.buffer = Some(buf);
                if let Some(ref mut hl) = self.editor.highlighter { hl.invalidate(); }
            }
            Err(_) => {}
        }
    }

    pub fn track_agent_edit(&mut self, path: String, diff: Option<DiffInfo>, line: Option<usize>) {
        // notify LSP that the file changed on disk
        self.lsp.pending.push(LspNotify::Open(std::path::PathBuf::from(&path)));

        let edit = AgentEdit {
            path,
            diff,
            line,
            _timestamp: std::time::Instant::now(),
        };
        self.editor.agent_edits.push(edit);

        // in follow mode, auto-jump to the new edit
        if self.editor.follow_mode {
            self.editor.agent_edit_index = self.editor.agent_edits.len() - 1;
            self.open_agent_edit(self.editor.agent_edit_index);
        }
    }

    fn update_file_search(&mut self) {
        if let Some(ref mut search) = self.editor.search {
            if !matches!(search.mode, SearchMode::Files) {
                return;
            }
            let root = std::path::Path::new(&self.cwd);
            let files = editor::collect_files(root);
            let mut scored: Vec<(i32, String)> = files
                .into_iter()
                .filter_map(|f| {
                    editor::fuzzy_match(&search.query, &f).map(|score| (score, f))
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            search.results = scored
                .into_iter()
                .take(100)
                .map(|(_, path)| editor::SearchResult {
                    path,
                    line: None,
                    content: None,
                })
                .collect();
            search.selected = 0;
        }
    }

    fn update_text_search(&mut self) {
        if let Some(ref mut search) = self.editor.search {
            if !matches!(search.mode, SearchMode::Text) {
                return;
            }
            let root = std::path::Path::new(&self.cwd);
            search.results = editor::search_text(root, &search.query, 100);
            search.selected = 0;
        }
    }
}

fn handle_editor_key(
    key: KeyEvent,
    app: &mut App,
    ctrl: bool,
    alt: bool,
    shift: bool,
    super_key: bool,
) -> InputAction {
    // search overlay intercepts input when active
    if app.editor.search.is_some() {
        return handle_search_key(key, app, ctrl);
    }

    // autocomplete menu intercepts navigation keys
    if app.editor.autocomplete.is_some() {
        match key.code {
            KeyCode::Tab | KeyCode::Enter => {
                // accept the selected completion
                if let Some(ac) = app.editor.autocomplete.take() {
                    if let Some(candidate) = ac.candidates.get(ac.selected) {
                        if let Some(ref mut buf) = app.editor.buffer {
                            let line = &mut buf.lines[buf.cursor_row];
                            let end = buf.cursor_col.min(line.len());
                            line.replace_range(ac.word_start..end, &candidate.label);
                            buf.cursor_col = ac.word_start + candidate.label.len();
                            buf.dirty = true;
                            buf.generation += 1;
                        }
                    }
                }
                return InputAction::None;
            }
            KeyCode::Up => {
                if let Some(ref mut ac) = app.editor.autocomplete {
                    ac.select_up();
                }
                return InputAction::None;
            }
            KeyCode::Down => {
                if let Some(ref mut ac) = app.editor.autocomplete {
                    ac.select_down();
                }
                return InputAction::None;
            }
            KeyCode::Esc => {
                app.editor.autocomplete = None;
                return InputAction::None;
            }
            // any other key dismisses and falls through to normal handling
            _ => {
                if !matches!(key.code, KeyCode::Char(_)) {
                    app.editor.autocomplete = None;
                }
            }
        }
    }

    match key.code {
        // ctrl+c: quit from editor
        KeyCode::Char('c') if ctrl => {
            return InputAction::Quit;
        }
        // ctrl+s: save
        KeyCode::Char('s') if ctrl => {
            if let Some(ref mut buf) = app.editor.buffer {
                let _ = buf.save();
                let path = buf.path.clone();
                app.lsp.pending.push(LspNotify::Save(path));
            }
        }
        // ctrl+z: undo
        KeyCode::Char('z') if ctrl => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.undo();
            }
        }
        // ctrl+y: redo
        KeyCode::Char('y') if ctrl => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.redo();
            }
        }
        // ctrl+p: file finder
        KeyCode::Char('p') if ctrl => {
            app.editor.search = Some(SearchState::new(SearchMode::Files));
            app.update_file_search();
        }
        // ctrl+shift+f or ctrl+/: text search
        KeyCode::Char('/') if ctrl => {
            app.editor.search = Some(SearchState::new(SearchMode::Text));
        }
        // ctrl+g: goto line (opens file search with : prefix behavior)
        KeyCode::Char('g') if ctrl => {
            // for now reuse file search
            app.editor.search = Some(SearchState::new(SearchMode::Files));
            app.update_file_search();
        }
        // F12: goto definition (LSP)
        KeyCode::F(12) => {
            if let Some(ref buf) = app.editor.buffer {
                app.lsp.goto_request = Some((
                    buf.path.clone(),
                    buf.cursor_row as u32,
                    buf.cursor_col as u32,
                ));
            }
        }
        // ctrl+k: delete line
        KeyCode::Char('k') if ctrl => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.delete_line();
            }
        }
        // navigation
        KeyCode::Up if alt => {
            // in follow mode, navigate to previous edit
            if app.editor.follow_mode && app.editor.agent_edit_index > 0 {
                app.editor.agent_edit_index -= 1;
                app.open_agent_edit(app.editor.agent_edit_index);
            }
        }
        KeyCode::Down if alt => {
            if app.editor.follow_mode && app.editor.agent_edit_index + 1 < app.editor.agent_edits.len() {
                app.editor.agent_edit_index += 1;
                app.open_agent_edit(app.editor.agent_edit_index);
            }
        }
        KeyCode::Up if shift => {
            if let Some(ref mut buf) = app.editor.buffer {
                let h = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24) as usize;
                buf.page_up(h.saturating_sub(2) / 2);
            }
        }
        KeyCode::Down if shift => {
            if let Some(ref mut buf) = app.editor.buffer {
                let h = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24) as usize;
                buf.page_down(h.saturating_sub(2) / 2);
            }
        }
        KeyCode::Up => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.move_up();
            }
        }
        KeyCode::Down => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.move_down();
            }
        }
        KeyCode::Left if super_key => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.move_home();
            }
        }
        KeyCode::Right if super_key => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.move_end();
            }
        }
        KeyCode::Left if ctrl || alt => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.move_word_left();
            }
        }
        KeyCode::Right if ctrl || alt => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.move_word_right();
            }
        }
        KeyCode::Left => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.move_left();
            }
        }
        KeyCode::Right => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.move_right();
            }
        }
        KeyCode::Home => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.move_home();
            }
        }
        KeyCode::End => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.move_end();
            }
        }
        KeyCode::PageUp => {
            if let Some(ref mut buf) = app.editor.buffer {
                let h = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24) as usize;
                buf.page_up(h.saturating_sub(2));
            }
        }
        KeyCode::PageDown => {
            if let Some(ref mut buf) = app.editor.buffer {
                let h = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24) as usize;
                buf.page_down(h.saturating_sub(2));
            }
        }
        // editing
        KeyCode::Enter => {
            if let Some(ref mut buf) = app.editor.buffer {
                let line = buf.lines[buf.cursor_row].clone();
                let col = buf.cursor_col.min(line.len());
                let prev = line[..col].chars().last();
                let next = line[col..].chars().next();

                // detect leading whitespace for auto-indent
                let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();

                if prev == Some('{') && next == Some('}') {
                    // expand braces: {|} becomes:
                    //   {
                    //       |
                    //   }
                    let uses_tabs = indent.contains('\t');
                    let extra = if uses_tabs { "\t".to_string() } else { "    ".to_string() };
                    let inner_indent = format!("{}{}", indent, extra);

                    buf.save_undo();
                    let before = &line[..col];
                    let after = &line[col..];
                    buf.lines[buf.cursor_row] = before.to_string();
                    buf.lines.insert(buf.cursor_row + 1, inner_indent.clone());
                    buf.lines.insert(buf.cursor_row + 2, format!("{}{}", indent, after));
                    buf.cursor_row += 1;
                    buf.cursor_col = inner_indent.len();
                    buf.dirty = true;
                } else {
                    buf.insert_newline();
                    // auto-indent: carry over leading whitespace from previous line
                    if !indent.is_empty() {
                        let new_line = &mut buf.lines[buf.cursor_row];
                        *new_line = format!("{}{}", indent, new_line);
                        buf.cursor_col = indent.len();
                    }
                }
            }
            app.editor.autocomplete = None;
        }
        KeyCode::Backspace if super_key => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.delete_to_line_start();
            }
            app.editor.autocomplete = None;
        }
        KeyCode::Backspace if alt || ctrl => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.delete_word_backward();
            }
            if let Some(ref buf) = app.editor.buffer {
                app.editor.autocomplete = crate::autocomplete::compute_completions(
                    &buf.lines,
                    buf.cursor_row,
                    buf.cursor_col,
                    &buf.path,
                );
            }
            app.request_lsp_completion();
        }
        KeyCode::Backspace => {
            if let Some(ref mut buf) = app.editor.buffer {
                // delete matching closing char when backspacing an empty pair
                if buf.cursor_col > 0 {
                    let line = &buf.lines[buf.cursor_row];
                    let prev = line[..buf.cursor_col].chars().last();
                    let next = line[buf.cursor_col..].chars().next();
                    let is_empty_pair = matches!(
                        (prev, next),
                        (Some('('), Some(')'))
                        | (Some('['), Some(']'))
                        | (Some('{'), Some('}'))
                        | (Some('"'), Some('"'))
                        | (Some('\''), Some('\''))
                        | (Some('`'), Some('`'))
                    );
                    if is_empty_pair {
                        buf.delete();
                    }
                }
                buf.backspace();
            }
            // re-trigger autocomplete with updated prefix
            if let Some(ref buf) = app.editor.buffer {
                app.editor.autocomplete = crate::autocomplete::compute_completions(
                    &buf.lines,
                    buf.cursor_row,
                    buf.cursor_col,
                    &buf.path,
                );
            }
            app.request_lsp_completion();
        }
        KeyCode::Delete if alt || ctrl => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.delete_word_forward();
            }
        }
        KeyCode::Delete => {
            if let Some(ref mut buf) = app.editor.buffer {
                buf.delete();
            }
        }
        KeyCode::Tab => {
            if let Some(ref mut buf) = app.editor.buffer {
                // insert 4 spaces
                for _ in 0..4 {
                    buf.insert_char(' ');
                }
            }
        }
        KeyCode::Char(c) => {
            if let Some(ref mut buf) = app.editor.buffer {
                let closing = match c {
                    '(' => Some(')'),
                    '[' => Some(']'),
                    '{' => Some('}'),
                    '"' => Some('"'),
                    '\'' => Some('\''),
                    '`' => Some('`'),
                    _ => None,
                };
                // typing a closing char that's already under the cursor: skip over it
                let char_at_cursor = buf.lines.get(buf.cursor_row)
                    .and_then(|l| l[buf.cursor_col..].chars().next());
                let is_quote = matches!(c, '"' | '\'' | '`');
                if is_quote && char_at_cursor == Some(c) {
                    buf.cursor_col += c.len_utf8();
                } else if !is_quote && matches!(c, ')' | ']' | '}') && char_at_cursor == Some(c) {
                    buf.cursor_col += c.len_utf8();
                } else if let Some(close) = closing {
                    buf.insert_char(c);
                    // insert the closing char without moving cursor
                    let line = &mut buf.lines[buf.cursor_row];
                    if buf.cursor_col >= line.len() {
                        line.push(close);
                    } else {
                        line.insert(buf.cursor_col, close);
                    }
                } else {
                    buf.insert_char(c);
                }
            }
            // trigger or update autocomplete
            let is_trigger = c.is_alphanumeric() || c == '_'
                || c == '.' || c == ':' || c == '-';
            if is_trigger {
                if let Some(ref buf) = app.editor.buffer {
                    app.editor.autocomplete = crate::autocomplete::compute_completions(
                        &buf.lines,
                        buf.cursor_row,
                        buf.cursor_col,
                        &buf.path,
                    );
                }
                app.request_lsp_completion();
            } else {
                app.editor.autocomplete = None;
            }
        }
        _ => {}
    }

    // dismiss autocomplete on movement keys
    if matches!(key.code,
        KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End |
        KeyCode::PageUp | KeyCode::PageDown
    ) {
        app.editor.autocomplete = None;
    }

    // keep cursor visible after any action
    if let Some(ref mut buf) = app.editor.buffer {
        let h = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24) as usize;
        buf.ensure_cursor_visible(h.saturating_sub(2)); // minus status bar
    }

    InputAction::None
}

fn handle_search_key(key: KeyEvent, app: &mut App, ctrl: bool) -> InputAction {
    match key.code {
        KeyCode::Esc => {
            app.editor.search = None;
        }
        KeyCode::Enter => {
            // open selected result
            if let Some(ref search) = app.editor.search {
                if let Some(result) = search.results.get(search.selected) {
                    let full_path = std::path::Path::new(&app.cwd).join(&result.path);
                    let line = result.line;
                    match EditorBuffer::open(&full_path) {
                        Ok(mut buf) => {
                            if let Some(l) = line {
                                buf.goto_line(l);
                                let h = crossterm::terminal::size()
                                    .map(|(_, h)| h)
                                    .unwrap_or(24) as usize;
                                buf.ensure_cursor_visible(h.saturating_sub(2));
                            }
                            app.editor.buffer = Some(buf);
                            app.editor.diff_markers.clear();
                            if let Some(ref mut hl) = app.editor.highlighter { hl.invalidate(); }
                        }
                        Err(_) => {}
                    }
                }
            }
            app.editor.search = None;
        }
        KeyCode::Up => {
            if let Some(ref mut search) = app.editor.search {
                search.select_up();
            }
        }
        KeyCode::Down => {
            if let Some(ref mut search) = app.editor.search {
                search.select_down();
            }
        }
        KeyCode::Backspace => {
            if let Some(ref mut search) = app.editor.search {
                search.backspace();
            }
            match app.editor.search.as_ref().map(|s| s.mode.clone()) {
                Some(SearchMode::Files) => app.update_file_search(),
                Some(SearchMode::Text) => app.update_text_search(),
                None => {}
            }
        }
        KeyCode::Char('c') if ctrl => {
            app.editor.search = None;
        }
        KeyCode::Char(c) => {
            if let Some(ref mut search) = app.editor.search {
                search.insert_char(c);
            }
            match app.editor.search.as_ref().map(|s| s.mode.clone()) {
                Some(SearchMode::Files) => app.update_file_search(),
                Some(SearchMode::Text) => app.update_text_search(),
                None => {}
            }
        }
        _ => {}
    }
    InputAction::None
}

pub fn render(frame: &mut Frame, app: &mut App) {
    if app.tree_view.is_some() {
        render_tree_view(frame, app);
        return;
    }
    match app.editor.mode {
        ViewMode::Chat => render_chat(frame, app),
        ViewMode::Editor => render_editor(frame, app),
    }
}

fn render_editor(frame: &mut Frame, app: &mut App) {
    let size = frame.area();
    let has_search = app.editor.search.is_some();
    let search_h: u16 = if has_search { 12.min(size.height / 3) } else { 0 };

    if app.editor.buffer.is_none() && !has_search {
        // split horizontally so the sidebar still renders
        let hsplit = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(20),
                Constraint::Length(SIDEBAR_WIDTH),
            ])
            .split(size);
        let msg = Paragraph::new(Line::from(vec![
            Span::styled("  no file open. ", Style::default().fg(MUTED)),
            Span::styled("ctrl+p", Style::default().fg(ACCENT)),
            Span::styled(" to find a file, ", Style::default().fg(MUTED)),
            Span::styled("ctrl+e", Style::default().fg(ACCENT)),
            Span::styled(" to go back", Style::default().fg(MUTED)),
        ]));
        frame.render_widget(msg, hsplit[0]);
        render_editor_sidebar(frame, app, hsplit[1]);
        return;
    }

    // split: left (editor + search) | right (sidebar)
    let hsplit = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(20),
            Constraint::Length(SIDEBAR_WIDTH),
        ])
        .split(size);

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                   // status bar
            Constraint::Min(3),                      // editor content
            Constraint::Length(search_h),             // search overlay
        ])
        .split(hsplit[0]);

    render_editor_status(frame, app, left_chunks[0]);

    if app.editor.buffer.is_some() {
        render_editor_content(frame, app, left_chunks[1]);
        render_autocomplete_menu(frame, app, left_chunks[1]);
    }

    if has_search {
        render_search_overlay(frame, app, left_chunks[2]);
    }

    render_editor_sidebar(frame, app, hsplit[1]);
}

fn render_editor_status(frame: &mut Frame, app: &App, area: Rect) {
    let buf = match &app.editor.buffer {
        Some(b) => b,
        None => return,
    };

    let dirty_marker = if buf.dirty { " [+]" } else { "" };
    let rel_path = buf.relative_path(&app.cwd);
    let position = format!("{}:{}", buf.cursor_row + 1, buf.cursor_col + 1);
    let lines = format!("{} lines", buf.line_count());

    let follow_indicator = if app.editor.follow_mode {
        let edit_pos = if app.editor.agent_edits.is_empty() {
            String::new()
        } else {
            format!(" {}/{}", app.editor.agent_edit_index + 1, app.editor.agent_edits.len())
        };
        format!("  follow{}", edit_pos)
    } else {
        String::new()
    };

    let right = format!("{}  {}  {}", follow_indicator, lines, position);
    let left_budget = (area.width as usize).saturating_sub(right.len() + 2);

    let display_path = if rel_path.len() > left_budget {
        format!("...{}", &rel_path[rel_path.len().saturating_sub(left_budget - 3)..])
    } else {
        rel_path
    };

    let pad = (area.width as usize).saturating_sub(display_path.len() + dirty_marker.len() + right.len());

    let mut spans = vec![
        Span::styled(format!(" {}", display_path), Style::default().fg(FG)),
        Span::styled(dirty_marker.to_string(), Style::default().fg(YELLOW)),
        Span::styled(" ".repeat(pad), Style::default()),
    ];

    if app.editor.follow_mode {
        let fi = format!("  follow");
        let edit_pos = if app.editor.agent_edits.is_empty() {
            String::new()
        } else {
            format!(" {}/{}", app.editor.agent_edit_index + 1, app.editor.agent_edits.len())
        };
        spans.push(Span::styled(fi, Style::default().fg(GREEN)));
        spans.push(Span::styled(edit_pos, Style::default().fg(DIM)));
        spans.push(Span::styled(
            format!("  {}  {}", lines, position),
            Style::default().fg(MUTED),
        ));
    } else {
        spans.push(Span::styled(right, Style::default().fg(MUTED)));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(INPUT_BG)),
        area,
    );
}

fn render_editor_content(frame: &mut Frame, app: &mut App, area: Rect) {
    use unicode_width::UnicodeWidthStr;

    let buf = match &app.editor.buffer {
        Some(b) => b,
        None => return,
    };

    let viewport_h = area.height as usize;
    let gutter_width: u16 = (buf.line_count().max(1).to_string().len() + 2) as u16;
    let content_cols = (area.width as usize).saturating_sub(gutter_width as usize);

    // initialize highlighter lazily
    if app.editor.highlighter.is_none() {
        app.editor.highlighter = Some(editor::Highlighter::new());
    }

    // request enough highlighted lines to fill the viewport even with wrapping.
    // worst case every line wraps, but typically we need fewer.
    let hl_request = viewport_h;
    let highlighted = if let Some(ref mut hl) = app.editor.highlighter {
        hl.highlight_lines(&buf.path, &buf.lines, buf.generation, buf.scroll_row, hl_request)
    } else {
        Vec::new()
    };

    let mut lines: Vec<Line> = Vec::with_capacity(viewport_h);
    // track which screen row the cursor lands on
    let mut cursor_screen_row: Option<usize> = None;
    let mut cursor_screen_col: Option<usize> = None;
    let mut line_idx = buf.scroll_row;
    let mut hl_idx: usize = 0;

    while lines.len() < viewport_h {
        if line_idx >= buf.lines.len() {
            let spans = vec![
                Span::styled(
                    format!("{:>width$} ", "~", width = gutter_width as usize - 1),
                    Style::default().fg(DIM),
                ),
            ];
            lines.push(Line::from(spans));
            continue; // will hit viewport_h and exit
        }

        let is_cursor_line = line_idx == buf.cursor_row;
        let diff_marker = if app.editor.follow_mode { app.editor.diff_markers.get(&line_idx) } else { None };

        // check for LSP diagnostics on this line
        let line_diag = app.lsp.diagnostics.iter().find(|d| d.line as usize == line_idx);
        let diag_severity = line_diag.map(|d| d.severity);

        let gutter_style = match diff_marker {
            Some(DiffMarker::Insert) => Style::default().fg(GREEN),
            Some(DiffMarker::DeleteBoundary) => Style::default().fg(RED),
            _ if is_cursor_line => Style::default().fg(ACCENT),
            _ => Style::default().fg(DIM),
        };

        let line_bg = match diff_marker {
            Some(DiffMarker::Insert) => Some(Color::Rgb(20, 40, 20)),
            Some(DiffMarker::DeleteBoundary) => Some(Color::Rgb(40, 20, 20)),
            _ => match diag_severity {
                Some(crate::lsp::DiagSeverity::Error) => Some(Color::Rgb(40, 15, 15)),
                Some(crate::lsp::DiagSeverity::Warning) => Some(Color::Rgb(40, 30, 10)),
                _ => None,
            },
        };

        let cursor_bg = Color::Rgb(30, 33, 40);

        // build the content spans for this file line
        let content_spans: Vec<(Style, String)> = if hl_idx < highlighted.len() {
            highlighted[hl_idx]
                .iter()
                .map(|(style, text)| {
                    let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                    let mut s = Style::default().fg(fg);
                    if is_cursor_line {
                        s = s.bg(cursor_bg);
                    } else if let Some(bg) = line_bg {
                        s = s.bg(bg);
                    }
                    (s, text.clone())
                })
                .collect()
        } else {
            let text = &buf.lines[line_idx];
            let mut s = if is_cursor_line {
                Style::default().fg(FG).bg(cursor_bg)
            } else {
                Style::default().fg(FG)
            };
            if let Some(bg) = line_bg {
                if !is_cursor_line {
                    s = s.bg(bg);
                }
            }
            vec![(s, text.clone())]
        };

        // compute cursor visual column within this line
        let cursor_vcol = if is_cursor_line {
            let line = &buf.lines[buf.cursor_row];
            let byte_pos = buf.cursor_col.min(line.len());
            let safe_pos = (0..=byte_pos).rev().find(|&i| line.is_char_boundary(i)).unwrap_or(0);
            Some(UnicodeWidthStr::width(&line[..safe_pos]))
        } else {
            None
        };

        // split content spans into wrapped screen rows
        let wrap_rows = wrap_spans(&content_spans, content_cols);
        let num_wraps = wrap_rows.len().max(1);

        for (wrap_i, row_spans) in wrap_rows.iter().enumerate() {
            if lines.len() >= viewport_h {
                break;
            }

            let mut spans: Vec<Span> = Vec::new();

            // gutter: show line number on first wrap row, blank on continuation
            if wrap_i == 0 {
                let line_num = format!("{:>width$} ", line_idx + 1, width = gutter_width as usize - 1);
                spans.push(Span::styled(line_num, gutter_style));
            } else {
                let blank = format!("{:>width$} ", "·", width = gutter_width as usize - 1);
                spans.push(Span::styled(blank, Style::default().fg(DIM)));
            }

            spans.extend(row_spans.iter().cloned());

            let row_width: usize = row_spans.iter().map(|s| UnicodeWidthStr::width(s.content.as_ref())).sum();
            let remaining = content_cols.saturating_sub(row_width);
            let is_last_wrap = wrap_i == num_wraps - 1;

            // inline diagnostic after end of line content (last wrap row only)
            let mut diag_used = 0usize;
            if is_last_wrap {
                if let Some(d) = line_diag {
                    let (icon, color) = match d.severity {
                        crate::lsp::DiagSeverity::Error => (" ✗ ", Color::Rgb(255, 100, 100)),
                        crate::lsp::DiagSeverity::Warning => (" ⚠ ", YELLOW),
                        crate::lsp::DiagSeverity::Info => (" ℹ ", Color::Rgb(100, 180, 255)),
                        crate::lsp::DiagSeverity::Hint => (" · ", MUTED),
                    };
                    if remaining > 5 {
                        let icon_w = UnicodeWidthStr::width(icon);
                        let msg_budget = remaining.saturating_sub(icon_w + 1);
                        let msg: String = d.message.chars().take(msg_budget).collect();
                        let msg_w = UnicodeWidthStr::width(msg.as_str());
                        spans.push(Span::styled(icon, Style::default().fg(color)));
                        spans.push(Span::styled(msg, Style::default().fg(color)));
                        diag_used = icon_w + msg_w;
                    }
                }
            }

            // pad to edge
            let pad_bg = if is_cursor_line {
                Some(cursor_bg)
            } else {
                line_bg
            };
            if let Some(bg) = pad_bg {
                let pad = remaining.saturating_sub(diag_used);
                if pad > 0 {
                    spans.push(Span::styled(
                        " ".repeat(pad),
                        Style::default().bg(bg),
                    ));
                }
            }

            // track cursor screen position
            if let Some(vcol) = cursor_vcol {
                let row_start_col = wrap_i * content_cols;
                let row_end_col = row_start_col + content_cols;
                if vcol >= row_start_col && vcol < row_end_col {
                    cursor_screen_row = Some(lines.len());
                    cursor_screen_col = Some(vcol - row_start_col);
                } else if wrap_i == num_wraps - 1 && cursor_screen_row.is_none() {
                    // cursor past end of last wrap row
                    cursor_screen_row = Some(lines.len());
                    let row_width: usize = row_spans.iter().map(|s| UnicodeWidthStr::width(s.content.as_ref())).sum();
                    cursor_screen_col = Some(row_width.min(vcol.saturating_sub(row_start_col)));
                }
            }

            lines.push(Line::from(spans));
        }

        // empty line (no content) still needs one row
        if wrap_rows.is_empty() {
            if lines.len() < viewport_h {
                let line_num = format!("{:>width$} ", line_idx + 1, width = gutter_width as usize - 1);
                let mut spans = vec![Span::styled(line_num, gutter_style)];
                let pad_bg = if is_cursor_line { Some(cursor_bg) } else { line_bg };
                if let Some(bg) = pad_bg {
                    spans.push(Span::styled(
                        " ".repeat(content_cols),
                        Style::default().bg(bg),
                    ));
                }
                if is_cursor_line {
                    cursor_screen_row = Some(lines.len());
                    cursor_screen_col = Some(0);
                }
                lines.push(Line::from(spans));
            }
        }

        line_idx += 1;
        hl_idx += 1;
    }

    let widget = Paragraph::new(lines).style(Style::default().bg(BG));
    frame.render_widget(widget, area);

    // set cursor position
    if let (Some(row), Some(col)) = (cursor_screen_row, cursor_screen_col) {
        let cursor_x = area.x + gutter_width + col as u16;
        let cursor_y = area.y + row as u16;
        if cursor_y < area.y + area.height && cursor_x < area.x + area.width {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }
}

// split a sequence of styled spans into rows that fit within `max_cols` visual columns
fn wrap_spans(spans: &[(Style, String)], max_cols: usize) -> Vec<Vec<Span<'static>>> {
    use unicode_width::UnicodeWidthChar;

    if max_cols == 0 {
        return vec![];
    }

    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current_row: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;

    for (style, text) in spans {
        let mut segment = String::new();
        for ch in text.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if col + w > max_cols && col > 0 {
                // wrap: push current segment, start new row
                if !segment.is_empty() {
                    current_row.push(Span::styled(segment.clone(), *style));
                    segment.clear();
                }
                rows.push(std::mem::take(&mut current_row));
                col = 0;
            }
            segment.push(ch);
            col += w;
        }
        if !segment.is_empty() {
            current_row.push(Span::styled(segment, *style));
        }
    }
    if !current_row.is_empty() {
        rows.push(current_row);
    }

    rows
}

fn render_autocomplete_menu(frame: &mut Frame, app: &App, area: Rect) {
    use unicode_width::UnicodeWidthStr;

    let ac = match &app.editor.autocomplete {
        Some(ac) if !ac.candidates.is_empty() => ac,
        _ => return,
    };
    let buf = match &app.editor.buffer {
        Some(b) => b,
        None => return,
    };

    let gutter_width = (buf.line_count().max(1).to_string().len() + 2) as u16;
    let content_cols = (area.width as usize).saturating_sub(gutter_width as usize);

    // find cursor screen position relative to editor area
    let line = &buf.lines[buf.cursor_row];
    let word_start_visual = {
        let safe = ac.word_start.min(line.len());
        let pos = (0..=safe).rev().find(|&i| line.is_char_boundary(i)).unwrap_or(0);
        UnicodeWidthStr::width(&line[..pos])
    };

    // account for wrapping
    let wrap_row = word_start_visual / content_cols.max(1);
    let wrap_col = word_start_visual % content_cols.max(1);

    // compute screen row of cursor line (accounting for wrapped lines above)
    let mut screen_row = 0usize;
    for i in buf.scroll_row..buf.cursor_row {
        if i >= buf.lines.len() { break; }
        let lw = UnicodeWidthStr::width(buf.lines[i].as_str());
        screen_row += if lw == 0 || content_cols == 0 { 1 } else { (lw + content_cols - 1) / content_cols };
    }
    screen_row += wrap_row;

    // position the menu below the cursor line
    let menu_y = area.y + screen_row as u16 + 1;
    let menu_x = area.x + gutter_width + wrap_col as u16;
    let max_label = ac.candidates.iter().map(|c| {
        let detail_len = c.detail.as_ref().map(|d| d.len() + 1).unwrap_or(0);
        c.label.len() + detail_len
    }).max().unwrap_or(10);
    let menu_w = (max_label + 4).min(50) as u16;
    let menu_h = ac.candidates.len().min(8) as u16;

    // flip above if not enough room below
    let available_below = area.y + area.height - menu_y;
    let (final_y, final_h) = if available_below >= menu_h {
        (menu_y, menu_h.min(available_below))
    } else {
        let above = screen_row as u16;
        let h = menu_h.min(above);
        (area.y + screen_row as u16 - h, h)
    };

    // clamp to area bounds
    let final_x = menu_x.min(area.x + area.width - menu_w.min(area.width));
    let final_w = menu_w.min(area.x + area.width - final_x);

    if final_h == 0 || final_w == 0 {
        return;
    }

    let menu_area = Rect::new(final_x, final_y, final_w, final_h);

    let mut lines: Vec<Line> = Vec::new();
    for (i, candidate) in ac.candidates.iter().take(final_h as usize).enumerate() {
        let is_selected = i == ac.selected;

        let kind_icon = candidate.kind.icon();

        let (label_style, icon_style, detail_style) = if is_selected {
            (
                Style::default().fg(BG).bg(ACCENT),
                Style::default().fg(BG).bg(ACCENT),
                Style::default().fg(BG).bg(ACCENT),
            )
        } else {
            (
                Style::default().fg(FG).bg(INPUT_BG),
                Style::default().fg(MUTED).bg(INPUT_BG),
                Style::default().fg(MUTED).bg(INPUT_BG),
            )
        };

        let max_label_len = (final_w as usize).saturating_sub(4);
        let label: String = candidate.label.chars().take(max_label_len).collect();
        // show detail (type info) if there's room
        let detail_str = if let Some(ref d) = candidate.detail {
            let avail = max_label_len.saturating_sub(label.len() + 1);
            if avail > 3 {
                let d: String = d.chars().take(avail).collect();
                format!(" {}", d)
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        let pad = (final_w as usize).saturating_sub(label.len() + detail_str.len() + 3);
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", kind_icon), icon_style),
            Span::styled(label, label_style),
            Span::styled(detail_str, detail_style),
            Span::styled(" ".repeat(pad), label_style),
        ]));
    }

    let widget = Paragraph::new(lines).style(Style::default().bg(INPUT_BG));
    frame.render_widget(ratatui::widgets::Clear, menu_area);
    frame.render_widget(widget, menu_area);
}

fn render_search_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let search = match &app.editor.search {
        Some(s) => s,
        None => return,
    };

    let mode_label = match search.mode {
        SearchMode::Files => "files",
        SearchMode::Text => "search",
    };

    // input line
    let input_line = Line::from(vec![
        Span::styled(format!(" {} ", mode_label), Style::default().fg(BG).bg(ACCENT)),
        Span::styled(format!(" {}", search.query), Style::default().fg(FG)),
    ]);

    let mut lines = vec![input_line];

    // results
    let max_visible = (area.height as usize).saturating_sub(1);
    let start = if search.selected >= max_visible {
        search.selected - max_visible + 1
    } else {
        0
    };

    for (i, result) in search.results.iter().skip(start).take(max_visible).enumerate() {
        let idx = start + i;
        let is_selected = idx == search.selected;

        let (path_style, content_style) = if is_selected {
            (
                Style::default().fg(BG).bg(ACCENT),
                Style::default().fg(BG).bg(ACCENT),
            )
        } else {
            (Style::default().fg(ACCENT), Style::default().fg(MUTED))
        };

        let mut spans = vec![Span::styled(format!("  {}", result.path), path_style)];

        if let Some(line) = result.line {
            spans.push(Span::styled(format!(":{}", line + 1), path_style));
        }

        if let Some(ref content) = result.content {
            let truncated = if content.chars().count() > 60 {
                let s: String = content.chars().take(57).collect();
                format!("  {}...", s)
            } else {
                format!("  {}", content)
            };
            spans.push(Span::styled(truncated, content_style));
        }

        lines.push(Line::from(spans));
    }

    let widget = Paragraph::new(lines).style(Style::default().bg(INPUT_BG));
    frame.render_widget(widget, area);
}

fn render_editor_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    if area.width < 4 || area.height < 2 {
        return;
    }

    let max_w = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();

    // render activity items bottom-up (most recent first),
    // fitting as many as the sidebar height allows
    let avail = area.height as usize;
    let mut item_lines: Vec<Line> = Vec::new();

    for item in app.feed.items.iter().rev() {
        if item_lines.len() >= avail {
            break;
        }
        match item {
            ActivityItem::Thinking(t) => {
                let text = t.chars().take(max_w).collect::<String>();
                item_lines.push(Line::from(Span::styled(
                    format!(" {}", text),
                    Style::default().fg(THINKING_COLOR).add_modifier(Modifier::ITALIC),
                )));
            }
            ActivityItem::Text(t) => {
                // show just the last line of text output
                let last = t.lines().last().unwrap_or("");
                let text: String = last.chars().take(max_w.saturating_sub(2)).collect();
                item_lines.push(Line::from(vec![
                    Span::styled(" \u{2502} ", Style::default().fg(BAR_COLOR)),
                    Span::styled(text, Style::default().fg(FG)),
                ]));
            }
            ActivityItem::Tool(entry) => {
                let icon = match entry.status {
                    ToolStatus::Running => spinner_char(app.spin_frame),
                    ToolStatus::Complete { exit_code } => {
                        if exit_code.unwrap_or(0) != 0 { "✗" } else { "✓" }
                    }
                    ToolStatus::Error(_) => "✗",
                };
                let icon_color = match entry.status {
                    ToolStatus::Running => ACCENT,
                    ToolStatus::Complete { exit_code } => {
                        if exit_code.unwrap_or(0) != 0 { RED } else { GREEN }
                    }
                    ToolStatus::Error(_) => RED,
                };
                let label = capitalize_tool(&entry.name);
                let arg_budget = max_w.saturating_sub(label.len() + 4);
                let arg: String = entry.arg.chars().take(arg_budget).collect();
                item_lines.push(Line::from(vec![
                    Span::styled(format!(" {} ", icon), Style::default().fg(icon_color)),
                    Span::styled(label.to_string(), Style::default().fg(TOOL_COLOR)),
                    Span::styled(format!(" {}", arg), Style::default().fg(DIM)),
                ]));
            }
            ActivityItem::UserMessage(msg) => {
                let text: String = msg.chars().take(max_w.saturating_sub(2)).collect();
                item_lines.push(Line::from(vec![
                    Span::styled(" > ", Style::default().fg(ACCENT)),
                    Span::styled(text, Style::default().fg(FG)),
                ]));
            }
            ActivityItem::System(kind, msg) => {
                let color = match kind {
                    SystemKind::Info => MUTED,
                    SystemKind::Success => GREEN,
                    SystemKind::Warning => YELLOW,
                    SystemKind::Error => RED,
                    SystemKind::Update => ACCENT,
                };
                let text: String = msg.lines().next().unwrap_or("").chars().take(max_w).collect();
                item_lines.push(Line::from(Span::styled(
                    format!(" {}", text),
                    Style::default().fg(color),
                )));
            }
            ActivityItem::Compact(status) => {
                let (icon, text) = match status {
                    CompactStatus::Running => (spinner_char(app.spin_frame), "compacting..."),
                    CompactStatus::Done(_) => ("✓", "compacted"),
                    CompactStatus::Cancelled => ("✗", "cancelled"),
                };
                item_lines.push(Line::from(vec![
                    Span::styled(format!(" {} ", icon), Style::default().fg(MUTED)),
                    Span::styled(text.to_string(), Style::default().fg(MUTED)),
                ]));
            }
        }
    }

    // reverse so newest is at bottom
    item_lines.reverse();
    lines.extend(item_lines);

    // separator on the left edge
    let sep_area = Rect {
        x: area.x,
        y: area.y,
        width: 1,
        height: area.height,
    };
    let sep_lines: Vec<Line> = (0..area.height)
        .map(|_| Line::from(Span::styled("\u{2502}", Style::default().fg(Color::Rgb(40, 44, 52)))))
        .collect();
    frame.render_widget(Paragraph::new(sep_lines), sep_area);

    let content_area = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(1),
        height: area.height,
    };
    frame.render_widget(Paragraph::new(lines), content_area);
}

fn render_tree_view(frame: &mut Frame, app: &mut App) {
    use crate::tree::NodeKind;

    let tv = match app.tree_view.as_ref() {
        Some(tv) => tv,
        None => return,
    };

    let size = frame.area();
    frame.render_widget(
        ratatui::widgets::Block::default().style(Style::default().bg(BG)),
        size,
    );

    // header
    let header_area = Rect::new(0, 0, size.width, 1);
    let branch_count = app.session_tree.branches.len();
    let header_text = format!(
        " session tree  {} branch{}  (↑↓ navigate, ⇧↑↓ jump user msgs, enter fork, space switch, esc close)",
        branch_count,
        if branch_count == 1 { "" } else { "es" }
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(header_text, Style::default().fg(MUTED)),
        ]))
        .style(Style::default().bg(SURFACE)),
        header_area,
    );

    // tree content area
    let content_area = Rect::new(0, 1, size.width, size.height.saturating_sub(1));
    let viewport_h = content_area.height as usize;

    if tv.nodes.is_empty() {
        frame.render_widget(
            Paragraph::new("  (empty session)")
                .style(Style::default().fg(MUTED).bg(BG)),
            content_area,
        );
        return;
    }

    let active_branch = app.session_tree.active;

    for (vi, node_idx) in (tv.scroll..).take(viewport_h).enumerate() {
        if node_idx >= tv.nodes.len() {
            break;
        }
        let node = &tv.nodes[node_idx];
        let is_selected = node_idx == tv.cursor;
        let is_active_branch = node.branch_idx == active_branch;
        let y = content_area.y + vi as u16;
        let row_area = Rect::new(0, y, size.width, 1);

        let mut spans: Vec<Span> = Vec::new();

        // indentation + tree connectors
        let indent_w = node.depth * 3;
        if indent_w > 0 {
            let mut indent = String::new();
            for d in 0..node.depth {
                if d == node.depth - 1 {
                    if node.branch_head {
                        if node.is_last_child {
                            indent.push_str(" └─");
                        } else {
                            indent.push_str(" ├─");
                        }
                    } else if node.active_pipes.contains(&d) {
                        indent.push_str(" │ ");
                    } else {
                        indent.push_str("   ");
                    }
                } else if node.active_pipes.contains(&d) {
                    indent.push_str(" │ ");
                } else {
                    indent.push_str("   ");
                }
            }
            spans.push(Span::styled(indent, Style::default().fg(DIM)));
        }

        // node icon
        let (icon, icon_color) = match node.kind {
            NodeKind::UserMessage => ("● ", ACCENT),
            NodeKind::AssistantText => ("○ ", FG),
            NodeKind::ToolCall => ("◆ ", YELLOW),
            NodeKind::Thinking => ("◇ ", DIM),
            NodeKind::Compact => ("◈ ", GREEN),
        };
        spans.push(Span::styled(icon, Style::default().fg(icon_color)));

        // node text
        let max_text = (size.width as usize)
            .saturating_sub(indent_w + 2 + 8); // icon + branch tag
        let text = if node.text.len() > max_text && max_text > 3 {
            format!("{}...", &node.text[..max_text - 3])
        } else {
            node.text.clone()
        };

        let text_color = if is_active_branch { FG } else { MUTED };
        spans.push(Span::styled(text, Style::default().fg(text_color)));

        // branch indicator for fork heads
        if node.branch_head {
            let tag = if is_active_branch {
                " ◀"
            } else {
                ""
            };
            if !tag.is_empty() {
                spans.push(Span::styled(
                    tag.to_string(),
                    Style::default().fg(ACCENT),
                ));
            }
        }

        let bg = if is_selected { SURFACE } else { BG };
        let line = Line::from(spans);
        frame.render_widget(
            Paragraph::new(line).style(Style::default().bg(bg)),
            row_area,
        );
    }
}

fn render_chat(frame: &mut Frame, app: &mut App) {
    let size = frame.area();

    let max_input = (size.height / 3).max(2);

    // slash command suggestions shown when input starts with "/".
    // use the snapshot taken at first Tab press so cycling doesn't narrow the set.
    let completion_input = app.input.slash_prefix.as_deref().unwrap_or(app.input.text.as_str());
    let suggestions: Vec<Suggestion> = if app.input.text.starts_with('/') {
        slash_suggestions(completion_input)
    } else {
        vec![]
    };
    let slash_selected = app.input.slash_selected;

    let message_height: u16 = if !suggestions.is_empty() {
        (suggestions.len() as u16).min(8)
    } else if app.is_running {
        let mut total: u16 = 0;
        if let Some(ref msg) = app.current_message {
            total += visual_line_count(msg, size.width, 2) as u16;
        }
        for qm in &app.queued_messages {
            let text = match qm {
                QueuedItem::Message(s) | QueuedItem::Command(s) => s.as_str(),
            };
            total += visual_line_count(text, size.width, 2) as u16;
        }
        total
    } else {
        0
    };

    let input_only_height = (app.input_visual_line_count() as u16).max(1);
    let combined = (message_height + input_only_height).max(1).min(max_input);
    let msg_h = message_height.min(combined);
    let input_h = combined - msg_h;

    let visible_jobs = app.background_jobs.iter().any(|j| j.visible);
    // no jobs: 1 blank line at bottom
    // jobs: 1 buffer line + 1 jobs line
    let (jobs_h, bottom_h): (u16, u16) = if visible_jobs { (1, 1) } else { (0, 1) };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),        // header
            Constraint::Length(msg_h),    // current/queued messages
            Constraint::Length(input_h),  // input field
            Constraint::Length(1),        // buffer after input
            Constraint::Min(4),           // activity feed
            Constraint::Length(bottom_h), // buffer before jobs / bottom
            Constraint::Length(jobs_h),   // background jobs bar
        ])
        .split(size);

    render_header(frame, app, chunks[0]);
    render_message_area(frame, app, chunks[1], &suggestions, slash_selected);
    render_input_area(frame, app, chunks[2]);

    // chunks[3] is the buffer after input
    render_activity(frame, app, chunks[4]);
    if jobs_h > 0 {
        render_jobs_bar(frame, app, chunks[6]);
    }
}

fn render_jobs_bar(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }

    let mut spans: Vec<Span> = Vec::new();
    let visible: Vec<&BackgroundJob> = app.background_jobs.iter().filter(|j| j.visible).collect();
    for (i, job) in visible.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  │  ", Style::default().fg(DIM)));
        }
        let (icon, icon_color) = match &job.status {
            JobStatus::Running => ("◌", YELLOW),
            JobStatus::Passed => ("✓", GREEN),
            JobStatus::Failed(_) => ("✗", RED),
        };
        spans.push(Span::styled(
            format!("{} ", icon),
            Style::default().fg(icon_color),
        ));
        spans.push(Span::styled(job.label.clone(), Style::default().fg(MUTED)));
        if !job.detail.is_empty() {
            spans.push(Span::styled(
                format!(" {}", job.detail),
                Style::default().fg(DIM),
            ));
        }
        if matches!(job.status, JobStatus::Running) {
            let secs = job.started_at.elapsed().as_secs();
            if secs >= 5 {
                let timer = if secs >= 60 {
                    format!(" {}m{}s", secs / 60, secs % 60)
                } else {
                    format!(" {}s", secs)
                };
                spans.push(Span::styled(timer, Style::default().fg(DIM)));
            }
        }
    }

    let line = Line::from(spans);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(INPUT_BG)),
        area,
    );
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let w = area.width as usize;

    let thinking_suffix = if app.thinking_level != "off" {
        format!(" ({}) ", app.thinking_level)
    } else {
        String::new()
    };

    // build right-side metrics first (fixed width, drives how much space the left side gets)
    let rate = app.avg_rate();
    let pct = app.context_pct();
    let used_k = app.context_used() / 1000;
    let limit_k = app.tokens.context_limit / 1000;

    let spark_width: usize = 16;
    let ctx_bar_width: usize = 8;
    let filled = ((pct * ctx_bar_width as f64).round() as usize).min(ctx_bar_width);
    let empty = ctx_bar_width - filled;
    let ctx_color = if pct > 0.8 {
        RED
    } else if pct > 0.6 {
        YELLOW
    } else {
        ACCENT
    };

    let rate_str = format!("{:.0} tok/s", rate);
    let cost_str = format!("${:.3}", app.cost_usd());
    let ctx_label = format!("{}k/{}k", used_k, limit_k);
    let ctx_pct = format!("{:.0}%", pct * 100.0);

    let right_len = spark_width
        + 1
        + rate_str.len()
        + 2
        + cost_str.len()
        + 2
        + ctx_label.len()
        + 2
        + ctx_bar_width
        + 2
        + ctx_pct.len();

    // fixed-width parts of the left side: "rum  " + "  " + model + thinking
    let model_part = app.model_name.len() + thinking_suffix.len();
    let fixed_left = 4 + 2 + model_part; // "rum" + "  " around cwd + model+thinking

    // available width for cwd + branch
    let budget = w.saturating_sub(fixed_left + right_len + 2); // +2 spacing margin

    let full_branch = app.git_branch.as_deref().unwrap_or("");
    let has_branch = !full_branch.is_empty();
    // branch overhead: "(" + branch + ")  " = branch_len + 4
    let branch_overhead = if has_branch { 4 } else { 0 };

    let full_cwd = &app.cwd;
    let full_left_content = full_cwd.len()
        + if has_branch {
            full_branch.len() + branch_overhead
        } else {
            0
        };

    // determine what to show, truncating to fit within budget
    let (display_cwd, display_branch): (String, Option<String>) = if full_left_content <= budget {
        // everything fits
        (
            full_cwd.clone(),
            if has_branch {
                Some(full_branch.to_string())
            } else {
                None
            },
        )
    } else if has_branch {
        // try truncating branch first (min 7 chars)
        let min_branch = 7usize;
        let cwd_plus_overhead = full_cwd.len() + branch_overhead;
        let branch_budget = budget.saturating_sub(cwd_plus_overhead);

        if branch_budget >= min_branch {
            // truncate branch to fit
            let trunc: String = full_branch.chars().take(branch_budget).collect();
            (full_cwd.clone(), Some(trunc))
        } else {
            // branch won't fit at min size with full cwd; try truncating cwd too
            let cwd_budget = budget.saturating_sub(min_branch + branch_overhead);
            if cwd_budget >= 8 {
                // truncate cwd from the start: .../tail
                let trunc_cwd = truncate_path_start(full_cwd, cwd_budget);
                let trunc_branch: String = full_branch.chars().take(min_branch).collect();
                (trunc_cwd, Some(trunc_branch))
            } else {
                // hide branch entirely, give all space to cwd
                if full_cwd.len() <= budget {
                    (full_cwd.clone(), None)
                } else if budget >= 8 {
                    (truncate_path_start(full_cwd, budget), None)
                } else {
                    (full_cwd.clone(), None)
                }
            }
        }
    } else {
        // no branch, just truncate cwd
        if budget >= 8 {
            (truncate_path_start(full_cwd, budget), None)
        } else {
            (full_cwd.clone(), None)
        }
    };

    let mut spans = vec![
        Span::styled(
            "rum",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {}  ", display_cwd), Style::default().fg(FG)),
    ];

    let branch_display_len = if let Some(ref b) = display_branch {
        spans.push(Span::styled("(", Style::default().fg(DIM)));
        spans.push(Span::styled(b.clone(), Style::default().fg(BRANCH_COLOR)));
        spans.push(Span::styled(")  ", Style::default().fg(DIM)));
        b.len() + 4
    } else {
        0
    };

    spans.push(Span::styled(
        app.model_name.clone(),
        Style::default().fg(MUTED),
    ));
    spans.push(Span::styled(
        thinking_suffix.clone(),
        Style::default().fg(MUTED),
    ));

    let left_len = 4 + display_cwd.len() + 4 + branch_display_len + model_part;

    let pad = w.saturating_sub(left_len + right_len);
    spans.push(Span::styled(" ".repeat(pad), Style::default()));

    // sparkline
    let spark_spans = render_colored_sparkline(&app.tokens.rate_samples, spark_width);
    spans.extend(spark_spans);
    spans.push(Span::styled(" ", Style::default()));

    // rate + cost
    spans.push(Span::styled(rate_str, Style::default().fg(MUTED)));
    spans.push(Span::styled("  ", Style::default()));
    spans.push(Span::styled(cost_str, Style::default().fg(DIM)));
    spans.push(Span::styled("  ", Style::default()));

    // context bar
    spans.push(Span::styled(ctx_label, Style::default().fg(DIM)));
    spans.push(Span::styled(" [", Style::default().fg(DIM)));
    spans.push(Span::styled(
        "\u{2588}".repeat(filled),
        Style::default().fg(ctx_color),
    ));
    spans.push(Span::styled(
        "\u{2591}".repeat(empty),
        Style::default().fg(Color::Rgb(30, 33, 40)),
    ));
    spans.push(Span::styled("] ", Style::default().fg(DIM)));
    spans.push(Span::styled(ctx_pct, Style::default().fg(ctx_color)));

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG)),
        area,
    );
}

// truncate a path from the start, keeping the tail: ".../parent/dir"
fn truncate_path_start(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        return path.to_string();
    }
    let prefix = ".../";
    let tail_budget = max_len.saturating_sub(prefix.len());
    if tail_budget == 0 {
        return path[path.len().saturating_sub(max_len)..].to_string();
    }
    // find a path separator within the tail portion
    let start = path.len().saturating_sub(tail_budget);
    if let Some(sep) = path[start..].find('/') {
        let clean_start = start + sep + 1;
        if clean_start < path.len() {
            return format!("{}{}", prefix, &path[clean_start..]);
        }
    }
    format!("{}{}", prefix, &path[start..])
}

// count visual lines after soft-wrapping text to fit within
// (max_width - prefix_width) columns
fn visual_line_count(text: &str, max_width: u16, prefix_width: usize) -> usize {
    let content_width = (max_width as usize).saturating_sub(prefix_width);
    if content_width == 0 {
        return 1;
    }
    let mut count = 0;
    for line in text.split('\n') {
        let w = UnicodeWidthStr::width(line);
        if w == 0 {
            count += 1;
        } else {
            count += (w + content_width - 1) / content_width;
        }
    }
    count.max(1)
}

// wrap a message into indented lines with a given text style.
// used for rendering the active message and queued messages above the input.
fn wrap_message_lines(
    text: &str,
    max_width: u16,
    text_style: Style,
    spinner: Option<&str>,
) -> Vec<Line<'static>> {
    let prefix_width = 2usize;
    let content_width = (max_width as usize).saturating_sub(prefix_width);
    if content_width == 0 {
        return vec![];
    }

    let mut lines = Vec::new();
    for logical_line in text.split('\n') {
        if logical_line.is_empty() {
            lines.push(Line::from(vec![Span::styled("  ", Style::default())]));
            continue;
        }
        let chars: Vec<char> = logical_line.chars().collect();
        let mut chunk_start = 0;
        while chunk_start < chars.len() {
            let mut w = 0;
            let mut chunk_end = chunk_start;
            while chunk_end < chars.len() {
                let cw = unicode_width::UnicodeWidthChar::width(chars[chunk_end]).unwrap_or(0);
                if w + cw > content_width {
                    break;
                }
                w += cw;
                chunk_end += 1;
            }
            if chunk_end == chunk_start {
                chunk_end = chunk_start + 1;
            }
            let chunk_text: String = chars[chunk_start..chunk_end].iter().collect();

            let prefix = if lines.is_empty() {
                if let Some(s) = spinner {
                    format!("{} ", s)
                } else {
                    "  ".to_string()
                }
            } else {
                "  ".to_string()
            };
            let prefix_style = if lines.is_empty() && spinner.is_some() {
                Style::default().fg(ACCENT)
            } else {
                Style::default()
            };

            lines.push(Line::from(vec![
                Span::styled(prefix, prefix_style),
                Span::styled(chunk_text, text_style),
            ]));
            chunk_start = chunk_end;
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(vec![Span::styled("  ", Style::default())]));
    }
    lines
}

// wrap input/message text into visual lines with a prefix on the first line.
// returns the visual lines and, if a cursor char position is given,
// the (visual_row, visual_col) of the cursor.
fn wrap_input_text(
    text: &str,
    max_width: u16,
    cursor_char_pos: Option<usize>,
    text_color: Color,
) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let prefix_width: usize = 2;
    let content_width = (max_width as usize).saturating_sub(prefix_width);
    if content_width == 0 {
        return (vec![], None);
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut cursor_visual: Option<(u16, u16)> = None;
    let mut char_offset: usize = 0; // running char position across the whole input

    for (line_idx, logical_line) in text.split('\n').enumerate() {
        let prefix = if lines.is_empty() { "\u{203a} " } else { "  " };

        if logical_line.is_empty() {
            // check if cursor is on this empty line
            if let Some(cp) = cursor_char_pos {
                if cp == char_offset {
                    cursor_visual = Some((lines.len() as u16, 0));
                }
            }
            lines.push(Line::from(vec![Span::styled(
                prefix,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )]));
            // advance past the newline separator (except after the last line)
            char_offset += 1; // for the '\n'
            continue;
        }

        let chars: Vec<char> = logical_line.chars().collect();
        let mut chunk_start: usize = 0; // index into chars[]

        while chunk_start < chars.len() {
            let row_prefix = if lines.is_empty() { "\u{203a} " } else { "  " };

            // find how many chars fit in content_width
            let mut w: usize = 0;
            let mut chunk_end = chunk_start;
            while chunk_end < chars.len() {
                let cw = unicode_width::UnicodeWidthChar::width(chars[chunk_end]).unwrap_or(0);
                if w + cw > content_width {
                    break;
                }
                w += cw;
                chunk_end += 1;
            }
            if chunk_end == chunk_start {
                // single char wider than content_width, force it
                chunk_end = chunk_start + 1;
            }

            let chunk_text: String = chars[chunk_start..chunk_end].iter().collect();

            // check if cursor falls within this visual row
            if let Some(cp) = cursor_char_pos {
                let abs_start = char_offset + chunk_start;
                let abs_end = char_offset + chunk_end;
                if cp >= abs_start && cp < abs_end {
                    let col_chars = &chars[chunk_start..(chunk_start + (cp - abs_start))];
                    let col_w: usize = col_chars
                        .iter()
                        .map(|c| unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0))
                        .sum();
                    cursor_visual = Some((lines.len() as u16, col_w as u16));
                }
                // cursor at end of last chunk of this logical line
                if cp == abs_end && chunk_end == chars.len() {
                    cursor_visual = Some((lines.len() as u16, w as u16));
                }
            }

            lines.push(Line::from(vec![
                Span::styled(
                    row_prefix,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(chunk_text, Style::default().fg(text_color)),
            ]));

            chunk_start = chunk_end;
        }

        // advance char_offset past this logical line + newline separator
        char_offset += chars.len();
        if line_idx < text.matches('\n').count() {
            char_offset += 1; // '\n'
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "\u{203a} ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )]));
        if cursor_char_pos == Some(0) {
            cursor_visual = Some((0, 0));
        }
    }

    (lines, cursor_visual)
}

fn render_message_area(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    suggestions: &[Suggestion],
    selected: Option<usize>,
) {
    if area.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    if app.is_running && suggestions.is_empty() {
        let spin = spinner_char(app.spin_frame);
        if let Some(ref msg) = app.current_message {
            lines.extend(wrap_message_lines(
                msg,
                area.width,
                Style::default().fg(ACCENT),
                Some(spin),
            ));
        }
        for qm in &app.queued_messages {
            match qm {
                QueuedItem::Message(s) => {
                    lines.extend(wrap_message_lines(
                        s,
                        area.width,
                        Style::default().fg(MUTED),
                        None,
                    ));
                }
                QueuedItem::Command(s) => {
                    lines.push(Line::from(vec![
                        Span::styled("  › ", Style::default().fg(DIM)),
                        Span::styled(s.clone(), Style::default().fg(DIM)),
                    ]));
                }
            }
        }
    } else if !suggestions.is_empty() {
        for (i, s) in suggestions.iter().enumerate() {
            let is_sel = selected == Some(i);
            let (cmd_style, desc_style) = if is_sel {
                (
                    Style::default().fg(BG).bg(ACCENT),
                    Style::default().fg(BG).bg(ACCENT),
                )
            } else {
                (Style::default().fg(ACCENT), Style::default().fg(MUTED))
            };
            let prefix = if is_sel { "\u{203a} " } else { "  " };
            let mut spans = vec![
                Span::styled(prefix, cmd_style),
                Span::styled(s.display.clone(), cmd_style),
            ];
            if !s.description.is_empty() {
                spans.push(Span::styled(format!("  {}", s.description), desc_style));
            }
            lines.push(Line::from(spans));
        }
    }

    let widget = Paragraph::new(lines).style(Style::default().bg(BG));
    frame.render_widget(widget, area);
}

fn render_input_area(frame: &mut Frame, app: &App, area: Rect) {
    let display = make_display_input(&app.input.text, &app.input.paste_chunks);
    let display_cursor = remap_cursor(&app.input.text, &app.input.paste_chunks, app.input.cursor_pos);
    let (input_lines, cursor_pos) = wrap_input_text(&display, area.width, Some(display_cursor), FG);

    let visible = area.height;
    let cursor_row = cursor_pos.map(|(r, _)| r).unwrap_or(0);
    let scroll: u16 = if cursor_row >= visible {
        cursor_row - visible + 1
    } else {
        0
    };

    let widget = Paragraph::new(input_lines)
        .style(Style::default().bg(INPUT_BG))
        .scroll((scroll, 0));
    frame.render_widget(widget, area);

    let prefix_width: u16 = 2;
    let (vrow, vcol) = cursor_pos.unwrap_or((0, 0));
    let cx = area.x + prefix_width + vcol;
    let cy = area.y + vrow.saturating_sub(scroll);
    frame.set_cursor_position((cx, cy));
}

fn is_compact_tool(item: &ActivityItem) -> bool {
    match item {
        ActivityItem::Tool(e) => !e.expanded,
        _ => false,
    }
}

fn render_activity(frame: &mut Frame, app: &mut App, area: Rect) {
    let w = area.width;
    let n = app.feed.items.len();

    // sync cache length with activity list
    app.feed.render_cache.truncate(n);
    while app.feed.render_cache.len() < n {
        app.feed.render_cache.push(CachedRender::default());
    }

    // re-render only stale items
    for idx in 0..n {
        let (content_len, expanded, status_tag) = match &app.feed.items[idx] {
            ActivityItem::Thinking(t) => (t.len(), false, 0u8),
            ActivityItem::Text(t) => (t.len(), false, 0u8),
            ActivityItem::UserMessage(t) => (t.len(), false, 0u8),
            ActivityItem::System(_, t) => (t.len(), false, 0u8),
            ActivityItem::Compact(CompactStatus::Running) => (app.spin_frame as usize, false, 0u8),
            ActivityItem::Compact(CompactStatus::Done(_)) => (0, false, 1u8),
            ActivityItem::Compact(CompactStatus::Cancelled) => (0, false, 2u8),
            ActivityItem::Tool(e) => {
                let st = match &e.status {
                    ToolStatus::Running => 0,
                    ToolStatus::Complete { .. } => 1,
                    ToolStatus::Error(_) => 2,
                };
                let len = e.arg.len()
                    + e.output.as_ref().map_or(0, |o| o.len())
                    + match &e.status {
                        // include elapsed seconds so the timer re-renders
                        ToolStatus::Running => e.started_at.elapsed().as_secs() as usize,
                        ToolStatus::Error(s) => s.len(),
                        _ => 0,
                    };
                (len, e.expanded, st)
            }
        };

        let stale = {
            let c = &app.feed.render_cache[idx];
            c.content_len != content_len
                || c.width != w
                || c.expanded != expanded
                || c.status_tag != status_tag
        };

        if stale {
            let item_lines = render_activity_item(&app.feed.items[idx], w, app.spin_frame);
            app.feed.render_cache[idx] = CachedRender {
                lines: item_lines,
                content_len,
                width: w,
                expanded,
                status_tag,
            };
        }
    }

    // compute total line count including inter-item spacing.
    // collapsed tool entries stack without blank lines between them.
    let mut total: usize = 0;
    for idx in 0..n {
        if idx > 0 {
            let both_collapsed_tools =
                is_compact_tool(&app.feed.items[idx - 1]) && is_compact_tool(&app.feed.items[idx]);
            if !both_collapsed_tools {
                total += 1;
            }
        }
        total += app.feed.render_cache[idx].lines.len();
    }

    let show_waiting = app.is_running && total == 0;
    if show_waiting {
        total = 1;
    }

    let total_lines = total as u16;
    let max_scroll = total_lines.saturating_sub(area.height);

    // re-engage auto-scroll when manual scrolling reaches the bottom
    if !app.feed.auto_scroll && app.feed.scroll_offset >= max_scroll {
        app.feed.scroll_offset = max_scroll;
        app.feed.auto_scroll = true;
    }

    let scroll = if app.feed.auto_scroll {
        app.feed.scroll_offset = max_scroll;
        max_scroll
    } else {
        app.feed.scroll_offset
    };

    // only build the visible window of lines
    let vp_start = scroll as usize;
    let vp_end = vp_start + area.height as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);

    if show_waiting {
        if vp_start == 0 {
            lines.push(Line::from(Span::styled(
                "  waiting...",
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            )));
        }
    } else {
        let mut cursor: usize = 0;
        for idx in 0..n {
            if cursor >= vp_end {
                break;
            }

            // blank line between items, except between consecutive collapsed tools
            if idx > 0 {
                let both_collapsed_tools =
                    is_compact_tool(&app.feed.items[idx - 1]) && is_compact_tool(&app.feed.items[idx]);
                if !both_collapsed_tools {
                    if cursor >= vp_start {
                        lines.push(Line::from(""));
                    }
                    cursor += 1;
                }
            }

            // item lines
            for line in &app.feed.render_cache[idx].lines {
                if cursor >= vp_start && cursor < vp_end {
                    lines.push(line.clone());
                }
                cursor += 1;
                if cursor >= vp_end {
                    break;
                }
            }
        }
    }

    let activity = Paragraph::new(lines).style(Style::default().bg(BG));
    frame.render_widget(activity, area);
}

// render a single activity item into lines (no inter-item spacing)
fn render_activity_item(item: &ActivityItem, w: u16, spin_frame: u64) -> Vec<Line<'static>> {
    match item {
        ActivityItem::Thinking(text) => {
            let para = last_paragraph(text);
            let style = Style::default().fg(DIM).add_modifier(Modifier::ITALIC);
            wrap_text_with_bar(para, w, style)
        }
        ActivityItem::Text(text) => {
            let mut md = crate::markdown::TuiMarkdownRenderer::new();
            let md_lines = md.render_lines(text);
            wrap_md_lines_with_bar(md_lines, w)
        }
        ActivityItem::UserMessage(text) => render_user_message(text, w),
        ActivityItem::Tool(entry) => {
            let mut lines = Vec::new();
            render_tool_entry(&mut lines, entry, w);
            lines
        }
        ActivityItem::System(kind, text) => render_system_msg(kind, text),
        ActivityItem::Compact(status) => render_compact_item(status, spin_frame),
    }
}

fn render_user_message(text: &str, w: u16) -> Vec<Line<'static>> {
    let content_width = w.saturating_sub(2) as usize;
    if content_width == 0 {
        return vec![];
    }
    let style = Style::default().fg(FG);
    let prefix_style = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for logical_line in text.split('\n') {
        if logical_line.is_empty() {
            let pfx = if lines.is_empty() { "\u{203a} " } else { "  " };
            lines.push(Line::from(Span::styled(pfx.to_string(), prefix_style)));
            continue;
        }
        let chars: Vec<char> = logical_line.chars().collect();
        let mut start = 0;
        while start < chars.len() {
            let pfx = if lines.is_empty() { "\u{203a} " } else { "  " };
            let mut w_used = 0usize;
            let mut end = start;
            while end < chars.len() {
                let cw = unicode_width::UnicodeWidthChar::width(chars[end]).unwrap_or(0);
                if w_used + cw > content_width {
                    break;
                }
                w_used += cw;
                end += 1;
            }
            if end == start {
                end = start + 1;
            }
            let chunk: String = chars[start..end].iter().collect();
            lines.push(Line::from(vec![
                Span::styled(pfx.to_string(), prefix_style),
                Span::styled(chunk, style),
            ]));
            start = end;
        }
    }
    lines
}

fn render_system_msg(kind: &SystemKind, text: &str) -> Vec<Line<'static>> {
    let (icon, color, bold) = match kind {
        SystemKind::Info => ("  ", MUTED, false),
        SystemKind::Success => ("  ✓ ", GREEN, false),
        SystemKind::Warning => ("  ⚠ ", YELLOW, false),
        SystemKind::Error => ("  ✗ ", RED, false),
        SystemKind::Update => ("  ↑ ", ACCENT, true),
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let prefix = if i == 0 { icon } else { "    " };
        let mut style = Style::default().fg(color);
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(line.to_string(), style),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

// comet scan: a bright head with a gradient tail bouncing across a fixed-width field.
// returns a fixed 10-char string, colored separately by the caller.
fn compact_comet_frame(spin: u64) -> String {
    const FIELD: usize = 10;
    const PERIOD: usize = (FIELD - 1) * 2; // 18-frame full bounce
    const TAIL: [char; 3] = ['▓', '▒', '░'];

    let t = (spin / 4) as usize % PERIOD;
    let going_right = t < FIELD;
    let pos = if going_right { t } else { PERIOD - t };

    let mut field = [' '; FIELD];
    field[pos] = '█';
    for (i, &tc) in TAIL.iter().enumerate() {
        let trail = if going_right {
            pos.checked_sub(i + 1)
        } else {
            (pos + i + 1 < FIELD).then_some(pos + i + 1)
        };
        if let Some(p) = trail {
            field[p] = tc;
        }
    }
    field.iter().collect()
}

fn render_compact_item(status: &CompactStatus, spin: u64) -> Vec<Line<'static>> {
    match status {
        CompactStatus::Running => {
            let anim = compact_comet_frame(spin);
            vec![Line::from(vec![
                Span::styled("  ◈  ", Style::default().fg(ACCENT)),
                Span::styled("compacting context  ", Style::default().fg(FG)),
                Span::styled("[", Style::default().fg(DIM)),
                Span::styled(anim, Style::default().fg(THINKING_COLOR)),
                Span::styled("]", Style::default().fg(DIM)),
            ])]
        }
        CompactStatus::Done(msg) => {
            vec![Line::from(vec![
                Span::styled("  ✓  ", Style::default().fg(GREEN)),
                Span::styled(msg.clone(), Style::default().fg(FG)),
            ])]
        }
        CompactStatus::Cancelled => {
            vec![Line::from(vec![
                Span::styled("  ✕  ", Style::default().fg(MUTED)),
                Span::styled("compaction cancelled", Style::default().fg(MUTED)),
            ])]
        }
    }
}

fn render_tool_entry(lines: &mut Vec<Line<'static>>, entry: &ToolEntry, _w: u16) {
    let label = capitalize_tool(&entry.name);
    let display_arg = if entry.expanded {
        entry.arg.clone()
    } else if entry.arg.len() > 80 {
        format!("{}…", &entry.arg[..79])
    } else {
        entry.arg.clone()
    };

    match &entry.status {
        ToolStatus::Running => {
            let elapsed = entry.started_at.elapsed();
            let secs = elapsed.as_secs();

            let mut spans = vec![
                Span::styled("\u{25cc} ", Style::default().fg(YELLOW)),
                Span::styled(label.to_string(), Style::default().fg(YELLOW)),
            ];
            if secs >= 5 {
                let timer = if secs >= 60 {
                    format!("{}m{}s", secs / 60, secs % 60)
                } else {
                    format!("{}s", secs)
                };
                spans.push(Span::styled(
                    format!(" {}", timer),
                    Style::default().fg(DIM),
                ));
            }
            if !display_arg.is_empty() {
                spans.push(Span::styled(
                    format!(" {}", display_arg),
                    Style::default().fg(MUTED),
                ));
            }
            lines.push(tool_line(spans));

            // show streaming output while running, respecting the expanded toggle
            if entry.expanded {
                if let Some(ref output) = entry.output {
                    let out_lines: Vec<&str> = output.lines().collect();
                    let start = out_lines.len().saturating_sub(8);
                    for ol in &out_lines[start..] {
                        lines.push(tool_line(vec![
                            Span::styled("    ", Style::default()),
                            Span::styled((*ol).to_string(), Style::default().fg(DIM)),
                        ]));
                    }
                }
            }
        }
        ToolStatus::Complete { exit_code } => {
            let mut spans = vec![];

            // success/failure indicator — bash shows exit code, others just a checkmark
            if entry.name == "bash" {
                match exit_code {
                    Some(0) | None => {
                        spans.push(Span::styled("\u{2713} ", Style::default().fg(GREEN)));
                    }
                    Some(code) => {
                        spans.push(Span::styled(
                            format!("\u{2717}({}) ", code),
                            Style::default().fg(RED),
                        ));
                    }
                }
            } else {
                spans.push(Span::styled("\u{2713} ", Style::default().fg(GREEN)));
            }

            // tool name in accent, argument in muted
            spans.push(Span::styled(label.to_string(), Style::default().fg(ACCENT)));
            if !display_arg.is_empty() {
                spans.push(Span::styled(
                    format!(" {}", display_arg),
                    Style::default().fg(MUTED),
                ));
            }

            // diff stats
            if let Some(ref diff) = entry.diff {
                if diff.stat.additions > 0 {
                    spans.push(Span::styled(
                        format!(" +{}", diff.stat.additions),
                        Style::default().fg(GREEN),
                    ));
                }
                if diff.stat.deletions > 0 {
                    spans.push(Span::styled(
                        format!(" -{}", diff.stat.deletions),
                        Style::default().fg(RED),
                    ));
                }
            }

            lines.push(tool_line(spans));

            if entry.expanded {
                // tool output (first few lines, indented)
                if let Some(ref output) = entry.output {
                    let display = strip_exit_prefix(output);
                    let out_lines: Vec<&str> = display.lines().take(8).collect();
                    for ol in &out_lines {
                        lines.push(tool_line(vec![
                            Span::styled("    ", Style::default()),
                            Span::styled((*ol).to_string(), Style::default().fg(DIM)),
                        ]));
                    }
                    let total_lines = display.lines().count();
                    if total_lines > 8 {
                        lines.push(tool_line(vec![Span::styled(
                            format!("    ...{} more lines", total_lines - 8),
                            Style::default().fg(DIM),
                        )]));
                    }
                }

                // diff lines
                if let Some(ref diff) = entry.diff {
                    lines.extend(build_diff_lines(diff));
                }
            }
        }
        ToolStatus::Error(e) => {
            let short = if e.len() > 80 {
                format!("{}...", &e[..77])
            } else {
                e.clone()
            };
            lines.push(tool_line(vec![
                Span::styled("\u{2717} ", Style::default().fg(RED)),
                Span::styled(
                    format!("{} ", label),
                    Style::default().fg(RED).add_modifier(Modifier::BOLD),
                ),
                Span::styled(short, Style::default().fg(RED)),
            ]));
        }
    }
}

// render a sparkline where each bar is colored by the dominant token type
// in that bucket. colors: text=accent, thinking=purple, tool=cyan, input=blue.
fn render_colored_sparkline(samples: &[TokenBucket], width: usize) -> Vec<Span<'static>> {
    let blocks = [
        ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
        '\u{2588}',
    ];

    if samples.is_empty() {
        return vec![Span::styled(" ".repeat(width), Style::default())];
    }

    let start = samples.len().saturating_sub(width);
    let window = &samples[start..];
    let max = window.iter().map(|b| b.total()).max().unwrap_or(1).max(1);

    let mut spans: Vec<Span<'static>> = Vec::new();

    // leading padding
    let pad_count = width.saturating_sub(window.len());
    if pad_count > 0 {
        spans.push(Span::styled(" ".repeat(pad_count), Style::default()));
    }

    // batch consecutive chars with the same color into a single span
    let mut run_color: Option<Color> = None;
    let mut run_chars = String::new();

    for bucket in window {
        let total = bucket.total();
        let idx = ((total as f64 / max as f64) * 8.0).round() as usize;
        let ch = blocks[idx.min(8)];

        // pick color from the dominant token type
        let color = if total == 0 {
            DIM
        } else {
            dominant_color(bucket)
        };

        if run_color == Some(color) {
            run_chars.push(ch);
        } else {
            if !run_chars.is_empty() {
                spans.push(Span::styled(
                    run_chars.clone(),
                    Style::default().fg(run_color.unwrap_or(DIM)),
                ));
            }
            run_chars.clear();
            run_chars.push(ch);
            run_color = Some(color);
        }
    }
    if !run_chars.is_empty() {
        spans.push(Span::styled(
            run_chars,
            Style::default().fg(run_color.unwrap_or(DIM)),
        ));
    }

    spans
}

// pick the color of whichever token type contributed the most to this bucket
fn dominant_color(bucket: &TokenBucket) -> Color {
    let pairs = [
        (bucket.text, ACCENT),
        (bucket.thinking, THINKING_COLOR),
        (bucket.tool, TOOL_COLOR),
    ];
    pairs
        .iter()
        .max_by_key(|(count, _)| *count)
        .map(|(_, color)| *color)
        .unwrap_or(ACCENT)
}

fn build_diff_lines(diff: &DiffInfo) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for hunk in &diff.hunks {
        for dl in &hunk.lines {
            let (prefix, color) = match dl.tag {
                DiffLineTag::Equal => (" ", DIM),
                DiffLineTag::Insert => ("+", GREEN),
                DiffLineTag::Delete => ("-", RED),
            };
            let content = dl.content.trim_end_matches('\n').to_string();
            out.push(tool_line(vec![
                Span::styled(format!("  {}", prefix), Style::default().fg(color)),
                Span::styled(content, Style::default().fg(color)),
            ]));
        }
    }
    out
}

pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    pub fn new() -> Result<Self, io::Error> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        execute!(stdout, EnableBracketedPaste)?;
        execute!(stdout, EnableMouseCapture)?;
        // enable kitty keyboard protocol so terminals report modifier
        // keys on Enter (needed for shift+enter newline detection)
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    pub fn restore(&mut self) -> Result<(), io::Error> {
        let _ = execute!(self.terminal.backend_mut(), PopKeyboardEnhancementFlags);
        let _ = execute!(self.terminal.backend_mut(), DisableBracketedPaste);
        let _ = execute!(self.terminal.backend_mut(), DisableMouseCapture);
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        Ok(())
    }

    pub fn draw(&mut self, app: &mut App) -> Result<(), io::Error> {
        self.terminal.draw(|frame| render(frame, app))?;
        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub enum InputAction {
    Submit(String),
    Cancel,
    Quit,
    ScrollUp,
    ScrollDown,
    ToggleDiff,
    PasteFromClipboard,
    None,
}

pub fn handle_key_event(key: KeyEvent, app: &mut App) -> InputAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let super_key = key.modifiers.contains(KeyModifiers::SUPER);

    // ctrl+e: toggle editor mode (only when no other modifiers)
    if ctrl && !super_key && !alt && key.code == KeyCode::Char('e') {
        app.editor.mode = match app.editor.mode {
            ViewMode::Chat => ViewMode::Editor,
            ViewMode::Editor => {
                app.editor.search = None;
                ViewMode::Chat
            }
        };
        return InputAction::None;
    }

    // ctrl+p: file finder (enters editor if not already)
    if ctrl && key.code == KeyCode::Char('p') {
        app.editor.mode = ViewMode::Editor;
        app.editor.search = Some(SearchState::new(SearchMode::Files));
        app.update_file_search();
        return InputAction::None;
    }

    // ctrl+f: toggle follow mode (enters editor if not already)
    if ctrl && key.code == KeyCode::Char('f') {
        if app.editor.mode == ViewMode::Chat {
            // from chat mode: always enable follow and switch to editor
            app.editor.mode = ViewMode::Editor;
            app.editor.follow_mode = true;
        } else {
            // from editor mode: toggle
            app.editor.follow_mode = !app.editor.follow_mode;
        }
        if app.editor.follow_mode && !app.editor.agent_edits.is_empty() {
            app.editor.agent_edit_index = app.editor.agent_edits.len() - 1;
            app.open_agent_edit(app.editor.agent_edit_index);
        } else if !app.editor.follow_mode {
            app.editor.diff_markers.clear();
        }
        return InputAction::None;
    }

    // dispatch to editor-specific handler when in editor mode
    if app.editor.mode == ViewMode::Editor {
        return handle_editor_key(key, app, ctrl, alt, shift, super_key);
    }

    // ctrl+c: cancel if running, quit if idle with empty input, clear input otherwise
    if ctrl && key.code == KeyCode::Char('c') {
        if app.is_running {
            return InputAction::Cancel;
        }
        if app.input.text.is_empty() {
            return InputAction::Quit;
        }
        app.input.text.clear();
        app.input.cursor_pos = 0;
        app.input.paste_chunks.clear();
        app.reset_slash_completion();
        return InputAction::None;
    }

    // super+v (Cmd+V on macOS) or ctrl+shift+v: explicit clipboard paste for images
    if (super_key && key.code == KeyCode::Char('v'))
        || (ctrl && shift && key.code == KeyCode::Char('v'))
    {
        return InputAction::PasteFromClipboard;
    }

    // escape: cancel if running, clear input if non-empty, otherwise no-op
    if key.code == KeyCode::Esc {
        if app.is_running {
            return InputAction::Cancel;
        }
        if !app.input.text.is_empty() {
            app.input.text.clear();
            app.input.cursor_pos = 0;
            app.input.paste_chunks.clear();
            app.reset_slash_completion();
        }
        return InputAction::None;
    }

    // page scroll (always available)
    if key.code == KeyCode::PageUp {
        app.feed.auto_scroll = false;
        app.feed.scroll_offset = app.feed.scroll_offset.saturating_sub(10);
        return InputAction::None;
    }
    if key.code == KeyCode::PageDown {
        app.feed.scroll_offset = app.feed.scroll_offset.saturating_add(10);
        return InputAction::None;
    }

    match key.code {
        KeyCode::Enter => {
            if shift || alt || ctrl {
                app.reset_slash_completion();
                let bp = app.cursor_byte_pos();
                app.input.text.insert(bp, '\n');
                app.input.cursor_pos += 1;
            } else if !app.input.text.is_empty() {
                // slash commands and ! bash commands are dispatched immediately even during a running turn
                if app.is_running && !app.input.text.starts_with('/') && !app.input.text.starts_with('!') {
                    app.reset_slash_completion();
                    app.queue_message();
                } else {
                    app.reset_slash_completion();
                    let msg = app.expand_input();
                    app.input.text.clear();
                    app.input.cursor_pos = 0;
                    app.input.paste_chunks.clear();
                    return InputAction::Submit(msg);
                }
            }
        }
        KeyCode::Tab | KeyCode::BackTab => {
            if app.input.text.starts_with('/') {
                let forward = key.code == KeyCode::Tab;
                // snapshot the input before the first Tab so cycling doesn't narrow the set
                if app.input.slash_prefix.is_none() {
                    app.input.slash_prefix = Some(app.input.text.clone());
                }
                let prefix = app.input.slash_prefix.clone().unwrap();
                let suggestions = slash_suggestions(&prefix);
                let count = suggestions.len();
                if count == 0 {
                    return InputAction::None;
                }
                let next = if forward {
                    match app.input.slash_selected {
                        None => 0,
                        Some(i) => (i + 1) % count,
                    }
                } else {
                    match app.input.slash_selected {
                        None | Some(0) => count - 1,
                        Some(i) => i - 1,
                    }
                };
                app.input.slash_selected = Some(next);
                let completion = suggestions[next].completion.clone();
                // when a command name is completed (trailing space), reset the prefix
                // so the next Tab opens a fresh arg-completion session
                let is_cmd_completion = completion.ends_with(' ');
                app.input.text = completion;
                app.input.cursor_pos = app.char_count();
                if is_cmd_completion {
                    app.input.slash_prefix = Some(app.input.text.clone());
                    app.input.slash_selected = None;
                }
            }
        }
        KeyCode::Backspace => {
            app.reset_slash_completion();
            if super_key {
                app.delete_to_line_start();
            } else if alt || ctrl {
                app.delete_word_backward();
            } else if app.input.cursor_pos > 0 {
                let bp = app.cursor_byte_pos();
                let prev = app.input.text[..bp].char_indices().last().map(|(i, _)| i);
                if let Some(pb) = prev {
                    app.input.text.remove(pb);
                    app.input.cursor_pos -= 1;
                }
            }
        }
        KeyCode::Delete => {
            app.reset_slash_completion();
            if app.input.cursor_pos < app.char_count() {
                let bp = app.cursor_byte_pos();
                app.input.text.remove(bp);
            }
        }
        KeyCode::Left => {
            if super_key {
                app.move_line_start();
            } else if alt {
                app.move_word_left();
            } else {
                app.input.cursor_pos = app.input.cursor_pos.saturating_sub(1);
            }
        }
        KeyCode::Right => {
            if super_key {
                app.move_line_end();
            } else if alt {
                app.move_word_right();
            } else if app.input.cursor_pos < app.char_count() {
                app.input.cursor_pos += 1;
            }
        }
        KeyCode::Up => {
            if shift {
                app.feed.auto_scroll = false;
                app.feed.scroll_offset = app.feed.scroll_offset.saturating_sub(1);
            } else if app.input_line_count() > 1 && app.move_cursor_up() {
                // moved within multi-line input
            } else if app.input.text.is_empty() && app.pop_queued_message() {
                // popped last queued message into input
            } else if app.navigate_history_up() {
                // navigated to an older history entry
            } else {
                return InputAction::ScrollUp;
            }
        }
        KeyCode::Down => {
            if shift {
                app.feed.scroll_offset = app.feed.scroll_offset.saturating_add(1);
            } else if app.input_line_count() > 1 && app.move_cursor_down() {
                // moved within multi-line input
            } else if app.navigate_history_down() {
                // navigated to a newer history entry or back to draft
            } else {
                return InputAction::ScrollDown;
            }
        }
        KeyCode::Home => {
            app.move_line_start();
        }
        KeyCode::End => {
            app.move_line_end();
        }
        KeyCode::Char(c) => {
            if ctrl {
                match c {
                    'a' => app.move_line_start(),
                    'e' => app.move_line_end(),
                    'u' => {
                        app.reset_slash_completion();
                        app.delete_to_line_start();
                    }
                    'k' => {
                        app.reset_slash_completion();
                        app.delete_to_line_end();
                    }
                    'w' => {
                        app.reset_slash_completion();
                        app.delete_word_backward();
                    }
                    'j' => {
                        // ctrl+j inserts newline (traditional unix LF)
                        app.reset_slash_completion();
                        let bp = app.cursor_byte_pos();
                        app.input.text.insert(bp, '\n');
                        app.input.cursor_pos += 1;
                    }
                    'o' => return InputAction::ToggleDiff,
                    _ => {}
                }
            } else if alt {
                // terminals without kitty protocol send Alt+Left/Right
                // as ESC b / ESC f, which arrive as Alt+Char('b')/'f'
                match c {
                    'b' => app.move_word_left(),
                    'f' => app.move_word_right(),
                    'd' => {
                        // alt+d: delete word forward
                        app.reset_slash_completion();
                        let start = app.input.cursor_pos;
                        app.move_word_right();
                        let end = app.input.cursor_pos;
                        if end > start {
                            let byte_start = app
                                .input.text
                                .char_indices()
                                .nth(start)
                                .map(|(i, _)| i)
                                .unwrap_or(app.input.text.len());
                            let byte_end = app
                                .input.text
                                .char_indices()
                                .nth(end)
                                .map(|(i, _)| i)
                                .unwrap_or(app.input.text.len());
                            app.input.text.replace_range(byte_start..byte_end, "");
                            app.input.cursor_pos = start;
                        }
                    }
                    _ => {}
                }
            } else {
                app.reset_slash_completion();
                let bp = app.cursor_byte_pos();
                app.input.text.insert(bp, c);
                app.input.cursor_pos += 1;
            }
        }
        _ => {}
    }

    InputAction::None
}
