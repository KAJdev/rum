use crate::api::{ContentBlock, Message, MessageContent};
use crate::persistence::SessionTree;

#[derive(Debug, Clone)]
pub enum NodeKind {
    UserMessage,
    AssistantText,
    ToolCall,
    Thinking,
    Compact,
}

// one visible row in the flattened tree display
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub kind: NodeKind,
    pub text: String,
    // which branch and message index this node belongs to
    pub branch_idx: usize,
    pub msg_idx: usize,
    // visual depth for indentation
    pub depth: usize,
    // connector info for drawing tree lines
    pub is_last_child: bool,
    // depths that have a continuing vertical line
    pub active_pipes: Vec<usize>,
    // whether this node starts a branch segment
    pub branch_head: bool,
}

pub struct TreeView {
    pub nodes: Vec<TreeNode>,
    pub cursor: usize,
    pub scroll: usize,
}

impl TreeView {
    pub fn build(tree: &SessionTree) -> Self {
        let nodes = build_tree_nodes(tree);
        // place cursor on the last node of the active branch
        let active = tree.active;
        let cursor = nodes
            .iter()
            .rposition(|n| n.branch_idx == active)
            .unwrap_or(0);
        // start scroll so cursor is visible
        let h = crossterm::terminal::size()
            .map(|(_, h)| h)
            .unwrap_or(24) as usize;
        let viewport = h.saturating_sub(2);
        let scroll = if cursor >= viewport {
            cursor + 1 - viewport
        } else {
            0
        };
        Self {
            nodes,
            cursor,
            scroll,
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 < self.nodes.len() {
            self.cursor += 1;
        }
    }

    // jump to previous user message
    pub fn jump_prev_user(&mut self) {
        for i in (0..self.cursor).rev() {
            if matches!(self.nodes[i].kind, NodeKind::UserMessage) {
                self.cursor = i;
                return;
            }
        }
    }

    // jump to next user message
    pub fn jump_next_user(&mut self) {
        for i in (self.cursor + 1)..self.nodes.len() {
            if matches!(self.nodes[i].kind, NodeKind::UserMessage) {
                self.cursor = i;
                return;
            }
        }
    }

    pub fn ensure_visible(&mut self, viewport_height: usize) {
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + viewport_height {
            self.scroll = self.cursor + 1 - viewport_height;
        }
    }

    // the branch and message index at the current cursor
    pub fn selected(&self) -> Option<(usize, usize)> {
        self.nodes
            .get(self.cursor)
            .map(|n| (n.branch_idx, n.msg_idx))
    }

    // the branch index at the current cursor
    pub fn selected_branch(&self) -> Option<usize> {
        self.nodes.get(self.cursor).map(|n| n.branch_idx)
    }
}

// extract display nodes from a single message
fn nodes_from_message(
    msg: &Message,
    _branch_idx: usize,
    _msg_idx: usize,
) -> Vec<(NodeKind, String)> {
    let mut out = Vec::new();
    match &msg.content {
        MessageContent::Text(s) => {
            if msg.role == "user" && !s.trim().is_empty() {
                let preview = truncate_preview(s, 80);
                out.push((NodeKind::UserMessage, preview));
            } else if msg.role == "assistant" && !s.trim().is_empty() {
                let preview = truncate_preview(s, 80);
                out.push((NodeKind::AssistantText, preview));
            }
        }
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    ContentBlock::Text { text } if !text.trim().is_empty() => {
                        let preview = truncate_preview(text, 80);
                        if msg.role == "user" {
                            out.push((NodeKind::UserMessage, preview));
                        } else {
                            out.push((NodeKind::AssistantText, preview));
                        }
                    }
                    ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                        let preview = truncate_preview(thinking, 50);
                        out.push((NodeKind::Thinking, format!("thinking: {preview}")));
                    }
                    ContentBlock::ToolUse { name, input, .. } => {
                        let arg = extract_tool_arg(name, input);
                        let display_name = crate::agent::from_cc_name(name);
                        out.push((NodeKind::ToolCall, format!("{display_name} {arg}")));
                    }
                    ContentBlock::Compaction { .. } => {
                        out.push((NodeKind::Compact, "compacted".to_string()));
                    }
                    // skip tool results, empty text, etc.
                    _ => {}
                }
            }
        }
    }
    out
}

