use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::auth::OAuthCredentials;

pub struct Config {
    pub provider: String,
    pub model: String,
    pub thinking_level: String,
    pub api_key: Option<String>,
    pub oauth: Option<OAuthCredentials>,
    pub system_prompt: String,
    pub context_files: Vec<String>,
}

fn rum_config_dir() -> PathBuf {
    crate::persistence::rum_config_dir()
}

fn load_json_file<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RumConfigFile {
    default_model: Option<String>,
    default_thinking_level: Option<String>,
}

fn collect_context_files(cwd: &Path) -> Vec<String> {
    let mut files = Vec::new();

    // walk from root to cwd collecting AGENTS.md / CLAUDE.md
    let mut ancestors: Vec<&Path> = cwd.ancestors().collect();
    ancestors.reverse();
    for dir in ancestors {
        for name in &["AGENTS.md", "CLAUDE.md"] {
            let p = dir.join(name);
            if let Ok(content) = std::fs::read_to_string(&p) {
                files.push(content);
            }
        }
    }

    files
}

fn load_system_prompt(cwd: &Path) -> String {
    let config_dir = rum_config_dir();

    let project_system = cwd.join(".rum").join("SYSTEM.md");
    let global_system = config_dir.join("SYSTEM.md");

    let base = if project_system.exists() {
        std::fs::read_to_string(&project_system).unwrap_or_default()
    } else if global_system.exists() {
        std::fs::read_to_string(&global_system).unwrap_or_default()
    } else {
        default_system_prompt()
    };

    let mut append = String::new();
    let project_append = cwd.join(".rum").join("APPEND_SYSTEM.md");
    let global_append = config_dir.join("APPEND_SYSTEM.md");

    for path in &[global_append, project_append] {
        if let Ok(content) = std::fs::read_to_string(path) {
            append.push_str("\n\n");
            append.push_str(&content);
        }
    }

    format!("{}{}", base, append)
}

fn default_system_prompt() -> String {
    r#"You are an expert coding assistant. You help users by reading files, executing commands, editing code, and writing new files.

Available tools:
- read: Read file contents
- bash: Execute bash commands (ls, grep, find, etc.)
- edit: Make surgical edits to files (find exact text and replace)
- write: Create or overwrite files
- web_search: Search the web using DuckDuckGo

Guidelines:
- Use bash for file operations like ls, rg, find
- Use read to examine files before editing
- Use edit for precise changes (old text must match exactly)
- Use write only for new files or complete rewrites
- Be concise in your responses
- Show file paths clearly when working with files"#
        .to_string()
}

pub fn load_config(cwd: &Path) -> Result<Config> {
    let config_dir = rum_config_dir();
    let cfg: RumConfigFile =
        load_json_file(&config_dir.join("config.json")).unwrap_or_default();

    let model = cfg
        .default_model
        .unwrap_or_else(|| "claude-sonnet-4-0".to_string());

    let thinking_level = cfg
        .default_thinking_level
        .unwrap_or_else(|| "off".to_string());

    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let oauth = crate::auth::load_auth();

    let system_prompt = load_system_prompt(cwd);
    let context_files = collect_context_files(cwd);

    Ok(Config {
        provider: "anthropic".to_string(),
        model,
        thinking_level,
        api_key,
        oauth,
        system_prompt,
        context_files,
    })
}

// per-million-token pricing for anthropic models
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
}

pub struct ModelDef {
    pub id: &'static str,
    pub name: &'static str,
    pub context_window: u32,
    pub input_price: f64,
    pub output_price: f64,
}

pub const ANTHROPIC_MODELS: &[ModelDef] = &[
    ModelDef {
        id: "claude-opus-4-6",
        name: "Opus 4.6",
        context_window: 200_000,
        input_price: 5.0,
        output_price: 25.0,
    },
    ModelDef {
        id: "claude-opus-4-5",
        name: "Opus 4.5",
        context_window: 200_000,
        input_price: 5.0,
        output_price: 25.0,
    },
    ModelDef {
        id: "claude-opus-4-1",
        name: "Opus 4.1",
        context_window: 200_000,
        input_price: 15.0,
        output_price: 75.0,
    },
    ModelDef {
        id: "claude-opus-4-0",
        name: "Opus 4",
        context_window: 200_000,
        input_price: 15.0,
        output_price: 75.0,
    },
    ModelDef {
        id: "claude-sonnet-4-6",
        name: "Sonnet 4.6",
        context_window: 200_000,
        input_price: 3.0,
        output_price: 15.0,
    },
    ModelDef {
        id: "claude-sonnet-4-5",
        name: "Sonnet 4.5",
        context_window: 200_000,
        input_price: 3.0,
        output_price: 15.0,
    },
    ModelDef {
        id: "claude-sonnet-4-0",
        name: "Sonnet 4",
        context_window: 200_000,
        input_price: 3.0,
        output_price: 15.0,
    },
    ModelDef {
        id: "claude-haiku-4-5",
        name: "Haiku 4.5",
        context_window: 200_000,
        input_price: 1.0,
        output_price: 5.0,
    },
];

pub const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh"];

pub fn match_model(pattern: &str) -> Option<&'static ModelDef> {
    let p = pattern.to_lowercase();

    // exact id match
    if let Some(m) = ANTHROPIC_MODELS.iter().find(|m| m.id.to_lowercase() == p) {
        return Some(m);
    }

    // common aliases
    let resolved = match p.as_str() {
        "opus" | "opus-4-6" | "opus-4.6" | "opus46" => Some("claude-opus-4-6"),
        "opus-4-5" | "opus-4.5" | "opus45" => Some("claude-opus-4-5"),
        "opus-4-1" | "opus-4.1" | "opus41" => Some("claude-opus-4-1"),
        "opus-4" | "opus-4-0" | "opus-4.0" | "opus4" => Some("claude-opus-4-0"),
        "sonnet" | "sonnet-4-6" | "sonnet-4.6" | "sonnet46" => Some("claude-sonnet-4-6"),
        "sonnet-4-5" | "sonnet-4.5" | "sonnet45" => Some("claude-sonnet-4-5"),
        "sonnet-4" | "sonnet-4-0" | "sonnet-4.0" | "sonnet4" => Some("claude-sonnet-4-0"),
        "haiku" | "haiku-4-5" | "haiku-4.5" | "haiku45" => Some("claude-haiku-4-5"),
        _ => None,
    };

    if let Some(id) = resolved {
        return ANTHROPIC_MODELS.iter().find(|m| m.id == id);
    }

    // partial id or display name match
    ANTHROPIC_MODELS
        .iter()
        .find(|m| m.id.to_lowercase().contains(&p) || m.name.to_lowercase().contains(&p))
}

pub fn model_pricing(model: &str) -> ModelPricing {
    if let Some(def) = ANTHROPIC_MODELS.iter().find(|m| m.id == model) {
        return ModelPricing {
            input: def.input_price,
            output: def.output_price,
        };
    }
    let m = model.to_lowercase();
    if m.contains("opus") {
        ModelPricing { input: 5.0, output: 25.0 }
    } else if m.contains("haiku") {
        ModelPricing { input: 1.0, output: 5.0 }
    } else {
        ModelPricing { input: 3.0, output: 15.0 }
    }
}
