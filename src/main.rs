use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use spec_xray::{analyzer::analyzer, report};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Parser)]
#[command(
    name = "spec-xray",
    about = "Claude Code Subagent 上下文与合并诊断分析器"
)]
struct Cli {
    /// 目标项目工作目录（默认为当前目录）
    #[arg(long, global = true, default_value = ".")]
    project: PathBuf,

    /// 单步上下文激增警告阈值（tokens）
    #[arg(long, global = true, default_value_t = 10_000)]
    threshold: u64,

    /// 诊断报告输出路径（默认写入当前目录的 output.log，不重复输出到终端）
    #[arg(long, global = true, default_value = "output.log")]
    output: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug, Clone)]
enum Commands {
    /// 列出当前项目最近的会话 UUID
    List {
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// 分析最近一次会话
    Latest,
    /// 分析指定主会话 UUID
    Inspect { session_id: String },
    /// 打开实时 TUI 诊断面板（默认加载最新会话）
    Tui { session_id: Option<String> },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let proj_dir = analyzer::resolve_project_dir(&cli.project)?;

    match cli.command {
        Commands::List { limit } => {
            if limit == 0 {
                bail!("--limit 必须大于 0");
            }
            let sessions = analyzer::list_sessions(&proj_dir)?;
            for (sid, _) in sessions.into_iter().take(limit) {
                println!("{sid}");
            }
        }
        Commands::Latest => {
            let sessions = analyzer::list_sessions(&proj_dir)?;
            let (latest_id, _) = sessions.first().context("当前项目下没有任何会话日志")?;
            let rep = analyzer::analyze_session(&proj_dir, latest_id, cli.threshold)?;
            emit_report(&report::render_markdown(&rep), &cli.output)?;
        }
        Commands::Inspect { session_id } => {
            let rep = analyzer::analyze_session(&proj_dir, &session_id, cli.threshold)?;
            emit_report(&report::render_markdown(&rep), &cli.output)?;
        }
        Commands::Tui { session_id } => {
            spec_xray::ui::run(&proj_dir, session_id.as_deref(), cli.threshold)?;
        }
    }

    Ok(())
}

fn emit_report(markdown: &str, output: &Path) -> Result<()> {
    fs::write(output, markdown)
        .with_context(|| format!("写入诊断报告到 {} 失败", output.display()))?;

    eprintln!("诊断报告已写入 {}。", output.display());
    Ok(())
}
