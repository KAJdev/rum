// streaming ansi markdown renderer.
// accumulates text token-by-token and emits colored lines.

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const STRIKETHROUGH: &str = "\x1b[9m";

// palette
const HEADING: &str = "\x1b[1;33m";       // bold yellow
const HEADING2: &str = "\x1b[1;36m";      // bold cyan
const HEADING3: &str = "\x1b[1;35m";      // bold magenta
const INLINE_CODE: &str = "\x1b[38;5;223m\x1b[48;5;236m"; // warm on dark bg
const CODE_BORDER: &str = "\x1b[38;5;242m";
const CODE_BG: &str = "\x1b[48;5;235m";
const LINK_TEXT: &str = "\x1b[36m";        // cyan
const LINK_URL: &str = "\x1b[38;5;242m";  // dim
const LIST_BULLET: &str = "\x1b[33m";     // yellow
const BLOCKQUOTE: &str = "\x1b[38;5;109m"; // muted blue
const HR: &str = "\x1b[38;5;242m";
const BOLD_STYLE: &str = "\x1b[1m";
const ITALIC_STYLE: &str = "\x1b[3m";
const BOLD_ITALIC: &str = "\x1b[1;3m";

// syntax highlight colors
const SYN_KEYWORD: &str = "\x1b[38;5;176m";  // purple/pink
const SYN_STRING: &str = "\x1b[38;5;150m";   // green
const SYN_COMMENT: &str = "\x1b[38;5;242m";  // dim gray
const SYN_NUMBER: &str = "\x1b[38;5;215m";   // orange
const SYN_TYPE: &str = "\x1b[38;5;117m";     // light blue
const SYN_FUNC: &str = "\x1b[38;5;222m";     // yellow
const SYN_PUNCT: &str = "\x1b[38;5;248m";    // light gray
const SYN_NORMAL: &str = "\x1b[38;5;252m";   // white-ish

pub struct MarkdownRenderer {
    line_buf: String,
    in_code_block: bool,
    code_lang: String,
    // pending output lines ready to be flushed
    output: Vec<String>,
}

impl MarkdownRenderer {
    pub fn new() -> Self {
        Self {
            line_buf: String::new(),
            in_code_block: false,
            code_lang: String::new(),
            output: Vec::new(),
        }
    }

    // feed a chunk of streaming text. returns completed rendered lines.
    pub fn push(&mut self, text: &str) -> Vec<String> {
        self.output.clear();

        for ch in text.chars() {
            if ch == '\n' {
                self.flush_line();
            } else {
                self.line_buf.push(ch);
            }
        }

        std::mem::take(&mut self.output)
    }

    // render whatever is left in the buffer (for partial lines)
    #[allow(dead_code)]
    pub fn peek_partial(&self) -> Option<String> {
        if self.line_buf.is_empty() {
            return None;
        }
        if self.in_code_block {
            Some(format!(
                "{}  {}{}{}",
                CODE_BORDER,
                CODE_BG,
                highlight_line(&self.line_buf, &self.code_lang),
                RESET
            ))
        } else {
            Some(render_inline(&self.line_buf))
        }
    }

    // return the raw partial line without any formatting.
    // used for streaming output where the partial will be cleared
    // and the completed line re-rendered through the full pipeline.
    pub fn peek_raw(&self) -> Option<&str> {
        if self.line_buf.is_empty() {
            None
        } else {
            Some(&self.line_buf)
        }
    }

    // flush remaining content
    pub fn finish(&mut self) -> Vec<String> {
        self.output.clear();
        if !self.line_buf.is_empty() {
            self.flush_line();
        }
        if self.in_code_block {
            self.output.push(format!(
                "{}\u{2514}\u{2500}\u{2500}\u{2500}{}",
                CODE_BORDER, RESET
            ));
            self.in_code_block = false;
        }
        std::mem::take(&mut self.output)
    }

