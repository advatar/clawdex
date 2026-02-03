use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod app_server;
mod config;
mod cron;
mod heartbeat;
mod mcp;
mod memory;
mod skills_sync;
mod ui_bridge;
mod util;

#[derive(Parser)]
#[command(author, version, about = "Clawdex compatibility runtime", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the MCP server (stdio)
    McpServer {
        /// Disable cron scheduler (stubbed in Rust MVP).
        #[arg(long = "no-cron")]
        no_cron: bool,
        /// Disable heartbeat scheduler (stubbed in Rust MVP).
        #[arg(long = "no-heartbeat")]
        no_heartbeat: bool,
        /// Workspace directory (overrides config/env)
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// State directory (overrides default)
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
    },
    /// Run the daemon loop (cron + heartbeat)
    Daemon {
        /// Workspace directory (overrides config/env)
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// State directory (overrides default)
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
    },
    /// Sync OpenClaw skills into Codex skill directories
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
    /// Run the macOS UI bridge (JSONL stdio)
    UiBridge {
        /// Use stdio (required by current mac app)
        #[arg(long)]
        stdio: bool,
        /// Path to the codex binary to spawn
        #[arg(long = "codex-path")]
        codex_path: PathBuf,
        /// State directory for Clawdex
        #[arg(long = "state-dir")]
        state_dir: PathBuf,
        /// Workspace directory
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SkillsCommand {
    Sync {
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        link: bool,
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(long = "user-dir")]
        user_dir: Option<PathBuf>,
        #[arg(long = "repo-dir")]
        repo_dir: Option<PathBuf>,
        #[arg(long = "source-dir")]
        source_dir: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::McpServer {
            no_cron,
            no_heartbeat,
            workspace,
            state_dir,
        } => {
            let (cfg, paths) = config::load_config(state_dir, workspace)?;
            mcp::run_mcp_server(cfg, paths, !no_cron, !no_heartbeat)
        }
        Commands::Daemon { workspace, state_dir } => {
            let (cfg, paths) = config::load_config(state_dir, workspace)?;
            heartbeat::run_daemon(cfg, paths)
        }
        Commands::Skills { command } => match command {
            SkillsCommand::Sync {
                prefix,
                link,
                dry_run,
                user_dir,
                repo_dir,
                source_dir,
            } => skills_sync::sync_skills(skills_sync::SkillsSyncOptions {
                prefix,
                link,
                dry_run,
                user_dir,
                repo_dir,
                source_dir,
            }),
        },
        Commands::UiBridge {
            stdio: _,
            codex_path,
            state_dir,
            workspace,
        } => ui_bridge::run_ui_bridge(codex_path, state_dir, workspace),
    }
}
