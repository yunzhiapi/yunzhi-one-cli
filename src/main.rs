use anyhow::Result;
use yunzhi_one_cli::cli::{run_cli, Cli};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = <Cli as clap::Parser>::parse();
    run_cli(cli).await
}
