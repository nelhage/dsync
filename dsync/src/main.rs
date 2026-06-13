use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod barrier;
mod client;
mod exec;
mod fastpath;
mod ignore;
mod protocol;
mod repo;
mod server;
mod state;
mod status;
mod sync;
mod target;
mod wquery;

use target::Target;

/// Sync a git repository to a remote (or local-path) replica, driven by
/// watchman.
#[derive(Debug, Parser)]
#[command(name = "ds", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Watch the repository and continuously sync changes to TARGET.
    Sync {
        /// Sync destination, as [HOST:]PATH. With no HOST, syncs to a local
        /// filesystem path.
        target: String,
    },
    /// Show the status of the running sync process.
    #[command(visible_aliases = ["stat", "s"])]
    Status,
    /// Block until the replica is up-to-date as-of this invocation.
    #[command(visible_alias = "b")]
    Barrier {
        /// Give up (and exit non-zero) if not up-to-date within this many
        /// seconds.
        #[arg(long)]
        timeout: Option<f64>,
    },
    /// Run a command on the sync target, in the replica directory, after
    /// waiting for it to be up-to-date.
    #[command(visible_alias = "x")]
    Exec {
        /// Skip the barrier; run immediately.
        #[arg(long)]
        no_wait: bool,
        /// Give up (and exit with code 3) if the replica is not up-to-date
        /// within this many seconds; the command is not run.
        #[arg(long, conflicts_with = "no_wait")]
        timeout: Option<f64>,
        /// The command (and arguments) to run.
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
}

async fn cmd_sync(target: &str) -> anyhow::Result<()> {
    let target = Target::parse(target)?;
    let repo_root = repo::find_repo_root(&std::env::current_dir()?)?;
    target.validate_against_repo(&repo_root)?;
    sync::run(repo_root, target).await
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match &cli.command {
        Command::Sync { target } => cmd_sync(target).await,
        Command::Status => status::cmd_status().await,
        Command::Barrier { timeout } => match barrier::cmd_barrier(*timeout).await? {
            barrier::Outcome::Synced => Ok(()),
            barrier::Outcome::TimedOut => std::process::exit(barrier::TIMEOUT_EXIT_CODE),
        },
        Command::Exec {
            no_wait,
            timeout,
            command,
        } => exec::cmd_exec(*no_wait, *timeout, command).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
