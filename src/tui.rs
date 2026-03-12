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
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame, Terminal,
};
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use crate::agent::AgentEvent;
use crate::tools::{DiffInfo, DiffLineTag, ToolResult};

// colors matching a dark theme similar to the mockup
const BG: Color = Color::Rgb(11, 14, 20);
const FG: Color = Color::Rgb(191, 189, 182);
const MUTED: Color = Color::Rgb(108, 115, 128);
const ACCENT: Color = Color::Rgb(230, 180, 80);
const GREEN: Color = Color::Rgb(170, 217, 76);
const RED: Color = Color::Rgb(240, 113, 120);
const YELLOW: Color = Color::Rgb(255, 180, 84);
#[allow(dead_code)]
const ORANGE: Color = Color::Rgb(255, 143, 64);
const DIM: Color = Color::Rgb(86, 91, 102);

#[derive(Debug, Clone)]
pub enum ActivityPhase {
    Reading,
    Editing,
    Thinking,
    Writing,
    Running,
}

#[derive(Debug, Clone)]
struct ToolEntry {
    name: String,
    display: String,
    status: ToolStatus,
    diff: Option<DiffInfo>,
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
    // token rate samples: (timestamp, tokens_delta)
    rate_samples: Vec<(Instant, u32)>,
    start_time: Instant,
    // activity timeline
    phases: Vec<(Instant, ActivityPhase)>,
}

impl TokenStats {
    fn new() -> Self {
        Self {
            total_input: 0,
            total_output: 0,
            rate_samples: Vec::new(),
            start_time: Instant::now(),
            phases: Vec::new(),
        }
    }

    fn total_tokens(&self) -> u32 {
        self.total_input + self.total_output
    }

    fn cost_usd(&self) -> f64 {
        // approximate pricing for claude sonnet
        let input_cost = self.total_input as f64 * 3.0 / 1_000_000.0;
        let output_cost = self.total_output as f64 * 15.0 / 1_000_000.0;
        input_cost + output_cost
    }

    fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    fn current_rate(&self) -> f64 {
        let now = Instant::now();
        let window = Duration::from_secs(5);
        let recent: u32 = self
            .rate_samples
            .iter()
            .filter(|(t, _)| now.duration_since(*t) < window)
            .map(|(_, n)| n)
            .sum();
        recent as f64 / window.as_secs_f64()
    }

    fn avg_rate(&self) -> f64 {
        let elapsed = self.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            self.total_output as f64 / elapsed
        } else {
            0.0
        }
    }

    fn peak_rate(&self) -> f64 {
        // compute peak over rolling 2s windows
        if self.rate_samples.is_empty() {
            return 0.0;
        }
        let window = Duration::from_secs(2);
        let mut peak = 0.0f64;
        for sample in &self.rate_samples {
            let sum: u32 = self
                .rate_samples
                .iter()
                .filter(|(t, _)| {
                    *t >= sample.0 && t.duration_since(sample.0) < window
                })
                .map(|(_, n)| n)
                .sum();
            let rate = sum as f64 / window.as_secs_f64();
            if rate > peak {
                peak = rate;
            }
        }
        peak
    }

    // sparkline data: token counts per bucket over the last 2 minutes
    fn sparkline_data(&self, buckets: usize) -> Vec<u64> {
        let now = Instant::now();
        let total_window = Duration::from_secs(120);
        let bucket_width = total_window.as_secs_f64() / buckets as f64;

        let mut data = vec![0u64; buckets];
        for (t, n) in &self.rate_samples {
            let age = now.duration_since(*t).as_secs_f64();
            if age > total_window.as_secs_f64() {
                continue;
            }
            let idx = ((total_window.as_secs_f64() - age) / bucket_width) as usize;
            let idx = idx.min(buckets - 1);
            data[idx] += *n as u64;
        }
        data
    }

    fn record_output_tokens(&mut self, count: u32) {
        self.total_output += count;
        self.rate_samples.push((Instant::now(), count));
    }

    fn record_input_tokens(&mut self, count: u32) {
        self.total_input += count;
    }

    fn record_phase(&mut self, phase: ActivityPhase) {
        self.phases.push((Instant::now(), phase));
    }
}

pub struct App {
    input: String,
    cursor_pos: usize,
    tool_entries: Vec<ToolEntry>,
    text_output: String,
    thinking_text: String,
    current_message: Option<String>,
    token_stats: TokenStats,
    pub is_running: bool,
    pub should_quit: bool,
    pub scroll_offset: u16,
    model_name: String,
    cwd: String,
    pub expanded_diffs: std::collections::HashSet<usize>,
    current_tool_input: String,
}

