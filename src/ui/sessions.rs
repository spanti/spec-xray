use crate::{
    analyzer::analyzer::{
        extract_tool_target, is_build_or_test_command, is_merge_command, list_sessions,
    },
    parser::schema::RawLine,
};
use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
    time::SystemTime,
};

pub struct ProjectSessions {
    pub sessions: Vec<SessionSummary>,
}

pub struct SessionSummary {
    pub id: String,
    pub start: DateTime<Utc>,
    pub last: DateTime<Utc>,
    pub turns: usize,
    pub agents: usize,
    pub tokens: u64,
    pub dispatched: usize,
    pub had_error: bool,
    pub failed: bool,
    pub compacted: bool,
    pub ended: bool,
    pub prompt: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Live,
    Fail,
    Warn,
    Ok,
}

impl SessionStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Live => "[LIVE]",
            Self::Fail => "[FAIL]",
            Self::Warn => "[WARN]",
            Self::Ok => "[OK]",
        }
    }
}

#[derive(Default)]
struct Scan {
    start: Option<DateTime<Utc>>,
    last: Option<DateTime<Utc>>,
    message_ids: HashSet<String>,
    spawn_ids: HashSet<String>,
    turns_without_id: usize,
    spawns_without_id: usize,
    tokens: u64,
    had_error: bool,
    build_error: bool,
    merge_error: bool,
    max_failures: usize,
    compacted: bool,
    ended: bool,
    prompt: Option<String>,
}

impl ProjectSessions {
    pub fn scan(project_dir: &Path) -> Result<Self> {
        let mut sessions = Vec::new();
        for (id, modified) in list_sessions(project_dir)? {
            let main = project_dir.join(format!("{id}.jsonl"));
            let mut paths = vec![main];
            let subagents = project_dir.join(&id).join("subagents");
            if let Ok(entries) = std::fs::read_dir(subagents) {
                let mut children: Vec<_> = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
                    .collect();
                children.sort();
                paths.extend(children);
            }

            let mut scan = Scan::default();
            for (index, path) in paths.iter().enumerate() {
                scan_file(path, index == 0, &mut scan);
            }
            let fallback = DateTime::<Utc>::from(modified);
            let start = scan.start.unwrap_or(fallback);
            let last = scan.last.unwrap_or_else(|| {
                DateTime::<Utc>::from(
                    std::fs::metadata(&paths[0])
                        .and_then(|meta| meta.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH),
                )
            });
            sessions.push(SessionSummary {
                id,
                start,
                last,
                turns: scan.message_ids.len() + scan.turns_without_id,
                agents: paths.len(),
                tokens: scan.tokens,
                dispatched: scan.spawn_ids.len() + scan.spawns_without_id,
                had_error: scan.had_error,
                failed: scan.build_error || scan.merge_error || scan.max_failures >= 3,
                compacted: scan.compacted,
                ended: scan.ended,
                prompt: scan.prompt.unwrap_or_else(|| "-".into()),
            });
        }
        sessions.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.id.cmp(&b.id)));
        Ok(Self { sessions })
    }

    pub fn total_tokens(&self) -> u64 {
        self.sessions.iter().map(|session| session.tokens).sum()
    }

    pub fn dispatched(&self) -> usize {
        self.sessions.iter().map(|session| session.dispatched).sum()
    }

    pub fn failed_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| session.had_error)
            .count()
    }

    pub fn compacted_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| session.compacted)
            .count()
    }
}

impl SessionSummary {
    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        !self.ended && (0..90).contains(&now.signed_duration_since(self.last).num_seconds())
    }

    pub fn status(&self, now: DateTime<Utc>) -> SessionStatus {
        if self.is_live(now) {
            SessionStatus::Live
        } else if self.failed {
            SessionStatus::Fail
        } else if self.compacted {
            SessionStatus::Warn
        } else {
            SessionStatus::Ok
        }
    }

    pub fn time_range(&self, now: DateTime<Utc>) -> String {
        let live = self.is_live(now);
        let end = if live {
            "进行中".to_string()
        } else {
            self.last.with_timezone(&Local).format("%H:%M").to_string()
        };
        let seconds = (if live { now } else { self.last })
            .signed_duration_since(self.start)
            .num_seconds()
            .max(0);
        format!(
            "{} → {} ({:02}m{:02}s)",
            self.start.with_timezone(&Local).format("%m月%d日 %H:%M"),
            end,
            seconds / 60,
            seconds % 60
        )
    }
}

pub fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.2}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

