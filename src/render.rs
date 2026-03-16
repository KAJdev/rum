use crate::diff::{DiffInfo, DiffLineTag};
use crate::input::{
    Suggestion, make_display_input, remap_cursor, slash_suggestions,
};
use crate::tui::{
    ActivityItem, App, BackgroundJob, CachedRender, CompactStatus, DiffMarker,
    JobStatus, QueuedItem,
    SystemKind, TokenBucket, ToolEntry, ToolStatus, ViewMode,
    ACCENT, BAR_COLOR, BG, BRANCH_COLOR, DIM, FG, GREEN, INPUT_BG, MUTED,
    RED, SIDEBAR_WIDTH, SURFACE, THINKING_COLOR, TOOL_COLOR, YELLOW,
    capitalize_tool, last_paragraph,
    spinner_char, strip_exit_prefix, tool_line, wrap_md_lines_with_bar, wrap_text_with_bar,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::editor::{self, SearchMode};

pub fn render(frame: &mut Frame, app: &mut App) {
    if app.tree_view.is_some() {
        render_tree_view(frame, app);
        return;
    }
    match app.editor.mode {
        ViewMode::Chat => render_chat(frame, app),
        ViewMode::Editor => render_editor(frame, app),
    }
}

fn render_editor(frame: &mut Frame, app: &mut App) {
    let size = frame.area();
    let has_search = app.editor.search.is_some();
    let search_h: u16 = if has_search { 12.min(size.height / 3) } else { 0 };

    if app.editor.buffer.is_none() && !has_search {
        // split horizontally so the sidebar still renders
        let hsplit = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(20),
                Constraint::Length(SIDEBAR_WIDTH),
            ])
            .split(size);
        let msg = Paragraph::new(Line::from(vec![
            Span::styled("  no file open. ", Style::default().fg(MUTED)),
            Span::styled("ctrl+p", Style::default().fg(ACCENT)),
            Span::styled(" to find a file, ", Style::default().fg(MUTED)),
            Span::styled("ctrl+e", Style::default().fg(ACCENT)),
            Span::styled(" to go back", Style::default().fg(MUTED)),
        ]));
        frame.render_widget(msg, hsplit[0]);
        render_editor_sidebar(frame, app, hsplit[1]);
        return;
    }

    // split: left (editor + search) | right (sidebar)
    let hsplit = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(20),
            Constraint::Length(SIDEBAR_WIDTH),
        ])
        .split(size);

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                   // status bar
            Constraint::Min(3),                      // editor content
            Constraint::Length(search_h),             // search overlay
        ])
        .split(hsplit[0]);

    render_editor_status(frame, app, left_chunks[0]);

    if app.editor.buffer.is_some() {
        render_editor_content(frame, app, left_chunks[1]);
        render_autocomplete_menu(frame, app, left_chunks[1]);
    }

    if has_search {
        render_search_overlay(frame, app, left_chunks[2]);
    }

    render_editor_sidebar(frame, app, hsplit[1]);
}

fn render_editor_status(frame: &mut Frame, app: &App, area: Rect) {
    let buf = match &app.editor.buffer {
        Some(b) => b,
        None => return,
    };

    let dirty_marker = if buf.dirty { " [+]" } else { "" };
    let rel_path = buf.relative_path(&app.cwd);
    let position = format!("{}:{}", buf.cursor_row + 1, buf.cursor_col + 1);
    let lines = format!("{} lines", buf.line_count());

    let follow_indicator = if app.editor.follow_mode {
        let edit_pos = if app.editor.agent_edits.is_empty() {
            String::new()
        } else {
            format!(" {}/{}", app.editor.agent_edit_index + 1, app.editor.agent_edits.len())
        };
        format!("  follow{}", edit_pos)
    } else {
        String::new()
    };

    let right = format!("{}  {}  {}", follow_indicator, lines, position);
    let left_budget = (area.width as usize).saturating_sub(right.len() + 2);

    let display_path = if rel_path.len() > left_budget {
        format!("...{}", &rel_path[rel_path.len().saturating_sub(left_budget - 3)..])
    } else {
        rel_path
    };

    let pad = (area.width as usize).saturating_sub(display_path.len() + dirty_marker.len() + right.len());

    let mut spans = vec![
        Span::styled(format!(" {}", display_path), Style::default().fg(FG)),
        Span::styled(dirty_marker.to_string(), Style::default().fg(YELLOW)),
        Span::styled(" ".repeat(pad), Style::default()),
    ];

    if app.editor.follow_mode {
        let fi = format!("  follow");
        let edit_pos = if app.editor.agent_edits.is_empty() {
            String::new()
        } else {
            format!(" {}/{}", app.editor.agent_edit_index + 1, app.editor.agent_edits.len())
        };
        spans.push(Span::styled(fi, Style::default().fg(GREEN)));
        spans.push(Span::styled(edit_pos, Style::default().fg(DIM)));
        spans.push(Span::styled(
            format!("  {}  {}", lines, position),
            Style::default().fg(MUTED),
        ));
    } else {
        spans.push(Span::styled(right, Style::default().fg(MUTED)));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(INPUT_BG)),
        area,
    );
}

