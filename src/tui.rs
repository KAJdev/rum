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
use std::time::Instant;
use unicode_width::UnicodeWidthStr;
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

const DEFAULT_CONTEXT: u32 = 200_000;

// the bar prefix string and its display width
const BAR_STR: &str = "\u{2502} ";
const BAR_WIDTH: u16 = 2;

#[derive(Debug, Clone)]
enum ActivityItem {
    // thinking text from the model, shown dim/italic
    Thinking(String),
    // text output from the model, rendered as markdown with bar prefix
    Text(String),
    // tool call entry
    Tool(ToolEntry),
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
}

#[derive(Debug, Clone)]
enum ToolStatus {
    Running,
    // exit_code is Some for bash commands
    Complete { exit_code: Option<i32> },
    Error(String),
}

pub struct App {
    input: String,
    cursor_pos: usize,
    activity: Vec<ActivityItem>,
    current_message: Option<String>,
    // summed across all api calls (for cost calculation)
    total_input: u32,
    total_output: u32,
    // from the most recent api call (for context window display).
    // each call's input_tokens already includes the full conversation
    // history, so these reflect actual context window usage.
    last_input: u32,
    last_output: u32,
    context_limit: u32,
    rate_samples: Vec<u64>,
    rate_bucket_tokens: u32,
    last_sample: Instant,
    pub is_running: bool,
    pub should_quit: bool,
    pub scroll_offset: u16,
    // when true, viewport follows new content to the bottom
    auto_scroll: bool,
    model_name: String,
    cwd: String,
    current_tool_input: String,
    start_time: Option<Instant>,
    // cached terminal width for manual line wrapping
    term_width: u16,
}

impl App {
    pub fn new(model_name: &str, cwd: &str) -> Self {
        let context_limit = guess_context_limit(model_name);
        let term_width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
        Self {
            input: String::new(),
            cursor_pos: 0,
            activity: Vec::new(),
            current_message: None,
            total_input: 0,
            total_output: 0,
            last_input: 0,
            last_output: 0,
            context_limit,
            rate_samples: Vec::new(),
            rate_bucket_tokens: 0,
            last_sample: Instant::now(),
            is_running: false,
            should_quit: false,
            scroll_offset: 0,
            auto_scroll: true,
            model_name: model_name.to_string(),
            cwd: cwd.to_string(),
            current_tool_input: String::new(),
            start_time: None,
            term_width,
        }
    }

    pub fn tick_rate(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_sample).as_millis() >= 500 {
            self.rate_samples.push(self.rate_bucket_tokens as u64);
            self.rate_bucket_tokens = 0;
            self.last_sample = now;
            if self.rate_samples.len() > 120 {
                self.rate_samples.remove(0);
            }
        }
        // refresh terminal width periodically
        if let Ok((w, _)) = crossterm::terminal::size() {
            self.term_width = w;
        }
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Thinking(t) => {
                if let Some(ActivityItem::Thinking(ref mut s)) = self.activity.last_mut() {
                    s.push_str(&t);
                } else {
                    self.activity.push(ActivityItem::Thinking(t));
                }
            }
            AgentEvent::Text(t) => {
                let approx = (t.len() as u32 / 4).max(1);
                self.rate_bucket_tokens += approx;
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
                    arg: String::new(),
                    status: ToolStatus::Running,
                    diff: None,
                    output: None,
                    expanded: false,
                }));
            }
            AgentEvent::ToolInputDelta(json) => {
                self.current_tool_input.push_str(&json);
                if let Some(ActivityItem::Tool(ref mut entry)) = self.activity.last_mut() {
                    if let Ok(partial) =
                        serde_json::from_str::<serde_json::Value>(&self.current_tool_input)
                    {
                        entry.arg = extract_tool_arg(&entry.name, &partial);
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

                            // store output for display (bash output, truncated)
                            let trimmed = output.trim();
                            if !trimmed.is_empty() && trimmed != "(no output)" {
                                let display_output = if trimmed.len() > 2000 {
                                    format!("{}...", &trimmed[..2000])
                                } else {
                                    trimmed.to_string()
                                };
                                entry.output = Some(display_output);
                            }

                            entry.status = ToolStatus::Complete { exit_code };
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
                self.last_input = input_tokens;
                self.last_output = output_tokens;
            }
            AgentEvent::TurnComplete => {
                self.is_running = false;
            }
            AgentEvent::Error(e) => {
                self.is_running = false;
                self.activity.push(ActivityItem::Text(format!("[error] {}", e)));
            }
        }
    }

    pub fn start_new_message(&mut self, message: &str) {
        self.current_message = Some(message.to_string());
        self.activity.clear();
        self.is_running = true;
        self.scroll_offset = 0;
        self.auto_scroll = true;
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
        self.last_input + self.last_output
    }

    fn context_pct(&self) -> f64 {
        if self.context_limit == 0 { return 0.0; }
        (self.context_used() as f64 / self.context_limit as f64).min(1.0)
    }

    fn cost_usd(&self) -> f64 {
        self.total_input as f64 * 3.0 / 1_000_000.0
            + self.total_output as f64 * 15.0 / 1_000_000.0
    }

    fn avg_rate(&self) -> f64 {
        let elapsed = self.start_time.map_or(0.0, |t| t.elapsed().as_secs_f64());
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

// extract the primary argument (path or command) from streaming tool input
fn extract_tool_arg(name: &str, input: &serde_json::Value) -> String {
    match name {
        "read" | "edit" | "write" => {
            input.get("path").and_then(|v| v.as_str()).unwrap_or("...").to_string()
        }
        "bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("...");
            if cmd.len() > 80 {
                format!("{}...", &cmd[..77])
            } else {
                cmd.to_string()
            }
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

pub fn render(frame: &mut Frame, app: &App) {
    let size = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(2), // user message / input
            Constraint::Min(4),   // activity feed
            Constraint::Length(1), // metrics bar
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
        Span::styled(format!("  {}  ", app.cwd), Style::default().fg(FG)),
        Span::styled(&app.model_name, Style::default().fg(MUTED)),
    ];

    let cost = app.cost_usd();
    let ctx = app.context_used();
    if ctx > 0 {
        let left_len = 4 + app.cwd.len() + 4 + app.model_name.len();
        let ctx_k = ctx / 1000;
        let limit_k = app.context_limit / 1000;
        let stats_str = format!("{}k/{}k  ${:.3}", ctx_k, limit_k, cost);
        let padding = (area.width as usize).saturating_sub(left_len + stats_str.len());
        spans.push(Span::styled(" ".repeat(padding), Style::default()));
        spans.push(Span::styled(stats_str, Style::default().fg(MUTED)));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG)),
        area,
    );
}

