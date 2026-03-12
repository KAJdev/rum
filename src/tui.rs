use crossterm::{
    event::{KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame, Terminal,
};
use std::io::{self, Stdout};
use crate::agent::AgentEvent;
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

// ordered activity feed items, preserving the interleaved
// sequence of text, tool calls, and thinking blocks
#[derive(Debug, Clone)]
enum ActivityItem {
    Text(String),
    Thinking(String),
    Tool(ToolEntry),
}

#[derive(Debug, Clone)]
struct ToolEntry {
    name: String,
    display: String,
    status: ToolStatus,
    diff: Option<DiffInfo>,
    expanded: bool,
}

#[derive(Debug, Clone)]
enum ToolStatus {
    Running,
    Complete,
    Error(String),
}

struct TokenStats {
    total_input: u32,
    total_output: u32,
}

impl TokenStats {
    fn new() -> Self {
        Self {
            total_input: 0,
            total_output: 0,
        }
    }

    fn total_tokens(&self) -> u32 {
        self.total_input + self.total_output
    }

    fn cost_usd(&self) -> f64 {
        let input_cost = self.total_input as f64 * 3.0 / 1_000_000.0;
        let output_cost = self.total_output as f64 * 15.0 / 1_000_000.0;
        input_cost + output_cost
    }
}

pub struct App {
    input: String,
    cursor_pos: usize,
    // ordered list of activity items (text, tools, thinking interleaved)
    activity: Vec<ActivityItem>,
    current_message: Option<String>,
    token_stats: TokenStats,
    pub is_running: bool,
    pub should_quit: bool,
    pub scroll_offset: u16,
    model_name: String,
    cwd: String,
    current_tool_input: String,
}

impl App {
    pub fn new(model_name: &str, cwd: &str) -> Self {
        Self {
            input: String::new(),
            cursor_pos: 0,
            activity: Vec::new(),
            current_message: None,
            token_stats: TokenStats::new(),
            is_running: false,
            should_quit: false,
            scroll_offset: 0,
            model_name: model_name.to_string(),
            cwd: cwd.to_string(),
            current_tool_input: String::new(),
        }
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Thinking(t) => {
                // append to the last thinking block, or start a new one
                if let Some(ActivityItem::Thinking(ref mut s)) = self.activity.last_mut() {
                    s.push_str(&t);
                } else {
                    self.activity.push(ActivityItem::Thinking(t));
                }
            }
            AgentEvent::Text(t) => {
                // append to the last text block, or start a new one
                if let Some(ActivityItem::Text(ref mut s)) = self.activity.last_mut() {
                    s.push_str(&t);
                } else {
                    self.activity.push(ActivityItem::Text(t));
                }
            }
            AgentEvent::ToolStart { id: _, name } => {
                self.current_tool_input.clear();
                self.activity.push(ActivityItem::Tool(ToolEntry {
                    name: name.clone(),
                    display: format!("{}...", name),
                    status: ToolStatus::Running,
                    diff: None,
                    expanded: false,
                }));
            }
            AgentEvent::ToolInputDelta(json) => {
                self.current_tool_input.push_str(&json);
                if let Some(ActivityItem::Tool(ref mut entry)) = self.activity.last_mut() {
                    if let Ok(partial) =
                        serde_json::from_str::<serde_json::Value>(&self.current_tool_input)
                    {
                        entry.display = format_tool_display(&entry.name, &partial);
                    }
                }
            }
            AgentEvent::ToolComplete { id: _, name, result } => {
                // find the last tool entry to update
                if let Some(ActivityItem::Tool(ref mut entry)) = self.activity.iter_mut().rev()
                    .find(|item| matches!(item, ActivityItem::Tool(e) if matches!(e.status, ToolStatus::Running)))
                {
                    match &result {
                        ToolResult::Success { output: _, diff } => {
                            if let Some(d) = diff {
                                entry.display = format!(
                                    "{} {}",
                                    capitalize_tool(&name),
                                    d.path
                                );
                                entry.diff = Some(d.clone());
                            }
                            entry.status = ToolStatus::Complete;
                        }
                        ToolResult::Error(e) => {
                            entry.status = ToolStatus::Error(e.clone());
                        }
                    }
                }
            }
            AgentEvent::TokenUsage {
                input_tokens,
                output_tokens,
            } => {
                self.token_stats.total_input += input_tokens;
                self.token_stats.total_output += output_tokens;
            }
            AgentEvent::TurnComplete => {
                self.is_running = false;
            }
            AgentEvent::Error(e) => {
                self.is_running = false;
                // append error as text
                self.activity.push(ActivityItem::Text(format!("\n[error] {}", e)));
            }
        }
    }

    pub fn start_new_message(&mut self, message: &str) {
        self.current_message = Some(message.to_string());
        self.activity.clear();
        self.is_running = true;
        self.scroll_offset = 0;
        self.current_tool_input.clear();
    }

    // toggle diff expansion on the nth tool entry
    pub fn toggle_diff(&mut self, tool_index: usize) {
        let mut count = 0;
        for item in &mut self.activity {
            if let ActivityItem::Tool(ref mut entry) = item {
                if entry.diff.is_some() {
                    if count == tool_index {
                        entry.expanded = !entry.expanded;
                        return;
                    }
                    count += 1;
                }
            }
        }
    }

    pub fn tool_diff_count(&self) -> usize {
        self.activity.iter().filter(|item| {
            matches!(item, ActivityItem::Tool(e) if e.diff.is_some())
        }).count()
    }
}