fn render_editor_content(frame: &mut Frame, app: &mut App, area: Rect) {
    use unicode_width::UnicodeWidthStr;

    let dirty_from = app.editor.buffer.as_mut().and_then(|b| b.dirty_from.take());

    let buf = match &app.editor.buffer {
        Some(b) => b,
        None => return,
    };

    let viewport_h = area.height as usize;
    let gutter_width: u16 = (buf.line_count().max(1).to_string().len() + 2) as u16;
    let content_cols = (area.width as usize).saturating_sub(gutter_width as usize);

    // initialize highlighter lazily
    if app.editor.highlighter.is_none() {
        app.editor.highlighter = Some(editor::Highlighter::new());
    }

    // request enough highlighted lines to fill the viewport even with wrapping.
    // worst case every line wraps, but typically we need fewer.
    let hl_request = viewport_h;
    let highlighted = if let Some(ref mut hl) = app.editor.highlighter {
        hl.highlight_lines(&buf.path, &buf.lines, buf.generation, dirty_from, buf.scroll_row, hl_request)
    } else {
        Vec::new()
    };

    let mut lines: Vec<Line> = Vec::with_capacity(viewport_h);
    // track which screen row the cursor lands on
    let mut cursor_screen_row: Option<usize> = None;
    let mut cursor_screen_col: Option<usize> = None;
    let mut line_idx = buf.scroll_row;
    let mut hl_idx: usize = 0;

    // build a line-number index for diagnostics to avoid O(n) search per line
    let diag_map: std::collections::HashMap<usize, &crate::lsp::DiagnosticInfo> = app
        .lsp
        .diagnostics
        .iter()
        .map(|d| (d.line as usize, d))
        .collect();

    while lines.len() < viewport_h {
        if line_idx >= buf.lines.len() {
            let spans = vec![
                Span::styled(
                    format!("{:>width$} ", "~", width = gutter_width as usize - 1),
                    Style::default().fg(DIM),
                ),
            ];
            lines.push(Line::from(spans));
            continue; // will hit viewport_h and exit
        }

        let is_cursor_line = line_idx == buf.cursor_row;
        let diff_marker = if app.editor.follow_mode { app.editor.diff_markers.get(&line_idx) } else { None };

        // check for LSP diagnostics on this line
        let line_diag = diag_map.get(&line_idx).copied();
        let diag_severity = line_diag.map(|d| d.severity);

        let gutter_style = match diff_marker {
            Some(DiffMarker::Insert) => Style::default().fg(GREEN),
            Some(DiffMarker::DeleteBoundary) => Style::default().fg(RED),
            _ if is_cursor_line => Style::default().fg(ACCENT),
            _ => Style::default().fg(DIM),
        };

        let line_bg = match diff_marker {
            Some(DiffMarker::Insert) => Some(Color::Rgb(20, 40, 20)),
            Some(DiffMarker::DeleteBoundary) => Some(Color::Rgb(40, 20, 20)),
            _ => match diag_severity {
                Some(crate::lsp::DiagSeverity::Error) => Some(Color::Rgb(40, 15, 15)),
                Some(crate::lsp::DiagSeverity::Warning) => Some(Color::Rgb(40, 30, 10)),
                _ => None,
            },
        };

        let cursor_bg = Color::Rgb(30, 33, 40);

        // build the content spans for this file line
        let content_spans: Vec<(Style, String)> = if hl_idx < highlighted.len() {
            highlighted[hl_idx]
                .iter()
                .map(|(style, text)| {
                    let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                    let mut s = Style::default().fg(fg);
                    if is_cursor_line {
                        s = s.bg(cursor_bg);
                    } else if let Some(bg) = line_bg {
                        s = s.bg(bg);
                    }
                    (s, text.clone())
                })
                .collect()
        } else {
            let text = &buf.lines[line_idx];
            let mut s = if is_cursor_line {
                Style::default().fg(FG).bg(cursor_bg)
            } else {
                Style::default().fg(FG)
            };
            if let Some(bg) = line_bg {
                if !is_cursor_line {
                    s = s.bg(bg);
                }
            }
            vec![(s, text.clone())]
        };

        // compute cursor visual column within this line
        let cursor_vcol = if is_cursor_line {
            let line = &buf.lines[buf.cursor_row];
            let byte_pos = buf.cursor_col.min(line.len());
            let safe_pos = (0..=byte_pos).rev().find(|&i| line.is_char_boundary(i)).unwrap_or(0);
            Some(UnicodeWidthStr::width(&line[..safe_pos]))
        } else {
            None
        };

        // split content spans into wrapped screen rows
        let wrap_rows = wrap_spans(&content_spans, content_cols);
        let num_wraps = wrap_rows.len().max(1);

        for (wrap_i, row_spans) in wrap_rows.iter().enumerate() {
            if lines.len() >= viewport_h {
                break;
            }

            let mut spans: Vec<Span> = Vec::new();

            // gutter: show line number on first wrap row, blank on continuation
            if wrap_i == 0 {
                let line_num = format!("{:>width$} ", line_idx + 1, width = gutter_width as usize - 1);
                spans.push(Span::styled(line_num, gutter_style));
            } else {
                let blank = format!("{:>width$} ", "·", width = gutter_width as usize - 1);
                spans.push(Span::styled(blank, Style::default().fg(DIM)));
            }

            spans.extend(row_spans.iter().cloned());

            let row_width: usize = row_spans.iter().map(|s| UnicodeWidthStr::width(s.content.as_ref())).sum();
            let remaining = content_cols.saturating_sub(row_width);
            let is_last_wrap = wrap_i == num_wraps - 1;

            // inline diagnostic after end of line content (last wrap row only)
            let mut diag_used = 0usize;
            if is_last_wrap {
                if let Some(d) = line_diag {
                    let (icon, color) = match d.severity {
                        crate::lsp::DiagSeverity::Error => (" ✗ ", Color::Rgb(255, 100, 100)),
                        crate::lsp::DiagSeverity::Warning => (" ⚠ ", YELLOW),
                        crate::lsp::DiagSeverity::Info => (" ℹ ", Color::Rgb(100, 180, 255)),
                        crate::lsp::DiagSeverity::Hint => (" · ", MUTED),
                    };
                    if remaining > 5 {
                        let icon_w = UnicodeWidthStr::width(icon);
                        let msg_budget = remaining.saturating_sub(icon_w + 1);
                        let msg: String = d.message.chars().take(msg_budget).collect();
                        let msg_w = UnicodeWidthStr::width(msg.as_str());
                        spans.push(Span::styled(icon, Style::default().fg(color)));
                        spans.push(Span::styled(msg, Style::default().fg(color)));
                        diag_used = icon_w + msg_w;
                    }
                }
            }

            // pad to edge
            let pad_bg = if is_cursor_line {
                Some(cursor_bg)
            } else {
                line_bg
            };
            if let Some(bg) = pad_bg {
                let pad = remaining.saturating_sub(diag_used);
                if pad > 0 {
                    spans.push(Span::styled(
                        " ".repeat(pad),
                        Style::default().bg(bg),
                    ));
                }
            }

            // track cursor screen position
            if let Some(vcol) = cursor_vcol {
                let row_start_col = wrap_i * content_cols;
                let row_end_col = row_start_col + content_cols;
                if vcol >= row_start_col && vcol < row_end_col {
                    cursor_screen_row = Some(lines.len());
                    cursor_screen_col = Some(vcol - row_start_col);
                } else if wrap_i == num_wraps - 1 && cursor_screen_row.is_none() {
                    // cursor past end of last wrap row
                    cursor_screen_row = Some(lines.len());
                    let row_width: usize = row_spans.iter().map(|s| UnicodeWidthStr::width(s.content.as_ref())).sum();
                    cursor_screen_col = Some(row_width.min(vcol.saturating_sub(row_start_col)));
                }
            }

            lines.push(Line::from(spans));
        }

        // empty line (no content) still needs one row
        if wrap_rows.is_empty() {
            if lines.len() < viewport_h {
                let line_num = format!("{:>width$} ", line_idx + 1, width = gutter_width as usize - 1);
                let mut spans = vec![Span::styled(line_num, gutter_style)];
                let pad_bg = if is_cursor_line { Some(cursor_bg) } else { line_bg };
                if let Some(bg) = pad_bg {
                    spans.push(Span::styled(
                        " ".repeat(content_cols),
                        Style::default().bg(bg),
                    ));
                }
                if is_cursor_line {
                    cursor_screen_row = Some(lines.len());
                    cursor_screen_col = Some(0);
                }
                lines.push(Line::from(spans));
            }
        }

        line_idx += 1;
        hl_idx += 1;
    }

    let widget = Paragraph::new(lines).style(Style::default().bg(BG));
    frame.render_widget(widget, area);

    // set cursor position
    if let (Some(row), Some(col)) = (cursor_screen_row, cursor_screen_col) {
        let cursor_x = area.x + gutter_width + col as u16;
        let cursor_y = area.y + row as u16;
        if cursor_y < area.y + area.height && cursor_x < area.x + area.width {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }
}

// split a sequence of styled spans into rows that fit within `max_cols` visual columns
fn wrap_spans(spans: &[(Style, String)], max_cols: usize) -> Vec<Vec<Span<'static>>> {
    use unicode_width::UnicodeWidthChar;

    if max_cols == 0 {
        return vec![];
    }

    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current_row: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;

    for (style, text) in spans {
        let mut segment = String::new();
        for ch in text.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if col + w > max_cols && col > 0 {
                // wrap: push current segment, start new row
                if !segment.is_empty() {
                    current_row.push(Span::styled(segment.clone(), *style));
                    segment.clear();
                }
                rows.push(std::mem::take(&mut current_row));
                col = 0;
            }
            segment.push(ch);
            col += w;
        }
        if !segment.is_empty() {
            current_row.push(Span::styled(segment, *style));
        }
    }
    if !current_row.is_empty() {
        rows.push(current_row);
    }

    rows
}

