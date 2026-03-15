use std::path::{Path, PathBuf};
use std::time::Instant;

use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

use crate::tools::DiffInfo;

// tracks an agent file operation (edit or read) for follow mode navigation
#[derive(Debug, Clone)]
pub struct AgentEdit {
    pub path: String,
    pub diff: Option<DiffInfo>,
    // line to jump to when no diff is present (e.g. read tool offset)
    pub line: Option<usize>,
    pub _timestamp: Instant,
}

#[derive(Debug, Clone)]
struct UndoEntry {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

#[derive(Debug, Clone)]
pub struct EditorBuffer {
    pub path: PathBuf,
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub scroll_row: usize,
    pub dirty: bool,
    pub generation: u64,
    // tracks the desired column across vertical movements so the cursor
    // returns to its original column after passing through short lines
    desired_col: Option<usize>,
    undo_stack: Vec<UndoEntry>,
    redo_stack: Vec<UndoEntry>,
}

impl EditorBuffer {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let lines: Vec<String> = if content.is_empty() {
            vec![String::new()]
        } else {
            content.lines().map(|l| l.to_string()).collect()
        };
        Ok(Self {
            path: path.to_path_buf(),
            lines,
            cursor_row: 0,
            cursor_col: 0,
            scroll_row: 0,
            dirty: false,
            generation: 0,
            desired_col: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        })
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        let content = self.lines.join("\n");
        let content = if content.is_empty() {
            String::new()
        } else {
            format!("{}\n", content)
        };
        std::fs::write(&self.path, content)?;
        self.dirty = false;
        Ok(())
    }

    pub fn save_undo(&mut self) {
        self.undo_stack.push(UndoEntry {
            lines: self.lines.clone(),
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
        });
        self.redo_stack.clear();
        self.generation += 1;
        self.desired_col = None;
        if self.undo_stack.len() > 200 {
            self.undo_stack.remove(0);
        }
    }

    pub fn undo(&mut self) {
        if let Some(entry) = self.undo_stack.pop() {
            self.redo_stack.push(UndoEntry {
                lines: self.lines.clone(),
                cursor_row: self.cursor_row,
                cursor_col: self.cursor_col,
            });
            self.lines = entry.lines;
            self.cursor_row = entry.cursor_row;
            self.cursor_col = entry.cursor_col;
            self.dirty = true;
            self.generation += 1;
        }
    }

    pub fn redo(&mut self) {
        if let Some(entry) = self.redo_stack.pop() {
            self.undo_stack.push(UndoEntry {
                lines: self.lines.clone(),
                cursor_row: self.cursor_row,
                cursor_col: self.cursor_col,
            });
            self.lines = entry.lines;
            self.cursor_row = entry.cursor_row;
            self.cursor_col = entry.cursor_col;
            self.dirty = true;
            self.generation += 1;
        }
    }

    fn clamp_cursor(&mut self) {
        if self.cursor_row >= self.lines.len() {
            self.cursor_row = self.lines.len().saturating_sub(1);
        }
        let line_len = self.lines[self.cursor_row].len();
        if self.cursor_col > line_len {
            self.cursor_col = line_len;
        }
    }

    // clamp cursor_col but use desired_col to remember the target column
    fn clamp_cursor_vertical(&mut self) {
        if self.cursor_row >= self.lines.len() {
            self.cursor_row = self.lines.len().saturating_sub(1);
        }
        let target = self.desired_col.unwrap_or(self.cursor_col);
        let line_len = self.lines[self.cursor_row].len();
        self.cursor_col = target.min(line_len);
    }

    // any horizontal movement clears the desired column
    fn clear_desired_col(&mut self) {
        self.desired_col = None;
    }

    pub fn ensure_cursor_visible(&mut self, viewport_height: usize) {
        self.ensure_cursor_visible_wrap(viewport_height, None);
    }

