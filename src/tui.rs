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
    widgets::{Gauge, Paragraph, Sparkline, Wrap},
    Frame, Terminal,
};
use std::io::{self, Stdout};
use std::time::Instant;
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

// context window sizes for common models (tokens)
const DEFAULT_CONTEXT: u32 = 200_000;

// activity feed items: text blocks and tool calls interleaved
#[derive(Debug, Clone)]
enum ActivityItem {
    Text(String),
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

pub struct App {
    input: String,
    cursor_pos: usize,
    activity: Vec<ActivityItem>,
    current_message: Option<String>,
    // token accounting
    total_input: u32,
    total_output: u32,
    context_limit: u32,
    // output rate tracking for sparkline
    rate_samples: Vec<u64>,
    rate_bucket_tokens: u32,
    last_sample: Instant,
    // thinking state (separate from activity feed)
    thinking_text: String,
    is_thinking: bool,
    // general state
    pub is_running: bool,
    pub should_quit: bool,
    pub scroll_offset: u16,
    model_name: String,
    cwd: String,
    current_tool_input: String,
    start_time: Option<Instant>,
}

impl App {
    pub fn new(model_name: &str, cwd: &str) -> Self {
        let context_limit = guess_context_limit(model_name);
        Self {
            input: String::new(),
            cursor_pos: 0,
            activity: Vec::new(),
            current_message: None,
            total_input: 0,
            total_output: 0,
            context_limit,
            rate_samples: Vec::new(),
            rate_bucket_tokens: 0,
            last_sample: Instant::now(),
            thinking_text: String::new(),
            is_thinking: false,
            is_running: false,
            should_quit: false,
            scroll_offset: 0,
            model_name: model_name.to_string(),
            cwd: cwd.to_string(),
            current_tool_input: String::new(),
            start_time: None,
        }
    }

    // sample the output rate into the sparkline buffer.
    // called every frame (~16ms), collapses into 500ms buckets.
    pub fn tick_rate(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_sample).as_millis() >= 500 {
            self.rate_samples.push(self.rate_bucket_tokens as u64);
            self.rate_bucket_tokens = 0;
            self.last_sample = now;
            // keep the last 120 samples (60s at 500ms intervals)
            if self.rate_samples.len() > 120 {
                self.rate_samples.remove(0);
            }
        }
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Thinking(t) => {
                self.is_thinking = true;
                self.thinking_text.push_str(&t);
            }
            AgentEvent::Text(t) => {
                if self.is_thinking {
                    self.is_thinking = false;
                    self.thinking_text.clear();
                }
                let approx = (t.len() as u32 / 4).max(1);
                self.rate_bucket_tokens += approx;
                if let Some(ActivityItem::Text(ref mut s)) = self.activity.last_mut() {
                    s.push_str(&t);
                } else {
                    self.activity.push(ActivityItem::Text(t));
                }
            }
            AgentEvent::ToolStart { id: _, name } => {
                if self.is_thinking {
                    self.is_thinking = false;
                    self.thinking_text.clear();
                }
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
                self.total_input += input_tokens;
                self.total_output += output_tokens;
            }
            AgentEvent::TurnComplete => {
                self.is_running = false;
                self.is_thinking = false;
            }
            AgentEvent::Error(e) => {
                self.is_running = false;
                self.is_thinking = false;
                self.activity.push(ActivityItem::Text(format!("[error] {}", e)));
            }
        }
    }

    pub fn start_new_message(&mut self, message: &str) {
        self.current_message = Some(message.to_string());
        self.activity.clear();
        self.thinking_text.clear();
        self.is_thinking = false;
        self.is_running = true;
        self.scroll_offset = 0;
        self.current_tool_input.clear();
        self.start_time = Some(Instant::now());
    }

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

    fn context_used(&self) -> u32 {
        self.total_input + self.total_output
    }

    fn context_pct(&self) -> f64 {
        if self.context_limit == 0 { return 0.0; }
        (self.context_used() as f64 / self.context_limit as f64).min(1.0)
    }

    fn cost_usd(&self) -> f64 {
        let input_cost = self.total_input as f64 * 3.0 / 1_000_000.0;
        let output_cost = self.total_output as f64 * 15.0 / 1_000_000.0;
        input_cost + output_cost
    }

    fn elapsed_secs(&self) -> f64 {
        self.start_time.map_or(0.0, |t| t.elapsed().as_secs_f64())
    }

    fn avg_rate(&self) -> f64 {
        let elapsed = self.elapsed_secs();
        if elapsed > 0.0 {
            self.total_output as f64 / elapsed
        } else {
            0.0
        }
    }
}