fn render_autocomplete_menu(frame: &mut Frame, app: &App, area: Rect) {
    use unicode_width::UnicodeWidthStr;

    let ac = match &app.editor.autocomplete {
        Some(ac) if !ac.candidates.is_empty() => ac,
        _ => return,
    };
    let buf = match &app.editor.buffer {
        Some(b) => b,
        None => return,
    };

    let gutter_width = (buf.line_count().max(1).to_string().len() + 2) as u16;
    let content_cols = (area.width as usize).saturating_sub(gutter_width as usize);

    // find cursor screen position relative to editor area
    let line = &buf.lines[buf.cursor_row];
    let word_start_visual = {
        let safe = ac.word_start.min(line.len());
        let pos = (0..=safe).rev().find(|&i| line.is_char_boundary(i)).unwrap_or(0);
        UnicodeWidthStr::width(&line[..pos])
    };

    // account for wrapping
    let wrap_row = word_start_visual / content_cols.max(1);
    let wrap_col = word_start_visual % content_cols.max(1);

    // compute screen row of cursor line (accounting for wrapped lines above)
    let mut screen_row = 0usize;
    for i in buf.scroll_row..buf.cursor_row {
        if i >= buf.lines.len() { break; }
        let lw = UnicodeWidthStr::width(buf.lines[i].as_str());
        screen_row += if lw == 0 || content_cols == 0 { 1 } else { (lw + content_cols - 1) / content_cols };
    }
    screen_row += wrap_row;

    // position the menu below the cursor line
    let menu_y = area.y + screen_row as u16 + 1;
    let menu_x = area.x + gutter_width + wrap_col as u16;
    let max_label = ac.candidates.iter().map(|c| {
        let detail_len = c.detail.as_ref().map(|d| d.len() + 1).unwrap_or(0);
        c.label.len() + detail_len
    }).max().unwrap_or(10);
    let menu_w = (max_label + 4).min(50) as u16;
    let menu_h = ac.candidates.len().min(8) as u16;

    // flip above if not enough room below
    let available_below = area.y + area.height - menu_y;
    let (final_y, final_h) = if available_below >= menu_h {
        (menu_y, menu_h.min(available_below))
    } else {
        let above = screen_row as u16;
        let h = menu_h.min(above);
        (area.y + screen_row as u16 - h, h)
    };

    // clamp to area bounds
    let final_x = menu_x.min(area.x + area.width - menu_w.min(area.width));
    let final_w = menu_w.min(area.x + area.width - final_x);

    if final_h == 0 || final_w == 0 {
        return;
    }

    let menu_area = Rect::new(final_x, final_y, final_w, final_h);

    let mut lines: Vec<Line> = Vec::new();
    for (i, candidate) in ac.candidates.iter().take(final_h as usize).enumerate() {
        let is_selected = i == ac.selected;

        let kind_icon = candidate.kind.icon();

        let (label_style, icon_style, detail_style) = if is_selected {
            (
                Style::default().fg(BG).bg(ACCENT),
                Style::default().fg(BG).bg(ACCENT),
                Style::default().fg(BG).bg(ACCENT),
            )
        } else {
            (
                Style::default().fg(FG).bg(INPUT_BG),
                Style::default().fg(MUTED).bg(INPUT_BG),
                Style::default().fg(MUTED).bg(INPUT_BG),
            )
        };

        let max_label_len = (final_w as usize).saturating_sub(4);
        let label: String = candidate.label.chars().take(max_label_len).collect();
        // show detail (type info) if there's room
        let detail_str = if let Some(ref d) = candidate.detail {
            let avail = max_label_len.saturating_sub(label.len() + 1);
            if avail > 3 {
                let d: String = d.chars().take(avail).collect();
                format!(" {}", d)
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        let pad = (final_w as usize).saturating_sub(label.len() + detail_str.len() + 3);
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", kind_icon), icon_style),
            Span::styled(label, label_style),
            Span::styled(detail_str, detail_style),
            Span::styled(" ".repeat(pad), label_style),
        ]));
    }

    let widget = Paragraph::new(lines).style(Style::default().bg(INPUT_BG));
    frame.render_widget(ratatui::widgets::Clear, menu_area);
    frame.render_widget(widget, menu_area);
}

fn render_search_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let search = match &app.editor.search {
        Some(s) => s,
        None => return,
    };

    let mode_label = match search.mode {
        SearchMode::Files => "files",
        SearchMode::Text => "search",
    };

    // input line
    let input_line = Line::from(vec![
        Span::styled(format!(" {} ", mode_label), Style::default().fg(BG).bg(ACCENT)),
        Span::styled(format!(" {}", search.query), Style::default().fg(FG)),
    ]);

    let mut lines = vec![input_line];

    // results
    let max_visible = (area.height as usize).saturating_sub(1);
    let start = if search.selected >= max_visible {
        search.selected - max_visible + 1
    } else {
        0
    };

    for (i, result) in search.results.iter().skip(start).take(max_visible).enumerate() {
        let idx = start + i;
        let is_selected = idx == search.selected;

        let (path_style, content_style) = if is_selected {
            (
                Style::default().fg(BG).bg(ACCENT),
                Style::default().fg(BG).bg(ACCENT),
            )
        } else {
            (Style::default().fg(ACCENT), Style::default().fg(MUTED))
        };

        let mut spans = vec![Span::styled(format!("  {}", result.path), path_style)];

        if let Some(line) = result.line {
            spans.push(Span::styled(format!(":{}", line + 1), path_style));
        }

        if let Some(ref content) = result.content {
            let truncated = if content.chars().count() > 60 {
                let s: String = content.chars().take(57).collect();
                format!("  {}...", s)
            } else {
                format!("  {}", content)
            };
            spans.push(Span::styled(truncated, content_style));
        }

        lines.push(Line::from(spans));
    }

    let widget = Paragraph::new(lines).style(Style::default().bg(INPUT_BG));
    frame.render_widget(widget, area);
}

fn render_editor_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    if area.width < 4 || area.height < 2 {
        return;
    }

    let max_w = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();

    // render activity items bottom-up (most recent first),
    // fitting as many as the sidebar height allows
    let avail = area.height as usize;
    let mut item_lines: Vec<Line> = Vec::new();

    for item in app.feed.items.iter().rev() {
        if item_lines.len() >= avail {
            break;
        }
        match item {
            ActivityItem::Thinking(t) => {
                let text = t.chars().take(max_w).collect::<String>();
                item_lines.push(Line::from(Span::styled(
                    format!(" {}", text),
                    Style::default().fg(THINKING_COLOR).add_modifier(Modifier::ITALIC),
                )));
            }
            ActivityItem::Text(t) => {
                // show just the last line of text output
                let last = t.lines().last().unwrap_or("");
                let text: String = last.chars().take(max_w.saturating_sub(2)).collect();
                item_lines.push(Line::from(vec![
                    Span::styled(" \u{2502} ", Style::default().fg(BAR_COLOR)),
                    Span::styled(text, Style::default().fg(FG)),
                ]));
            }
            ActivityItem::Tool(entry) => {
                let icon = match entry.status {
                    ToolStatus::Running => spinner_char(app.spin_frame),
                    ToolStatus::Complete { exit_code } => {
                        if exit_code.unwrap_or(0) != 0 { "✗" } else { "✓" }
                    }
                    ToolStatus::Error(_) => "✗",
                };
                let icon_color = match entry.status {
                    ToolStatus::Running => ACCENT,
                    ToolStatus::Complete { exit_code } => {
                        if exit_code.unwrap_or(0) != 0 { RED } else { GREEN }
                    }
                    ToolStatus::Error(_) => RED,
                };
                let label = capitalize_tool(&entry.name);
                let arg_budget = max_w.saturating_sub(label.len() + 4);
                let arg: String = entry.arg.chars().take(arg_budget).collect();
                item_lines.push(Line::from(vec![
                    Span::styled(format!(" {} ", icon), Style::default().fg(icon_color)),
                    Span::styled(label.to_string(), Style::default().fg(TOOL_COLOR)),
                    Span::styled(format!(" {}", arg), Style::default().fg(DIM)),
                ]));
            }
            ActivityItem::UserMessage(msg) => {
                let text: String = msg.chars().take(max_w.saturating_sub(2)).collect();
                item_lines.push(Line::from(vec![
                    Span::styled(" > ", Style::default().fg(ACCENT)),
                    Span::styled(text, Style::default().fg(FG)),
                ]));
            }
            ActivityItem::System(kind, msg) => {
                let color = match kind {
                    SystemKind::Info => MUTED,
                    SystemKind::Success => GREEN,
                    SystemKind::Warning => YELLOW,
                    SystemKind::Error => RED,
                    SystemKind::Update => ACCENT,
                };
                let text: String = msg.lines().next().unwrap_or("").chars().take(max_w).collect();
                item_lines.push(Line::from(Span::styled(
                    format!(" {}", text),
                    Style::default().fg(color),
                )));
            }
            ActivityItem::Compact(status) => {
                let (icon, text) = match status {
                    CompactStatus::Running => (spinner_char(app.spin_frame), "compacting..."),
                    CompactStatus::Done(_) => ("✓", "compacted"),
                    CompactStatus::Cancelled => ("✗", "cancelled"),
                };
                item_lines.push(Line::from(vec![
                    Span::styled(format!(" {} ", icon), Style::default().fg(MUTED)),
                    Span::styled(text.to_string(), Style::default().fg(MUTED)),
                ]));
            }
        }
    }

    // reverse so newest is at bottom
    item_lines.reverse();
    lines.extend(item_lines);

    // separator on the left edge
    let sep_area = Rect {
        x: area.x,
        y: area.y,
        width: 1,
        height: area.height,
    };
    let sep_lines: Vec<Line> = (0..area.height)
        .map(|_| Line::from(Span::styled("\u{2502}", Style::default().fg(Color::Rgb(40, 44, 52)))))
        .collect();
    frame.render_widget(Paragraph::new(sep_lines), sep_area);

    let content_area = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(1),
        height: area.height,
    };
    frame.render_widget(Paragraph::new(lines), content_area);
}