    // accounts for soft-wrapped lines when content_cols is provided
    pub fn ensure_cursor_visible_wrap(&mut self, viewport_height: usize, content_cols: Option<usize>) {
        if self.cursor_row < self.scroll_row {
            self.scroll_row = self.cursor_row;
        }

        // simple check: cursor file line must be within scroll range
        if self.cursor_row >= self.scroll_row + viewport_height {
            self.scroll_row = self.cursor_row - viewport_height + 1;
        }

        // with wrapping, verify the cursor screen row actually fits
        if let Some(cols) = content_cols {
            if cols > 0 {
                loop {
                    let mut screen_rows = 0usize;
                    let mut cursor_end_row = 0usize;
                    for i in self.scroll_row..=self.cursor_row.min(self.lines.len().saturating_sub(1)) {
                        let line_w = unicode_width::UnicodeWidthStr::width(self.lines[i].as_str());
                        let rows = if line_w == 0 { 1 } else { (line_w + cols - 1) / cols };
                        if i == self.cursor_row {
                            cursor_end_row = screen_rows + rows;
                        }
                        screen_rows += rows;
                    }
                    if cursor_end_row <= viewport_height {
                        break;
                    }
                    self.scroll_row += 1;
                    if self.scroll_row > self.cursor_row {
                        break;
                    }
                }
            }
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor_row > 0 {
            if self.desired_col.is_none() {
                self.desired_col = Some(self.cursor_col);
            }
            self.cursor_row -= 1;
            self.clamp_cursor_vertical();
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            if self.desired_col.is_none() {
                self.desired_col = Some(self.cursor_col);
            }
            self.cursor_row += 1;
            self.clamp_cursor_vertical();
        }
    }

    pub fn move_left(&mut self) {
        self.clear_desired_col();
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
        }
    }

    pub fn move_right(&mut self) {
        self.clear_desired_col();
        let line_len = self.lines[self.cursor_row].len();
        if self.cursor_col < line_len {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    pub fn move_home(&mut self) {
        self.clear_desired_col();
        self.cursor_col = 0;
    }

    pub fn move_end(&mut self) {
        self.clear_desired_col();
        self.cursor_col = self.lines[self.cursor_row].len();
    }

    pub fn move_word_left(&mut self) {
        self.clear_desired_col();
        if self.cursor_col == 0 {
            if self.cursor_row > 0 {
                self.cursor_row -= 1;
                self.cursor_col = self.lines[self.cursor_row].len();
            }
            return;
        }
        let line = &self.lines[self.cursor_row];
        let bytes = line.as_bytes();
        let mut pos = self.cursor_col.min(bytes.len());
        while pos > 0 && bytes[pos - 1].is_ascii_whitespace() {
            pos -= 1;
        }
        while pos > 0 && !bytes[pos - 1].is_ascii_whitespace() {
            pos -= 1;
        }
        self.cursor_col = pos;
    }

    pub fn move_word_right(&mut self) {
        self.clear_desired_col();
        let line = &self.lines[self.cursor_row];
        let len = line.len();
        if self.cursor_col >= len {
            if self.cursor_row + 1 < self.lines.len() {
                self.cursor_row += 1;
                self.cursor_col = 0;
            }
            return;
        }
        let bytes = line.as_bytes();
        let mut pos = self.cursor_col.min(len);
        while pos < len && !bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        while pos < len && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        self.cursor_col = pos;
    }

    pub fn page_up(&mut self, viewport_height: usize) {
        if self.desired_col.is_none() {
            self.desired_col = Some(self.cursor_col);
        }
        self.cursor_row = self.cursor_row.saturating_sub(viewport_height);
        self.clamp_cursor_vertical();
    }

    pub fn page_down(&mut self, viewport_height: usize) {
        if self.desired_col.is_none() {
            self.desired_col = Some(self.cursor_col);
        }
        self.cursor_row =
            (self.cursor_row + viewport_height).min(self.lines.len().saturating_sub(1));
        self.clamp_cursor_vertical();
    }

    pub fn goto_line(&mut self, line: usize) {
        self.cursor_row = line.min(self.lines.len().saturating_sub(1));
        self.cursor_col = 0;
    }

    pub fn insert_char(&mut self, c: char) {
        self.save_undo();
        let line = &mut self.lines[self.cursor_row];
        if self.cursor_col >= line.len() {
            line.push(c);
        } else {
            line.insert(self.cursor_col, c);
        }
        self.cursor_col += c.len_utf8();
        self.dirty = true;
    }

    pub fn insert_newline(&mut self) {
        self.save_undo();
        let line = self.lines[self.cursor_row].clone();
        let (before, after) = line.split_at(self.cursor_col.min(line.len()));
        self.lines[self.cursor_row] = before.to_string();
        self.lines.insert(self.cursor_row + 1, after.to_string());
        self.cursor_row += 1;
        self.cursor_col = 0;
        self.dirty = true;
    }

    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.save_undo();
            let line = &mut self.lines[self.cursor_row];
            let remove_pos = self.cursor_col - 1;
            if remove_pos < line.len() {
                line.remove(remove_pos);
            }
            self.cursor_col -= 1;
            self.dirty = true;
        } else if self.cursor_row > 0 {
            self.save_undo();
            let current_line = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
            self.lines[self.cursor_row].push_str(&current_line);
            self.dirty = true;
        }
    }