impl App {
    pub fn new(model_name: &str, cwd: &str) -> Self {
        Self {
            input: String::new(),
            cursor_pos: 0,
            tool_entries: Vec::new(),
            text_output: String::new(),
            thinking_text: String::new(),
            current_message: None,
            token_stats: TokenStats::new(),
            is_running: false,
            should_quit: false,
            scroll_offset: 0,
            model_name: model_name.to_string(),
            cwd: cwd.to_string(),
            expanded_diffs: std::collections::HashSet::new(),
            current_tool_input: String::new(),
        }
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Thinking(t) => {
                self.thinking_text.push_str(&t);
                self.token_stats.record_phase(ActivityPhase::Thinking);
            }
            AgentEvent::Text(t) => {
                self.text_output.push_str(&t);
                // rough estimate: ~4 chars per token
                let approx_tokens = (t.len() as u32 / 4).max(1);
                self.token_stats.record_output_tokens(approx_tokens);
            }
            AgentEvent::ToolStart { id: _, name } => {
                let phase = match name.as_str() {
                    "read" => ActivityPhase::Reading,
                    "edit" => ActivityPhase::Editing,
                    "write" => ActivityPhase::Writing,
                    "bash" => ActivityPhase::Running,
                    _ => ActivityPhase::Running,
                };
                self.token_stats.record_phase(phase);
                self.current_tool_input.clear();

                self.tool_entries.push(ToolEntry {
                    name: name.clone(),
                    display: format!("{}...", name),
                    status: ToolStatus::Running,
                    diff: None,
                });
            }
            AgentEvent::ToolInputDelta(json) => {
                self.current_tool_input.push_str(&json);
                // try to parse partial json to update display
                if let Some(entry) = self.tool_entries.last_mut() {
                    if let Ok(partial) =
                        serde_json::from_str::<serde_json::Value>(&self.current_tool_input)
                    {
                        entry.display = format_tool_display(&entry.name, &partial);
                    }
                }
                let approx_tokens = (json.len() as u32 / 4).max(1);
                self.token_stats.record_output_tokens(approx_tokens);
            }
            AgentEvent::ToolComplete { id: _, name, result } => {
                if let Some(entry) = self.tool_entries.last_mut() {
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
                self.token_stats.record_input_tokens(input_tokens);
                self.token_stats.record_output_tokens(output_tokens);
            }
            AgentEvent::TurnComplete => {
                self.is_running = false;
            }
            AgentEvent::Error(e) => {
                self.is_running = false;
                self.text_output.push_str(&format!("\n[error] {}", e));
            }
        }
    }

    pub fn start_new_message(&mut self, message: &str) {
        self.current_message = Some(message.to_string());
        self.tool_entries.clear();
        self.text_output.clear();
        self.thinking_text.clear();
        self.is_running = true;
        self.scroll_offset = 0;
        self.expanded_diffs.clear();
        self.current_tool_input.clear();
        self.token_stats.start_time = Instant::now();
        self.token_stats.rate_samples.clear();
        self.token_stats.phases.clear();
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
            let short = if cmd.len() > 60 {
                format!("{}...", &cmd[..57])
            } else {
                cmd.to_string()
            };
            format!("Bash {}", short)
        }
        _ => format!("{} ...", name),
    }
}

fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    format!("{:02}:{:02}:{:02}", hours, mins, secs)
}

// sparkline chars
const SPARK_CHARS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn sparkline_string(data: &[u64]) -> String {
    let max = data.iter().copied().max().unwrap_or(1).max(1);
    data.iter()
        .map(|&v| {
            if v == 0 {
                ' '
            } else {
                let idx = ((v as f64 / max as f64) * 7.0) as usize;
                SPARK_CHARS[idx.min(7)]
            }
        })
        .collect()
}