    fn flush_line(&mut self) {
        let line = std::mem::take(&mut self.line_buf);

        // check for code fence
        if line.starts_with("```") {
            if self.in_code_block {
                // closing fence
                self.output.push(format!(
                    "{}\u{2514}\u{2500}\u{2500}\u{2500}{}",
                    CODE_BORDER, RESET
                ));
                self.in_code_block = false;
                self.code_lang.clear();
            } else {
                // opening fence
                self.code_lang = line[3..].trim().to_string();
                let lang_label = if self.code_lang.is_empty() {
                    String::new()
                } else {
                    format!(" {}", self.code_lang)
                };
                self.output.push(format!(
                    "{}\u{250c}\u{2500}\u{2500}\u{2500}{}{}",
                    CODE_BORDER, lang_label, RESET
                ));
                self.in_code_block = true;
            }
            return;
        }

        if self.in_code_block {
            let highlighted = highlight_line(&line, &self.code_lang);
            self.output.push(format!(
                "{}\u{2502}{} {}{}",
                CODE_BORDER, CODE_BG, highlighted, RESET
            ));
            return;
        }

        // markdown block-level elements

        // horizontal rule
        if (line.starts_with("---") || line.starts_with("***") || line.starts_with("___"))
            && line.chars().all(|c| c == '-' || c == '*' || c == '_' || c == ' ')
            && line.len() >= 3
        {
            self.output.push(format!(
                "{}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}{}",
                HR, RESET
            ));
            return;
        }

        // headings
        if line.starts_with("# ") {
            self.output.push(format!(
                "{}{}{}", HEADING, &line[2..], RESET
            ));
            return;
        }
        if line.starts_with("## ") {
            self.output.push(format!(
                "{}{}{}", HEADING2, &line[3..], RESET
            ));
            return;
        }
        if line.starts_with("### ") {
            self.output.push(format!(
                "{}{}{}", HEADING3, &line[4..], RESET
            ));
            return;
        }
        if line.starts_with("#### ") {
            self.output.push(format!(
                "{}{}{}", HEADING3, &line[5..], RESET
            ));
            return;
        }

        // blockquote
        if line.starts_with("> ") {
            self.output.push(format!(
                "{}\u{2502} {}{}",
                BLOCKQUOTE,
                render_inline(&line[2..]),
                RESET
            ));
            return;
        }

        // unordered list
        if line.starts_with("- ")
            || line.starts_with("* ")
            || line.starts_with("+ ")
        {
            self.output.push(format!(
                "{}  \u{2022}{} {}",
                LIST_BULLET,
                RESET,
                render_inline(&line[2..])
            ));
            return;
        }

        // indented list items
        if let Some(rest) = strip_list_indent(&line) {
            let indent_len = line.len() - rest.len();
            let indent: String = " ".repeat(indent_len);
            self.output.push(format!(
                "{}{}  \u{2022}{} {}",
                indent,
                LIST_BULLET,
                RESET,
                render_inline(rest)
            ));
            return;
        }

        // ordered list: "1. ", "2. ", etc.
        if let Some((num, rest)) = parse_ordered_list(&line) {
            self.output.push(format!(
                "{}  {}.{} {}",
                LIST_BULLET,
                num,
                RESET,
                render_inline(rest)
            ));
            return;
        }

        // table rows (simple: just render with dim pipes)
        if line.starts_with('|') && line.ends_with('|') {
            // separator row
            if line.contains("---") {
                self.output.push(format!(
                    "{}{}{}", DIM, line, RESET
                ));
                return;
            }
            self.output.push(render_table_row(&line));
            return;
        }

        // empty line
        if line.is_empty() {
            self.output.push(String::new());
            return;
        }

        // regular paragraph line
        self.output.push(render_inline(&line));
    }
}

fn strip_list_indent(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if line.len() == trimmed.len() {
        return None;
    }
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        Some(&trimmed[2..])
    } else {
        None
    }
}

fn parse_ordered_list(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    let dot_pos = trimmed.find(". ")?;
    if dot_pos > 4 {
        return None;
    }
    let num = &trimmed[..dot_pos];
    if num.chars().all(|c| c.is_ascii_digit()) {
        Some((num, &trimmed[dot_pos + 2..]))
    } else {
        None
    }
}

fn render_table_row(line: &str) -> String {
    let mut out = String::new();
    let cells: Vec<&str> = line.split('|').collect();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push_str(&format!("{}\u{2502}{}", DIM, RESET));
        }
        let trimmed = cell.trim();
        if !trimmed.is_empty() {
            out.push(' ');
            out.push_str(&render_inline(trimmed));
            out.push(' ');
        }
    }
    out
}

// render inline markdown: bold, italic, code, links, strikethrough
fn render_inline(text: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // bold italic ***text***
        if i + 2 < len && chars[i] == '*' && chars[i + 1] == '*' && chars[i + 2] == '*' {
            if let Some(end) = find_closing(&chars, i + 3, &['*', '*', '*']) {
                out.push_str(BOLD_ITALIC);
                let inner: String = chars[i + 3..end].iter().collect();
                out.push_str(&inner);
                out.push_str(RESET);
                i = end + 3;
                continue;
            }
        }

        // bold **text**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_closing(&chars, i + 2, &['*', '*']) {
                out.push_str(BOLD_STYLE);
                let inner: String = chars[i + 2..end].iter().collect();
                out.push_str(&inner);
                out.push_str(RESET);
                i = end + 2;
                continue;
            }
        }

        // italic *text*
        if chars[i] == '*' && (i + 1 < len && chars[i + 1] != ' ') {
            if let Some(end) = find_closing_single(&chars, i + 1, '*') {
                out.push_str(ITALIC_STYLE);
                let inner: String = chars[i + 1..end].iter().collect();
                out.push_str(&inner);
                out.push_str(RESET);
                i = end + 1;
                continue;
            }
        }

        // strikethrough ~~text~~
        if i + 1 < len && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some(end) = find_closing(&chars, i + 2, &['~', '~']) {
                out.push_str(STRIKETHROUGH);
                let inner: String = chars[i + 2..end].iter().collect();
                out.push_str(&inner);
                out.push_str(RESET);
                i = end + 2;
                continue;
            }
        }

        // inline code `text`
        if chars[i] == '`' {
            if let Some(end) = find_closing_single(&chars, i + 1, '`') {
                out.push_str(INLINE_CODE);
                let inner: String = chars[i + 1..end].iter().collect();
                out.push_str(&inner);
                out.push_str(RESET);
                i = end + 1;
                continue;
            }
        }

        // links [text](url)
        if chars[i] == '[' {
            if let Some((link_text, url, end_pos)) = parse_link(&chars, i) {
                out.push_str(LINK_TEXT);
                out.push_str(&link_text);
                out.push_str(RESET);
                out.push_str(&format!("{} ({}){}", LINK_URL, url, RESET));
                i = end_pos;
                continue;
            }
        }

        out.push(chars[i]);
        i += 1;
    }

    out
}

