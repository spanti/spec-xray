use crate::{analyzer::analyzer::SessionReport, format_tokens, gantt::render_gantt, parser::tree};
use indoc::indoc;

pub fn render_markdown(report: &SessionReport) -> String {
    let topology =
        tree::build_topology(&report.project_dir, &report.session_id, &report.agents).ok();
    render_markdown_with_topology(report, topology.as_ref())
}

/// 使用调用方已有的拓扑渲染报告，避免实时视图为了 Markdown 重读日志。
pub fn render_markdown_with_topology(
    report: &SessionReport,
    cached_topology: Option<&tree::SessionTopology>,
) -> String {
    let total_all: u64 = report.agents.iter().map(|a| a.total_tokens).sum();

    let header = format!(
        indoc! {r#"
            # spec-xray 会话诊断报告
            - **Session ID**: `{}`
            - **Agent 总数**: {} (1 Main + {} Subagents)
            - **累计 Token**: {}

        "#},
        report.session_id,
        report.agents.len(),
        report.agents.len().saturating_sub(1),
        format_tokens(total_all),
    );

    // 0. 执行拓扑与时间轴树
    let topology: String = cached_topology
        .map(|topology| {
            let ascii_tree = tree::render_ascii_tree(topology);
            format!(
                indoc! {r#"
                ## 0. Subagent 执行拓扑与耗时
                ```text
                {ascii_tree}```

            "#},
                ascii_tree = ascii_tree,
            )
        })
        .unwrap_or_default();

    let gantt = cached_topology
        .map(|topology| {
            format!(
                "## 0.1 并行甘特图与编排空窗\n```text\n{}\n```\n\n",
                render_gantt(topology, 40)
            )
        })
        .unwrap_or_default();

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
                "| `{}` | {} | `{}` | {} | {} | {} | {} | {} |\n",
                a.agent_id,
                desc,
                branch,
                a.turns,
                format_tokens(a.startup_cache_tokens),
                format_tokens(a.peak_context_tokens),
                format_tokens(a.total_tokens),
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

            if dup_reads.is_empty()
                && a.heavy_ops.is_empty()
                && tool_breakdown.is_empty()
                && a.compactions.is_empty()
            {
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
                    .map(|(tool, tok)| format!("{}: +{}", tool, format_tokens(*tok)))
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
                format!(
                    "- **单步上下文激增 (+{} tok)**: `{op}`",
                    format_tokens(*delta)
                )
            }));
            lines.extend(a.compactions.iter().map(|c| match c.after_tokens {
                Some(after) => format!(
                    "- **Compaction（已完成 {} 轮后）**: {} → {} tokens（下降 {}）",
                    c.turn,
                    format_tokens(c.before_tokens),
                    format_tokens(after),
                    format_tokens(i128::from(c.before_tokens) - i128::from(after))
                ),
                None => format!(
                    "- **Compaction（已完成 {} 轮后）**: {} → 未知 tokens",
                    c.turn,
                    format_tokens(c.before_tokens)
                ),
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

    format!("{header}{topology}{gantt}{overview}{clashes}{bloat}{failures}")
}