    pub fn delete(&mut self) {
        let line_len = self.lines[self.cursor_row].len();
        if self.cursor_col < line_len {
            self.save_undo();
            self.lines[self.cursor_row].remove(self.cursor_col);
            self.dirty = true;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.save_undo();
            let next_line = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next_line);
            self.dirty = true;
        }
    }

    pub fn delete_line(&mut self) {
        self.save_undo();
        if self.lines.len() > 1 {
            self.lines.remove(self.cursor_row);
            self.clamp_cursor();
        } else {
            self.lines[0].clear();
            self.cursor_col = 0;
        }
        self.dirty = true;
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn relative_path(&self, cwd: &str) -> String {
        match self.path.strip_prefix(cwd) {
            Ok(rel) => rel.to_string_lossy().to_string(),
            Err(_) => self.path.to_string_lossy().to_string(),
        }
    }
}

// syntax highlighting via syntect with aggressive caching.
// caches parse state checkpoints every CHECKPOINT_INTERVAL lines so scrolling
// only re-parses from the nearest checkpoint rather than from line 0.
// also caches the final rendered output to avoid re-highlighting when nothing changed.

const CHECKPOINT_INTERVAL: usize = 100;

type HighlightedLine = Vec<(syntect::highlighting::Style, String)>;

pub struct Highlighter {
    pub syntax_set: SyntaxSet,
    pub theme: Theme,
    // cached parse state checkpoints: (line_index, parse_state, scope_stack)
    cache_path: Option<PathBuf>,
    cache_generation: u64,
    checkpoints: Vec<(usize, ParseState, ScopeStack)>,
    // cached render output
    render_cache: Option<RenderCache>,
}

struct RenderCache {
    generation: u64,
    start: usize,
    count: usize,
    lines: Vec<HighlightedLine>,
}

impl Highlighter {
    pub fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        let theme = theme_set
            .themes
            .get("base16-eighties.dark")
            .cloned()
            .or_else(|| theme_set.themes.values().next().cloned())
            .unwrap_or_default();
        Self {
            syntax_set,
            theme,
            cache_path: None,
            cache_generation: 0,
            checkpoints: Vec::new(),
            render_cache: None,
        }
    }

    pub fn invalidate(&mut self) {
        self.checkpoints.clear();
        self.render_cache = None;
        self.cache_path = None;
        self.cache_generation = 0;
    }

    pub fn highlight_lines(
        &mut self,
        path: &Path,
        lines: &[String],
        generation: u64,
        start: usize,
        count: usize,
    ) -> Vec<HighlightedLine> {
        // check render cache first
        if let Some(ref rc) = self.render_cache {
            if rc.generation == generation && rc.start == start && rc.count == count {
                return rc.lines.clone();
            }
        }

        // invalidate checkpoints if file changed
        let path_changed = self.cache_path.as_deref() != Some(path);
        if path_changed || self.cache_generation != generation {
            self.checkpoints.clear();
            self.cache_path = Some(path.to_path_buf());
            self.cache_generation = generation;
        }

        let syntax = self
            .syntax_set
            .find_syntax_for_file(path)
            .ok()
            .flatten()
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        // find the best checkpoint at or before `start`
        let (resume_line, mut parse_state, mut scope_stack) = self
            .checkpoints
            .iter()
            .filter(|(line, _, _)| *line <= start)
            .max_by_key(|(line, _, _)| *line)
            .map(|(line, ps, ss)| (*line, ps.clone(), ss.clone()))
            .unwrap_or_else(|| (0, ParseState::new(syntax), ScopeStack::new()));

        let end = (start + count).min(lines.len());
        let mut result = Vec::with_capacity(count);

        // parse from the checkpoint to the end of the viewport, saving new checkpoints
        for i in resume_line..end {
            let line_with_nl = format!("{}\n", lines[i]);
            let ops = parse_state
                .parse_line(&line_with_nl, &self.syntax_set)
                .unwrap_or_default();

            // save checkpoint at interval boundaries
            if i > 0 && i % CHECKPOINT_INTERVAL == 0 {
                let already_cached = self.checkpoints.iter().any(|(l, _, _)| *l == i);
                if !already_cached {
                    self.checkpoints.push((i, parse_state.clone(), scope_stack.clone()));
                }
            }

            if i >= start {
                let highlighter = syntect::highlighting::Highlighter::new(&self.theme);
                let mut hl_state = syntect::highlighting::HighlightState::new(&highlighter, scope_stack.clone());
                let ranges = syntect::highlighting::RangedHighlightIterator::new(&mut hl_state, &ops, &line_with_nl, &highlighter);

                let spans: HighlightedLine = ranges
                    .map(|(style, text, _range)| (style, text.trim_end_matches('\n').to_string()))
                    .filter(|(_, text)| !text.is_empty())
                    .collect();

                result.push(if spans.is_empty() {
                    vec![(syntect::highlighting::Style::default(), String::new())]
                } else {
                    spans
                });
            }

            // apply scope changes
            for (_, op) in &ops {
                scope_stack.apply(op).ok();
            }
        }

        // save render cache
        self.render_cache = Some(RenderCache {
            generation,
            start,
            count,
            lines: result.clone(),
        });

        result
    }
}