fn find_closing(chars: &[char], from: usize, pattern: &[char]) -> Option<usize> {
    let plen = pattern.len();
    if chars.len() < plen {
        return None;
    }
    for i in from..=(chars.len() - plen) {
        if &chars[i..i + plen] == pattern {
            return Some(i);
        }
    }
    None
}

fn find_closing_single(chars: &[char], from: usize, ch: char) -> Option<usize> {
    for i in from..chars.len() {
        if chars[i] == ch {
            return Some(i);
        }
    }
    None
}

fn parse_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    // [text](url)
    let close_bracket = find_closing_single(chars, start + 1, ']')?;
    if close_bracket + 1 >= chars.len() || chars[close_bracket + 1] != '(' {
        return None;
    }
    let close_paren = find_closing_single(chars, close_bracket + 2, ')')?;
    let text: String = chars[start + 1..close_bracket].iter().collect();
    let url: String = chars[close_bracket + 2..close_paren].iter().collect();
    Some((text, url, close_paren + 1))
}

// syntax highlighting for code blocks

fn highlight_line(line: &str, lang: &str) -> String {
    match lang {
        "rust" | "rs" => highlight_rust(line),
        "python" | "py" => highlight_python(line),
        "javascript" | "js" | "typescript" | "ts" | "jsx" | "tsx" => highlight_js(line),
        "bash" | "sh" | "zsh" | "shell" => highlight_bash(line),
        "go" => highlight_go(line),
        "c" | "cpp" | "cc" | "cxx" | "h" | "hpp" => highlight_c(line),
        "json" => highlight_json(line),
        "toml" => highlight_toml(line),
        "yaml" | "yml" => highlight_yaml(line),
        "sql" => highlight_sql(line),
        _ => highlight_generic(line),
    }
}

// generic tokenizer that handles strings, comments, and numbers
struct Tokenizer<'a> {
    chars: Vec<char>,
    pos: usize,
    line_comment: &'a str,
    keywords: &'a [&'a str],
    types: &'a [&'a str],
    builtins: &'a [&'a str],
}

impl<'a> Tokenizer<'a> {
    fn new(
        line: &str,
        line_comment: &'a str,
        keywords: &'a [&'a str],
        types: &'a [&'a str],
        builtins: &'a [&'a str],
    ) -> Self {
        Self {
            chars: line.chars().collect(),
            pos: 0,
            line_comment,
            keywords,
            types,
            builtins,
        }
    }

    fn highlight(&mut self) -> String {
        let mut out = String::new();
        let len = self.chars.len();

        while self.pos < len {
            // line comment
            if !self.line_comment.is_empty() && self.remaining().starts_with(self.line_comment) {
                let rest: String = self.chars[self.pos..].iter().collect();
                out.push_str(SYN_COMMENT);
                out.push_str(&rest);
                out.push_str(RESET);
                out.push_str(CODE_BG);
                self.pos = len;
                continue;
            }

            // strings
            if self.chars[self.pos] == '"' || self.chars[self.pos] == '\'' {
                let quote = self.chars[self.pos];
                let start = self.pos;
                self.pos += 1;
                while self.pos < len {
                    if self.chars[self.pos] == '\\' {
                        self.pos += 2;
                        continue;
                    }
                    if self.chars[self.pos] == quote {
                        self.pos += 1;
                        break;
                    }
                    self.pos += 1;
                }
                let s: String = self.chars[start..self.pos].iter().collect();
                out.push_str(SYN_STRING);
                out.push_str(&s);
                out.push_str(RESET);
                out.push_str(CODE_BG);
                continue;
            }

            // backtick strings (js template literals)
            if self.chars[self.pos] == '`' {
                let start = self.pos;
                self.pos += 1;
                while self.pos < len {
                    if self.chars[self.pos] == '\\' {
                        self.pos += 2;
                        continue;
                    }
                    if self.chars[self.pos] == '`' {
                        self.pos += 1;
                        break;
                    }
                    self.pos += 1;
                }
                let s: String = self.chars[start..self.pos].iter().collect();
                out.push_str(SYN_STRING);
                out.push_str(&s);
                out.push_str(RESET);
                out.push_str(CODE_BG);
                continue;
            }

            // numbers
            if self.chars[self.pos].is_ascii_digit()
                && (self.pos == 0 || !self.chars[self.pos - 1].is_alphanumeric())
            {
                let start = self.pos;
                while self.pos < len
                    && (self.chars[self.pos].is_ascii_digit()
                        || self.chars[self.pos] == '.'
                        || self.chars[self.pos] == 'x'
                        || self.chars[self.pos] == 'o'
                        || self.chars[self.pos] == 'b'
                        || self.chars[self.pos] == '_'
                        || (self.chars[self.pos].is_ascii_hexdigit()
                            && self.pos > start
                            && start + 1 < len))
                {
                    self.pos += 1;
                }
                let s: String = self.chars[start..self.pos].iter().collect();
                out.push_str(SYN_NUMBER);
                out.push_str(&s);
                out.push_str(RESET);
                out.push_str(CODE_BG);
                continue;
            }

            // identifiers and keywords
            if self.chars[self.pos].is_alphanumeric() || self.chars[self.pos] == '_' {
                let start = self.pos;
                while self.pos < len
                    && (self.chars[self.pos].is_alphanumeric() || self.chars[self.pos] == '_')
                {
                    self.pos += 1;
                }
                let word: String = self.chars[start..self.pos].iter().collect();

                if self.keywords.contains(&word.as_str()) {
                    out.push_str(SYN_KEYWORD);
                    out.push_str(&word);
                    out.push_str(RESET);
                    out.push_str(CODE_BG);
                } else if self.types.contains(&word.as_str()) {
                    out.push_str(SYN_TYPE);
                    out.push_str(&word);
                    out.push_str(RESET);
                    out.push_str(CODE_BG);
                } else if self.builtins.contains(&word.as_str()) {
                    out.push_str(SYN_FUNC);
                    out.push_str(&word);
                    out.push_str(RESET);
                    out.push_str(CODE_BG);
                } else if self.pos < len && self.chars[self.pos] == '(' {
                    // function call
                    out.push_str(SYN_FUNC);
                    out.push_str(&word);
                    out.push_str(RESET);
                    out.push_str(CODE_BG);
                } else if word.chars().next().map_or(false, |c| c.is_uppercase()) {
                    // likely a type
                    out.push_str(SYN_TYPE);
                    out.push_str(&word);
                    out.push_str(RESET);
                    out.push_str(CODE_BG);
                } else {
                    out.push_str(SYN_NORMAL);
                    out.push_str(&word);
                    out.push_str(RESET);
                    out.push_str(CODE_BG);
                }
                continue;
            }

            // punctuation
            let ch = self.chars[self.pos];
            if "{}()[]<>;:,.+-=!&|^%/?@#~".contains(ch) {
                out.push_str(SYN_PUNCT);
                out.push(ch);
                out.push_str(RESET);
                out.push_str(CODE_BG);
            } else {
                out.push(ch);
            }
            self.pos += 1;
        }

        out
    }

    fn remaining(&self) -> String {
        self.chars[self.pos..].iter().collect()
    }
}

