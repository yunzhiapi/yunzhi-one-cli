use crate::tools::{PermissionDecision, PermissionPrompter, PermissionRequest};
use crate::types::AgentMode;
use anyhow::Result;
use async_trait::async_trait;
use crossterm::style::Stylize;
use std::io::{self, Write};

pub fn print_banner(version: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    println!("{}", "云智 One".bold().cyan());
    println!("{} v{}", "Yunzhi One CLI".bold(), version);
    println!("当前目录: {}", cwd.display());
    println!("输入 /help 查看命令，/exit 退出。\n");
    Ok(())
}

pub fn print_help() {
    println!("可用命令:");
    println!("  /help   显示帮助");
    println!("  /clear  清空当前对话上下文");
    println!("  /mode   查看可选模式");
    println!("  /mode <模式>  切换模式");
    println!("  /exit   退出");
    println!("快捷键: Ctrl+C 中断当前输入，Ctrl+D 退出。\n");
}

pub fn print_modes(current: AgentMode) {
    println!("当前模式: {}", current);
    println!("可选模式:");
    for mode in AgentMode::ALL {
        let marker = if mode == current { "*" } else { " " };
        println!("  {} {}", marker, mode);
    }
    println!("\n用法: /mode chat 或 yunzhi --mode plan-act\n");
}

pub fn print_user(text: &str) {
    println!("{} {}", ">".bold(), text);
}

pub fn print_agent_delta(text: &str) -> Result<()> {
    print!("{}", text);
    io::stdout().flush()?;
    Ok(())
}

pub fn print_agent_done(elapsed_secs: f32, estimated_tokens: usize) {
    println!(
        "\n{} 耗时 {:.1}s | 估算 tokens {}\n",
        "状态".dark_grey(),
        elapsed_secs,
        estimated_tokens
    );
}

pub fn print_tool_start(name: &str, summary: &str) {
    println!("{} 调用工具 {}", "●".yellow(), name.bold());
    if !summary.is_empty() {
        println!("└ {}", summary);
    }
}

pub fn print_tool_done(success: bool, elapsed_secs: f32) {
    let mark = if success {
        "✓ 完成".green()
    } else {
        "✗ 失败".red()
    };
    println!("└ {} ({:.1}s)\n", mark, elapsed_secs);
}

pub struct StdoutPrompter;

#[async_trait]
impl PermissionPrompter for StdoutPrompter {
    async fn confirm(&self, request: PermissionRequest) -> Result<PermissionDecision> {
        println!("{} {}", "需要确认".yellow().bold(), request.tool_name);
        println!("{}", request.summary);
        if let Some(diff) = request.diff {
            println!("{}", "--- diff ---".dark_grey());
            for line in diff.lines() {
                if line.starts_with('+') {
                    println!("{}", line.green());
                } else if line.starts_with('-') {
                    println!("{}", line.red());
                } else {
                    println!("{}", line);
                }
            }
            println!("{}", "------------".dark_grey());
        }
        loop {
            print!("确认执行? [y] 是 / [n] 否 / [a] 全部允许: ");
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            match input.trim().to_lowercase().as_str() {
                "y" | "yes" => return Ok(PermissionDecision::Allow),
                "a" | "all" => return Ok(PermissionDecision::AllowAll),
                "n" | "no" => return Ok(PermissionDecision::Deny),
                _ => println!("请输入 y、n 或 a。"),
            }
        }
    }
}

pub fn ratatui_plan() -> &'static str {
    "进阶 TUI 将使用 ratatui + crossterm 拆分滚动消息区、固定输入框和状态栏；MVP 当前使用 stdout 流式渲染以保证核心 Agent 先可用。"
}