fn render_tree_view(frame: &mut Frame, app: &mut App) {
    use crate::tree::NodeKind;

    let tv = match app.tree_view.as_ref() {
        Some(tv) => tv,
        None => return,
    };

    let size = frame.area();
    frame.render_widget(
        ratatui::widgets::Block::default().style(Style::default().bg(BG)),
        size,
    );

    // header
    let header_area = Rect::new(0, 0, size.width, 1);
    let branch_count = app.session_tree.branches.len();
    let header_text = format!(
        " session tree  {} branch{}  (↑↓ navigate, ⇧↑↓ jump user msgs, enter fork, space switch, esc close)",
        branch_count,
        if branch_count == 1 { "" } else { "es" }
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(header_text, Style::default().fg(MUTED)),
        ]))
        .style(Style::default().bg(SURFACE)),
        header_area,
    );

    // tree content area
    let content_area = Rect::new(0, 1, size.width, size.height.saturating_sub(1));
    let viewport_h = content_area.height as usize;

    if tv.nodes.is_empty() {
        frame.render_widget(
            Paragraph::new("  (empty session)")
                .style(Style::default().fg(MUTED).bg(BG)),
            content_area,
        );
        return;
    }

    let active_branch = app.session_tree.active;

    for (vi, node_idx) in (tv.scroll..).take(viewport_h).enumerate() {
        if node_idx >= tv.nodes.len() {
            break;
        }
        let node = &tv.nodes[node_idx];
        let is_selected = node_idx == tv.cursor;
        let is_active_branch = node.branch_idx == active_branch;
        let y = content_area.y + vi as u16;
        let row_area = Rect::new(0, y, size.width, 1);

        let mut spans: Vec<Span> = Vec::new();

        // indentation + tree connectors
        let indent_w = node.depth * 3;
        if indent_w > 0 {
            let mut indent = String::new();
            for d in 0..node.depth {
                if d == node.depth - 1 {
                    if node.branch_head {
                        if node.is_last_child {
                            indent.push_str(" └─");
                        } else {
                            indent.push_str(" ├─");
                        }
                    } else if node.active_pipes.contains(&d) {
                        indent.push_str(" │ ");
                    } else {
                        indent.push_str("   ");
                    }
                } else if node.active_pipes.contains(&d) {
                    indent.push_str(" │ ");
                } else {
                    indent.push_str("   ");
                }
            }
            spans.push(Span::styled(indent, Style::default().fg(DIM)));
        }

        // node icon
        let (icon, icon_color) = match node.kind {
            NodeKind::UserMessage => ("● ", ACCENT),
            NodeKind::AssistantText => ("○ ", FG),
            NodeKind::ToolCall => ("◆ ", YELLOW),
            NodeKind::Thinking => ("◇ ", DIM),
            NodeKind::Compact => ("◈ ", GREEN),
        };
        spans.push(Span::styled(icon, Style::default().fg(icon_color)));

        // node text
        let max_text = (size.width as usize)
            .saturating_sub(indent_w + 2 + 8); // icon + branch tag
        let text = if node.text.len() > max_text && max_text > 3 {
            format!("{}...", &node.text[..max_text - 3])
        } else {
            node.text.clone()
        };

        let text_color = if is_active_branch { FG } else { MUTED };
        spans.push(Span::styled(text, Style::default().fg(text_color)));

        // branch indicator for fork heads
        if node.branch_head {
            let tag = if is_active_branch {
                " ◀"
            } else {
                ""
            };
            if !tag.is_empty() {
                spans.push(Span::styled(
                    tag.to_string(),
                    Style::default().fg(ACCENT),
                ));
            }
        }

        let bg = if is_selected { SURFACE } else { BG };
        let line = Line::from(spans);
        frame.render_widget(
            Paragraph::new(line).style(Style::default().bg(bg)),
            row_area,
        );
    }
}

fn render_chat(frame: &mut Frame, app: &mut App) {
    let size = frame.area();

    let max_input = (size.height / 3).max(2);

    // slash command suggestions shown when input starts with "/".
    // use the snapshot taken at first Tab press so cycling doesn't narrow the set.
    let completion_input = app.input.slash_prefix.as_deref().unwrap_or(app.input.text.as_str());
    let suggestions: Vec<Suggestion> = if app.input.text.starts_with('/') {
        slash_suggestions(completion_input)
    } else {
        vec![]
    };
    let slash_selected = app.input.slash_selected;

    let message_height: u16 = if !suggestions.is_empty() {
        (suggestions.len() as u16).min(8)
    } else if app.is_running {
        let mut total: u16 = 0;
        if let Some(ref msg) = app.current_message {
            total += visual_line_count(msg, size.width, 2) as u16;
        }
        for qm in &app.queued_messages {
            let text = match qm {
                QueuedItem::Message(s) | QueuedItem::Command(s) => s.as_str(),
            };
            total += visual_line_count(text, size.width, 2) as u16;
        }
        total
    } else {
        0
    };

    let input_only_height = (app.input_visual_line_count() as u16).max(1);
    let combined = (message_height + input_only_height).max(1).min(max_input);
    let msg_h = message_height.min(combined);
    let input_h = combined - msg_h;

    let visible_jobs = app.background_jobs.iter().any(|j| j.visible);
    // no jobs: 1 blank line at bottom
    // jobs: 1 buffer line + 1 jobs line
    let (jobs_h, bottom_h): (u16, u16) = if visible_jobs { (1, 1) } else { (0, 1) };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),        // header
            Constraint::Length(msg_h),    // current/queued messages
            Constraint::Length(input_h),  // input field
            Constraint::Length(1),        // buffer after input
            Constraint::Min(4),           // activity feed
            Constraint::Length(bottom_h), // buffer before jobs / bottom
            Constraint::Length(jobs_h),   // background jobs bar
        ])
        .split(size);

    render_header(frame, app, chunks[0]);
    render_message_area(frame, app, chunks[1], &suggestions, slash_selected);
    render_input_area(frame, app, chunks[2]);

    // chunks[3] is the buffer after input
    render_activity(frame, app, chunks[4]);
    if jobs_h > 0 {
        render_jobs_bar(frame, app, chunks[6]);
    }
}

