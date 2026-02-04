use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod app_server;
mod config;
mod cron;
mod daemon;
mod gateway;
mod heartbeat;
mod mcp;
mod memory;
mod runner;
mod skills_sync;
mod task_db;
mod tasks;
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
        /// Path to the codex binary (overrides config/env)
        #[arg(long = "codex-path")]
        codex_path: Option<PathBuf>,
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
    /// Run the minimal gateway server (HTTP)
    Gateway {
        /// Bind address (overrides config; default: 127.0.0.1:18789)
        #[arg(long)]
        bind: Option<String>,
        /// Workspace directory (overrides config/env)
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// State directory (overrides default)
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
    },
    /// Task runtime CLI (Cowork-style tasks)
    Tasks {
        #[command(subcommand)]
        command: TasksCommand,
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

#[derive(Subcommand)]
enum TasksCommand {
    /// List tasks
    List {
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Create a task
    Create {
        #[arg(long)]
        title: String,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Run a task
    Run {
        #[arg(long = "task-id")]
        task_id: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long = "codex-path")]
        codex_path: Option<PathBuf>,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long = "auto-approve")]
        auto_approve: bool,
    },
    /// List events for a task run
    Events {
        #[arg(long = "run-id")]
        run_id: String,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Run a simple HTTP task server (for UI integration)
    Server {
        #[arg(long, default_value = "127.0.0.1:18790")]
        bind: String,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
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
        Commands::Daemon {
            workspace,
            state_dir,
            codex_path,
        } => {
            let (cfg, paths) = config::load_config(state_dir, workspace)?;
            daemon::run_daemon(cfg, paths, codex_path)
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
        Commands::Gateway {
            bind,
            workspace,
            state_dir,
        } => {
            let (cfg, paths) = config::load_config(state_dir, workspace)?;
            let resolved_bind = bind
                .or_else(|| cfg.gateway.and_then(|g| g.bind))
                .unwrap_or_else(|| "127.0.0.1:18789".to_string());
            gateway::run_gateway(&resolved_bind, &paths)
        }
        Commands::Tasks { command } => match command {
            TasksCommand::List {
                state_dir,
                workspace,
            } => {
                let value = tasks::list_tasks_command(state_dir, workspace)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            TasksCommand::Create {
                title,
                state_dir,
                workspace,
            } => {
                let value = tasks::create_task_command(&title, state_dir, workspace)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            TasksCommand::Run {
                task_id,
                title,
                prompt,
                codex_path,
                state_dir,
                workspace,
                auto_approve,
            } => tasks::run_task_command(tasks::TaskRunOptions {
                task_id,
                title,
                prompt,
                codex_path,
                state_dir,
                workspace,
                auto_approve,
                approval_policy: None,
            }),
            TasksCommand::Events {
                run_id,
                limit,
                state_dir,
                workspace,
            } => {
                let value = tasks::list_events_command(&run_id, limit, state_dir, workspace)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            TasksCommand::Server {
                bind,
                state_dir,
                workspace,
            } => tasks::run_task_server(&bind, state_dir, workspace),
        },
    }
}
