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

    // spawn agent in a task that owns all its data
    tokio::spawn({
        let msg = msg.clone();
        let cfg_model = cfg.model.clone();
        let cfg_thinking = cfg.thinking_level.clone();
        let cfg_system = cfg.system_prompt.clone();
        let cfg_context = cfg.context_files.clone();
        let cfg_provider = cfg.provider.clone();
        let cfg_api_key = cfg.api_key.clone();
        let cfg_auth = cfg.auth_entry.as_ref().map(|e| match e {
            config::AuthEntry::OAuth { access, .. } => access.clone(),
            config::AuthEntry::ApiKey { key } => key.clone(),
        });
        let cwd = cwd.clone();
        let tx = agent_tx.clone();

        async move {
            let key = cfg_auth.or(cfg_api_key.clone());
            let rebuilt_cfg = config::Config {
                provider: cfg_provider,
                model: cfg_model,
                thinking_level: cfg_thinking,
                api_key: key,
                auth_entry: None,
                system_prompt: cfg_system,
                context_files: cfg_context,
            };
            let api_client = match api::ApiClient::new(&rebuilt_cfg) {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(AgentEvent::Error(e.to_string()));
                    let _ = tx.send(AgentEvent::TurnComplete);
                    return;
                }
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

    let mut pm = print::PrintMode::new();
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

    let agent_cwd = cwd.clone();
    let agent_cancel = cancel.clone();
    tokio::spawn(async move {
        let agent = agent::Agent::new(&cfg, api_client, agent_cwd, agent_cancel);
        agent_loop(agent, user_rx, agent_tx).await;
    });

    if !message_parts.is_empty() {
        let msg = message_parts.join(" ");
        app.start_new_message(&msg);
        let _ = user_tx.send(msg);
    }

    loop {
        app.tick_rate();
        terminal.draw(&app)?;

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

        if event::poll(Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                match tui::handle_key_event(key, &mut app) {
                    tui::InputAction::Submit(msg) => {
                        cancel.reset();
                        app.start_new_message(&msg);
                        let _ = user_tx.send(msg);
                    }
                    tui::InputAction::Cancel => {
                        cancel.cancel();
                        app.is_running = false;
                    }
                    tui::InputAction::Quit => break,
                    tui::InputAction::ScrollUp => {
                        app.scroll_offset = app.scroll_offset.saturating_sub(1);
                    }
                    tui::InputAction::ScrollDown => {
                        app.scroll_offset = app.scroll_offset.saturating_add(1);
                    }
                    tui::InputAction::ToggleDiff => {
                        let count = app.tool_diff_count();
                        if count > 0 {
                            app.toggle_diff(count - 1);
                        }
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

async fn agent_loop(
    mut agent: agent::Agent,
    mut user_rx: mpsc::UnboundedReceiver<String>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
) {
    while let Some(message) = user_rx.recv().await {
        let result = agent.send_message(&message, event_tx.clone()).await;
        if let Err(e) = result {
            let _ = event_tx.send(AgentEvent::Error(e.to_string()));
            let _ = event_tx.send(AgentEvent::TurnComplete);
        }
    }
}