fn capitalize_tool(name: &str) -> &str {
    match name {
        "read" => "Read",
        "edit" => "Edit",
        "write" => "Write",
        "bash" => "Bash",
        _ => name,
    }
}

fn format_tool_display(name: &str, input: &serde_json::Value) -> String {
    match name {
        "read" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("...");
            format!("Read {}", path)
        }
        "edit" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("...");
            format!("Edit {}", path)
        }
        "write" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("...");
            format!("Write {}", path)
        }
        "bash" => {
            let cmd = input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("...");
            let short = if cmd.len() > 80 {
                format!("{}...", &cmd[..77])
            } else {
                cmd.to_string()
            };
            format!("$ {}", short)
        }
        _ => format!("{} ...", name),
    }
}

// prepend a dim bar to every line for model output
fn bar_line(spans: Vec<Span<'static>>) -> Line<'static> {
    let mut all = vec![Span::styled("\u{2502} ", Style::default().fg(BAR_COLOR))];
    all.extend(spans);
    Line::from(all)
}

fn bar_empty() -> Line<'static> {
    Line::from(Span::styled("\u{2502}", Style::default().fg(BAR_COLOR)))
}

pub fn render(frame: &mut Frame, app: &App) {
    let size = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(2), // user message / input
            Constraint::Min(10),  // activity feed
        ])
        .split(size);

    render_header(frame, app, chunks[0]);

    if app.is_running {
        render_user_message(frame, app, chunks[1]);
    } else {
        render_input(frame, app, chunks[1]);
    }

    render_activity(frame, app, chunks[2]);
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let stats = &app.token_stats;
    let total = stats.total_tokens();
    let cost = stats.cost_usd();

    let mut spans = vec![
        Span::styled("rum", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("  {}  ", app.cwd),
            Style::default().fg(MUTED),
        ),
        Span::styled(
            &app.model_name,
            Style::default().fg(DIM),
        ),
    ];

    if total > 0 {
        let left_len = 4 + app.cwd.len() + 4 + app.model_name.len();
        let stats_str = format!("{} tokens  ${:.3}", total, cost);
        let padding = (area.width as usize).saturating_sub(left_len + stats_str.len());
        spans.push(Span::styled(
            " ".repeat(padding),
            Style::default(),
        ));
        spans.push(Span::styled(stats_str, Style::default().fg(MUTED)));
    }

    let header = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(BG));

    frame.render_widget(header, area);
}