fn render_user_message(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(ref msg) = app.current_message {
        let widget = Paragraph::new(Line::from(vec![
            Span::styled("\u{203a} ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(msg.as_str(), Style::default().fg(FG)),
        ]))
        .style(Style::default().bg(BG))
        .wrap(Wrap { trim: false });
        frame.render_widget(widget, area);
    }
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let widget = Paragraph::new(Line::from(vec![
        Span::styled("\u{203a} ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(&app.input, Style::default().fg(FG)),
        Span::styled("\u{2588}", Style::default().fg(ACCENT)),
    ]))
    .style(Style::default().bg(BG))
    .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn render_activity(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    let w = area.width;

    let n = app.activity.len();
    for (idx, item) in app.activity.iter().enumerate() {
        match item {
            ActivityItem::Thinking(text) => {
                let text = text.clone();
                // empty line before thinking if preceded by something
                if idx > 0 {
                    lines.push(Line::from(""));
                }
                let style = Style::default().fg(DIM).add_modifier(Modifier::ITALIC);
                let wrapped = wrap_text_with_bar(&text, w, style);
                lines.extend(wrapped);
                // empty line after thinking
                if idx + 1 < n {
                    lines.push(Line::from(""));
                }
            }
            ActivityItem::Text(text) => {
                let text = text.clone();
                // empty line before text if preceded by something
                if idx > 0 {
                    lines.push(Line::from(""));
                }

                let mut md = crate::markdown::TuiMarkdownRenderer::new();
                let md_lines = md.render_lines(&text);
                let wrapped = wrap_md_lines_with_bar(md_lines, w);
                lines.extend(wrapped);

                // empty line after text
                if idx + 1 < n {
                    lines.push(Line::from(""));
                }
            }
            ActivityItem::Tool(entry) => {
                render_tool_entry(&mut lines, entry, w);
            }
        }
    }

    // thinking / waiting indicators at the tail
    if app.is_running && lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  waiting...",
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        )));
    }

    // don't use Wrap here since we do manual wrapping above
    let total_lines = lines.len() as u16;
    let scroll = if app.auto_scroll {
        total_lines.saturating_sub(area.height)
    } else {
        app.scroll_offset
    };
    let activity = Paragraph::new(lines)
        .style(Style::default().bg(BG))
        .scroll((scroll, 0));

    frame.render_widget(activity, area);
}

