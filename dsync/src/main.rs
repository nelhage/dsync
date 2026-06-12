use anyhow::bail;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

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
        /// The command (and arguments) to run.
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Command::Sync { .. } => "sync",
            Command::Status => "status",
            Command::Barrier { .. } => "barrier",
            Command::Exec { .. } => "exec",
        }
    }
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
    bail!("`ds {}` is not implemented yet", cli.command.name());
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
