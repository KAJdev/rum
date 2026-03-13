mod agent;
mod api;
mod auth;
mod config;
mod markdown;
mod persistence;
mod print;
mod tools;
mod tui;

use anyhow::{bail, Result};
use clap::Parser;
use crossterm::event::{self, Event};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::agent::AgentEvent;

enum SlashCommand {
    Model(Option<String>),
    Thinking(Option<String>),
    New,
    Login,
    Logout,
    Help,
    Quit,
}

fn parse_slash_command(text: &str) -> Option<SlashCommand> {
    let text = text.trim();
    if !text.starts_with('/') {
        return None;
    }

    let parts: Vec<&str> = text.splitn(2, ' ').collect();
    let cmd = parts[0].to_lowercase();
    let arg = parts.get(1).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    match cmd.as_str() {
        "/model" => Some(SlashCommand::Model(arg)),
        "/thinking" => Some(SlashCommand::Thinking(arg)),
        "/new" => Some(SlashCommand::New),
        "/login" => Some(SlashCommand::Login),
        "/logout" => Some(SlashCommand::Logout),
        "/help" => Some(SlashCommand::Help),
        "/quit" => Some(SlashCommand::Quit),
        _ => None,
    }
}

#[derive(Parser)]
#[command(name = "rum", about = "a diff-centric coding agent TUI")]
struct Cli {
    /// initial message to send
    #[arg(trailing_var_arg = true)]
    message: Vec<String>,

    /// print mode: run without TUI, stream output to stdout, exit when done
    #[arg(short = 'p', long = "print")]
    print: bool,

    /// working directory
    #[arg(short = 'C', long)]
    dir: Option<PathBuf>,

    /// override model
    #[arg(long)]
    model: Option<String>,

    /// thinking level: off, minimal, low, medium, high, xhigh
    #[arg(long)]
    thinking: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // handle login/logout before clap so they don't interfere with
    // the trailing positional message arg
    let raw: Vec<String> = std::env::args().collect();
    match raw.get(1).map(|s| s.as_str()) {
        Some("login") => return run_login_command().await,
        Some("logout") => return run_logout_command(),
        _ => {}
    }

    let cli = Cli::parse();

    let cwd = match cli.dir {
        Some(dir) => std::fs::canonicalize(dir)?,
        None => std::env::current_dir()?,
    };

    let mut cfg = config::load_config(&cwd)?;

    // refresh an expired oauth token before starting so the first request
    // doesn't fail with an auth error
    maybe_refresh_token().await;

    // rum settings (model, thinking, diffs_expanded) persist the user's
    // last interactive choice across sessions. cli flags override them.
    let rum_settings = persistence::load_settings();
    if let Some(model) = rum_settings.model {
        cfg.model = model;
    }
    if let Some(thinking) = rum_settings.thinking_level {
        cfg.thinking_level = thinking;
    }

    if let Some(model) = cli.model {
        cfg.model = model;
    }
    if let Some(thinking) = cli.thinking {
        cfg.thinking_level = thinking;
    }

    if cli.print {
        return run_print_mode(&cfg, &cwd, &cli.message).await;
    }

    run_tui_mode(cfg, cwd, cli.message).await
}

async fn run_print_mode(
    cfg: &config::Config,
    cwd: &PathBuf,
    message_parts: &[String],
) -> Result<()> {
    if message_parts.is_empty() {
        bail!("print mode requires a message. usage: rum -p \"your prompt\"");
    }

    let msg = message_parts.join(" ");

    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<AgentEvent>();

    // build the api client on this thread before spawning; the client
    // extracts credentials and is Send-safe.
    let api_client = api::ApiClient::new(cfg)?;

    tokio::spawn({
        let msg = msg.clone();
        let cfg_model = cfg.model.clone();
        let cfg_thinking = cfg.thinking_level.clone();
        let cfg_system = cfg.system_prompt.clone();
        let cfg_context = cfg.context_files.clone();
        let cfg_provider = cfg.provider.clone();
        let cwd = cwd.clone();
        let tx = agent_tx.clone();

        async move {
            let rebuilt_cfg = config::Config {
                provider: cfg_provider,
                model: cfg_model,
                thinking_level: cfg_thinking,
                api_key: None,
                oauth: None,
                system_prompt: cfg_system,
                context_files: cfg_context,
            };
            let cancel = agent::CancelToken::new();
            let mut agent = agent::Agent::new(&rebuilt_cfg, api_client, cwd, cancel);
            let result = agent.send_message(&msg, tx.clone()).await;
            if let Err(e) = result {
                let _ = tx.send(AgentEvent::Error(e.to_string()));
                let _ = tx.send(AgentEvent::TurnComplete);
            }
        }
    });

    drop(agent_tx);

    let mut pm = print::PrintMode::new(&cfg.model);
    pm.print_header(&cfg.model, &cwd.to_string_lossy(), &msg);

    while let Some(evt) = agent_rx.recv().await {
        if pm.handle_event(evt) {
            break;
        }
    }

    pm.print_summary();

    if pm.has_errors() {
        std::process::exit(1);
    }

    Ok(())
}

