mod agent;
mod api;
mod config;
mod markdown;
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

    /// override provider
    #[arg(long)]
    provider: Option<String>,

    /// thinking level: off, minimal, low, medium, high, xhigh
    #[arg(long)]
    thinking: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let cwd = match cli.dir {
        Some(dir) => std::fs::canonicalize(dir)?,
        None => std::env::current_dir()?,
    };

    let mut cfg = config::load_config(&cwd)?;

    if let Some(model) = cli.model {
        cfg.model = model;
    }
    if let Some(provider) = cli.provider {
        cfg.provider = provider;
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

    // build the api client on this thread (before spawn) since
    // AuthEntry contains non-Send types. the client extracts what
    // it needs and is itself Send-safe.
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
                auth_entry: None,
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
    let api_client = api::ApiClient::new(&cfg)?;

    let mut terminal = tui::Tui::new()?;
    let mut app = tui::App::new(&cfg.model, &cwd.to_string_lossy());

    let cancel = agent::CancelToken::new();

    let (user_tx, user_rx) = mpsc::unbounded_channel::<String>();
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (control_tx, control_rx) = mpsc::unbounded_channel::<agent::ControlMessage>();

    let agent_cwd = cwd.clone();
    let agent_cancel = cancel.clone();
    tokio::spawn(async move {
        let agent = agent::Agent::new(&cfg, api_client, agent_cwd, agent_cancel);
        agent_loop(agent, user_rx, control_rx, agent_tx).await;
    });

    if !message_parts.is_empty() {
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
                        if let Some(cmd) = parse_slash_command(&msg) {
                            handle_slash_command(cmd, &mut app, &control_tx);
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
            app.push_system_message("conversation cleared".to_string());
        }
        SlashCommand::Help => {
            let help = "\
available commands:\n\
\n\
  /model [name]       switch model (opus, sonnet, sonnet-4.5, haiku, ...)\n\
  /thinking [level]   set thinking level (off, minimal, low, medium, high, xhigh)\n\
  /new                start a new conversation\n\
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
            lines.push_str("\naliases: opus, sonnet, sonnet-4.5, haiku");
            app.push_system_message(lines);
        }
        Some(pat) => {
            if let Some(model_def) = config::match_model(&pat) {
                app.update_model(model_def.id);
                let _ = control_tx.send(agent::ControlMessage::ChangeModel(
                    model_def.id.to_string(),
                ));
                app.push_system_message(format!(
                    "switched to {} ({})",
                    model_def.id, model_def.name
                ));
            } else {
                let mut msg = format!("no model matching \"{pat}\"");
                msg.push_str("\navailable: ");
                let names: Vec<&str> = config::ANTHROPIC_MODELS.iter().map(|m| m.id).collect();
                msg.push_str(&names.join(", "));
                app.push_system_message(msg);
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
                app.push_system_message(format!("thinking level set to {lvl_lower}"));
            } else {
                app.push_system_message(format!(
                    "unknown thinking level \"{}\"\navailable: {}",
                    lvl,
                    config::THINKING_LEVELS.join(", ")
                ));
            }
        }
    }
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
                    Some(agent::ControlMessage::ClearHistory) => {
                        agent.clear_history();
                    }
                    None => break,
                }
            }
        }
    }
}