#[derive(Debug, Clone)]
pub enum SearchMode {
    Files,
    Text,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub path: String,
    pub line: Option<usize>,
    pub content: Option<String>,
}

#[derive(Debug)]
pub struct SearchState {
    pub mode: SearchMode,
    pub query: String,
    pub cursor: usize,
    pub results: Vec<SearchResult>,
    pub selected: usize,
}

impl SearchState {
    pub fn new(mode: SearchMode) -> Self {
        Self {
            mode,
            query: String::new(),
            cursor: 0,
            results: Vec::new(),
            selected: 0,
        }
    }

    pub fn insert_char(&mut self, c: char) {
        self.query.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.query.remove(self.cursor);
        }
    }

    pub fn select_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn select_down(&mut self) {
        if self.selected + 1 < self.results.len() {
            self.selected += 1;
        }
    }
}

// collect files recursively, skipping common non-source directories
pub fn collect_files(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_files_recursive(root, root, &mut files);
    files.sort();
    files
}

fn collect_files_recursive(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with('.')
            || name_str == "target"
            || name_str == "node_modules"
            || name_str == "__pycache__"
            || name_str == "dist"
            || name_str == "build"
        {
            continue;
        }

        if path.is_dir() {
            collect_files_recursive(root, &path, out);
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().to_string());
            }
        }
    }
}

// fuzzy substring match: all query chars must appear in order.
// returns a score (higher is better) or None for no match.
pub fn fuzzy_match(query: &str, target: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let query_lower = query.to_lowercase();
    let target_lower = target.to_lowercase();

    let mut score: i32 = 0;
    let mut qi = 0;
    let query_chars: Vec<char> = query_lower.chars().collect();
    let mut last_match: Option<usize> = None;

    for (ti, tc) in target_lower.chars().enumerate() {
        if qi < query_chars.len() && tc == query_chars[qi] {
            if let Some(prev) = last_match {
                if ti == prev + 1 {
                    score += 3;
                }
            }
            if ti == 0
                || target
                    .as_bytes()
                    .get(ti - 1)
                    .map_or(false, |&b| b == b'/' || b == b'_' || b == b'.')
            {
                score += 5;
            }
            score += 1;
            last_match = Some(ti);
            qi += 1;
        }
    }

    if qi == query_chars.len() {
        score -= (target.len() as i32) / 4;
        Some(score)
    } else {
        None
    }
}

// search for text across files (returns up to max_results matches)
pub fn search_text(root: &Path, query: &str, max_results: usize) -> Vec<SearchResult> {
    if query.is_empty() {
        return Vec::new();
    }

    let files = collect_files(root);
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();

    for file_path in &files {
        let full_path = root.join(file_path);
        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for (line_num, line) in content.lines().enumerate() {
            if line.to_lowercase().contains(&query_lower) {
                results.push(SearchResult {
                    path: file_path.clone(),
                    line: Some(line_num),
                    content: Some(line.trim().to_string()),
                });
                if results.len() >= max_results {
                    return results;
                }
            }
        }
    }

    results
}