async fn run_tui_mode(
    cfg: config::Config,
    cwd: PathBuf,
    message_parts: Vec<String>,
) -> Result<()> {
    let no_credentials = cfg.api_key.is_none() && cfg.oauth.is_none();
    let api_client = api::ApiClient::new(&cfg)?;

    let mut terminal = tui::Tui::new()?;
    let mut app = tui::App::new(&cfg.model, &cfg.thinking_level, &cwd.to_string_lossy());

    // apply persisted diffs_expanded state (saved by the user's last toggle)
    let rum_settings = persistence::load_settings();
    if let Some(expanded) = rum_settings.diffs_expanded {
        app.diffs_expanded = expanded;
    }

    let cancel = agent::CancelToken::new();

    let (user_tx, user_rx) = mpsc::unbounded_channel::<String>();
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (control_tx, control_rx) = mpsc::unbounded_channel::<agent::ControlMessage>();
    let (login_tx, mut login_rx) = mpsc::unbounded_channel::<Result<(), String>>();
    let (update_tx, mut update_rx) = mpsc::unbounded_channel::<String>();

    // holds the pkce verifier while waiting for the user to paste the auth code
    let mut login_pending: Option<String> = None;

    let agent_cwd = cwd.clone();
    let agent_cancel = cancel.clone();

    // construct the agent before spawning so we can read the loaded history length
    let agent = agent::Agent::new(&cfg, api_client, agent_cwd, agent_cancel);
    let history_len = agent.loaded_history_len();
    if history_len > 0 {
        app.push_system_message(format!(
            "resumed previous session ({history_len} messages in context)  /new to start fresh"
        ));
    }

    tokio::spawn(async move {
        agent_loop(agent, user_rx, control_rx, agent_tx).await;
    });

    tokio::spawn(async move {
        if let Some(tag) = check_for_update().await {
            let _ = update_tx.send(tag);
        }
    });

    if no_credentials {
        let (url, verifier) = auth::build_auth_url();
        auth::open_browser(&url);
        login_pending = Some(verifier);
        app.push_system_message(format!(
            "welcome to rum! log in to get started.\n\nopening browser...\nif it didn't open, visit:\n{url}\n\nthen paste the redirect URL here and press enter"
        ));
    } else if !message_parts.is_empty() {
        let msg = message_parts.join(" ");
        app.start_new_message(&msg);
        let _ = user_tx.send(msg);
    }

    loop {
        app.tick_rate();
        terminal.draw(&mut app)?;

        loop {
            match agent_rx.try_recv() {
                Ok(evt) => {
                    app.handle_agent_event(evt);
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    app.is_running = false;
                    break;
                }
            }
        }

        // drain completed login attempts
        while let Ok(result) = login_rx.try_recv() {
            match result {
                Ok(()) => {
                    app.push_success("logged in! start chatting below.".to_string());
                    if let Some(creds) = auth::load_auth() {
                        let _ = control_tx.send(agent::ControlMessage::UpdateAuth(creds.access));
                    }
                }
                Err(e) => app.push_error_msg(format!("login failed: {e}")),
            }
        }

        // check for available updates
        while let Ok(tag) = update_rx.try_recv() {
            app.push_update_notice(format!(
                "rum {} is available  (you have {})  cargo binstall rum",
                tag,
                env!("CARGO_PKG_VERSION"),
            ));
        }

        // send queued follow-up messages when the current turn finishes
        if !app.is_running && app.has_queued_messages() {
            cancel.reset();
            let combined = app.flush_queued_messages();
            let _ = user_tx.send(combined);
        }

        if event::poll(Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                match tui::handle_key_event(key, &mut app) {
                    tui::InputAction::Submit(msg) => {
                        if let Some(verifier) = login_pending.take() {
                            // the user pasted the auth code from the browser
                            handle_login_code(&msg, verifier, &mut app, login_tx.clone());
                        } else if let Some(cmd) = parse_slash_command(&msg) {
                            handle_slash_command(cmd, &mut app, &control_tx, &mut login_pending);
                            if !app.should_quit {
                                // persist any settings that may have changed
                                let _ = persistence::save_settings(&persistence::RumSettings {
                                    model: Some(app.model_name().to_string()),
                                    thinking_level: Some(app.thinking_level().to_string()),
                                    diffs_expanded: Some(app.diffs_expanded),
                                });
                            }
                            if app.should_quit {
                                break;
                            }
                        } else {
                            cancel.reset();
                            app.start_new_message(&msg);
                            let _ = user_tx.send(msg);
                        }
                    }
                    tui::InputAction::Cancel => {
                        cancel.cancel();
                        app.is_running = false;
                        app.clear_queue();
                    }
                    tui::InputAction::Quit => break,
                    tui::InputAction::ScrollUp => {
                        app.auto_scroll = false;
                        app.scroll_offset = app.scroll_offset.saturating_sub(1);
                    }
                    tui::InputAction::ScrollDown => {
                        app.scroll_offset = app.scroll_offset.saturating_add(1);
                    }
                    tui::InputAction::ToggleDiff => {
                        app.toggle_diff();
                        let _ = persistence::save_settings(&persistence::RumSettings {
                            model: Some(app.model_name().to_string()),
                            thinking_level: Some(app.thinking_level().to_string()),
                            diffs_expanded: Some(app.diffs_expanded),
                        });
                    }
                    tui::InputAction::None => {}
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    terminal.restore()?;
    Ok(())
}

fn handle_slash_command(
    cmd: SlashCommand,
    app: &mut tui::App,
    control_tx: &mpsc::UnboundedSender<agent::ControlMessage>,
    login_pending: &mut Option<String>,
) {
    match cmd {
        SlashCommand::Model(pattern) => {
            handle_model_command(pattern, app, control_tx);
        }
        SlashCommand::Thinking(level) => {
            handle_thinking_command(level, app, control_tx);
        }
        SlashCommand::New => {
            app.reset_session();
            let _ = control_tx.send(agent::ControlMessage::ClearHistory);
            app.push_success("conversation cleared".to_string());
        }
        SlashCommand::Login => {
            let (url, verifier) = auth::build_auth_url();
            auth::open_browser(&url);
            *login_pending = Some(verifier);
            app.push_system_message(format!(
                "opening browser for anthropic login...\n\nif the browser didn't open, visit:\n{url}\n\nthen paste the redirect URL (or code#state) here and press enter"
            ));
        }
        SlashCommand::Logout => {
            match auth::delete_auth() {
                Ok(()) => app.push_warning(
                    "logged out. set ANTHROPIC_API_KEY or run /login to re-authenticate.".to_string(),
                ),
                Err(e) => app.push_error_msg(format!("logout failed: {e}")),
            }
        }
        SlashCommand::Help => {
            let help = "\
available commands:\n\
\n\
  /model [name]       switch model (opus, sonnet, haiku, opus-4.5, ...)\n\
  /thinking [level]   set thinking level (off, minimal, low, medium, high, xhigh)\n\
  /new                start a new conversation\n\
  /login              log in with anthropic oauth\n\
  /logout             log out\n\
  /help               show this help\n\
  /quit               quit rum";
            app.push_system_message(help.to_string());
        }
        SlashCommand::Quit => {
            app.should_quit = true;
        }
    }
}

fn handle_model_command(
    pattern: Option<String>,
    app: &mut tui::App,
    control_tx: &mpsc::UnboundedSender<agent::ControlMessage>,
) {
    let current = app.model_name().to_string();

    match pattern {
        None => {
            let mut lines = String::from("available models:\n");
            for m in config::ANTHROPIC_MODELS {
                let marker = if m.id == current { "\u{2192}" } else { " " };
                lines.push_str(&format!(
                    "\n {} {}  ({}  ${:.2}/${:.2} per 1M tok)",
                    marker, m.id, m.name, m.input_price, m.output_price
                ));
            }
            lines.push_str("\n\nusage: /model <name>");
            lines.push_str("\naliases: opus, sonnet, haiku, opus-4.5, sonnet-4.5, ...");
            app.push_system_message(lines);
        }
        Some(pat) => {
            if let Some(model_def) = config::match_model(&pat) {
                app.update_model(model_def.id);
                let _ = control_tx.send(agent::ControlMessage::ChangeModel(
                    model_def.id.to_string(),
                ));
                app.push_success(format!("switched to {} ({})", model_def.id, model_def.name));
            } else {
                let mut msg = format!("no model matching \"{pat}\"");
                msg.push_str("\navailable: ");
                let names: Vec<&str> = config::ANTHROPIC_MODELS.iter().map(|m| m.id).collect();
                msg.push_str(&names.join(", "));
                app.push_warning(msg);
            }
        }
    }
}

fn handle_thinking_command(
    level: Option<String>,
    app: &mut tui::App,
    control_tx: &mpsc::UnboundedSender<agent::ControlMessage>,
) {
    match level {
        None => {
            let msg = format!(
                "thinking levels: {}\nusage: /thinking <level>",
                config::THINKING_LEVELS.join(", ")
            );
            app.push_system_message(msg);
        }
        Some(lvl) => {
            let lvl_lower = lvl.to_lowercase();
            if config::THINKING_LEVELS.contains(&lvl_lower.as_str()) {
                let _ = control_tx.send(agent::ControlMessage::ChangeThinking(
                    lvl_lower.clone(),
                ));
                app.update_thinking(&lvl_lower);
                app.push_success(format!("thinking level set to {lvl_lower}"));
            } else {
                app.push_warning(format!(
                    "unknown thinking level \"{}\"\navailable: {}",
                    lvl,
                    config::THINKING_LEVELS.join(", ")
                ));
            }
        }
    }
}

// called in the tui loop after the user pastes the auth code
fn handle_login_code(
    input: &str,
    verifier: String,
    app: &mut tui::App,
    login_tx: mpsc::UnboundedSender<Result<(), String>>,
) {
    match auth::parse_auth_response(input) {
        Some((code, state)) => {
            app.push_system_message("authenticating...".to_string()); // info/spinner feel
            tokio::spawn(async move {
                let result = auth::exchange_code(&code, &state, &verifier)
                    .await
                    .and_then(|creds| auth::save_auth(&creds))
                    .map_err(|e| e.to_string());
                let _ = login_tx.send(result);
            });
        }
        None => {
            app.push_warning(
                "could not parse auth code — expected CODE#STATE or the full redirect URL. try /login again".to_string(),
            );
        }
    }
}

// refreshes the stored oauth token if it is expired
async fn maybe_refresh_token() {
    let Some(creds) = auth::load_auth() else { return };
    if !auth::is_expired(&creds) {
        return;
    }
    if let Ok(new_creds) = auth::refresh(&creds.refresh).await {
        let _ = auth::save_auth(&new_creds);
    }
}

// standalone `rum login` command
async fn run_login_command() -> Result<()> {
    let (url, verifier) = auth::build_auth_url();

    println!("opening browser for anthropic login...");
    auth::open_browser(&url);
    println!("\nif the browser didn't open, visit:\n{url}\n");
    println!("after authenticating, paste the redirect URL (or code#state) and press enter:");

    let mut input = String::new();
    tokio::io::AsyncBufReadExt::read_line(
        &mut tokio::io::BufReader::new(tokio::io::stdin()),
        &mut input,
    )
    .await?;

    let (code, state) = auth::parse_auth_response(input.trim())
        .ok_or_else(|| anyhow::anyhow!("could not parse auth response. expected CODE#STATE or the full redirect URL"))?;

    println!("exchanging code for token...");
    let creds = auth::exchange_code(&code, &state, &verifier).await?;
    auth::save_auth(&creds)?;
    println!("logged in successfully");
    Ok(())
}

// standalone `rum logout` command
fn run_logout_command() -> Result<()> {
    auth::delete_auth()?;
    println!("logged out");
    Ok(())
}

async fn check_for_update() -> Option<String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("rum/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;

    let resp = client
        .get("https://api.github.com/repos/KAJdev/rum/releases/latest")
        .send()
        .await
        .ok()?;

    let json: serde_json::Value = resp.json().await.ok()?;
    let tag = json.get("tag_name")?.as_str()?;
    let latest = tag.trim_start_matches('v');
    let current = env!("CARGO_PKG_VERSION");

    if parse_semver(latest) > parse_semver(current) {
        Some(tag.to_string())
    } else {
        None
    }
}

fn parse_semver(v: &str) -> (u32, u32, u32) {
    let parts: Vec<u32> = v.split('.').filter_map(|p| p.parse().ok()).collect();
    (
        parts.first().copied().unwrap_or(0),
        parts.get(1).copied().unwrap_or(0),
        parts.get(2).copied().unwrap_or(0),
    )
}

async fn agent_loop(
    mut agent: agent::Agent,
    mut user_rx: mpsc::UnboundedReceiver<String>,
    mut control_rx: mpsc::UnboundedReceiver<agent::ControlMessage>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
) {
    loop {
        tokio::select! {
            msg = user_rx.recv() => {
                match msg {
                    Some(message) => {
                        let result = agent.send_message(&message, event_tx.clone()).await;
                        if let Err(e) = result {
                            let _ = event_tx.send(AgentEvent::Error(e.to_string()));
                            let _ = event_tx.send(AgentEvent::TurnComplete);
                        }
                    }
                    None => break,
                }
            }
            ctrl = control_rx.recv() => {
                match ctrl {
                    Some(agent::ControlMessage::ChangeModel(model)) => {
                        agent.set_model(&model);
                    }
                    Some(agent::ControlMessage::ChangeThinking(level)) => {
                        agent.set_thinking(&level);
                    }
                    Some(agent::ControlMessage::UpdateAuth(token)) => {
                        agent.set_auth_token(token);
                    }
                    Some(agent::ControlMessage::ClearHistory) => {
                        agent.clear_history();
                    }
                    None => break,
                }
            }
        }
    }
}
