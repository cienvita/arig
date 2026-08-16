mod client;
mod config;
mod dag;
mod event;
mod ipc;
mod probe;
mod protocol;
mod registry;
mod runtime;
mod sink;
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
        /// Run the supervisor in the background; the CLI returns once every
        /// build has finished, not once the services are ready. Use
        /// `arig wait` for that.
        #[arg(short = 'd', long = "detach")]
        detach: bool,
        /// Start the services as they are, without running any `build:` first
        #[arg(long)]
        no_build: bool,
        /// Give up on the build stage as a whole after this long, e.g. "5m"
        #[arg(long, default_value = "10m", value_parser = humantime::parse_duration)]
        build_timeout: std::time::Duration,
    },
    /// Stop all services
    Down,
    /// List services tracked by the supervisor for this workspace
    Ps,
    /// Block until every service has started and every readiness probe passed
    Wait {
        /// Give up after this long, e.g. "30s", "5m". Startup includes the
        /// build stage, so the default covers `up --build-timeout` as well as
        /// the time the services take to become ready.
        #[arg(long, default_value = "12m", value_parser = humantime::parse_duration)]
        timeout: std::time::Duration,
    },
    /// Stop one service and leave the rest running
    Stop {
        /// Service to stop
        service: String,
        /// Give up waiting for it to stop after this long, e.g. "30s", "5m"
        #[arg(long, default_value = "2m", value_parser = humantime::parse_duration)]
        timeout: std::time::Duration,
    },
    /// Start one service that is not running
    Start {
        /// Service to start
        service: String,
        /// Return once it has spawned, without waiting for its readiness probe
        #[arg(long)]
        no_wait: bool,
        /// Give up waiting for it to be ready after this long, e.g. "30s", "5m"
        #[arg(long, default_value = "2m", value_parser = humantime::parse_duration)]
        timeout: std::time::Duration,
    },
    /// Stop and start one service
    Restart {
        /// Service to restart
        service: String,
        /// Run the service's build first; a build that fails leaves the
        /// running instance alone
        #[arg(long)]
        build: bool,
        /// Return once it has spawned, without waiting for its readiness probe
        #[arg(long)]
        no_wait: bool,
        /// Give up waiting for it to be ready after this long, e.g. "30s", "5m"
        #[arg(long, default_value = "2m", value_parser = humantime::parse_duration)]
        timeout: std::time::Duration,
    },
    /// Run one service's build, leaving what is running alone
    Build {
        /// Service to build
        service: String,
        /// Give up waiting for the build after this long, e.g. "30s", "5m"
        #[arg(long, default_value = "2m", value_parser = humantime::parse_duration)]
        timeout: std::time::Duration,
    },
    /// Print the JSON schema for arig.yaml to stdout
    Schema,
    /// Internal: act as a workspace supervisor. Spawned by `arig up --detach`.
    #[command(name = "__supervise", hide = true)]
    Supervise {
        /// Absolute path to the workspace this supervisor manages.
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        no_build: bool,
        #[arg(long, default_value = "10m", value_parser = humantime::parse_duration)]
        build_timeout: std::time::Duration,
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
        Commands::Supervise {
            workspace,
            no_build,
            build_timeout,
        } => {
            std::env::set_current_dir(&workspace)
                .map_err(|e| anyhow::anyhow!("failed to chdir to {}: {e}", workspace.display()))?;
            let config = config::ArigConfig::load(&cli.file)?;
            let opts = supervisor::UpOptions {
                detached: true,
                no_build,
                build_timeout,
            };
            supervisor::up(config, Some(cli.file), opts).await
        }
        Commands::Up {
            detach,
            no_build,
            build_timeout,
        } => {
            let config = config::ArigConfig::load(&cli.file)?;
            let opts = supervisor::UpOptions {
                detached: detach,
                no_build,
                build_timeout,
            };
            if detach {
                supervisor::detach_and_exit(&cli.file, &opts).await
            } else {
                supervisor::up(config, Some(cli.file), opts).await
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
        Commands::Wait { timeout } => {
            let cwd = std::env::current_dir()?;
            client::wait(&cwd, timeout).await
        }
        Commands::Stop { service, timeout } => {
            let cwd = std::env::current_dir()?;
            client::stop(&cwd, &service, timeout).await
        }
        Commands::Start {
            service,
            no_wait,
            timeout,
        } => {
            let cwd = std::env::current_dir()?;
            client::start(&cwd, &service, no_wait, timeout).await
        }
        Commands::Restart {
            service,
            build,
            no_wait,
            timeout,
        } => {
            let cwd = std::env::current_dir()?;
            client::restart(&cwd, &service, build, no_wait, timeout).await
        }
        Commands::Build { service, timeout } => {
            let cwd = std::env::current_dir()?;
            client::build(&cwd, &service, timeout).await
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