fn render_tool_entry(lines: &mut Vec<Line<'static>>, entry: &ToolEntry, _w: u16) {
    let label = capitalize_tool(&entry.name);
    let has_output = entry.output.is_some() || entry.diff.is_some();

    match &entry.status {
        ToolStatus::Running => {
            let mut spans = vec![
                Span::styled("\u{25cc} ", Style::default().fg(YELLOW)),
                Span::styled(label.to_string(), Style::default().fg(YELLOW)),
            ];
            if !entry.arg.is_empty() {
                spans.push(Span::styled(
                    format!(" {}", entry.arg),
                    Style::default().fg(MUTED),
                ));
            }
            lines.push(tool_line(spans));
        }
        ToolStatus::Complete { exit_code } => {
            let mut spans = vec![];

            // exit status indicator for bash
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
            }

            // tool name in accent, argument in muted
            spans.push(Span::styled(
                label.to_string(),
                Style::default().fg(ACCENT),
            ));
            if !entry.arg.is_empty() {
                spans.push(Span::styled(
                    format!(" {}", entry.arg),
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

            // bash output (first few lines, indented further)
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

            // expanded diff lines
            if entry.expanded {
                if let Some(ref diff) = entry.diff {
                    lines.extend(build_diff_lines(diff));
                }
            }

            // blank line after tools that produced output
            if has_output {
                lines.push(Line::from(""));
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

fn render_metrics(frame: &mut Frame, app: &App, area: Rect) {
    // single-line metrics: sparkline | stats | context bar
    //
    // layout: [sparkline 12 chars] [stats] [padding] [context]
    let w = area.width as usize;

    let rate = app.avg_rate();
    let pct = app.context_pct();
    let used_k = app.context_used() / 1000;
    let limit_k = app.context_limit / 1000;

    // context bar: 8 chars wide
    let ctx_bar_width: usize = 8;
    let filled = ((pct * ctx_bar_width as f64).round() as usize).min(ctx_bar_width);
    let empty = ctx_bar_width - filled;
    let ctx_color = if pct > 0.8 { RED } else if pct > 0.6 { YELLOW } else { ACCENT };

    // build the right side: "  123k/200k [========] 62%"
    let ctx_label = format!("{}k/{}k", used_k, limit_k);
    let ctx_pct = format!("{:.0}%", pct * 100.0);

    // build spans from left to right
    let mut spans: Vec<Span> = Vec::new();

    // sparkline as braille-style bar using block chars
    let spark_width: usize = 16;
    let spark_str = render_inline_sparkline(&app.rate_samples, spark_width);
    spans.push(Span::styled(spark_str, Style::default().fg(ACCENT)));
    spans.push(Span::styled(" ", Style::default()));

    // stats
    spans.push(Span::styled(
        format!("{:.0} tok/s", rate),
        Style::default().fg(MUTED),
    ));
    spans.push(Span::styled("  ", Style::default()));
    spans.push(Span::styled(
        format!("${:.3}", app.cost_usd()),
        Style::default().fg(DIM),
    ));

    // compute padding between stats and context
    let left_used: usize = spark_width + 1
        + format!("{:.0} tok/s", rate).len() + 2
        + format!("${:.3}", app.cost_usd()).len();
    let right_len = 2 + ctx_label.len() + 1 + 1 + ctx_bar_width + 1 + 1 + ctx_pct.len();
    let pad = w.saturating_sub(left_used + right_len);
    spans.push(Span::styled(" ".repeat(pad), Style::default()));

    // context
    spans.push(Span::styled(format!("  {}", ctx_label), Style::default().fg(DIM)));
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
    spans.push(Span::styled(
        ctx_pct,
        Style::default().fg(ctx_color),
    ));

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG)),
        area,
    );
}

// render a tiny inline sparkline using block characters
fn render_inline_sparkline(samples: &[u64], width: usize) -> String {
    let blocks = [' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}'];

    if samples.is_empty() {
        return " ".repeat(width);
    }

    // take the last `width` samples
    let start = samples.len().saturating_sub(width);
    let window = &samples[start..];
    let max = window.iter().copied().max().unwrap_or(1).max(1);

    let mut out = String::with_capacity(width);
    // pad if fewer samples than width
    for _ in 0..(width.saturating_sub(window.len())) {
        out.push(' ');
    }
    for &v in window {
        let idx = ((v as f64 / max as f64) * 8.0).round() as usize;
        out.push(blocks[idx.min(8)]);
    }
    out
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
    Cancel,
    Quit,
    ScrollUp,
    ScrollDown,
    ToggleDiff,
    None,
}

pub fn handle_key_event(key: KeyEvent, app: &mut App) -> InputAction {
    // ctrl+c: cancel if running, quit if idle with empty input, clear input otherwise
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if app.is_running {
            return InputAction::Cancel;
        }
        if app.input.is_empty() {
            return InputAction::Quit;
        }
        app.input.clear();
        app.cursor_pos = 0;
        return InputAction::None;
    }

    // escape: cancel if running, quit if idle
    if key.code == KeyCode::Esc {
        if app.is_running {
            return InputAction::Cancel;
        }
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