fn highlight_rust(line: &str) -> String {
    static KEYWORDS: &[&str] = &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
        "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
        "trait", "true", "type", "unsafe", "use", "where", "while", "yield",
    ];
    static TYPES: &[&str] = &[
        "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str",
        "u8", "u16", "u32", "u64", "u128", "usize", "String", "Vec", "Box", "Rc", "Arc",
        "Option", "Result", "Ok", "Err", "Some", "None", "HashMap", "HashSet", "Path",
        "PathBuf",
    ];
    static BUILTINS: &[&str] = &[
        "println", "eprintln", "format", "write", "writeln", "todo", "unimplemented",
        "unreachable", "assert", "assert_eq", "assert_ne", "dbg", "vec", "panic",
    ];
    let mut tok = Tokenizer::new(line, "//", KEYWORDS, TYPES, BUILTINS);
    tok.highlight()
}

fn highlight_python(line: &str) -> String {
    static KEYWORDS: &[&str] = &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import",
        "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return",
        "True", "try", "while", "with", "yield",
    ];
    static TYPES: &[&str] = &[
        "int", "float", "str", "bool", "list", "dict", "tuple", "set", "bytes", "type",
        "object", "Exception",
    ];
    static BUILTINS: &[&str] = &[
        "print", "len", "range", "enumerate", "zip", "map", "filter", "sorted", "reversed",
        "isinstance", "issubclass", "hasattr", "getattr", "setattr", "open", "input", "super",
        "property", "staticmethod", "classmethod",
    ];
    let mut tok = Tokenizer::new(line, "#", KEYWORDS, TYPES, BUILTINS);
    tok.highlight()
}

fn highlight_js(line: &str) -> String {
    static KEYWORDS: &[&str] = &[
        "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger",
        "default", "delete", "do", "else", "export", "extends", "false", "finally", "for",
        "from", "function", "if", "import", "in", "instanceof", "let", "new", "null", "of",
        "return", "static", "super", "switch", "this", "throw", "true", "try", "typeof",
        "undefined", "var", "void", "while", "with", "yield",
        "interface", "type", "enum", "implements", "namespace", "declare", "abstract",
        "readonly", "as", "keyof", "satisfies",
    ];
    static TYPES: &[&str] = &[
        "string", "number", "boolean", "any", "void", "never", "unknown", "object",
        "Array", "Map", "Set", "Promise", "Record", "Partial", "Required", "Readonly",
    ];
    static BUILTINS: &[&str] = &[
        "console", "Math", "JSON", "parseInt", "parseFloat", "setTimeout", "setInterval",
        "fetch", "require",
    ];
    let mut tok = Tokenizer::new(line, "//", KEYWORDS, TYPES, BUILTINS);
    tok.highlight()
}

fn highlight_bash(line: &str) -> String {
    static KEYWORDS: &[&str] = &[
        "if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac",
        "in", "function", "return", "exit", "local", "export", "readonly", "declare",
        "unset", "shift", "break", "continue", "source", "true", "false",
    ];
    static BUILTINS: &[&str] = &[
        "echo", "printf", "cd", "pwd", "ls", "cat", "grep", "sed", "awk", "find", "xargs",
        "sort", "uniq", "wc", "head", "tail", "cut", "tr", "tee", "mkdir", "rm", "cp", "mv",
        "chmod", "chown", "curl", "wget", "git", "docker", "cargo", "npm", "pip",
    ];
    let mut tok = Tokenizer::new(line, "#", KEYWORDS, &[], BUILTINS);
    tok.highlight()
}