pub fn render(frame: &mut Frame, app: &App) {
    let size = frame.area();

    // main layout: header, activity, metrics footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header with cwd + model
            Constraint::Length(4),  // user message
            Constraint::Min(10),   // activity feed + diffs
            Constraint::Length(6), // metrics panel
            Constraint::Length(1), // status bar
        ])
        .split(size);

    render_header(frame, app, chunks[0]);
    render_user_message(frame, app, chunks[1]);
    render_activity(frame, app, chunks[2]);
    render_metrics(frame, app, chunks[3]);
    render_status_bar(frame, app, chunks[4]);

    // render input overlay if not running
    if !app.is_running {
        render_input(frame, app, chunks[1]);
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let header_text = vec![
        Line::from(vec![
            Span::styled("rum", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("  {}  ", app.cwd),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                &app.model_name,
                Style::default().fg(DIM),
            ),
        ]),
    ];

    let header = Paragraph::new(header_text)
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(DIM)),
        )
        .style(Style::default().bg(BG));

    frame.render_widget(header, area);
}

fn render_user_message(frame: &mut Frame, app: &App, area: Rect) {
    if app.is_running {
        if let Some(ref msg) = app.current_message {
            let lines = vec![Line::from(vec![
                Span::styled("› ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                Span::styled(msg.as_str(), Style::default().fg(FG)),
            ])];

            let widget = Paragraph::new(lines)
                .style(Style::default().bg(BG))
                .wrap(Wrap { trim: false });
            frame.render_widget(widget, area);
        }
    }
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let input_lines = vec![Line::from(vec![
        Span::styled("› ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(&app.input, Style::default().fg(FG)),
        Span::styled("█", Style::default().fg(ACCENT)),
    ])];

    let widget = Paragraph::new(input_lines)
        .style(Style::default().bg(BG))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn render_activity(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    // tool entries
    for (i, entry) in app.tool_entries.iter().enumerate() {
        let _status_style = match &entry.status {
            ToolStatus::Running => Style::default().fg(YELLOW),
            ToolStatus::Complete => Style::default().fg(FG),
            ToolStatus::Error(_) => Style::default().fg(RED),
        };

        let mut spans = Vec::new();

        match &entry.status {
            ToolStatus::Running => {
                spans.push(Span::styled("◌ ", Style::default().fg(YELLOW)));
                spans.push(Span::styled(
                    &entry.display,
                    Style::default().fg(YELLOW),
                ));
                spans.push(Span::styled("...", Style::default().fg(YELLOW)));
            }
            ToolStatus::Complete => {
                spans.push(Span::styled("  ", Style::default().fg(FG)));
                spans.push(Span::styled(&entry.display, Style::default().fg(FG)));

                // show diff stats inline
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
            }
            ToolStatus::Error(_e) => {
                spans.push(Span::styled("✗ ", Style::default().fg(RED)));
                spans.push(Span::styled(&entry.display, Style::default().fg(RED)));
            }
        }

        lines.push(Line::from(spans));

        // render expanded diff if present and entry is complete
        if let Some(ref diff) = entry.diff {
            if app.expanded_diffs.contains(&i) {
                render_diff_lines(&mut lines, diff);
            }
        }
    }

    // text output from the model, rendered as markdown
    if !app.text_output.is_empty() {
        lines.push(Line::from(""));
        let mut md = crate::markdown::TuiMarkdownRenderer::new();
        let md_lines = md.render_lines(&app.text_output);
        lines.extend(md_lines);
    }

    // if thinking and nothing else yet, show thinking indicator
    if app.is_running && app.tool_entries.is_empty() && app.text_output.is_empty() {
        if !app.thinking_text.is_empty() {
            lines.push(Line::from(Span::styled(
                "thinking...",
                Style::default().fg(MUTED),
            )));
        }
    }

    let activity = Paragraph::new(lines)
        .style(Style::default().bg(BG))
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset, 0));

    frame.render_widget(activity, area);
}

fn render_diff_lines(lines: &mut Vec<Line>, diff: &DiffInfo) {
    for hunk in &diff.hunks {
        for dl in &hunk.lines {
            let (prefix, color) = match dl.tag {
                DiffLineTag::Equal => (" ", DIM),
                DiffLineTag::Insert => ("+", GREEN),
                DiffLineTag::Delete => ("-", RED),
            };

            let content = dl.content.trim_end_matches('\n');
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {}", prefix),
                    Style::default().fg(color),
                ),
                Span::styled(
                    content.to_string(),
                    Style::default().fg(color),
                ),
            ]));
        }
        lines.push(Line::from(""));
    }
}

