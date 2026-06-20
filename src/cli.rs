use crate::agent::Agent;
use crate::config::{
    ensure_config_interactive, load_config, load_profile, masked_key, save_config,
};
use crate::extensions::{
    add_mcp_server, add_skill, load_mcp_servers, read_skill, skills_index, ExtensionScope,
    McpServerConfig,
};
use crate::llm::AnthropicLikeClient;
use crate::mcp_server;
use crate::tui::{self, EventPrompter, StdoutPrompter};
use crate::types::{AgentMode, AgentOptions, AppConfig};
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::collections::BTreeMap;
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
    /// Skill 管理
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// MCP server 配置管理
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
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

#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// 新增 Skill
    Add {
        /// Skill id，可用 / 分组，例如 code/review
        id: String,
        /// Skill 简短描述
        #[arg(short, long)]
        description: String,
        /// Skill 正文。未设置 --file 时使用此值。
        #[arg(short, long)]
        content: Option<String>,
        /// 从 Markdown 文件读取 Skill 正文
        #[arg(short, long)]
        file: Option<std::path::PathBuf>,
        /// 写入范围：project 或 user
        #[arg(long, default_value = "project")]
        scope: String,
        /// 覆盖同名 Skill
        #[arg(long)]
        overwrite: bool,
    },
    /// 列出已发现的 Skills
    List,
    /// 读取 Skill 内容
    Read { skill: String },
}

#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// 新增 MCP stdio server
    Add {
        /// MCP server 名称
        name: String,
        /// 启动命令
        command: String,
        /// 命令参数，可重复：--arg server.js --arg --flag
        #[arg(long = "arg")]
        args: Vec<String>,
        /// 环境变量 KEY=VALUE，可重复
        #[arg(long = "env")]
        env: Vec<String>,
        /// 写入范围：project 或 user
        #[arg(long, default_value = "project")]
        scope: String,
        /// 覆盖同名 server
        #[arg(long)]
        overwrite: bool,
    },
    /// 列出已配置的 MCP servers
    List,
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
        Some(Commands::Skill { command }) => run_skill(command),
        Some(Commands::Mcp { command }) => run_mcp(command),
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

fn run_skill(command: SkillCommand) -> Result<()> {
    let cwd = std::env::current_dir()?;
    match command {
        SkillCommand::Add {
            id,
            description,
            content,
            file,
            scope,
            overwrite,
        } => {
            let body = match (content, file) {
                (Some(_), Some(_)) => anyhow::bail!("--content 和 --file 只能二选一"),
                (Some(content), None) => content,
                (None, Some(file)) => std::fs::read_to_string(&file).map_err(|error| {
                    anyhow::anyhow!("读取 Skill 文件失败 {}: {error}", file.display())
                })?,
                (None, None) => anyhow::bail!("请通过 --content 或 --file 提供 Skill 正文"),
            };
            let path = add_skill(
                &cwd,
                ExtensionScope::parse(&scope)?,
                &id,
                &description,
                &body,
                overwrite,
            )?;
            println!("已添加 Skill: {}", path.display());
        }
        SkillCommand::List => {
            let skills = skills_index(&cwd)?;
            if skills.is_empty() {
                println!("未发现 Skills");
            } else {
                for skill in skills {
                    println!(
                        "{}\t{}\t{}",
                        skill.id,
                        skill.description,
                        skill.path.display()
                    );
                }
            }
        }
        SkillCommand::Read { skill } => {
            let (info, content) = read_skill(&cwd, &skill)?;
            println!("Skill: {}", info.id);
            println!("Path: {}", info.path.display());
            println!();
            println!("{content}");
        }
    }
    Ok(())
}

fn run_mcp(command: McpCommand) -> Result<()> {
    let cwd = std::env::current_dir()?;
    match command {
        McpCommand::Add {
            name,
            command,
            args,
            env,
            scope,
            overwrite,
        } => {
            let path = add_mcp_server(
                &cwd,
                ExtensionScope::parse(&scope)?,
                &name,
                McpServerConfig {
                    command,
                    args,
                    env: parse_env_pairs(env)?,
                },
                overwrite,
            )?;
            println!("已更新 MCP 配置: {}", path.display());
        }
        McpCommand::List => {
            let servers = load_mcp_servers(&cwd)?;
            if servers.is_empty() {
                println!("未配置 MCP servers");
            } else {
                for (name, server) in servers {
                    let args = if server.args.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " {}",
                            shell_words::join(server.args.iter().map(String::as_str))
                        )
                    };
                    println!("{}\t{}{}", name, server.command, args);
                }
            }
        }
    }
    Ok(())
}

fn parse_env_pairs(values: Vec<String>) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for value in values {
        let (key, val) = value
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("环境变量必须使用 KEY=VALUE 格式: {value}"))?;
        anyhow::ensure!(!key.trim().is_empty(), "环境变量名称不能为空: {value}");
        env.insert(key.to_string(), val.to_string());
    }
    Ok(env)
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
