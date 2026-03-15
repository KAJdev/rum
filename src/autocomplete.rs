use std::collections::HashSet;
use std::path::Path;

// completion item shown in the autocomplete menu
#[derive(Debug, Clone)]
pub struct Completion {
    pub label: String,
    pub kind: CompletionKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompletionKind {
    Keyword,
    Identifier,
}

// active autocomplete session state
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AutocompleteState {
    // byte offset in the line where the completing word starts
    pub word_start: usize,
    pub prefix: String,
    pub candidates: Vec<Completion>,
    pub selected: usize,
}

impl AutocompleteState {
    pub fn select_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn select_down(&mut self) {
        if !self.candidates.is_empty() && self.selected + 1 < self.candidates.len() {
            self.selected += 1;
        }
    }
}

// extract the identifier prefix ending at byte position `col` in `line`
// extract the completing word and detect trigger context.
// returns (byte offset where replacement starts, prefix to match, whether a trigger char precedes)
pub fn word_at_cursor(line: &str, col: usize) -> (usize, String, bool) {
    let safe = col.min(line.len());
    let before = &line[..safe];

    // collect trailing identifier chars
    let word: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let start = safe - word.len();

    // check if a trigger character precedes the word (or is at cursor with no word)
    let pre = &before[..start];
    let has_trigger = pre.ends_with('.')
        || pre.ends_with("::")
        || pre.ends_with("->")
        || pre.ends_with('-');

    (start, word, has_trigger)
}

// build completions from identifiers in the current file + language keywords
pub fn compute_completions(
    lines: &[String],
    cursor_row: usize,
    cursor_col: usize,
    file_path: &Path,
) -> Option<AutocompleteState> {
    let line = lines.get(cursor_row)?;
    let (word_start, prefix, has_trigger) = word_at_cursor(line, cursor_col);

    // need either a prefix or a trigger character
    if prefix.is_empty() && !has_trigger {
        return None;
    }

    let keywords = keywords_for_file(file_path);

    // collect unique identifiers from the file
    let mut seen = HashSet::new();
    let mut candidates: Vec<(i32, Completion)> = Vec::new();

    for keyword in keywords {
        if let Some(score) = crate::editor::fuzzy_match(&prefix, keyword) {
            if seen.insert(keyword.to_string()) {
                candidates.push((score, Completion {
                    label: keyword.to_string(),
                    kind: CompletionKind::Keyword,
                }));
            }
        }
    }

    for file_line in lines {
        for word in split_identifiers(file_line) {
            if word == prefix || word.len() < 2 {
                continue;
            }
            if seen.contains(word) {
                continue;
            }
            if let Some(score) = crate::editor::fuzzy_match(&prefix, word) {
                seen.insert(word.to_string());
                candidates.push((score, Completion {
                    label: word.to_string(),
                    kind: CompletionKind::Identifier,
                }));
            }
        }
    }

    if candidates.is_empty() {
        return None;
    }

    // sort: higher score first, then shorter label, then alphabetic
    candidates.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.label.len().cmp(&b.1.label.len()))
            .then_with(|| a.1.label.cmp(&b.1.label))
    });

    // cap at 12 results
    candidates.truncate(12);

    Some(AutocompleteState {
        word_start,
        prefix,
        candidates: candidates.into_iter().map(|(_, c)| c).collect(),
        selected: 0,
    })
}

