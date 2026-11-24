use crate::analyzer::SessionReport;
use std::fmt::Write;

pub fn render_markdown(report: &SessionReport) -> String {
    let mut out = String::new();
    let total_all: u64 = report.agents.iter().map(|a| a.total_tokens).sum();

    writeln!(out, "# spec-xray 会话诊断报告").unwrap();
    writeln!(out, "- **Session ID**: `{}`", report.session_id).unwrap();
    writeln!(
        out,
        "- **Agent 总数**: {} (1 Main + {} Subagents)",
        report.agents.len(),
        report.agents.len().saturating_sub(1)
    )
    .unwrap();
    writeln!(
        out,
        "- **累计 Token**: {} ({:.2}M)\n",
        total_all,
        total_all as f64 / 1_000_000.0
    )
    .unwrap();

    // 1. Subagent 消耗与健康度总览表
    writeln!(out, "## 1. Subagent 上下文与健康度总览").unwrap();
    writeln!(
        out,
        "| Agent | 任务描述 | 分支 | 轮次 | 冷启动 | 峰值上下文 | 累计 Token | 连续失败 |"
    )
    .unwrap();
    writeln!(out, "| :--- | :--- | :--- | ---: | ---: | ---: | ---: | ---: |").unwrap();
    for a in &report.agents {
        let desc = a
            .task_desc
            .as_deref()
            .unwrap_or(if a.is_main { "Orchestrator" } else { "-" });
        let branch = a.branch.as_deref().unwrap_or("-");
        let warn = if a.max_consecutive_failures >= 3 {
            format!("⚠️ {} 次", a.max_consecutive_failures)
        } else {
            a.max_consecutive_failures.to_string()
        };
        writeln!(
            out,
            "| `{}` | {} | `{}` | {} | {}k | {}k | {}k | {} |",
            a.agent_id,
            desc,
            branch,
            a.turns,
            a.startup_cache_tokens / 1000,
            a.peak_context_tokens / 1000,
            a.total_tokens / 1000,
            warn
        )
        .unwrap();
    }
    writeln!(out).unwrap();

    // 2. 跨 Subagent 文件修改碰撞（分支合并高危区）
    writeln!(out, "## 2. 跨 Subagent 文件修改交集（合并冲突雷达）").unwrap();
    if report.file_clashes.is_empty() {
        writeln!(out, "_未检测到多个 Agent 修改同一文件的情况。_\n").unwrap();
    } else {
        writeln!(out, "| 被多方修改的文件 | 涉及 Agent |").unwrap();
        writeln!(out, "| :--- | :--- |").unwrap();
        for (file, editors) in &report.file_clashes {
            writeln!(out, "| `{}` | {} |", file, editors.join(", ")).unwrap();
        }
        writeln!(out).unwrap();
    }

    // 3. 上下文膨胀归因（按工具分类占比 + 重复全量读取 + 大体积返回）
    writeln!(out, "## 3. 上下文污染与膨胀归因").unwrap();
    let mut has_bloat = false;
    for a in &report.agents {
        let dup_reads: Vec<_> = a.full_read_counts.iter().filter(|&(_, &c)| c >= 2).collect();
        let mut tool_breakdown: Vec<_> = a.tool_token_delta.iter().collect();
        tool_breakdown.sort_by(|x, y| y.1.cmp(x.1));

        if dup_reads.is_empty() && a.heavy_ops.is_empty() && tool_breakdown.is_empty() {
            continue;
        }
        has_bloat = true;
        let title = match &a.task_desc {
            Some(d) => format!("`{}` ({})", a.agent_id, d),
            None => format!("`{}`", a.agent_id),
        };
        writeln!(out, "### {}", title).unwrap();

        if !tool_breakdown.is_empty() {
            let summary: Vec<String> = tool_breakdown
                .into_iter()
                .filter(|& (_, &tok)| tok >= 1_000)
                .map(|(tool, tok)| format!("{}: +{}k", tool, tok / 1000))
                .collect();
            if !summary.is_empty() {
                writeln!(out, "- **工具增量分布**: {}", summary.join(" | ")).unwrap();
            }
        }
        for (path, count) in dup_reads {
            writeln!(out, "- **重复全量 Read ({} 次)**: `{}`", count, path).unwrap();
        }
        for (op, delta) in a.heavy_ops.iter().take(5) {
            writeln!(out, "- **单步上下文激增 (+{}k tok)**: `{}`", delta / 1000, op).unwrap();
        }
        writeln!(out).unwrap();
    }
    if !has_bloat {
        writeln!(out, "_未检测到显著的上下文膨胀。_\n").unwrap();
    }

    // 4. 构建/测试失败摘要
    writeln!(out, "## 4. Bash 构建与测试失败记录").unwrap();
    let mut has_fail = false;
    for a in &report.agents {
        if a.failed_commands.is_empty() {
            continue;
        }
        has_fail = true;
        writeln!(out, "### `{}` (共 {} 次失败)", a.agent_id, a.failed_commands.len()).unwrap();
        for (cmd, snippet) in a.failed_commands.iter().rev().take(3).rev() {
            writeln!(out, "- 命令: `{}`\n  - 报错摘要: `{}`", cmd, snippet).unwrap();
        }
        writeln!(out).unwrap();
    }
    if !has_fail {
        writeln!(out, "_未捕获到 is_error=true 的 Bash 命令失败。_").unwrap();
    }

    out
}