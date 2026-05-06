mod config;
mod dag;
mod supervisor;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "arig", about = "Polyglot service orchestrator")]
struct Cli {
    /// Change to DIR before doing anything else (like git -C).
    #[arg(short = 'C', long = "directory", value_name = "DIR")]
    directory: Option<PathBuf>,

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

    if let Some(dir) = &cli.directory {
        std::env::set_current_dir(dir).map_err(|e| {
            anyhow::anyhow!("failed to chdir to {}: {e}", dir.display())
        })?;
    }

    let config = config::ArigConfig::load(&cli.file)?;

    match cli.command {
        Commands::Up => supervisor::up(config).await?,
        Commands::Down => {
            eprintln!("down: not yet implemented");
        }
    }

    Ok(())
}
