use crate::parser::tree::{SessionTopology, TopologyNode};
use chrono::{DateTime, Utc};
use std::fmt::Write;

/// CLI 与 TUI 共用的纯文本甘特图；width 仅控制时间轨道宽度。
pub fn render_gantt(topology: &SessionTopology, width: usize) -> String {
    let width = width.max(4);
    let mut nodes = Vec::new();
    flatten(&topology.root, &mut nodes);
    for orphan in &topology.orphans {
        flatten(orphan, &mut nodes);
    }
    let Some(start) = nodes.iter().filter_map(|node| node.start_time).min() else {
        return "并行甘特图: 暂无有效时间戳。".into();
    };
    let end = nodes
        .iter()
        .filter_map(|node| node.end_time)
        .max()
        .unwrap_or(start);
    let total_ms = (end - start).num_milliseconds().max(1);
    let total_secs = (end - start).num_seconds().max(0);
    let mut subs: Vec<_> = nodes
        .iter()
        .copied()
        .filter(|node| node.agent_id != "main")
        .collect();
    subs.sort_by_key(|node| (node.start_time, node.agent_id.as_str()));

    let mut events = Vec::new();
    let mut durations = Vec::new();
    for node in &subs {
        if let (Some(from), Some(to)) = (node.start_time, node.end_time)
            && to > from
        {
            events.push(((from - start).num_milliseconds(), 1));
            events.push(((to - start).num_milliseconds(), -1));
            durations.push((to - from).num_milliseconds());
        }
    }
    events.sort_unstable();
    let (mut active, mut peak) = (0, 0);
    for (_, delta) in events {
        active += delta;
        peak = peak.max(active);
    }
    let busy_ms: i64 = durations.iter().sum();
    let speedup = busy_ms as f64 / total_ms as f64;
    let mut sorted_durations = durations.clone();
    sorted_durations.sort_unstable();
    let median = median(&sorted_durations);
    let last_subagent_end = subs.iter().filter_map(|node| node.end_time).max();

    let mut out = format!(
        "全局跨度: {} {} {} | 峰值并行: {} | 加速比: {:.1}x",
        clock(0),
        "─".repeat(width),
        clock(total_secs),
        peak,
        speedup
    );
    if subs.len() >= 2 && peak == 1 {
        out.push_str(" | ⚠️ 串行排队");
    }
    out.push('\n');
    out.push_str("图例: █ 活跃执行  ░ 等待/未启动\n");

    for node in std::iter::once(&topology.root).chain(subs.iter().copied()) {
        let (Some(from), Some(to)) = (node.start_time, node.end_time) else {
            continue;
        };
        let mut track = vec!['░'; width];
        if node.agent_id == "main"
            && let (Some(dispatch), Some(sub_end)) = (topology.first_dispatch, last_subagent_end)
        {
            paint(&mut track, from, dispatch.min(to), start, total_ms);
            paint(&mut track, sub_end.max(from), to, start, total_ms);
        } else {
            paint(&mut track, from, to, start, total_ms);
        }
        let duration_ms = (to - from).num_milliseconds().max(0);
        let share = duration_ms as f64 / total_ms as f64;
        let tail = (node.agent_id != "main"
            && durations.len() >= 2
            && share >= 0.5
            && duration_ms as f64 > median * 2.0)
            .then(|| format!(" ⚠️长尾({}%)", (share * 100.0).round() as i64))
            .unwrap_or_default();
        let id: String = node.agent_id.chars().take(12).collect();
        let description = if node.agent_id == "main" {
            "Orchestrator (编排/合并)".to_string()
        } else {
            node.label.chars().take(36).collect()
        };
        writeln!(
            out,
            "{id:<12} │{}│ {:>4}s  {description}{tail}",
            track.into_iter().collect::<String>(),
            duration_ms / 1000
        )
        .unwrap();
    }

    out.push_str("编排空窗:\n");
    let mut warning_count = 0;
    if let Some(dispatch) = topology.first_dispatch {
        let gap = (dispatch - start).num_seconds();
        if gap > 30 {
            writeln!(out, "- ⚠️ Pre-Dispatch Gap: {gap}s（首次派发前）").unwrap();
            warning_count += 1;
        }
    }
    let workers: Vec<_> = subs
        .iter()
        .copied()
        .filter(|node| !is_merger(node))
        .collect();
    if !workers.is_empty() && workers.iter().all(|node| node.end_time.is_some()) {
        let ready = workers
            .iter()
            .filter_map(|node| node.end_time)
            .max()
            .unwrap();
        let merge = topology
            .merge_events
            .iter()
            .filter(|event| {
                event.agent_id == "main"
                    || nodes
                        .iter()
                        .any(|node| node.agent_id == event.agent_id && is_merger(node))
            })
            .filter_map(|event| event.timestamp)
            .filter(|timestamp| *timestamp >= ready)
            .min();
        let gap = (merge.unwrap_or(end) - ready).num_seconds();
        if gap > 30 {
            let suffix = if merge.is_some() {
                "全部worker结束后至首次合并"
            } else {
                "全部worker结束后未检测到合并"
            };
            writeln!(out, "- ⚠️ Post-Subagent Merge Gap: {gap}s（{suffix}）").unwrap();
            warning_count += 1;
        }
    }
    if warning_count == 0 {
        out.push_str("- 无（阈值 >30s）\n");
    }
    out
}

