use serde::Deserialize;
use serde_json::Value;

/// 单行 JSONL 记录（宽容解析：只提取诊断需要字段，忽略其余变更字段）
#[derive(Debug, Deserialize)]
pub struct RawLine {
    pub uuid: Option<String>,
    #[serde(rename = "type")]
    pub event_type: Option<String>,
    pub subtype: Option<String>,
    pub timestamp: Option<String>,
    pub cwd: Option<String>,
    #[serde(rename = "gitBranch")]
    pub git_branch: Option<String>,
    #[serde(rename = "isSidechain")]
    pub is_sidechain: Option<bool>,
    #[serde(rename = "parentToolUseID")]
    pub parent_tool_use_id: Option<String>,
    #[serde(rename = "toolUseResult")]
    pub tool_use_result: Option<Value>,
    #[serde(rename = "compactMetadata")]
    pub compact_metadata: Option<CompactMetadata>,
    pub message: Option<MessageBody>,
}

#[derive(Debug, Deserialize)]
pub struct CompactMetadata {
    #[serde(rename = "preTokens")]
    pub pre_tokens: u64,
    #[serde(rename = "postTokens")]
    pub post_tokens: Option<u64>,
}

impl RawLine {
    pub fn spawned_agent_id(&self, content: Option<&Value>) -> Option<String> {
        if let Some(id) = self
            .tool_use_result
            .as_ref()
            .and_then(|r| r.get("agentId"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            return Some(id.to_string());
        }

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
}

#[derive(Debug, Deserialize)]
pub struct MessageBody {
    pub id: Option<String>,
    pub role: Option<String>,
    pub usage: Option<Usage>,
    pub content: Option<Value>,
}

#[derive(Debug, Deserialize, Default, Clone, Copy)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl Usage {
    /// 单轮上下文总输入规模（含缓存创建与缓存命中）
    pub fn context_size(&self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }

    /// 计费维度总 Token（含输出）
    pub fn total(&self) -> u64 {
        self.context_size() + self.output_tokens
    }
}

/// 解析后的单条工具调用记录
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub id: String,
    pub name: String,
    pub target: Option<String>,
    pub has_line_limit: bool,
}
