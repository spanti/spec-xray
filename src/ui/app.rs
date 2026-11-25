use super::{
    live::{LiveSnapshot, LiveStatus, LiveTracker, TreeRow},
    sessions::ProjectSessions,
};
use crate::{
    analyzer::analyzer::{AgentReport, SessionReport, list_sessions},
    parser::tree::SessionTopology,
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
pub enum ViewMode {
    Compact,
    Diagnostic,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewTab {
    Radar,
    Merge,
    Timeline,
    Report,
    Gantt,
}

impl ViewTab {
    pub const ALL: [Self; 5] = [
        Self::Radar,
        Self::Merge,
        Self::Timeline,
        Self::Report,
        Self::Gantt,
    ];

    pub fn index(self) -> usize {
        match self {
            Self::Radar => 0,
            Self::Merge => 1,
            Self::Timeline => 2,
            Self::Report => 3,
            Self::Gantt => 4,
        }
    }
}

pub struct App {
    project_dir: PathBuf,
    threshold: u64,
    tracker: LiveTracker,
    pub report: SessionReport,
    pub topology: SessionTopology,
    pub sessions: Option<ProjectSessions>,
    pub selector_open: bool,
    pub session_selected: usize,
    pub selector_height: u16,
    pub markdown: String,
    pub rows: Vec<TreeRow>,
    pub selected: usize,
    pub mode: ViewMode,
    pub focus: Focus,
    pub tab: ViewTab,
    pub right_scroll: u16,
    pub right_len: usize,
    pub right_height: u16,
    pub live: bool,
    pub status: String,
    pub quit_confirm_open: bool,
    pub quit: bool,
    watcher: Option<RecommendedWatcher>,
    events: Option<Receiver<notify::Result<notify::Event>>>,
    dirty: bool,
    last_change: Instant,
    last_tick: Instant,
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
        let tracker = LiveTracker::load(project_dir, &session_id, threshold)?;
        let snapshot = tracker.snapshot();
        Ok(Self {
            project_dir: project_dir.to_path_buf(),
            threshold,
            tracker,
            report: snapshot.report,
            topology: snapshot.topology,
            sessions: None,
            selector_open: false,
            session_selected: 0,
            selector_height: 10,
            markdown: snapshot.markdown,
            rows: snapshot.rows,
            selected: 0,
            mode: ViewMode::Compact,
            focus: Focus::Tree,
            tab: ViewTab::Radar,
            right_scroll: 0,
            right_len: 0,
            right_height: 0,
            live: false,
            status: String::new(),
            quit_confirm_open: false,
            quit: false,
            watcher: None,
            events: None,
            dirty: false,
            last_change: Instant::now(),
            last_tick: Instant::now(),
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
            self.events = None;
            self.live = false;
            self.status = "File watcher disconnected; press r to refresh.".into();
        }
        if changed {
            self.dirty = true;
            self.last_change = Instant::now();
        }
        let debounced = self.dirty && self.last_change.elapsed() >= Duration::from_millis(100);
        let tick = self.last_tick.elapsed() >= Duration::from_millis(500);
        if debounced || tick {
            self.dirty = false;
            self.last_tick = Instant::now();
            self.refresh();
        }
    }

    fn refresh(&mut self) {
        match self.tracker.poll() {
            Ok(changed) => {
                self.apply_snapshot();
                if changed {
                    self.status = "Incrementally refreshed.".into();
                }
            }
            Err(error) => self.status = format!("Refresh failed: {error:#}"),
        }
    }

    pub fn reload(&mut self) {
        match LiveTracker::load(&self.project_dir, &self.report.session_id, self.threshold) {
            Ok(tracker) => {
                self.tracker = tracker;
                self.apply_snapshot();
                self.right_scroll = 0;
                self.status = "Fully reloaded.".into();
            }
            Err(error) => self.status = format!("Full reload failed: {error:#}"),
        }
    }

    fn apply_snapshot(&mut self) {
        let selected_id = self.rows.get(self.selected).map(|row| row.agent_id.clone());
        let LiveSnapshot {
            report,
            topology,
            markdown,
            rows,
        } = self.tracker.snapshot();
        self.report = report;
        self.topology = topology;
        self.markdown = markdown;
        self.rows = rows;
        self.selected = selected_id
            .and_then(|id| self.rows.iter().position(|row| row.agent_id == id))
            .unwrap_or_else(|| self.selected.min(self.rows.len().saturating_sub(1)));
        let max_scroll = self.right_len.saturating_sub(self.right_height as usize);
        self.right_scroll = self
            .right_scroll
            .min(max_scroll.min(u16::MAX as usize) as u16);
    }

    pub fn selected_agent(&self) -> Option<&AgentReport> {
        let id = self.rows.get(self.selected)?.agent_id.as_str();
        self.report.agents.iter().find(|agent| agent.agent_id == id)
    }

    pub fn status_count(&self, status: LiveStatus) -> usize {
        self.rows.iter().filter(|row| row.status == status).count()
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self.quit_confirm_open {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('q') => {
                    self.quit_confirm_open = false;
                    self.quit = true;
                }
                KeyCode::Esc | KeyCode::Char('n') => self.quit_confirm_open = false,
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Char('q') {
            self.quit_confirm_open = true;
            return;
        }
        if key.code == KeyCode::Char('s') {
            self.toggle_selector();
            return;
        }
        if self.selector_open {
            match key.code {
                KeyCode::Esc => self.selector_open = false,
                KeyCode::Char('r') => self.rescan_sessions(),
                KeyCode::Enter => self.load_selected_session(),
                KeyCode::Char('j') | KeyCode::Down => self.move_session(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_session(-1),
                KeyCode::PageDown => self.move_session(self.selector_height.max(1) as i32),
                KeyCode::PageUp => self.move_session(-(self.selector_height.max(1) as i32)),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => self.quit_confirm_open = true,
            KeyCode::Char('v') => {
                self.mode = match self.mode {
                    ViewMode::Compact => ViewMode::Diagnostic,
                    ViewMode::Diagnostic => ViewMode::Compact,
                };
                self.right_scroll = 0;
            }
            KeyCode::Char('h') | KeyCode::Left => self.focus = Focus::Tree,
            KeyCode::Char('l') | KeyCode::Right => self.focus = Focus::Detail,
            KeyCode::Tab if self.mode == ViewMode::Diagnostic => {
                self.select_tab((self.tab.index() + 1) % ViewTab::ALL.len());
            }
            KeyCode::Char('1') if self.mode == ViewMode::Diagnostic => self.select_tab(0),
            KeyCode::Char('2') if self.mode == ViewMode::Diagnostic => self.select_tab(1),
            KeyCode::Char('3') if self.mode == ViewMode::Diagnostic => self.select_tab(2),
            KeyCode::Char('4') if self.mode == ViewMode::Diagnostic => self.select_tab(3),
            KeyCode::Char('5') if self.mode == ViewMode::Diagnostic => self.select_tab(4),
            KeyCode::PageUp if self.mode == ViewMode::Compact => {
                self.move_tree(-(self.right_height.max(1) as i32));
            }
            KeyCode::PageDown if self.mode == ViewMode::Compact => {
                self.move_tree(self.right_height.max(1) as i32);
            }
            KeyCode::PageUp => self.scroll_detail(-(self.right_height.max(1) as i32)),
            KeyCode::PageDown => self.scroll_detail(self.right_height.max(1) as i32),
            KeyCode::Char('j') | KeyCode::Down
                if self.mode == ViewMode::Compact || self.focus == Focus::Tree =>
            {
                self.move_tree(1);
            }
            KeyCode::Char('k') | KeyCode::Up
                if self.mode == ViewMode::Compact || self.focus == Focus::Tree =>
            {
                self.move_tree(-1);
            }
            KeyCode::Char('j') | KeyCode::Down => self.scroll_detail(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_detail(-1),
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('y') => self.copy_report(),
            _ => {}
        }
    }

    fn toggle_selector(&mut self) {
        if self.selector_open {
            self.selector_open = false;
            return;
        }
        if self.sessions.is_none() {
            self.rescan_sessions();
        } else {
            self.select_current_session();
        }
        self.selector_open = true;
    }

    fn rescan_sessions(&mut self) {
        match ProjectSessions::scan(&self.project_dir) {
            Ok(sessions) => {
                self.status = format!("Scanned {} sessions.", sessions.sessions.len());
                self.sessions = Some(sessions);
                self.select_current_session();
            }
            Err(error) => self.status = format!("Session scan failed: {error:#}"),
        }
    }

    fn select_current_session(&mut self) {
        self.session_selected = self
            .sessions
            .as_ref()
            .and_then(|sessions| {
                sessions
                    .sessions
                    .iter()
                    .position(|session| session.id == self.report.session_id)
            })
            .unwrap_or(0);
    }

    fn move_session(&mut self, amount: i32) {
        let Some(sessions) = &self.sessions else {
            return;
        };
        if sessions.sessions.is_empty() {
            return;
        }
        self.session_selected = (self.session_selected as i32 + amount)
            .clamp(0, sessions.sessions.len() as i32 - 1) as usize;
    }

    fn load_selected_session(&mut self) {
        let Some(id) = self.sessions.as_ref().and_then(|sessions| {
            sessions
                .sessions
                .get(self.session_selected)
                .map(|session| session.id.clone())
        }) else {
            return;
        };
        match LiveTracker::load(&self.project_dir, &id, self.threshold) {
            Ok(tracker) => {
                self.tracker = tracker;
                self.apply_snapshot();
                self.selected = 0;
                self.right_scroll = 0;
                self.selector_open = false;
                self.status = format!("Loaded session {id}.");
            }
            Err(error) => self.status = format!("Session load failed: {error:#}"),
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
