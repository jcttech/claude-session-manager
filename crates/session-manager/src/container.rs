use anyhow::{anyhow, Result};
use dashmap::DashMap;
use shell_escape::escape;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::config::settings;
use crate::container_registry::{ContainerRegistry, ContainerState};
use crate::database::Database;
use crate::devcontainer;
use crate::grpc::GrpcExecutor;
use crate::ssh;
use crate::stream_json::OutputEvent;

/// Escape a string for safe use in shell commands
fn shell_escape(s: &str) -> Cow<'_, str> {
    escape(Cow::Borrowed(s))
}

pub struct ContainerManager {
    sessions: DashMap<String, Session>,
    pub registry: ContainerRegistry,
}

/// Result of attempting to claim a session for cleanup.
/// If successful, contains the session name, worktree path, and repo/branch for registry decrement.
pub struct ClaimedSession {
    pub name: String,
    pub worktree_path: Option<PathBuf>,
    pub repo: String,
    pub branch: String,
}

struct Session {
    name: String,
    message_tx: mpsc::Sender<String>,
    /// Path to worktree if session is using one (for cleanup)
    worktree_path: Option<PathBuf>,
    /// Repo identifier (e.g., "org/repo") for container registry lookups
    repo: String,
    /// Branch name for container registry lookups
    branch: String,
    /// Session type: "standard", "orchestrator", "worker", "reviewer"
    _session_type: String,
    /// true → first message (gRPC Execute); false → subsequent (gRPC SendMessage)
    is_first_message: Arc<AtomicBool>,
    /// Claude session ID captured from gRPC SessionInit event
    claude_session_id: Arc<Mutex<Option<String>>>,
    /// Whether plan mode is enabled for this session
    plan_mode: Arc<AtomicBool>,
    /// Whether the next response should be captured as a thread title
    pending_title: Arc<AtomicBool>,
}

impl Default for ContainerManager {
    fn default() -> Self {
        Self {
            sessions: DashMap::new(),
            registry: ContainerRegistry::new(),
        }
    }
}

/// Return value from `start` indicating whether the container was reused or freshly created.
pub struct StartResult {
    pub container_name: String,
    /// true if an existing container was reused (fast path)
    pub reused: bool,
    /// Warning message if same-branch session detected
    pub warning: Option<String>,
}