fn flatten<'a>(node: &'a TopologyNode, nodes: &mut Vec<&'a TopologyNode>) {
    nodes.push(node);
    for child in &node.children {
        flatten(child, nodes);
    }
}

fn is_merger(node: &TopologyNode) -> bool {
    node.label.to_ascii_lowercase().contains("merg")
        || node.agent_id.to_ascii_lowercase().contains("merg")
        || node.label.contains("合并")
}

fn median(values: &[i64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) as f64 / 2.0
    } else {
        values[middle] as f64
    }
}

fn paint(
    track: &mut [char],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    base: DateTime<Utc>,
    total_ms: i64,
) {
    if to <= from {
        return;
    }
    let width = track.len();
    let offset = |time: DateTime<Utc>| {
        (time - base).num_milliseconds().clamp(0, total_ms) as f64 / total_ms as f64 * width as f64
    };
    let left = (offset(from).floor() as usize).min(width.saturating_sub(1));
    let right = ((offset(to).ceil() as usize).max(left + 1)).min(width);
    track[left..right].fill('█');
}

fn clock(seconds: i64) -> String {
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::tree::MergeEvent;
    use chrono::TimeZone;

    fn node(id: &str, label: &str, from: i64, to: i64) -> TopologyNode {
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        TopologyNode {
            agent_id: id.into(),
            label: label.into(),
            branch: None,
            start_time: Some(base + chrono::Duration::seconds(from)),
            end_time: Some(base + chrono::Duration::seconds(to)),
            total_tokens: 0,
            max_consecutive_failures: 0,
            has_errors: false,
            children: Vec::new(),
        }
    }

    fn topology(main: TopologyNode, children: Vec<TopologyNode>) -> SessionTopology {
        SessionTopology {
            root: TopologyNode { children, ..main },
            orphans: Vec::new(),
            first_dispatch: None,
            merge_events: Vec::new(),
        }
    }

    #[test]
    fn renders_parallelism_tail_and_merge_gap() {
        let mut topology = topology(
            node("main", "Main", 0, 400),
            vec![
                node("agent-a", "Ticket A", 40, 100),
                node("agent-b", "Ticket B", 40, 340),
                node("agent-c", "Ticket C", 100, 160),
            ],
        );
        topology.first_dispatch = topology.root.children[0]
            .start_time
            .map(|start| start - chrono::Duration::seconds(20));
        topology.merge_events.push(MergeEvent {
            agent_id: "main".into(),
            action: "git merge topic".into(),
            timestamp: topology
                .root
                .end_time
                .map(|end| end - chrono::Duration::seconds(20)),
        });
        let text = render_gantt(&topology, 40);
        assert!(text.contains("峰值并行: 2 | 加速比: 1.1x"), "{text}");
        assert!(text.contains("⚠️长尾(75%)"), "{text}");
        assert!(text.contains("Post-Subagent Merge Gap: 40s"), "{text}");
        assert!(!text.contains("Pre-Dispatch Gap"), "{text}");
        let report = crate::analyzer::analyzer::build_session_report(
            std::path::Path::new("."),
            "test",
            Vec::new(),
        );
        let markdown = crate::report::render_markdown_with_topology(&report, Some(&topology));
        assert!(markdown.contains("## 0.1 并行甘特图与编排空窗"));
        assert!(markdown.contains("⚠️长尾(75%)"));
    }

    #[test]
    fn flags_serial_dispatch_and_cold_start() {
        let mut topology = topology(
            node("main", "Main", 0, 200),
            vec![
                node("agent-a", "Ticket A", 50, 80),
                node("agent-b", "Ticket B", 100, 130),
            ],
        );
        topology.first_dispatch = topology.root.children[0].start_time;
        let text = render_gantt(&topology, 20);
        assert!(text.contains("⚠️ 串行排队"), "{text}");
        assert!(text.contains("Pre-Dispatch Gap: 50s"), "{text}");
        assert!(text.contains("Post-Subagent Merge Gap: 70s"), "{text}");
        topology.root.children[1].end_time = None;
        let text = render_gantt(&topology, 20);
        assert!(!text.contains("Post-Subagent Merge Gap"), "{text}");
    }

    #[test]
    fn measures_gap_to_merger_subagent() {
        let mut topology = topology(
            node("main", "Main", 0, 300),
            vec![
                node("agent-worker", "Worker", 40, 100),
                node("agent-merger", "Merger", 100, 200),
            ],
        );
        topology.merge_events.push(MergeEvent {
            agent_id: "agent-merger".into(),
            action: "git rebase main".into(),
            timestamp: topology.root.children[1]
                .start_time
                .map(|start| start + chrono::Duration::seconds(50)),
        });
        let text = render_gantt(&topology, 20);
        assert!(text.contains("Post-Subagent Merge Gap: 50s"), "{text}");
    }

    #[test]
    fn captures_cherry_pick_during_existing_link_scan() {
        let raw: crate::parser::schema::RawLine = serde_json::from_value(serde_json::json!({
            "timestamp": "2026-01-01T00:02:00Z",
            "message": {"content": [{"type": "tool_use", "id": "git-1", "name": "Bash",
                "input": {"command": "git.exe cherry-pick abc123"}}]}
        }))
        .unwrap();
        let mut meta = crate::parser::tree::AgentLinkMeta::new("main");
        meta.observe(&raw);
        assert_eq!(meta.merge_events.len(), 1);
        assert_eq!(meta.merge_events[0].action, "git.exe cherry-pick abc123");
    }
}