fn extract_tool_arg(name: &str, input: &serde_json::Value) -> String {
    let display = crate::agent::from_cc_name(name);
    match display {
        "read" | "write" | "edit" => input
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| truncate_preview(s, 60))
            .unwrap_or_default(),
        "explore" => input
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(|s| truncate_preview(s, 60))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn truncate_preview(s: &str, max: usize) -> String {
    let first_line = s.lines().next().unwrap_or(s);
    if first_line.len() > max {
        format!("{}...", crate::util::truncate_str(first_line, max))
    } else {
        first_line.to_string()
    }
}

// build the flattened, depth-annotated node list from a session tree.
//
// the algorithm:
// 1. find the "trunk" - the shared prefix of the first branch
// 2. at each message in the trunk, check if other branches fork here
// 3. recursively render each sub-branch with increased depth
fn build_tree_nodes(tree: &SessionTree) -> Vec<TreeNode> {
    if tree.branches.is_empty() {
        return Vec::new();
    }

    // build a children map: for each (branch, msg_idx), which branches fork here?
    let mut fork_children: std::collections::HashMap<(usize, usize), Vec<usize>> =
        std::collections::HashMap::new();
    for (i, branch) in tree.branches.iter().enumerate() {
        if let Some((parent, msg_idx)) = branch.fork_from {
            fork_children
                .entry((parent, msg_idx))
                .or_default()
                .push(i);
        }
    }

    let mut nodes = Vec::new();
    render_branch(tree, 0, 0, 0, true, &Vec::new(), &fork_children, &mut nodes);
    nodes
}

// recursively render a branch starting from start_msg_idx
fn render_branch(
    tree: &SessionTree,
    branch_idx: usize,
    start_msg_idx: usize,
    depth: usize,
    is_last: bool,
    parent_pipes: &[usize],
    fork_children: &std::collections::HashMap<(usize, usize), Vec<usize>>,
    out: &mut Vec<TreeNode>,
) {
    let branch = &tree.branches[branch_idx];
    let mut pipes = parent_pipes.to_vec();
    if depth > 0 && !is_last {
        pipes.push(depth - 1);
    }

    // if this branch has no unique messages past the start, show a stub
    if start_msg_idx >= branch.messages.len() && depth > 0 {
        out.push(TreeNode {
            kind: NodeKind::AssistantText,
            text: "(waiting for input)".to_string(),
            branch_idx,
            msg_idx: branch.messages.len().saturating_sub(1),
            depth,
            is_last_child: is_last,
            active_pipes: pipes.clone(),
            branch_head: true,
        });
        return;
    }

    for msg_idx in start_msg_idx..branch.messages.len() {
        let msg = &branch.messages[msg_idx];
        let display_nodes = nodes_from_message(msg, branch_idx, msg_idx);

        // check if any branches fork at this message
        let children = fork_children
            .get(&(branch_idx, msg_idx))
            .cloned()
            .unwrap_or_default();

        for (i, (kind, text)) in display_nodes.iter().enumerate() {
            out.push(TreeNode {
                kind: kind.clone(),
                text: text.clone(),
                branch_idx,
                msg_idx,
                depth,
                is_last_child: is_last,
                active_pipes: pipes.clone(),
                branch_head: msg_idx == start_msg_idx && i == 0 && depth > 0,
            });
        }

        // if there are forks at this message, render the continuation of this
        // branch and all forked branches as siblings
        if !children.is_empty() {
            let remaining = msg_idx + 1 < branch.messages.len();

            // render continuation of current branch first
            if remaining {
                let cont_is_last = children.is_empty();
                render_branch(
                    tree,
                    branch_idx,
                    msg_idx + 1,
                    depth + 1,
                    cont_is_last,
                    &pipes,
                    fork_children,
                    out,
                );
            }

            // render forked branches
            for (ci, &child_branch) in children.iter().enumerate() {
                let child_start = msg_idx + 1;
                render_branch(
                    tree,
                    child_branch,
                    child_start,
                    depth + 1,
                    ci == children.len() - 1,
                    &pipes,
                    fork_children,
                    out,
                );
            }

            return;
        }
    }
}
