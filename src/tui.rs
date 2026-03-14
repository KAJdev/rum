use crossterm::{
    event::{
        KeyCode, KeyEvent, KeyModifiers,
        KeyboardEnhancementFlags, PushKeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        EnableBracketedPaste, DisableBracketedPaste,
        EnableMouseCapture, DisableMouseCapture,
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
use crate::agent::AgentEvent;
use crate::api::{ContentBlock, Message, MessageContent};
use crate::tools::{DiffInfo, DiffLineTag, ToolResult};

const BG: Color = Color::Rgb(11, 14, 20);
const FG: Color = Color::Rgb(191, 189, 182);
const MUTED: Color = Color::Rgb(108, 115, 128);
const ACCENT: Color = Color::Rgb(230, 180, 80);
const GREEN: Color = Color::Rgb(170, 217, 76);
const RED: Color = Color::Rgb(240, 113, 120);
const YELLOW: Color = Color::Rgb(255, 180, 84);
const DIM: Color = Color::Rgb(86, 91, 102);
const BAR_COLOR: Color = Color::Rgb(60, 65, 75);

const THINKING_COLOR: Color = Color::Rgb(180, 140, 255);
const TOOL_COLOR: Color = Color::Rgb(100, 200, 220);
const INPUT_BG: Color = Color::Rgb(16, 20, 28);
const BRANCH_COLOR: Color = Color::Rgb(120, 190, 148);

const DEFAULT_CONTEXT: u32 = 200_000;

struct SlashDef {
    name: &'static str,
    args: &'static str,
    description: &'static str,
}

const SLASH_COMMANDS: &[SlashDef] = &[
    SlashDef { name: "/model", args: "[name]", description: "Switch model" },
    SlashDef { name: "/thinking", args: "[level]", description: "Set thinking level" },
    SlashDef { name: "/new", args: "", description: "Start new conversation" },
    SlashDef { name: "/compact", args: "", description: "Summarize context to free up space" },
    SlashDef { name: "/cd", args: "<path>", description: "Change working directory" },
    SlashDef { name: "/login", args: "", description: "Log in with Anthropic OAuth" },
    SlashDef { name: "/logout", args: "", description: "Log out" },
    SlashDef { name: "/help", args: "", description: "Show available commands" },
    SlashDef { name: "/quit", args: "", description: "Quit" },
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
        return SLASH_COMMANDS.iter()
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
        "/model" => crate::config::ANTHROPIC_MODELS.iter()
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
        "/thinking" => crate::config::THINKING_LEVELS.iter()
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
struct TokenBucket {
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
enum ActivityItem {
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
enum CompactStatus {
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
enum QueuedItem {
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
    Show { id: u64 },
    Update { id: u64, detail: String },
    Complete { id: u64, status: JobStatus, summary: String },
    // silently remove a job without inserting a message
    Dismiss { id: u64 },
}

#[derive(Debug, Clone)]
struct ToolEntry {
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
enum ToolStatus {
    Running,
    // exit_code is Some for bash commands
    Complete { exit_code: Option<i32> },
    Error(String),
}

// cached rendered lines for a single activity item.
// invalidated when content length, terminal width, expand state, or
// tool status changes.
#[derive(Clone, Default)]
struct CachedRender {
    lines: Vec<Line<'static>>,
    content_len: usize,
    width: u16,
    expanded: bool,
    status_tag: u8,
}

pub struct App {
    input: String,
    cursor_pos: usize,
    activity: Vec<ActivityItem>,
    // index into activity where the current login flow started; used to
    // wipe login messages on success so only the result remains
    login_activity_start: Option<usize>,
    pub current_message: Option<String>,
    queued_messages: Vec<QueuedItem>,
    // summed across all api calls (for cost calculation)
    total_input: u32,
    total_output: u32,
    total_cache_read: u32,
    total_cache_creation: u32,
    // from the most recent api call (for context window display).
    // each call's input_tokens already includes the full conversation
    // history, so these reflect actual context window usage.
    last_input: u32,
    last_output: u32,
    context_limit: u32,
    rate_samples: Vec<TokenBucket>,
    rate_bucket: TokenBucket,
    last_sample: Instant,
    pub is_running: bool,
    pub should_quit: bool,
    pub scroll_offset: u16,
    // when true, viewport follows new content to the bottom
    pub auto_scroll: bool,
    pub diffs_expanded: bool,
    // set after TurnComplete so the next text/thinking block always starts fresh
    // rather than appending to the last item from the previous turn
    new_turn: bool,
    // multi-line or long pastes are stored here; the input string holds a single
    // placeholder char (private use area \u{E000}+index) per chunk
    paste_chunks: Vec<String>,
    model_name: String,
    thinking_level: String,
    cwd: String,
    git_branch: Option<String>,
    current_tool_input: String,
    start_time: Option<Instant>,
    // cached terminal width for manual line wrapping
    term_width: u16,
    // animation frame counter, incremented every render tick
    spin_frame: u64,
    // per-item cache of rendered lines for the activity feed
    activity_render_cache: Vec<CachedRender>,
    // slash command tab-completion state
    slash_prefix: Option<String>,
    slash_selected: Option<usize>,
    // input history: previously sent messages, navigated with up/down
    input_history: Vec<String>,
    input_history_pos: Option<usize>,
    input_draft: String,
    // set when PasteFromClipboard already handled an image this tick,
    // so the subsequent Event::Paste("") doesn't duplicate it
    pub paste_handled: bool,
    // channel for injecting queued messages into a running turn
    inject_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    // background jobs shown in the bottom status bar
    pub background_jobs: Vec<BackgroundJob>,
    next_job_id: u64,
    // set when a git push is detected; main.rs reads and clears this to spawn CI watch
    pub pending_ci_watch: Option<String>,
}

impl App {
    pub fn new(model_name: &str, thinking_level: &str, cwd: &str) -> Self {
        let context_limit = guess_context_limit(model_name);
        let term_width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
        Self {
            input: String::new(),
            cursor_pos: 0,
            activity: Vec::new(),
            login_activity_start: None,
            current_message: None,
            queued_messages: Vec::new(),
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
            is_running: false,
            should_quit: false,
            scroll_offset: 0,
            auto_scroll: true,
            diffs_expanded: true,
            new_turn: false,
            paste_chunks: Vec::new(),
            model_name: model_name.to_string(),
            thinking_level: thinking_level.to_string(),
            cwd: cwd.to_string(),
            git_branch: detect_git_branch(cwd),
            current_tool_input: String::new(),
            start_time: None,
            term_width,
            spin_frame: 0,
            activity_render_cache: Vec::new(),
            slash_prefix: None,
            slash_selected: None,
            input_history: Vec::new(),
            input_history_pos: None,
            input_draft: String::new(),
            paste_handled: false,
            inject_tx: None,
            background_jobs: Vec::new(),
            next_job_id: 0,
            pending_ci_watch: None,
        }
    }

    pub fn set_inject_tx(&mut self, tx: tokio::sync::mpsc::UnboundedSender<String>) {
        self.inject_tx = Some(tx);
    }

    pub fn tick_rate(&mut self) {
        self.spin_frame = self.spin_frame.wrapping_add(1);
        let now = Instant::now();
        if now.duration_since(self.last_sample).as_millis() >= 2000 {
            self.rate_samples.push(self.rate_bucket.clone());
            self.rate_bucket = TokenBucket::default();
            self.last_sample = now;
            if self.rate_samples.len() > 120 {
                self.rate_samples.remove(0);
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
                self.rate_bucket.thinking += approx;
                if !self.new_turn {
                    if let Some(ActivityItem::Thinking(ref mut s)) = self.activity.last_mut() {
                        s.push_str(&t);
                        return;
                    }
                }
                self.new_turn = false;
                self.activity.push(ActivityItem::Thinking(t));
            }
            AgentEvent::Text(t) => {
                let approx = (t.len() as u32 / 4).max(1);
                self.rate_bucket.text += approx;
                if !self.new_turn {
                    if let Some(ActivityItem::Text(ref mut s)) = self.activity.last_mut() {
                        s.push_str(&t);
                        return;
                    }
                }
                self.new_turn = false;
                self.activity.push(ActivityItem::Text(t));
            }
            AgentEvent::ToolStart { id: _, name } => {
                self.current_tool_input.clear();
                self.activity.push(ActivityItem::Tool(ToolEntry {
                    name: name.clone(),
                    arg: String::new(),
                    status: ToolStatus::Running,
                    diff: None,
                    output: None,
                    expanded: self.diffs_expanded,
                    started_at: Instant::now(),
                }));
            }
            AgentEvent::ToolInputDelta(json) => {
                let approx = (json.len() as u32 / 4).max(1);
                self.rate_bucket.tool += approx;
                self.current_tool_input.push_str(&json);
                if let Some(ActivityItem::Tool(ref mut entry)) = self.activity.last_mut() {
                    if let Ok(partial) =
                        serde_json::from_str::<serde_json::Value>(&self.current_tool_input)
                    {
                        entry.arg = extract_tool_arg(&entry.name, &partial);
                    }
                }
            }
            AgentEvent::ToolOutputDelta { id: _, text } => {
                if let Some(ActivityItem::Tool(ref mut entry)) = self.activity.iter_mut().rev()
                    .find(|item| matches!(item, ActivityItem::Tool(e) if matches!(e.status, ToolStatus::Running)))
                {
                    let buf = entry.output.get_or_insert_with(String::new);
                    // cap the display buffer to keep re-renders cheap
                    if buf.len() < 10_000 {
                        buf.push_str(&strip_ansi(&text));
                    }
                }
            }
            AgentEvent::ToolComplete { id: _, name, result } => {
                if let Some(ActivityItem::Tool(ref mut entry)) = self.activity.iter_mut().rev()
                    .find(|item| matches!(item, ActivityItem::Tool(e) if matches!(e.status, ToolStatus::Running)))
                {
                    match &result {
                        ToolResult::Success { output, diff } => {
                            let exit_code = if name == "bash" {
                                parse_exit_code(output)
                            } else {
                                None
                            };

                            if let Some(d) = diff {
                                entry.arg = d.path.clone();
                                entry.diff = Some(d.clone());
                            }

                            entry.expanded = self.diffs_expanded;

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
            }
            AgentEvent::TokenUsage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } => {
                self.total_input += input_tokens;
                self.total_output += output_tokens;
                self.total_cache_read += cache_read_tokens;
                self.total_cache_creation += cache_creation_tokens;
                // context meter: update each field only when nonzero so
                // incremental emissions (input first, output later) don't
                // reset each other
                let input_total = input_tokens + cache_read_tokens + cache_creation_tokens;
                if input_total > 0 {
                    self.last_input = input_total;
                }
                if output_tokens > 0 {
                    self.last_output = output_tokens;
                }
            }
            AgentEvent::TurnComplete => {
                self.is_running = false;
                self.new_turn = true;
                // cancel any in-progress compact animation
                for item in self.activity.iter_mut().rev() {
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
                self.queued_messages.retain(|q| !matches!(q, QueuedItem::Message(_)));
                self.activity.push(ActivityItem::UserMessage(msg.clone()));
                if let Some(ref mut current) = self.current_message {
                    current.push_str(&format!("\n{}", msg));
                }
                self.auto_scroll = true;
            }
            AgentEvent::Status(msg) => {
                self.activity.push(ActivityItem::System(SystemKind::Info, msg));
            }
            AgentEvent::Error(e) => {
                self.is_running = false;
                self.activity.push(ActivityItem::Text(format!("[error] {e}")));
            }
            AgentEvent::CompactStart => {
                self.activity.push(ActivityItem::Compact(CompactStatus::Running));
                self.auto_scroll = true;
            }
            AgentEvent::CompactDone(msg) => {
                if let Some(ActivityItem::Compact(ref mut s)) = self.activity.iter_mut().rev()
                    .find(|i| matches!(i, ActivityItem::Compact(_)))
                {
                    *s = CompactStatus::Done(msg);
                }
            }
        }
    }

    pub fn start_new_message(&mut self, message: &str) {
        self.activity.push(ActivityItem::UserMessage(message.to_string()));
        self.current_message = Some(message.to_string());
        self.is_running = true;
        self.auto_scroll = true;
        self.current_tool_input.clear();
        if self.start_time.is_none() {
            self.start_time = Some(Instant::now());
        }
    }

    // immediately reflect cancellation in the UI without waiting for TurnComplete
    pub fn cancel_running(&mut self) {
        self.is_running = false;
        self.current_message = None;
        for item in self.activity.iter_mut().rev() {
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
        if !self.input.is_empty() {
            let msg = self.expand_input();
            if let Some(ref tx) = self.inject_tx {
                let _ = tx.send(msg.clone());
            }
            self.queued_messages.push(QueuedItem::Message(msg));
            self.input.clear();
            self.cursor_pos = 0;
            self.paste_chunks.clear();
        }
    }

    // queue a slash command to be dispatched when the current turn finishes
    pub fn queue_command(&mut self, cmd: &str) {
        self.queued_messages.push(QueuedItem::Command(cmd.to_string()));
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
                self.auto_scroll = true;
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
        self.slash_prefix = None;
        self.slash_selected = None;
    }

    // pop the last queued message back into the input for editing
    pub fn pop_queued_message(&mut self) -> bool {
        // find the last Message item (skip over any queued commands)
        let pos = self.queued_messages.iter().rposition(|i| matches!(i, QueuedItem::Message(_)));
        if let Some(idx) = pos {
            if let QueuedItem::Message(msg) = self.queued_messages.remove(idx) {
                self.input = msg;
                self.cursor_pos = self.char_count();
                return true;
            }
        }
        false
    }

    pub fn toggle_diff(&mut self) {
        self.diffs_expanded = !self.diffs_expanded;
        for item in &mut self.activity {
            if let ActivityItem::Tool(ref mut entry) = item {
                entry.expanded = self.diffs_expanded;
            }
        }
    }

    pub fn push_user_message(&mut self, msg: &str) {
        self.activity.push(ActivityItem::UserMessage(msg.to_string()));
        self.auto_scroll = true;
    }

    pub fn push_system_message(&mut self, msg: String) {
        self.activity.push(ActivityItem::System(SystemKind::Info, msg));
        self.auto_scroll = true;
    }

    pub fn push_success(&mut self, msg: String) {
        self.activity.push(ActivityItem::System(SystemKind::Success, msg));
        self.auto_scroll = true;
    }

    pub fn push_warning(&mut self, msg: String) {
        self.activity.push(ActivityItem::System(SystemKind::Warning, msg));
        self.auto_scroll = true;
    }

    pub fn push_error_msg(&mut self, msg: String) {
        self.activity.push(ActivityItem::System(SystemKind::Error, msg));
        self.auto_scroll = true;
    }

    pub fn push_update_notice(&mut self, msg: String) {
        self.activity.push(ActivityItem::System(SystemKind::Update, msg));
        self.auto_scroll = true;
    }

    pub fn start_background_job(&mut self, label: String, detail: String) -> u64 {
        let id = self.next_job_id;
        self.next_job_id += 1;
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
                }
            }
            JobEvent::Update { id, detail } => {
                if let Some(job) = self.background_jobs.iter_mut().find(|j| j.id == id) {
                    job.detail = detail;
                }
            }
            JobEvent::Complete { id, status, summary } => {
                if let Some(job) = self.background_jobs.iter_mut().find(|j| j.id == id) {
                    job.status = status.clone();
                    job.detail = summary.clone();
                }
                let kind = match &status {
                    JobStatus::Passed => SystemKind::Success,
                    JobStatus::Failed(_) => SystemKind::Error,
                    JobStatus::Running => SystemKind::Info,
                };
                self.activity.push(ActivityItem::System(kind, summary));
                self.auto_scroll = true;
            }
            JobEvent::Dismiss { id } => {
                self.background_jobs.retain(|j| j.id != id);
            }
        }
    }

    // remove completed background jobs older than the given duration
    pub fn gc_background_jobs(&mut self, max_age: std::time::Duration) {
        self.background_jobs.retain(|j| {
            matches!(j.status, JobStatus::Running) || j.started_at.elapsed() < max_age
        });
    }

    // reconstruct the activity feed and input history from persisted messages
    pub fn hydrate_from_history(&mut self, messages: &[Message]) {
        use std::collections::HashMap;
        // tool_use_id -> index in self.activity for matching results
        let mut tool_map: HashMap<String, usize> = HashMap::new();

        for msg in messages {
            match (&msg.role.as_str(), &msg.content) {
                (&"user", MessageContent::Text(s)) => {
                    if !s.trim().is_empty() {
                        self.activity.push(ActivityItem::UserMessage(s.clone()));
                        self.push_history(s);
                    }
                }
                (&"user", MessageContent::Blocks(blocks)) => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text } => {
                                if !text.trim().is_empty() {
                                    self.activity.push(ActivityItem::UserMessage(text.clone()));
                                    self.push_history(text);
                                }
                            }
                            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                                if let Some(&idx) = tool_map.get(tool_use_id) {
                                    if let ActivityItem::Tool(ref mut entry) = self.activity[idx] {
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
                        self.activity.push(ActivityItem::Text(s.clone()));
                    }
                }
                (&"assistant", MessageContent::Blocks(blocks)) => {
                    for block in blocks {
                        match block {
                            ContentBlock::Thinking { thinking, .. } => {
                                if !thinking.trim().is_empty() {
                                    self.activity.push(ActivityItem::Thinking(thinking.clone()));
                                }
                            }
                            ContentBlock::Text { text } => {
                                if !text.trim().is_empty() {
                                    self.activity.push(ActivityItem::Text(text.clone()));
                                }
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                let display_name = crate::agent::from_cc_name(name).to_string();
                                let arg = extract_tool_arg(&display_name, input);
                                let idx = self.activity.len();
                                self.activity.push(ActivityItem::Tool(ToolEntry {
                                    name: display_name,
                                    arg,
                                    status: ToolStatus::Complete { exit_code: None },
                                    diff: None,
                                    output: None,
                                    expanded: self.diffs_expanded,
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
        self.login_activity_start = Some(self.activity.len());
    }

    // truncate all activity added since mark_login_start, then push a
    // single clean result message in its place
    pub fn finish_login(&mut self, msg: String, success: bool) {
        if let Some(start) = self.login_activity_start.take() {
            self.activity.truncate(start);
        }
        if success {
            self.push_success(msg);
        } else {
            self.push_error_msg(msg);
        }
    }

    pub fn update_model(&mut self, model_id: &str) {
        self.model_name = model_id.to_string();
        self.context_limit = guess_context_limit(model_id);
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
            if self.input_history.last().map(|s| s.as_str()) != Some(msg) {
                self.input_history.push(msg.to_string());
                if self.input_history.len() > 1000 {
                    self.input_history.remove(0);
                }
            }
        }
        self.input_history_pos = None;
        self.input_draft = String::new();
    }

    // navigate to the previous (older) history entry. returns true if handled
    // (so the caller knows not to fall through to scroll).
    pub fn navigate_history_up(&mut self) -> bool {
        if self.input_history.is_empty() {
            return false;
        }
        match self.input_history_pos {
            None => {
                self.input_draft = self.expand_input();
                let pos = self.input_history.len() - 1;
                self.input_history_pos = Some(pos);
                self.input = self.input_history[pos].clone();
                self.paste_chunks.clear();
                self.cursor_pos = self.char_count();
                true
            }
            Some(0) => true, // already at oldest entry, absorb the keypress
            Some(p) => {
                let new_pos = p - 1;
                self.input_history_pos = Some(new_pos);
                self.input = self.input_history[new_pos].clone();
                self.cursor_pos = self.char_count();
                true
            }
        }
    }

    // navigate to the next (newer) history entry, or back to the saved draft.
    // returns false when not in history mode so the caller can scroll instead.
    pub fn navigate_history_down(&mut self) -> bool {
        match self.input_history_pos {
            None => false,
            Some(p) if p + 1 >= self.input_history.len() => {
                self.input = self.input_draft.clone();
                self.paste_chunks.clear();
                self.cursor_pos = self.char_count();
                self.input_history_pos = None;
                true
            }
            Some(p) => {
                let new_pos = p + 1;
                self.input_history_pos = Some(new_pos);
                self.input = self.input_history[new_pos].clone();
                self.cursor_pos = self.char_count();
                true
            }
        }
    }

    pub fn reset_session(&mut self) {
        self.activity.clear();
        self.activity_render_cache.clear();
        self.total_input = 0;
        self.total_output = 0;
        self.total_cache_read = 0;
        self.total_cache_creation = 0;
        self.last_input = 0;
        self.last_output = 0;
        self.current_message = None;
        self.queued_messages.clear();
        self.rate_samples.clear();
        self.rate_bucket = TokenBucket::default();
        self.start_time = None;
        self.scroll_offset = 0;
        self.auto_scroll = true;
        self.new_turn = false;
        self.paste_chunks.clear();
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
        self.last_input + self.last_output
    }

    fn context_pct(&self) -> f64 {
        if self.context_limit == 0 { return 0.0; }
        (self.context_used() as f64 / self.context_limit as f64).min(1.0)
    }

    fn cost_usd(&self) -> f64 {
        let p = crate::config::model_pricing(&self.model_name);
        // cache writes cost 1.25x, cache reads cost 0.1x base input price
        self.total_input as f64 * p.input / 1_000_000.0
            + self.total_cache_creation as f64 * p.input * 1.25 / 1_000_000.0
            + self.total_cache_read as f64 * p.input * 0.1 / 1_000_000.0
            + self.total_output as f64 * p.output / 1_000_000.0
    }

    fn avg_rate(&self) -> f64 {
        let elapsed = self.start_time.map_or(0.0, |t| t.elapsed().as_secs_f64());
        if elapsed > 0.0 {
            self.total_output as f64 / elapsed
        } else {
            0.0
        }
    }

    // cursor_pos is a char-count offset. convert to byte index for
    // String insert/remove operations.
    fn cursor_byte_pos(&self) -> usize {
        self.input.char_indices()
            .nth(self.cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    // insert text directly at the cursor position without any collapsing
    pub fn insert_text(&mut self, text: String) {
        let bp = self.cursor_byte_pos();
        self.input.insert_str(bp, &text);
        self.cursor_pos += text.chars().count();
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
            self.input.insert_str(bp, &text);
            self.cursor_pos += text.chars().count();
            return;
        }

        let idx = self.paste_chunks.len();
        if idx > 15 {
            let bp = self.cursor_byte_pos();
            self.input.insert_str(bp, &text);
            self.cursor_pos += text.chars().count();
            return;
        }

        self.paste_chunks.push(text);
        let placeholder = char::from_u32(0xE000 + idx as u32).unwrap();
        let bp = self.cursor_byte_pos();
        self.input.insert(bp, placeholder);
        self.cursor_pos += 1;
    }

    // replace all paste placeholders with their real content
    pub fn expand_input(&self) -> String {
        let mut out = String::new();
        for c in self.input.chars() {
            if is_paste_placeholder(c) {
                let idx = paste_placeholder_index(c);
                if idx < self.paste_chunks.len() {
                    out.push_str(&self.paste_chunks[idx]);
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn char_count(&self) -> usize {
        self.input.chars().count()
    }

    // (line_number, column_in_chars) from cursor_pos
    fn cursor_line_col(&self) -> (usize, usize) {
        let text_before: String = self.input.chars().take(self.cursor_pos).collect();
        let line = text_before.matches('\n').count();
        let col = match text_before.rfind('\n') {
            Some(i) => text_before[i + 1..].chars().count(),
            None => text_before.chars().count(),
        };
        (line, col)
    }

    fn input_line_count(&self) -> usize {
        self.input.split('\n').count()
    }

    // visual line count after soft-wrapping to terminal width
    fn input_visual_line_count(&self) -> usize {
        let content_width = (self.term_width as usize).saturating_sub(2); // prefix width
        if content_width == 0 { return 1; }
        let display = make_display_input(&self.input, &self.paste_chunks);
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

    fn delete_to_line_start(&mut self) {        let (_, col) = self.cursor_line_col();
        if col == 0 {
            if self.cursor_pos > 0 {
                let bp = self.cursor_byte_pos();
                let prev = self.input[..bp].char_indices().last().map(|(i, _)| i);
                if let Some(pb) = prev {
                    self.input.remove(pb);
                    self.cursor_pos -= 1;
                }
            }
        } else {
            let bp = self.cursor_byte_pos();
            let start = bp - self.input[..bp].chars().rev().take(col)
                .map(|c| c.len_utf8()).sum::<usize>();
            self.input.replace_range(start..bp, "");
            self.cursor_pos -= col;
        }
    }

    fn delete_to_line_end(&mut self) {
        let bp = self.cursor_byte_pos();
        let end = self.input[bp..].find('\n')
            .map(|i| bp + i)
            .unwrap_or(self.input.len());
        self.input.replace_range(bp..end, "");
    }

    fn delete_word_backward(&mut self) {
        if self.cursor_pos == 0 { return; }
        let chars: Vec<char> = self.input.chars().collect();
        let mut new_pos = self.cursor_pos;
        while new_pos > 0 && chars[new_pos - 1].is_whitespace() { new_pos -= 1; }
        while new_pos > 0 && !chars[new_pos - 1].is_whitespace() { new_pos -= 1; }
        let byte_start = self.input.char_indices()
            .nth(new_pos).map(|(i, _)| i).unwrap_or(0);
        let byte_end = self.cursor_byte_pos();
        self.input.replace_range(byte_start..byte_end, "");
        self.cursor_pos = new_pos;
    }

    fn move_word_left(&mut self) {
        if self.cursor_pos == 0 { return; }
        let chars: Vec<char> = self.input.chars().collect();
        let mut pos = self.cursor_pos;
        while pos > 0 && chars[pos - 1].is_whitespace() { pos -= 1; }
        while pos > 0 && !chars[pos - 1].is_whitespace() { pos -= 1; }
        self.cursor_pos = pos;
    }

    fn move_word_right(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let len = chars.len();
        let mut pos = self.cursor_pos;
        while pos < len && !chars[pos].is_whitespace() { pos += 1; }
        while pos < len && chars[pos].is_whitespace() { pos += 1; }
        self.cursor_pos = pos;
    }

    fn move_line_start(&mut self) {
        let (_, col) = self.cursor_line_col();
        self.cursor_pos -= col;
    }

    fn move_line_end(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut pos = self.cursor_pos;
        while pos < chars.len() && chars[pos] != '\n' { pos += 1; }
        self.cursor_pos = pos;
    }

    // returns false if already on the first line
    fn move_cursor_up(&mut self) -> bool {
        let (line, col) = self.cursor_line_col();
        if line == 0 { return false; }
        let lines: Vec<&str> = self.input.split('\n').collect();
        let prev_len = lines[line - 1].chars().count();
        let new_col = col.min(prev_len);
        let mut new_pos = 0;
        for i in 0..line - 1 {
            new_pos += lines[i].chars().count() + 1;
        }
        new_pos += new_col;
        self.cursor_pos = new_pos;
        true
    }

    // returns false if already on the last line
    fn move_cursor_down(&mut self) -> bool {
        let (line, col) = self.cursor_line_col();
        let lines: Vec<&str> = self.input.split('\n').collect();
        if line >= lines.len() - 1 { return false; }
        let next_len = lines[line + 1].chars().count();
        let new_col = col.min(next_len);
        let mut new_pos = 0;
        for i in 0..=line {
            new_pos += lines[i].chars().count() + 1;
        }
        new_pos += new_col;
        self.cursor_pos = new_pos;
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
        if branch.is_empty() { None } else { Some(branch) }
    } else {
        None
    }
}

fn guess_context_limit(model: &str) -> u32 {
    if let Some(def) = crate::config::ANTHROPIC_MODELS.iter().find(|m| m.id == model) {
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
        "read" | "edit" | "write" => {
            input.get("path").and_then(|v| v.as_str()).unwrap_or("...").to_string()
        }
        "bash" => {
            input.get("command").and_then(|v| v.as_str()).unwrap_or("...").to_string()
        }
        "web_search" => {
            input.get("query").and_then(|v| v.as_str()).unwrap_or("...").to_string()
        }
        "explore" => {
            input.get("prompt").and_then(|v| v.as_str()).unwrap_or("...").to_string()
        }
        _ => "...".to_string(),
    }
}

// extract exit code from bash output text.
// tools.rs prefixes non-zero exits with "[exit code: N]\n"
fn parse_exit_code(output: &str) -> Option<i32> {
    if output.starts_with("[exit code: ") {
        output[12..].split(']').next()
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
        serde_json::Value::Array(arr) => {
            arr.iter()
                .filter_map(|item| {
                    if item.get("type")?.as_str()? == "text" {
                        item.get("text")?.as_str().map(|s| s.to_string())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => String::new(),
    }
}

// return the last paragraph from a block of text.
// paragraphs are separated by blank lines (\n\n).
fn last_paragraph(text: &str) -> &str {
    let trimmed = text.trim_end();
    if let Some(pos) = trimmed.rfind("\n\n") {
        let after = trimmed[pos + 2..].trim_start_matches('\n');
        if after.is_empty() { trimmed } else { after }
    } else {
        trimmed
    }
}

// remove ansi escape sequences and other terminal control codes from tool output.
// covers CSI sequences (\x1b[...X), OSC sequences (\x1b]...ST), character set
// designations (\x1b(F), and bare \x1b.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: consume until final byte (0x40-0x7E)
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch as u32 >= 0x40 && ch as u32 <= 0x7E {
                            break;
                        }
                    }
                }
                Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                    chars.next();
                    // OSC/DCS/APC: consume until ST (\x1b\\) or BEL (\x07)
                    let mut prev = '\0';
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch == '\x07' {
                            break;
                        }
                        if prev == '\x1b' && ch == '\\' {
                            break;
                        }
                        prev = ch;
                    }
                }
                Some('(' | ')' | '*' | '+') => {
                    // character set designation: ESC ( F, ESC ) F, etc.
                    chars.next();
                    chars.next();
                }
                Some(_) => {
                    // two-char sequence, skip next char
                    chars.next();
                }
                _ => {}
            }
        } else if c == '\r' {
            // carriage return: overwrite the current line
            if let Some(pos) = out.rfind('\n') {
                out.truncate(pos + 1);
            } else {
                out.clear();
            }
        } else {
            out.push(c);
        }
    }
    out
}

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
                lines.push(Line::from(vec![bar.clone(), Span::styled(remaining.to_string(), style)]));
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
            if split == 0 { split = remaining.len(); }
            lines.push(Line::from(vec![bar.clone(), Span::styled(remaining[..split].to_string(), style)]));
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
        let line_width: usize = ml.spans.iter().map(|s| UnicodeWidthStr::width(s.content.as_ref())).sum();
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
                        if w + cw > avail { break; }
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

pub fn render(frame: &mut Frame, app: &mut App) {
    let size = frame.area();

    let max_input = (size.height / 3).max(2);

    // slash command suggestions shown when input starts with "/".
    // use the snapshot taken at first Tab press so cycling doesn't narrow the set.
    let completion_input = app.slash_prefix.as_deref().unwrap_or(app.input.as_str());
    let suggestions: Vec<Suggestion> = if app.input.starts_with('/') {
        slash_suggestions(completion_input)
    } else {
        vec![]
    };
    let slash_selected = app.slash_selected;

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
            Constraint::Min(4),          // activity feed
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
    if area.height == 0 { return; }

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
        spans.push(Span::styled(format!("{} ", icon), Style::default().fg(icon_color)));
        spans.push(Span::styled(job.label.clone(), Style::default().fg(MUTED)));
        if !job.detail.is_empty() {
            spans.push(Span::styled(format!(" {}", job.detail), Style::default().fg(DIM)));
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
        format!(" ({})", app.thinking_level)
    } else {
        String::new()
    };

    // build right-side metrics first (fixed width, drives how much space the left side gets)
    let rate = app.avg_rate();
    let pct = app.context_pct();
    let used_k = app.context_used() / 1000;
    let limit_k = app.context_limit / 1000;

    let spark_width: usize = 16;
    let ctx_bar_width: usize = 8;
    let filled = ((pct * ctx_bar_width as f64).round() as usize).min(ctx_bar_width);
    let empty = ctx_bar_width - filled;
    let ctx_color = if pct > 0.8 { RED } else if pct > 0.6 { YELLOW } else { ACCENT };

    let rate_str = format!("{:.0} tok/s", rate);
    let cost_str = format!("${:.3}", app.cost_usd());
    let ctx_label = format!("{}k/{}k", used_k, limit_k);
    let ctx_pct = format!("{:.0}%", pct * 100.0);

    let right_len = spark_width + 1 + rate_str.len() + 2 + cost_str.len() + 2
        + ctx_label.len() + 2 + ctx_bar_width + 2 + ctx_pct.len();

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
    let full_left_content = full_cwd.len() + if has_branch { full_branch.len() + branch_overhead } else { 0 };

    // determine what to show, truncating to fit within budget
    let (display_cwd, display_branch): (String, Option<String>) = if full_left_content <= budget {
        // everything fits
        (full_cwd.clone(), if has_branch { Some(full_branch.to_string()) } else { None })
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
        Span::styled("rum", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
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

    spans.push(Span::styled(app.model_name.clone(), Style::default().fg(MUTED)));
    spans.push(Span::styled(thinking_suffix.clone(), Style::default().fg(MUTED)));

    let left_len = 4 + display_cwd.len() + 4 + branch_display_len + model_part;

    let pad = w.saturating_sub(left_len + right_len);
    spans.push(Span::styled(" ".repeat(pad), Style::default()));

    // sparkline
    let spark_spans = render_colored_sparkline(&app.rate_samples, spark_width);
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
    if content_width == 0 { return 1; }
    let mut count = 0;
    for line in text.split('\n') {
        let w = UnicodeWidthStr::width(line);
        if w == 0 { count += 1; } else {
            count += (w + content_width - 1) / content_width;
        }
    }
    count.max(1)
}

// wrap a message into indented lines with a given text style.
// used for rendering the active message and queued messages above the input.
fn wrap_message_lines(text: &str, max_width: u16, text_style: Style, spinner: Option<&str>) -> Vec<Line<'static>> {
    let prefix_width = 2usize;
    let content_width = (max_width as usize).saturating_sub(prefix_width);
    if content_width == 0 { return vec![]; }

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
                if w + cw > content_width { break; }
                w += cw;
                chunk_end += 1;
            }
            if chunk_end == chunk_start { chunk_end = chunk_start + 1; }
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
            lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            ]));
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
                    let col_w: usize = col_chars.iter()
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
                Span::styled(row_prefix, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
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
        lines.push(Line::from(vec![
            Span::styled("\u{203a} ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        ]));
        if cursor_char_pos == Some(0) {
            cursor_visual = Some((0, 0));
        }
    }

    (lines, cursor_visual)
}

fn render_message_area(frame: &mut Frame, app: &App, area: Rect, suggestions: &[Suggestion], selected: Option<usize>) {
    if area.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    if app.is_running && suggestions.is_empty() {
        let spin = spinner_char(app.spin_frame);
        if let Some(ref msg) = app.current_message {
            lines.extend(wrap_message_lines(msg, area.width, Style::default().fg(ACCENT), Some(spin)));
        }
        for qm in &app.queued_messages {
            match qm {
                QueuedItem::Message(s) => {
                    lines.extend(wrap_message_lines(s, area.width, Style::default().fg(MUTED), None));
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
    let display = make_display_input(&app.input, &app.paste_chunks);
    let display_cursor = remap_cursor(&app.input, &app.paste_chunks, app.cursor_pos);
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
    let n = app.activity.len();

    // sync cache length with activity list
    app.activity_render_cache.truncate(n);
    while app.activity_render_cache.len() < n {
        app.activity_render_cache.push(CachedRender::default());
    }

    // re-render only stale items
    for idx in 0..n {
        let (content_len, expanded, status_tag) = match &app.activity[idx] {
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
            let c = &app.activity_render_cache[idx];
            c.content_len != content_len
                || c.width != w
                || c.expanded != expanded
                || c.status_tag != status_tag
        };

        if stale {
            let item_lines = render_activity_item(&app.activity[idx], w, app.spin_frame);
            app.activity_render_cache[idx] = CachedRender {
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
                is_compact_tool(&app.activity[idx - 1])
                && is_compact_tool(&app.activity[idx]);
            if !both_collapsed_tools {
                total += 1;
            }
        }
        total += app.activity_render_cache[idx].lines.len();
    }

    let show_waiting = app.is_running && total == 0;
    if show_waiting {
        total = 1;
    }

    let total_lines = total as u16;
    let max_scroll = total_lines.saturating_sub(area.height);

    // re-engage auto-scroll when manual scrolling reaches the bottom
    if !app.auto_scroll && app.scroll_offset >= max_scroll {
        app.scroll_offset = max_scroll;
        app.auto_scroll = true;
    }

    let scroll = if app.auto_scroll {
        app.scroll_offset = max_scroll;
        max_scroll
    } else {
        app.scroll_offset
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
                    is_compact_tool(&app.activity[idx - 1])
                    && is_compact_tool(&app.activity[idx]);
                if !both_collapsed_tools {
                    if cursor >= vp_start {
                        lines.push(Line::from(""));
                    }
                    cursor += 1;
                }
            }

            // item lines
            for line in &app.activity_render_cache[idx].lines {
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
                if w_used + cw > content_width { break; }
                w_used += cw;
                end += 1;
            }
            if end == start { end = start + 1; }
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
        SystemKind::Info    => ("  ",     MUTED,   false),
        SystemKind::Success => ("  ✓ ",   GREEN,   false),
        SystemKind::Warning => ("  ⚠ ",   YELLOW,  false),
        SystemKind::Error   => ("  ✗ ",   RED,     false),
        SystemKind::Update  => ("  ↑ ",   ACCENT,  true),
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
                spans.push(Span::styled(format!(" {}", timer), Style::default().fg(DIM)));
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
            spans.push(Span::styled(
                label.to_string(),
                Style::default().fg(ACCENT),
            ));
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
                        lines.push(tool_line(vec![
                            Span::styled(
                                format!("    ...{} more lines", total_lines - 8),
                                Style::default().fg(DIM),
                            ),
                        ]));
                    }
                }

                // diff lines
                if let Some(ref diff) = entry.diff {
                    lines.extend(build_diff_lines(diff));
                }
            }
        }
        ToolStatus::Error(e) => {
            let short = if e.len() > 80 { format!("{}...", &e[..77]) } else { e.clone() };
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
    let blocks = [' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}'];

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
    pairs.iter()
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
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
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

    // ctrl+c: cancel if running, quit if idle with empty input, clear input otherwise
    if ctrl && key.code == KeyCode::Char('c') {
        if app.is_running {
            return InputAction::Cancel;
        }
        if app.input.is_empty() {
            return InputAction::Quit;
        }
        app.input.clear();
        app.cursor_pos = 0;
        app.paste_chunks.clear();
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
        if !app.input.is_empty() {
            app.input.clear();
            app.cursor_pos = 0;
            app.paste_chunks.clear();
            app.reset_slash_completion();
        }
        return InputAction::None;
    }

    // page scroll (always available)
    if key.code == KeyCode::PageUp {
        app.auto_scroll = false;
        app.scroll_offset = app.scroll_offset.saturating_sub(10);
        return InputAction::None;
    }
    if key.code == KeyCode::PageDown {
        app.scroll_offset = app.scroll_offset.saturating_add(10);
        return InputAction::None;
    }

    match key.code {
        KeyCode::Enter => {
            if shift || alt || ctrl {
                app.reset_slash_completion();
                let bp = app.cursor_byte_pos();
                app.input.insert(bp, '\n');
                app.cursor_pos += 1;
            } else if !app.input.is_empty() {
                // slash commands and ! bash commands are dispatched immediately even during a running turn
                if app.is_running && !app.input.starts_with('/') && !app.input.starts_with('!') {
                    app.reset_slash_completion();
                    app.queue_message();
                } else {
                    app.reset_slash_completion();
                    let msg = app.expand_input();
                    app.input.clear();
                    app.cursor_pos = 0;
                    app.paste_chunks.clear();
                    return InputAction::Submit(msg);
                }
            }
        }
        KeyCode::Tab | KeyCode::BackTab => {
            if app.input.starts_with('/') {
                let forward = key.code == KeyCode::Tab;
                // snapshot the input before the first Tab so cycling doesn't narrow the set
                if app.slash_prefix.is_none() {
                    app.slash_prefix = Some(app.input.clone());
                }
                let prefix = app.slash_prefix.clone().unwrap();
                let suggestions = slash_suggestions(&prefix);
                let count = suggestions.len();
                if count == 0 {
                    return InputAction::None;
                }
                let next = if forward {
                    match app.slash_selected {
                        None => 0,
                        Some(i) => (i + 1) % count,
                    }
                } else {
                    match app.slash_selected {
                        None | Some(0) => count - 1,
                        Some(i) => i - 1,
                    }
                };
                app.slash_selected = Some(next);
                let completion = suggestions[next].completion.clone();
                // when a command name is completed (trailing space), reset the prefix
                // so the next Tab opens a fresh arg-completion session
                let is_cmd_completion = completion.ends_with(' ');
                app.input = completion;
                app.cursor_pos = app.char_count();
                if is_cmd_completion {
                    app.slash_prefix = Some(app.input.clone());
                    app.slash_selected = None;
                }
            }
        }
        KeyCode::Backspace => {
            app.reset_slash_completion();
            if super_key {
                app.delete_to_line_start();
            } else if alt || ctrl {
                app.delete_word_backward();
            } else if app.cursor_pos > 0 {
                let bp = app.cursor_byte_pos();
                let prev = app.input[..bp].char_indices().last().map(|(i, _)| i);
                if let Some(pb) = prev {
                    app.input.remove(pb);
                    app.cursor_pos -= 1;
                }
            }
        }
        KeyCode::Delete => {
            app.reset_slash_completion();
            if app.cursor_pos < app.char_count() {
                let bp = app.cursor_byte_pos();
                app.input.remove(bp);
            }
        }
        KeyCode::Left => {
            if super_key {
                app.move_line_start();
            } else if alt {
                app.move_word_left();
            } else {
                app.cursor_pos = app.cursor_pos.saturating_sub(1);
            }
        }
        KeyCode::Right => {
            if super_key {
                app.move_line_end();
            } else if alt {
                app.move_word_right();
            } else if app.cursor_pos < app.char_count() {
                app.cursor_pos += 1;
            }
        }
        KeyCode::Up => {
            if shift {
                app.auto_scroll = false;
                app.scroll_offset = app.scroll_offset.saturating_sub(1);
            } else if app.input_line_count() > 1 && app.move_cursor_up() {
                // moved within multi-line input
            } else if app.input.is_empty() && app.pop_queued_message() {
                // popped last queued message into input
            } else if app.navigate_history_up() {
                // navigated to an older history entry
            } else {
                return InputAction::ScrollUp;
            }
        }
        KeyCode::Down => {
            if shift {
                app.scroll_offset = app.scroll_offset.saturating_add(1);
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
                    'u' => { app.reset_slash_completion(); app.delete_to_line_start(); }
                    'k' => { app.reset_slash_completion(); app.delete_to_line_end(); }
                    'w' => { app.reset_slash_completion(); app.delete_word_backward(); }
                    'j' => {
                        // ctrl+j inserts newline (traditional unix LF)
                        app.reset_slash_completion();
                        let bp = app.cursor_byte_pos();
                        app.input.insert(bp, '\n');
                        app.cursor_pos += 1;
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
                        let start = app.cursor_pos;
                        app.move_word_right();
                        let end = app.cursor_pos;
                        if end > start {
                            let byte_start = app.input.char_indices()
                                .nth(start).map(|(i, _)| i).unwrap_or(app.input.len());
                            let byte_end = app.input.char_indices()
                                .nth(end).map(|(i, _)| i).unwrap_or(app.input.len());
                            app.input.replace_range(byte_start..byte_end, "");
                            app.cursor_pos = start;
                        }
                    }
                    _ => {}
                }
            } else {
                app.reset_slash_completion();
                let bp = app.cursor_byte_pos();
                app.input.insert(bp, c);
                app.cursor_pos += 1;
            }
        }
        _ => {}
    }

    InputAction::None
}