fn highlight_go(line: &str) -> String {
    static KEYWORDS: &[&str] = &[
        "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough",
        "for", "func", "go", "goto", "if", "import", "interface", "map", "package", "range",
        "return", "select", "struct", "switch", "type", "var", "true", "false", "nil",
    ];
    static TYPES: &[&str] = &[
        "bool", "byte", "complex64", "complex128", "error", "float32", "float64",
        "int", "int8", "int16", "int32", "int64", "rune", "string",
        "uint", "uint8", "uint16", "uint32", "uint64", "uintptr",
    ];
    static BUILTINS: &[&str] = &[
        "append", "cap", "close", "copy", "delete", "len", "make", "new", "panic", "print",
        "println", "recover",
    ];
    let mut tok = Tokenizer::new(line, "//", KEYWORDS, TYPES, BUILTINS);
    tok.highlight()
}

fn highlight_c(line: &str) -> String {
    static KEYWORDS: &[&str] = &[
        "auto", "break", "case", "char", "const", "continue", "default", "do", "double",
        "else", "enum", "extern", "float", "for", "goto", "if", "inline", "int", "long",
        "register", "return", "short", "signed", "sizeof", "static", "struct", "switch",
        "typedef", "union", "unsigned", "void", "volatile", "while",
        "class", "namespace", "template", "typename", "public", "private", "protected",
        "virtual", "override", "new", "delete", "try", "catch", "throw", "nullptr",
        "true", "false", "using", "constexpr", "noexcept", "auto",
        "#include", "#define", "#ifdef", "#ifndef", "#endif", "#pragma",
    ];
    static TYPES: &[&str] = &[
        "size_t", "int8_t", "int16_t", "int32_t", "int64_t",
        "uint8_t", "uint16_t", "uint32_t", "uint64_t",
        "bool", "string", "vector", "map", "set", "unique_ptr", "shared_ptr",
    ];
    let mut tok = Tokenizer::new(line, "//", KEYWORDS, TYPES, &[]);
    tok.highlight()
}

fn highlight_json(line: &str) -> String {
    // json is simple: keys are strings, values can be strings/numbers/booleans
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];

    let mut out = String::from(indent);

    // simple approach: color key-value pairs
    let chars: Vec<char> = trimmed.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '"' {
            let start = i;
            i += 1;
            while i < len {
                if chars[i] == '\\' {
                    i += 2;
                    continue;
                }
                if chars[i] == '"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();

            // check if this is a key (followed by :)
            let mut j = i;
            while j < len && chars[j] == ' ' {
                j += 1;
            }
            if j < len && chars[j] == ':' {
                out.push_str(SYN_TYPE);
            } else {
                out.push_str(SYN_STRING);
            }
            out.push_str(&s);
            out.push_str(RESET);
            out.push_str(CODE_BG);
        } else if chars[i].is_ascii_digit() || chars[i] == '-' {
            let start = i;
            if chars[i] == '-' {
                i += 1;
            }
            while i < len && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == 'e' || chars[i] == 'E') {
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            out.push_str(SYN_NUMBER);
            out.push_str(&s);
            out.push_str(RESET);
            out.push_str(CODE_BG);
        } else if trimmed[i..].starts_with("true") || trimmed[i..].starts_with("false") || trimmed[i..].starts_with("null") {
            let word = if trimmed[i..].starts_with("true") { "true" }
                else if trimmed[i..].starts_with("false") { "false" }
                else { "null" };
            out.push_str(SYN_KEYWORD);
            out.push_str(word);
            out.push_str(RESET);
            out.push_str(CODE_BG);
            i += word.len();
        } else {
            if "{}[]:,".contains(chars[i]) {
                out.push_str(SYN_PUNCT);
            }
            out.push(chars[i]);
            if "{}[]:,".contains(chars[i]) {
                out.push_str(RESET);
                out.push_str(CODE_BG);
            }
            i += 1;
        }
    }

    out
}

fn highlight_toml(line: &str) -> String {
    let trimmed = line.trim();
    // section headers
    if trimmed.starts_with('[') {
        return format!("{}{}{}{}", SYN_TYPE, line, RESET, CODE_BG);
    }
    // comments
    if trimmed.starts_with('#') {
        return format!("{}{}{}{}", SYN_COMMENT, line, RESET, CODE_BG);
    }
    // key = value
    if let Some(eq_pos) = line.find(" = ") {
        let key = &line[..eq_pos];
        let value = &line[eq_pos + 3..];
        return format!(
            "{}{}{}{}= {}",
            SYN_TYPE, key, RESET, CODE_BG,
            highlight_toml_value(value),
        );
    }
    format!("{}{}{}", SYN_NORMAL, line, RESET)
}

fn highlight_toml_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.starts_with('"') || trimmed.starts_with('\'') {
        format!("{}{}{}{}", SYN_STRING, value, RESET, CODE_BG)
    } else if trimmed == "true" || trimmed == "false" {
        format!("{}{}{}{}", SYN_KEYWORD, value, RESET, CODE_BG)
    } else if trimmed.chars().next().map_or(false, |c| c.is_ascii_digit()) {
        format!("{}{}{}{}", SYN_NUMBER, value, RESET, CODE_BG)
    } else {
        format!("{}{}{}", SYN_NORMAL, value, RESET)
    }
}

