# spec-xray

spec-xray 从本地 Claude Code 会话轨迹生成单次、只读的诊断分析。以下术语固定分析边界，避免把会话、项目路径和原始日志混为一谈。

## Language

**主会话（Root Session）**:
由一个 UUID 标识的一次 Claude Code 主会话，是单次诊断分析不可跨越的边界。
_Avoid_: 项目会话、跨会话运行记录

**Subagent 轨迹**:
主会话内某个 Subagent 产生的独立事件序列；它与所属主会话共同构成一次分析范围。
_Avoid_: 子会话、分支会话

**项目路径**:
用于定位本地会话日志的原始工作目录路径，不是编码后的日志目录名或会话标识。
_Avoid_: 项目 ID、日志目录、Session ID

**诊断报告**:
从一个主会话及其 Subagent 轨迹提炼出的分析结果，而不是原始对话或日志的转储。
_Avoid_: 完整日志导出、原始 JSON 转储
