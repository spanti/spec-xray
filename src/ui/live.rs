use crate::{
    analyzer::analyzer::{
        AgentReport, IncrementalAnalyzer, SessionReport, build_session_report, extract_tool_target,
    },
    parser::{
        schema::RawLine,
        tree::{
            AgentLinkMeta, SessionTopology, SpawnCall, TopologyNode, build_topology_from_links,
        },
    },
    report::render_markdown_with_topology,
};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Instant, SystemTime},
};

const STALE_AFTER_SECS: u64 = 90;
const PLACEHOLDER_PREFIX: &str = "pending-";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

impl LiveStatus {
    pub fn name(self) -> &'static str {
        match self {
            Self::Queued => "排队中",
            Self::Running => "运行中",
            Self::Completed => "已完成",
            Self::Failed => "已失败",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "[排队中]",
            Self::Running => "[运行中]",
            Self::Completed => "[已完成]",
            Self::Failed => "[已失败]",
        }
    }
}

pub struct TreeRow {
    pub agent_id: String,
    pub label: String,
    pub prefix: String,
    pub tokens: u64,
    pub turns: usize,
    pub context_tokens: u64,
    pub compacted: bool,
    pub status: LiveStatus,
    pub action: String,
    pub action_secs: u64,
}

pub struct LiveSnapshot {
    pub report: SessionReport,
    pub topology: SessionTopology,
    pub markdown: String,
    pub rows: Vec<TreeRow>,
}

pub struct LiveTracker {
    project_dir: PathBuf,
    session_id: String,
    threshold: u64,
    traces: BTreeMap<String, LiveTrace>,
}

struct LiveTrace {
    path: PathBuf,
    offset: u64,
    last_write: Option<SystemTime>,
    partial: Vec<u8>,
    analyzer: IncrementalAnalyzer,
    links: AgentLinkMeta,
    runtime: RuntimeState,
}

#[derive(Debug)]
struct PendingCall {
    name: String,
    action: String,
}

#[derive(Debug)]
struct RuntimeState {
    pending: HashMap<String, PendingCall>,
    task_results: HashMap<String, bool>,
    spawn_ticks: HashMap<String, Instant>,
    last_event: Option<Instant>,
    last_timestamp: Option<DateTime<Utc>>,
    action: String,
    action_since: Instant,
    ended: bool,
    failure_streak: usize,
}

struct SpawnRuntime {
    result: Option<bool>,
    spawned_at: Option<DateTime<Utc>>,
    observed_at: Option<Instant>,
}

impl LiveTracker {
    pub fn load(project_dir: &Path, session_id: &str, threshold: u64) -> Result<Self> {
        let main_path = project_dir.join(format!("{session_id}.jsonl"));
        if !main_path.exists() {
            bail!("主会话日志不存在: {}", main_path.display());
        }
        let mut traces = BTreeMap::new();
        traces.insert(
            "main".to_string(),
            LiveTrace::new(main_path, "main", true, threshold),
        );
        let mut tracker = Self {
            project_dir: project_dir.to_path_buf(),
            session_id: session_id.to_string(),
            threshold,
            traces,
        };
        tracker.poll()?;
        Ok(tracker)
    }

    /// 仅发现新文件并读取各文件新增字节；返回是否有字节追加。
    pub fn poll(&mut self) -> Result<bool> {
        self.discover_subagents()?;
        let mut changed = false;
        for trace in self.traces.values_mut() {
            changed |= trace.read_new()?;
        }
        Ok(changed)
    }

