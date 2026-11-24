use crate::schema::{RawLine, ToolInvocation};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct AgentReport {
    pub agent_id: String,
    pub is_main: bool,
    pub branch: Option<String>,
    pub turns: usize,
    pub startup_cache_tokens: u64,
    pub peak_context_tokens: u64,
    pub total_tokens: u64,
    /// 工具名 -> 累计触发的上下文增长估算
    pub tool_token_delta: HashMap<String, u64>,
    /// 单次大体积噪音操作：(工具/目标, 增量 tokens)
    pub heavy_ops: Vec<(String, u64)>,
    /// 文件路径 -> 无行号限制的 Read 次数
    pub full_read_counts: HashMap<String, usize>,
    /// Edit / Write 修改过的文件集合
    pub edited_files: HashSet<String>,
    /// 失败的 Bash 构建/测试记录
    pub failed_commands: Vec<(String, String)>,
    /// 连续失败死循环次数（连续 >=3 次失败记为风暴）
    pub max_consecutive_failures: usize,
}

#[derive(Debug)]
pub struct SessionReport {
    pub project_dir: PathBuf,
    pub session_id: String,
    pub agents: Vec<AgentReport>,
    /// 跨 Agent 文件修改碰撞：文件路径 -> 修改过该文件的 agent_id 列表
    pub file_clashes: BTreeMap<String, Vec<String>>,
}

/// 将本地工作目录路径映射为 ~/.claude/projects/<encoded-path>
pub fn resolve_project_dir(work_dir: &Path) -> Result<PathBuf> {
    let abs = fs::canonicalize(work_dir).unwrap_or_else(|_| work_dir.to_path_buf());
    let raw_str = abs.to_string_lossy();
    // 剥离 Windows fs::canonicalize 产生的 \\?\ 前缀
    let clean_str = raw_str.strip_prefix(r"\\?\").unwrap_or(&raw_str);

    let encoded: String = clean_str
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    let home = dirs::home_dir().context("无法定位用户 Home 目录")?;
    let projects_root = home.join(".claude").join("projects");
    let exact_dir = projects_root.join(&encoded);
    if exact_dir.exists() {
        return Ok(exact_dir);
    }

    // Windows 盘符大小写兼容匹配（如 E--... vs e--...）
    if let Ok(entries) = fs::read_dir(&projects_root) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().eq_ignore_ascii_case(&encoded) {
                return Ok(entry.path());
            }
        }
    }

    bail!("未找到对应的 Claude Code 日志目录: {}", exact_dir.display());
}

/// 获取指定项目下按最后修改时间倒序排列的会话 UUID 列表
pub fn list_sessions(proj_dir: &Path) -> Result<Vec<(String, std::time::SystemTime)>> {
    let mut sessions = Vec::new();
    for entry in fs::read_dir(proj_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            let mtime = entry.metadata()?.modified()?;
            sessions.push((stem.to_string(), mtime));
        }
    }
    sessions.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(sessions)
}

