mod agent;
mod api;
mod auth;
mod autocomplete;
mod config;
mod editor;
mod markdown;
mod persistence;
mod print;
mod tools;
mod tree;
mod tui;

use anyhow::{bail, Result};
use clap::Parser;
use crossterm::event::{self, Event, MouseEventKind};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::agent::AgentEvent;

enum SlashCommand {
    Model(Option<String>),
    Thinking(Option<String>),
    New,
    Compact,
    Cd(String),
    Login,
    Logout,
    Help,
    Quit,
    Tree,
}

fn parse_slash_command(text: &str) -> Option<SlashCommand> {
    let text = text.trim();
    if !text.starts_with('/') {
        return None;
    }

    let parts: Vec<&str> = text.splitn(2, ' ').collect();
    let cmd = parts[0].to_lowercase();
    let arg = parts
        .get(1)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    match cmd.as_str() {
        "/model" => Some(SlashCommand::Model(arg)),
        "/thinking" => Some(SlashCommand::Thinking(arg)),
        "/new" => Some(SlashCommand::New),
        "/compact" => Some(SlashCommand::Compact),
        "/cd" => Some(SlashCommand::Cd(arg.unwrap_or_default())),
        "/login" => Some(SlashCommand::Login),
        "/logout" => Some(SlashCommand::Logout),
        "/help" => Some(SlashCommand::Help),
        "/quit" => Some(SlashCommand::Quit),
        "/tree" => Some(SlashCommand::Tree),
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
                loaded_sources: vec![],
            };
            let cancel = agent::CancelToken::new();
            let mut agent = agent::Agent::new(&rebuilt_cfg, api_client, cwd, cancel);
            let (_inject_tx, inject_rx) = mpsc::unbounded_channel::<String>();
            let inject_rx = std::sync::Mutex::new(inject_rx);
            let result = agent.send_message(&msg, &inject_rx, tx.clone()).await;
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

async fn run_tui_mode(cfg: config::Config, cwd: PathBuf, message_parts: Vec<String>) -> Result<()> {
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
    let (inject_tx, inject_rx) = mpsc::unbounded_channel::<String>();
    app.set_inject_tx(inject_tx);
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (control_tx, control_rx) = mpsc::unbounded_channel::<agent::ControlMessage>();
    let (login_tx, mut login_rx) = mpsc::unbounded_channel::<Result<(), String>>();
    let (update_tx, mut update_rx) = mpsc::unbounded_channel::<String>();
    let (job_tx, mut job_rx) = mpsc::unbounded_channel::<tui::JobEvent>();

    // holds the pkce verifier while waiting for the user to paste the auth code
    let mut login_pending: Option<String> = None;

    let agent_cwd = cwd.clone();
    let agent_cancel = cancel.clone();

    // construct the agent before spawning so we can read the loaded history length
    let session_tree = crate::persistence::load_session(&agent_cwd);
    let mut agent = agent::Agent::new(&cfg, api_client, agent_cwd, agent_cancel);
    agent.job_tx = Some(job_tx.clone());
    agent.next_job_id = app.next_job_id.clone();
    app.session_tree = session_tree;
    let history_len = agent.loaded_history_len();

    // show startup info: loaded config files and session state
    if !cfg.loaded_sources.is_empty() {
        let home = dirs::home_dir().unwrap_or_default();
        let sources: Vec<String> = cfg
            .loaded_sources
            .iter()
            .map(|s| {
                // shorten home dir to ~ for readability
                match s.strip_prefix(&home.to_string_lossy().as_ref()) {
                    Some(rest) => format!("~{rest}"),
                    None => s.clone(),
                }
            })
            .collect();
        app.push_system_message(format!("loaded: {}", sources.join(", ")));
    }
    if history_len > 0 {
        app.hydrate_from_history(agent.messages());
        app.push_system_message(format!(
            "resumed session ({history_len} messages in context)  /new to start fresh"
        ));
    }

    tokio::spawn(async move {
        agent_loop(agent, user_rx, inject_rx, control_rx, agent_tx).await;
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
        app.mark_login_start();
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

        // spawn CI watch when a git push is detected
        if let Some(cwd) = app.pending_ci_watch.take() {
            let tx = job_tx.clone();
            let job_id =
                app.start_background_job("CI".to_string(), "waiting for runs…".to_string());
            tokio::spawn(async move {
                ci_watch(job_id, &cwd, tx).await;
            });
        }

        // drain background job events
        while let Ok(evt) = job_rx.try_recv() {
            // detect completions that should be sent to the agent:
            // - CI failures with logs (contain "---")
            // - background bash completions (contain "background `")
            let trigger_msg = match &evt {
                tui::JobEvent::Complete { summary, .. }
                    if summary.contains("---") || summary.starts_with("background `") =>
                {
                    Some(summary.clone())
                }
                _ => None,
            };
            // always show the job event in the activity feed
            app.handle_job_event(evt);
            // send the output to the agent
            if let Some(msg) = trigger_msg {
                if !app.is_running {
                    cancel.reset();
                    let label = if msg.starts_with("background") {
                        "[background command finished]"
                    } else {
                        "[CI failed]"
                    };
                    app.start_new_message(label);
                    let _ = user_tx.send(msg);
                } else {
                    app.queue_message_str(msg);
                }
            }
        }
        app.gc_background_jobs(std::time::Duration::from_secs(15));

        // drain completed login attempts
        while let Ok(result) = login_rx.try_recv() {
            match result {
                Ok(()) => {
                    app.finish_login("logged in! start chatting below.".to_string(), true);
                    if let Some(creds) = auth::load_auth() {
                        let _ = control_tx.send(agent::ControlMessage::UpdateAuth(creds.access));
                    }
                }
                Err(e) => app.finish_login(format!("login failed: {e}"), false),
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

        // process queued items when the current turn finishes
        if !app.is_running && app.has_queued_items() {
            match app.drain_next_queued() {
                Some(tui::QueuedAction::SendMessage(combined)) => {
                    cancel.reset();
                    let _ = user_tx.send(combined);
                }
                Some(tui::QueuedAction::RunCommand(cmd)) => match cmd.as_str() {
                    "/compact" => {
                        cancel.reset();
                        app.current_message = Some("/compact".to_string());
                        app.is_running = true;
                        let _ = control_tx.send(agent::ControlMessage::Compact);
                    }
                    _ => {}
                },
                None => {}
            }
        }

        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) => {
                    // tree view intercepts all key events
                    if app.tree_view.is_some() {
                        handle_tree_key(key, &mut app, &control_tx);
                        continue;
                    }
                    match tui::handle_key_event(key, &mut app) {
                        tui::InputAction::Submit(msg) => {
                            if let Some(verifier) = login_pending.take() {
                                // the user pasted the auth code from the browser
                                handle_login_code(&msg, verifier, &mut app, login_tx.clone());
                            } else if let Some(cmd) = parse_slash_command(&msg) {
                                handle_slash_command(
                                    cmd,
                                    &mut app,
                                    &control_tx,
                                    &mut login_pending,
                                );
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
                            } else if msg.starts_with('!') {
                                let cmd = msg[1..].trim().to_string();
                                if !cmd.is_empty() {
                                    cancel.reset();
                                    app.push_history(&msg);
                                    app.push_user_message(&msg);
                                    app.is_running = true;
                                    let _ = control_tx.send(agent::ControlMessage::UserBash(cmd));
                                }
                            } else {
                                cancel.reset();
                                app.push_history(&msg);
                                app.start_new_message(&msg);
                                let _ = user_tx.send(msg);
                            }
                        }
                        tui::InputAction::Cancel => {
                            cancel.cancel();
                            app.cancel_running();
                            // restore the last queued message to input, then drop the rest
                            app.pop_queued_message();
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
                        tui::InputAction::PasteFromClipboard => {
                            if let Some(img_path) = try_read_clipboard_image() {
                                app.insert_text(img_path);
                                app.paste_handled = true;
                            }
                            // if no image is on clipboard, bracketed paste will fire separately
                        }
                        tui::InputAction::None => {}
                    }
                }
                Event::Paste(text) => {
                    if app.paste_handled {
                        app.paste_handled = false;
                        // skip: PasteFromClipboard already processed this
                    } else if text.is_empty() || paste_looks_like_binary(&text) {
                        if let Some(img_path) = try_read_clipboard_image() {
                            app.insert_text(img_path);
                        } else if !text.is_empty() {
                            app.insert_paste(text);
                        }
                    } else if let Some(path) = resolve_pasted_path(&text, &cwd) {
                        app.insert_text(path);
                    } else {
                        app.insert_paste(text);
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        if app.view_mode == tui::ViewMode::Editor {
                            if let Some(ref mut buf) = app.editor_buffer {
                                for _ in 0..3 {
                                    buf.move_up();
                                }
                                let h = crossterm::terminal::size()
                                    .map(|(_, h)| h)
                                    .unwrap_or(24) as usize;
                                buf.ensure_cursor_visible(h.saturating_sub(2));
                            }
                        } else {
                            app.auto_scroll = false;
                            app.scroll_offset = app.scroll_offset.saturating_sub(3);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if app.view_mode == tui::ViewMode::Editor {
                            if let Some(ref mut buf) = app.editor_buffer {
                                for _ in 0..3 {
                                    buf.move_down();
                                }
                                let h = crossterm::terminal::size()
                                    .map(|(_, h)| h)
                                    .unwrap_or(24) as usize;
                                buf.ensure_cursor_visible(h.saturating_sub(2));
                            }
                        } else {
                            app.scroll_offset = app.scroll_offset.saturating_add(3);
                        }
                    }
                    MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                        if app.view_mode == tui::ViewMode::Editor {
                            if let Some(ref mut buf) = app.editor_buffer {
                                let size = crossterm::terminal::size().unwrap_or((80, 24));
                                let sidebar_w: u16 = 30;
                                let gutter_w = (buf.line_count().max(1).to_string().len() + 2) as u16;
                                // status bar is 1 row at top
                                let editor_y_start: u16 = 1;
                                let editor_x_end = size.0.saturating_sub(sidebar_w);
                                let content_cols = (editor_x_end as usize).saturating_sub(gutter_w as usize);

                                if mouse.column >= gutter_w
                                    && mouse.column < editor_x_end
                                    && mouse.row >= editor_y_start
                                {
                                    let click_screen_row = (mouse.row - editor_y_start) as usize;
                                    let click_col = (mouse.column - gutter_w) as usize;

                                    // map screen row to file line, accounting for wrapping
                                    let mut screen_row = 0usize;
                                    let mut target_line = buf.scroll_row;
                                    let mut wrap_offset = 0usize;
                                    for i in buf.scroll_row..buf.lines.len() {
                                        let lw = unicode_width::UnicodeWidthStr::width(buf.lines[i].as_str());
                                        let rows = if lw == 0 || content_cols == 0 { 1 } else { (lw + content_cols - 1) / content_cols };
                                        if click_screen_row < screen_row + rows {
                                            target_line = i;
                                            wrap_offset = click_screen_row - screen_row;
                                            break;
                                        }
                                        screen_row += rows;
                                        if screen_row > click_screen_row {
                                            break;
                                        }
                                        target_line = i + 1;
                                    }

                                    if target_line < buf.lines.len() {
                                        buf.cursor_row = target_line;
                                        // convert visual column to byte offset
                                        let visual_target = wrap_offset * content_cols + click_col;
                                        let line = &buf.lines[target_line];
                                        let mut byte_off = 0usize;
                                        let mut vis_off = 0usize;
                                        for ch in line.chars() {
                                            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                                            if vis_off + w > visual_target {
                                                break;
                                            }
                                            vis_off += w;
                                            byte_off += ch.len_utf8();
                                        }
                                        buf.cursor_col = byte_off;
                                        buf.desired_col = None;
                                    }
                                    app.editor_autocomplete = None;
                                }
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
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
            app.session_tree = crate::persistence::SessionTree::new();
            let _ = control_tx.send(agent::ControlMessage::ClearHistory);
            app.push_success("conversation cleared".to_string());
        }
        SlashCommand::Compact => {
            if app.is_running {
                app.queue_command("/compact");
            } else {
                app.current_message = Some("/compact".to_string());
                app.is_running = true;
                let _ = control_tx.send(agent::ControlMessage::Compact);
            }
        }
        SlashCommand::Cd(path) => {
            if path.is_empty() {
                app.push_error_msg("usage: /cd <path>".to_string());
            } else {
                let base = std::path::Path::new(app.cwd());
                let target = if std::path::Path::new(&path).is_absolute() {
                    std::path::PathBuf::from(&path)
                } else {
                    base.join(&path)
                };
                match std::fs::canonicalize(&target) {
                    Ok(resolved) => {
                        let resolved_str = resolved.to_string_lossy().to_string();
                        app.update_cwd(&resolved_str);
                        let _ = control_tx.send(agent::ControlMessage::ChangeDir(resolved));
                        app.push_success(format!("changed directory to {}", resolved_str));
                    }
                    Err(e) => {
                        app.push_error_msg(format!("cd: {}: {}", path, e));
                    }
                }
            }
        }
        SlashCommand::Login => {
            let (url, verifier) = auth::build_auth_url();
            auth::open_browser(&url);
            *login_pending = Some(verifier);
            app.mark_login_start();
            app.push_system_message(format!(
                "opening browser for anthropic login...\n\nif the browser didn't open, visit:\n{url}\n\nthen paste the redirect URL (or code#state) here and press enter"
            ));
        }
        SlashCommand::Logout => match auth::delete_auth() {
            Ok(()) => app.push_warning(
                "logged out. set ANTHROPIC_API_KEY or run /login to re-authenticate.".to_string(),
            ),
            Err(e) => app.push_error_msg(format!("logout failed: {e}")),
        },
        SlashCommand::Help => {
            let help = "\
available commands:\n\
\n\
  /model [name]       switch model (opus, sonnet, haiku, opus-4.5, ...)\n\
  /thinking [level]   set thinking level (off, minimal, low, medium, high, xhigh)\n\
  /new                start a new conversation\n\
  /compact            summarize conversation history to free up context\n\
  /tree               view and branch the conversation tree\n\
  /cd <path>          change working directory\n\
  /login              log in with anthropic oauth\n\
  /logout             log out\n\
  /help               show this help\n\
  /quit               quit rum";
            app.push_system_message(help.to_string());
        }
        SlashCommand::Quit => {
            app.should_quit = true;
        }
        SlashCommand::Tree => {
            let tv = crate::tree::TreeView::build(&app.session_tree);
            app.tree_view = Some(tv);
        }
    }
}

fn handle_tree_key(
    key: crossterm::event::KeyEvent,
    app: &mut tui::App,
    control_tx: &mpsc::UnboundedSender<agent::ControlMessage>,
) {
    use crossterm::event::{KeyCode, KeyModifiers};
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    let tv = match app.tree_view.as_mut() {
        Some(tv) => tv,
        None => return,
    };

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.tree_view = None;
        }
        KeyCode::Up if shift => {
            tv.jump_prev_user();
        }
        KeyCode::Down if shift => {
            tv.jump_next_user();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            tv.move_up();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            tv.move_down();
        }
        // enter: fork a new branch from the selected node
        KeyCode::Enter => {
            if let Some((branch_idx, msg_idx)) = tv.selected() {
                let new_branch = app.session_tree.fork(branch_idx, msg_idx);
                let messages = app.session_tree.active_messages().to_vec();
                let _ = control_tx.send(agent::ControlMessage::SwitchBranch(messages.clone()));
                let _ = crate::persistence::save_session(
                    std::path::Path::new(app.cwd()),
                    &app.session_tree,
                );
                // rehydrate activity feed from new branch
                app.reset_session();
                app.hydrate_from_history(&messages);
                app.push_system_message(format!(
                    "forked new branch {} from branch {} at message {}",
                    new_branch, branch_idx, msg_idx
                ));
                app.tree_view = None;
            }
        }
        // space or tab: switch to the branch at the selected node
        KeyCode::Char(' ') | KeyCode::Tab => {
            if let Some(branch_idx) = tv.selected_branch() {
                if branch_idx != app.session_tree.active {
                    app.session_tree.switch(branch_idx);
                    let messages = app.session_tree.active_messages().to_vec();
                    let _ = control_tx.send(agent::ControlMessage::SwitchBranch(messages.clone()));
                    let _ = crate::persistence::save_session(
                        std::path::Path::new(app.cwd()),
                        &app.session_tree,
                    );
                    app.reset_session();
                    app.hydrate_from_history(&messages);
                    app.push_system_message(format!(
                        "switched to branch {} ({} messages)",
                        branch_idx,
                        app.session_tree.branches[branch_idx].messages.len()
                    ));
                    app.tree_view = None;
                }
            }
        }
        _ => {}
    }

    // keep cursor in view
    if let Some(tv) = app.tree_view.as_mut() {
        let h = crossterm::terminal::size()
            .map(|(_, h)| h)
            .unwrap_or(24) as usize;
        tv.ensure_visible(h.saturating_sub(2));
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
                let _ =
                    control_tx.send(agent::ControlMessage::ChangeModel(model_def.id.to_string()));
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
                let _ = control_tx.send(agent::ControlMessage::ChangeThinking(lvl_lower.clone()));
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

async fn maybe_refresh_token() {
    auth::maybe_refresh().await;
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

    let (code, state) = auth::parse_auth_response(input.trim()).ok_or_else(|| {
        anyhow::anyhow!(
            "could not parse auth response. expected CODE#STATE or the full redirect URL"
        )
    })?;

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

// if the pasted text is a file path (possibly as a file:// URL from drag-and-drop),
// return the resolved filesystem path. handles file:// URLs, percent-encoding,
// shell-style quote wrapping, and backslash-escaped spaces.
fn resolve_pasted_path(text: &str, cwd: &std::path::Path) -> Option<String> {
    let text = text.trim();

    // must be single-line
    if text.contains('\n') {
        return None;
    }

    // strip surrounding shell quotes that some terminals add
    let text = if (text.starts_with('\'') && text.ends_with('\''))
        || (text.starts_with('"') && text.ends_with('"'))
    {
        &text[1..text.len() - 1]
    } else {
        text
    };

    // strip file:// URL prefix (file:///path or file://hostname/path)
    let path_str = if let Some(rest) = text.strip_prefix("file://") {
        if let Some(stripped) = rest.strip_prefix('/') {
            // file:///path → /path
            format!("/{stripped}")
        } else {
            // file://hostname/path → /path
            rest.split_once('/')
                .map(|(_, p)| format!("/{p}"))?
                .to_string()
        }
    } else {
        text.to_string()
    };

    // decode percent-encoded characters (%20 → space, etc.)
    let path_str = percent_decode(&path_str);

    // unescape backslash sequences (\ followed by space, etc.)
    let path_str = unescape_backslashes(&path_str);

    let path_str = path_str.trim_end_matches('/');
    if path_str.is_empty() {
        return None;
    }

    // expand leading ~ to home directory
    let path_str: std::borrow::Cow<str> = if let Some(rest) = path_str.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(rest).to_string_lossy().into_owned().into())
            .unwrap_or(path_str.into())
    } else {
        path_str.into()
    };

    let path = std::path::Path::new(path_str.as_ref());
    if path.is_absolute() && path.exists() {
        return Some(path_str.into_owned());
    }

    // try relative to cwd
    let full = cwd.join(path);
    if full.exists() {
        return Some(full.to_string_lossy().into_owned());
    }

    None
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte as char);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn unescape_backslashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                out.push(next);
                chars.next();
                continue;
            }
        }
        out.push(c);
    }
    out
}

// returns true if the text appears to be binary data passed through a lossy UTF-8 decode.
// terminals that forward raw clipboard bytes produce replacement chars for non-UTF-8 sequences.
fn paste_looks_like_binary(text: &str) -> bool {
    let sample: Vec<char> = text.chars().take(200).collect();
    if sample.len() < 4 {
        return false;
    }
    let replacement_count = sample.iter().filter(|&&c| c == '\u{FFFD}').count();
    replacement_count * 4 > sample.len()
}

// tries to save clipboard image data to a temp file and returns the path.
// on macOS uses pngpaste (if available) then osascript.
// on linux uses wl-paste (wayland) then xclip (x11).
fn try_read_clipboard_image() -> Option<String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let path = format!("/tmp/rum_img_{ts}.png");

    if read_clipboard_image_to_path(&path) {
        Some(path)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn read_clipboard_image_to_path(path: &str) -> bool {
    // try pngpaste first (handles PNG, TIFF, JPEG, etc. automatically)
    if let Ok(status) = std::process::Command::new("pngpaste")
        .arg(path)
        .stderr(std::process::Stdio::null())
        .status()
    {
        if status.success() {
            return true;
        }
    }

    // try PNG directly via osascript
    let script = format!(
        "set d to the clipboard as «class PNGf»\n\
         set f to open for access POSIX file \"{path}\" with write permission\n\
         write d to f\n\
         close access f"
    );
    if matches!(
        std::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .stderr(std::process::Stdio::null())
            .status(),
        Ok(s) if s.success()
    ) {
        return true;
    }

    // screenshots are stored as TIFF on the clipboard; write TIFF then convert with sips
    let tiff_path = format!("{path}.tiff");
    let script = format!(
        "set d to the clipboard as «class TIFF»\n\
         set f to open for access POSIX file \"{tiff_path}\" with write permission\n\
         write d to f\n\
         close access f"
    );
    if matches!(
        std::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .stderr(std::process::Stdio::null())
            .status(),
        Ok(s) if s.success()
    ) {
        let converted = std::process::Command::new("sips")
            .args(["-s", "format", "png", &tiff_path, "--out", path])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let _ = std::fs::remove_file(&tiff_path);
        return converted;
    }

    false
}

#[cfg(target_os = "linux")]
fn read_clipboard_image_to_path(path: &str) -> bool {
    // try wl-paste (wayland)
    if let Ok(out) = std::process::Command::new("wl-paste")
        .args(["--type", "image/png", "--no-newline"])
        .stderr(std::process::Stdio::null())
        .output()
    {
        if out.status.success() && !out.stdout.is_empty() {
            if std::fs::write(path, &out.stdout).is_ok() {
                return true;
            }
        }
    }

    // fall back to xclip (x11)
    if let Ok(out) = std::process::Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "image/png", "-o"])
        .stderr(std::process::Stdio::null())
        .output()
    {
        if out.status.success() && !out.stdout.is_empty() {
            if std::fs::write(path, &out.stdout).is_ok() {
                return true;
            }
        }
    }

    false
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read_clipboard_image_to_path(_path: &str) -> bool {
    false
}

async fn agent_loop(
    mut agent: agent::Agent,
    mut user_rx: mpsc::UnboundedReceiver<String>,
    inject_rx: mpsc::UnboundedReceiver<String>,
    mut control_rx: mpsc::UnboundedReceiver<agent::ControlMessage>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
) {
    // wrap in a mutex so run_turn can borrow it without holding a mutable
    // ref to the receiver across await points
    let inject_rx = std::sync::Mutex::new(inject_rx);

    loop {
        tokio::select! {
            msg = user_rx.recv() => {
                match msg {
                    Some(message) => {
                        let result = agent.send_message(&message, &inject_rx, event_tx.clone()).await;
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
                    Some(agent::ControlMessage::ChangeDir(path)) => {
                        agent.set_cwd(path);
                    }
                    Some(agent::ControlMessage::Compact) => {
                        let result = agent.compact_context(event_tx.clone()).await;
                        if let Err(e) = result {
                            let _ = event_tx.send(AgentEvent::Error(e.to_string()));
                            let _ = event_tx.send(AgentEvent::TurnComplete);
                        }
                    }
                    Some(agent::ControlMessage::UserBash(cmd)) => {
                        agent.run_user_bash(&cmd, event_tx.clone()).await;
                    }
                    Some(agent::ControlMessage::SwitchBranch(messages)) => {
                        agent.set_messages(messages);
                    }
                    None => break,
                }
            }
        }
    }
}

async fn ci_watch(job_id: u64, cwd: &str, tx: mpsc::UnboundedSender<tui::JobEvent>) {
    // get the current commit sha and branch
    let sha = match tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .await
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => {
            let _ = tx.send(tui::JobEvent::Dismiss { id: job_id });
            return;
        }
    };

    let branch = tokio::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let branch_label = if branch.is_empty() {
        sha[..7].to_string()
    } else {
        branch.clone()
    };

    // wait for runs to appear (github can take a few seconds)
    let mut runs_found = false;
    for attempt in 0..60 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }

        let output = match tokio::process::Command::new("gh")
            .args([
                "run",
                "list",
                "--commit",
                &sha,
                "--json",
                "status,conclusion,name,url,databaseId",
                "--limit",
                "20",
            ])
            .current_dir(cwd)
            .output()
            .await
        {
            Ok(o) if o.status.success() => o,
            Ok(_) | Err(_) => {
                if attempt == 0 {
                    // gh not available or not authed, silently bail
                    let _ = tx.send(tui::JobEvent::Dismiss { id: job_id });
                    return;
                }
                continue;
            }
        };

        let runs: Vec<serde_json::Value> = match serde_json::from_slice(&output.stdout) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if runs.is_empty() {
            if attempt < 6 {
                // keep waiting for runs to appear in the first ~60s
                continue;
            }
            // no runs after waiting, silently dismiss
            let _ = tx.send(tui::JobEvent::Dismiss { id: job_id });
            return;
        }

        // runs detected, make the job visible
        if !runs_found {
            runs_found = true;
            let _ = tx.send(tui::JobEvent::Show { id: job_id });
        }

        let total = runs.len();
        let completed = runs
            .iter()
            .filter(|r| r.get("status").and_then(|s| s.as_str()) == Some("completed"))
            .count();
        let failed = runs
            .iter()
            .filter(|r| {
                r.get("conclusion")
                    .and_then(|s| s.as_str())
                    .map_or(false, |c| {
                        c == "failure" || c == "cancelled" || c == "timed_out"
                    })
            })
            .count();

        let _ = tx.send(tui::JobEvent::Update {
            id: job_id,
            detail: format!("{}/{} on {}", completed, total, branch_label),
        });

        if completed == total {
            if failed == 0 {
                let _ = tx.send(tui::JobEvent::Complete {
                    id: job_id,
                    status: tui::JobStatus::Passed,
                    summary: format!("CI: {} checks passed on {}", total, branch_label),
                });
            } else {
                let failed_runs: Vec<(&serde_json::Value, String)> = runs
                    .iter()
                    .filter(|r| {
                        r.get("conclusion")
                            .and_then(|s| s.as_str())
                            .map_or(false, |c| {
                                c == "failure" || c == "cancelled" || c == "timed_out"
                            })
                    })
                    .map(|r| {
                        let name = r
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        (r, name)
                    })
                    .collect();

                // fetch logs from failed runs
                let mut log_output = String::new();
                for (run, name) in &failed_runs {
                    if let Some(run_id) = run.get("databaseId").and_then(|v| v.as_u64()) {
                        if let Ok(o) = tokio::process::Command::new("gh")
                            .args(["run", "view", &run_id.to_string(), "--log-failed"])
                            .current_dir(cwd)
                            .output()
                            .await
                        {
                            if o.status.success() {
                                let logs = String::from_utf8_lossy(&o.stdout);
                                let mut in_group = false;
                                let cleaned: Vec<String> = logs
                                    .lines()
                                    .filter_map(|line| {
                                        // each line is: "job\tstep\ttimestamp content"
                                        let rest = line.splitn(3, '\t').nth(2).unwrap_or(line);
                                        // strip the timestamp prefix (2026-01-01T00:00:00.0000000Z)
                                        let content = if rest.len() > 28
                                            && rest.as_bytes().get(4) == Some(&b'-')
                                        {
                                            rest.get(29..).unwrap_or(rest).trim()
                                        } else {
                                            rest.trim()
                                        };
                                        // skip group blocks (script definitions echoed by actions)
                                        if content.starts_with("##[group]") {
                                            in_group = true;
                                            return None;
                                        }
                                        if content.starts_with("##[endgroup]") {
                                            in_group = false;
                                            return None;
                                        }
                                        if in_group {
                                            return None;
                                        }
                                        if content.is_empty() || content.starts_with("\u{FEFF}") {
                                            return None;
                                        }
                                        let content =
                                            content.strip_prefix("##[error]").unwrap_or(content);
                                        let clean = tui::strip_ansi(content);
                                        if clean.is_empty() {
                                            return None;
                                        }
                                        Some(clean)
                                    })
                                    .collect();
                                let tail: String = cleaned
                                    .iter()
                                    .rev()
                                    .take(30)
                                    .collect::<Vec<_>>()
                                    .into_iter()
                                    .rev()
                                    .map(|s| s.as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                if !tail.is_empty() {
                                    log_output.push_str(&format!("\n--- {} ---\n{}", name, tail));
                                }
                            }
                        }
                    }
                }

                let failed_names: Vec<&String> = failed_runs.iter().map(|(_, n)| n).collect();
                let summary = if log_output.is_empty() {
                    format!(
                        "CI: {}/{} failed on {} ({})",
                        failed,
                        total,
                        branch_label,
                        failed_names
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                } else {
                    format!(
                        "CI: {}/{} failed on {} ({}){}",
                        failed,
                        total,
                        branch_label,
                        failed_names
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        log_output
                    )
                };

                let _ = tx.send(tui::JobEvent::Complete {
                    id: job_id,
                    status: tui::JobStatus::Failed(format!("{} failed", failed)),
                    summary,
                });
            }
            return;
        }
    }

    if !runs_found {
        let _ = tx.send(tui::JobEvent::Dismiss { id: job_id });
    } else {
        let _ = tx.send(tui::JobEvent::Complete {
            id: job_id,
            status: tui::JobStatus::Failed("timed out".to_string()),
            summary: format!("CI: timed out waiting on {}", branch_label),
        });
    }
}
