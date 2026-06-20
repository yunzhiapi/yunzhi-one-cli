use crate::agent::Agent;
use crate::config::{ensure_config_interactive, load_config, masked_key, save_config};
use crate::llm::AnthropicLikeClient;
use crate::tui::{self, StdoutPrompter};
use crate::types::{AgentMode, AgentOptions, AppConfig};
use anyhow::Result;
use clap::{Parser, Subcommand};
use rustyline::error::ReadlineError;
use std::str::FromStr;
use std::sync::Arc;

#[derive(Debug, Parser)]
#[command(name = "yunzhi", version, about = "云智 One CLI 智能体工具")]
pub struct Cli {
    /// 非交互单次执行指令，等价于 yunzhi print <指令>
    #[arg(short = 'p', long = "print")]
    pub prompt: Option<String>,

    /// 跳过写文件和 bash 的确认提示。危险，仅用于受信任环境。
    #[arg(long)]
    pub dangerously_skip_permissions: bool,

    /// 智能体模式：chat、plan-act、entanglement、agent、team、analyze
    #[arg(long, default_value_t = AgentMode::Agent)]
    pub mode: AgentMode,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// 配置管理
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// 非交互单次执行指令
    Print {
        /// 智能体模式：chat、plan-act、entanglement、agent、team、analyze
        #[arg(long)]
        mode: Option<AgentMode>,
        prompt: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// 设置 API Key
    SetKey { key: String },
    /// 查看当前配置，API Key 会做掩码显示
    Show,
}

pub async fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Commands::Config { command }) => run_config(command),
        Some(Commands::Print { mode, prompt }) => {
            run_print(
                prompt,
                cli.dangerously_skip_permissions,
                mode.unwrap_or(cli.mode),
            )
            .await
        }
        None => {
            if let Some(prompt) = cli.prompt {
                run_print(prompt, cli.dangerously_skip_permissions, cli.mode).await
            } else {
                run_interactive(cli.dangerously_skip_permissions, cli.mode).await
            }
        }
    }
}

fn run_config(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::SetKey { key } => {
            save_config(&AppConfig { api_key: key })?;
            println!("API Key 已保存。");
        }
        ConfigCommand::Show => match load_config()? {
            Some(config) => println!("api_key = \"{}\"", masked_key(&config.api_key)),
            None => println!(
                "尚未配置 API Key。运行 yunzhi config set-key <key> 或直接启动 yunzhi 进入引导。"
            ),
        },
    }
    Ok(())
}

async fn build_agent(
    dangerously_skip_permissions: bool,
    mode: AgentMode,
) -> Result<Agent<AnthropicLikeClient>> {
    let config = ensure_config_interactive()?;
    let client = AnthropicLikeClient::new(config.api_key.clone());
    let options = AgentOptions {
        dangerously_skip_permissions,
        mode,
        ..AgentOptions::default()
    };
    Agent::new(
        client,
        std::env::current_dir()?,
        config.api_key,
        options,
        Arc::new(StdoutPrompter),
    )
}

async fn run_print(
    prompt: String,
    dangerously_skip_permissions: bool,
    mode: AgentMode,
) -> Result<()> {
    let mut agent = build_agent(dangerously_skip_permissions, mode).await?;
    agent.run_turn(prompt).await?;
    Ok(())
}

async fn run_interactive(dangerously_skip_permissions: bool, mode: AgentMode) -> Result<()> {
    tui::print_banner(env!("CARGO_PKG_VERSION"))?;
    println!("{}", tui::ratatui_plan());
    let mut agent = build_agent(dangerously_skip_permissions, mode).await?;
    println!("当前模式: {}。输入 /mode 查看或切换。\n", agent.mode());
    let mut editor = rustyline::DefaultEditor::new()?;

    loop {
        match editor.readline("yunzhi> ") {
            Ok(line) => {
                let input = line.trim();
                if input.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(input);
                match input {
                    "/exit" => break,
                    "/help" => tui::print_help(),
                    "/clear" => {
                        agent.clear()?;
                        println!("上下文已清空。\n");
                    }
                    "/mode" => tui::print_modes(agent.mode()),
                    _ => {
                        if let Some(raw_mode) = input.strip_prefix("/mode ") {
                            match AgentMode::from_str(raw_mode) {
                                Ok(mode) => {
                                    agent.set_mode(mode)?;
                                    println!("已切换到 {} 模式。\n", agent.mode());
                                }
                                Err(error) => eprintln!("错误: {error}"),
                            }
                            continue;
                        }
                        tui::print_user(input);
                        if let Err(error) = agent.run_turn(input.to_string()).await {
                            eprintln!("错误: {error:#}");
                        }
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                println!("已中断当前输入。输入 /exit 退出。 ");
            }
            Err(ReadlineError::Eof) => break,
            Err(error) => return Err(error.into()),
        }
    }

    Ok(())
}
