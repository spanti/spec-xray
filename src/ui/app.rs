use crate::{
    analyzer::analyzer::{AgentReport, SessionReport, analyze_session, list_sessions},
    parser::tree::{SessionTopology, TopologyNode, build_topology},
    report::render_markdown,
};
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, TryRecvError, channel},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Detail,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewTab {
    Radar,
    Merge,
    Timeline,
    Report,
}

impl ViewTab {
    pub const ALL: [Self; 4] = [Self::Radar, Self::Merge, Self::Timeline, Self::Report];

    pub fn index(self) -> usize {
        match self {
            Self::Radar => 0,
            Self::Merge => 1,
            Self::Timeline => 2,
            Self::Report => 3,
        }
    }
}

pub struct TreeRow {
    pub agent_id: String,
    pub label: String,
    pub prefix: String,
    pub tokens: u64,
    pub has_errors: bool,
    pub loop_failures: usize,
    pub compacted: bool,
}

struct SessionData {
    report: SessionReport,
    markdown: String,
    rows: Vec<TreeRow>,
}

pub struct App {
    project_dir: PathBuf,
    threshold: u64,
    pub report: SessionReport,
    pub markdown: String,
    pub rows: Vec<TreeRow>,
    pub selected: usize,
    pub focus: Focus,
    pub tab: ViewTab,
    pub right_scroll: u16,
    pub right_len: usize,
    pub right_height: u16,
    pub live: bool,
    pub status: String,
    pub quit: bool,
    watcher: Option<RecommendedWatcher>,
    events: Option<Receiver<notify::Result<notify::Event>>>,
    dirty: bool,
    last_change: Instant,
}

impl App {
    pub fn new(project_dir: &Path, session_id: Option<&str>, threshold: u64) -> Result<Self> {
        let session_id = match session_id {
            Some(id) => id.to_string(),
            None => {
                list_sessions(project_dir)?
                    .into_iter()
                    .next()
                    .context("当前项目下没有任何会话日志")?
                    .0
            }
        };
        let data = load_data(project_dir, &session_id, threshold)?;
        Ok(Self {
            project_dir: project_dir.to_path_buf(),
            threshold,
            report: data.report,
            markdown: data.markdown,
            rows: data.rows,
            selected: 0,
            focus: Focus::Tree,
            tab: ViewTab::Radar,
            right_scroll: 0,
            right_len: 0,
            right_height: 0,
            live: false,
            status: String::new(),
            quit: false,
            watcher: None,
            events: None,
            dirty: false,
            last_change: Instant::now(),
        })
    }

