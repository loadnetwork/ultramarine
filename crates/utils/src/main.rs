use clap::Parser;
use color_eyre::eyre::Result;

mod commands;
mod tx;

use commands::Commands;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();
    match cli.command {
        Commands::Spam(cmd) => cmd.run().await,
        Commands::Genesis(cmd) => cmd.run().await,
    }
}