impl ContainerManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a session, reusing an existing container for the same (repo, branch) if available.
    /// Falls back to `devcontainer up` for cold start.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        &self,
        session_id: &str,
        project_path: &str,
        repo: &str,
        branch: &str,
        db: &Database,
        output_tx: mpsc::Sender<OutputEvent>,
        plan_mode: bool,
        session_type: &str,
    ) -> Result<StartResult> {
        let s = settings();
        let mut warning: Option<String> = None;

        // Check if a container already exists for this (repo, branch)
        let existing = self.registry.get_container(repo, branch).await;
        let container_name = if let Some(ref entry) = existing {
            if entry.state != ContainerState::Running {
                return Err(anyhow!(
                    "Container for {}/{} is in '{}' state, cannot start new session",
                    repo, branch, entry.state
                ));
            }

            // Check max sessions limit
            if s.container_max_sessions > 0 && entry.session_count >= s.container_max_sessions {
                return Err(anyhow!(
                    "Container for {}/{} has reached max sessions ({}). Stop a session first.",
                    repo, branch, s.container_max_sessions
                ));
            }

            // Warn about same-branch concurrent sessions
            if entry.session_count > 0 {
                warning = Some(format!(
                    "Warning: Another session is already active on `{}@{}`. \
                     Concurrent file writes on the same branch may cause conflicts. \
                     Consider using `--worktree` for branch isolation.",
                    repo, branch
                ));
                tracing::warn!(
                    repo, branch, session_count = entry.session_count,
                    "Same-branch concurrent session started"
                );
            }

            // Check devcontainer.json hash for rebuild
            let current_hash = devcontainer::hash_config(project_path).await;
            if let (Some(stored_hash), Some(current)) = (&entry.devcontainer_json_hash, &current_hash) && stored_hash != current {
                tracing::warn!(
                    repo, branch,
                    %stored_hash, current_hash = %current,
                    "devcontainer.json changed — rebuild needed on next cold start"
                );
                // For now, we reuse the container and log the warning.
                // Full drain-and-rebuild is deferred to a future story.
            }

            // Reuse existing container — increment session count
            self.registry.increment_sessions(db, repo, branch).await?;

            tracing::info!(
                session_id, repo, branch,
                container = %entry.container_name,
                "Reusing existing container (fast path)"
            );
            entry.container_name.clone()
        } else {
            // Cold start: no existing container, run devcontainer up
            self.cold_start(project_path, repo, branch, db).await?
        };

        // Set up message channel and spawn processor
        let reused = existing.is_some();
        self.create_session_internal(
            session_id, &container_name, project_path, repo, branch,
            output_tx, plan_mode, session_type,
        );

        Ok(StartResult {
            container_name,
            reused,
            warning,
        })
    }

    /// Cold start: generate devcontainer config if needed, run `devcontainer up`,
    /// wait for worker health check, register container.
    async fn cold_start(
        &self,
        project_path: &str,
        repo: &str,
        branch: &str,
        db: &Database,
    ) -> Result<String> {
        let s = settings();

        // If project has no devcontainer.json, generate a default one
        if !devcontainer::has_devcontainer_config(project_path).await {
            let default_config = devcontainer::generate_default_config(
                &s.container_image,
                &s.container_network,
            );
            devcontainer::write_default_config(project_path, &default_config).await?;
            tracing::info!(
                project_path = %project_path,
                "Generated default devcontainer.json (no existing config found)"
            );
        }

        // Hash the devcontainer.json for future rebuild detection
        let config_hash = devcontainer::hash_config(project_path).await;

        // Start container via devcontainer up
        let escaped_project_path = shell_escape(project_path);
        let devcontainer_cmd = format!(
            "devcontainer up --docker-path {} --workspace-folder {}",
            shell_escape(&s.container_runtime),
            escaped_project_path,
        );

        let timeout = Duration::from_secs(s.devcontainer_timeout_secs);
        let output_future = ssh::command()?
            .arg(ssh::login_shell(&devcontainer_cmd))
            .output();
        let output = tokio::time::timeout(timeout, output_future)
            .await
            .map_err(|_| anyhow!("devcontainer up timed out after {}s", s.devcontainer_timeout_secs))??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("devcontainer up failed: {}", stderr));
        }

        // Parse JSON output to get container ID
        let output_str = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = serde_json::from_str(output_str.trim())
            .map_err(|e| anyhow!("Failed to parse devcontainer output: {}", e))?;
        let container_name = json["containerId"].as_str()
            .ok_or_else(|| anyhow!("No containerId in devcontainer output"))?
            .to_string();

        // Wait for worker health check
        let grpc_addr = format!("http://{}:{}", s.vm_host, s.grpc_port_start);
        crate::grpc::wait_for_health(&grpc_addr, 30, Duration::from_secs(1)).await?;

        // Register container in registry and database, with session_count = 1
        self.registry
            .register_container(db, repo, branch, &container_name, config_hash.as_deref())
            .await?;
        self.registry.increment_sessions(db, repo, branch).await?;

        tracing::info!(
            repo, branch, container = %container_name,
            "Cold-started new container"
        );
        Ok(container_name)
    }

    /// Internal helper to create session state and spawn message processor.
    #[allow(clippy::too_many_arguments)]
    fn create_session_internal(
        &self,
        session_id: &str,
        container_name: &str,
        _project_path: &str,
        repo: &str,
        branch: &str,
        output_tx: mpsc::Sender<OutputEvent>,
        plan_mode: bool,
        session_type: &str,
    ) {
        let (message_tx, message_rx) = mpsc::channel::<String>(32);
        let is_first_message = Arc::new(AtomicBool::new(true));
        let claude_session_id = Arc::new(Mutex::new(None));
        let plan_mode_flag = Arc::new(AtomicBool::new(plan_mode));
        let pending_title_flag = Arc::new(AtomicBool::new(false));

        let session_id_owned = session_id.to_string();
        let session_type_owned = session_type.to_string();
        let is_first_clone = Arc::clone(&is_first_message);
        let claude_sid_clone = Arc::clone(&claude_session_id);
        let plan_mode_clone = Arc::clone(&plan_mode_flag);
        let pending_title_clone = Arc::clone(&pending_title_flag);
        tokio::spawn(async move {
            message_processor(
                message_rx,
                output_tx,
                session_id_owned,
                session_type_owned,
                is_first_clone,
                claude_sid_clone,
                plan_mode_clone,
                pending_title_clone,
            ).await;
        });

        self.sessions.insert(
            session_id.to_string(),
            Session {
                name: container_name.to_string(),
                message_tx,
                worktree_path: None,
                repo: repo.to_string(),
                branch: branch.to_string(),
                _session_type: session_type.to_string(),
                is_first_message,
                claude_session_id,
                plan_mode: plan_mode_flag,
                pending_title: pending_title_flag,
            },
        );
    }

    /// Reconnect to an existing session after a restart.
    /// Creates in-memory state (channels, message_processor) without running `devcontainer up`.
    /// The container must already be running.
    #[allow(clippy::too_many_arguments)]
    pub fn reconnect(
        &self,
        session_id: &str,
        container_name: &str,
        project_path: &str,
        repo: &str,
        branch: &str,
        session_type: &str,
        output_tx: mpsc::Sender<OutputEvent>,
    ) {
        self.create_session_internal(
            session_id, container_name, project_path, repo, branch,
            output_tx, false, session_type,
        );

        tracing::info!(
            session_id = %session_id,
            container = %container_name,
            "Reconnected session (fresh conversation)"
        );
    }

    pub async fn send(&self, session_id: &str, text: &str) -> Result<()> {
        match self.sessions.get(session_id) {
            Some(session) => {
                session.message_tx.send(text.to_string()).await?;
            }
            None => {
                return Err(anyhow!("Session {} not found in-memory (stale DB record after restart?)", session_id));
            }
        }
        Ok(())
    }

    /// Set the worktree path for a session (for tracking purposes)
    pub fn set_worktree_path(&self, session_id: &str, worktree_path: PathBuf) {
        if let Some(mut session) = self.sessions.get_mut(session_id) {
            session.worktree_path = Some(worktree_path);
        }
    }

    /// Atomically claim a session for cleanup. Returns `Some` if this caller
    /// wins the race (removes the entry), `None` if already claimed by another.
    /// Includes repo/branch so the caller can decrement the container's session count.
    pub fn claim_session(&self, session_id: &str) -> Option<ClaimedSession> {
        self.sessions.remove(session_id).map(|(_, session)| ClaimedSession {
            name: session.name,
            worktree_path: session.worktree_path,
            repo: session.repo,
            branch: session.branch,
        })
    }

    /// Decrement the session count for a container after a session is stopped.
    /// If the count reaches zero, the container remains alive for idle timeout.
    pub async fn release_session(&self, db: &Database, repo: &str, branch: &str) -> Result<i32> {
        self.registry.decrement_sessions(db, repo, branch).await
    }

    /// Remove a container by its name (used after claiming a session)
    pub async fn remove_container_by_name(&self, container_name: &str) -> Result<()> {
        let s = settings();
        let escaped_name = shell_escape(container_name);
        let rm_cmd = format!(
            "{} rm -f {}",
            shell_escape(&s.container_runtime),
            escaped_name
        );

        let timeout = Duration::from_secs(s.ssh_timeout_secs);
        let output_future = ssh::command()?
            .arg(ssh::login_shell(&rm_cmd))
            .output();
        tokio::time::timeout(timeout, output_future)
            .await
            .map_err(|_| anyhow!("Container removal timed out after {}s", s.ssh_timeout_secs))??;
        Ok(())
    }

    /// Restart the Claude conversation inside a session.
    /// Resets the first-message flag and clears the Claude session ID
    /// so the next message starts a fresh conversation.
    pub async fn restart_session(&self, session_id: &str) -> Result<()> {
        let session = self.sessions.get(session_id)
            .ok_or_else(|| anyhow!("Session {} not found", session_id))?;
        session.is_first_message.store(true, Ordering::SeqCst);
        *session.claude_session_id.lock().unwrap() = None;
        Ok(())
    }

    /// Set plan mode for a session
    pub fn set_plan_mode(&self, session_id: &str, enabled: bool) {
        if let Some(session) = self.sessions.get(session_id) {
            session.plan_mode.store(enabled, Ordering::SeqCst);
        }
    }

    /// Set pending_title flag so next response is captured as a thread title
    pub fn set_pending_title(&self, session_id: &str) {
        if let Some(session) = self.sessions.get(session_id) {
            session.pending_title.store(true, Ordering::SeqCst);
        }
    }

    /// Get plan mode status for a session
    pub fn get_plan_mode(&self, session_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .map(|s| s.plan_mode.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// Get live runtime info for a session
    pub fn get_session_info(&self, session_id: &str) -> Option<SessionInfo> {
        self.sessions.get(session_id).map(|s| SessionInfo {
            claude_session_id: s.claude_session_id.lock().unwrap().clone(),
            plan_mode: s.plan_mode.load(Ordering::SeqCst),
            is_first_message: s.is_first_message.load(Ordering::SeqCst),
        })
    }

    /// Find and claim all sessions that belong to a given (repo, branch) container.
    /// Returns the list of claimed sessions.
    pub fn stop_all_sessions_for_container(&self, repo: &str, branch: &str) -> Vec<(String, ClaimedSession)> {
        // First, collect session IDs that match the repo/branch
        let matching_ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|entry| entry.value().repo == repo && entry.value().branch == branch)
            .map(|entry| entry.key().clone())
            .collect();

        // Then claim each one (atomic removal from DashMap)
        let mut claimed = Vec::new();
        for session_id in matching_ids {
            if let Some((id, session)) = self.sessions.remove(&session_id) {
                claimed.push((id, ClaimedSession {
                    name: session.name,
                    worktree_path: session.worktree_path,
                    repo: session.repo,
                    branch: session.branch,
                }));
            }
        }
        claimed
    }

    /// Tear down a container: set state to Stopping, remove via SSH, remove from registry, mark stopped in DB.
    pub async fn tear_down_container(&self, db: &Database, repo: &str, branch: &str) -> Result<()> {
        // Set state to Stopping
        self.registry
            .set_state(db, repo, branch, ContainerState::Stopping)
            .await?;

        // Get the container name for removal
        let container_name = self
            .registry
            .get_container(repo, branch)
            .await
            .map(|e| e.container_name);

        // Remove container via SSH
        if let Some(ref name) = container_name && let Err(e) = self.remove_container_by_name(name).await {
            tracing::warn!(
                container = %name, repo, branch, error = %e,
                "Failed to remove container via SSH (may already be gone)"
            );
        }

        // Remove from registry and mark stopped in DB
        self.registry.remove_container(db, repo, branch).await?;

        tracing::info!(
            repo, branch,
            container = container_name.as_deref().unwrap_or("unknown"),
            "Container torn down"
        );
        Ok(())
    }
}

