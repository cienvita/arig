mod config;
mod dag;
mod supervisor;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "arig", about = "Cross-platform service orchestrator")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "arig.yaml")]
    file: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build and start all services
    Up,
    /// Stop all services
    Down,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = config::ArigConfig::load(&cli.file)?;

    match cli.command {
        Commands::Up => supervisor::up(config).await?,
        Commands::Down => {
            eprintln!("down: not yet implemented");
        }
    }

    Ok(())
}