fn render_jobs_bar(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }

    let mut spans: Vec<Span> = Vec::new();
    let visible: Vec<&BackgroundJob> = app.background_jobs.iter().filter(|j| j.visible).collect();
    for (i, job) in visible.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  │  ", Style::default().fg(DIM)));
        }
        let (icon, icon_color) = match &job.status {
            JobStatus::Running => ("◌", YELLOW),
            JobStatus::Passed => ("✓", GREEN),
            JobStatus::Failed(_) => ("✗", RED),
        };
        spans.push(Span::styled(
            format!("{} ", icon),
            Style::default().fg(icon_color),
        ));
        spans.push(Span::styled(job.label.clone(), Style::default().fg(MUTED)));
        if !job.detail.is_empty() {
            spans.push(Span::styled(
                format!(" {}", job.detail),
                Style::default().fg(DIM),
            ));
        }
        if matches!(job.status, JobStatus::Running) {
            let secs = job.started_at.elapsed().as_secs();
            if secs >= 5 {
                let timer = if secs >= 60 {
                    format!(" {}m{}s", secs / 60, secs % 60)
                } else {
                    format!(" {}s", secs)
                };
                spans.push(Span::styled(timer, Style::default().fg(DIM)));
            }
        }
    }

    let line = Line::from(spans);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(INPUT_BG)),
        area,
    );
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let w = area.width as usize;

    let thinking_suffix = if app.thinking_level != "off" {
        format!(" ({}) ", app.thinking_level)
    } else {
        String::new()
    };

    // build right-side metrics first (fixed width, drives how much space the left side gets)
    let rate = app.avg_rate();
    let pct = app.context_pct();
    let used_k = app.context_used() / 1000;
    let limit_k = app.tokens.context_limit / 1000;

    let spark_width: usize = 16;
    let ctx_bar_width: usize = 8;
    let filled = ((pct * ctx_bar_width as f64).round() as usize).min(ctx_bar_width);
    let empty = ctx_bar_width - filled;
    let ctx_color = if pct > 0.8 {
        RED
    } else if pct > 0.6 {
        YELLOW
    } else {
        ACCENT
    };

    let rate_str = format!("{:.0} tok/s", rate);
    let cost_str = format!("${:.3}", app.cost_usd());
    let ctx_label = format!("{}k/{}k", used_k, limit_k);
    let ctx_pct = format!("{:.0}%", pct * 100.0);

    let right_len = spark_width
        + 1
        + rate_str.len()
        + 2
        + cost_str.len()
        + 2
        + ctx_label.len()
        + 2
        + ctx_bar_width
        + 2
        + ctx_pct.len();

    // fixed-width parts of the left side: "rum  " + "  " + model + thinking
    let model_part = app.model_name.len() + thinking_suffix.len();
    let fixed_left = 4 + 2 + model_part; // "rum" + "  " around cwd + model+thinking

    // available width for cwd + branch
    let budget = w.saturating_sub(fixed_left + right_len + 2); // +2 spacing margin

    let full_branch = app.git_branch.as_deref().unwrap_or("");
    let has_branch = !full_branch.is_empty();
    // branch overhead: "(" + branch + ")  " = branch_len + 4
    let branch_overhead = if has_branch { 4 } else { 0 };

    let full_cwd = &app.cwd;
    let full_left_content = full_cwd.len()
        + if has_branch {
            full_branch.len() + branch_overhead
        } else {
            0
        };

    // determine what to show, truncating to fit within budget
    let (display_cwd, display_branch): (String, Option<String>) = if full_left_content <= budget {
        // everything fits
        (
            full_cwd.clone(),
            if has_branch {
                Some(full_branch.to_string())
            } else {
                None
            },
        )
    } else if has_branch {
        // try truncating branch first (min 7 chars)
        let min_branch = 7usize;
        let cwd_plus_overhead = full_cwd.len() + branch_overhead;
        let branch_budget = budget.saturating_sub(cwd_plus_overhead);

        if branch_budget >= min_branch {
            // truncate branch to fit
            let trunc: String = full_branch.chars().take(branch_budget).collect();
            (full_cwd.clone(), Some(trunc))
        } else {
            // branch won't fit at min size with full cwd; try truncating cwd too
            let cwd_budget = budget.saturating_sub(min_branch + branch_overhead);
            if cwd_budget >= 8 {
                // truncate cwd from the start: .../tail
                let trunc_cwd = truncate_path_start(full_cwd, cwd_budget);
                let trunc_branch: String = full_branch.chars().take(min_branch).collect();
                (trunc_cwd, Some(trunc_branch))
            } else {
                // hide branch entirely, give all space to cwd
                if full_cwd.len() <= budget {
                    (full_cwd.clone(), None)
                } else if budget >= 8 {
                    (truncate_path_start(full_cwd, budget), None)
                } else {
                    (full_cwd.clone(), None)
                }
            }
        }
    } else {
        // no branch, just truncate cwd
        if budget >= 8 {
            (truncate_path_start(full_cwd, budget), None)
        } else {
            (full_cwd.clone(), None)
        }
    };

    let mut spans = vec![
        Span::styled(
            "rum",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {}  ", display_cwd), Style::default().fg(FG)),
    ];

    let branch_display_len = if let Some(ref b) = display_branch {
        spans.push(Span::styled("(", Style::default().fg(DIM)));
        spans.push(Span::styled(b.clone(), Style::default().fg(BRANCH_COLOR)));
        spans.push(Span::styled(")  ", Style::default().fg(DIM)));
        b.len() + 4
    } else {
        0
    };

    spans.push(Span::styled(
        app.model_name.clone(),
        Style::default().fg(MUTED),
    ));
    spans.push(Span::styled(
        thinking_suffix.clone(),
        Style::default().fg(MUTED),
    ));

    let left_len = 4 + display_cwd.len() + 4 + branch_display_len + model_part;

    let pad = w.saturating_sub(left_len + right_len);
    spans.push(Span::styled(" ".repeat(pad), Style::default()));

    // sparkline
    let spark_spans = render_colored_sparkline(&app.tokens.rate_samples, spark_width);
    spans.extend(spark_spans);
    spans.push(Span::styled(" ", Style::default()));

    // rate + cost
    spans.push(Span::styled(rate_str, Style::default().fg(MUTED)));
    spans.push(Span::styled("  ", Style::default()));
    spans.push(Span::styled(cost_str, Style::default().fg(DIM)));
    spans.push(Span::styled("  ", Style::default()));

    // context bar
    spans.push(Span::styled(ctx_label, Style::default().fg(DIM)));
    spans.push(Span::styled(" [", Style::default().fg(DIM)));
    spans.push(Span::styled(
        "\u{2588}".repeat(filled),
        Style::default().fg(ctx_color),
    ));
    spans.push(Span::styled(
        "\u{2591}".repeat(empty),
        Style::default().fg(Color::Rgb(30, 33, 40)),
    ));
    spans.push(Span::styled("] ", Style::default().fg(DIM)));
    spans.push(Span::styled(ctx_pct, Style::default().fg(ctx_color)));

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG)),
        area,
    );
}

// truncate a path from the start, keeping the tail: ".../parent/dir"
fn truncate_path_start(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        return path.to_string();
    }
    let prefix = ".../";
    let tail_budget = max_len.saturating_sub(prefix.len());
    if tail_budget == 0 {
        return path[path.len().saturating_sub(max_len)..].to_string();
    }
    // find a path separator within the tail portion
    let start = path.len().saturating_sub(tail_budget);
    if let Some(sep) = path[start..].find('/') {
        let clean_start = start + sep + 1;
        if clean_start < path.len() {
            return format!("{}{}", prefix, &path[clean_start..]);
        }
    }
    format!("{}{}", prefix, &path[start..])
}

// count visual lines after soft-wrapping text to fit within
// (max_width - prefix_width) columns
fn visual_line_count(text: &str, max_width: u16, prefix_width: usize) -> usize {
    let content_width = (max_width as usize).saturating_sub(prefix_width);
    if content_width == 0 {
        return 1;
    }
    let mut count = 0;
    for line in text.split('\n') {
        let w = UnicodeWidthStr::width(line);
        if w == 0 {
            count += 1;
        } else {
            count += (w + content_width - 1) / content_width;
        }
    }
    count.max(1)
}

