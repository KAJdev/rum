use crate::editor::{EditorBuffer, SearchMode, SearchState};
use crate::tui::{App, LspNotify, ViewMode};
use crate::util::sanitize_text;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthStr;

// slash command definitions for tab-completion

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

// paste placeholder helpers - private use area \u{E000}..\u{E00F}

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

// input text manipulation methods on App

impl App {
    // cursor_pos is a char-count offset. convert to byte index for
    // String insert/remove operations.
    pub(crate) fn cursor_byte_pos(&self) -> usize {
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
        let text = sanitize_text(&text);
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

    pub(crate) fn char_count(&self) -> usize {
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

    pub(crate) fn input_line_count(&self) -> usize {
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

// key event handler results

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

// main key event dispatcher for chat mode

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

// editor mode key handler

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

// search overlay key handler (file search or text search in editor)

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
