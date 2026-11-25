use crate::parser::schema::{RawLine, ToolInvocation, Usage};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct CompactionReport {
    /// 压缩发生前已完成的 usage 轮次数
    pub turn: usize,
    pub before_tokens: u64,
    pub after_tokens: Option<u64>,
}

#[derive(Debug)]
pub struct ToolAction {
    pub turn: usize,
    pub tool: String,
    pub target: Option<String>,
    pub delta: u64,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct MergeAudit {
    pub agent_id: String,
    pub action: String,
    pub build_success: Option<bool>,
    pub build_error: Option<String>,
    pub suspect_files: Vec<String>,
}

#[derive(Debug, Default)]
pub struct AgentReport {
    pub agent_id: String,
    /// 从主会话 Task/Agent 调用提取的任务描述（如 Ticket 标题）
    pub task_desc: Option<String>,
    pub is_main: bool,
    pub branch: Option<String>,
    pub turns: usize,
    pub startup_cache_tokens: u64,
    pub peak_context_tokens: u64,
    pub total_tokens: u64,
    pub compactions: Vec<CompactionReport>,
    /// 工具名 -> 累计触发的上下文增量估算
    pub tool_token_delta: HashMap<String, u64>,
    /// 单次大体积噪音操作：(工具/目标, 增量 tokens)
    pub heavy_ops: Vec<(String, u64)>,
    /// 文件路径 -> 无行号限制的 Read 次数
    pub full_read_counts: HashMap<String, usize>,
    /// Edit / Write 修改过的文件 -> 最后一次成功修改时的分支
    pub edited_files: HashMap<String, Option<String>>,
    /// 失败的 Bash 构建/测试记录
    pub failed_commands: Vec<(String, String)>,
    /// 连续失败死循环次数（连续 >=3 次失败记为风暴）
    pub max_consecutive_failures: usize,
    /// 按日志顺序记录的工具调用流水
    pub actions: Vec<ToolAction>,
    merge_audits: Vec<MergeAudit>,
    /// 内部使用：派发的 subagent_id -> 任务描述映射
    pub spawned_tasks: HashMap<String, String>,
}

#[derive(Debug)]
pub struct SessionReport {
    pub project_dir: PathBuf,
    pub session_id: String,
    pub agents: Vec<AgentReport>,
    /// 跨 Agent 文件修改碰撞：文件路径 -> 修改过该文件的显示名称列表
    pub file_clashes: BTreeMap<String, Vec<String>>,
    pub merge_audits: Vec<MergeAudit>,
}

/// 将本地工作目录路径映射为 ~/.claude/projects/<encoded-path>
pub fn resolve_project_dir(work_dir: &Path) -> Result<PathBuf> {
    let abs = fs::canonicalize(work_dir).unwrap_or_else(|_| work_dir.to_path_buf());
    let raw_str = abs.to_string_lossy();
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

    if let Ok(entries) = fs::read_dir(&projects_root) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(&encoded)
            {
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
pub fn analyze_session(
    proj_dir: &Path,
    session_id: &str,
    heavy_threshold: u64,
) -> Result<SessionReport> {
    let main_file = proj_dir.join(format!("{session_id}.jsonl"));
    if !main_file.exists() {
        bail!("主会话日志不存在: {}", main_file.display());
    }

    let main_report = analyze_single_jsonl(&main_file, "main", true, heavy_threshold)?;
    let task_desc_map = main_report.spawned_tasks.clone();

    let mut agents = vec![main_report];

    let subagents_dir = proj_dir.join(session_id).join("subagents");
    if subagents_dir.is_dir() {
        let mut sub_files: Vec<PathBuf> = fs::read_dir(&subagents_dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .collect();
        sub_files.sort();

        for sub_path in sub_files {
            let file_stem = sub_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown-subagent")
                .to_string();
            let raw_id = file_stem.strip_prefix("agent-").unwrap_or(&file_stem);
            let mut sub_rep = analyze_single_jsonl(&sub_path, &file_stem, false, heavy_threshold)?;
            sub_rep.task_desc = task_desc_map
                .get(raw_id)
                .or_else(|| task_desc_map.get(&file_stem))
                .cloned();
            agents.push(sub_rep);
        }
    }

    let mut file_editors: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for agent in &agents {
        let display_name = match &agent.task_desc {
            Some(desc) => format!("`{}` ({})", agent.agent_id, desc),
            None => format!("`{}`", agent.agent_id),
        };
        for (file, edit_branch) in &agent.edited_files {
            let editor = match edit_branch {
                Some(branch) => format!("{display_name} [`{branch}`]"),
                None => display_name.clone(),
            };
            file_editors.entry(file.clone()).or_default().push(editor);
        }
    }
    let file_clashes: BTreeMap<String, Vec<String>> = file_editors
        .into_iter()
        .filter(|(_, editors)| editors.len() >= 2)
        .collect();

    let merge_audits = agents
        .iter_mut()
        .flat_map(|agent| std::mem::take(&mut agent.merge_audits))
        .collect();

    Ok(SessionReport {
        project_dir: proj_dir.to_path_buf(),
        session_id: session_id.to_string(),
        agents,
        file_clashes,
        merge_audits,
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

    let mut usage_indices: HashMap<String, usize> = HashMap::new();
    let mut usage_records: Vec<(Usage, Vec<ToolInvocation>)> = Vec::new();
    let mut pending_tools: HashMap<String, ToolInvocation> = HashMap::new();
    let mut tool_errors: HashMap<String, String> = HashMap::new();
    let mut last_tools: Vec<ToolInvocation> = Vec::new();
    let mut pending_edits: HashSet<String> = HashSet::new();
    let mut last_failed_edits: HashSet<String> = HashSet::new();
    let mut current_fail_streak: usize = 0;

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<RawLine>(&line) else {
            continue;
        };

        if raw.event_type.as_deref() == Some("system")
            && raw.subtype.as_deref() == Some("compact_boundary")
            && let Some(metadata) = &raw.compact_metadata
        {
            report.compactions.push(CompactionReport {
                turn: usage_records.len(),
                before_tokens: metadata.pre_tokens,
                after_tokens: metadata.post_tokens,
            });
        }

        if let Some(branch) = &raw.git_branch
            && !branch.is_empty()
        {
            report.branch = Some(branch.clone());
        }

        let Some(msg) = raw.message.as_ref() else {
            continue;
        };

        if let Some(usage) = msg.usage {
            let dedup_key = msg.id.as_deref().or(raw.uuid.as_deref());
            let existing = dedup_key.and_then(|key| usage_indices.get(key).copied());
            if let Some(index) = existing {
                usage_records[index].0 = usage;
            } else {
                if let Some(key) = dedup_key {
                    usage_indices.insert(key.to_string(), usage_records.len());
                }
                usage_records.push((usage, std::mem::take(&mut last_tools)));
            }
        }

        let Some(Value::Array(blocks)) = msg.content.as_ref() else {
            continue;
        };

        for block in blocks {
            let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match btype {
                "tool_use" => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown")
                        .to_string();
                    let input = block.get("input");

                    let (target, has_line_limit) =
                        extract_tool_target(&name, input, raw.cwd.as_deref());
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
                    let tool_use_id = block
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let is_error = block
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if let Some(inv) = pending_tools.remove(tool_use_id) {
                        let content = block.get("content");
                        if is_error {
                            tool_errors
                                .insert(tool_use_id.to_string(), extract_result_snippet(content));
                        }
                        if !is_error
                            && inv.name == "Bash"
                            && inv.target.as_deref().is_some_and(is_merge_command)
                        {
                            let mut suspect_files: Vec<_> =
                                report.edited_files.keys().cloned().collect();
                            suspect_files.sort();
                            report.merge_audits.push(MergeAudit {
                                agent_id: agent_id.to_string(),
                                action: inv.target.clone().unwrap_or_else(|| "git merge".into()),
                                build_success: None,
                                build_error: None,
                                suspect_files,
                            });
                        }
                        if inv.name == "Bash"
                            && inv.target.as_deref().is_some_and(is_build_or_test_command)
                        {
                            let error = is_error.then(|| extract_result_snippet(content));
                            for audit in report
                                .merge_audits
                                .iter_mut()
                                .filter(|audit| audit.build_success.is_none())
                            {
                                audit.build_success = Some(!is_error);
                                audit.build_error = error.clone();
                            }
                        }
                        if is_error {
                            if inv.name == "Bash" {
                                let cmd = inv.target.unwrap_or_else(|| "bash".into());
                                if is_build_or_test_command(&cmd) {
                                    if pending_edits.is_empty() {
                                        current_fail_streak = 0;
                                        last_failed_edits.clear();
                                    } else {
                                        if pending_edits
                                            .iter()
                                            .any(|path| last_failed_edits.contains(path))
                                        {
                                            current_fail_streak += 1;
                                        } else {
                                            current_fail_streak = 1;
                                        }
                                        last_failed_edits = std::mem::take(&mut pending_edits);
                                    }
                                    report.max_consecutive_failures =
                                        report.max_consecutive_failures.max(current_fail_streak);
                                    let snippet = extract_result_snippet(content);
                                    let cmd =
                                        cmd.lines().next().unwrap_or("").chars().take(72).collect();
                                    report.failed_commands.push((cmd, snippet));
                                }
                            }
                            continue;
                        }

                        match inv.name.as_str() {
                            "Read" if !inv.has_line_limit => {
                                if let Some(path) = inv.target {
                                    *report.full_read_counts.entry(path).or_default() += 1;
                                }
                            }
                            "Edit" | "Write" | "MultiEdit" => {
                                if let Some(path) = inv.target {
                                    report
                                        .edited_files
                                        .insert(path.clone(), report.branch.clone());
                                    pending_edits.insert(path);
                                }
                            }
                            "Bash"
                                if inv.target.as_deref().is_some_and(is_build_or_test_command) =>
                            {
                                current_fail_streak = 0;
                                pending_edits.clear();
                                last_failed_edits.clear();
                            }
                            "Task" | "Agent" => {
                                if let (Some(desc), Some(sub_id)) =
                                    (inv.target, raw.spawned_agent_id(content))
                                {
                                    report.spawned_tasks.insert(sub_id, desc);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let mut prev_context_size = 0;
    for (usage, tools) in usage_records {
        report.turns += 1;
        let ctx = usage.context_size();
        let mut action_delta = 0;
        if report.turns == 1 {
            report.startup_cache_tokens = usage.cache_creation_input_tokens;
        } else if ctx > prev_context_size && !tools.is_empty() {
            let delta = ctx - prev_context_size;
            action_delta = delta / (tools.len() as u64);
            for tool in &tools {
                *report
                    .tool_token_delta
                    .entry(tool.name.clone())
                    .or_default() += action_delta;
                if action_delta >= heavy_threshold {
                    let label =
                        format!("{} ({})", tool.name, tool.target.as_deref().unwrap_or("-"));
                    report.heavy_ops.push((label, action_delta));
                }
            }
        }
        report.actions.extend(tools.iter().map(|tool| ToolAction {
            turn: report.turns,
            tool: tool.name.clone(),
            target: tool.target.clone(),
            delta: action_delta,
            error: tool_errors.get(&tool.id).cloned(),
        }));
        report.peak_context_tokens = report.peak_context_tokens.max(ctx);
        report.total_tokens += usage.total();
        prev_context_size = ctx;
    }
    report
        .actions
        .extend(last_tools.iter().map(|tool| ToolAction {
            turn: report.turns + 1,
            tool: tool.name.clone(),
            target: tool.target.clone(),
            delta: 0,
            error: tool_errors.get(&tool.id).cloned(),
        }));

    report.heavy_ops.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(report)
}

fn extract_tool_target(
    name: &str,
    input: Option<&Value>,
    cwd: Option<&str>,
) -> (Option<String>, bool) {
    let Some(obj) = input.and_then(|v| v.as_object()) else {
        return (None, false);
    };
    match name {
        "Read" => {
            let path = obj
                .get("file_path")
                .and_then(|v| v.as_str())
                .map(|path| normalize_file_path(path, cwd));
            let limited = obj.contains_key("limit") || obj.contains_key("offset");
            (path, limited)
        }
        "Edit" | "Write" | "MultiEdit" => {
            let path = obj
                .get("file_path")
                .and_then(|v| v.as_str())
                .map(|path| normalize_file_path(path, cwd));
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
                .map(String::from);
            (cmd, false)
        }
        "Task" | "Agent" => {
            let desc = obj
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.chars().take(48).collect());
            (desc, false)
        }
        _ => (None, false),
    }
}

fn normalize_file_path(path: &str, cwd: Option<&str>) -> String {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(cwd) = cwd {
        let cwd = cwd.strip_prefix(r"\\?\").unwrap_or(cwd);
        Path::new(cwd).join(path)
    } else {
        path.to_path_buf()
    };

    let mut clean = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                clean.pop();
            }
            _ => clean.push(component.as_os_str()),
        }
    }
    let normalized = clean.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        normalized.to_lowercase()
    } else {
        normalized
    }
}

fn is_merge_command(command: &str) -> bool {
    command.split(['&', '|', ';', '\n']).any(|segment| {
        let mut words = segment
            .split_whitespace()
            .skip_while(|word| word.contains('='));
        let Some(executable) = words.next() else {
            return false;
        };
        Path::new(executable)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(executable)
            .trim_end_matches(".exe")
            .eq_ignore_ascii_case("git")
            && words.next() == Some("merge")
    })
}

// shortcut: 仅识别常见构建/测试命令，支持其他工具链时扩展此列表。
fn is_build_or_test_command(command: &str) -> bool {
    command.split(['&', '|', ';', '\n']).any(|segment| {
        let mut words = segment
            .split_whitespace()
            .skip_while(|word| word.contains('='));
        let Some(executable) = words.next() else {
            return false;
        };
        let executable = Path::new(executable)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(executable)
            .trim_end_matches(".exe")
            .trim_end_matches(".bat")
            .to_ascii_lowercase();
        match executable.as_str() {
            "cargo" | "go" => words.any(|word| matches!(word, "build" | "test" | "check")),
            "npm" | "pnpm" | "yarn" | "bun" => words.any(|word| matches!(word, "build" | "test")),
            "pytest" | "jest" | "vitest" | "make" => true,
            _ => false,
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);

    fn tool_call(id: &str, name: &str, input: Value, cwd: Option<&str>) -> String {
        json!({
            "cwd": cwd,
            "message": {"content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}
        })
        .to_string()
    }

    fn tool_result(id: &str, is_error: bool) -> String {
        json!({
            "message": {"content": [{
                "type": "tool_result", "tool_use_id": id, "content": "result", "is_error": is_error
            }]}
        })
        .to_string()
    }

    fn analyze(lines: Vec<String>) -> AgentReport {
        let file_id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "spec-xray-test-{}-{file_id}.jsonl",
            std::process::id()
        ));
        fs::write(&path, lines.join("\n")).unwrap();
        let report = analyze_single_jsonl(&path, "test", false, 10_000).unwrap();
        let _ = fs::remove_file(path);
        report
    }

    #[test]
    fn reads_structured_agent_id() {
        let report = analyze(vec![
            tool_call(
                "task-1",
                "Agent",
                json!({"description": "Fix parser"}),
                None,
            ),
            json!({
                "toolUseResult": {"agentId": "agent-123"},
                "message": {"content": [{
                    "type": "tool_result", "tool_use_id": "task-1", "content": "done"
                }]}
            })
            .to_string(),
        ]);
        assert_eq!(
            report.spawned_tasks.get("agent-123").map(String::as_str),
            Some("Fix parser")
        );
    }

    #[test]
    fn uses_final_usage_snapshot() {
        let report = analyze(vec![
            json!({"message": {"id": "msg-1", "content": [], "usage": {
                "input_tokens": 2, "cache_read_input_tokens": 5, "output_tokens": 1
            }}})
            .to_string(),
            json!({"message": {"id": "msg-1", "content": [], "usage": {
                "input_tokens": 2, "cache_creation_input_tokens": 3,
                "cache_read_input_tokens": 5, "output_tokens": 500
            }}})
            .to_string(),
        ]);
        assert_eq!(report.turns, 1);
        assert_eq!(report.startup_cache_tokens, 3);
        assert_eq!(report.peak_context_tokens, 10);
        assert_eq!(report.total_tokens, 510);
    }

    #[test]
    fn ignores_failed_reads_and_edits() {
        let path = if cfg!(windows) {
            "E:/repo/a.rs"
        } else {
            "/repo/a.rs"
        };
        let read = |id| tool_call(id, "Read", json!({"file_path": path}), None);
        let report = analyze(vec![
            read("read-1"),
            tool_result("read-1", true),
            read("read-2"),
            tool_result("read-2", true),
            read("read-3"),
            tool_result("read-3", false),
            tool_call(
                "edit-1",
                "Edit",
                json!({"file_path": path, "old_string": "a", "new_string": "b"}),
                None,
            ),
            tool_result("edit-1", true),
        ]);
        assert_eq!(report.full_read_counts.values().sum::<usize>(), 1);
        assert!(report.edited_files.is_empty());
    }

    #[test]
    fn counts_only_same_file_build_retries() {
        let cwd = if cfg!(windows) { "E:/repo" } else { "/repo" };
        let mut lines = Vec::new();
        for index in 0..3 {
            let id = format!("ls-{index}");
            lines.push(tool_call(
                &id,
                "Bash",
                json!({"command": "ls missing"}),
                None,
            ));
            lines.push(tool_result(&id, true));
        }
        for index in 0..3 {
            let edit_id = format!("edit-{index}");
            let test_id = format!("test-{index}");
            lines.push(tool_call(
                &edit_id,
                "Edit",
                json!({"file_path": "src/a.rs", "old_string": "a", "new_string": "b"}),
                Some(cwd),
            ));
            lines.push(tool_result(&edit_id, false));
            lines.push(tool_call(
                &test_id,
                "Bash",
                json!({"command": "cargo test"}),
                None,
            ));
            lines.push(tool_result(&test_id, true));
        }
        let report = analyze(lines);
        assert_eq!(report.max_consecutive_failures, 3);
        assert_eq!(report.failed_commands.len(), 3);
        assert!(
            report
                .failed_commands
                .iter()
                .all(|(cmd, _)| cmd == "cargo test")
        );
    }

    #[test]
    fn normalizes_read_and_edit_paths() {
        let cwd = if cfg!(windows) {
            "E:/Repo"
        } else {
            "/tmp/Repo"
        };
        let absolute = Path::new(cwd)
            .join("src/./lib.rs")
            .to_string_lossy()
            .to_string();
        let absolute = if cfg!(windows) {
            absolute.replace('/', "\\")
        } else {
            absolute
        };
        let expected = normalize_file_path("src/lib.rs", Some(cwd));
        let mut lines = Vec::new();
        for (index, path) in ["src/lib.rs".to_string(), absolute].into_iter().enumerate() {
            let read_id = format!("read-{index}");
            let edit_id = format!("edit-{index}");
            lines.push(tool_call(
                &read_id,
                "Read",
                json!({"file_path": path}),
                Some(cwd),
            ));
            lines.push(tool_result(&read_id, false));
            lines.push(tool_call(
                &edit_id,
                "Write",
                json!({"file_path": path, "content": "fn main() {}"}),
                Some(cwd),
            ));
            lines.push(tool_result(&edit_id, false));
        }
        let report = analyze(lines);
        assert_eq!(report.full_read_counts.get(&expected), Some(&2));
        assert_eq!(report.full_read_counts.len(), 1);
        assert!(report.edited_files.contains_key(&expected));
        assert_eq!(report.edited_files.len(), 1);
    }

    #[test]
    fn records_branch_for_each_edited_file() {
        let edit = |id: &str, path: &str, branch: &str| {
            json!({
                "gitBranch": branch,
                "message": {"content": [{
                    "type": "tool_use", "id": id, "name": "Edit",
                    "input": {"file_path": path, "old_string": "a", "new_string": "b"}
                }]}
            })
            .to_string()
        };
        let (first, second) = if cfg!(windows) {
            ("E:/repo/a.rs", "E:/repo/b.rs")
        } else {
            ("/repo/a.rs", "/repo/b.rs")
        };
        let report = analyze(vec![
            edit("edit-1", first, "master"),
            tool_result("edit-1", false),
            edit("edit-2", second, "feature/x"),
            tool_result("edit-2", false),
        ]);
        assert_eq!(report.branch.as_deref(), Some("feature/x"));
        assert_eq!(
            report
                .edited_files
                .get(&normalize_file_path(first, None))
                .and_then(|branch| branch.as_deref()),
            Some("master")
        );
        assert_eq!(
            report
                .edited_files
                .get(&normalize_file_path(second, None))
                .and_then(|branch| branch.as_deref()),
            Some("feature/x")
        );
    }

    #[test]
    fn records_compact_boundaries_without_usage_inference() {
        let usage = |id: &str, context: u64| {
            json!({"message": {
                "id": id, "content": [], "usage": {"input_tokens": context}
            }})
            .to_string()
        };
        let report = analyze(vec![
            usage("msg-1", 100),
            json!({
                "type": "system", "subtype": "compact_boundary",
                "compactMetadata": {"trigger": "auto", "preTokens": 80}
            })
            .to_string(),
            usage("msg-2", 200),
            json!({
                "type": "system", "subtype": "compact_boundary",
                "compactMetadata": {"trigger": "auto", "preTokens": 180, "postTokens": 20}
            })
            .to_string(),
        ]);
        assert_eq!(report.compactions.len(), 2);
        assert_eq!(report.compactions[0].turn, 1);
        assert_eq!(report.compactions[0].before_tokens, 80);
        assert_eq!(report.compactions[0].after_tokens, None);
        assert_eq!(report.compactions[1].turn, 2);
        assert_eq!(report.compactions[1].before_tokens, 180);
        assert_eq!(report.compactions[1].after_tokens, Some(20));

        let markdown = crate::report::render_markdown(&SessionReport {
            project_dir: std::env::temp_dir().join("spec-xray-render-test-no-logs"),
            session_id: "test".to_string(),
            agents: vec![report],
            file_clashes: BTreeMap::new(),
            merge_audits: Vec::new(),
        });
        assert!(markdown.contains("Compaction（已完成 1 轮后）"));
        assert!(markdown.contains("80 → 未知 tokens"));
        assert!(markdown.contains("Compaction（已完成 2 轮后）"));
        assert!(markdown.contains("180 → 20 tokens（下降 160）"));
    }

    #[test]
    fn records_action_delta_and_error() {
        let report = analyze(vec![
            json!({"message": {"id": "msg-1", "content": [], "usage": {
                "input_tokens": 100
            }}})
            .to_string(),
            tool_call("read-1", "Read", json!({"file_path": "a.rs"}), None),
            json!({"message": {"content": [{
                "type": "tool_result", "tool_use_id": "read-1", "content": "missing file",
                "is_error": true
            }]}})
            .to_string(),
            json!({"message": {"id": "msg-2", "content": [], "usage": {
                "input_tokens": 160
            }}})
            .to_string(),
        ]);
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].turn, 2);
        assert_eq!(report.actions[0].delta, 60);
        assert_eq!(report.actions[0].error.as_deref(), Some("missing file"));
    }

    #[test]
    fn audits_first_build_after_merge() {
        let report = analyze(vec![
            tool_call(
                "edit-1",
                "Edit",
                json!({"file_path": "src/a.rs", "old_string": "a", "new_string": "b"}),
                None,
            ),
            tool_result("edit-1", false),
            tool_call(
                "merge-1",
                "Bash",
                json!({"command": "git merge feature"}),
                None,
            ),
            tool_result("merge-1", false),
            tool_call("build-1", "Bash", json!({"command": "cargo test"}), None),
            tool_result("build-1", true),
        ]);
        assert_eq!(report.merge_audits.len(), 1);
        assert_eq!(report.merge_audits[0].build_success, Some(false));
        assert_eq!(report.merge_audits[0].suspect_files.len(), 1);
        assert!(report.merge_audits[0].build_error.is_some());
    }

    #[test]
    fn ignores_microcompact_context_drops() {
        let usage = |id: &str, context: u64| {
            json!({"message": {
                "id": id, "content": [], "usage": {"input_tokens": context}
            }})
            .to_string()
        };
        let report = analyze(vec![
            usage("msg-1", 200),
            json!({
                "type": "system", "subtype": "microcompact_boundary",
                "microcompactMetadata": {
                    "preTokens": 200, "tokensSaved": 180, "compactedToolIds": ["tool-1"]
                }
            })
            .to_string(),
            usage("msg-2", 20),
        ]);
        assert!(report.compactions.is_empty());
    }
}
