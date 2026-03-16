use super::types::{ApiContext, ToolResult};
use crate::util::strip_ansi;
use std::path::Path;

pub(super) async fn exec_bash(
    input: &serde_json::Value,
    cwd: &Path,
    stream_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    api_ctx: Option<&ApiContext>,
) -> ToolResult {
    let command = match input.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return ToolResult::Error("missing 'command' parameter".to_string()),
    };

    let background = input
        .get("background")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let timeout_secs = input.get("timeout").and_then(|v| v.as_u64()).unwrap_or(600);

    let cancel = api_ctx.and_then(|c| c.cancel.clone());

    if background {
        return exec_bash_background(command, cwd, timeout_secs, api_ctx);
    }

    use tokio::io::AsyncReadExt;

    let mut child = match tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ToolResult::Error(format!("failed to execute: {}", e)),
    };

    let stdout = child.stdout.take().expect("stdout should be piped");
    let stderr = child.stderr.take().expect("stderr should be piped");

    // funnel both stdout and stderr into a single byte channel
    let (merge_tx, mut merge_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    let tx1 = merge_tx.clone();
    let stdout_task = tokio::spawn(async move {
        let mut rdr = stdout;
        let mut buf = vec![0u8; 4096];
        loop {
            match rdr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx1.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let tx2 = merge_tx.clone();
    let stderr_task = tokio::spawn(async move {
        let mut rdr = stderr;
        let mut buf = vec![0u8; 4096];
        loop {
            match rdr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx2.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // dropping the original sender so the channel closes when both reader tasks finish
    drop(merge_tx);

    let mut collected;
    let mut raw_collected = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut timed_out = false;

    // poll for cancellation every 100 ms alongside the output stream
    let cancel_poll = async {
        loop {
            if cancel
                .as_ref()
                .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(false)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    tokio::pin!(cancel_poll);

    loop {
        tokio::select! {
            biased;
            chunk = tokio::time::timeout_at(deadline, merge_rx.recv()) => {
                match chunk {
                    Ok(Some(bytes)) => {
                        let raw = String::from_utf8_lossy(&bytes);
                        raw_collected.push_str(&raw);
                        // per-chunk strip for streaming display (may have minor
                        // artifacts from sequences split across chunks, but the
                        // final result is stripped from the full raw buffer below)
                        if let Some(ref tx) = stream_tx {
                            let clean = strip_ansi(&raw);
                            if !clean.is_empty() {
                                let _ = tx.send(clean);
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        timed_out = true;
                        break;
                    }
                }
            }
            _ = &mut cancel_poll => {
                stdout_task.abort();
                stderr_task.abort();
                child.kill().await.ok();
                return ToolResult::Error("cancelled".to_string());
            }
        }
    }

    stdout_task.abort();
    stderr_task.abort();

    if timed_out {
        child.kill().await.ok();
        return ToolResult::Error(format!("command timed out after {}s", timeout_secs));
    }

    let exit_status = child.wait().await.ok();
    let exit_code = exit_status.and_then(|s| s.code()).unwrap_or(-1);

    // strip the full raw buffer so escape sequences split across chunks are
    // handled correctly (streaming display may have had minor artifacts but
    // this final result is what the agent and history see)
    collected = strip_ansi(&raw_collected);

    if collected.is_empty() {
        collected = "(no output)".to_string();
    }

    if collected.len() > 50_000 {
        collected = format!("{}...\n[truncated]", &collected[..50_000]);
    }

    let output = if exit_code != 0 {
        format!("[exit code: {}]\n{}", exit_code, collected)
    } else {
        collected
    };

    ToolResult::Success { output, diff: None, read: None }
}

fn exec_bash_background(
    command: &str,
    cwd: &Path,
    timeout_secs: u64,
    api_ctx: Option<&ApiContext>,
) -> ToolResult {
    let job_tx = match api_ctx.and_then(|c| c.job_tx.clone()) {
        Some(tx) => tx,
        None => return ToolResult::Error("background jobs not available in this context".to_string()),
    };

    let job_id = api_ctx
        .map(|c| c.next_job_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(0);

    let cmd_label: String = command.chars().take(40).collect();
    let return_label = cmd_label.clone();
    let command = command.to_string();
    let cwd = cwd.to_path_buf();

    let _ = job_tx.send(crate::tui::JobEvent::Show { id: job_id });

    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;

        let mut child = match tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&command)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = job_tx.send(crate::tui::JobEvent::Complete {
                    id: job_id,
                    status: crate::tui::JobStatus::Failed("error".to_string()),
                    summary: format!("background `{}`: {}", cmd_label, e),
                });
                return;
            }
        };

        let stdout = child.stdout.take().expect("stdout should be piped");
        let stderr = child.stderr.take().expect("stderr should be piped");

        let (merge_tx, mut merge_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        let tx1 = merge_tx.clone();
        let stdout_task = tokio::spawn(async move {
            let mut rdr = stdout;
            let mut buf = vec![0u8; 4096];
            loop {
                match rdr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx1.send(buf[..n].to_vec()).is_err() { break; }
                    }
                }
            }
        });

        let tx2 = merge_tx.clone();
        let stderr_task = tokio::spawn(async move {
            let mut rdr = stderr;
            let mut buf = vec![0u8; 4096];
            loop {
                match rdr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx2.send(buf[..n].to_vec()).is_err() { break; }
                    }
                }
            }
        });

        drop(merge_tx);

        let mut raw_collected = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let mut timed_out = false;

        loop {
            tokio::select! {
                biased;
                chunk = tokio::time::timeout_at(deadline, merge_rx.recv()) => {
                    match chunk {
                        Ok(Some(bytes)) => {
                            let raw = String::from_utf8_lossy(&bytes);
                            raw_collected.push_str(&raw);
                        }
                        Ok(None) => break,
                        Err(_) => {
                            timed_out = true;
                            break;
                        }
                    }
                }
            }
        }

        stdout_task.abort();
        stderr_task.abort();

        if timed_out {
            child.kill().await.ok();
            let _ = job_tx.send(crate::tui::JobEvent::Complete {
                id: job_id,
                status: crate::tui::JobStatus::Failed("timed out".to_string()),
                summary: format!("background `{}`: timed out after {}s", cmd_label, timeout_secs),
            });
            return;
        }

        let exit_status = child.wait().await.ok();
        let exit_code = exit_status.and_then(|s| s.code()).unwrap_or(-1);

        let mut collected = strip_ansi(&raw_collected);
        if collected.is_empty() {
            collected = "(no output)".to_string();
        }
        if collected.len() > 50_000 {
            collected = format!("{}...\n[truncated]", &collected[..50_000]);
        }

        let output = if exit_code != 0 {
            format!("[exit code: {}]\n{}", exit_code, collected)
        } else {
            collected
        };

        if exit_code == 0 {
            let _ = job_tx.send(crate::tui::JobEvent::Complete {
                id: job_id,
                status: crate::tui::JobStatus::Passed,
                summary: format!("background `{}`:\n{}", cmd_label, output),
            });
        } else {
            let _ = job_tx.send(crate::tui::JobEvent::Complete {
                id: job_id,
                status: crate::tui::JobStatus::Failed(format!("exit {}", exit_code)),
                summary: format!("background `{}`:\n{}", cmd_label, output),
            });
        }
    });

    ToolResult::Success {
        output: format!("started in background: {}", return_label),
        diff: None,
        read: None,
    }
}
