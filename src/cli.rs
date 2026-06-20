use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "yunzhi", version, about = "云智 One CLI 智能体工具")]
pub struct Cli {
	#[command(subcommand)]
	pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
	/// 配置管理
	Config,
}

pub async fn run_cli(_cli: Cli) -> Result<()> {
	println!("云智 One CLI 初始化完成。后续功能正在接入。");
	Ok(())
}
