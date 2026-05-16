mod config;
mod dag;
mod supervisor;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "arig", version, about = "Polyglot service orchestrator")]
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
    /// Create `.arig/` and `.arig/.gitignore`
    Init,
    /// Build and start all services
    Up,
    /// Stop all services
    Down,
    /// Print the JSON schema for arig.yaml to stdout
    Schema,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(dir) = &cli.directory {
        std::env::set_current_dir(dir)
            .map_err(|e| anyhow::anyhow!("failed to chdir to {}: {e}", dir.display()))?;
    }

    match &cli.command {
        Commands::Schema => {
            let schema = schemars::schema_for!(config::ArigConfig);
            println!("{}", serde_json::to_string_pretty(&schema)?);
            return Ok(());
        }
        Commands::Init => {
            init()?;
            return Ok(());
        }
        _ => {}
    }

    let config = config::ArigConfig::load(&cli.file)?;

    match cli.command {
        Commands::Up => supervisor::up(config).await?,
        Commands::Down => {
            eprintln!("down: not yet implemented");
        }
        Commands::Init | Commands::Schema => unreachable!(),
    }

    Ok(())
}

fn init() -> anyhow::Result<()> {
    let arig_dir = std::path::Path::new(".arig");
    std::fs::create_dir_all(arig_dir)?;
    let gitignore = arig_dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "var/\n")?;
    }
    println!("arig: initialized {}", arig_dir.display());
    Ok(())
}