fn render_user_message(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(ref msg) = app.current_message {
        let lines = vec![Line::from(vec![
            Span::styled("\u{203a} ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(msg.as_str(), Style::default().fg(FG)),
        ])];

        let widget = Paragraph::new(lines)
            .style(Style::default().bg(BG))
            .wrap(Wrap { trim: false });
        frame.render_widget(widget, area);
    }
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let input_lines = vec![Line::from(vec![
        Span::styled("\u{203a} ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(&app.input, Style::default().fg(FG)),
        Span::styled("\u{2588}", Style::default().fg(ACCENT)),
    ])];

    let widget = Paragraph::new(input_lines)
        .style(Style::default().bg(BG))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn render_activity(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    for item in &app.activity {
        match item {
            ActivityItem::Thinking(text) => {
                // show a condensed thinking indicator
                let preview: String = text.chars().take(60).collect();
                let display = if text.len() > 60 {
                    format!("{}...", preview.trim())
                } else {
                    preview.trim().to_string()
                };
                lines.push(bar_line(vec![
                    Span::styled("thinking: ", Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)),
                    Span::styled(display, Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)),
                ]));
            }
            ActivityItem::Text(text) => {
                let mut md = crate::markdown::TuiMarkdownRenderer::new();
                let md_lines = md.render_lines(text);
                for ml in md_lines {
                    let mut all_spans = vec![
                        Span::styled("\u{2502} ", Style::default().fg(BAR_COLOR)),
                    ];
                    all_spans.extend(ml.spans);
                    lines.push(Line::from(all_spans));
                }
            }
            ActivityItem::Tool(entry) => {
                match &entry.status {
                    ToolStatus::Running => {
                        lines.push(bar_line(vec![
                            Span::styled("\u{25cc} ", Style::default().fg(YELLOW)),
                            Span::styled(
                                entry.display.clone(),
                                Style::default().fg(YELLOW),
                            ),
                        ]));
                    }
                    ToolStatus::Complete => {
                        let mut spans = vec![
                            Span::styled(
                                entry.display.clone(),
                                Style::default().fg(MUTED),
                            ),
                        ];

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

                        lines.push(bar_line(spans));

                        if entry.expanded {
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
                        lines.push(bar_line(vec![
                            Span::styled("\u{2717} ", Style::default().fg(RED)),
                            Span::styled(
                                format!("{} - {}", entry.display, short),
                                Style::default().fg(RED),
                            ),
                        ]));
                    }
                }
            }
        }
    }

    // show thinking indicator if running and nothing visible yet
    if app.is_running && lines.is_empty() {
        lines.push(bar_line(vec![
            Span::styled(
                "thinking...",
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            ),
        ]));
    }

    // blank bar at the end for visual closure
    if !lines.is_empty() {
        lines.push(bar_empty());
    }

    let activity = Paragraph::new(lines)
        .style(Style::default().bg(BG))
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset, 0));

    frame.render_widget(activity, area);
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
            out.push(bar_line(vec![
                Span::styled(
                    format!("  {}", prefix),
                    Style::default().fg(color),
                ),
                Span::styled(
                    content,
                    Style::default().fg(color),
                ),
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
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    pub fn restore(&mut self) -> Result<(), io::Error> {
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        Ok(())
    }

    pub fn draw(&mut self, app: &App) -> Result<(), io::Error> {
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
    Quit,
    ScrollUp,
    ScrollDown,
    ToggleDiff,
    None,
}

pub fn handle_key_event(key: KeyEvent, app: &mut App) -> InputAction {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if app.is_running {
            return InputAction::Quit;
        }
        if app.input.is_empty() {
            return InputAction::Quit;
        }
        app.input.clear();
        app.cursor_pos = 0;
        return InputAction::None;
    }

    if key.code == KeyCode::Esc {
        return InputAction::Quit;
    }

    if app.is_running {
        match key.code {
            KeyCode::Up => return InputAction::ScrollUp,
            KeyCode::Down => return InputAction::ScrollDown,
            KeyCode::PageUp => {
                app.scroll_offset = app.scroll_offset.saturating_sub(10);
                return InputAction::None;
            }
            KeyCode::PageDown => {
                app.scroll_offset = app.scroll_offset.saturating_add(10);
                return InputAction::None;
            }
            _ => return InputAction::None,
        }
    }

    match key.code {
        KeyCode::Enter => {
            if !app.input.is_empty() {
                let msg = app.input.clone();
                app.input.clear();
                app.cursor_pos = 0;
                return InputAction::Submit(msg);
            }
        }
        KeyCode::Backspace => {
            if app.cursor_pos > 0 {
                app.input.remove(app.cursor_pos - 1);
                app.cursor_pos -= 1;
            }
        }
        KeyCode::Left => {
            app.cursor_pos = app.cursor_pos.saturating_sub(1);
        }
        KeyCode::Right => {
            if app.cursor_pos < app.input.len() {
                app.cursor_pos += 1;
            }
        }
        KeyCode::Up => return InputAction::ScrollUp,
        KeyCode::Down => return InputAction::ScrollDown,
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'o' {
                return InputAction::ToggleDiff;
            }

            app.input.insert(app.cursor_pos, c);
            app.cursor_pos += 1;
        }
        KeyCode::Home => {
            app.cursor_pos = 0;
        }
        KeyCode::End => {
            app.cursor_pos = app.input.len();
        }
        _ => {}
    }

    InputAction::None
}
