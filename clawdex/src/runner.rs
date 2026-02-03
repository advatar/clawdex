use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use codex_app_server_protocol::{AskForApproval, SandboxPolicy};
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::app_server::{ApprovalMode, CodexClient, TurnOutcome};

#[derive(Debug, Clone)]
pub struct CodexRunnerConfig {
    pub codex_path: PathBuf,
    pub codex_home: PathBuf,
    pub workspace: PathBuf,
    pub approval_policy: AskForApproval,
    pub config_overrides: Vec<String>,
}

pub struct CodexRunner {
    client: CodexClient,
    main_thread: String,
    isolated_threads: HashMap<String, String>,
    workspace: PathBuf,
    approval_policy: AskForApproval,
}

impl CodexRunner {
    pub fn start(cfg: CodexRunnerConfig) -> Result<Self> {
        let approval_mode = ApprovalMode::from_env();
        std::fs::create_dir_all(&cfg.codex_home)
            .with_context(|| format!("create {}", cfg.codex_home.display()))?;
        let mut env = Vec::<(String, String)>::new();
        env.push((
            "CODEX_HOME".to_string(),
            cfg.codex_home.to_string_lossy().to_string(),
        ));
        env.push((
            "CODEX_WORKSPACE_DIR".to_string(),
            cfg.workspace.to_string_lossy().to_string(),
        ));
        let mut client =
            CodexClient::spawn(&cfg.codex_path, &cfg.config_overrides, &env, approval_mode)?;
        client.initialize()?;
        let main_thread = client.thread_start()?;
        Ok(Self {
            client,
            main_thread,
            isolated_threads: HashMap::new(),
            workspace: cfg.workspace,
            approval_policy: cfg.approval_policy,
        })
    }

    pub fn run_main(&mut self, message: &str) -> Result<TurnOutcome> {
        let sandbox_policy = workspace_sandbox_policy(&self.workspace)?;
        self.client.run_turn(
            &self.main_thread,
            message,
            Some(self.approval_policy),
            sandbox_policy,
            Some(self.workspace.clone()),
        )
    }

    pub fn run_isolated(&mut self, key: &str, message: &str) -> Result<TurnOutcome> {
        let thread_id = if let Some(thread) = self.isolated_threads.get(key) {
            thread.clone()
        } else {
            let thread = self.client.thread_start()?;
            self.isolated_threads.insert(key.to_string(), thread.clone());
            thread
        };
        let sandbox_policy = workspace_sandbox_policy(&self.workspace)?;
        self.client.run_turn(
            &thread_id,
            message,
            Some(self.approval_policy),
            sandbox_policy,
            Some(self.workspace.clone()),
        )
    }
}

fn workspace_sandbox_policy(workspace: &Path) -> Result<Option<SandboxPolicy>> {
    let abs = AbsolutePathBuf::try_from(workspace.to_path_buf())
        .context("workspace path must be absolute")?;
    Ok(Some(SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![abs],
        network_access: true,
        exclude_tmpdir_env_var: false,
        exclude_slash_tmp: false,
    }))
}