fn guess_context_limit(model: &str) -> u32 {
    let m = model.to_lowercase();
    if m.contains("opus") || m.contains("sonnet") || m.contains("haiku") {
        DEFAULT_CONTEXT
    } else {
        DEFAULT_CONTEXT
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

// plain line with indent (no bar) for tool entries
fn tool_line(spans: Vec<Span<'static>>) -> Line<'static> {
    let mut all = vec![Span::styled("  ", Style::default())];
    all.extend(spans);
    Line::from(all)
}

pub fn render(frame: &mut Frame, app: &App) {
    let size = frame.area();

    // bottom panel: 3 lines for metrics (sparkline + context + stats)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(2), // user message / input
            Constraint::Min(6),   // activity feed
            Constraint::Length(3), // metrics panel
        ])
        .split(size);

    render_header(frame, app, chunks[0]);

    if app.is_running {
        render_user_message(frame, app, chunks[1]);
    } else {
        render_input(frame, app, chunks[1]);
    }

    render_activity(frame, app, chunks[2]);
    render_metrics(frame, app, chunks[3]);
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![
        Span::styled("rum", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("  {}  ", app.cwd),
            Style::default().fg(FG),
        ),
        Span::styled(
            &app.model_name,
            Style::default().fg(MUTED),
        ),
    ];

    // right-align cost
    let cost = app.cost_usd();
    let total = app.context_used();
    if total > 0 {
        let left_len = 4 + app.cwd.len() + 4 + app.model_name.len();
        let stats_str = format!("{} tokens  ${:.3}", total, cost);
        let padding = (area.width as usize).saturating_sub(left_len + stats_str.len());
        spans.push(Span::styled(" ".repeat(padding), Style::default()));
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
                        lines.push(tool_line(vec![
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

                        lines.push(tool_line(spans));

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
                        lines.push(tool_line(vec![
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

    // thinking indicator when the model is actively thinking
    if app.is_thinking {
        let dots = match (app.elapsed_secs() * 2.0) as u32 % 4 {
            0 => ".",
            1 => "..",
            2 => "...",
            _ => "",
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  thinking{}", dots),
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            ),
        ]));
    }

    // waiting indicator when running but nothing yet
    if app.is_running && !app.is_thinking && lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  waiting...",
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        )));
    }

    let activity = Paragraph::new(lines)
        .style(Style::default().bg(BG))
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset, 0));

    frame.render_widget(activity, area);
}

fn render_metrics(frame: &mut Frame, app: &App, area: Rect) {
    // split: left side for sparkline, right side for context gauge
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(60),
            Constraint::Percentage(40),
        ])
        .split(area);

    // left: sparkline + stats label
    let left_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // stats line
            Constraint::Min(1),   // sparkline
        ])
        .split(cols[0]);

    let elapsed = app.elapsed_secs();
    let rate = app.avg_rate();
    let stats_line = Line::from(vec![
        Span::styled("out ", Style::default().fg(DIM)),
        Span::styled(
            format!("{}", app.total_output),
            Style::default().fg(MUTED),
        ),
        Span::styled("  in ", Style::default().fg(DIM)),
        Span::styled(
            format!("{}", app.total_input),
            Style::default().fg(MUTED),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{:.0} tok/s", rate),
            Style::default().fg(ACCENT),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{:.1}s", elapsed),
            Style::default().fg(DIM),
        ),
    ]);

    let stats_widget = Paragraph::new(stats_line)
        .style(Style::default().bg(BG));
    frame.render_widget(stats_widget, left_rows[0]);

    let spark_data: Vec<u64> = if app.rate_samples.is_empty() {
        vec![0]
    } else {
        app.rate_samples.clone()
    };

    let sparkline = Sparkline::default()
        .data(&spark_data)
        .style(Style::default().fg(ACCENT).bg(BG));
    frame.render_widget(sparkline, left_rows[1]);

    // right: context gauge
    let right_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // label
            Constraint::Min(1),   // gauge
        ])
        .split(cols[1]);

    let pct = app.context_pct();
    let used = app.context_used();
    let limit = app.context_limit;
    let context_label = Line::from(vec![
        Span::styled("context ", Style::default().fg(DIM)),
        Span::styled(
            format!("{}k / {}k", used / 1000, limit / 1000),
            Style::default().fg(MUTED),
        ),
        Span::styled(
            format!("  {:.0}%", pct * 100.0),
            Style::default().fg(if pct > 0.8 { RED } else if pct > 0.6 { YELLOW } else { MUTED }),
        ),
    ]);

    let label_widget = Paragraph::new(context_label)
        .style(Style::default().bg(BG));
    frame.render_widget(label_widget, right_rows[0]);

    let gauge_color = if pct > 0.8 { RED } else if pct > 0.6 { YELLOW } else { ACCENT };

    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(gauge_color).bg(Color::Rgb(30, 33, 40)))
        .ratio(pct)
        .label("")
        .style(Style::default().bg(BG));
    frame.render_widget(gauge, right_rows[1]);
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
        KeyCode::PageUp => {
            app.scroll_offset = app.scroll_offset.saturating_sub(10);
            return InputAction::None;
        }
        KeyCode::PageDown => {
            app.scroll_offset = app.scroll_offset.saturating_add(10);
            return InputAction::None;
        }
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
