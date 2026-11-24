# spec-xray 需求规格说明书 (Phase 1: Pure CLI)

## 1. 背景与核心目标
在使用 `grill -> /to-spec -> /to-tickets -> /implement-specs` 流水线时，主控 Agent 会派发多个 Subagent 实现 Ticket 并执行 Git 分支合并。多次运行后常出现：
1. 最终分支合并后构建/测试失败，缺乏跨 Subagent 的文件变更碰撞洞察；
2. 单个 Subagent 上下文消耗异常膨胀，不清楚具体被哪些文件读取、搜索或报错日志吞噬；
3. 使用 `/retro` 时依赖 AI 临时编写分析脚本，耗时且维度不稳定。

**Phase 1 目标：**
构建一个纯本地、零后台服务的 **CLI 诊断工具 (`spec-xray`)**，读取 `~/.claude/projects/` 下的本地 JSONL 会话日志，输出高纯度 Markdown 诊断报告（终端打印 / 写入剪贴板 / JSON 导出），彻底取代 `/retro` 的临时分析脚本。*(注：TUI 交互界面留待 Phase 2，本期严禁引入任何 TUI/终端渲染库)*

## 2. 工程红线与约束
- **代码规模上限：** Phase 1 非测试代码总量严格控制在 **3,000 ~ 5,000 行**（绝不超过 6,000 行）。
- **架构解耦：** `parser` 与 `analyzer` 必须输出纯数据结构体（`SessionReport`），与 `cli` / `exporter` 输出格式完全分离，为后续扩展 TUI 预留干净的数据接口。
- **零侵入只读：** 仅读取本地 JSONL 日志，不修改 Claude Code 配置，不启动后台常驻进程或 HTTP 代理。

## 3. 数据源与解析规范
- **日志定位：** 自动将当前工作目录路径映射为 Claude Code 项目目录（`~/.claude/projects/<encoded-path>/`），支持 `--project <path>` 覆盖。
- **拓扑还原：**
  - 解析主会话 `<session-uuid>.jsonl` 中的 `Task` / `Agent` 工具调用节点；
  - 关联 `<session-uuid>/subagents/agent-<id>.jsonl` 子轨迹文件及 `isSidechain` 标记，重建 `Main -> Subagent` 调用树与时间顺序。
- **精确计费与去重：**
  - 必须按 `message.id`（或记录 `uuid`）去重后再累加 `usage` 字段，防止流式写入导致的重复计数；
  - 区分 **冷启动缓存写入开销**（首轮 `cache_creation_input_tokens`）与 **执行期增量消耗**（后续轮次 `input` / `output` / `cache_read`）。
- **宽容解析：** 未知事件类型或工具字段必须安全降级为 `Unknown` 变体，严禁 Panic。

## 4. 核心分析需求

### F1: Subagent 上下文消耗归因雷达
- **按工具/目标聚合：** 统计每个 Subagent 中 `Read`、`Grep`、`Glob`、`Bash`、`Edit/Write` 及 `Thinking` 的 Token 增量与占比。
- **上下文污染检测：**
  - **重复全量读取：** 识别同一 Subagent 对同一文件路径执行 $\ge 2$ 次无行号限制 `Read` 的行为；
  - **大体积噪音注入：** 标记单次工具返回导致上下文激增（默认 $> 10\text{k}$ tokens，支持 `--threshold` 调整）的操作。
- **Compaction 断层标记：** 识别上下文压缩事件，记录发生轮次及压缩前后的 Token 落差。

### F2: 跨 Subagent 文件修改交集与合并冲突分析
- **文件修改矩阵：** 提取所有 Subagent 调用 `Edit` / `Write` 修改的文件路径、行号范围（若可获取）及所属 `gitBranch`。
- **隐式冲突预警：**
  - 列出被 $\ge 2$ 个 Subagent 同时修改过的文件；
  - 提取分支合并阶段（Merger Subagent 或主控合并步骤）的 `Bash` 合并记录及合并后的首次构建/测试报错输出。

### F3: 死循环与重试风暴识别
- **模式识别：** 检测同一 Subagent 连续出现 `修改文件 -> 构建/测试失败 -> 再次修改同一文件` 且连续失败 $\ge 3$ 次的片段，量化该区间浪费的 Token 总量。

## 5. CLI 命令设计
- `spec-xray list [--limit 10]`：列出当前项目近期会话（Session ID、时间、Subagent 数量、总 Token、最终构建状态）。
- `spec-xray latest [--copy] [--json] [--subagent <id>]`：分析最近一次会话。默认输出精简 Markdown 诊断报告；`--copy` 直接写入系统剪贴板；`--json` 输出完整结构化分析结果。
- `spec-xray inspect <session-id> [--copy] [--json] [--subagent <id>]`：分析指定历史会话。