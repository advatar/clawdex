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
    /// Unix domain socket path for local JSON-RPC IPC (default: <state-dir>/daemon.sock)
    #[arg(long = "ipc-uds")]
    ipc_uds: Option<PathBuf>,
    /// Disable Unix domain socket IPC even when a default socket path is available
    #[arg(long = "no-ipc-uds")]
    no_ipc_uds: bool,
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
    let ipc_uds = if cli.no_ipc_uds {
        None
    } else {
        Some(
            cli.ipc_uds
                .unwrap_or_else(|| paths.state_dir.join("daemon.sock")),
        )
    };
    daemon_server::run_daemon_server(cfg, paths, cli.codex_path, &cli.bind, ipc_uds)
}
