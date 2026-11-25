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

/// 扫描主会话与所有 subagents 日志，重建完整的父子调用拓扑树
pub fn build_topology(
    proj_dir: &Path,
    session_id: &str,
    reports: &[AgentReport],
) -> Result<SessionTopology> {
    let report_map: HashMap<&str, &AgentReport> =
        reports.iter().map(|r| (r.agent_id.as_str(), r)).collect();

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

    // 收集每个日志的时间边界、parentToolUseID 以及它向外派发的 SpawnCall
    let mut time_bounds: HashMap<String, (Option<DateTime<Utc>>, Option<DateTime<Utc>>)> =
        HashMap::new();
    let mut parent_tool_ptr: HashMap<String, String> = HashMap::new();
    let mut all_spawns: Vec<SpawnCall> = Vec::new();

    for (agent_id, path) in &log_files {
        let meta = scan_agent_links(agent_id, path)?;
        time_bounds.insert(agent_id.clone(), (meta.first_ts, meta.last_ts));
        if let Some(pt_id) = meta.parent_tool_use_id {
            parent_tool_ptr.insert(agent_id.clone(), pt_id);
        }
        all_spawns.extend(meta.spawns);
    }

    // 建立 parent_agent_id -> Vec<child_agent_id> 映射
    let mut children_map: HashMap<String, Vec<String>> = HashMap::new();
    let mut attached_children: HashSet<String> = HashSet::new();

    for spawn in &all_spawns {
        // 优先匹配 tool_result 里明确返回的 agentId，其次反查 parentToolUseID
        let matched_child = reports.iter().find(|r| {
            if r.is_main || attached_children.contains(&r.agent_id) {
                return false;
            }
            let raw_id = r.agent_id.strip_prefix("agent-").unwrap_or(&r.agent_id);
            let by_result = spawn
                .resolved_child_id
                .as_deref()
                .map(|cid| cid == raw_id || cid == r.agent_id)
                .unwrap_or(false);
            let by_ptr = parent_tool_ptr
                .get(&r.agent_id)
                .map(|pt| pt == &spawn.tool_use_id)
                .unwrap_or(false);
            by_result || by_ptr
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

    // 收集未能关联到具体 Task 调用节点的孤立 Subagent
    let mut orphans = Vec::new();
    for rep in reports {
        if !rep.is_main && !attached_children.contains(&rep.agent_id) {
            let label = rep.task_desc.as_deref().unwrap_or("Detached Subagent");
            orphans.push(build_node(
                &rep.agent_id,
                label,
                &report_map,
                &time_bounds,
                &children_map,
                &mut HashSet::new(),
            ));
        }
    }

    Ok(SessionTopology { root, orphans })
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

struct AgentLinkMeta {
    first_ts: Option<DateTime<Utc>>,
    last_ts: Option<DateTime<Utc>>,
    parent_tool_use_id: Option<String>,
    spawns: Vec<SpawnCall>,
}

fn scan_agent_links(agent_id: &str, path: &Path) -> Result<AgentLinkMeta> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut first_ts: Option<DateTime<Utc>> = None;
    let mut last_ts: Option<DateTime<Utc>> = None;
    let mut parent_tool_use_id: Option<String> = None;
    let mut pending_spawns: HashMap<String, usize> = HashMap::new();
    let mut spawns: Vec<SpawnCall> = Vec::new();

    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<RawLine>(&line) else {
            continue;
        };

        let ts = raw
            .timestamp
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        if let Some(t) = ts {
            if first_ts.is_none() {
                first_ts = Some(t);
            }
            last_ts = Some(t);
        }

        if parent_tool_use_id.is_none() && raw.parent_tool_use_id.is_some() {
            parent_tool_use_id = raw.parent_tool_use_id.clone();
        }

        let Some(Value::Array(blocks)) = raw.message.and_then(|m| m.content) else {
            continue;
        };

        for block in blocks {
            match block.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "tool_use" => {
                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if name == "Task" || name == "Agent" {
                        let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let desc = block
                            .get("input")
                            .and_then(|i| i.get("description"))
                            .and_then(|d| d.as_str())
                            .unwrap_or("Subagent Task")
                            .chars()
                            .take(48)
                            .collect();
                        if !id.is_empty() {
                            pending_spawns.insert(id.clone(), spawns.len());
                            spawns.push(SpawnCall {
                                tool_use_id: id,
                                parent_agent_id: agent_id.to_string(),
                                description: desc,
                                spawned_at: ts,
                                resolved_child_id: None,
                            });
                        }
                    }
                }
                "tool_result" => {
                    let tuid = block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                    if let Some(&idx) = pending_spawns.get(tuid) {
                        spawns[idx].resolved_child_id = extract_agent_id(block.get("content"));
                    }
                }
                _ => {}
            }
        }
    }

    Ok(AgentLinkMeta {
        first_ts,
        last_ts,
        parent_tool_use_id,
        spawns,
    })
}

fn extract_agent_id(content: Option<&Value>) -> Option<String> {
    let text = match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("agentId:")
            .map(|rest| rest.trim().trim_matches('"').trim_matches('\'').to_string())
            .filter(|id| !id.is_empty())
    })
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