fn highlight_yaml(line: &str) -> String {
    let trimmed = line.trim();
    // comments
    if trimmed.starts_with('#') {
        return format!("{}{}{}{}", SYN_COMMENT, line, RESET, CODE_BG);
    }
    // key: value
    if let Some(colon_pos) = trimmed.find(": ") {
        let indent = &line[..line.len() - trimmed.len()];
        let key = &trimmed[..colon_pos];
        let value = &trimmed[colon_pos + 2..];

        // handle list prefix
        let (prefix, actual_key) = if key.starts_with("- ") {
            (format!("{}- {}", LIST_BULLET, RESET), &key[2..])
        } else {
            (String::new(), key)
        };

        return format!(
            "{}{}{}{}{}:{} {}",
            indent, prefix,
            SYN_TYPE, actual_key, RESET, CODE_BG,
            highlight_yaml_value(value),
        );
    }
    // list items
    if trimmed.starts_with("- ") {
        let indent = &line[..line.len() - trimmed.len()];
        return format!(
            "{}{}- {}{}{}", indent, LIST_BULLET, RESET, CODE_BG,
            highlight_yaml_value(&trimmed[2..]),
        );
    }
    format!("{}{}{}", SYN_NORMAL, line, RESET)
}

fn highlight_yaml_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.starts_with('"') || trimmed.starts_with('\'') {
        format!("{}{}{}{}", SYN_STRING, value, RESET, CODE_BG)
    } else if trimmed == "true" || trimmed == "false" || trimmed == "null" || trimmed == "~" {
        format!("{}{}{}{}", SYN_KEYWORD, value, RESET, CODE_BG)
    } else if trimmed.chars().next().map_or(false, |c| c.is_ascii_digit()) {
        format!("{}{}{}{}", SYN_NUMBER, value, RESET, CODE_BG)
    } else {
        format!("{}{}{}", SYN_NORMAL, value, RESET)
    }
}

fn highlight_sql(line: &str) -> String {
    static KEYWORDS: &[&str] = &[
        "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "IN", "IS", "NULL", "AS", "ON",
        "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "FULL", "CROSS", "GROUP", "BY", "ORDER",
        "HAVING", "LIMIT", "OFFSET", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE",
        "CREATE", "TABLE", "ALTER", "DROP", "INDEX", "VIEW", "TRIGGER", "FUNCTION",
        "IF", "EXISTS", "THEN", "ELSE", "END", "CASE", "WHEN", "BEGIN", "COMMIT", "ROLLBACK",
        "PRIMARY", "KEY", "FOREIGN", "REFERENCES", "UNIQUE", "CHECK", "DEFAULT", "CONSTRAINT",
        "ASC", "DESC", "DISTINCT", "UNION", "ALL", "ANY", "BETWEEN", "LIKE", "TRUE", "FALSE",
        "select", "from", "where", "and", "or", "not", "in", "is", "null", "as", "on",
        "join", "left", "right", "inner", "outer", "group", "by", "order", "having",
        "limit", "offset", "insert", "into", "values", "update", "set", "delete",
        "create", "table", "alter", "drop", "if", "exists", "primary", "key",
    ];
    static TYPES: &[&str] = &[
        "INT", "INTEGER", "BIGINT", "SMALLINT", "TINYINT", "FLOAT", "DOUBLE", "DECIMAL",
        "NUMERIC", "VARCHAR", "CHAR", "TEXT", "BLOB", "DATE", "DATETIME", "TIMESTAMP",
        "BOOLEAN", "SERIAL", "UUID",
        "int", "integer", "bigint", "varchar", "char", "text", "boolean", "serial",
    ];
    static BUILTINS: &[&str] = &[
        "COUNT", "SUM", "AVG", "MIN", "MAX", "COALESCE", "CAST", "CONCAT", "NOW",
        "count", "sum", "avg", "min", "max", "coalesce", "cast", "concat", "now",
    ];
    let mut tok = Tokenizer::new(line, "--", KEYWORDS, TYPES, BUILTINS);
    tok.highlight()
}

fn highlight_generic(line: &str) -> String {
    // no language specified, just do strings and numbers
    let mut tok = Tokenizer::new(line, "", &[], &[], &[]);
    tok.highlight()
}

// ratatui integration: convert markdown text into styled Line objects for the TUI
use ratatui::style::{Color as RColor, Modifier as RModifier, Style as RStyle};
use ratatui::text::{Line as RLine, Span as RSpan};

const TUI_FG: RColor = RColor::Rgb(191, 189, 182);
const TUI_MUTED: RColor = RColor::Rgb(108, 115, 128);
const TUI_ACCENT: RColor = RColor::Rgb(230, 180, 80);
#[allow(dead_code)]
const TUI_GREEN: RColor = RColor::Rgb(170, 217, 76);
#[allow(dead_code)]
const TUI_RED: RColor = RColor::Rgb(240, 113, 120);
const TUI_YELLOW: RColor = RColor::Rgb(255, 180, 84);
const TUI_CYAN: RColor = RColor::Rgb(149, 230, 203);
const TUI_PURPLE: RColor = RColor::Rgb(199, 146, 234);
const TUI_CODE_BG: RColor = RColor::Rgb(30, 35, 43);
const TUI_DIM: RColor = RColor::Rgb(86, 91, 102);

