use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use clawdex::{config, daemon_server};

#[derive(Parser)]
#[command(author, version, about = "Clawdex daemon runtime", long_about = None)]
struct Cli {
    /// Bind address for daemon IPC (HTTP)
    #[arg(long, default_value = "127.0.0.1:18791")]
    bind: String,
    /// Workspace directory (overrides config/env)
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// State directory (overrides default)
    #[arg(long = "state-dir")]
    state_dir: Option<PathBuf>,
    /// Path to the codex binary (overrides config/env)
    #[arg(long = "codex-path")]
    codex_path: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let (cfg, paths) = config::load_config(cli.state_dir, cli.workspace)?;
    daemon_server::run_daemon_server(cfg, paths, cli.codex_path, &cli.bind)
}
