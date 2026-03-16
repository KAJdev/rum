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

pub(crate) const BG: Color = Color::Rgb(11, 14, 20);
pub(crate) const FG: Color = Color::Rgb(191, 189, 182);
pub(crate) const MUTED: Color = Color::Rgb(108, 115, 128);
pub(crate) const ACCENT: Color = Color::Rgb(230, 180, 80);
pub(crate) const GREEN: Color = Color::Rgb(170, 217, 76);
pub(crate) const RED: Color = Color::Rgb(240, 113, 120);
pub(crate) const YELLOW: Color = Color::Rgb(255, 180, 84);
pub(crate) const DIM: Color = Color::Rgb(86, 91, 102);
pub(crate) const SURFACE: Color = Color::Rgb(22, 27, 36);
pub(crate) const BAR_COLOR: Color = Color::Rgb(60, 65, 75);

pub(crate) const THINKING_COLOR: Color = Color::Rgb(180, 140, 255);
pub(crate) const TOOL_COLOR: Color = Color::Rgb(100, 200, 220);
pub(crate) const INPUT_BG: Color = Color::Rgb(16, 20, 28);
pub(crate) const BRANCH_COLOR: Color = Color::Rgb(120, 190, 148);
pub(crate) const SIDEBAR_WIDTH: u16 = 30;

pub(crate) const DEFAULT_CONTEXT: u32 = 200_000;

pub(crate) struct SlashDef {
    pub(crate) name: &'static str,
    pub(crate) args: &'static str,
    pub(crate) description: &'static str,
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

pub(crate) struct Suggestion {
    pub(crate) display: String,
    pub(crate) description: String,
    // full string placed in the input field when this suggestion is applied
    pub(crate) completion: String,
}

pub(crate) fn slash_suggestions(input: &str) -> Vec<Suggestion> {
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
    pub(crate) text: u32,
    pub(crate) thinking: u32,
    pub(crate) tool: u32,
}

impl TokenBucket {
    pub(crate) fn total(&self) -> u64 {
        // input tokens are excluded: they're a usage count reported once
        // per api call, not streaming throughput
        (self.text + self.thinking + self.tool) as u64
    }
}

// the bar prefix string and its display width
pub(crate) const BAR_STR: &str = "\u{2502} ";
pub(crate) const BAR_WIDTH: u16 = 2;

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
    pub(crate) name: String,
    // argument portion (path, command, etc.) shown after the tool label
    pub(crate) arg: String,
    pub(crate) status: ToolStatus,
    pub(crate) diff: Option<DiffInfo>,
    pub(crate) output: Option<String>,
    pub(crate) expanded: bool,
    pub(crate) started_at: Instant,
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
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) content_len: usize,
    pub(crate) width: u16,
    pub(crate) expanded: bool,
    pub(crate) status_tag: u8,
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

    pub(crate) fn context_used(&self) -> u32 {
        self.tokens.last_input + self.tokens.last_output
    }

    pub(crate) fn context_pct(&self) -> f64 {
        if self.tokens.context_limit == 0 {
            return 0.0;
        }
        (self.context_used() as f64 / self.tokens.context_limit as f64).min(1.0)
    }

    pub(crate) fn cost_usd(&self) -> f64 {
        let p = crate::config::model_pricing(&self.model_name);
        // cache writes cost 1.25x, cache reads cost 0.1x base input price
        self.tokens.total_input as f64 * p.input / 1_000_000.0
            + self.tokens.total_cache_creation as f64 * p.input * 1.25 / 1_000_000.0
            + self.tokens.total_cache_read as f64 * p.input * 0.1 / 1_000_000.0
            + self.tokens.total_output as f64 * p.output / 1_000_000.0
    }

    pub(crate) fn avg_rate(&self) -> f64 {
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
    pub(crate) fn input_visual_line_count(&self) -> usize {
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

pub(crate) const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// paste placeholder helpers — private use area \u{E000}..\u{E00F}
pub(crate) fn is_paste_placeholder(c: char) -> bool {
    (c as u32) >= 0xE000 && (c as u32) <= 0xE00F
}

pub(crate) fn paste_placeholder_index(c: char) -> usize {
    ((c as u32) - 0xE000) as usize
}

pub(crate) fn paste_display_str(chunk: &str) -> String {
    let lines = chunk.lines().count().max(1);
    if lines > 1 {
        format!("[{lines} lines]")
    } else {
        format!("[{} chars]", chunk.chars().count())
    }
}

// write paste content to a uniquely named temp file and return its path
// expand paste placeholders to their display summaries (e.g. "[3 lines]")
pub(crate) fn make_display_input(input: &str, paste_chunks: &[String]) -> String {
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
pub(crate) fn remap_cursor(input: &str, paste_chunks: &[String], real_pos: usize) -> usize {
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
pub(crate) fn spinner_char(frame: u64) -> &'static str {
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

pub(crate) fn capitalize_tool(name: &str) -> &str {
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
pub(crate) fn last_paragraph(text: &str) -> &str {
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
pub(crate) fn strip_exit_prefix(s: &str) -> &str {
    if s.starts_with("[exit code: ") {
        if let Some(idx) = s.find("]\n") {
            return &s[idx + 2..];
        }
    }
    s
}

// indent for tool lines (no bar)
pub(crate) fn tool_line(spans: Vec<Span<'static>>) -> Line<'static> {
    let mut all = vec![Span::styled("  ", Style::default())];
    all.extend(spans);
    Line::from(all)
}

// wrap a plain text string to fit within `max_width` characters,
// returning one Line per visual row. each line gets the bar prefix.
pub(crate) fn wrap_text_with_bar(text: &str, max_width: u16, style: Style) -> Vec<Line<'static>> {
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
pub(crate) fn wrap_md_lines_with_bar(md_lines: Vec<Line<'static>>, max_width: u16) -> Vec<Line<'static>> {
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
        self.terminal.draw(|frame| crate::render::render(frame, app))?;
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
