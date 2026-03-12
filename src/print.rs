use std::io::Write;
use std::time::Instant;

use crate::agent::AgentEvent;
use crate::markdown::MarkdownRenderer;
use crate::tools::{DiffInfo, DiffLineTag, ToolResult};

const DIM: &str = "\x1b[38;5;242m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const BOLD: &str = "\x1b[1m";
const ITALIC: &str = "\x1b[3m";
const RESET: &str = "\x1b[0m";

// left edge bar for all model output, giving a visual "quote" feel
const BAR: &str = "\x1b[38;5;242m\u{2502}\x1b[0m ";
const BAR_DIM: &str = "\x1b[38;5;242m\u{2502} ";

// tracks which json field is currently being streamed
#[derive(Debug, Clone, PartialEq)]
enum StreamingField {
    None,
    Path,
    Command,
    Content,
    OldText,
    NewText,
    Other,
}

// incremental json key/value extractor for streaming tool inputs.
// the anthropic api sends partial json fragments; this parser tracks
// which field is actively streaming so we can display it live.
struct JsonFieldTracker {
    raw: String,
    current_field: StreamingField,
    displayed_up_to: usize,
    header_printed: bool,
    #[allow(dead_code)]
    tool_name: String,
}

impl JsonFieldTracker {
    fn new(tool_name: &str) -> Self {
        Self {
            raw: String::new(),
            current_field: StreamingField::None,
            displayed_up_to: 0,
            header_printed: false,
            tool_name: tool_name.to_string(),
        }
    }

    fn push(&mut self, fragment: &str) -> Vec<PrintAction> {
        self.raw.push_str(fragment);
        self.extract_actions()
    }

    fn extract_actions(&mut self) -> Vec<PrintAction> {
        let mut actions = Vec::new();
        let len = self.raw.len();
        let mut i = self.displayed_up_to;
        let bytes = self.raw.as_bytes();

        while i < len {
            match self.current_field {
                StreamingField::None => {
                    if let Some((key, value_start)) = find_next_key(&self.raw, i) {
                        i = value_start;
                        self.current_field = classify_field(&key);
                        self.displayed_up_to = i;

                        match self.current_field {
                            StreamingField::Path => {}
                            StreamingField::Command => {
                                if !self.header_printed {
                                    self.header_printed = true;
                                }
                                actions.push(PrintAction::StartCommand);
                            }
                            StreamingField::Content | StreamingField::NewText => {
                                actions.push(PrintAction::StartContent(self.extract_path()));
                            }
                            _ => {}
                        }
                    } else {
                        break;
                    }
                }
                _ => {
                    let chunk_start = i;
                    let mut hit_end = false;

                    while i < len {
                        if bytes[i] == b'\\' && i + 1 < len {
                            i += 2;
                            continue;
                        }
                        if bytes[i] == b'"' {
                            hit_end = true;
                            break;
                        }
                        i += 1;
                    }

                    let chunk = &self.raw[chunk_start..i];
                    if !chunk.is_empty() {
                        let decoded = unescape_json_str(chunk);
                        match self.current_field {
                            StreamingField::Command => {
                                actions.push(PrintAction::StreamCommand(decoded));
                            }
                            StreamingField::Content | StreamingField::NewText => {
                                actions.push(PrintAction::StreamContent(decoded));
                            }
                            StreamingField::Path => {
                                actions.push(PrintAction::StreamPath(decoded));
                            }
                            StreamingField::OldText | StreamingField::Other => {}
                            StreamingField::None => {}
                        }
                    }

                    if hit_end {
                        match self.current_field {
                            StreamingField::Command => actions.push(PrintAction::EndCommand),
                            StreamingField::Content | StreamingField::NewText => {
                                actions.push(PrintAction::EndContent);
                            }
                            StreamingField::Path => actions.push(PrintAction::EndPath),
                            _ => {}
                        }
                        self.current_field = StreamingField::None;
                        i += 1;
                    }

                    self.displayed_up_to = i;
                }
            }
        }

        actions
    }

    fn extract_path(&self) -> Option<String> {
        if let Some(start) = self.raw.find("\"path\"") {
            let after = &self.raw[start + 6..];
            let after = after.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
            if after.starts_with('"') {
                let inner = &after[1..];
                if let Some(end) = find_unescaped_quote(inner) {
                    return Some(unescape_json_str(&inner[..end]));
                }
            }
        }
        None
    }
}

#[derive(Debug)]
enum PrintAction {
    StreamPath(String),
    EndPath,
    StartCommand,
    StreamCommand(String),
    EndCommand,
    StartContent(Option<String>),
    StreamContent(String),
    EndContent,
}

fn classify_field(key: &str) -> StreamingField {
    match key {
        "path" => StreamingField::Path,
        "command" => StreamingField::Command,
        "content" => StreamingField::Content,
        "oldText" => StreamingField::OldText,
        "newText" => StreamingField::NewText,
        _ => StreamingField::Other,
    }
}

fn find_next_key(s: &str, from: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let mut i = from;

    while i < bytes.len() {
        if bytes[i] == b'"' {
            let key_start = i + 1;
            i = key_start;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            if i >= bytes.len() {
                return None;
            }
            let key = &s[key_start..i];
            i += 1;

            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b':') {
                i += 1;
            }

            if i < bytes.len() && bytes[i] == b'"' {
                i += 1;
                return Some((key.to_string(), i));
            }
            continue;
        }
        i += 1;
    }
    None
}

