use crate::analyzer::analyzer::AgentReport;
use crate::parser::schema::RawLine;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// 单次 Task/Agent 工具派发记录
#[derive(Debug, Clone)]
pub struct SpawnCall {
    pub tool_use_id: String,
    pub parent_agent_id: String,
    pub description: String,
    pub spawned_at: Option<DateTime<Utc>>,
    pub resolved_child_id: Option<String>,
}

/// 拓扑树节点（可用于 CLI 树状输出，也可直接复用于后续 Ratatui 左侧树面板）
#[derive(Debug, Clone)]
pub struct TopologyNode {
    pub agent_id: String,
    pub label: String,
    pub branch: Option<String>,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub total_tokens: u64,
    pub max_consecutive_failures: usize,
    pub has_errors: bool,
    pub children: Vec<TopologyNode>,
}

#[derive(Debug)]
pub struct SessionTopology {
    pub root: TopologyNode,
    pub orphans: Vec<TopologyNode>,
}

/// 扫描主会话与所有 subagents 日志，重建完整的父子调用拓扑树。
pub fn build_topology(
    proj_dir: &Path,
    session_id: &str,
    reports: &[AgentReport],
) -> Result<SessionTopology> {
    let mut log_files: Vec<(String, PathBuf)> = Vec::new();
    let main_path = proj_dir.join(format!("{session_id}.jsonl"));
    if main_path.exists() {
        log_files.push(("main".to_string(), main_path));
    }
    let sub_dir = proj_dir.join(session_id).join("subagents");
    if sub_dir.is_dir()
        && let Ok(entries) = fs::read_dir(&sub_dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                log_files.push((stem.to_string(), path));
            }
        }
    }
    let links = log_files
        .iter()
        .map(|(agent_id, path)| scan_agent_links(agent_id, path))
        .collect::<Result<Vec<_>>>()?;
    Ok(build_topology_from_links(reports, &links))
}

/// 从增量追踪器已经收集的链接构建拓扑，不触碰文件系统。
pub fn build_topology_from_links(
    reports: &[AgentReport],
    links: &[AgentLinkMeta],
) -> SessionTopology {
    let report_map: HashMap<&str, &AgentReport> =
        reports.iter().map(|r| (r.agent_id.as_str(), r)).collect();
    let mut time_bounds: HashMap<String, (Option<DateTime<Utc>>, Option<DateTime<Utc>>)> =
        HashMap::new();
    let mut parent_tool_ptr: HashMap<String, String> = HashMap::new();
    let mut all_spawns: Vec<SpawnCall> = Vec::new();
    for meta in links {
        time_bounds.insert(meta.agent_id.clone(), (meta.first_ts, meta.last_ts));
        if let Some(pointer) = &meta.parent_tool_use_id {
            parent_tool_ptr.insert(meta.agent_id.clone(), pointer.clone());
        }
        all_spawns.extend(meta.spawns.iter().cloned());
    }

    let mut children_map: HashMap<String, Vec<String>> = HashMap::new();
    let mut attached_children: HashSet<String> = HashSet::new();
    for spawn in &all_spawns {
        let matched_child = reports.iter().find(|report| {
            if report.is_main || attached_children.contains(&report.agent_id) {
                return false;
            }
            let raw_id = report
                .agent_id
                .strip_prefix("agent-")
                .unwrap_or(&report.agent_id);
            let by_result = spawn
                .resolved_child_id
                .as_deref()
                .is_some_and(|id| id == raw_id || id == report.agent_id);
            let by_pointer = parent_tool_ptr
                .get(&report.agent_id)
                .is_some_and(|pointer| pointer == &spawn.tool_use_id);
            by_result || by_pointer
        });
        if let Some(child) = matched_child {
            attached_children.insert(child.agent_id.clone());
            children_map
                .entry(spawn.parent_agent_id.clone())
                .or_default()
                .push(child.agent_id.clone());
        }
    }

    let root = build_node(
        "main",
        "Main Orchestrator",
        &report_map,
        &time_bounds,
        &children_map,
        &mut HashSet::new(),
    );
    let mut orphans = Vec::new();
    for report in reports {
        if !report.is_main && !attached_children.contains(&report.agent_id) {
            let label = report.task_desc.as_deref().unwrap_or("Detached Subagent");
            orphans.push(build_node(
                &report.agent_id,
                label,
                &report_map,
                &time_bounds,
                &children_map,
                &mut HashSet::new(),
            ));
        }
    }
    SessionTopology { root, orphans }
}

fn build_node(
    agent_id: &str,
    default_label: &str,
    report_map: &HashMap<&str, &AgentReport>,
    time_bounds: &HashMap<String, (Option<DateTime<Utc>>, Option<DateTime<Utc>>)>,
    children_map: &HashMap<String, Vec<String>>,
    visited: &mut HashSet<String>,
) -> TopologyNode {
    visited.insert(agent_id.to_string());
    let rep = report_map.get(agent_id).copied();
    let (start_time, end_time) = time_bounds.get(agent_id).cloned().unwrap_or((None, None));

    let label = rep
        .and_then(|r| r.task_desc.clone())
        .unwrap_or_else(|| default_label.to_string());

    let mut child_nodes = Vec::new();
    if let Some(cids) = children_map.get(agent_id) {
        for cid in cids {
            if !visited.contains(cid) {
                child_nodes.push(build_node(
                    cid,
                    "Subagent",
                    report_map,
                    time_bounds,
                    children_map,
                    visited,
                ));
            }
        }
    }
    // 按启动时间排序子节点
    child_nodes.sort_by_key(|n| n.start_time);

    TopologyNode {
        agent_id: agent_id.to_string(),
        label,
        branch: rep.and_then(|r| r.branch.clone()),
        start_time,
        end_time,
        total_tokens: rep.map(|r| r.total_tokens).unwrap_or(0),
        max_consecutive_failures: rep.map(|r| r.max_consecutive_failures).unwrap_or(0),
        has_errors: rep.map(|r| !r.failed_commands.is_empty()).unwrap_or(false),
        children: child_nodes,
    }
}

