use super::{
    app::{App, Focus, ViewMode, ViewTab},
    live::LiveStatus,
    sessions::{ProjectSessions, SessionStatus},
};
use crate::{analyzer::analyzer::AgentReport, format_tokens};
use chrono::{Local, Utc};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, List, ListItem, ListState, Padding, Paragraph,
        Row, Table, TableState, Wrap,
    },
};

const CONTEXT_LIMIT: u64 = 1_000_000;
const CONTEXT_WARN: u64 = 900_000;

fn context_fill(tokens: u64) -> usize {
    (tokens.min(CONTEXT_LIMIT) as usize * 8 / CONTEXT_LIMIT as usize).min(8)
}

fn panel(title: &'static str, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }))
        .padding(Padding::horizontal(1))
        .title(title)
}

fn key_hints(hints: &[(&'static str, &'static str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, description)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            *key,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {description}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

fn short_agent_id(agent_id: &str) -> String {
    agent_id
        .strip_prefix("agent-")
        .unwrap_or(agent_id)
        .chars()
        .take(7)
        .collect()
}

fn split_slash_prompt(prompt: &str) -> Option<(&str, &str)> {
    let (command, description) = prompt.strip_prefix('[')?.split_once(']')?;
    command
        .starts_with('/')
        .then_some((command, description.trim_start()))
}

fn prompt_spans(prompt: &str) -> Vec<Span<'static>> {
    if let Some((command, description)) = split_slash_prompt(prompt) {
        let mut spans = vec![Span::styled(
            format!("[{command}]"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )];
        if !description.is_empty() {
            spans.push(Span::styled(
                format!(" {description}"),
                Style::default().fg(Color::White),
            ));
        }
        spans
    } else {
        vec![Span::styled(
            prompt.to_string(),
            Style::default().fg(Color::White),
        )]
    }
}

fn command_label(prompt: &str) -> &str {
    if let Some((command, _)) = split_slash_prompt(prompt) {
        command
    } else {
        prompt
            .split_whitespace()
            .next()
            .filter(|word| word.starts_with('/'))
            .unwrap_or("-")
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    if app.selector_open {
        draw_session_selector(frame, app);
        if app.quit_confirm_open {
            let area = frame.area();
            draw_quit_confirmation(frame, area);
        }
        return;
    }
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(1),
            Constraint::Length(4),
        ])
        .split(frame.area());
    draw_header(frame, app, areas[0]);

    match app.mode {
        ViewMode::Compact => draw_compact(frame, app, areas[1]),
        ViewMode::Diagnostic => {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
                .split(areas[1]);
            draw_tree(frame, app, body[0]);
            draw_detail(frame, app, body[1]);
        }
    }

    let footer = vec![
        key_hints(&[
            ("q/Esc", "Quit"),
            ("s", "Sessions"),
            ("v", "View"),
            ("h/l", "Focus"),
            ("j/k", "Move/Scroll"),
            ("Tab/1-5", "Tabs"),
            ("PgUp/PgDn", "Page"),
            ("r", "Reload"),
            ("y", "Copy"),
        ]),
        Line::from(Span::styled(
            app.status.clone(),
            Style::default().fg(Color::Yellow),
        )),
    ];
    frame.render_widget(Paragraph::new(footer).block(panel("", false)), areas[2]);
    if app.quit_confirm_open {
        let area = frame.area();
        draw_quit_confirmation(frame, area);
    }
}

fn draw_quit_confirmation(frame: &mut Frame, area: Rect) {
    let width = 40.min(area.width);
    let height = 7.min(area.height);
    let modal = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let lines = vec![
        Line::default(),
        Line::from(Span::styled(
            "确定要退出 spec-xray 吗？",
            Style::default().fg(Color::White),
        )),
        Line::default(),
        Line::from(vec![
            Span::styled(
                "[Enter / y]",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" 确认退出"),
        ]),
        Line::from(vec![
            Span::styled(
                "[Esc / n]",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("   取消返回"),
        ]),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow))
        .padding(Padding::horizontal(1))
        .title("确认退出");
    frame.render_widget(Clear, modal);
    frame.render_widget(Paragraph::new(lines).block(block), modal);
}

fn draw_session_selector(frame: &mut Frame, app: &mut App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(15),
            Constraint::Min(1),
            Constraint::Length(4),
        ])
        .split(frame.area());
    draw_project_summary(frame, app.sessions.as_ref(), areas[0]);
    app.selector_height = areas[1].height.saturating_sub(3);

    let now = Utc::now();
    let empty = Vec::new();
    let sessions = app
        .sessions
        .as_ref()
        .map_or(&empty, |catalog| &catalog.sessions);
    let rows = sessions.iter().map(|session| {
        let status = session.status(now);
        Row::new(vec![
            Cell::from(if session.id == app.report.session_id {
                "●"
            } else {
                ""
            }),
            Cell::from(session.time_range(now)),
            Cell::from(format!("{:>5}轮", session.turns)),
            Cell::from(format!("{:>4}", session.agents)),
            Cell::from(Span::styled(
                format!("{:>7}", format_tokens(session.tokens)),
                Style::default().fg(Color::Cyan),
            )),
            Cell::from(Span::styled(status.label(), session_status_style(status))),
            Cell::from(Line::from(prompt_spans(&session.prompt))),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(40),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Min(8),
        ],
    )
    .header(
        Row::new([
            "",
            "时间段",
            "轮数",
            "Agents",
            "Token",
            "状态",
            "首条 Prompt 摘要",
        ])
        .style(
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .column_spacing(1)
    .block(panel("Sessions — newest first", true))
    .row_highlight_style(
        Style::default()
            .bg(Color::Indexed(236))
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ");
    let selected = (!sessions.is_empty()).then_some(app.session_selected);
    let mut state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(table, areas[1], &mut state);

    let footer = vec![
        key_hints(&[
            ("s/Esc", "Back"),
            ("Enter", "Load session"),
            ("j/k", "Move"),
            ("PgUp/PgDn", "Page"),
            ("r", "Rescan"),
            ("q", "Quit"),
        ]),
        Line::from(Span::styled(
            app.status.clone(),
            Style::default().fg(Color::Yellow),
        )),
    ];
    frame.render_widget(Paragraph::new(footer).block(panel("", false)), areas[2]);
}

fn draw_project_summary(frame: &mut Frame, catalog: Option<&ProjectSessions>, area: Rect) {
    let now = Utc::now();
    let mut lines = Vec::new();
    if let Some(catalog) = catalog {
        let count = catalog.sessions.len();
        let total = catalog.total_tokens();
        let average = if count == 0 { 0 } else { total / count as u64 };
        lines.push(Line::from(format!(
            "Sessions: {count}  |  Total: {}  |  Avg: {}  |  Subagents: {}",
            format_tokens(total),
            format_tokens(average),
            catalog.dispatched()
        )));
        lines.push(Line::from(format!(
            "Build/tool failures: {} ({:.1}%)  |  Compactions: {} ({:.1}%)",
            catalog.failed_count(),
            percentage(catalog.failed_count(), count),
            catalog.compacted_count(),
            percentage(catalog.compacted_count(), count)
        )));
        lines.push(Line::from(
            "Recent 10 sessions (newest → oldest):  ! failure  ~ warning",
        ));
        let recent: Vec<_> = catalog
            .sessions
            .iter()
            .filter(|session| !session.is_empty())
            .take(10)
            .collect();
        let max = recent
            .iter()
            .map(|session| session.tokens)
            .max()
            .unwrap_or(0);
        for session in recent {
            let filled = if max == 0 || session.tokens == 0 {
                0
            } else {
                ((session.tokens as f64 * 20.0 / max as f64).round() as usize).max(1)
            };
            let status = session.status(now);
            let marker = if session.failed {
                Span::styled("!", Style::default().fg(Color::Red))
            } else if session.had_error || session.compacted {
                Span::styled("~", Style::default().fg(Color::Yellow))
            } else {
                Span::raw(" ")
            };
            lines.push(Line::from(vec![
                Span::styled("█".repeat(filled), Style::default().fg(Color::Cyan)),
                Span::raw(" ".repeat(20 - filled)),
                Span::styled(
                    format!(" {:>7} ", format_tokens(session.tokens)),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!("{} ", session.start.with_timezone(&Local).format("%m-%d")),
                    Style::default().fg(Color::DarkGray),
                ),
                marker,
                Span::styled(
                    format!(" {} ", status.label()),
                    session_status_style(status),
                ),
                Span::styled(
                    command_label(&session.prompt),
                    Style::default().fg(Color::Cyan),
                ),
            ]));
        }
        if count == 0 {
            lines.push(muted("No sessions found."));
        }
    } else {
        lines.push(muted("No project metadata. Press r to rescan."));
    }
    frame.render_widget(
        Paragraph::new(lines).block(panel("Project overview — all historical sessions", false)),
        area,
    );
}

fn percentage(part: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 * 100.0 / total as f64
    }
}

fn session_status_style(status: SessionStatus) -> Style {
    let color = match status {
        SessionStatus::Live => Color::Cyan,
        SessionStatus::Fail => Color::Red,
        SessionStatus::Warn => Color::Yellow,
        SessionStatus::Ok => Color::Green,
        SessionStatus::Empty => Color::DarkGray,
    };
    let style = Style::default().fg(color);
    if status == SessionStatus::Empty {
        style
    } else {
        style.add_modifier(Modifier::BOLD)
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let total: u64 = app
        .report
        .agents
        .iter()
        .map(|agent| agent.total_tokens)
        .sum();
    let cold: u64 = app
        .report
        .agents
        .iter()
        .map(|agent| agent.startup_cache_tokens)
        .sum();
    let cold_pct = if total == 0 {
        0.0
    } else {
        cold as f64 * 100.0 / total as f64
    };
    let live = if app.live {
        Span::styled(
            "[LIVE]",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("[PAUSED]", Style::default().fg(Color::Red))
    };
    let session = Line::from(vec![
        Span::raw(format!("Session: {}  |  ", app.report.session_id)),
        live,
        Span::raw(format!("  |  Agents: {}", app.report.agents.len())),
    ]);
    let stats = Line::from(vec![
        status_badge(LiveStatus::Running, app.status_count(LiveStatus::Running)),
        Span::raw(" "),
        status_badge(LiveStatus::Queued, app.status_count(LiveStatus::Queued)),
        Span::raw(" "),
        status_badge(
            LiveStatus::Completed,
            app.status_count(LiveStatus::Completed),
        ),
        Span::raw(" "),
        status_badge(LiveStatus::Failed, app.status_count(LiveStatus::Failed)),
        Span::raw("  Tokens: "),
        Span::styled(format_tokens(total), Style::default().fg(Color::Cyan)),
        Span::raw("  |  Cold: "),
        Span::styled(
            format!("{} ({cold_pct:.1}%)", format_tokens(cold)),
            Style::default().fg(Color::Cyan),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(vec![session, stats]).block(panel("spec-xray", false)),
        area,
    );
}

fn status_style(status: LiveStatus) -> Style {
    let color = match status {
        LiveStatus::Queued => Color::DarkGray,
        LiveStatus::Running => Color::Yellow,
        LiveStatus::Completed => Color::Green,
        LiveStatus::Failed => Color::LightRed,
    };
    let style = Style::default().fg(color);
    if status == LiveStatus::Failed {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

fn status_span(status: LiveStatus) -> Span<'static> {
    Span::styled(status.label(), status_style(status))
}

fn status_badge(status: LiveStatus, count: usize) -> Span<'static> {
    let style = match status {
        LiveStatus::Running if count > 0 => Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        LiveStatus::Completed if count > 0 => {
            Style::default().fg(Color::Green).bg(Color::Indexed(236))
        }
        LiveStatus::Failed if count > 0 => Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD),
        LiveStatus::Queued if count > 0 => Style::default().bg(Color::Indexed(236)),
        _ => Style::default().fg(Color::DarkGray),
    };
    Span::styled(format!(" {}: {count} ", status.name()), style)
}

fn draw_compact(frame: &mut Frame, app: &mut App, area: Rect) {
    app.right_height = area.height.saturating_sub(3);
    let header = Row::new([
        "Main / Subagent (Ticket)",
        "Status",
        "Turns",
        "Context",
        "Tokens",
        "Action / elapsed",
    ])
    .style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    let rows = app.rows.iter().map(|row| {
        let mut task = vec![
            Span::styled(row.prefix.clone(), Style::default().fg(Color::DarkGray)),
            Span::styled(row.label.clone(), Style::default().fg(Color::White)),
            Span::styled(
                format!(" ({})", short_agent_id(&row.agent_id)),
                Style::default().fg(Color::DarkGray),
            ),
        ];
        if row.compacted {
            task.push(Span::styled(" [COMPACT]", Style::default().fg(Color::Cyan)));
        }
        let filled = context_fill(row.context_tokens);
        let context_color = if row.context_tokens >= CONTEXT_WARN {
            Color::Rgb(255, 110, 70)
        } else {
            Color::Cyan
        };
        let context = Line::from(vec![
            Span::styled("█".repeat(filled), Style::default().fg(context_color)),
            Span::styled("░".repeat(8 - filled), Style::default().fg(Color::DarkGray)),
            Span::raw(format!(" {}/1M", format_tokens(row.context_tokens))),
        ]);
        Row::new(vec![
            Cell::from(Line::from(task)),
            Cell::from(status_span(row.status)),
            Cell::from(row.turns.to_string()),
            Cell::from(context),
            Cell::from(Span::styled(
                format_tokens(row.tokens),
                Style::default().fg(Color::Cyan),
            )),
            Cell::from(format!("{} ({}s)", row.action, row.action_secs)),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Min(26),
            Constraint::Length(10),
            Constraint::Length(7),
            Constraint::Length(23),
            Constraint::Length(12),
            Constraint::Min(24),
        ],
    )
    .header(header)
    .column_spacing(1)
    .block(panel("Compact live tree — v: diagnostics", true))
    .row_highlight_style(
        Style::default()
            .bg(Color::Indexed(236))
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ");
    let selected = (!app.rows.is_empty()).then_some(app.selected);
    let mut state = TableState::default().with_selected(selected);
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_tree(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Tree;
    let items = app.rows.iter().map(|row| {
        let mut spans = vec![
            Span::styled(row.prefix.clone(), Style::default().fg(Color::DarkGray)),
            Span::styled(
                short_agent_id(&row.agent_id),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(" "),
            Span::styled(row.label.clone(), Style::default().fg(Color::White)),
            Span::raw(" "),
            Span::styled(format_tokens(row.tokens), Style::default().fg(Color::Cyan)),
            Span::raw(" "),
            status_span(row.status),
        ];
        if row.compacted {
            spans.push(Span::styled(" [COMPACT]", Style::default().fg(Color::Cyan)));
        }
        ListItem::new(Line::from(spans))
    });
    let list = List::new(items)
        .block(panel("Subagent topology", focused))
        .highlight_style(
            Style::default()
                .bg(Color::Indexed(236))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut state = ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Detail;
    let block = panel("Diagnostics", focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    let mut tabs = Vec::new();
    for (index, (tab, title)) in ViewTab::ALL
        .into_iter()
        .zip(["1 Radar", "2 Merge", "3 Timeline", "4 Report", "5 Gantt"])
        .enumerate()
    {
        if index > 0 {
            tabs.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        }
        let style = if tab == app.tab {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        tabs.push(Span::styled(format!(" {title} "), style));
    }
    frame.render_widget(Paragraph::new(Line::from(tabs)), sections[0]);

    let lines = detail_lines(app, sections[1].width as usize);
    app.right_len = lines.len();
    app.right_height = sections[1].height;
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((app.right_scroll, 0))
            .wrap(Wrap { trim: false }),
        sections[1],
    );
}

fn detail_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    match app.tab {
        ViewTab::Radar => app
            .selected_agent()
            .map(radar_lines)
            .unwrap_or_else(|| empty("No agent selected.")),
        ViewTab::Merge => merge_lines(app),
        ViewTab::Timeline => app
            .selected_agent()
            .map(timeline_lines)
            .unwrap_or_else(|| empty("No agent selected.")),
        ViewTab::Report => app
            .markdown
            .lines()
            .map(|line| Line::from(line.to_string()))
            .collect(),
        ViewTab::Gantt => {
            let track_width = width.saturating_sub(52).clamp(10, 60);
            crate::gantt::render_gantt(&app.topology, track_width)
                .lines()
                .map(|line| Line::from(line.to_string()))
                .collect()
        }
    }
}

fn radar_lines(agent: &AgentReport) -> Vec<Line<'static>> {
    let mut lines = vec![
        heading(format!(
            "Attribution & bloat — {} {}",
            agent.agent_id,
            agent.task_desc.as_deref().unwrap_or("")
        )),
        heading("Tool token deltas"),
    ];
    let mut tools: Vec<_> = agent.tool_token_delta.iter().collect();
    tools.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let total: u64 = tools.iter().map(|(_, tokens)| **tokens).sum();
    if tools.is_empty() {
        lines.push(muted("No tool-attributed context growth."));
    }
    for (tool, tokens) in tools {
        let percent = if total == 0 {
            0.0
        } else {
            *tokens as f64 * 100.0 / total as f64
        };
        let filled = (percent * 0.2).round() as usize;
        let name: String = tool.chars().take(12).collect();
        let delta = format!("+{}", format_tokens(*tokens));
        lines.push(Line::from(vec![
            Span::styled(format!("{name:<12}"), Style::default().fg(Color::White)),
            Span::styled(format!("{delta:>9}"), Style::default().fg(Color::Cyan)),
            Span::raw(format!("  {percent:>5.1}%  ")),
            Span::styled("█".repeat(filled), Style::default().fg(Color::Cyan)),
            Span::styled(
                "░".repeat(20 - filled),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    lines.push(Line::default());
    lines.push(heading("Compaction boundaries"));
    if agent.compactions.is_empty() {
        lines.push(muted("No compactions recorded."));
    }
    lines.extend(agent.compactions.iter().map(|compact| {
        Line::from(match compact.after_tokens {
            Some(after) => format!(
                "Turn {}  {} → {} tokens (Δ {})",
                compact.turn,
                format_tokens(compact.before_tokens),
                format_tokens(after),
                format_tokens(i128::from(compact.before_tokens) - i128::from(after))
            ),
            None => format!(
                "Turn {}  {} → unknown tokens",
                compact.turn,
                format_tokens(compact.before_tokens)
            ),
        })
    }));

    lines.push(Line::default());
    lines.push(heading("Repeated full Reads"));
    let mut reads: Vec<_> = agent
        .full_read_counts
        .iter()
        .filter(|(_, count)| **count >= 2)
        .collect();
    reads.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    if reads.is_empty() {
        lines.push(muted("No repeated unrestricted Reads."));
    }
    lines.extend(
        reads
            .into_iter()
            .map(|(path, count)| Line::from(format!("{count}×  {path}"))),
    );

    lines.push(Line::default());
    lines.push(heading("Top 10 single-step surges"));
    if agent.heavy_ops.is_empty() {
        lines.push(muted("No operation exceeded the configured threshold."));
    }
    lines.extend(agent.heavy_ops.iter().take(10).map(|(operation, delta)| {
        Line::from(vec![
            Span::styled(
                format!("+{:<9}", format_tokens(*delta)),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(format!(" {operation}")),
        ])
    }));
    lines
}

fn merge_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![heading("Merge audits")];
    if app.report.merge_audits.is_empty() {
        lines.push(muted("No successful git merge actions recorded."));
    }
    for audit in &app.report.merge_audits {
        lines.push(Line::from(format!(
            "{}  |  {}",
            audit.agent_id, audit.action
        )));
        let (result, color) = match audit.build_success {
            Some(true) => ("First build/test: OK", Color::Green),
            Some(false) => ("First build/test: ERR", Color::Red),
            None => ("First build/test: PENDING", Color::Yellow),
        };
        lines.push(Line::from(Span::styled(result, Style::default().fg(color))));
        if let Some(error) = &audit.build_error {
            lines.push(Line::from(format!("  {error}")));
        }
        if audit.suspect_files.is_empty() {
            lines.push(muted("  Suspect files: none recorded before merge."));
        }
        for file in &audit.suspect_files {
            let origin = app
                .report
                .file_clashes
                .get(file)
                .map(|editors| editors.join(", "))
                .unwrap_or_else(|| format!("only {}", audit.agent_id));
            lines.push(Line::from(format!("  {file}  ←  {origin}")));
        }
        lines.push(Line::default());
    }

    lines.push(heading("Cross-agent file clash matrix"));
    if app.report.file_clashes.is_empty() {
        lines.push(muted("No files modified by multiple agents."));
        return lines;
    }
    let agents: Vec<_> = app
        .report
        .agents
        .iter()
        .filter(|agent| {
            agent
                .edited_files
                .keys()
                .any(|file| app.report.file_clashes.contains_key(file))
        })
        .collect();
    for (index, agent) in agents.iter().enumerate() {
        lines.push(Line::from(format!(
            "A{:<2} = {} {}",
            index + 1,
            agent.agent_id,
            agent.task_desc.as_deref().unwrap_or("")
        )));
    }
    lines.push(Line::from(format!(
        "File | {}",
        (1..=agents.len())
            .map(|index| format!("A{index}"))
            .collect::<Vec<_>>()
            .join(" | ")
    )));
    for file in app.report.file_clashes.keys() {
        let marks = agents
            .iter()
            .map(|agent| {
                if agent.edited_files.contains_key(file) {
                    " ●"
                } else {
                    "  ·"
                }
            })
            .collect::<Vec<_>>()
            .join(" |");
        lines.push(Line::from(format!("{file} |{marks}")));
    }
    lines
}

fn timeline_lines(agent: &AgentReport) -> Vec<Line<'static>> {
    let mut lines = vec![
        heading(format!(
            "Action timeline — {} ({} calls)",
            agent.agent_id,
            agent.actions.len()
        )),
        Line::from("Turn | Tool | Target | +Delta | Error"),
    ];
    if agent.actions.is_empty() {
        lines.push(muted("No tool calls recorded."));
    }
    lines.extend(agent.actions.iter().map(|action| {
        let target: String = action
            .target
            .as_deref()
            .unwrap_or("-")
            .chars()
            .take(96)
            .collect();
        let error: String = action
            .error
            .as_deref()
            .unwrap_or("-")
            .chars()
            .take(120)
            .collect();
        let line = format!(
            "#{:<4} | {:<10} | {} | +{} | {}",
            action.turn,
            action.tool,
            target,
            format_tokens(action.delta),
            error
        );
        if action.error.is_some() {
            Line::from(Span::styled(line, Style::default().fg(Color::Red)))
        } else {
            Line::from(line)
        }
    }));
    lines
}

fn heading(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        text.into(),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ))
}

fn muted(text: &'static str) -> Line<'static> {
    Line::from(Span::styled(text, Style::default().fg(Color::Gray)))
}

fn empty(text: &'static str) -> Vec<Line<'static>> {
    vec![muted(text)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn project() -> PathBuf {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("spec-xray-view-{}-{id}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let line = json!({
            "message": {"id": "message-1", "role": "assistant", "content": [{
                "type": "tool_use", "id": "task-1", "name": "Agent",
                "input": {"description": "Render ticket"}
            }], "usage": {"input_tokens": 150_000}}
        });
        fs::write(directory.join("session.jsonl"), format!("{line}\n")).unwrap();
        fs::write(directory.join("other.jsonl"), "").unwrap();
        directory
    }

    #[test]
    fn renders_both_views_and_every_diagnostic_tab() {
        let project = project();
        let mut app = App::new(&project, Some("session"), 10_000).unwrap();
        app.live = true;
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = buffer_text(&terminal);
        assert!(rendered.contains("spec-xray"));
        assert!(rendered.contains("150.0k"));
        assert_eq!(terminal.backend().buffer().content()[0].symbol(), "╭");

        app.mode = ViewMode::Diagnostic;
        for tab in ViewTab::ALL {
            app.tab = tab;
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        }
        let rendered = buffer_text(&terminal);
        assert!(rendered.contains("3 Timeline"));
        assert!(rendered.contains("5 Gantt"));
        app.tab = ViewTab::Radar;
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('5'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(app.tab == ViewTab::Gantt);
        app.mode = ViewMode::Compact;
        terminal.backend_mut().resize(80, 20);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('s'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(app.selector_open);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.sessions.as_ref().unwrap().sessions.len(), 2);
        app.session_selected = app
            .sessions
            .as_ref()
            .unwrap()
            .sessions
            .iter()
            .position(|session| session.id == "other")
            .unwrap();
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(!app.selector_open);
        assert_eq!(app.report.session_id, "other");

        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('q'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(app.quit_confirm_open && !app.quit);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = buffer_text(&terminal);
        let compact: String = rendered
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        assert!(compact.contains("确认退出"), "{rendered}");
        assert!(compact.contains("确定要退出spec-xray吗？"), "{rendered}");
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('n'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(!app.quit_confirm_open && !app.quit);
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('q'),
            crossterm::event::KeyModifiers::NONE,
        ));
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('q'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(app.quit && !app.quit_confirm_open);
        let _ = fs::remove_dir_all(project);
    }

    #[test]
    fn slash_prompt_separates_badge_and_description() {
        let spans = prompt_spans("[/implement-spec] 实现选择器");
        assert_eq!(spans[0].content.as_ref(), "[/implement-spec]");
        assert_eq!(spans[0].style.fg, Some(Color::Cyan));
        assert_eq!(spans[1].content.as_ref(), " 实现选择器");
        assert_eq!(spans[1].style.fg, Some(Color::White));
        assert_eq!(
            command_label("[/implement-spec] 实现选择器"),
            "/implement-spec"
        );
    }

    #[test]
    fn context_progress_uses_one_million_token_limit() {
        assert_eq!(context_fill(0), 0);
        assert_eq!(context_fill(500_000), 4);
        assert_eq!(context_fill(1_000_000), 8);
        assert_eq!(context_fill(2_000_000), 8);
        assert_eq!(CONTEXT_WARN, 900_000);
    }
}