// split a line into identifier tokens
fn split_identifiers(line: &str) -> Vec<&str> {
    let mut results = Vec::new();
    let bytes = line.as_bytes();
    let mut start = None;

    for (i, &b) in bytes.iter().enumerate() {
        let is_ident = b.is_ascii_alphanumeric() || b == b'_';
        match (is_ident, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                if let Ok(word) = std::str::from_utf8(&bytes[s..i]) {
                    // skip pure numeric tokens
                    if !word.chars().next().unwrap_or('0').is_ascii_digit() {
                        results.push(word);
                    }
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        if let Ok(word) = std::str::from_utf8(&bytes[s..]) {
            if !word.chars().next().unwrap_or('0').is_ascii_digit() {
                results.push(word);
            }
        }
    }

    results
}

fn keywords_for_file(path: &Path) -> &'static [&'static str] {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "rs" => &[
            "as", "async", "await", "break", "const", "continue", "crate", "dyn",
            "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in",
            "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
            "self", "Self", "static", "struct", "super", "trait", "true", "type",
            "unsafe", "use", "where", "while", "yield",
            "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128",
            "isize", "str", "u8", "u16", "u32", "u64", "u128", "usize",
            "String", "Vec", "Option", "Result", "Some", "None", "Ok", "Err",
            "Box", "Rc", "Arc", "HashMap", "HashSet", "BTreeMap", "BTreeSet",
            "println", "eprintln", "format", "panic", "todo", "unimplemented",
            "derive", "allow", "cfg", "test", "Clone", "Debug", "Default",
            "Display", "Send", "Sync", "Copy", "Drop", "From", "Into",
        ],
        "js" | "jsx" | "mjs" => &[
            "async", "await", "break", "case", "catch", "class", "const",
            "continue", "debugger", "default", "delete", "do", "else", "export",
            "extends", "false", "finally", "for", "function", "if", "import",
            "in", "instanceof", "let", "new", "null", "of", "return", "static",
            "super", "switch", "this", "throw", "true", "try", "typeof",
            "undefined", "var", "void", "while", "with", "yield",
            "console", "document", "window", "Array", "Object", "Promise",
            "Map", "Set", "JSON", "Math", "Date", "RegExp", "Error",
            "parseInt", "parseFloat", "setTimeout", "setInterval",
            "addEventListener", "querySelector", "querySelectorAll",
            "createElement", "appendChild", "removeChild",
        ],
        "ts" | "tsx" => &[
            "abstract", "as", "async", "await", "break", "case", "catch",
            "class", "const", "continue", "debugger", "declare", "default",
            "delete", "do", "else", "enum", "export", "extends", "false",
            "finally", "for", "from", "function", "get", "if", "implements",
            "import", "in", "infer", "instanceof", "interface", "is", "keyof",
            "let", "module", "namespace", "never", "new", "null", "of",
            "override", "private", "protected", "public", "readonly", "return",
            "satisfies", "set", "static", "string", "super", "switch", "this",
            "throw", "true", "try", "type", "typeof", "undefined", "unique",
            "unknown", "var", "void", "while", "with", "yield",
            "any", "boolean", "number", "object", "symbol", "bigint",
            "Array", "Object", "Promise", "Map", "Set", "Record", "Partial",
            "Required", "Readonly", "Pick", "Omit", "Exclude", "Extract",
            "NonNullable", "ReturnType", "Parameters", "Awaited",
        ],
        "py" => &[
            "False", "None", "True", "and", "as", "assert", "async", "await",
            "break", "class", "continue", "def", "del", "elif", "else",
            "except", "finally", "for", "from", "global", "if", "import",
            "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise",
            "return", "try", "while", "with", "yield",
            "print", "len", "range", "enumerate", "zip", "map", "filter",
            "list", "dict", "set", "tuple", "int", "float", "str", "bool",
            "isinstance", "issubclass", "hasattr", "getattr", "setattr",
            "super", "property", "staticmethod", "classmethod",
            "ValueError", "TypeError", "KeyError", "IndexError",
            "RuntimeError", "FileNotFoundError", "ImportError",
        ],
        "go" => &[
            "break", "case", "chan", "const", "continue", "default", "defer",
            "else", "fallthrough", "for", "func", "go", "goto", "if",
            "import", "interface", "map", "package", "range", "return",
            "select", "struct", "switch", "type", "var",
            "bool", "byte", "complex64", "complex128", "error", "float32",
            "float64", "int", "int8", "int16", "int32", "int64", "rune",
            "string", "uint", "uint8", "uint16", "uint32", "uint64", "uintptr",
            "true", "false", "nil", "iota", "append", "cap", "close",
            "copy", "delete", "len", "make", "new", "panic", "print",
            "println", "recover",
        ],
        "c" | "h" => &[
            "auto", "break", "case", "char", "const", "continue", "default",
            "do", "double", "else", "enum", "extern", "float", "for", "goto",
            "if", "inline", "int", "long", "register", "restrict", "return",
            "short", "signed", "sizeof", "static", "struct", "switch",
            "typedef", "union", "unsigned", "void", "volatile", "while",
            "NULL", "stdin", "stdout", "stderr", "printf", "fprintf",
            "sprintf", "scanf", "malloc", "calloc", "realloc", "free",
            "memcpy", "memset", "strlen", "strcmp", "strcpy", "strcat",
            "sizeof", "typeof", "include", "define", "ifdef", "ifndef",
        ],
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => &[
            "alignas", "alignof", "and", "auto", "bool", "break", "case",
            "catch", "char", "class", "const", "constexpr", "continue",
            "decltype", "default", "delete", "do", "double", "dynamic_cast",
            "else", "enum", "explicit", "export", "extern", "false", "float",
            "for", "friend", "goto", "if", "inline", "int", "long",
            "mutable", "namespace", "new", "noexcept", "not", "nullptr",
            "operator", "or", "override", "private", "protected", "public",
            "register", "return", "short", "signed", "sizeof", "static",
            "static_cast", "struct", "switch", "template", "this", "throw",
            "true", "try", "typedef", "typeid", "typename", "union",
            "unsigned", "using", "virtual", "void", "volatile", "while",
            "string", "vector", "map", "set", "pair", "tuple", "array",
            "unique_ptr", "shared_ptr", "weak_ptr", "optional", "variant",
            "cout", "cerr", "cin", "endl", "std",
        ],
        _ => &[],
    }
}
