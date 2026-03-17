use tokio::sync::mpsc;

use crate::{agent, auth, config, persistence, tui, tree};

pub enum SlashCommand {
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
    Usage,
}

pub fn parse(text: &str) -> Option<SlashCommand> {
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
        "/usage" | "/cost" | "/stats" => Some(SlashCommand::Usage),
        _ => None,
    }
}

pub fn handle(
    cmd: SlashCommand,
    app: &mut tui::App,
    control_tx: &mpsc::UnboundedSender<agent::ControlMessage>,
    login_pending: &mut Option<String>,
) {
    match cmd {
        SlashCommand::Model(pattern) => {
            handle_model(pattern, app, control_tx);
        }
        SlashCommand::Thinking(level) => {
            handle_thinking(level, app, control_tx);
        }
        SlashCommand::New => {
            app.reset_session();
            app.session_tree = persistence::SessionTree::new();
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
  /usage              show session token usage and cost\n\
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
            let tv = tree::TreeView::build(&app.session_tree);
            app.tree_view = Some(tv);
        }
        SlashCommand::Usage => {
            handle_usage(app);
        }
    }
}

pub fn handle_tree_key(
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
        KeyCode::Enter => {
            if let Some((branch_idx, msg_idx)) = tv.selected() {
                let new_branch = app.session_tree.fork(branch_idx, msg_idx);
                let messages = app.session_tree.active_messages().to_vec();
                let _ = control_tx.send(agent::ControlMessage::SwitchBranch(messages.clone()));
                let _ = persistence::save_session(
                    std::path::Path::new(app.cwd()),
                    &app.session_tree,
                );
                app.reset_session();
                app.hydrate_from_history(&messages);
                app.push_system_message(format!(
                    "forked new branch {} from branch {} at message {}",
                    new_branch, branch_idx, msg_idx
                ));
                app.tree_view = None;
            }
        }
        KeyCode::Char(' ') | KeyCode::Tab => {
            if let Some(branch_idx) = tv.selected_branch() {
                if branch_idx != app.session_tree.active {
                    app.session_tree.switch(branch_idx);
                    let messages = app.session_tree.active_messages().to_vec();
                    let _ = control_tx.send(agent::ControlMessage::SwitchBranch(messages.clone()));
                    let _ = persistence::save_session(
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

    if let Some(tv) = app.tree_view.as_mut() {
        let h = crossterm::terminal::size()
            .map(|(_, h)| h)
            .unwrap_or(24) as usize;
        tv.ensure_visible(h.saturating_sub(2));
    }
}

pub fn handle_login_code(
    input: &str,
    verifier: String,
    app: &mut tui::App,
    login_tx: mpsc::UnboundedSender<Result<(), String>>,
) {
    match auth::parse_auth_response(input) {
        Some((code, state)) => {
            app.push_system_message("authenticating...".to_string());
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
                "could not parse auth code - expected CODE#STATE or the full redirect URL. try /login again".to_string(),
            );
        }
    }
}

fn handle_model(
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

fn handle_thinking(
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

fn fmt_tokens(n: u32) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else if n < 1_000_000 {
        format!("{}k", n / 1_000)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

fn handle_usage(app: &mut tui::App) {
    let t = &app.tokens;
    let p = config::model_pricing(app.model_name());

    let input_cost = t.total_input as f64 * p.input / 1_000_000.0;
    let cache_write_cost = t.total_cache_creation as f64 * p.input * 1.25 / 1_000_000.0;
    let cache_read_cost = t.total_cache_read as f64 * p.input * 0.1 / 1_000_000.0;
    let output_cost = t.total_output as f64 * p.output / 1_000_000.0;
    let total_cost = app.cost_usd();

    let context_used = app.context_used();
    let context_pct = app.context_pct() * 100.0;

    let elapsed = t.start_time.map_or(String::from("n/a"), |s| {
        let secs = s.elapsed().as_secs();
        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
        }
    });

    let mut lines = format!(
        "session usage  ({}, {})\n",
        app.model_name(),
        app.thinking_level()
    );

    lines.push_str(&format!(
        "\n  tokens in     {:>10}    ${:.4}",
        fmt_tokens(t.total_input),
        input_cost
    ));
    lines.push_str(&format!(
        "\n  tokens out    {:>10}    ${:.4}",
        fmt_tokens(t.total_output),
        output_cost
    ));
    lines.push_str(&format!(
        "\n  cache read    {:>10}    ${:.4}",
        fmt_tokens(t.total_cache_read),
        cache_read_cost
    ));
    lines.push_str(&format!(
        "\n  cache write   {:>10}    ${:.4}",
        fmt_tokens(t.total_cache_creation),
        cache_write_cost
    ));
    lines.push_str(&format!("\n  total cost    {:>10}    ${:.4}", "", total_cost));

    lines.push_str(&format!(
        "\n\n  context       {:>10} / {}  ({:.0}%)",
        fmt_tokens(context_used),
        fmt_tokens(t.context_limit),
        context_pct
    ));
    lines.push_str(&format!(
        "\n  elapsed       {:>10}",
        elapsed
    ));

    app.push_system_message(lines);
}
