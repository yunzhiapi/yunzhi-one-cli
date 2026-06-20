use crate::observability::{TurnMetrics, UsageMetrics};
use crate::tools::{
    PermissionDecision, PermissionPrompter, PermissionRequest, UserChoiceRequest,
    UserChoiceResponse, UserQuestionRequest,
};
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
    println!("  /session help  查看会话保存、恢复、checkpoint 和 rollback 命令");
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

pub fn print_agent_done(turn: TurnMetrics, session: UsageMetrics, context_tokens: usize) {
    println!(
        "\n{} 本轮 {:.1}s | req {} | tokens {} | ${:.6} | 会话 {:.1}s | req {} | tokens {} | ${:.6} | 上下文 {}\n",
        "状态".dark_grey(),
        turn.elapsed_ms as f32 / 1000.0,
        turn.request_count,
        turn.total_tokens(),
        turn.estimated_cost_usd,
        session.elapsed_ms as f32 / 1000.0,
        session.request_count,
        session.total_tokens(),
        session.estimated_cost_usd,
        context_tokens
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

pub fn print_unverified_completion_warning() {
    println!(
        "\n{} 模型声称操作完成，但本轮没有检测到工具调用；文件或命令可能没有真正执行。\n",
        "警告".yellow().bold()
    );
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
            print!("确认执行? [y] 是 / [p] 选择 diff 块 / [n] 否 / [a] 全部允许: ");
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            match input.trim().to_lowercase().as_str() {
                "y" | "yes" => return Ok(PermissionDecision::Allow),
                "a" | "all" => return Ok(PermissionDecision::AllowAll),
                "p" | "partial" => {
                    print!("请输入要应用的 hunk 编号，例如 1,3-5: ");
                    io::stdout().flush()?;
                    let mut hunk_input = String::new();
                    io::stdin().read_line(&mut hunk_input)?;
                    match parse_hunk_selection(&hunk_input) {
                        Ok(selected) if !selected.is_empty() => {
                            return Ok(PermissionDecision::Partial(selected));
                        }
                        _ => println!("请输入有效的 hunk 编号。"),
                    }
                }
                "n" | "no" => return Ok(PermissionDecision::Deny),
                _ => println!("请输入 y、p、n 或 a。"),
            }
        }
    }

    async fn ask_user(&self, request: UserQuestionRequest) -> Result<String> {
        println!("{}", "需要用户输入".yellow().bold());
        if let Some(context) = request.context {
            println!("{}", context);
        }
        loop {
            match &request.default_answer {
                Some(default_answer) => print!("{} [{}]: ", request.question, default_answer),
                None => print!("{}: ", request.question),
            }
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let answer = input.trim().to_string();
            if !answer.is_empty() {
                return Ok(answer);
            }
            if let Some(default_answer) = &request.default_answer {
                return Ok(default_answer.clone());
            }
            println!("请输入答案。")
        }
    }

    async fn choose_option(&self, request: UserChoiceRequest) -> Result<UserChoiceResponse> {
        println!("{}", "需要用户选择".yellow().bold());
        if let Some(context) = request.context {
            println!("{}", context);
        }
        println!("{}", request.question);
        for (index, option) in request.options.iter().enumerate() {
            println!("  {}. {}", index + 1, option);
        }
        if request.allow_custom {
            println!("也可以直接输入自定义答案。")
        }
        loop {
            print!("请选择 [1-{}]: ", request.options.len());
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let answer = input.trim();
            if let Ok(choice) = answer.parse::<usize>() {
                if (1..=request.options.len()).contains(&choice) {
                    return Ok(UserChoiceResponse {
                        answer: request.options[choice - 1].clone(),
                        index: Some(choice - 1),
                        custom: false,
                    });
                }
            }
            if request.allow_custom && !answer.is_empty() {
                return Ok(UserChoiceResponse {
                    answer: answer.to_string(),
                    index: None,
                    custom: true,
                });
            }
            println!("请输入 1 到 {} 之间的序号。", request.options.len());
        }
    }
}

pub fn ratatui_plan() -> &'static str {
    "进阶 TUI 将使用 ratatui + crossterm 拆分滚动消息区、固定输入框和状态栏；MVP 当前使用 stdout 流式渲染以保证核心 Agent 先可用。"
}

fn parse_hunk_selection(input: &str) -> Result<Vec<usize>> {
    let mut selected = Vec::new();
    for part in input
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if let Some((start, end)) = part.split_once('-') {
            let start = start.trim().parse::<usize>()?;
            let end = end.trim().parse::<usize>()?;
            anyhow::ensure!(start <= end, "hunk 范围起点不能大于终点");
            selected.extend(start..=end);
        } else {
            selected.push(part.parse::<usize>()?);
        }
    }
    selected.sort_unstable();
    selected.dedup();
    Ok(selected)
}
