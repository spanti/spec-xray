use super::app::{App, Focus, ViewTab};
use crate::analyzer::analyzer::AgentReport;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(4),
        ])
        .split(frame.area());
    draw_header(frame, app, areas[0]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(areas[1]);
    draw_tree(frame, app, body[0]);
    draw_detail(frame, app, body[1]);

    let footer = vec![
        Line::from(
            "q/Esc Quit  h/l Focus  j/k/↑/↓ Move/Scroll  Tab/1-4 View  PgUp/PgDn Page  r Refresh  y Copy",
        ),
        Line::from(Span::styled(
            app.status.clone(),
            Style::default().fg(Color::Yellow),
        )),
    ];
    frame.render_widget(
        Paragraph::new(footer).block(Block::default().borders(Borders::ALL)),
        areas[2],
    );
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
        Span::styled("[LIVE]", Style::default().fg(Color::Green))
    } else {
        Span::styled("[PAUSED]", Style::default().fg(Color::Red))
    };
    let line = Line::from(vec![
        Span::raw(format!(
            "Session: {}  |  Tokens: {} (cold {} / {:.1}%)  |  Agents: {}  |  ",
            app.report.session_id,
            total,
            cold,
            cold_pct,
            app.report.agents.len()
        )),
        live,
    ]);
    frame.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL).title("spec-xray")),
        area,
    );
}

fn draw_tree(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Tree;
    let items = app.rows.iter().map(|row| {
        let (status, color) = if row.loop_failures >= 3 {
            ("[LOOP!]", Color::Yellow)
        } else if row.has_errors {
            ("[ERR]", Color::Red)
        } else {
            ("[OK]", Color::Green)
        };
        let mut spans = vec![
            Span::raw(format!(
                "{}{} {} — {} tok ",
                row.prefix, row.agent_id, row.label, row.tokens
            )),
            Span::styled(
                status,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ];
        if row.compacted {
            spans.push(Span::styled(" [COMPACT]", Style::default().fg(Color::Cyan)));
        }
        ListItem::new(Line::from(spans))
    });
    let border = if focused { Color::Cyan } else { Color::Gray };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Subagent topology")
                .border_style(Style::default().fg(border)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("▶ ");
    let mut state = ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Detail;
    let border = if focused { Color::Cyan } else { Color::Gray };
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Diagnostics")
        .border_style(Style::default().fg(border));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    let mut tabs = Vec::new();
    for (index, (tab, title)) in ViewTab::ALL
        .into_iter()
        .zip(["1 Radar", "2 Merge/Clashes", "3 Timeline", "4 Report"])
        .enumerate()
    {
        if index > 0 {
            tabs.push(Span::raw("  "));
        }
        let style = if tab == app.tab {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        tabs.push(Span::styled(title, style));
    }
    frame.render_widget(Paragraph::new(Line::from(tabs)), sections[0]);

    let lines = detail_lines(app);
    app.right_len = lines.len();
    app.right_height = sections[1].height;
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((app.right_scroll, 0))
            .wrap(Wrap { trim: false }),
        sections[1],
    );
}

fn detail_lines(app: &App) -> Vec<Line<'static>> {
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
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(20 - filled));
        lines.push(Line::from(vec![
            Span::raw(format!("{tool:<12} +{tokens:<10} {percent:>5.1}%  ")),
            Span::styled(bar, Style::default().fg(Color::Cyan)),
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
                compact.before_tokens,
                after,
                i128::from(compact.before_tokens) - i128::from(after)
            ),
            None => format!(
                "Turn {}  {} → unknown tokens",
                compact.turn, compact.before_tokens
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
    lines.extend(
        agent
            .heavy_ops
            .iter()
            .take(10)
            .map(|(operation, delta)| Line::from(format!("+{delta:<10} {operation}"))),
    );
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
            action.turn, action.tool, target, action.delta, error
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