// wrap a message into indented lines with a given text style.
// used for rendering the active message and queued messages above the input.
fn wrap_message_lines(
    text: &str,
    max_width: u16,
    text_style: Style,
    spinner: Option<&str>,
) -> Vec<Line<'static>> {
    let prefix_width = 2usize;
    let content_width = (max_width as usize).saturating_sub(prefix_width);
    if content_width == 0 {
        return vec![];
    }

    let mut lines = Vec::new();
    for logical_line in text.split('\n') {
        if logical_line.is_empty() {
            lines.push(Line::from(vec![Span::styled("  ", Style::default())]));
            continue;
        }
        let chars: Vec<char> = logical_line.chars().collect();
        let mut chunk_start = 0;
        while chunk_start < chars.len() {
            let mut w = 0;
            let mut chunk_end = chunk_start;
            while chunk_end < chars.len() {
                let cw = unicode_width::UnicodeWidthChar::width(chars[chunk_end]).unwrap_or(0);
                if w + cw > content_width {
                    break;
                }
                w += cw;
                chunk_end += 1;
            }
            if chunk_end == chunk_start {
                chunk_end = chunk_start + 1;
            }
            let chunk_text: String = chars[chunk_start..chunk_end].iter().collect();

            let prefix = if lines.is_empty() {
                if let Some(s) = spinner {
                    format!("{} ", s)
                } else {
                    "  ".to_string()
                }
            } else {
                "  ".to_string()
            };
            let prefix_style = if lines.is_empty() && spinner.is_some() {
                Style::default().fg(ACCENT)
            } else {
                Style::default()
            };

            lines.push(Line::from(vec![
                Span::styled(prefix, prefix_style),
                Span::styled(chunk_text, text_style),
            ]));
            chunk_start = chunk_end;
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(vec![Span::styled("  ", Style::default())]));
    }
    lines
}

// wrap input/message text into visual lines with a prefix on the first line.
// returns the visual lines and, if a cursor char position is given,
// the (visual_row, visual_col) of the cursor.
fn wrap_input_text(
    text: &str,
    max_width: u16,
    cursor_char_pos: Option<usize>,
    text_color: Color,
) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let prefix_width: usize = 2;
    let content_width = (max_width as usize).saturating_sub(prefix_width);
    if content_width == 0 {
        return (vec![], None);
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut cursor_visual: Option<(u16, u16)> = None;
    let mut char_offset: usize = 0; // running char position across the whole input

    for (line_idx, logical_line) in text.split('\n').enumerate() {
        let prefix = if lines.is_empty() { "\u{203a} " } else { "  " };

        if logical_line.is_empty() {
            // check if cursor is on this empty line
            if let Some(cp) = cursor_char_pos {
                if cp == char_offset {
                    cursor_visual = Some((lines.len() as u16, 0));
                }
            }
            lines.push(Line::from(vec![Span::styled(
                prefix,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )]));
            // advance past the newline separator (except after the last line)
            char_offset += 1; // for the '\n'
            continue;
        }

        let chars: Vec<char> = logical_line.chars().collect();
        let mut chunk_start: usize = 0; // index into chars[]

        while chunk_start < chars.len() {
            let row_prefix = if lines.is_empty() { "\u{203a} " } else { "  " };

            // find how many chars fit in content_width
            let mut w: usize = 0;
            let mut chunk_end = chunk_start;
            while chunk_end < chars.len() {
                let cw = unicode_width::UnicodeWidthChar::width(chars[chunk_end]).unwrap_or(0);
                if w + cw > content_width {
                    break;
                }
                w += cw;
                chunk_end += 1;
            }
            if chunk_end == chunk_start {
                // single char wider than content_width, force it
                chunk_end = chunk_start + 1;
            }

            let chunk_text: String = chars[chunk_start..chunk_end].iter().collect();

            // check if cursor falls within this visual row
            if let Some(cp) = cursor_char_pos {
                let abs_start = char_offset + chunk_start;
                let abs_end = char_offset + chunk_end;
                if cp >= abs_start && cp < abs_end {
                    let col_chars = &chars[chunk_start..(chunk_start + (cp - abs_start))];
                    let col_w: usize = col_chars
                        .iter()
                        .map(|c| unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0))
                        .sum();
                    cursor_visual = Some((lines.len() as u16, col_w as u16));
                }
                // cursor at end of last chunk of this logical line
                if cp == abs_end && chunk_end == chars.len() {
                    cursor_visual = Some((lines.len() as u16, w as u16));
                }
            }

            lines.push(Line::from(vec![
                Span::styled(
                    row_prefix,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(chunk_text, Style::default().fg(text_color)),
            ]));

            chunk_start = chunk_end;
        }

        // advance char_offset past this logical line + newline separator
        char_offset += chars.len();
        if line_idx < text.matches('\n').count() {
            char_offset += 1; // '\n'
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "\u{203a} ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )]));
        if cursor_char_pos == Some(0) {
            cursor_visual = Some((0, 0));
        }
    }

    (lines, cursor_visual)
}

fn render_message_area(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    suggestions: &[Suggestion],
    selected: Option<usize>,
) {
    if area.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    if app.is_running && suggestions.is_empty() {
        let spin = spinner_char(app.spin_frame);
        if let Some(ref msg) = app.current_message {
            lines.extend(wrap_message_lines(
                msg,
                area.width,
                Style::default().fg(ACCENT),
                Some(spin),
            ));
        }
        for qm in &app.queued_messages {
            match qm {
                QueuedItem::Message(s) => {
                    lines.extend(wrap_message_lines(
                        s,
                        area.width,
                        Style::default().fg(MUTED),
                        None,
                    ));
                }
                QueuedItem::Command(s) => {
                    lines.push(Line::from(vec![
                        Span::styled("  › ", Style::default().fg(DIM)),
                        Span::styled(s.clone(), Style::default().fg(DIM)),
                    ]));
                }
            }
        }
    } else if !suggestions.is_empty() {
        for (i, s) in suggestions.iter().enumerate() {
            let is_sel = selected == Some(i);
            let (cmd_style, desc_style) = if is_sel {
                (
                    Style::default().fg(BG).bg(ACCENT),
                    Style::default().fg(BG).bg(ACCENT),
                )
            } else {
                (Style::default().fg(ACCENT), Style::default().fg(MUTED))
            };
            let prefix = if is_sel { "\u{203a} " } else { "  " };
            let mut spans = vec![
                Span::styled(prefix, cmd_style),
                Span::styled(s.display.clone(), cmd_style),
            ];
            if !s.description.is_empty() {
                spans.push(Span::styled(format!("  {}", s.description), desc_style));
            }
            lines.push(Line::from(spans));
        }
    }

    let widget = Paragraph::new(lines).style(Style::default().bg(BG));
    frame.render_widget(widget, area);
}

fn render_input_area(frame: &mut Frame, app: &App, area: Rect) {
    let display = make_display_input(&app.input.text, &app.input.paste_chunks);
    let display_cursor = remap_cursor(&app.input.text, &app.input.paste_chunks, app.input.cursor_pos);
    let (input_lines, cursor_pos) = wrap_input_text(&display, area.width, Some(display_cursor), FG);

    let visible = area.height;
    let cursor_row = cursor_pos.map(|(r, _)| r).unwrap_or(0);
    let scroll: u16 = if cursor_row >= visible {
        cursor_row - visible + 1
    } else {
        0
    };

    let widget = Paragraph::new(input_lines)
        .style(Style::default().bg(INPUT_BG))
        .scroll((scroll, 0));
    frame.render_widget(widget, area);

    let prefix_width: u16 = 2;
    let (vrow, vcol) = cursor_pos.unwrap_or((0, 0));
    let cx = area.x + prefix_width + vcol;
    let cy = area.y + vrow.saturating_sub(scroll);
    frame.set_cursor_position((cx, cy));
}

fn is_compact_tool(item: &ActivityItem) -> bool {
    match item {
        ActivityItem::Tool(e) => !e.expanded,
        _ => false,
    }
}

fn render_activity(frame: &mut Frame, app: &mut App, area: Rect) {
    let w = area.width;
    let n = app.feed.items.len();

    // sync cache length with activity list
    app.feed.render_cache.truncate(n);
    while app.feed.render_cache.len() < n {
        app.feed.render_cache.push(CachedRender::default());
    }

    // re-render only stale items
    for idx in 0..n {
        let (content_len, expanded, status_tag) = match &app.feed.items[idx] {
            ActivityItem::Thinking(t) => (t.len(), false, 0u8),
            ActivityItem::Text(t) => (t.len(), false, 0u8),
            ActivityItem::UserMessage(t) => (t.len(), false, 0u8),
            ActivityItem::System(_, t) => (t.len(), false, 0u8),
            ActivityItem::Compact(CompactStatus::Running) => (app.spin_frame as usize, false, 0u8),
            ActivityItem::Compact(CompactStatus::Done(_)) => (0, false, 1u8),
            ActivityItem::Compact(CompactStatus::Cancelled) => (0, false, 2u8),
            ActivityItem::Tool(e) => {
                let st = match &e.status {
                    ToolStatus::Running => 0,
                    ToolStatus::Complete { .. } => 1,
                    ToolStatus::Error(_) => 2,
                };
                let len = e.arg.len()
                    + e.output.as_ref().map_or(0, |o| o.len())
                    + match &e.status {
                        // include elapsed seconds so the timer re-renders
                        ToolStatus::Running => e.started_at.elapsed().as_secs() as usize,
                        ToolStatus::Error(s) => s.len(),
                        _ => 0,
                    };
                (len, e.expanded, st)
            }
        };

        let stale = {
            let c = &app.feed.render_cache[idx];
            c.content_len != content_len
                || c.width != w
                || c.expanded != expanded
                || c.status_tag != status_tag
        };

        if stale {
            let item_lines = render_activity_item(&app.feed.items[idx], w, app.spin_frame);
            app.feed.render_cache[idx] = CachedRender {
                lines: item_lines,
                content_len,
                width: w,
                expanded,
                status_tag,
            };
        }
    }

    // compute total line count including inter-item spacing.
    // collapsed tool entries stack without blank lines between them.
    let mut total: usize = 0;
    for idx in 0..n {
        if idx > 0 {
            let both_collapsed_tools =
                is_compact_tool(&app.feed.items[idx - 1]) && is_compact_tool(&app.feed.items[idx]);
            if !both_collapsed_tools {
                total += 1;
            }
        }
        total += app.feed.render_cache[idx].lines.len();
    }

    let show_waiting = app.is_running && total == 0;
    if show_waiting {
        total = 1;
    }

    let total_lines = total as u16;
    let max_scroll = total_lines.saturating_sub(area.height);

    // re-engage auto-scroll when manual scrolling reaches the bottom
    if !app.feed.auto_scroll && app.feed.scroll_offset >= max_scroll {
        app.feed.scroll_offset = max_scroll;
        app.feed.auto_scroll = true;
    }

    let scroll = if app.feed.auto_scroll {
        app.feed.scroll_offset = max_scroll;
        max_scroll
    } else {
        app.feed.scroll_offset
    };

    // only build the visible window of lines
    let vp_start = scroll as usize;
    let vp_end = vp_start + area.height as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);

    if show_waiting {
        if vp_start == 0 {
            lines.push(Line::from(Span::styled(
                "  waiting...",
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            )));
        }
    } else {
        let mut cursor: usize = 0;
        for idx in 0..n {
            if cursor >= vp_end {
                break;
            }

            // blank line between items, except between consecutive collapsed tools
            if idx > 0 {
                let both_collapsed_tools =
                    is_compact_tool(&app.feed.items[idx - 1]) && is_compact_tool(&app.feed.items[idx]);
                if !both_collapsed_tools {
                    if cursor >= vp_start {
                        lines.push(Line::from(""));
                    }
                    cursor += 1;
                }
            }

            // item lines
            for line in &app.feed.render_cache[idx].lines {
                if cursor >= vp_start && cursor < vp_end {
                    lines.push(line.clone());
                }
                cursor += 1;
                if cursor >= vp_end {
                    break;
                }
            }
        }
    }

    let activity = Paragraph::new(lines).style(Style::default().bg(BG));
    frame.render_widget(activity, area);
}