pub struct TuiMarkdownRenderer {
    in_code_block: bool,
    code_lang: String,
}

impl TuiMarkdownRenderer {
    pub fn new() -> Self {
        Self {
            in_code_block: false,
            code_lang: String::new(),
        }
    }

    pub fn render_lines(&mut self, text: &str) -> Vec<RLine<'static>> {
        let mut output = Vec::new();
        let all_lines: Vec<&str> = text.lines().collect();
        let mut idx = 0;

        while idx < all_lines.len() {
            let line = all_lines[idx];

            // code fence
            if line.starts_with("```") {
                if self.in_code_block {
                    output.push(RLine::from(RSpan::styled(
                        "\u{2514}\u{2500}\u{2500}\u{2500}",
                        RStyle::default().fg(TUI_DIM),
                    )));
                    self.in_code_block = false;
                    self.code_lang.clear();
                } else {
                    self.code_lang = line[3..].trim().to_string();
                    let label = if self.code_lang.is_empty() {
                        "\u{250c}\u{2500}\u{2500}\u{2500}".to_string()
                    } else {
                        format!("\u{250c}\u{2500}\u{2500}\u{2500} {}", self.code_lang)
                    };
                    output.push(RLine::from(RSpan::styled(
                        label,
                        RStyle::default().fg(TUI_DIM),
                    )));
                    self.in_code_block = true;
                }
                idx += 1;
                continue;
            }

            if self.in_code_block {
                output.push(RLine::from(vec![
                    RSpan::styled("\u{2502} ", RStyle::default().fg(TUI_DIM)),
                    RSpan::styled(
                        line.to_string(),
                        RStyle::default().fg(TUI_FG).bg(TUI_CODE_BG),
                    ),
                ]));
                idx += 1;
                continue;
            }

            // table block: collect consecutive table rows and render together
            if is_table_row(line) {
                let start = idx;
                while idx < all_lines.len() && is_table_row(all_lines[idx]) {
                    idx += 1;
                }
                output.extend(render_tui_table(&all_lines[start..idx]));
                continue;
            }

            // hr
            if (line.starts_with("---") || line.starts_with("***"))
                && line.len() >= 3
                && line.chars().all(|c| c == '-' || c == '*' || c == ' ')
            {
                output.push(RLine::from(RSpan::styled(
                    "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                    RStyle::default().fg(TUI_DIM),
                )));
                idx += 1;
                continue;
            }

            // headings
            if line.starts_with("# ") {
                output.push(RLine::from(RSpan::styled(
                    line[2..].to_string(),
                    RStyle::default().fg(TUI_YELLOW).add_modifier(RModifier::BOLD),
                )));
                idx += 1;
                continue;
            }
            if line.starts_with("## ") {
                output.push(RLine::from(RSpan::styled(
                    line[3..].to_string(),
                    RStyle::default().fg(TUI_CYAN).add_modifier(RModifier::BOLD),
                )));
                idx += 1;
                continue;
            }
            if line.starts_with("### ") || line.starts_with("#### ") {
                let text_start = if line.starts_with("#### ") { 5 } else { 4 };
                output.push(RLine::from(RSpan::styled(
                    line[text_start..].to_string(),
                    RStyle::default().fg(TUI_PURPLE).add_modifier(RModifier::BOLD),
                )));
                idx += 1;
                continue;
            }

            // blockquote
            if line.starts_with("> ") {
                output.push(RLine::from(vec![
                    RSpan::styled("\u{2502} ", RStyle::default().fg(TUI_DIM)),
                    RSpan::styled(
                        line[2..].to_string(),
                        RStyle::default().fg(TUI_MUTED).add_modifier(RModifier::ITALIC),
                    ),
                ]));
                idx += 1;
                continue;
            }

            // unordered list
            if line.starts_with("- ") || line.starts_with("* ") || line.starts_with("+ ") {
                let mut spans = vec![
                    RSpan::styled("  \u{2022} ", RStyle::default().fg(TUI_ACCENT)),
                ];
                spans.extend(tui_inline_spans(&line[2..]));
                output.push(RLine::from(spans));
                idx += 1;
                continue;
            }

            // ordered list
            if let Some((num, rest)) = parse_ordered_list(line) {
                let mut spans = vec![
                    RSpan::styled(
                        format!("  {num}. "),
                        RStyle::default().fg(TUI_ACCENT),
                    ),
                ];
                spans.extend(tui_inline_spans(rest));
                output.push(RLine::from(spans));
                idx += 1;
                continue;
            }

            // empty line
            if line.is_empty() {
                output.push(RLine::from(""));
                idx += 1;
                continue;
            }

            // regular text with inline formatting
            output.push(RLine::from(tui_inline_spans(line)));
            idx += 1;
        }

        output
    }
}

fn is_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() > 1
}

fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    if !is_table_row(trimmed) {
        return false;
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    inner
        .split('|')
        .all(|cell| cell.trim().chars().all(|c| c == '-' || c == ':'))
}

fn parse_table_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = if trimmed.starts_with('|') && trimmed.ends_with('|') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