fn scan_file(path: &Path, is_main: bool, scan: &mut Scan) {
    let Ok(file) = File::open(path) else { return };
    let mut failures = 0;
    let mut usage_totals = HashMap::new();
    let mut merge_calls = HashSet::new();
    let mut build_calls = HashSet::new();
    for line in BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
    {
        let Ok(raw) = serde_json::from_str::<RawLine>(&line) else {
            continue;
        };
        scan.compacted |= raw.event_type.as_deref() == Some("system")
            && raw.subtype.as_deref() == Some("compact_boundary")
            && raw.compact_metadata.is_some();
        if let Some(timestamp) = raw
            .timestamp
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
        {
            scan.start = Some(scan.start.map_or(timestamp, |start| start.min(timestamp)));
            scan.last = Some(scan.last.map_or(timestamp, |last| last.max(timestamp)));
        }

        if let Some(message) = &raw.message {
            if let Some(usage) = message.usage {
                let key = message
                    .id
                    .as_ref()
                    .map(|id| format!("id:{id}"))
                    .or_else(|| raw.uuid.as_ref().map(|uuid| format!("uuid:{uuid}")));
                if let Some(key) = key {
                    scan.message_ids.insert(key.clone());
                    let previous = usage_totals.insert(key, usage.total()).unwrap_or(0);
                    scan.tokens = scan.tokens.saturating_sub(previous) + usage.total();
                } else {
                    scan.turns_without_id += 1;
                    scan.tokens += usage.total();
                }
            }
            let role = message.role.as_deref().or(raw.event_type.as_deref());
            if is_main
                && role == Some("user")
                && let Some(text) = user_text(message.content.as_ref())
            {
                scan.ended = false;
                if scan.prompt.is_none() {
                    scan.prompt = Some(text);
                }
            }
            let mut saw_text = message
                .content
                .as_ref()
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty());
            let mut saw_tool = false;
            if let Some(blocks) = message.content.as_ref().and_then(Value::as_array) {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str).unwrap_or("") {
                        "text" => {
                            saw_text |= block
                                .get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|text| !text.trim().is_empty());
                        }
                        "tool_use" => {
                            saw_tool = true;
                            let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                            if matches!(name, "Task" | "Agent") {
                                if id.is_empty() {
                                    scan.spawns_without_id += 1;
                                } else {
                                    scan.spawn_ids.insert(id.to_string());
                                }
                            }
                            let (target, _) =
                                extract_tool_target(name, block.get("input"), raw.cwd.as_deref());
                            if name == "Bash" && !id.is_empty() {
                                if target.as_deref().is_some_and(is_merge_command) {
                                    merge_calls.insert(id.to_string());
                                }
                                if target.as_deref().is_some_and(is_build_or_test_command) {
                                    build_calls.insert(id.to_string());
                                }
                            }
                        }
                        "tool_result" => {
                            let error = block
                                .get("is_error")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            failures = if error { failures + 1 } else { 0 };
                            scan.max_failures = scan.max_failures.max(failures);
                            scan.had_error |= error;
                            let id = block
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let merged = merge_calls.remove(id);
                            let built = build_calls.remove(id);
                            scan.merge_error |= error && merged;
                            scan.build_error |= error && built;
                        }
                        _ => {}
                    }
                }
            }
            if is_main && role == Some("assistant") {
                scan.ended = saw_text && !saw_tool;
            }
        }
        if is_main && raw.event_type.as_deref() == Some("result") {
            scan.ended = raw.subtype.as_deref() != Some("error");
        }
    }
}

fn user_text(content: Option<&Value>) -> Option<String> {
    let text = match content? {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn scans_session_summaries_and_classifies_status() {
        let project =
            std::env::temp_dir().join(format!("spec-xray-sessions-{}", std::process::id()));
        let subagents = project.join("one/subagents");
        fs::create_dir_all(&subagents).unwrap();
        let lines = [
            json!({"type":"user", "timestamp":"2026-11-25T06:00:00Z", "message":{
                "role":"user", "content":" /implement-specs\n 实现选择器 "}}),
            json!({"type":"assistant", "timestamp":"2026-11-25T06:00:10Z", "message":{
                "id":"same", "role":"assistant", "usage":{"input_tokens":500,"output_tokens":100},
                "content":[{"type":"tool_use","id":"spawn","name":"Agent","input":{}},{"type":"tool_use","id":"merge","name":"Bash","input":{"command":"git.exe merge topic"}}]}}),
            json!({"type":"user", "timestamp":"2026-11-25T06:01:00Z", "message":{
            "role":"user", "content":[
                {"type":"tool_result","tool_use_id":"a","is_error":true},
                {"type":"tool_result","tool_use_id":"b","is_error":true},
                {"type":"tool_result","tool_use_id":"merge","is_error":true}
            ]}}),
        ];
        fs::write(
            project.join("one.jsonl"),
            lines.map(|line| line.to_string()).join("\n"),
        )
        .unwrap();
        fs::write(
            subagents.join("agent-a.jsonl"),
            json!({
                "timestamp":"2026-11-25T06:02:00Z", "message":{"id":"same", "role":"assistant",
                "usage":{"input_tokens":400,"output_tokens":100}, "content":[]}
            })
            .to_string(),
        )
        .unwrap();

        let catalog = ProjectSessions::scan(&project).unwrap();
        let summary = &catalog.sessions[0];
        assert_eq!(summary.turns, 1);
        assert_eq!(summary.agents, 2);
        assert_eq!(summary.tokens, 1_100);
        assert_eq!(summary.dispatched, 1);
        assert_eq!(summary.prompt, "/implement-specs 实现选择器");
        assert!(summary.failed && summary.had_error);
        let mut scan = Scan::default();
        scan_file(&project.join("one.jsonl"), true, &mut scan);
        assert!(scan.merge_error && scan.max_failures == 3);
        assert_eq!(summary.status(Utc::now()), SessionStatus::Fail);
        assert!(summary.time_range(Utc::now()).contains("(02m00s)"));
        assert_eq!(format_tokens(1_420_000), "1.42M");
        assert_eq!(format_tokens(850_000), "850k");
        let mut summary = catalog.sessions.into_iter().next().unwrap();
        summary.last = Utc::now();
        summary.failed = false;
        summary.compacted = true;
        assert_eq!(summary.status(Utc::now()), SessionStatus::Live);
        summary.ended = true;
        assert_eq!(summary.status(Utc::now()), SessionStatus::Warn);
        summary.compacted = false;
        assert_eq!(summary.status(Utc::now()), SessionStatus::Ok);
        fs::remove_dir_all(project).unwrap();
    }
}
