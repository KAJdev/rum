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
    /// human-readable list of config sources that were loaded, shown on startup
    pub loaded_sources: Vec<String>,
}

fn rum_config_dir() -> PathBuf {
    crate::persistence::rum_config_dir()
}

fn pi_agent_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".pi")
        .join("agent")
}

fn load_json_file<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// settings shape shared by pi and rum config files
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SettingsFile {
    default_model: Option<String>,
    default_thinking_level: Option<String>,
    #[allow(dead_code)]
    default_provider: Option<String>,
}

/// collect AGENTS.md / CLAUDE.md from ancestor directories, and from
/// the global config dirs for pi (~/.pi/agent) and claude (~/.claude)
fn collect_context_files(cwd: &Path, sources: &mut Vec<String>) -> Vec<String> {
    let mut files = Vec::new();

    // global context files from ~/.config/rum, ~/.pi/agent, ~/.claude
    let global_dirs = [
        rum_config_dir(),
        pi_agent_dir(),
        dirs::home_dir().unwrap_or_default().join(".claude"),
    ];
    for dir in &global_dirs {
        for name in &["AGENTS.md", "CLAUDE.md"] {
            let p = dir.join(name);
            if let Ok(content) = std::fs::read_to_string(&p) {
                files.push(content);
                sources.push(format!("{}", p.display()));
            }
        }
    }

    // walk from root to cwd collecting AGENTS.md / CLAUDE.md
    let mut ancestors: Vec<&Path> = cwd.ancestors().collect();
    ancestors.reverse();
    for dir in ancestors {
        for name in &["AGENTS.md", "CLAUDE.md"] {
            let p = dir.join(name);
            if let Ok(content) = std::fs::read_to_string(&p) {
                files.push(content);
                sources.push(format!("{}", p.display()));
            }
        }
    }

    files
}

/// load system prompt, checking project and global dirs across rum, pi, and claude
fn load_system_prompt(cwd: &Path, sources: &mut Vec<String>) -> String {
    let rum_dir = rum_config_dir();
    let pi_dir = pi_agent_dir();

    // priority order for base system prompt: project .rum > project .pi >
    // project .claude > global rum > global pi > built-in default
    let system_candidates = [
        cwd.join(".rum").join("SYSTEM.md"),
        cwd.join(".pi").join("SYSTEM.md"),
        cwd.join(".claude").join("SYSTEM.md"),
        rum_dir.join("SYSTEM.md"),
        pi_dir.join("SYSTEM.md"),
    ];

    let base = system_candidates
        .iter()
        .find_map(|p| {
            std::fs::read_to_string(p).ok().map(|c| {
                sources.push(format!("{}", p.display()));
                c
            })
        })
        .unwrap_or_else(default_system_prompt);

    // append files from global then project (both rum and pi dirs)
    let append_candidates = [
        rum_dir.join("APPEND_SYSTEM.md"),
        pi_dir.join("APPEND_SYSTEM.md"),
        cwd.join(".rum").join("APPEND_SYSTEM.md"),
        cwd.join(".pi").join("APPEND_SYSTEM.md"),
        cwd.join(".claude").join("APPEND_SYSTEM.md"),
    ];

    let mut append = String::new();
    for path in &append_candidates {
        if let Ok(content) = std::fs::read_to_string(path) {
            append.push_str("\n\n");
            append.push_str(&content);
            sources.push(format!("{}", path.display()));
        }
    }

    format!("{}{}", base, append)
}

fn default_system_prompt() -> String {
    r#"You are an expert coding assistant. You help users by reading files, executing commands, editing code, and writing new files.

Available tools:
- read: Read file contents
- bash: Execute bash commands (ls, grep, find, etc.). Optional timeout in seconds (default 120).
- edit: Make surgical edits to files (find exact text and replace)
- write: Create or overwrite files
- web_search: Search the web using DuckDuckGo
- view_file: View an image file (JPEG, PNG, GIF, WebP) and analyze its visual contents

Guidelines:
- Use bash for file operations like ls, rg, find
- Use read to examine files before editing
- Use edit for precise changes (old text must match exactly)
- Use write only for new files or complete rewrites
- Use view_file when the user references an image or screenshot
- Be concise in your responses
- Show file paths clearly when working with files"#
        .to_string()
}

pub fn load_config(cwd: &Path) -> Result<Config> {
    let mut loaded_sources: Vec<String> = Vec::new();

    // merge settings: rum > pi (first found for each field wins)
    let rum_cfg: SettingsFile =
        load_json_file(&rum_config_dir().join("config.json")).unwrap_or_default();
    let pi_cfg: SettingsFile =
        load_json_file(&pi_agent_dir().join("settings.json")).unwrap_or_default();

    let model = rum_cfg
        .default_model
        .or(pi_cfg.default_model)
        .unwrap_or_else(|| "claude-sonnet-4-0".to_string());

    let thinking_level = rum_cfg
        .default_thinking_level
        .or(pi_cfg.default_thinking_level)
        .unwrap_or_else(|| "off".to_string());

    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let oauth = crate::auth::load_auth();

    let system_prompt = load_system_prompt(cwd, &mut loaded_sources);
    let context_files = collect_context_files(cwd, &mut loaded_sources);

    Ok(Config {
        provider: "anthropic".to_string(),
        model,
        thinking_level,
        api_key,
        oauth,
        system_prompt,
        context_files,
        loaded_sources,
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
