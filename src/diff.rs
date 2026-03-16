use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone)]
pub struct DiffStat {
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Debug, Clone)]
pub struct DiffInfo {
    pub path: String,
    pub stat: DiffStat,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone)]
pub struct DiffHunk {
    pub new_start: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub tag: DiffLineTag,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiffLineTag {
    Equal,
    Insert,
    Delete,
}

pub fn compute_diff(path: &str, old: &str, new: &str) -> DiffInfo {
    let text_diff = TextDiff::from_lines(old, new);
    let mut additions = 0;
    let mut deletions = 0;
    let mut hunks = Vec::new();

    for group in text_diff.grouped_ops(3) {
        let new_start = group.first().map(|op| op.new_range().start).unwrap_or(0);
        let mut hunk = DiffHunk {
            new_start,
            lines: Vec::new(),
        };
        for op in &group {
            for change in text_diff.iter_changes(op) {
                let tag = match change.tag() {
                    ChangeTag::Equal => DiffLineTag::Equal,
                    ChangeTag::Insert => {
                        additions += 1;
                        DiffLineTag::Insert
                    }
                    ChangeTag::Delete => {
                        deletions += 1;
                        DiffLineTag::Delete
                    }
                };
                hunk.lines.push(DiffLine {
                    tag,
                    content: change.value().to_string(),
                });
            }
        }
        if !hunk.lines.is_empty() {
            hunks.push(hunk);
        }
    }

    DiffInfo {
        path: path.to_string(),
        stat: DiffStat {
            additions,
            deletions,
        },
        hunks,
    }
}