fn render_tui_table(rows: &[&str]) -> Vec<RLine<'static>> {
    let mut output = Vec::new();

    let parsed: Vec<Vec<String>> = rows.iter().map(|r| parse_table_cells(r)).collect();
    let num_cols = parsed.iter().map(|r| r.len()).max().unwrap_or(0);
    if num_cols == 0 {
        return output;
    }

    let sep_idx = rows
        .iter()
        .position(|r| is_table_separator(r));

    // column widths based on content (excluding separator row)
    let mut col_widths = vec![0usize; num_cols];
    for (row_idx, cells) in parsed.iter().enumerate() {
        if Some(row_idx) == sep_idx {
            continue;
        }
        for (col_idx, cell) in cells.iter().enumerate() {
            if col_idx < num_cols {
                col_widths[col_idx] = col_widths[col_idx].max(cell.len());
            }
        }
    }

    let pipe_style = RStyle::default().fg(TUI_DIM);
    let sep_style = RStyle::default().fg(TUI_DIM);

    for (row_idx, cells) in parsed.iter().enumerate() {
        if Some(row_idx) == sep_idx {
            // horizontal separator: ─┼─ between columns
            let mut spans: Vec<RSpan<'static>> = Vec::new();
            for (col_idx, width) in col_widths.iter().enumerate() {
                if col_idx > 0 {
                    spans.push(RSpan::styled("\u{2500}\u{253c}\u{2500}", sep_style));
                }
                spans.push(RSpan::styled(
                    "\u{2500}".repeat(*width),
                    sep_style,
                ));
            }
            output.push(RLine::from(spans));
            continue;
        }

        let is_header = sep_idx == Some(row_idx + 1);

        let mut spans: Vec<RSpan<'static>> = Vec::new();
        for (col_idx, width) in col_widths.iter().enumerate() {
            if col_idx > 0 {
                spans.push(RSpan::styled(" \u{2502} ", pipe_style));
            }
            let cell = cells.get(col_idx).map(|s| s.as_str()).unwrap_or("");
            let padding = width.saturating_sub(cell.len());

            if is_header {
                spans.push(RSpan::styled(
                    cell.to_string(),
                    RStyle::default()
                        .fg(TUI_FG)
                        .add_modifier(RModifier::BOLD),
                ));
            } else {
                spans.extend(tui_inline_spans(cell));
            }
            if padding > 0 {
                spans.push(RSpan::styled(
                    " ".repeat(padding),
                    RStyle::default(),
                ));
            }
        }
        output.push(RLine::from(spans));
    }

    output
}

// parse inline markdown into ratatui spans
fn tui_inline_spans(text: &str) -> Vec<RSpan<'static>> {
    let mut spans = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut buf = String::new();

    let flush = |buf: &mut String, spans: &mut Vec<RSpan<'static>>| {
        if !buf.is_empty() {
            spans.push(RSpan::styled(
                std::mem::take(buf),
                RStyle::default().fg(TUI_FG),
            ));
        }
    };

    while i < len {
        // bold italic ***
        if i + 2 < len && chars[i] == '*' && chars[i + 1] == '*' && chars[i + 2] == '*' {
            if let Some(end) = find_closing(&chars, i + 3, &['*', '*', '*']) {
                flush(&mut buf, &mut spans);
                let inner: String = chars[i + 3..end].iter().collect();
                spans.push(RSpan::styled(
                    inner,
                    RStyle::default().fg(TUI_FG).add_modifier(RModifier::BOLD | RModifier::ITALIC),
                ));
                i = end + 3;
                continue;
            }
        }

        // bold **
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_closing(&chars, i + 2, &['*', '*']) {
                flush(&mut buf, &mut spans);
                let inner: String = chars[i + 2..end].iter().collect();
                spans.push(RSpan::styled(
                    inner,
                    RStyle::default().fg(TUI_FG).add_modifier(RModifier::BOLD),
                ));
                i = end + 2;
                continue;
            }
        }

        // italic *
        if chars[i] == '*' && (i + 1 < len && chars[i + 1] != ' ') {
            if let Some(end) = find_closing_single(&chars, i + 1, '*') {
                flush(&mut buf, &mut spans);
                let inner: String = chars[i + 1..end].iter().collect();
                spans.push(RSpan::styled(
                    inner,
                    RStyle::default().fg(TUI_FG).add_modifier(RModifier::ITALIC),
                ));
                i = end + 1;
                continue;
            }
        }

        // inline code `
        if chars[i] == '`' {
            if let Some(end) = find_closing_single(&chars, i + 1, '`') {
                flush(&mut buf, &mut spans);
                let inner: String = chars[i + 1..end].iter().collect();
                spans.push(RSpan::styled(
                    inner,
                    RStyle::default().fg(TUI_YELLOW).bg(TUI_CODE_BG),
                ));
                i = end + 1;
                continue;
            }
        }

        // links [text](url)
        if chars[i] == '[' {
            if let Some((link_text, url, end_pos)) = parse_link(&chars, i) {
                flush(&mut buf, &mut spans);
                spans.push(RSpan::styled(
                    link_text,
                    RStyle::default().fg(TUI_CYAN).add_modifier(RModifier::UNDERLINED),
                ));
                spans.push(RSpan::styled(
                    format!(" ({})", url),
                    RStyle::default().fg(TUI_DIM),
                ));
                i = end_pos;
                continue;
            }
        }

        buf.push(chars[i]);
        i += 1;
    }

    flush(&mut buf, &mut spans);
    spans
}
