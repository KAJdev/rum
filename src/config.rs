use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct PiSettings {
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub default_thinking_level: Option<String>,
    pub theme: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[allow(dead_code)]
pub enum AuthEntry {
    #[serde(rename = "api_key")]
    ApiKey { key: String },
    #[serde(rename = "oauth")]
    OAuth {
        access: String,
        refresh: String,
        expires: u64,
        #[serde(flatten)]
        extra: serde_json::Value,
    },
}



pub struct Config {
    pub provider: String,
    pub model: String,
    pub thinking_level: String,
    pub api_key: Option<String>,
    pub auth_entry: Option<AuthEntry>,
    pub system_prompt: String,
    pub context_files: Vec<String>,
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

fn collect_context_files(cwd: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let agent_dir = pi_agent_dir();

    // global agents.md
    for name in &["AGENTS.md", "CLAUDE.md"] {
        let p = agent_dir.join(name);
        if let Ok(content) = std::fs::read_to_string(&p) {
            files.push(content);
        }
    }

    // walk from root to cwd collecting context files
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
    let agent_dir = pi_agent_dir();

    // check for custom system prompt
    let project_system = cwd.join(".pi").join("SYSTEM.md");
    let global_system = agent_dir.join("SYSTEM.md");

    let base = if project_system.exists() {
        std::fs::read_to_string(&project_system).unwrap_or_default()
    } else if global_system.exists() {
        std::fs::read_to_string(&global_system).unwrap_or_default()
    } else {
        default_system_prompt()
    };

    // append system prompt
    let mut append = String::new();
    let project_append = cwd.join(".pi").join("APPEND_SYSTEM.md");
    let global_append = agent_dir.join("APPEND_SYSTEM.md");

    if global_append.exists() {
        if let Ok(content) = std::fs::read_to_string(&global_append) {
            append.push_str("\n\n");
            append.push_str(&content);
        }
    }
    if project_append.exists() {
        if let Ok(content) = std::fs::read_to_string(&project_append) {
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

pub fn resolve_api_key(provider: &str) -> Option<String> {
    // check env vars first
    let env_key = match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "google" => "GEMINI_API_KEY",
        _ => return None,
    };

    if let Ok(key) = std::env::var(env_key) {
        return Some(key);
    }

    None
}

pub fn load_config(cwd: &Path) -> Result<Config> {
    let agent_dir = pi_agent_dir();
    let global_settings: PiSettings =
        load_json_file(&agent_dir.join("settings.json")).unwrap_or_default();

    let project_settings: PiSettings =
        load_json_file(&cwd.join(".pi").join("settings.json")).unwrap_or_default();

    let provider = project_settings
        .default_provider
        .or(global_settings.default_provider)
        .unwrap_or_else(|| "anthropic".to_string());

    let model = project_settings
        .default_model
        .or(global_settings.default_model)
        .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());

    let thinking_level = project_settings
        .default_thinking_level
        .or(global_settings.default_thinking_level)
        .unwrap_or_else(|| "off".to_string());

    // load auth from pi's auth.json
    let auth_path = agent_dir.join("auth.json");
    let auth_entry = if auth_path.exists() {
        let content = std::fs::read_to_string(&auth_path)
            .context("failed to read auth.json")?;
        let auth_map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&content).context("failed to parse auth.json")?;

        auth_map
            .get(&provider)
            .and_then(|v| serde_json::from_value::<AuthEntry>(v.clone()).ok())
    } else {
        None
    };

    let api_key = resolve_api_key(&provider);

    let system_prompt = load_system_prompt(cwd);
    let context_files = collect_context_files(cwd);

    Ok(Config {
        provider,
        model,
        thinking_level,
        api_key,
        auth_entry,
        system_prompt,
        context_files,
    })
}

// per-million-token pricing for anthropic models
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
}

pub fn model_pricing(model: &str) -> ModelPricing {
    let m = model.to_lowercase();
    if m.contains("opus") {
        ModelPricing { input: 15.0, output: 75.0 }
    } else if m.contains("haiku") {
        ModelPricing { input: 0.80, output: 4.0 }
    } else {
        // sonnet and unknown models
        ModelPricing { input: 3.0, output: 15.0 }
    }
}