    pub fn watch(&mut self) -> Result<()> {
        let (tx, rx) = channel();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let _ = tx.send(event);
            })?;
        watcher.watch(&self.project_dir, RecursiveMode::Recursive)?;
        self.watcher = Some(watcher);
        self.events = Some(rx);
        self.live = true;
        Ok(())
    }

    pub fn poll_refresh(&mut self) {
        let main_path = self
            .project_dir
            .join(format!("{}.jsonl", self.report.session_id));
        let session_dir = self.project_dir.join(&self.report.session_id);
        let mut changed = false;
        let mut disconnected = false;

        if let Some(events) = &self.events {
            loop {
                match events.try_recv() {
                    Ok(Ok(event)) => {
                        let relevant_kind = matches!(
                            event.kind,
                            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                        );
                        let relevant_path = event.paths.iter().any(|path| {
                            path == &main_path
                                || (path.starts_with(&session_dir)
                                    && path.extension().and_then(|ext| ext.to_str())
                                        == Some("jsonl"))
                        });
                        if relevant_kind && relevant_path {
                            changed = true;
                        }
                    }
                    Ok(Err(error)) => self.status = format!("Watch error: {error}"),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        if disconnected {
            self.live = false;
            self.status = "File watcher disconnected; press r to refresh.".into();
        }
        if changed {
            self.dirty = true;
            self.last_change = Instant::now();
        }
        if self.dirty && self.last_change.elapsed() >= Duration::from_millis(100) {
            self.dirty = false;
            self.reload();
        }
    }

    pub fn reload(&mut self) {
        match load_data(&self.project_dir, &self.report.session_id, self.threshold) {
            Ok(data) => {
                self.report = data.report;
                self.markdown = data.markdown;
                self.rows = data.rows;
                self.selected = self.selected.min(self.rows.len().saturating_sub(1));
                self.right_scroll = 0;
                self.status = "Refreshed.".into();
            }
            Err(error) => self.status = format!("Refresh failed: {error:#}"),
        }
    }

    pub fn selected_agent(&self) -> Option<&AgentReport> {
        let id = self.rows.get(self.selected)?.agent_id.as_str();
        self.report.agents.iter().find(|agent| agent.agent_id == id)
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('h') | KeyCode::Left => self.focus = Focus::Tree,
            KeyCode::Char('l') | KeyCode::Right => self.focus = Focus::Detail,
            KeyCode::Tab => self.select_tab((self.tab.index() + 1) % ViewTab::ALL.len()),
            KeyCode::Char('1') => self.select_tab(0),
            KeyCode::Char('2') => self.select_tab(1),
            KeyCode::Char('3') => self.select_tab(2),
            KeyCode::Char('4') => self.select_tab(3),
            KeyCode::PageUp => self.scroll_detail(-(self.right_height.max(1) as i32)),
            KeyCode::PageDown => self.scroll_detail(self.right_height.max(1) as i32),
            KeyCode::Char('j') | KeyCode::Down if self.focus == Focus::Tree => self.move_tree(1),
            KeyCode::Char('k') | KeyCode::Up if self.focus == Focus::Tree => self.move_tree(-1),
            KeyCode::Char('j') | KeyCode::Down => self.scroll_detail(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_detail(-1),
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('y') => self.copy_report(),
            _ => {}
        }
    }

    fn select_tab(&mut self, index: usize) {
        self.tab = ViewTab::ALL[index];
        self.right_scroll = 0;
        self.focus = Focus::Detail;
    }

    fn move_tree(&mut self, amount: i32) {
        if self.rows.is_empty() {
            return;
        }
        self.selected =
            (self.selected as i32 + amount).clamp(0, self.rows.len() as i32 - 1) as usize;
        self.right_scroll = 0;
    }

    fn scroll_detail(&mut self, amount: i32) {
        let max = self
            .right_len
            .saturating_sub(self.right_height.max(1) as usize)
            .min(i32::MAX as usize) as i32;
        self.right_scroll = (self.right_scroll as i32 + amount).clamp(0, max) as u16;
    }

    fn copy_report(&mut self) {
        self.status = match arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(self.markdown.clone()))
        {
            Ok(()) => "Copied to clipboard!".into(),
            Err(error) => format!("Clipboard error: {error}"),
        };
    }
}

fn load_data(project_dir: &Path, session_id: &str, threshold: u64) -> Result<SessionData> {
    let report = analyze_session(project_dir, session_id, threshold)?;
    let topology = build_topology(project_dir, session_id, &report.agents)?;
    let rows = flatten_topology(&topology, &report.agents);
    let markdown = render_markdown(&report);
    Ok(SessionData {
        report,
        markdown,
        rows,
    })
}

fn flatten_topology(topology: &SessionTopology, agents: &[AgentReport]) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    flatten_node(&topology.root, "", true, true, false, agents, &mut rows);
    for orphan in &topology.orphans {
        flatten_node(orphan, "", true, true, true, agents, &mut rows);
    }
    rows
}

fn flatten_node(
    node: &TopologyNode,
    prefix: &str,
    is_last: bool,
    is_root: bool,
    detached: bool,
    agents: &[AgentReport],
    rows: &mut Vec<TreeRow>,
) {
    let connector = if is_root {
        ""
    } else if is_last {
        "└─ "
    } else {
        "├─ "
    };
    let compacted = agents
        .iter()
        .find(|agent| agent.agent_id == node.agent_id)
        .is_some_and(|agent| !agent.compactions.is_empty());
    rows.push(TreeRow {
        agent_id: node.agent_id.clone(),
        label: if detached {
            format!("[detached] {}", node.label)
        } else {
            node.label.clone()
        },
        prefix: format!("{prefix}{connector}"),
        tokens: node.total_tokens,
        has_errors: node.has_errors,
        loop_failures: node.max_consecutive_failures,
        compacted,
    });

    let child_prefix = if is_root {
        String::new()
    } else if is_last {
        format!("{prefix}   ")
    } else {
        format!("{prefix}│  ")
    };
    for (index, child) in node.children.iter().enumerate() {
        flatten_node(
            child,
            &child_prefix,
            index + 1 == node.children.len(),
            false,
            false,
            agents,
            rows,
        );
    }
}