/// 加载主会话及同目录下的 subagents/*.jsonl 并执行全量诊断
pub fn analyze_session(proj_dir: &Path, session_id: &str, heavy_threshold: u64) -> Result<SessionReport> {
    let main_file = proj_dir.join(format!("{session_id}.jsonl"));
    if !main_file.exists() {
        bail!("主会话日志不存在: {}", main_file.display());
    }

    let mut agents = Vec::new();
    agents.push(analyze_single_jsonl(&main_file, "main", true, heavy_threshold)?);

    // 扫描 <session-id>/subagents/agent-*.jsonl
    let subagents_dir = proj_dir.join(session_id).join("subagents");
    if subagents_dir.is_dir() {
        let mut sub_files: Vec<PathBuf> = fs::read_dir(&subagents_dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .collect();
        sub_files.sort();

        for sub_path in sub_files {
            let agent_id = sub_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown-subagent")
                .to_string();
            agents.push(analyze_single_jsonl(&sub_path, &agent_id, false, heavy_threshold)?);
        }
    }

    // 计算跨 Agent 文件修改交集（排除 main 仅比较多个 agent，或包含所有修改者）
    let mut file_editors: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for agent in &agents {
        for file in &agent.edited_files {
            file_editors
                .entry(file.clone())
                .or_default()
                .push(agent.agent_id.clone());
        }
    }
    let file_clashes: BTreeMap<String, Vec<String>> = file_editors
        .into_iter()
        .filter(|(_, editors)| editors.len() >= 2)
        .collect();

    Ok(SessionReport {
        project_dir: proj_dir.to_path_buf(),
        session_id: session_id.to_string(),
        agents,
        file_clashes,
    })
}

fn analyze_single_jsonl(
    path: &Path,
    agent_id: &str,
    is_main: bool,
    heavy_threshold: u64,
) -> Result<AgentReport> {
    let file = File::open(path).with_context(|| format!("打开日志失败: {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut report = AgentReport {
        agent_id: agent_id.to_string(),
        is_main,
        ..Default::default()
    };

    let mut seen_msg_ids: HashSet<String> = HashSet::new();
    let mut pending_tools: HashMap<String, ToolInvocation> = HashMap::new();
    let mut last_tools: Vec<ToolInvocation> = Vec::new();
    let mut prev_context_size: u64 = 0;
    let mut current_fail_streak: usize = 0;

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<RawLine>(&line) else {
            continue;
        };

        if report.branch.is_none()
            && let Some(b) = &raw.git_branch
            && !b.is_empty()
        {
            report.branch = Some(b.clone());
        }

        let Some(msg) = raw.message else { continue };

        // 1. 统计 Token（严格按 message.id / uuid 去重）
        if let Some(usage) = msg.usage {
            let dedup_key = msg.id.clone().or(raw.uuid.clone());
            let is_new = dedup_key.map(|k| seen_msg_ids.insert(k)).unwrap_or(true);

            if is_new {
                report.turns += 1;
                let ctx = usage.context_size();
                if report.turns == 1 {
                    report.startup_cache_tokens = usage.cache_creation_input_tokens;
                } else if ctx > prev_context_size && !last_tools.is_empty() {
                    // 将本轮上下文增量均摊归因到上一轮触发的工具上
                    let delta = ctx - prev_context_size;
                    let share = delta / (last_tools.len() as u64);
                    for tool in &last_tools {
                        *report.tool_token_delta.entry(tool.name.clone()).or_default() += share;
                        if share >= heavy_threshold {
                            let label = format!(
                                "{} ({})",
                                tool.name,
                                tool.target.as_deref().unwrap_or("-")
                            );
                            report.heavy_ops.push((label, share));
                        }
                    }
                }
                report.peak_context_tokens = report.peak_context_tokens.max(ctx);
                report.total_tokens += usage.total();
                prev_context_size = ctx;
                last_tools.clear();
            }
        }

        // 2. 解析 content 数组中的 tool_use 与 tool_result
        let Some(Value::Array(blocks)) = msg.content else {
            continue;
        };

        for block in blocks {
            let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match btype {
                "tool_use" => {
                    let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
                    let input = block.get("input");

                    let (target, has_line_limit) = extract_tool_target(&name, input);
                    if name == "Read"
                        && !has_line_limit
                        && let Some(ref p) = target
                    {
                        *report.full_read_counts.entry(p.clone()).or_default() += 1;
                    }
                    if (name == "Edit" || name == "Write" || name == "MultiEdit")
                        && let Some(ref p) = target
                    {
                        report.edited_files.insert(p.clone());
                    }

                    let inv = ToolInvocation {
                        id: id.clone(),
                        name,
                        target,
                        has_line_limit,
                    };
                    if !id.is_empty() {
                        pending_tools.insert(id, inv.clone());
                    }
                    last_tools.push(inv);
                }
                "tool_result" => {
                    let tool_use_id = block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                    let is_error = block.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                    if let Some(inv) = pending_tools.remove(tool_use_id)
                        && inv.name == "Bash"
                    {
                        if is_error {
                            current_fail_streak += 1;
                            report.max_consecutive_failures =
                                report.max_consecutive_failures.max(current_fail_streak);
                            let snippet = extract_result_snippet(block.get("content"));
                            let cmd = inv.target.unwrap_or_else(|| "bash".into());
                            report.failed_commands.push((cmd, snippet));
                        } else {
                            current_fail_streak = 0;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    report.heavy_ops.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(report)
}

fn extract_tool_target(name: &str, input: Option<&Value>) -> (Option<String>, bool) {
    let Some(obj) = input.and_then(|v| v.as_object()) else {
        return (None, false);
    };
    match name {
        "Read" => {
            let path = obj.get("file_path").and_then(|v| v.as_str()).map(String::from);
            let limited = obj.contains_key("limit") || obj.contains_key("offset");
            (path, limited)
        }
        "Edit" | "Write" | "MultiEdit" => {
            let path = obj.get("file_path").and_then(|v| v.as_str()).map(String::from);
            (path, false)
        }
        "Grep" | "Glob" => {
            let pat = obj.get("pattern").and_then(|v| v.as_str()).unwrap_or("*");
            let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            (Some(format!("{pat} in {path}")), false)
        }
        "Bash" => {
            let cmd = obj
                .get("command")
                .and_then(|v| v.as_str())
                .map(|s| s.lines().next().unwrap_or("").chars().take(72).collect());
            (cmd, false)
        }
        _ => (None, false),
    }
}

fn extract_result_snippet(content: Option<&Value>) -> String {
    let raw = match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ")
        .chars()
        .take(180)
        .collect()
}