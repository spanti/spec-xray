use serde::Deserialize;
use serde_json::Value;

/// 单行 JSONL 记录（宽容解析：只提取诊断需要字段，忽略其余变更字段）
#[derive(Debug, Deserialize)]
pub struct RawLine {
    pub uuid: Option<String>,
    #[serde(rename = "type")]
    pub event_type: Option<String>,
    pub timestamp: Option<String>,
    #[serde(rename = "gitBranch")]
    pub git_branch: Option<String>,
    #[serde(rename = "isSidechain")]
    pub is_sidechain: Option<bool>,
    #[serde(rename = "parentToolUseID")]
    pub parent_tool_use_id: Option<String>,
    pub message: Option<MessageBody>,
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
