mod client;
mod config;
mod dag;
mod event;
mod ipc;
mod protocol;
mod state;
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
    Up {
        /// Run the supervisor in the background; the CLI returns once it is ready.
        #[arg(short = 'd', long = "detach")]
        detach: bool,
    },
    /// Stop all services
    Down,
    /// List services tracked by the supervisor for this workspace
    Ps,
    /// Print the JSON schema for arig.yaml to stdout
    Schema,
    /// Internal: act as a workspace supervisor. Spawned by `arig up --detach`.
    #[command(name = "__supervise", hide = true)]
    Supervise {
        /// Absolute path to the workspace this supervisor manages.
        #[arg(long)]
        workspace: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(dir) = &cli.directory {
        std::env::set_current_dir(dir)
            .map_err(|e| anyhow::anyhow!("failed to chdir to {}: {e}", dir.display()))?;
    }

    match cli.command {
        Commands::Schema => {
            let schema = schemars::schema_for!(config::ArigConfig);
            println!("{}", serde_json::to_string_pretty(&schema)?);
            Ok(())
        }
        Commands::Init => init(),
        Commands::Supervise { workspace } => {
            std::env::set_current_dir(&workspace)
                .map_err(|e| anyhow::anyhow!("failed to chdir to {}: {e}", workspace.display()))?;
            let config = config::ArigConfig::load(&cli.file)?;
            supervisor::up(config).await
        }
        Commands::Up { detach } => {
            let config = config::ArigConfig::load(&cli.file)?;
            if detach {
                supervisor::detach_and_exit(&cli.file).await
            } else {
                supervisor::up(config).await
            }
        }
        Commands::Down => {
            let cwd = std::env::current_dir()?;
            client::down(&cwd).await
        }
        Commands::Ps => {
            let cwd = std::env::current_dir()?;
            client::ps(&cwd).await
        }
    }
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