#[derive(Debug, Clone)]
pub struct AgentLinkMeta {
    pub agent_id: String,
    pub first_ts: Option<DateTime<Utc>>,
    pub last_ts: Option<DateTime<Utc>>,
    pub parent_tool_use_id: Option<String>,
    pub spawns: Vec<SpawnCall>,
    pending_spawns: HashMap<String, usize>,
}

impl AgentLinkMeta {
    pub fn new(agent_id: &str) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            first_ts: None,
            last_ts: None,
            parent_tool_use_id: None,
            spawns: Vec::new(),
            pending_spawns: HashMap::new(),
        }
    }

    pub fn observe(&mut self, raw: &RawLine) {
        let timestamp = raw
            .timestamp
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc));
        if let Some(timestamp) = timestamp {
            if self.first_ts.is_none() {
                self.first_ts = Some(timestamp);
            }
            self.last_ts = Some(timestamp);
        }
        if self.parent_tool_use_id.is_none() && raw.parent_tool_use_id.is_some() {
            self.parent_tool_use_id = raw.parent_tool_use_id.clone();
        }
        let Some(Value::Array(blocks)) = raw.message.as_ref().and_then(|m| m.content.as_ref())
        else {
            return;
        };
        for block in blocks {
            match block.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "tool_use"
                    if matches!(
                        block.get("name").and_then(|v| v.as_str()),
                        Some("Task" | "Agent")
                    ) =>
                {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let description = block
                        .get("input")
                        .and_then(|input| input.get("description"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("Subagent Task")
                        .chars()
                        .take(48)
                        .collect();
                    if !id.is_empty() {
                        self.pending_spawns.insert(id.clone(), self.spawns.len());
                        self.spawns.push(SpawnCall {
                            tool_use_id: id,
                            parent_agent_id: self.agent_id.clone(),
                            description,
                            spawned_at: timestamp,
                            resolved_child_id: None,
                        });
                    }
                }
                "tool_result" => {
                    let tool_use_id = block
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(&index) = self.pending_spawns.get(tool_use_id) {
                        self.spawns[index].resolved_child_id =
                            raw.spawned_agent_id(block.get("content"));
                    }
                }
                _ => {}
            }
        }
    }
}

fn scan_agent_links(agent_id: &str, path: &Path) -> Result<AgentLinkMeta> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut meta = AgentLinkMeta::new(agent_id);
    for line in reader.lines().map_while(Result::ok) {
        if let Ok(raw) = serde_json::from_str::<RawLine>(&line) {
            meta.observe(&raw);
        }
    }
    Ok(meta)
}

/// 将拓扑树渲染为紧凑的 ASCII 树状结构（可直接嵌入 Markdown 报告）
pub fn render_ascii_tree(topo: &SessionTopology) -> String {
    let mut out = String::new();
    let base_time = topo.root.start_time;
    render_node_recursive(&topo.root, "", true, true, base_time, &mut out);

    if !topo.orphans.is_empty() {
        writeln!(out, "\n[未挂接的独立子轨迹]").unwrap();
        for (i, orphan) in topo.orphans.iter().enumerate() {
            let is_last = i + 1 == topo.orphans.len();
            render_node_recursive(orphan, "", is_last, false, base_time, &mut out);
        }
    }
    out
}

fn render_node_recursive(
    node: &TopologyNode,
    prefix: &str,
    is_last: bool,
    is_root: bool,
    base_time: Option<DateTime<Utc>>,
    out: &mut String,
) {
    let connector = if is_root {
        ""
    } else if is_last {
        "└─ "
    } else {
        "├─ "
    };

    let status = if node.max_consecutive_failures >= 3 {
        "[LOOP!]"
    } else if node.has_errors {
        "[ERR]"
    } else {
        "[OK]"
    };

    let timing = match (base_time, node.start_time, node.end_time) {
        (Some(base), Some(start), Some(end)) => {
            let offset = (start - base).num_seconds().max(0);
            let dur = (end - start).num_seconds().max(0);
            format!("+{:02}:{:02} | {}s", offset / 60, offset % 60, dur)
        }
        _ => "-".to_string(),
    };

    writeln!(
        out,
        "{}{}{} `{}` ({}) — {}k tok [{}]",
        prefix,
        connector,
        status,
        node.agent_id,
        node.label,
        node.total_tokens / 1000,
        timing
    )
    .unwrap();

    let next_prefix = if is_root {
        String::new()
    } else if is_last {
        format!("{prefix}   ")
    } else {
        format!("{prefix}│  ")
    };

    for (i, child) in node.children.iter().enumerate() {
        let child_is_last = i + 1 == node.children.len();
        render_node_recursive(child, &next_prefix, child_is_last, false, base_time, out);
    }
}
