use crossterm::{
    event::{
        KeyCode, KeyEvent, KeyModifiers,
        KeyboardEnhancementFlags, PushKeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
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
const INPUT_COLOR: Color = Color::Rgb(80, 130, 190);

const DEFAULT_CONTEXT: u32 = 200_000;

// tokens accumulated in a single time bucket, broken down by type
#[derive(Clone, Default)]
struct TokenBucket {
    text: u32,
    thinking: u32,
    tool: u32,
    input: u32,
}

impl TokenBucket {
    fn total(&self) -> u64 {
        (self.text + self.thinking + self.tool + self.input) as u64
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
    rate_samples: Vec<TokenBucket>,
    rate_bucket: TokenBucket,
    last_sample: Instant,
    pub is_running: bool,
    pub should_quit: bool,
    pub scroll_offset: u16,
    // when true, viewport follows new content to the bottom
    pub auto_scroll: bool,
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
            rate_bucket: TokenBucket::default(),
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
        if now.duration_since(self.last_sample).as_millis() >= 2000 {
            self.rate_samples.push(self.rate_bucket.clone());
            self.rate_bucket = TokenBucket::default();
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
                let approx = (t.len() as u32 / 4).max(1);
                self.rate_bucket.thinking += approx;
                if let Some(ActivityItem::Thinking(ref mut s)) = self.activity.last_mut() {
                    s.push_str(&t);
                } else {
                    self.activity.push(ActivityItem::Thinking(t));
                }
            }
            AgentEvent::Text(t) => {
                let approx = (t.len() as u32 / 4).max(1);
                self.rate_bucket.text += approx;
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
                self.rate_bucket.input += input_tokens;
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

    // cursor_pos is a char-count offset. convert to byte index for
    // String insert/remove operations.
    fn cursor_byte_pos(&self) -> usize {
        self.input.char_indices()
            .nth(self.cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
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

    fn delete_to_line_start(&mut self) {
        let (_, col) = self.cursor_line_col();
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
    let input_height = if app.is_running {
        app.current_message.as_ref()
            .map(|m| m.split('\n').count() as u16)
            .unwrap_or(1)
            .max(1)
            .min(max_input)
    } else {
        (app.input_line_count() as u16).max(1).min(max_input)
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),           // header
            Constraint::Length(input_height), // input / user message
            Constraint::Length(1),           // buffer after input
            Constraint::Min(4),             // activity feed
            Constraint::Length(1),           // buffer before bottom edge
        ])
        .split(size);

    render_header(frame, app, chunks[0]);

    if app.is_running {
        render_user_message(frame, app, chunks[1]);
    } else {
        render_input(frame, app, chunks[1]);
    }

    // chunks[2] is the buffer after input
    render_activity(frame, app, chunks[3]);
    // chunks[4] is the buffer at the bottom
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let w = area.width as usize;

    let mut spans = vec![
        Span::styled("rum", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {}  ", app.cwd), Style::default().fg(FG)),
        Span::styled(&app.model_name, Style::default().fg(MUTED)),
    ];

    let left_len = 4 + app.cwd.len() + 4 + app.model_name.len();

    // build right-side metrics
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

    // right side width: spark + " " + rate + "  " + cost + "  " + ctx_label + " [" + bar + "] " + pct
    let right_len = spark_width + 1 + rate_str.len() + 2 + cost_str.len() + 2
        + ctx_label.len() + 2 + ctx_bar_width + 2 + ctx_pct.len();

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

fn render_user_message(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(ref msg) = app.current_message {
        let mut lines: Vec<Line> = Vec::new();
        for (i, line_text) in msg.split('\n').enumerate() {
            let prefix = if i == 0 { "\u{203a} " } else { "  " };
            lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                Span::styled(line_text.to_string(), Style::default().fg(FG)),
            ]));
        }
        let widget = Paragraph::new(lines)
            .style(Style::default().bg(BG));
        frame.render_widget(widget, area);
    }
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let input_lines: Vec<&str> = app.input.split('\n').collect();
    let (cursor_line, _cursor_col) = app.cursor_line_col();

    let visible_lines = area.height as usize;
    let input_scroll: u16 = if cursor_line >= visible_lines {
        (cursor_line - visible_lines + 1) as u16
    } else {
        0
    };

    let mut lines: Vec<Line> = Vec::new();
    for (i, line_text) in input_lines.iter().enumerate() {
        let prefix = if i == 0 { "\u{203a} " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(line_text.to_string(), Style::default().fg(FG)),
        ]));
    }

    let widget = Paragraph::new(lines)
        .style(Style::default().bg(BG))
        .scroll((input_scroll, 0));
    frame.render_widget(widget, area);

    // place the native terminal cursor
    let byte_pos = app.cursor_byte_pos();
    let line_start = app.input[..byte_pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let text_before_cursor = &app.input[line_start..byte_pos];
    let display_col = UnicodeWidthStr::width(text_before_cursor) as u16;
    let prefix_width: u16 = 2; // "› " or "  "
    let cx = area.x + prefix_width + display_col;
    let cy = area.y + (cursor_line as u16).saturating_sub(input_scroll);
    frame.set_cursor_position((cx, cy));
}

fn render_activity(frame: &mut Frame, app: &mut App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    let w = area.width;

    let n = app.activity.len();
    for (idx, item) in app.activity.iter().enumerate() {
        match item {
            ActivityItem::Thinking(text) => {
                // only show the most recent paragraph of thinking.
                // thinking tends to be a stream of evolving reasoning;
                // showing just the latest chunk keeps the display focused.
                let para = last_paragraph(text);
                if idx > 0 {
                    lines.push(Line::from(""));
                }
                let style = Style::default().fg(DIM).add_modifier(Modifier::ITALIC);
                let wrapped = wrap_text_with_bar(para, w, style);
                lines.extend(wrapped);
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

    let total_lines = lines.len() as u16;
    let max_scroll = total_lines.saturating_sub(area.height);

    // re-engage auto-scroll when manual scrolling reaches the bottom
    if !app.auto_scroll && app.scroll_offset >= max_scroll {
        app.scroll_offset = max_scroll;
        app.auto_scroll = true;
    }

    let scroll = if app.auto_scroll {
        max_scroll
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
        (bucket.input, INPUT_COLOR),
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
                app.auto_scroll = false;
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
            if shift || alt || ctrl {
                let bp = app.cursor_byte_pos();
                app.input.insert(bp, '\n');
                app.cursor_pos += 1;
            } else if !app.input.is_empty() {
                let msg = app.input.clone();
                app.input.clear();
                app.cursor_pos = 0;
                return InputAction::Submit(msg);
            }
        }
        KeyCode::Backspace => {
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
            if app.input_line_count() > 1 && app.move_cursor_up() {
                // moved within multi-line input
            } else {
                return InputAction::ScrollUp;
            }
        }
        KeyCode::Down => {
            if app.input_line_count() > 1 && app.move_cursor_down() {
                // moved within multi-line input
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
                    'u' => app.delete_to_line_start(),
                    'k' => app.delete_to_line_end(),
                    'w' => app.delete_word_backward(),
                    'j' => {
                        // ctrl+j inserts newline (traditional unix LF)
                        let bp = app.cursor_byte_pos();
                        app.input.insert(bp, '\n');
                        app.cursor_pos += 1;
                    }
                    'o' => return InputAction::ToggleDiff,
                    _ => {}
                }
            } else {
                let bp = app.cursor_byte_pos();
                app.input.insert(bp, c);
                app.cursor_pos += 1;
            }
        }
        _ => {}
    }

    InputAction::None
}