// render a single activity item into lines (no inter-item spacing)
fn render_activity_item(item: &ActivityItem, w: u16, spin_frame: u64) -> Vec<Line<'static>> {
    match item {
        ActivityItem::Thinking(text) => {
            let para = last_paragraph(text);
            let style = Style::default().fg(DIM).add_modifier(Modifier::ITALIC);
            wrap_text_with_bar(para, w, style)
        }
        ActivityItem::Text(text) => {
            let mut md = crate::markdown::TuiMarkdownRenderer::new();
            let md_lines = md.render_lines(text);
            wrap_md_lines_with_bar(md_lines, w)
        }
        ActivityItem::UserMessage(text) => render_user_message(text, w),
        ActivityItem::Tool(entry) => {
            let mut lines = Vec::new();
            render_tool_entry(&mut lines, entry, w);
            lines
        }
        ActivityItem::System(kind, text) => render_system_msg(kind, text),
        ActivityItem::Compact(status) => render_compact_item(status, spin_frame),
    }
}

fn render_user_message(text: &str, w: u16) -> Vec<Line<'static>> {
    let content_width = w.saturating_sub(2) as usize;
    if content_width == 0 {
        return vec![];
    }
    let style = Style::default().fg(FG);
    let prefix_style = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for logical_line in text.split('\n') {
        if logical_line.is_empty() {
            let pfx = if lines.is_empty() { "\u{203a} " } else { "  " };
            lines.push(Line::from(Span::styled(pfx.to_string(), prefix_style)));
            continue;
        }
        let chars: Vec<char> = logical_line.chars().collect();
        let mut start = 0;
        while start < chars.len() {
            let pfx = if lines.is_empty() { "\u{203a} " } else { "  " };
            let mut w_used = 0usize;
            let mut end = start;
            while end < chars.len() {
                let cw = unicode_width::UnicodeWidthChar::width(chars[end]).unwrap_or(0);
                if w_used + cw > content_width {
                    break;
                }
                w_used += cw;
                end += 1;
            }
            if end == start {
                end = start + 1;
            }
            let chunk: String = chars[start..end].iter().collect();
            lines.push(Line::from(vec![
                Span::styled(pfx.to_string(), prefix_style),
                Span::styled(chunk, style),
            ]));
            start = end;
        }
    }
    lines
}

fn render_system_msg(kind: &SystemKind, text: &str) -> Vec<Line<'static>> {
    let (icon, color, bold) = match kind {
        SystemKind::Info => ("  ", MUTED, false),
        SystemKind::Success => ("  ✓ ", GREEN, false),
        SystemKind::Warning => ("  ⚠ ", YELLOW, false),
        SystemKind::Error => ("  ✗ ", RED, false),
        SystemKind::Update => ("  ↑ ", ACCENT, true),
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let prefix = if i == 0 { icon } else { "    " };
        let mut style = Style::default().fg(color);
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(line.to_string(), style),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

// comet scan: a bright head with a gradient tail bouncing across a fixed-width field.
// returns a fixed 10-char string, colored separately by the caller.
fn compact_comet_frame(spin: u64) -> String {
    const FIELD: usize = 10;
    const PERIOD: usize = (FIELD - 1) * 2; // 18-frame full bounce
    const TAIL: [char; 3] = ['▓', '▒', '░'];

    let t = (spin / 4) as usize % PERIOD;
    let going_right = t < FIELD;
    let pos = if going_right { t } else { PERIOD - t };

    let mut field = [' '; FIELD];
    field[pos] = '█';
    for (i, &tc) in TAIL.iter().enumerate() {
        let trail = if going_right {
            pos.checked_sub(i + 1)
        } else {
            (pos + i + 1 < FIELD).then_some(pos + i + 1)
        };
        if let Some(p) = trail {
            field[p] = tc;
        }
    }
    field.iter().collect()
}

fn render_compact_item(status: &CompactStatus, spin: u64) -> Vec<Line<'static>> {
    match status {
        CompactStatus::Running => {
            let anim = compact_comet_frame(spin);
            vec![Line::from(vec![
                Span::styled("  ◈  ", Style::default().fg(ACCENT)),
                Span::styled("compacting context  ", Style::default().fg(FG)),
                Span::styled("[", Style::default().fg(DIM)),
                Span::styled(anim, Style::default().fg(THINKING_COLOR)),
                Span::styled("]", Style::default().fg(DIM)),
            ])]
        }
        CompactStatus::Done(msg) => {
            vec![Line::from(vec![
                Span::styled("  ✓  ", Style::default().fg(GREEN)),
                Span::styled(msg.clone(), Style::default().fg(FG)),
            ])]
        }
        CompactStatus::Cancelled => {
            vec![Line::from(vec![
                Span::styled("  ✕  ", Style::default().fg(MUTED)),
                Span::styled("compaction cancelled", Style::default().fg(MUTED)),
            ])]
        }
    }
}

