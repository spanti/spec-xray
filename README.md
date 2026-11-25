# spec-xray

`spec-xray` 是一个本地 Claude Code 会话诊断工具，用于分析 Main/Subagent 的执行拓扑、Token 消耗、上下文压缩、文件修改碰撞以及合并后的构建结果。它提供 Markdown 报告和实时 Ratatui TUI，不需要后台服务，也不会修改 Claude Code 会话日志。

## 功能概览

- 重建 Main 与 Subagent 的父子执行拓扑，包括尚未创建子轨迹的排队 Task。
- 按 `message.id` 去重 usage 快照，统计累计 Token、当前上下文和工具带来的上下文增量。
- 检测重复全量 Read、单步 Token 激增、Compaction 和连续工具失败。
- 找出多个 Agent 修改同一文件的碰撞，并记录合并后的首次构建/测试结果。
- 使用独立文件偏移和半行缓冲增量刷新 TUI，自动发现新的 `subagents/*.jsonl`。
- 提供项目级 Session 统计、最近 10 次消耗趋势和交互式 Session 选择器。

## 构建与安装

需要支持 Rust 2024 edition 的 Rust 工具链，以及本机已有的 Claude Code 会话日志。

在项目根目录执行：

```bash
cargo install --path .
```

也可以只构建，不安装：

```bash
cargo build --release
```

构建产物位于 `target/release/spec-xray`；Windows 下为 `target/release/spec-xray.exe`。

## 快速开始

以下命令中的项目路径指被分析的代码工作目录，不是 Claude Code 日志目录。工具会将它映射到 `~/.claude/projects/<encoded-project>/`。

```bash
# 列出最近 10 个主会话 ID
spec-xray list --project "E:\total-project\rs-trader"

# 分析最近一次会话，默认写入 output.log
spec-xray latest --project "E:\total-project\rs-trader"

# 分析指定会话并复制 Markdown 到系统剪贴板
spec-xray inspect <session_id> --project "E:\total-project\rs-trader" --copy

# 打开最近一次会话的实时 TUI
spec-xray tui --project "E:\total-project\rs-trader"

# 打开指定会话
spec-xray tui <session_id> --project "E:\total-project\rs-trader"
```

## CLI

```text
spec-xray [OPTIONS] <COMMAND>

Commands:
  list                 列出最近的主会话 ID
  latest               分析最近一次会话
  inspect <session_id> 分析指定主会话
  tui [session_id]     打开实时 TUI，缺省加载最新会话
```

### 全局参数

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `--project <PATH>` | `.` | 被分析项目的工作目录 |
| `--threshold <TOKENS>` | `10000` | 单步上下文激增警告阈值 |
| `--copy` | 关闭 | `latest`/`inspect` 将报告写入系统剪贴板，不写输出文件 |
| `--output <PATH>` | `output.log` | `latest`/`inspect` 的 Markdown 输出路径 |
| `--limit <N>` | `10` | `list` 返回的会话数量，必须大于 0 |

未指定 `--copy` 时，报告命令写入文件，不在终端重复输出整份报告。

## 实时 TUI

启动后默认进入紧凑全宽实时树。按 `v` 可切换到 38%/62% 的标准诊断分栏视图。

### 通用快捷键

| 按键 | 行为 |
| --- | --- |
| `s` | 打开项目统计与 Session 选择器 |
| `v` | 切换紧凑树与诊断分栏视图 |
| `j` / `k` / `↓` / `↑` | 移动树节点；Detail 获得焦点时滚动详情 |
| `h` / `l` / `←` / `→` | 切换 Tree/Detail 焦点 |
| `Tab` / `1`～`4` | 在诊断视图切换 Radar、Merge/Clashes、Timeline、Report |
| `PgUp` / `PgDn` | 紧凑树翻页或诊断详情翻页 |
| `r` | 完整重载当前会话并重置增量读取状态 |
| `y` | 复制当前会话的 Markdown 报告 |
| `q` / `Esc` | 退出 TUI |

实时节点状态：

| 状态 | 含义 |
| --- | --- |
| `排队中` | Task/Agent 已派发，但尚无子文件或有效响应轮次 |
| `运行中` | 正在推理或执行工具，且最后活动距今小于 90 秒 |
| `已完成` | Task 成功返回，或轨迹输出结束且没有挂起工具 |
| `已失败` | Task 返回错误、连续工具失败至少三次，或非完成状态超过 90 秒没有写入 |

紧凑树中的 Context 进度条以 **1M Token** 为上限，达到 **900k** 后显示告警颜色。

### 项目统计与 Session 选择器

在任意视图按 `s` 打开。上半部分展示会话总数、总 Token、单次平均 Token、累计派发 Subagent 数、失败/压缩占比以及最近 10 次会话的 Token 趋势。下半部分按最后活跃时间倒序列出 Session，当前会话以 `●` 标记。

| 按键 | 行为 |
| --- | --- |
| `j` / `k` / `↓` / `↑` | 移动选中行 |
| `PgUp` / `PgDn` | 快速翻页 |
| `Enter` | 加载选中 Session，重置其增量读取状态并返回监控视图 |
| `r` | 重新扫描当前项目的全部会话元数据 |
| `s` / `Esc` | 返回原会话视图 |

Session 徽标：

- `[LIVE]`：最后事件距今小于 90 秒，且会话尚未结束。
- `[FAIL]`：存在至少三次连续工具失败，或合并/构建报错。
- `[WARN]`：没有严重失败，但发生过 Compaction。
- `[OK]`：其他正常会话。

选择器元数据只在首次打开或按 `r` 时扫描并缓存；文件监听器不会自动重建项目统计。首次打开会同步扫描全部历史日志，历史目录较大时可能需要等待。

## 增量刷新行为

- `notify` 文件事件经过 100ms 防抖后触发刷新。
- 500ms tick 只发现新 Subagent 文件并读取各文件新增字节。
- Main 和每个 Subagent 都有独立的字节游标、半行缓冲和工具运行状态。
- 只有按 `r` 完整重载当前会话，或在选择器中切换 Session 时，才重建对应实时分析器。
- Panic Hook 会在异常退出时尝试恢复 Raw Mode、备用屏幕和光标。

## 代码结构

```text
src/
├── main.rs                 # CLI 参数与报告输出
├── analyzer/analyzer.rs    # Token、Compaction、失败与 Merge 分析
├── parser/
│   ├── schema.rs           # 宽容的 JSONL 数据结构
│   └── tree.rs             # Main/Subagent 拓扑重建
├── report.rs               # Markdown 报告渲染
└── ui/
    ├── mod.rs              # 终端生命周期与事件循环
    ├── app.rs              # 视图状态、缓存、快捷键与会话切换
    ├── live.rs             # 增量读取及实时状态
    ├── sessions.rs         # 项目级 Session 单遍摘要扫描
    └── view.rs             # Ratatui 绘制
```

## 开发验证

```bash
cargo fmt --check
cargo check --all-targets
cargo test --all-targets
cargo build
```

测试覆盖增量读取、usage 去重、Compaction、Merge、实时状态、Session 摘要、双视图及 Session 选择器。
