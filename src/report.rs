use crate::analyzer::SessionReport;
use indoc::indoc;

pub fn render_markdown(report: &SessionReport) -> String {
    let total_all: u64 = report.agents.iter().map(|a| a.total_tokens).sum();

    let header = format!(
        indoc! {r#"
            # spec-xray 会话诊断报告
            - **Session ID**: `{}`
            - **Agent 总数**: {} (1 Main + {} Subagents)
            - **累计 Token**: {} ({:.2}M)

        "#},
        report.session_id,
        report.agents.len(),
        report.agents.len().saturating_sub(1),
        total_all,
        total_all as f64 / 1_000_000.0,
    );

    // 1. Subagent 消耗与健康度总览表
    let agent_rows = report
        .agents
        .iter()
        .map(|a| {
            let desc =
                a.task_desc
                    .as_deref()
                    .unwrap_or(if a.is_main { "Orchestrator" } else { "-" });
            let branch = a.branch.as_deref().unwrap_or("-");
            let warn = if a.max_consecutive_failures >= 3 {
                format!("⚠️ {} 次", a.max_consecutive_failures)
            } else {
                a.max_consecutive_failures.to_string()
            };
            format!(
                "| `{}` | {} | `{}` | {} | {}k | {}k | {}k | {} |\n",
                a.agent_id,
                desc,
                branch,
                a.turns,
                a.startup_cache_tokens / 1000,
                a.peak_context_tokens / 1000,
                a.total_tokens / 1000,
                warn,
            )
        })
        .collect::<String>();
    let overview = format!(
        indoc! {r#"
            ## 1. Subagent 上下文与健康度总览
            | Agent | 任务描述 | 分支 | 轮次 | 冷启动 | 峰值上下文 | 累计 Token | 连续失败 |
            | :--- | :--- | :--- | ---: | ---: | ---: | ---: | ---: |
            {agent_rows}
        "#},
        agent_rows = agent_rows,
    );

    // 2. 跨 Subagent 文件修改碰撞（分支合并高危区）
    let clashes = if report.file_clashes.is_empty() {
        indoc! {r#"
            ## 2. 跨 Subagent 文件修改交集（合并冲突雷达）
            _未检测到多个 Agent 修改同一文件的情况。_

        "#}
        .to_owned()
    } else {
        let rows = report
            .file_clashes
            .iter()
            .map(|(file, editors)| format!("| `{file}` | {} |", editors.join(", ")))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            indoc! {r#"
                ## 2. 跨 Subagent 文件修改交集（合并冲突雷达）
                | 被多方修改的文件 | 涉及 Agent |
                | :--- | :--- |
                {rows}

            "#},
            rows = rows,
        )
    };

    // 3. 上下文膨胀归因（按工具分类占比 + 重复全量读取 + 大体积返回）
    let bloat_entries = report
        .agents
        .iter()
        .filter_map(|a| {
            let dup_reads: Vec<_> = a
                .full_read_counts
                .iter()
                .filter(|&(_, &c)| c >= 2)
                .collect();
            let mut tool_breakdown: Vec<_> = a.tool_token_delta.iter().collect();
            tool_breakdown.sort_by(|x, y| y.1.cmp(x.1));

            if dup_reads.is_empty() && a.heavy_ops.is_empty() && tool_breakdown.is_empty() {
                return None;
            }

            let title = match &a.task_desc {
                Some(d) => format!("`{}` ({})", a.agent_id, d),
                None => format!("`{}`", a.agent_id),
            };
            let mut lines = vec![format!("### {title}")];

            if !tool_breakdown.is_empty() {
                let summary: Vec<String> = tool_breakdown
                    .into_iter()
                    .filter(|&(_, &tok)| tok >= 1_000)
                    .map(|(tool, tok)| format!("{}: +{}k", tool, tok / 1000))
                    .collect();
                if !summary.is_empty() {
                    lines.push(format!("- **工具增量分布**: {}", summary.join(" | ")));
                }
            }
            lines.extend(
                dup_reads
                    .into_iter()
                    .map(|(path, count)| format!("- **重复全量 Read ({count} 次)**: `{path}`")),
            );
            lines.extend(a.heavy_ops.iter().take(5).map(|(op, delta)| {
                format!("- **单步上下文激增 (+{}k tok)**: `{op}`", delta / 1000)
            }));

            Some(lines.join("\n"))
        })
        .collect::<Vec<_>>();
    let bloat_body = if bloat_entries.is_empty() {
        "_未检测到显著的上下文膨胀。_".to_owned()
    } else {
        bloat_entries.join("\n\n")
    };
    let bloat = format!(
        indoc! {r#"
            ## 3. 上下文污染与膨胀归因
            {bloat_body}

        "#},
        bloat_body = bloat_body,
    );

    // 4. 构建/测试失败摘要
    let failure_entries = report
        .agents
        .iter()
        .filter(|a| !a.failed_commands.is_empty())
        .map(|a| {
            let commands = a
                .failed_commands
                .iter()
                .rev()
                .take(3)
                .rev()
                .map(|(cmd, snippet)| format!("- 命令: `{cmd}`\n  - 报错摘要: `{snippet}`"))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "### `{}` (共 {} 次失败)\n{commands}",
                a.agent_id,
                a.failed_commands.len(),
            )
        })
        .collect::<Vec<_>>();
    let failures = if failure_entries.is_empty() {
        indoc! {r#"
            ## 4. Bash 构建与测试失败记录
            _未捕获到 is_error=true 的 Bash 命令失败。_
        "#}
        .to_owned()
    } else {
        let entries = failure_entries.join("\n\n");
        format!(
            indoc! {r#"
                ## 4. Bash 构建与测试失败记录
                {entries}

            "#},
            entries = entries,
        )
    };

    format!("{header}{overview}{clashes}{bloat}{failures}")
}