    fn discover_subagents(&mut self) -> Result<()> {
        let directory = self.project_dir.join(&self.session_id).join("subagents");
        if !directory.is_dir() {
            return Ok(());
        }
        let mut files: Vec<PathBuf> = fs::read_dir(&directory)
            .with_context(|| format!("读取 Subagent 目录失败: {}", directory.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .collect();
        files.sort();
        for path in files {
            let Some(agent_id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            if !self.traces.contains_key(&agent_id) {
                self.traces.insert(
                    agent_id.clone(),
                    LiveTrace::new(path, &agent_id, false, self.threshold),
                );
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> LiveSnapshot {
        let parent_pointers: HashMap<String, String> = self
            .traces
            .values()
            .filter_map(|trace| {
                trace
                    .links
                    .parent_tool_use_id
                    .as_ref()
                    .map(|pointer| (trace.links.agent_id.clone(), pointer.clone()))
            })
            .collect();
        let mut agents: Vec<AgentReport> = self
            .traces
            .get("main")
            .into_iter()
            .chain(
                self.traces
                    .values()
                    .filter(|trace| trace.links.agent_id != "main"),
            )
            .map(|trace| trace.analyzer.report().clone())
            .collect();

        // 子文件一出现就使用 Task 描述，不必等待主会话的工具结果。
        for agent in agents.iter_mut().filter(|agent| !agent.is_main) {
            if agent.task_desc.is_none()
                && let Some(spawn) = self.find_spawn(&agent.agent_id, &parent_pointers)
            {
                agent.task_desc = Some(spawn.description.clone());
            }
        }
        let report = build_session_report(&self.project_dir, &self.session_id, agents.clone());
        let mut topology_agents = agents;
        let mut links: Vec<AgentLinkMeta> = self
            .traces
            .values()
            .map(|trace| trace.links.clone())
            .collect();

        // 没有子文件的 Task 也进入拓扑，用稳定占位 ID 呈现排队状态。
        for link in &mut links {
            for spawn in &mut link.spawns {
                if topology_agents
                    .iter()
                    .any(|agent| !agent.is_main && spawn_matches(spawn, agent, &parent_pointers))
                {
                    continue;
                }
                let agent_id = spawn
                    .resolved_child_id
                    .clone()
                    .unwrap_or_else(|| format!("{PLACEHOLDER_PREFIX}{}", spawn.tool_use_id));
                spawn.resolved_child_id = Some(agent_id.clone());
                let mut placeholder = AgentReport::default();
                placeholder.agent_id = agent_id;
                placeholder.task_desc = Some(spawn.description.clone());
                topology_agents.push(placeholder);
            }
        }

        let topology = build_topology_from_links(&topology_agents, &links);
        let spawn_runtimes = self.spawn_runtimes(&links, &topology_agents, &parent_pointers);
        let mut rows = Vec::new();
        self.flatten_node(
            &topology.root,
            "",
            true,
            true,
            false,
            &spawn_runtimes,
            &mut rows,
        );
        for orphan in &topology.orphans {
            self.flatten_node(orphan, "", true, true, true, &spawn_runtimes, &mut rows);
        }
        let markdown = render_markdown_with_topology(&report, Some(&topology));
        LiveSnapshot {
            report,
            topology,
            markdown,
            rows,
        }
    }

    fn find_spawn<'a>(
        &'a self,
        agent_id: &str,
        parent_pointers: &HashMap<String, String>,
    ) -> Option<&'a SpawnCall> {
        self.traces
            .values()
            .flat_map(|trace| trace.links.spawns.iter())
            .find(|spawn| {
                spawn
                    .resolved_child_id
                    .as_deref()
                    .is_some_and(|child| ids_match(child, agent_id))
                    || parent_pointers
                        .get(agent_id)
                        .is_some_and(|pointer| pointer == &spawn.tool_use_id)
            })
    }

    fn spawn_runtimes(
        &self,
        links: &[AgentLinkMeta],
        agents: &[AgentReport],
        parent_pointers: &HashMap<String, String>,
    ) -> HashMap<String, SpawnRuntime> {
        let mut runtimes = HashMap::new();
        for spawn in links.iter().flat_map(|link| link.spawns.iter()) {
            let Some(agent) = agents
                .iter()
                .find(|agent| !agent.is_main && spawn_matches(spawn, agent, parent_pointers))
            else {
                continue;
            };
            let runtime = self.traces.get(&spawn.parent_agent_id).map(|trace| {
                (
                    trace.runtime.task_results.get(&spawn.tool_use_id).copied(),
                    trace.runtime.spawn_ticks.get(&spawn.tool_use_id).copied(),
                )
            });
            runtimes.insert(
                agent.agent_id.clone(),
                SpawnRuntime {
                    result: runtime.and_then(|runtime| runtime.0),
                    spawned_at: spawn.spawned_at,
                    observed_at: runtime.and_then(|runtime| runtime.1),
                },
            );
        }
        runtimes
    }

    #[allow(clippy::too_many_arguments)]
    fn flatten_node(
        &self,
        node: &TopologyNode,
        prefix: &str,
        is_last: bool,
        is_root: bool,
        detached: bool,
        spawn_runtimes: &HashMap<String, SpawnRuntime>,
        rows: &mut Vec<TreeRow>,
    ) {
        let connector = if is_root {
            ""
        } else if is_last {
            "└─ "
        } else {
            "├─ "
        };
        let trace = self.traces.get(&node.agent_id);
        let spawn = spawn_runtimes.get(&node.agent_id);
        let turns = trace.map_or(0, |trace| trace.analyzer.report().turns);
        let context_tokens =
            trace.map_or(0, |trace| trace.analyzer.report().current_context_tokens);
        let pending = trace.is_some_and(|trace| !trace.runtime.pending.is_empty());
        let ended = trace.is_some_and(|trace| trace.runtime.ended);
        let failures = trace.map_or(0, |trace| trace.runtime.failure_streak);
        let idle = idle_secs(trace, spawn);
        let status = decide_status(
            turns,
            pending,
            ended,
            failures,
            spawn.and_then(|spawn| spawn.result),
            idle,
        );
        let action = current_action(trace, spawn, status, idle);
        let action_secs = trace
            .map(|trace| trace.runtime.action_since.elapsed().as_secs())
            .or_else(|| {
                spawn
                    .and_then(|spawn| spawn.observed_at)
                    .map(|at| at.elapsed().as_secs())
            })
            .unwrap_or(idle.unwrap_or(0));
        rows.push(TreeRow {
            agent_id: node.agent_id.clone(),
            label: if detached {
                format!("[detached] {}", node.label)
            } else {
                node.label.clone()
            },
            prefix: format!("{prefix}{connector}"),
            tokens: node.total_tokens,
            turns,
            context_tokens,
            compacted: trace.is_some_and(|trace| !trace.analyzer.report().compactions.is_empty()),
            status,
            action,
            action_secs,
        });

        let child_prefix = if is_root {
            String::new()
        } else if is_last {
            format!("{prefix}   ")
        } else {
            format!("{prefix}│  ")
        };
        for (index, child) in node.children.iter().enumerate() {
            self.flatten_node(
                child,
                &child_prefix,
                index + 1 == node.children.len(),
                false,
                false,
                spawn_runtimes,
                rows,
            );
        }
    }
}

impl LiveTrace {
    fn new(path: PathBuf, agent_id: &str, is_main: bool, threshold: u64) -> Self {
        Self {
            path,
            offset: 0,
            last_write: None,
            partial: Vec::new(),
            analyzer: IncrementalAnalyzer::new(agent_id, is_main, threshold),
            links: AgentLinkMeta::new(agent_id),
            runtime: RuntimeState::new(),
        }
    }

    fn read_new(&mut self) -> Result<bool> {
        let metadata = fs::metadata(&self.path)
            .with_context(|| format!("读取日志元数据失败: {}", self.path.display()))?;
        self.last_write = metadata.modified().ok();
        let length = metadata.len();
        if length < self.offset {
            bail!("日志发生截断，请按 r 完整重载: {}", self.path.display());
        }
        if length == self.offset {
            return Ok(false);
        }
        let mut file = File::open(&self.path)
            .with_context(|| format!("打开日志失败: {}", self.path.display()))?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut appended = Vec::new();
        file.read_to_end(&mut appended)?;
        self.offset += appended.len() as u64;
        self.partial.extend_from_slice(&appended);

        let Some(last_newline) = self.partial.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(true);
        };
        let remainder = self.partial.split_off(last_newline + 1);
        let complete = std::mem::replace(&mut self.partial, remainder);
        let now = Instant::now();
        for bytes in complete.split(|byte| *byte == b'\n') {
            let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
            if bytes.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            self.runtime.note_write(now);
            let Ok(text) = std::str::from_utf8(bytes) else {
                continue;
            };
            let Ok(raw) = serde_json::from_str::<RawLine>(text) else {
                continue;
            };
            self.links.observe(&raw);
            self.analyzer.ingest(&raw);
            self.runtime.observe(&raw, now);
        }
        self.analyzer.refresh();
        Ok(true)
    }
}

impl RuntimeState {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
            task_results: HashMap::new(),
            spawn_ticks: HashMap::new(),
            last_event: None,
            last_timestamp: None,
            action: "思考中".to_string(),
            action_since: Instant::now(),
            ended: false,
            failure_streak: 0,
        }
    }

    fn note_write(&mut self, now: Instant) {
        self.last_event = Some(now);
        self.last_timestamp = None;
    }

    fn observe(&mut self, raw: &RawLine, now: Instant) {
        self.note_write(now);
        self.last_timestamp = raw
            .timestamp
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc));
        let Some(message) = raw.message.as_ref() else {
            if raw.event_type.as_deref() == Some("result") {
                self.ended = raw.subtype.as_deref() != Some("error") && self.pending.is_empty();
            }
            return;
        };
        let Some(content) = message.content.as_ref() else {
            return;
        };
        let blocks = content.as_array();
        let mut saw_tool = false;
        let mut saw_text = content.as_str().is_some_and(|text| !text.trim().is_empty());
        if let Some(blocks) = blocks {
            for block in blocks {
                match block
                    .get("type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                {
                    "text" => {
                        saw_text |= block
                            .get("text")
                            .and_then(|value| value.as_str())
                            .is_some_and(|text| !text.trim().is_empty());
                    }
                    "tool_use" => {
                        saw_tool = true;
                        let id = block
                            .get("id")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        let name = block
                            .get("name")
                            .and_then(|value| value.as_str())
                            .unwrap_or("Unknown");
                        let (target, _) =
                            extract_tool_target(name, block.get("input"), raw.cwd.as_deref());
                        let action = action_label(name, target.as_deref());
                        if !id.is_empty() {
                            self.pending.insert(
                                id.to_string(),
                                PendingCall {
                                    name: name.to_string(),
                                    action: action.clone(),
                                },
                            );
                            if matches!(name, "Task" | "Agent") {
                                self.spawn_ticks.insert(id.to_string(), now);
                            }
                        }
                        self.action = action;
                        self.action_since = now;
                        self.ended = false;
                    }
                    "tool_result" => {
                        let id = block
                            .get("tool_use_id")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        let is_error = block
                            .get("is_error")
                            .and_then(|value| value.as_bool())
                            .unwrap_or(false);
                        if let Some(call) = self.pending.remove(id) {
                            if matches!(call.name.as_str(), "Task" | "Agent") {
                                self.task_results.insert(id.to_string(), !is_error);
                            }
                            self.failure_streak =
                                if is_error { self.failure_streak + 1 } else { 0 };
                            self.action = if is_error {
                                format!("失败: {}", call.action)
                            } else {
                                "思考中".to_string()
                            };
                            self.action_since = now;
                        }
                        self.ended = false;
                    }
                    _ => {}
                }
            }
        }
        if raw.event_type.as_deref() == Some("result") {
            self.ended = raw.subtype.as_deref() != Some("error") && self.pending.is_empty();
        } else if message.role.as_deref() == Some("assistant")
            || raw.event_type.as_deref() == Some("assistant")
        {
            self.ended = saw_text && !saw_tool && self.pending.is_empty();
            if !self.ended && self.pending.is_empty() {
                self.action = "思考中".to_string();
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decide_status(
    turns: usize,
    pending: bool,
    ended: bool,
    failures: usize,
    task_result: Option<bool>,
    idle_secs: Option<u64>,
) -> LiveStatus {
    if task_result == Some(false) || failures >= 3 {
        LiveStatus::Failed
    } else if task_result == Some(true) || (ended && !pending) {
        LiveStatus::Completed
    } else if idle_secs.is_some_and(|idle| idle >= STALE_AFTER_SECS) {
        LiveStatus::Failed
    } else if turns == 0 {
        LiveStatus::Queued
    } else {
        LiveStatus::Running
    }
}

fn idle_secs(trace: Option<&LiveTrace>, spawn: Option<&SpawnRuntime>) -> Option<u64> {
    if let Some(trace) = trace {
        if let Some(modified) = trace.last_write {
            return Some(modified.elapsed().map_or(0, |idle| idle.as_secs()));
        }
        if let Some(timestamp) = trace.runtime.last_timestamp {
            return Some(
                Utc::now()
                    .signed_duration_since(timestamp)
                    .num_seconds()
                    .max(0) as u64,
            );
        }
        if let Some(last_event) = trace.runtime.last_event {
            return Some(last_event.elapsed().as_secs());
        }
        // 空文件没有事件时间戳，只能用 mtime 判断长期无响应。
        if let Ok(modified) = fs::metadata(&trace.path).and_then(|meta| meta.modified()) {
            return Some(modified.elapsed().map_or(0, |idle| idle.as_secs()));
        }
    }
    if let Some(spawn) = spawn {
        if let Some(timestamp) = spawn.spawned_at {
            return Some(
                Utc::now()
                    .signed_duration_since(timestamp)
                    .num_seconds()
                    .max(0) as u64,
            );
        }
        if let Some(observed) = spawn.observed_at {
            return Some(observed.elapsed().as_secs());
        }
    }
    None
}

fn current_action(
    trace: Option<&LiveTrace>,
    spawn: Option<&SpawnRuntime>,
    status: LiveStatus,
    idle: Option<u64>,
) -> String {
    match status {
        LiveStatus::Queued => "等待有效响应".to_string(),
        LiveStatus::Completed => "已结束".to_string(),
        LiveStatus::Failed if spawn.and_then(|spawn| spawn.result) == Some(false) => {
            "Task 返回错误".to_string()
        }
        LiveStatus::Failed if trace.is_some_and(|trace| trace.runtime.failure_streak >= 3) => {
            format!(
                "连续工具失败 {} 次",
                trace.map_or(0, |trace| trace.runtime.failure_streak)
            )
        }
        LiveStatus::Failed => format!("无响应 {}s", idle.unwrap_or(STALE_AFTER_SECS)),
        LiveStatus::Running => trace
            .map(|trace| trace.runtime.action.clone())
            .unwrap_or_else(|| "思考中".to_string()),
    }
}

fn action_label(name: &str, target: Option<&str>) -> String {
    let target = target
        .and_then(|target| target.lines().next())
        .unwrap_or("")
        .trim();
    if target.is_empty() {
        return name.to_string();
    }
    let short: String = target.chars().take(56).collect();
    format!("{name}: {short}")
}

fn ids_match(left: &str, right: &str) -> bool {
    left == right
        || left.strip_prefix("agent-").unwrap_or(left)
            == right.strip_prefix("agent-").unwrap_or(right)
}

fn spawn_matches(
    spawn: &SpawnCall,
    agent: &AgentReport,
    parent_pointers: &HashMap<String, String>,
) -> bool {
    spawn
        .resolved_child_id
        .as_deref()
        .is_some_and(|child| ids_match(child, &agent.agent_id))
        || parent_pointers
            .get(&agent.agent_id)
            .is_some_and(|pointer| pointer == &spawn.tool_use_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs::OpenOptions,
        io::Write,
        sync::atomic::{AtomicUsize, Ordering},
        time::{Duration, SystemTime},
    };

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("spec-xray-live-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn append(&self, relative: &str, text: &str) {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            file.write_all(text.as_bytes()).unwrap();
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn usage(id: &str, tokens: u64) -> String {
        json!({
            "timestamp": Utc::now().to_rfc3339(),
            "message": {"id": id, "role": "assistant", "content": [], "usage": {
                "input_tokens": tokens
            }}
        })
        .to_string()
    }

    #[test]
    fn waits_for_partial_line_and_keeps_byte_cursor() {
        let project = TempProject::new();
        project.append("session.jsonl", "");
        let mut tracker = LiveTracker::load(&project.0, "session", 10_000).unwrap();
        let line = usage("message-1", 120);
        let split = line.len() / 2;
        project.append("session.jsonl", &line[..split]);
        assert!(tracker.poll().unwrap());
        assert_eq!(tracker.traces["main"].analyzer.report().turns, 0);
        let offset = tracker.traces["main"].offset;
        assert!(!tracker.poll().unwrap());
        assert_eq!(tracker.traces["main"].offset, offset);

        project.append("session.jsonl", &format!("{}\n", &line[split..]));
        assert!(tracker.poll().unwrap());
        assert_eq!(tracker.traces["main"].analyzer.report().turns, 1);
        assert_eq!(
            tracker.traces["main"]
                .analyzer
                .report()
                .current_context_tokens,
            120
        );
        let offset = tracker.traces["main"].offset;
        assert!(!tracker.poll().unwrap());
        assert_eq!(tracker.traces["main"].offset, offset);
    }

    #[test]
    fn discovers_new_subagent_and_tracks_completion() {
        let project = TempProject::new();
        let spawn = json!({
            "timestamp": Utc::now().to_rfc3339(),
            "message": {"role": "assistant", "content": [{
                "type": "tool_use", "id": "task-1", "name": "Agent",
                "input": {"description": "Fix parser"}
            }]}
        });
        project.append("session.jsonl", &format!("{spawn}\n"));
        let mut tracker = LiveTracker::load(&project.0, "session", 10_000).unwrap();
        let snapshot = tracker.snapshot();
        let pending = snapshot
            .rows
            .iter()
            .find(|row| row.agent_id == "pending-task-1")
            .unwrap();
        assert_eq!(pending.status, LiveStatus::Queued);
        assert_eq!(pending.label, "Fix parser");

        project.append(
            "session/subagents/agent-abc.jsonl",
            &format!(
                "{}\n",
                json!({
                    "timestamp": Utc::now().to_rfc3339(),
                    "parentToolUseID": "task-1",
                    "message": {"id": "sub-1", "role": "assistant", "content": [], "usage": {
                        "input_tokens": 50
                    }}
                })
            ),
        );
        tracker.poll().unwrap();
        let snapshot = tracker.snapshot();
        let running = snapshot
            .rows
            .iter()
            .find(|row| row.agent_id == "agent-abc")
            .unwrap();
        assert_eq!(running.status, LiveStatus::Running);
        assert_eq!(running.turns, 1);
        assert_eq!(running.context_tokens, 50);
        assert_eq!(running.label, "Fix parser");

        project.append(
            "session/subagents/agent-abc.jsonl",
            &format!(
                "{}\n",
                json!({
                    "timestamp": Utc::now().to_rfc3339(),
                    "message": {"role": "assistant", "content": [{
                        "type": "text", "text": "done"
                    }]}
                })
            ),
        );
        tracker.poll().unwrap();
        let snapshot = tracker.snapshot();
        assert_eq!(
            snapshot
                .rows
                .iter()
                .find(|row| row.agent_id == "agent-abc")
                .unwrap()
                .status,
            LiveStatus::Completed
        );
    }

    #[test]
    fn old_file_mtime_marks_unfinished_main_failed() {
        let project = TempProject::new();
        project.append("session.jsonl", &format!("{}\n", usage("message-1", 100)));
        let mut tracker = LiveTracker::load(&project.0, "session", 10_000).unwrap();
        assert_eq!(tracker.snapshot().rows[0].status, LiveStatus::Running);

        tracker.traces.get_mut("main").unwrap().last_write =
            Some(SystemTime::now() - Duration::from_secs(31 * 24 * 60 * 60));
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.rows[0].status, LiveStatus::Failed);
        assert!(snapshot.rows[0].action.starts_with("无响应"));
    }

    #[test]
    fn task_error_fails_placeholder_before_subagent_file_exists() {
        let project = TempProject::new();
        let timestamp = Utc::now().to_rfc3339();
        project.append(
            "session.jsonl",
            &format!(
                "{}\n{}\n",
                json!({
                    "timestamp": timestamp,
                    "message": {"role": "assistant", "content": [{
                        "type": "tool_use", "id": "task-error", "name": "Task",
                        "input": {"description": "Broken task"}
                    }]}
                }),
                json!({
                    "timestamp": timestamp,
                    "message": {"role": "user", "content": [{
                        "type": "tool_result", "tool_use_id": "task-error",
                        "content": "spawn failed", "is_error": true
                    }]}
                })
            ),
        );
        let tracker = LiveTracker::load(&project.0, "session", 10_000).unwrap();
        let snapshot = tracker.snapshot();
        let failed = snapshot
            .rows
            .iter()
            .find(|row| row.agent_id == "pending-task-error")
            .unwrap();
        assert_eq!(failed.status, LiveStatus::Failed);
        assert_eq!(failed.action, "Task 返回错误");
        assert_eq!(failed.label, "Broken task");
        assert!(
            !snapshot
                .report
                .agents
                .iter()
                .any(|agent| agent.agent_id == "pending-task-error")
        );
    }

    #[test]
    fn classifies_task_failure_loop_timeout_and_completion() {
        assert_eq!(
            decide_status(0, false, false, 0, None, Some(1)),
            LiveStatus::Queued
        );
        assert_eq!(
            decide_status(2, true, false, 0, None, Some(1)),
            LiveStatus::Running
        );
        assert_eq!(
            decide_status(2, false, true, 0, None, Some(1)),
            LiveStatus::Completed
        );
        assert_eq!(
            decide_status(0, false, false, 0, Some(false), Some(1)),
            LiveStatus::Failed
        );
        assert_eq!(
            decide_status(3, true, false, 3, None, Some(1)),
            LiveStatus::Failed
        );
        assert_eq!(
            decide_status(3, true, false, 0, None, Some(STALE_AFTER_SECS)),
            LiveStatus::Failed
        );
    }
}
