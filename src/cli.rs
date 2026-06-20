use crate::agent::Agent;
use crate::config::{
    ensure_config_interactive, load_config, load_profile, masked_key, save_config,
};
use crate::llm::AnthropicLikeClient;
use crate::mcp_server;
use crate::tui::{self, EventPrompter, StdoutPrompter};
use crate::types::{AgentMode, AgentOptions, AppConfig};
use anyhow::Result;
use clap::{Parser, Subcommand};
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

    /// 选择配置 Profile，读取 .yunzhi/profiles.toml 或 ~/.yunzhi/profiles.toml
    #[arg(long)]
    pub profile: Option<String>,

    /// 覆盖本次运行使用的主控模型
    #[arg(long)]
    pub model: Option<String>,

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
        /// 覆盖本次 print 使用的主控模型
        #[arg(long)]
        model: Option<String>,
        prompt: String,
    },
    /// 以 MCP stdio server 模式运行，供 IDE 插件等 MCP client 调用
    McpServer,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// 设置 API Key
    SetKey { key: String },
    /// 设置默认主控模型
    SetModel { model: String },
    /// 查看当前配置，API Key 会做掩码显示
    Show,
}

pub async fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Commands::Config { command }) => run_config(command),
        Some(Commands::Print {
            mode,
            model,
            prompt,
        }) => {
            run_print(
                prompt,
                cli.dangerously_skip_permissions,
                mode.unwrap_or(cli.mode),
                cli.profile,
                model.or(cli.model),
            )
            .await
        }
        Some(Commands::McpServer) => run_mcp_server(cli.profile).await,
        None => {
            if let Some(prompt) = cli.prompt {
                run_print(
                    prompt,
                    cli.dangerously_skip_permissions,
                    cli.mode,
                    cli.profile,
                    cli.model,
                )
                .await
            } else {
                run_interactive(
                    cli.dangerously_skip_permissions,
                    cli.mode,
                    cli.profile,
                    cli.model,
                )
                .await
            }
        }
    }
}

async fn run_mcp_server(profile: Option<String>) -> Result<()> {
    let config = load_runtime_config(profile)?;
    mcp_server::run_stdio_server(std::env::current_dir()?, config.api_key).await
}

fn load_runtime_config(profile_name: Option<String>) -> Result<AppConfig> {
    let config = ensure_config_interactive()?;
    if let Some(name) = profile_name.as_deref() {
        let cwd = std::env::current_dir()?;
        load_profile(&cwd, name)?.ok_or_else(|| anyhow::anyhow!("未找到 profile: {name}"))?;
    }
    Ok(config)
}

fn run_config(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::SetKey { key } => {
            let model = load_config()?.and_then(|config| config.model);
            save_config(&AppConfig {
                api_key: key,
                model,
            })?;
            println!("API Key 已保存。");
        }
        ConfigCommand::SetModel { model } => {
            let mut config = load_config()?.ok_or_else(|| {
                anyhow::anyhow!("尚未配置 API Key。请先运行 yunzhi config set-key <key>")
            })?;
            let model = model.trim().to_string();
            anyhow::ensure!(!model.is_empty(), "模型名称不能为空");
            config.model = Some(model.clone());
            save_config(&config)?;
            println!("默认主控模型已设置为 {model}。");
        }
        ConfigCommand::Show => match load_config()? {
            Some(config) => {
                println!("api_key = \"{}\"", masked_key(&config.api_key));
                println!(
                    "model = \"{}\"",
                    config.model.as_deref().unwrap_or(crate::llm::DEFAULT_MODEL)
                );
            }
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
    profile_name: Option<String>,
    model_override: Option<String>,
    fullscreen: bool,
) -> Result<Agent<AnthropicLikeClient>> {
    let config = ensure_config_interactive()?;
    let cwd = std::env::current_dir()?;
    let profile = match profile_name.as_deref() {
        Some(name) => Some(
            load_profile(&cwd, name)?.ok_or_else(|| anyhow::anyhow!("未找到 profile: {name}"))?,
        ),
        None => None,
    };
    let client = AnthropicLikeClient::new(config.api_key.clone());
    let mut options = AgentOptions {
        dangerously_skip_permissions,
        mode,
        model: config
            .model
            .clone()
            .unwrap_or_else(|| crate::llm::DEFAULT_MODEL.to_string()),
        profile_name,
        ..AgentOptions::default()
    };
    if let Some(profile) = profile {
        if let Some(mode) = profile.mode {
            options.mode = mode;
        }
        if let Some(model) = profile.model {
            options.model = model;
        }
        if let Some(max_tokens) = profile.max_tokens {
            options.max_tokens = max_tokens;
        }
        options.persona = profile.persona;
        options.tool_allowlist = profile.tools;
    }
    if let Some(model) = model_override {
        let model = model.trim().to_string();
        anyhow::ensure!(!model.is_empty(), "模型名称不能为空");
        options.model = model;
    }
    Agent::new(
        client,
        cwd,
        config.api_key,
        options,
        if fullscreen {
            Arc::new(EventPrompter)
        } else {
            Arc::new(StdoutPrompter)
        },
    )
}

async fn run_print(
    prompt: String,
    dangerously_skip_permissions: bool,
    mode: AgentMode,
    profile: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let mut agent = build_agent(dangerously_skip_permissions, mode, profile, model, false).await?;
    agent.run_turn(prompt).await?;
    Ok(())
}

async fn run_interactive(
    dangerously_skip_permissions: bool,
    mode: AgentMode,
    profile: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let agent = build_agent(dangerously_skip_permissions, mode, profile, model, true).await?;
    tui::run_fullscreen(agent, env!("CARGO_PKG_VERSION")).await
}