fn render_metrics(frame: &mut Frame, app: &App, area: Rect) {
    let stats = &app.token_stats;

    let sparkline_data = stats.sparkline_data(40);
    let spark_str = sparkline_string(&sparkline_data);
    let current_rate = stats.current_rate();

    let mut lines = Vec::new();

    // token burn rate header
    lines.push(Line::from(vec![
        Span::styled("TOKEN BURN RATE", Style::default().fg(MUTED)),
        Span::styled(
            format!(
                "{}",
                " ".repeat(
                    area.width
                        .saturating_sub(15 + 15) as usize
                ),
            ),
            Style::default(),
        ),
        Span::styled(
            format!("{:.0} tok/s", current_rate),
            Style::default().fg(GREEN),
        ),
    ]));

    // sparkline
    lines.push(Line::from(vec![
        Span::styled(&spark_str, Style::default().fg(GREEN)),
    ]));

    // timeline: "2m ago" ... activity phases ... "now"
    let mut timeline_spans = vec![
        Span::styled("2m ago  ", Style::default().fg(MUTED)),
    ];

    // build phase indicators
    let phase_display = build_phase_display(stats, area.width.saturating_sub(16) as usize);
    timeline_spans.push(Span::styled(phase_display, Style::default().fg(MUTED)));
    timeline_spans.push(Span::styled("  now", Style::default().fg(MUTED)));

    lines.push(Line::from(timeline_spans));

    let metrics = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(DIM)),
        )
        .style(Style::default().bg(BG));

    frame.render_widget(metrics, area);
}

fn build_phase_display(stats: &TokenStats, width: usize) -> String {
    if stats.phases.is_empty() || width == 0 {
        return " ".repeat(width);
    }

    let now = Instant::now();
    let window = Duration::from_secs(120);
    let mut display = vec![' '; width];

    for (_i, (time, phase)) in stats.phases.iter().enumerate() {
        let age = now.duration_since(*time).as_secs_f64();
        if age > window.as_secs_f64() {
            continue;
        }
        let pos = ((window.as_secs_f64() - age) / window.as_secs_f64() * width as f64) as usize;
        let pos = pos.min(width - 1);
        let ch = match phase {
            ActivityPhase::Reading => '·',
            ActivityPhase::Editing => '█',
            ActivityPhase::Thinking => '·',
            ActivityPhase::Writing => '█',
            ActivityPhase::Running => '▪',
        };
        display[pos] = ch;
    }

    display.into_iter().collect()
}

fn render_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let stats = &app.token_stats;
    let elapsed = format_duration(stats.elapsed());
    let cost = stats.cost_usd();
    let avg = stats.avg_rate();
    let peak = stats.peak_rate();
    let total = stats.total_tokens();

    let status = Line::from(vec![
        Span::styled(
            format!("total: {} tokens", total),
            Style::default().fg(MUTED),
        ),
        Span::styled("    ", Style::default()),
        Span::styled(
            format!("cost: ${:.3}", cost),
            Style::default().fg(MUTED),
        ),
        Span::styled("    ", Style::default()),
        Span::styled(
            format!("avg: {:.0} tok/s", avg),
            Style::default().fg(MUTED),
        ),
        Span::styled("    ", Style::default()),
        Span::styled(
            format!("peak: {:.0} tok/s", peak),
            Style::default().fg(if peak > 1000.0 { GREEN } else { MUTED }),
        ),
        Span::styled("    ", Style::default()),
        Span::styled(
            format!("elapsed: {}", elapsed),
            Style::default().fg(MUTED),
        ),
    ]);

    let widget = Paragraph::new(vec![status]).style(Style::default().bg(BG));
    frame.render_widget(widget, area);
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
    ToggleDiff(usize),
    None,
}

pub fn handle_key_event(key: KeyEvent, app: &mut App) -> InputAction {
    // ctrl+c to quit
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if app.is_running {
            // just cancel? for now, quit
            return InputAction::Quit;
        }
        if app.input.is_empty() {
            return InputAction::Quit;
        }
        app.input.clear();
        app.cursor_pos = 0;
        return InputAction::None;
    }

    // escape to quit if running
    if key.code == KeyCode::Esc {
        return InputAction::Quit;
    }

    if app.is_running {
        // while running, only handle scrolling
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

    // input mode
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
            // ctrl+o to toggle diff expansion
            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'o' {
                // toggle the last completed diff
                let last_diff_idx = app
                    .tool_entries
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, e)| e.diff.is_some())
                    .map(|(i, _)| i);
                if let Some(idx) = last_diff_idx {
                    return InputAction::ToggleDiff(idx);
                }
                return InputAction::None;
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