/// Live runtime info for a session (from ContainerManager)
pub struct SessionInfo {
    pub claude_session_id: Option<String>,
    pub plan_mode: bool,
    pub is_first_message: bool,
}

/// Per-message processor task. Reads messages from the channel and for each one:
/// 1. Connects to the gRPC worker via GrpcExecutor
/// 2. Calls Execute (first message) or SendMessage (subsequent) on the worker
/// 3. Streams AgentEvent → OutputEvent to the output channel
#[allow(clippy::too_many_arguments)]
async fn message_processor(
    mut message_rx: mpsc::Receiver<String>,
    output_tx: mpsc::Sender<OutputEvent>,
    session_id: String,
    _session_type: String,
    is_first_message: Arc<AtomicBool>,
    claude_session_id: Arc<Mutex<Option<String>>>,
    plan_mode: Arc<AtomicBool>,
    pending_title: Arc<AtomicBool>,
) {
    let s = settings();
    let grpc_addr = format!("http://{}:{}", s.vm_host, s.grpc_port_start);

    let mut executor = match GrpcExecutor::connect(&grpc_addr).await {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(session_id = %session_id, error = %e, "Failed to connect to gRPC worker");
            let _ = output_tx.send(OutputEvent::ProcessDied {
                exit_code: Some(1),
                signal: Some(format!("gRPC connection failed: {}", e)),
            }).await;
            return;
        }
    };

    while let Some(message) = message_rx.recv().await {
        let stored_sid = claude_session_id.lock().unwrap().clone();
        let is_first = is_first_message.load(Ordering::SeqCst);

        // Determine permission mode
        let permission_mode = if plan_mode.load(Ordering::SeqCst) {
            "plan"
        } else {
            "bypassPermissions"
        };

        tracing::info!(
            session_id = %session_id,
            is_first,
            has_session_id = stored_sid.is_some(),
            permission_mode,
            "Sending message via gRPC"
        );

        // If pending_title is set, we'll capture the response as a title
        let is_title_request = pending_title.swap(false, Ordering::SeqCst);

        let result = if is_first || stored_sid.is_none() {
            // First message: Execute (creates new session)
            executor.execute(
                &message,
                "",
                permission_mode,
                std::collections::HashMap::new(),
                &output_tx,
            ).await
        } else {
            // Subsequent message: SendMessage (continues session)
            executor.send_message(
                stored_sid.as_deref().unwrap(),
                &message,
                &output_tx,
            ).await
        };

        match result {
            Ok(new_session_id) => {
                // Capture session ID from first Execute
                if let Some(sid) = new_session_id {
                    if stored_sid.is_none() {
                        tracing::info!(
                            session_id = %session_id,
                            claude_session_id = %sid,
                            "Captured Claude session ID from gRPC"
                        );
                        *claude_session_id.lock().unwrap() = Some(sid);
                    }
                }
                is_first_message.store(false, Ordering::SeqCst);
            }
            Err(e) => {
                tracing::error!(
                    session_id = %session_id,
                    error = %e,
                    "gRPC call failed"
                );
                if output_tx.send(OutputEvent::ProcessDied {
                    exit_code: Some(1),
                    signal: Some(format!("gRPC error: {}", e)),
                }).await.is_err() {
                    break;
                }
            }
        }

        // If this was a title request, emit TitleGenerated
        // (The actual title text was already streamed as TextLine events,
        //  the stream_output handler will have captured it)
        if is_title_request {
            // Title requests are handled by the `title` command in main.rs
            // which sends a specific prompt and captures the response.
            // The TextLine events already went through.
        }
    }

    tracing::debug!(session_id = %session_id, "Message processor exiting");
}