fn find_unescaped_quote(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'"' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn unescape_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub struct PrintMode {
    start: Instant,
    model: String,
    // summed across all api calls (for cost calculation)
    total_input: u32,
    total_output: u32,
    // from the most recent api call (for context window display)
    last_input: u32,
    last_output: u32,
    tool_count: u32,
    error_count: u32,
    in_text: bool,
    in_thinking: bool,
    tracker: Option<JsonFieldTracker>,
    streamed_path: String,
    current_tool_name: String,
    md: MarkdownRenderer,
    has_partial: bool,
    // buffer for thinking text; we only display the latest paragraph
    thinking_buf: String,
    // number of lines currently rendered for the thinking paragraph
    thinking_lines_rendered: usize,
}

impl PrintMode {
    pub fn new(model: &str) -> Self {
        Self {
            start: Instant::now(),
            model: model.to_string(),
            total_input: 0,
            total_output: 0,
            last_input: 0,
            last_output: 0,
            tool_count: 0,
            error_count: 0,
            in_text: false,
            in_thinking: false,
            tracker: None,
            streamed_path: String::new(),
            current_tool_name: String::new(),
            md: MarkdownRenderer::new(),
            has_partial: false,
            thinking_buf: String::new(),
            thinking_lines_rendered: 0,
        }
    }

    pub fn print_header(&self, model: &str, cwd: &str, message: &str) {
        eprintln!(
            "{DIM}rum{RESET} {DIM}\u{00b7}{RESET} {BOLD}{model}{RESET} {DIM}\u{00b7}{RESET} {cwd}",
        );
        eprintln!("{DIM}\u{203a} {RESET}{BOLD}{message}{RESET}");
        eprintln!();
    }

    pub fn handle_event(&mut self, evt: AgentEvent) -> bool {
        let mut stderr = std::io::stderr();
        let mut stdout = std::io::stdout();

        match evt {
            AgentEvent::Thinking(t) => {
                if !self.in_thinking {
                    self.in_thinking = true;
                    self.thinking_buf.clear();
                    self.thinking_lines_rendered = 0;
                }

                self.thinking_buf.push_str(&t);

                // extract latest paragraph
                let para = last_paragraph(&self.thinking_buf).to_string();
                let para_lines: Vec<&str> = para.split('\n').collect();

                // clear previously rendered thinking lines
                if self.thinking_lines_rendered > 0 {
                    // move cursor up and clear each line
                    for _ in 0..self.thinking_lines_rendered {
                        eprint!("\x1b[A\x1b[2K");
                    }
                }

                // render the current paragraph
                self.thinking_lines_rendered = 0;
                for (i, line) in para_lines.iter().enumerate() {
                    if i > 0 {
                        eprintln!();
                    }
                    eprint!("{BAR_DIM}{ITALIC}{line}{RESET}");
                    self.thinking_lines_rendered += 1;
                }
                // move to next line so the cursor is at a clean position
                eprintln!();
                let _ = stderr.flush();
            }
            AgentEvent::Text(t) => {
                if self.in_thinking {
                    self.in_thinking = false;
                    self.thinking_buf.clear();
                    self.thinking_lines_rendered = 0;
                    eprintln!();
                }
                if !self.in_text {
                    self.in_text = true;
                }

                // completed lines go through markdown rendering.
                // the current partial line streams raw for responsiveness,
                // then gets cleared and re-rendered when the line completes.
                let lines = self.md.push(&t);

                if !lines.is_empty() {
                    // clear the raw partial we were streaming
                    if self.has_partial {
                        print!("\r\x1b[2K");
                        self.has_partial = false;
                    }
                    for line in &lines {
                        println!("{BAR}{line}");
                    }
                }

                // stream the current partial line as raw text
                if let Some(raw) = self.md.peek_raw() {
                    if self.has_partial {
                        print!("\r\x1b[2K{BAR}{raw}");
                    } else {
                        print!("{BAR}{raw}");
                        self.has_partial = true;
                    }
                    let _ = stdout.flush();
                }
            }
            AgentEvent::ToolStart { id: _, name } => {
                if self.in_thinking {
                    self.in_thinking = false;
                    self.thinking_buf.clear();
                    self.thinking_lines_rendered = 0;
                    eprintln!();
                }
                if self.in_text {
                    if self.has_partial {
                        eprint!("\r\x1b[2K");
                        self.has_partial = false;
                    }
                    for line in self.md.finish() {
                        println!("{BAR}{line}");
                    }
                    println!();
                    self.in_text = false;
                }
                self.tool_count += 1;
                self.current_tool_name = name.clone();
                self.streamed_path.clear();
                self.tracker = Some(JsonFieldTracker::new(&name));

                eprint!("  {DIM}\u{25cc} {name}...{RESET}");
                let _ = stderr.flush();
            }
            AgentEvent::ToolInputDelta(json) => {
                if let Some(ref mut tracker) = self.tracker {
                    let actions = tracker.push(&json);
                    for action in actions {
                        self.handle_print_action(action);
                    }
                }
            }
            AgentEvent::ToolComplete {
                id: _,
                name,
                result,
            } => {
                self.tracker = None;

                match &result {
                    ToolResult::Success { output: _, diff } => {
                        match name.as_str() {
                            "bash" => {
                                // command was already streamed
                            }
                            "write" | "edit" => {
                                if let Some(d) = diff {
                                    eprint!("\r\x1b[2K");
                                    self.print_diff_summary(&name, d);
                                } else {
                                    eprint!("\r\x1b[2K");
                                    eprintln!(
                                        "  {DIM}{} {}{RESET}",
                                        cap_tool(&name),
                                        self.streamed_path,
                                    );
                                }
                            }
                            "read" => {
                                eprint!("\r\x1b[2K");
                                eprintln!("  {DIM}Read {}{RESET}", self.streamed_path);
                            }
                            _ => {
                                eprint!("\r\x1b[2K");
                                eprintln!("  {DIM}{}{RESET}", name);
                            }
                        }
                    }
                    ToolResult::Error(e) => {
                        self.error_count += 1;
                        eprint!("\r\x1b[2K");
                        let short = truncate(e, 120);
                        eprintln!("  {RED}\u{2717} {name} - {short}{RESET}");
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
                if self.in_thinking {
                    self.in_thinking = false;
                    eprintln!("{RESET}");
                }
                if self.in_text {
                    if self.has_partial {
                        eprint!("\r\x1b[2K");
                        self.has_partial = false;
                    }
                    for line in self.md.finish() {
                        println!("{BAR}{line}");
                    }
                    self.in_text = false;
                }
                return true;
            }
            AgentEvent::Status(msg) => {
                eprintln!("{DIM}{msg}{RESET}");
            }
            AgentEvent::Error(e) => {
                self.error_count += 1;
                if self.in_thinking {
                    self.in_thinking = false;
                    eprintln!("{RESET}");
                }
                eprintln!("{RED}[error]{RESET} {e}");
            }
        }

        false
    }

    fn handle_print_action(&mut self, action: PrintAction) {
        let mut stderr = std::io::stderr();

        match action {
            PrintAction::StreamPath(s) => {
                self.streamed_path.push_str(&s);
            }
            PrintAction::EndPath => {
                eprint!("\r\x1b[2K");
                let label = match self.current_tool_name.as_str() {
                    "read" => "Read",
                    "write" => "Write",
                    "edit" => "Edit",
                    _ => &self.current_tool_name,
                };
                eprint!("  {DIM}\u{25cc} {label} {}{RESET}", self.streamed_path);
                let _ = stderr.flush();
            }
            PrintAction::StartCommand => {
                eprint!("\r\x1b[2K  {DIM}$ {RESET}");
                let _ = stderr.flush();
            }
            PrintAction::StreamCommand(s) => {
                eprint!("{DIM}{s}{RESET}");
                let _ = stderr.flush();
            }
            PrintAction::EndCommand => {
                eprintln!();
            }
            PrintAction::StartContent(path) => {
                eprintln!();
                if let Some(p) = &path {
                    eprintln!("  {DIM}\u{250c}\u{2500} {p}{RESET}");
                } else {
                    eprintln!("  {DIM}\u{250c}\u{2500}{RESET}");
                }
                eprint!("  {DIM}\u{2502}{RESET} ");
            }
            PrintAction::StreamContent(s) => {
                for ch in s.chars() {
                    if ch == '\n' {
                        eprintln!();
                        eprint!("  {DIM}\u{2502}{RESET} ");
                    } else {
                        eprint!("{ch}");
                    }
                }
                let _ = stderr.flush();
            }
            PrintAction::EndContent => {
                eprintln!();
                eprintln!("  {DIM}\u{2514}\u{2500}{RESET}");
            }
        }
    }

    fn print_diff_summary(&self, name: &str, diff: &DiffInfo) {
        let mut lines = vec![format!(
            "  {DIM}{} {}{RESET}",
            cap_tool(name),
            diff.path,
        )];

        if diff.stat.additions > 0 {
            let last = lines.last_mut().unwrap();
            last.push_str(&format!(" {GREEN}+{}{RESET}", diff.stat.additions));
        }
        if diff.stat.deletions > 0 {
            let last = lines.last_mut().unwrap();
            last.push_str(&format!(" {RED}-{}{RESET}", diff.stat.deletions));
        }

        for hunk in &diff.hunks {
            for dl in &hunk.lines {
                match dl.tag {
                    DiffLineTag::Delete => {
                        let content = dl.content.trim_end_matches('\n');
                        lines.push(format!("    {RED}-{content}{RESET}"));
                    }
                    DiffLineTag::Insert => {
                        let content = dl.content.trim_end_matches('\n');
                        lines.push(format!("    {GREEN}+{content}{RESET}"));
                    }
                    DiffLineTag::Equal => {}
                }
            }
        }

        for line in &lines {
            eprintln!("{line}");
        }
    }

    pub fn print_summary(&self) {
        let elapsed = self.start.elapsed();
        let context = self.last_input + self.last_output;
        let p = crate::config::model_pricing(&self.model);
        let cost = self.total_input as f64 * p.input / 1_000_000.0
            + self.total_output as f64 * p.output / 1_000_000.0;
        let rate = if elapsed.as_secs_f64() > 0.0 {
            self.total_output as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };

        eprintln!();
        eprintln!(
            "{DIM}{context} tokens \u{00b7} ${cost:.3} \u{00b7} {} tools \u{00b7} {rate:.0} tok/s \u{00b7} {:.1}s{RESET}",
            self.tool_count,
            elapsed.as_secs_f64(),
        );
    }

    pub fn has_errors(&self) -> bool {
        self.error_count > 0
    }
}

fn cap_tool(name: &str) -> &str {
    match name {
        "read" => "Read",
        "edit" => "Edit",
        "write" => "Write",
        "bash" => "Bash",
        "web_search" => "Search",
        _ => name,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max.saturating_sub(3)])
    } else {
        s.to_string()
    }
}

fn last_paragraph(text: &str) -> &str {
    let trimmed = text.trim_end();
    if let Some(pos) = trimmed.rfind("\n\n") {
        let after = trimmed[pos + 2..].trim_start_matches('\n');
        if after.is_empty() { trimmed } else { after }
    } else {
        trimmed
    }
}
