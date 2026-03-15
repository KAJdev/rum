use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::api::Message;

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct RumSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diffs_expanded: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Branch {
    pub messages: Vec<Message>,
    // which branch this was forked from, and at what message index.
    // None for the initial branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork_from: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTree {
    pub branches: Vec<Branch>,
    pub active: usize,
}

impl SessionTree {
    pub fn new() -> Self {
        Self {
            branches: vec![Branch {
                messages: Vec::new(),
                fork_from: None,
            }],
            active: 0,
        }
    }

    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self {
            branches: vec![Branch {
                messages,
                fork_from: None,
            }],
            active: 0,
        }
    }

    pub fn active_messages(&self) -> &[Message] {
        &self.branches[self.active].messages
    }

    pub fn active_messages_mut(&mut self) -> &mut Vec<Message> {
        &mut self.branches[self.active].messages
    }

    // create a new branch forking from the given branch at the given message index.
    // copies messages[0..=msg_idx] into the new branch and switches to it.
    pub fn fork(&mut self, from_branch: usize, msg_idx: usize) -> usize {
        let messages = self.branches[from_branch].messages[..=msg_idx].to_vec();
        let new_idx = self.branches.len();
        self.branches.push(Branch {
            messages,
            fork_from: Some((from_branch, msg_idx)),
        });
        self.active = new_idx;
        new_idx
    }

    pub fn switch(&mut self, branch_idx: usize) {
        if branch_idx < self.branches.len() {
            self.active = branch_idx;
        }
    }
}

pub fn rum_config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".config")
        .join("rum")
}

fn history_dir_for(cwd: &Path) -> PathBuf {
    rum_config_dir().join("history").join(path_hash(cwd))
}

// fnv-1a hash of the absolute path string, used as a stable directory name
fn path_hash(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}

pub fn load_settings() -> RumSettings {
    let path = rum_config_dir().join("settings.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return RumSettings::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

pub fn save_settings(settings: &RumSettings) -> Result<()> {
    let dir = rum_config_dir();
    std::fs::create_dir_all(&dir)?;
    let content = serde_json::to_string_pretty(settings)?;
    std::fs::write(dir.join("settings.json"), content)?;
    Ok(())
}

// load session tree, migrating from legacy messages.json if needed
pub fn load_session(cwd: &Path) -> SessionTree {
    let dir = history_dir_for(cwd);
    let session_path = dir.join("session.json");
    if let Ok(content) = std::fs::read_to_string(&session_path) {
        if let Ok(tree) = serde_json::from_str::<SessionTree>(&content) {
            return tree;
        }
    }
    // migrate from legacy messages.json
    let messages_path = dir.join("messages.json");
    if let Ok(content) = std::fs::read_to_string(&messages_path) {
        if let Ok(messages) = serde_json::from_str::<Vec<Message>>(&content) {
            if !messages.is_empty() {
                return SessionTree::from_messages(messages);
            }
        }
    }
    SessionTree::new()
}

pub fn save_session(cwd: &Path, tree: &SessionTree) -> Result<()> {
    let dir = history_dir_for(cwd);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("path.txt"), cwd.to_string_lossy().as_ref())?;
    let content = serde_json::to_string(tree)?;
    std::fs::write(dir.join("session.json"), content)?;
    Ok(())
}

// legacy compat: load just the active branch messages
pub fn load_history(cwd: &Path) -> Vec<Message> {
    load_session(cwd).active_messages().to_vec()
}

// legacy compat: save active branch messages only
pub fn save_history(cwd: &Path, messages: &[Message]) -> Result<()> {
    let dir = history_dir_for(cwd);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("path.txt"), cwd.to_string_lossy().as_ref())?;
    let content = serde_json::to_string(messages)?;
    std::fs::write(dir.join("messages.json"), content)?;
    Ok(())
}

pub fn clear_history(cwd: &Path) -> Result<()> {
    let dir = history_dir_for(cwd);
    let messages_path = dir.join("messages.json");
    if messages_path.exists() {
        std::fs::remove_file(messages_path)?;
    }
    let session_path = dir.join("session.json");
    if session_path.exists() {
        std::fs::remove_file(session_path)?;
    }
    Ok(())
}
