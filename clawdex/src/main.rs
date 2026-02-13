use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use clawdex::{config, daemon, gateway, mcp, permissions, plugins, skills_sync, tasks, ui_bridge};

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
        /// Disable cron scheduler.
        #[arg(long = "no-cron")]
        no_cron: bool,
        /// Disable heartbeat scheduler.
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
        /// Optional WebSocket bind address (overrides config)
        #[arg(long = "ws-bind")]
        ws_bind: Option<String>,
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
    /// Plugin manager (Cowork/OpenClaw-compatible plugins)
    Plugins {
        #[command(subcommand)]
        command: PluginsCommand,
    },
    /// Permissions and policy controls
    Permissions {
        #[command(subcommand)]
        command: PermissionsCommand,
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
    /// Resume from an existing run thread and continue with a new prompt
    Resume {
        #[arg(long = "run-id")]
        run_id: String,
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
    /// Fork an existing run thread and continue on a branch with a new prompt
    Fork {
        #[arg(long = "run-id")]
        run_id: String,
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
    /// Request cancellation of a running task run
    Cancel {
        #[arg(long = "run-id")]
        run_id: String,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
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
    /// Follow task-run events until completion
    Follow {
        #[arg(long = "run-id")]
        run_id: String,
        /// Poll interval in milliseconds (clamped to 100-10000)
        #[arg(long = "poll-ms", default_value_t = 750)]
        poll_ms: u64,
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
    /// Export an audit packet for a task run (events/approvals/artifacts/plugins + audit log)
    AuditExport {
        #[arg(long = "run-id")]
        run_id: String,
        #[arg(long = "output")]
        output: Option<PathBuf>,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum PluginsCommand {
    /// List installed plugins
    List {
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long = "include-disabled")]
        include_disabled: bool,
    },
    /// Install a plugin from a local path, npm spec, git source, or shorthand
    Add {
        #[arg(value_name = "SOURCE")]
        spec: Option<String>,
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long)]
        npm: Option<String>,
        #[arg(long)]
        git: Option<String>,
        #[arg(long)]
        link: bool,
        #[arg(long)]
        source: Option<String>,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Update installed plugins (npm installs only)
    Update {
        #[arg(long = "id")]
        id: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Enable an installed plugin
    Enable {
        #[arg(long = "id")]
        id: String,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Disable an installed plugin
    Disable {
        #[arg(long = "id")]
        id: String,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Remove an installed plugin
    Remove {
        #[arg(long = "id")]
        id: String,
        #[arg(long = "keep-files")]
        keep_files: bool,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Re-sync plugin skills into Codex skills directory
    Sync {
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Export merged MCP config from enabled plugins
    ExportMcp {
        #[arg(long = "output")]
        output: Option<PathBuf>,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// List or run plugin commands
    Commands {
        #[command(subcommand)]
        command: PluginCommandsCommand,
    },
}

#[derive(Subcommand)]
enum PluginCommandsCommand {
    /// List available plugin commands
    List {
        #[arg(long = "id")]
        id: Option<String>,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Run a plugin command
    Run {
        #[arg(long = "id")]
        id: String,
        #[arg(long)]
        command: String,
        #[arg(long)]
        input: Option<String>,
        #[arg(long = "codex-path")]
        codex_path: Option<PathBuf>,
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long = "auto-approve")]
        auto_approve: bool,
    },
}

#[derive(Subcommand)]
enum PermissionsCommand {
    /// Show current permission settings
    Get {
        #[arg(long = "state-dir")]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Update permission settings
    Set {
        #[arg(long)]
        internet: Option<String>,
        #[arg(long = "read-only")]
        read_only: Option<bool>,
        #[arg(long = "mcp-allow")]
        mcp_allow: Option<String>,
        #[arg(long = "mcp-deny")]
        mcp_deny: Option<String>,
        #[arg(long = "mcp-plugin")]
        mcp_plugin: Vec<String>,
        #[arg(long = "mcp-server")]
        mcp_server: Vec<String>,
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
            ws_bind,
            workspace,
            state_dir,
        } => {
            let (cfg, paths) = config::load_config(state_dir, workspace)?;
            let gateway_cfg = cfg.gateway.clone().unwrap_or_default();
            let resolved_bind = bind
                .or_else(|| gateway_cfg.bind.clone())
                .unwrap_or_else(|| "127.0.0.1:18789".to_string());
            let resolved_ws = ws_bind.or_else(|| gateway_cfg.ws_bind.clone());
            if let Some(ws_bind) = resolved_ws {
                let ws_paths = paths.clone();
                std::thread::spawn(move || {
                    if let Err(err) = gateway::run_gateway_ws(&ws_bind, &ws_paths) {
                        eprintln!("[clawdex][gateway-ws] {err}");
                    }
                });
            }
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
                policy: None,
                resume_from_run_id: None,
                fork_from_run_id: None,
            }),
            TasksCommand::Resume {
                run_id,
                prompt,
                codex_path,
                state_dir,
                workspace,
                auto_approve,
            } => tasks::run_task_command(tasks::TaskRunOptions {
                task_id: None,
                title: None,
                prompt,
                codex_path,
                state_dir,
                workspace,
                auto_approve,
                approval_policy: None,
                policy: None,
                resume_from_run_id: Some(run_id),
                fork_from_run_id: None,
            }),
            TasksCommand::Fork {
                run_id,
                prompt,
                codex_path,
                state_dir,
                workspace,
                auto_approve,
            } => tasks::run_task_command(tasks::TaskRunOptions {
                task_id: None,
                title: None,
                prompt,
                codex_path,
                state_dir,
                workspace,
                auto_approve,
                approval_policy: None,
                policy: None,
                resume_from_run_id: None,
                fork_from_run_id: Some(run_id),
            }),
            TasksCommand::Cancel {
                run_id,
                state_dir,
                workspace,
            } => {
                let value = tasks::cancel_run_command(&run_id, state_dir, workspace)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
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
            TasksCommand::Follow {
                run_id,
                poll_ms,
                state_dir,
                workspace,
            } => tasks::follow_events_command(&run_id, poll_ms, state_dir, workspace),
            TasksCommand::Server {
                bind,
                state_dir,
                workspace,
            } => tasks::run_task_server(&bind, state_dir, workspace),
            TasksCommand::AuditExport {
                run_id,
                output,
                state_dir,
                workspace,
            } => {
                let value = tasks::export_audit_packet_command(&run_id, output, state_dir, workspace)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
        },
        Commands::Plugins { command } => match command {
            PluginsCommand::List {
                state_dir,
                workspace,
                include_disabled,
            } => {
                let value = plugins::list_plugins_command(state_dir, workspace, include_disabled)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            PluginsCommand::Add {
                spec,
                path,
                npm,
                git,
                link,
                source,
                state_dir,
                workspace,
            } => {
                let value = plugins::add_plugin_command(
                    spec,
                    path,
                    npm,
                    git,
                    link,
                    source,
                    state_dir,
                    workspace,
                )?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            PluginsCommand::Update {
                id,
                all,
                dry_run,
                state_dir,
                workspace,
            } => {
                let value =
                    plugins::update_plugin_command(id, all, dry_run, state_dir, workspace)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            PluginsCommand::Enable {
                id,
                state_dir,
                workspace,
            } => {
                let value = plugins::enable_plugin_command(&id, state_dir, workspace)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            PluginsCommand::Disable {
                id,
                state_dir,
                workspace,
            } => {
                let value = plugins::disable_plugin_command(&id, state_dir, workspace)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            PluginsCommand::Remove {
                id,
                keep_files,
                state_dir,
                workspace,
            } => {
                let value = plugins::remove_plugin_command(&id, keep_files, state_dir, workspace)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            PluginsCommand::Sync { state_dir, workspace } => {
                let value = plugins::sync_plugins_command(state_dir, workspace)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            PluginsCommand::ExportMcp {
                output,
                state_dir,
                workspace,
            } => {
                let value = plugins::export_mcp_command(output, state_dir, workspace)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            PluginsCommand::Commands { command } => match command {
                PluginCommandsCommand::List {
                    id,
                    state_dir,
                    workspace,
                } => {
                    let value = plugins::list_plugin_commands_command(id, state_dir, workspace)?;
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    Ok(())
                }
                PluginCommandsCommand::Run {
                    id,
                    command,
                    input,
                    codex_path,
                    state_dir,
                    workspace,
                    auto_approve,
                } => {
                    let value = plugins::run_plugin_command_command(
                        &id,
                        &command,
                        input,
                        codex_path,
                        state_dir,
                        workspace,
                        auto_approve,
                    )?;
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    Ok(())
                }
            },
        },
        Commands::Permissions { command } => match command {
            PermissionsCommand::Get { state_dir, workspace } => {
                let value = permissions::get_permissions_command(state_dir, workspace)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            PermissionsCommand::Set {
                internet,
                read_only,
                mcp_allow,
                mcp_deny,
                mcp_plugin,
                mcp_server,
                state_dir,
                workspace,
            } => {
                let internet = internet
                    .as_deref()
                    .map(permissions::parse_on_off)
                    .transpose()?;
                let mcp_allow = mcp_allow.as_deref().map(permissions::parse_csv_list);
                let mcp_deny = mcp_deny.as_deref().map(permissions::parse_csv_list);
                let mcp_plugins = if mcp_plugin.is_empty() {
                    None
                } else {
                    Some(
                        mcp_plugin
                            .iter()
                            .map(|entry| permissions::parse_plugin_toggle(entry))
                            .collect::<Result<Vec<_>, _>>()?,
                    )
                };
                let mcp_server_policies = if mcp_server.is_empty() {
                    None
                } else {
                    Some(
                        mcp_server
                            .iter()
                            .map(|entry| permissions::parse_server_policy(entry))
                            .collect::<Result<Vec<_>, _>>()?,
                    )
                };
                let value = permissions::set_permissions_command(
                    permissions::PermissionsUpdate {
                        internet,
                        read_only,
                        mcp_allow,
                        mcp_deny,
                        mcp_plugins,
                        mcp_server_policies,
                    },
                    state_dir,
                    workspace,
                )?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
        },
    }
}
