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

pub fn load_history(cwd: &Path) -> Vec<Message> {
    let path = history_dir_for(cwd).join("messages.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

pub fn save_history(cwd: &Path, messages: &[Message]) -> Result<()> {
    let dir = history_dir_for(cwd);
    std::fs::create_dir_all(&dir)?;
    // path.txt lets you tell which cwd this history belongs to
    std::fs::write(dir.join("path.txt"), cwd.to_string_lossy().as_ref())?;
    let content = serde_json::to_string(messages)?;
    std::fs::write(dir.join("messages.json"), content)?;
    Ok(())
}

pub fn clear_history(cwd: &Path) -> Result<()> {
    let path = history_dir_for(cwd).join("messages.json");
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}