fn render_tool_entry(lines: &mut Vec<Line<'static>>, entry: &ToolEntry, _w: u16) {
    let label = capitalize_tool(&entry.name);
    let display_arg = if entry.expanded {
        entry.arg.clone()
    } else if entry.arg.len() > 80 {
        format!("{}…", &entry.arg[..79])
    } else {
        entry.arg.clone()
    };

    match &entry.status {
        ToolStatus::Running => {
            let elapsed = entry.started_at.elapsed();
            let secs = elapsed.as_secs();

            let mut spans = vec![
                Span::styled("\u{25cc} ", Style::default().fg(YELLOW)),
                Span::styled(label.to_string(), Style::default().fg(YELLOW)),
            ];
            if secs >= 5 {
                let timer = if secs >= 60 {
                    format!("{}m{}s", secs / 60, secs % 60)
                } else {
                    format!("{}s", secs)
                };
                spans.push(Span::styled(
                    format!(" {}", timer),
                    Style::default().fg(DIM),
                ));
            }
            if !display_arg.is_empty() {
                spans.push(Span::styled(
                    format!(" {}", display_arg),
                    Style::default().fg(MUTED),
                ));
            }
            lines.push(tool_line(spans));

            // show streaming output while running, respecting the expanded toggle
            if entry.expanded {
                if let Some(ref output) = entry.output {
                    let out_lines: Vec<&str> = output.lines().collect();
                    let start = out_lines.len().saturating_sub(8);
                    for ol in &out_lines[start..] {
                        lines.push(tool_line(vec![
                            Span::styled("    ", Style::default()),
                            Span::styled((*ol).to_string(), Style::default().fg(DIM)),
                        ]));
                    }
                }
            }
        }
        ToolStatus::Complete { exit_code } => {
            let mut spans = vec![];

            // success/failure indicator — bash shows exit code, others just a checkmark
            if entry.name == "bash" {
                match exit_code {
                    Some(0) | None => {
                        spans.push(Span::styled("\u{2713} ", Style::default().fg(GREEN)));
                    }
                    Some(code) => {
                        spans.push(Span::styled(
                            format!("\u{2717}({}) ", code),
                            Style::default().fg(RED),
                        ));
                    }
                }
            } else {
                spans.push(Span::styled("\u{2713} ", Style::default().fg(GREEN)));
            }

            // tool name in accent, argument in muted
            spans.push(Span::styled(label.to_string(), Style::default().fg(ACCENT)));
            if !display_arg.is_empty() {
                spans.push(Span::styled(
                    format!(" {}", display_arg),
                    Style::default().fg(MUTED),
                ));
            }

            // diff stats
            if let Some(ref diff) = entry.diff {
                if diff.stat.additions > 0 {
                    spans.push(Span::styled(
                        format!(" +{}", diff.stat.additions),
                        Style::default().fg(GREEN),
                    ));
                }
                if diff.stat.deletions > 0 {
                    spans.push(Span::styled(
                        format!(" -{}", diff.stat.deletions),
                        Style::default().fg(RED),
                    ));
                }
            }

            lines.push(tool_line(spans));

            if entry.expanded {
                // tool output (first few lines, indented)
                if let Some(ref output) = entry.output {
                    let display = strip_exit_prefix(output);
                    let out_lines: Vec<&str> = display.lines().take(8).collect();
                    for ol in &out_lines {
                        lines.push(tool_line(vec![
                            Span::styled("    ", Style::default()),
                            Span::styled((*ol).to_string(), Style::default().fg(DIM)),
                        ]));
                    }
                    let total_lines = display.lines().count();
                    if total_lines > 8 {
                        lines.push(tool_line(vec![Span::styled(
                            format!("    ...{} more lines", total_lines - 8),
                            Style::default().fg(DIM),
                        )]));
                    }
                }

                // diff lines
                if let Some(ref diff) = entry.diff {
                    lines.extend(build_diff_lines(diff));
                }
            }
        }
        ToolStatus::Error(e) => {
            let short = if e.len() > 80 {
                format!("{}...", &e[..77])
            } else {
                e.clone()
            };
            lines.push(tool_line(vec![
                Span::styled("\u{2717} ", Style::default().fg(RED)),
                Span::styled(
                    format!("{} ", label),
                    Style::default().fg(RED).add_modifier(Modifier::BOLD),
                ),
                Span::styled(short, Style::default().fg(RED)),
            ]));
        }
    }
}

// render a sparkline where each bar is colored by the dominant token type
// in that bucket. colors: text=accent, thinking=purple, tool=cyan, input=blue.
fn render_colored_sparkline(samples: &[TokenBucket], width: usize) -> Vec<Span<'static>> {
    let blocks = [
        ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
        '\u{2588}',
    ];

    if samples.is_empty() {
        return vec![Span::styled(" ".repeat(width), Style::default())];
    }

    let start = samples.len().saturating_sub(width);
    let window = &samples[start..];
    let max = window.iter().map(|b| b.total()).max().unwrap_or(1).max(1);

    let mut spans: Vec<Span<'static>> = Vec::new();

    // leading padding
    let pad_count = width.saturating_sub(window.len());
    if pad_count > 0 {
        spans.push(Span::styled(" ".repeat(pad_count), Style::default()));
    }

    // batch consecutive chars with the same color into a single span
    let mut run_color: Option<Color> = None;
    let mut run_chars = String::new();

    for bucket in window {
        let total = bucket.total();
        let idx = ((total as f64 / max as f64) * 8.0).round() as usize;
        let ch = blocks[idx.min(8)];

        // pick color from the dominant token type
        let color = if total == 0 {
            DIM
        } else {
            dominant_color(bucket)
        };

        if run_color == Some(color) {
            run_chars.push(ch);
        } else {
            if !run_chars.is_empty() {
                spans.push(Span::styled(
                    run_chars.clone(),
                    Style::default().fg(run_color.unwrap_or(DIM)),
                ));
            }
            run_chars.clear();
            run_chars.push(ch);
            run_color = Some(color);
        }
    }
    if !run_chars.is_empty() {
        spans.push(Span::styled(
            run_chars,
            Style::default().fg(run_color.unwrap_or(DIM)),
        ));
    }

    spans
}

// pick the color of whichever token type contributed the most to this bucket
fn dominant_color(bucket: &TokenBucket) -> Color {
    let pairs = [
        (bucket.text, ACCENT),
        (bucket.thinking, THINKING_COLOR),
        (bucket.tool, TOOL_COLOR),
    ];
    pairs
        .iter()
        .max_by_key(|(count, _)| *count)
        .map(|(_, color)| *color)
        .unwrap_or(ACCENT)
}

fn build_diff_lines(diff: &DiffInfo) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for hunk in &diff.hunks {
        for dl in &hunk.lines {
            let (prefix, color) = match dl.tag {
                DiffLineTag::Equal => (" ", DIM),
                DiffLineTag::Insert => ("+", GREEN),
                DiffLineTag::Delete => ("-", RED),
            };
            let content = dl.content.trim_end_matches('\n').to_string();
            out.push(tool_line(vec![
                Span::styled(format!("  {}", prefix), Style::default().fg(color)),
                Span::styled(content, Style::default().fg(color)),
            ]));
        }
    }
    out
}

