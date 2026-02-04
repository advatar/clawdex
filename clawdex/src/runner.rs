use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use codex_app_server_protocol::{AskForApproval, SandboxPolicy};
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::app_server::{ApprovalMode, CodexClient, TurnOutcome};
use crate::config::WorkspacePolicy;

#[derive(Debug, Clone)]
pub struct CodexRunnerConfig {
    pub codex_path: PathBuf,
    pub codex_home: PathBuf,
    pub workspace: PathBuf,
    pub workspace_policy: WorkspacePolicy,
    pub approval_policy: AskForApproval,
    pub config_overrides: Vec<String>,
}

pub struct CodexRunner {
    client: CodexClient,
    main_thread: String,
    isolated_threads: HashMap<String, String>,
    workspace: PathBuf,
    workspace_policy: WorkspacePolicy,
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
            workspace_policy: cfg.workspace_policy,
            approval_policy: cfg.approval_policy,
        })
    }

    pub fn run_main(&mut self, message: &str) -> Result<TurnOutcome> {
        let approval_policy = self.approval_policy;
        let workspace_policy = self.workspace_policy.clone();
        let cwd = self.workspace.clone();
        self.run_main_with_policy(message, approval_policy, &workspace_policy, cwd)
    }

    pub fn run_isolated(&mut self, key: &str, message: &str) -> Result<TurnOutcome> {
        let approval_policy = self.approval_policy;
        let workspace_policy = self.workspace_policy.clone();
        let cwd = self.workspace.clone();
        self.run_isolated_with_policy(key, message, approval_policy, &workspace_policy, cwd)
    }

    pub fn run_main_with_policy(
        &mut self,
        message: &str,
        approval_policy: AskForApproval,
        workspace_policy: &WorkspacePolicy,
        cwd: PathBuf,
    ) -> Result<TurnOutcome> {
        let thread_id = self.main_thread.clone();
        self.run_with_policy(&thread_id, message, approval_policy, workspace_policy, cwd)
    }

    pub fn run_isolated_with_policy(
        &mut self,
        key: &str,
        message: &str,
        approval_policy: AskForApproval,
        workspace_policy: &WorkspacePolicy,
        cwd: PathBuf,
    ) -> Result<TurnOutcome> {
        let thread_id = if let Some(thread) = self.isolated_threads.get(key) {
            thread.clone()
        } else {
            let thread = self.client.thread_start()?;
            self.isolated_threads.insert(key.to_string(), thread.clone());
            thread
        };
        self.run_with_policy(&thread_id, message, approval_policy, workspace_policy, cwd)
    }

    fn run_with_policy(
        &mut self,
        thread_id: &str,
        message: &str,
        approval_policy: AskForApproval,
        workspace_policy: &WorkspacePolicy,
        cwd: PathBuf,
    ) -> Result<TurnOutcome> {
        let sandbox_policy = workspace_sandbox_policy(workspace_policy)?;
        self.client.run_turn(
            thread_id,
            message,
            Some(approval_policy),
            sandbox_policy,
            Some(cwd),
        )
    }
}

pub fn workspace_sandbox_policy(policy: &WorkspacePolicy) -> Result<Option<SandboxPolicy>> {
    if policy.read_only {
        return Ok(Some(SandboxPolicy::ReadOnly));
    }
    let mut roots = Vec::new();
    for root in &policy.allowed_roots {
        let abs = AbsolutePathBuf::try_from(root.to_path_buf())
            .context("workspace root must be absolute")?;
        roots.push(abs);
    }
    Ok(Some(SandboxPolicy::WorkspaceWrite {
        writable_roots: roots,
        network_access: policy.network_access,
        exclude_tmpdir_env_var: false,
        exclude_slash_tmp: false,
    }))
}
