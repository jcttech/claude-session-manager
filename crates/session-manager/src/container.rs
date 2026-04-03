use anyhow::{Result, anyhow};
use dashmap::DashMap;
use shell_escape::escape;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::config::settings;
use crate::container_registry::{ContainerRegistry, ContainerState};
use crate::database::Database;
use crate::devcontainer;
use crate::grpc::{self, GrpcExecutor, GrpcStream};
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
    /// Session type: "standard", "worker", "reviewer"
    _session_type: String,
    /// true → first message (gRPC CreateSession); false → subsequent (gRPC FollowUp)
    is_first_message: Arc<AtomicBool>,
    /// Claude session ID captured from gRPC SessionInit event
    claude_session_id: Arc<Mutex<Option<String>>>,
    /// Whether plan mode is enabled for this session
    plan_mode: Arc<AtomicBool>,
    /// Whether extended thinking is enabled for this session
    thinking_mode: Arc<AtomicBool>,
    /// Whether the next response should be captured as a thread title
    pending_title: Arc<AtomicBool>,
    /// Last known input_tokens from ResponseComplete (for context window display)
    last_input_tokens: Arc<AtomicU64>,
    /// gRPC address for interrupt RPC (separate from the stream)
    grpc_addr: String,
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
        thinking_mode: bool,
        session_type: &str,
        system_prompt_append: Option<&str>,
    ) -> Result<StartResult> {
        let s = settings();
        let mut warning: Option<String> = None;

        // Resync session counts before checking limits — corrects any drift from
        // crashes or missed decrements so we don't wrongly reject new sessions.
        if let Err(e) = self.registry.resync_session_counts(db).await {
            tracing::warn!(error = %e, "Failed to resync session counts before start");
        }

        // Check if a container already exists for this (repo, branch)
        let existing = self.registry.get_container(repo, branch).await;
        let (container_name, grpc_port) = if let Some(ref entry) = existing {
            if entry.state != ContainerState::Running {
                return Err(anyhow!(
                    "Container for {}/{} is in '{}' state, cannot start new session",
                    repo,
                    branch,
                    entry.state
                ));
            }

            // Check max sessions limit
            if s.container_max_sessions > 0 && entry.session_count >= s.container_max_sessions {
                return Err(anyhow!(
                    "Container for {}/{} has reached max sessions ({}). Stop a session first.",
                    repo,
                    branch,
                    s.container_max_sessions
                ));
            }

            // Warn about same-branch concurrent sessions (skip for team/worktree sessions —
            // teams share a container by design, worktrees have file isolation)
            let is_team = session_type == "team_member" || session_type == "team_lead";
            // Worktree paths always contain /worktrees/ (from SM_WORKTREES_PATH config)
            let is_worktree = project_path.contains("/worktrees/");
            if entry.session_count > 0 && !is_team && !is_worktree {
                warning = Some(format!(
                    "Warning: Another session is already active on `{}@{}`. \
                     Concurrent file writes on the same branch may cause conflicts. \
                     Consider using `--worktree` for branch isolation.",
                    repo, branch
                ));
                tracing::warn!(
                    repo,
                    branch,
                    session_count = entry.session_count,
                    "Same-branch concurrent session started"
                );
            }

            // Check devcontainer.json hash for rebuild
            let current_hash = devcontainer::hash_config(project_path).await;
            if let (Some(stored_hash), Some(current)) =
                (&entry.devcontainer_json_hash, &current_hash)
                && stored_hash != current
            {
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
                grpc_port = entry.grpc_port,
                "Reusing existing container (fast path)"
            );
            // grpc_port=0 means pre-migration container — fall back to config default
            let port = if entry.grpc_port == 0 {
                s.grpc_port_start
            } else {
                entry.grpc_port
            };
            (entry.container_name.clone(), port)
        } else {
            // Cold start: no existing container, run devcontainer up
            self.cold_start(project_path, repo, branch, db).await?
        };

        // On the fast path (reuse), the container may be stopped after a VM reboot.
        // Ensure it's running at the podman level before trying to reach the worker.
        if existing.is_some() {
            match ensure_container_started(&container_name).await {
                Ok(restarted) => {
                    if restarted {
                        tracing::info!(
                            repo, branch, container = %container_name,
                            "Container was stopped (VM reboot?), restarted it"
                        );
                    }
                }
                Err(e) => {
                    // Container is gone — remove from registry and return error.
                    // The caller can retry, which will take the cold start path.
                    tracing::warn!(
                        repo, branch, container = %container_name, error = %e,
                        "Container no longer exists, removing from registry"
                    );
                    self.registry
                        .decrement_sessions(db, repo, branch)
                        .await
                        .ok();
                    self.registry.remove_container(db, repo, branch).await.ok();
                    return Err(anyhow!(
                        "Container for {}/{} no longer exists (removed from registry). Retry to create a new one.",
                        repo,
                        branch
                    ));
                }
            }

            // Ensure the agent worker is running (starts via SSH if needed).
            // On cold start, devcontainer up's postStartCommand handles this,
            // but on fast path (reuse), the worker may have crashed or never started.
            let grpc_addr = format!("http://{}:{}", s.vm_host, grpc_port);
            ensure_worker_running(&container_name, &grpc_addr).await?;
        }

        // Set up message channel, connect gRPC, and spawn processor
        let reused = existing.is_some();
        self.create_session_internal(
            session_id,
            &container_name,
            project_path,
            repo,
            branch,
            output_tx,
            plan_mode,
            thinking_mode,
            session_type,
            grpc_port,
            system_prompt_append,
        )
        .await?;

        Ok(StartResult {
            container_name,
            reused,
            warning,
        })
    }

    /// Cold start: allocate port, generate devcontainer config if needed,
    /// write override config, run `devcontainer up` with --override-config,
    /// wait for worker health check, register container.
    /// Returns (container_name, grpc_port).
    async fn cold_start(
        &self,
        project_path: &str,
        repo: &str,
        branch: &str,
        db: &Database,
    ) -> Result<(String, u16)> {
        let s = settings();

        // Probe the VM for ports already bound by existing containers (including
        // ones removed from the registry but still alive at the podman level).
        let vm_used_ports = get_vm_used_ports().await;

        // Allocate a unique port for this container
        let grpc_port = self
            .registry
            .allocate_port(s.grpc_port_start, &vm_used_ports)
            .await;

        // Read existing devcontainer.json content (if any)
        let existing_config = devcontainer::read_config_content(project_path).await;

        if existing_config.is_none() {
            // No devcontainer.json — generate a complete default one with port mapping
            let default_config = devcontainer::generate_default_config(
                &s.container_image,
                &s.container_network,
                grpc_port,
            );
            devcontainer::write_default_config(project_path, &default_config).await?;
            tracing::info!(
                project_path = %project_path, grpc_port,
                "Generated default devcontainer.json (no existing config found)"
            );
        }

        // Build override config by merging our settings into the existing config.
        // For repos with their own devcontainer.json, this adds port mapping and worker startup.
        // For repos with our generated default, this is a no-op merge (same settings).
        let override_path = if let Some(ref content) = existing_config {
            let merged = devcontainer::build_override_config(content, grpc_port)?;
            let path = devcontainer::write_override_config(grpc_port, &merged).await?;
            Some(path)
        } else {
            None
        };

        // Hash the devcontainer.json for future rebuild detection
        let config_hash = devcontainer::hash_config(project_path).await;

        // Remove orphaned CSM-managed devcontainers for this workspace path.
        // The devcontainer CLI is idempotent — if a container already exists for this
        // workspace folder (from a previous lifecycle our registry doesn't know about),
        // `devcontainer up` tries to reuse it. If that ghost container has mismatched
        // ports or a broken state, `devcontainer up` hangs. Clean up first.
        // Only removes containers with csm.managed=true label to avoid touching
        // manually-created devcontainers.
        let escaped_project_path = shell_escape(project_path);
        let find_orphan_cmd = format!(
            "{} ps -aq --filter label=csm.managed=true --filter label=devcontainer.local_folder={}",
            shell_escape(&s.container_runtime),
            escaped_project_path,
        );
        if let Ok(orphan_ids) = ssh::run_command(&find_orphan_cmd).await {
            let ids: Vec<&str> = orphan_ids
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .collect();
            if !ids.is_empty() {
                tracing::warn!(
                    project_path = %project_path,
                    orphan_count = ids.len(),
                    "Found orphaned devcontainer(s) not in registry, removing before cold start"
                );
                for id in &ids {
                    let rm_cmd = format!(
                        "{} rm -f {}",
                        shell_escape(&s.container_runtime),
                        shell_escape(id),
                    );
                    if let Err(e) = ssh::run_command(&rm_cmd).await {
                        tracing::warn!(container = %id, error = %e, "Failed to remove orphaned container");
                    }
                }
            }
        }

        // Start container via devcontainer up, with override config if repo has its own
        let devcontainer_cmd = if let Some(ref override_path) = override_path {
            let escaped_override_path = shell_escape(override_path);
            format!(
                "devcontainer up --docker-path {} --workspace-folder {} --override-config {}",
                shell_escape(&s.container_runtime),
                escaped_project_path,
                escaped_override_path,
            )
        } else {
            format!(
                "devcontainer up --docker-path {} --workspace-folder {}",
                shell_escape(&s.container_runtime),
                escaped_project_path,
            )
        };

        let timeout = Duration::from_secs(s.devcontainer_timeout_secs);
        let output_future = ssh::command()?
            .arg(ssh::login_shell(&devcontainer_cmd))
            .output();
        let output = tokio::time::timeout(timeout, output_future)
            .await
            .map_err(|_| {
                anyhow!(
                    "devcontainer up timed out after {}s",
                    s.devcontainer_timeout_secs
                )
            })??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("devcontainer up failed: {}", stderr));
        }

        // Parse JSON output to get container ID
        let output_str = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = serde_json::from_str(output_str.trim())
            .map_err(|e| anyhow!("Failed to parse devcontainer output: {}", e))?;
        let container_name = json["containerId"]
            .as_str()
            .ok_or_else(|| anyhow!("No containerId in devcontainer output"))?
            .to_string();

        // Verify the returned container is actually for the right workspace.
        // devcontainer up can reuse a stale container from a previous lifecycle
        // that was created for a different repo but has the same workspace label.
        let verify_cmd = format!(
            "{} inspect --format '{{{{index .Config.Labels \"devcontainer.local_folder\"}}}}' {}",
            shell_escape(&s.container_runtime),
            shell_escape(&container_name),
        );
        if let Ok(label) = ssh::run_command(&verify_cmd).await {
            let label = label.trim();
            if !label.is_empty() && label != project_path {
                tracing::warn!(
                    expected = %project_path,
                    actual = %label,
                    container = %container_name,
                    "devcontainer up returned a container for a different workspace, removing stale container"
                );
                // Remove the stale container so the next attempt creates a fresh one
                let rm_cmd = format!(
                    "{} rm -f {}",
                    shell_escape(&s.container_runtime),
                    shell_escape(&container_name),
                );
                let _ = ssh::run_command(&rm_cmd).await;
                return Err(anyhow!(
                    "Removed stale container for wrong workspace (expected {}, got {}). Retry will create a fresh container.",
                    project_path,
                    label
                ));
            }
        }

        // Wait for worker health check on the allocated port.
        // If postStartCommand didn't start the worker (e.g. override config merge issue),
        // fall back to starting it via SSH.
        let grpc_addr = format!("http://{}:{}", s.vm_host, grpc_port);
        if crate::grpc::wait_for_health(&grpc_addr, 10, Duration::from_secs(1))
            .await
            .is_err()
        {
            tracing::warn!(
                repo,
                branch,
                grpc_port,
                "postStartCommand worker not responding, starting via SSH fallback"
            );
            ensure_worker_running(&container_name, &grpc_addr).await?;
        }

        // Register container in registry and database, with session_count = 1
        self.registry
            .register_container(
                db,
                repo,
                branch,
                &container_name,
                config_hash.as_deref(),
                grpc_port,
            )
            .await?;
        self.registry.increment_sessions(db, repo, branch).await?;

        tracing::info!(
            repo, branch, container = %container_name, grpc_port,
            "Cold-started new container"
        );
        Ok((container_name, grpc_port))
    }

    /// Internal helper to create session state and spawn message processor.
    /// Connects to the gRPC worker and opens a bidirectional session stream
    /// before spawning the message processor, so the caller gets immediate
    /// feedback if the connection fails.
    #[allow(clippy::too_many_arguments)]
    async fn create_session_internal(
        &self,
        session_id: &str,
        container_name: &str,
        _project_path: &str,
        repo: &str,
        branch: &str,
        output_tx: mpsc::Sender<OutputEvent>,
        plan_mode: bool,
        thinking_mode: bool,
        session_type: &str,
        grpc_port: u16,
        system_prompt_append: Option<&str>,
    ) -> Result<()> {
        let s = settings();
        let grpc_addr = format!("http://{}:{}", s.vm_host, grpc_port);

        // Connect to gRPC worker (with retry/restart)
        let mut executor = match GrpcExecutor::connect(&grpc_addr).await {
            Ok(e) => e,
            Err(_) => {
                tracing::warn!(session_id = %session_id, "Initial gRPC connect failed, attempting worker restart");
                ensure_worker_running(container_name, &grpc_addr).await?;
                GrpcExecutor::connect(&grpc_addr).await?
            }
        };

        // Open bidirectional stream with timeout
        let grpc_stream = tokio::time::timeout(Duration::from_secs(30), executor.open_stream())
            .await
            .map_err(|_| anyhow!("gRPC open_stream timed out after 30s (handshake deadlock?)"))?
            .map_err(|e| anyhow!("Failed to open gRPC stream: {}", e))?;

        let (message_tx, message_rx) = mpsc::channel::<String>(32);
        let is_first_message = Arc::new(AtomicBool::new(true));
        let claude_session_id = Arc::new(Mutex::new(None));
        let plan_mode_flag = Arc::new(AtomicBool::new(plan_mode));
        let thinking_mode_flag = Arc::new(AtomicBool::new(thinking_mode));
        let pending_title_flag = Arc::new(AtomicBool::new(false));
        let last_input_tokens_flag = Arc::new(AtomicU64::new(0));

        let session_id_owned = session_id.to_string();
        let is_first_clone = Arc::clone(&is_first_message);
        let claude_sid_clone = Arc::clone(&claude_session_id);
        let plan_mode_clone = Arc::clone(&plan_mode_flag);
        let thinking_mode_clone = Arc::clone(&thinking_mode_flag);
        let pending_title_clone = Arc::clone(&pending_title_flag);
        let container_name_owned = container_name.to_string();
        let grpc_addr_owned = grpc_addr.clone();
        let system_prompt_owned = system_prompt_append.unwrap_or("").to_string();
        tokio::spawn(async move {
            message_processor(
                message_rx,
                output_tx,
                session_id_owned,
                is_first_clone,
                claude_sid_clone,
                plan_mode_clone,
                thinking_mode_clone,
                pending_title_clone,
                grpc_stream,
                container_name_owned,
                grpc_addr_owned,
                system_prompt_owned,
            )
            .await;
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
                thinking_mode: thinking_mode_flag,
                pending_title: pending_title_flag,
                last_input_tokens: last_input_tokens_flag,
                grpc_addr: grpc_addr.clone(),
            },
        );

        Ok(())
    }

    /// Reconnect to an existing session after a restart.
    /// Tries progressively harder recovery:
    ///  1. If the container is stopped, start it and reconnect the gRPC worker.
    ///  2. If the container is gone entirely, cold-start a new one (devcontainer up).
    ///
    /// Creates in-memory state (channels, message_processor) in all success paths.
    #[allow(clippy::too_many_arguments)]
    pub async fn reconnect(
        &self,
        session_id: &str,
        container_name: &str,
        project_path: &str,
        repo: &str,
        branch: &str,
        session_type: &str,
        db: &Database,
        output_tx: mpsc::Sender<OutputEvent>,
        grpc_port: u16,
        system_prompt_append: Option<&str>,
    ) -> Result<()> {
        // Resync session counts before reconnecting
        if let Err(e) = self.registry.resync_session_counts(db).await {
            tracing::warn!(error = %e, "Failed to resync session counts before reconnect");
        }

        // Determine the actual container name and gRPC port to use.
        // If the container is stopped, start it. If it's gone, run a full cold start.
        let (effective_container, effective_port) = match ensure_container_started(container_name)
            .await
        {
            Ok(restarted) => {
                if restarted {
                    tracing::info!(
                        session_id = %session_id,
                        container = %container_name,
                        "Container was stopped, restarted it for reconnect"
                    );
                }
                (container_name.to_string(), grpc_port)
            }
            Err(e) => {
                // Container is gone — run devcontainer up to recreate it.
                tracing::warn!(
                    session_id = %session_id,
                    container = %container_name,
                    error = %e,
                    "Container missing on reconnect, attempting cold-start recovery"
                );
                // Clear from registry so cold_start doesn't take the fast-reuse path
                let _ = self.registry.remove_container(db, repo, branch).await;

                let (new_name, new_port) = self.cold_start(project_path, repo, branch, db).await?;
                // Update the session's container_name in the DB so future restarts
                // find the right container.
                if let Err(db_err) = db
                    .update_session_container_name(session_id, &new_name)
                    .await
                {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %db_err,
                        "Failed to update session container_name after cold-start recovery"
                    );
                }
                tracing::info!(
                    session_id = %session_id,
                    old_container = %container_name,
                    new_container = %new_name,
                    new_port,
                    "Cold-started replacement container for reconnect"
                );
                (new_name, new_port)
            }
        };

        self.create_session_internal(
            session_id,
            &effective_container,
            project_path,
            repo,
            branch,
            output_tx,
            false,
            false,
            session_type,
            effective_port,
            system_prompt_append,
        )
        .await?;

        tracing::info!(
            session_id = %session_id,
            container = %effective_container,
            grpc_port = effective_port,
            "Reconnected session (fresh conversation)"
        );
        Ok(())
    }

    pub async fn send(&self, session_id: &str, text: &str) -> Result<()> {
        match self.sessions.get(session_id) {
            Some(session) => {
                session.message_tx.send(text.to_string()).await?;
            }
            None => {
                return Err(anyhow!(
                    "Session {} not found in-memory (stale DB record after restart?)",
                    session_id
                ));
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
        self.sessions
            .remove(session_id)
            .map(|(_, session)| ClaimedSession {
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
        let output_future = ssh::command()?.arg(ssh::login_shell(&rm_cmd)).output();
        tokio::time::timeout(timeout, output_future)
            .await
            .map_err(|_| anyhow!("Container removal timed out after {}s", s.ssh_timeout_secs))??;
        Ok(())
    }

    /// Restart the Claude conversation inside a session.
    /// Resets the first-message flag and clears the Claude session ID
    /// so the next message starts a fresh conversation.
    pub async fn restart_session(&self, session_id: &str) -> Result<()> {
        let session = self
            .sessions
            .get(session_id)
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

    /// Interrupt a running Claude session via a separate gRPC unary RPC.
    /// Returns Ok(true) if interrupted, Ok(false) if no active session.
    pub async fn interrupt(&self, session_id: &str) -> Result<bool> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow!("Session {} not found", session_id))?;

        let claude_sid = session.claude_session_id.lock().unwrap().clone();
        let grpc_addr = session.grpc_addr.clone();
        drop(session); // Release DashMap ref before async call

        let Some(claude_sid) = claude_sid else {
            return Ok(false);
        };

        let mut executor = GrpcExecutor::connect(&grpc_addr).await?;
        executor.interrupt(&claude_sid).await
    }

    /// Store the last known input_tokens from a ResponseComplete event
    pub fn set_last_input_tokens(&self, session_id: &str, tokens: u64) {
        if let Some(s) = self.sessions.get(session_id) {
            s.last_input_tokens.store(tokens, Ordering::SeqCst);
        }
    }

    /// Get plan mode status for a session
    pub fn get_plan_mode(&self, session_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .map(|s| s.plan_mode.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// Set thinking mode for a session
    pub fn set_thinking_mode(&self, session_id: &str, enabled: bool) {
        if let Some(session) = self.sessions.get(session_id) {
            session.thinking_mode.store(enabled, Ordering::SeqCst);
        }
    }

    /// Get thinking mode status for a session
    pub fn get_thinking_mode(&self, session_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .map(|s| s.thinking_mode.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// Get live runtime info for a session
    pub fn get_session_info(&self, session_id: &str) -> Option<SessionInfo> {
        self.sessions.get(session_id).map(|s| SessionInfo {
            claude_session_id: s.claude_session_id.lock().unwrap().clone(),
            plan_mode: s.plan_mode.load(Ordering::SeqCst),
            thinking_mode: s.thinking_mode.load(Ordering::SeqCst),
            is_first_message: s.is_first_message.load(Ordering::SeqCst),
            last_input_tokens: s.last_input_tokens.load(Ordering::SeqCst),
        })
    }

    /// Find and claim all sessions that belong to a given (repo, branch) container.
    /// Returns the list of claimed sessions.
    pub fn stop_all_sessions_for_container(
        &self,
        repo: &str,
        branch: &str,
    ) -> Vec<(String, ClaimedSession)> {
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
                claimed.push((
                    id,
                    ClaimedSession {
                        name: session.name,
                        worktree_path: session.worktree_path,
                        repo: session.repo,
                        branch: session.branch,
                    },
                ));
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
        if let Some(ref name) = container_name
            && let Err(e) = self.remove_container_by_name(name).await
        {
            tracing::warn!(
                container = %name, repo, branch, error = %e,
                "Failed to remove container via SSH (may already be gone)"
            );
        }

        // Remove from registry and mark stopped in DB
        self.registry.remove_container(db, repo, branch).await?;

        tracing::info!(
            repo,
            branch,
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
    pub thinking_mode: bool,
    pub is_first_message: bool,
    pub last_input_tokens: u64,
}

/// Query the VM for host ports mapped to container port 50051 across ALL containers
/// (including ones not tracked in the registry). This prevents port collisions
/// when stale containers survive a failed teardown or VM restart.
async fn get_vm_used_ports() -> std::collections::HashSet<u16> {
    let s = settings();
    // List all containers (running or stopped) and extract host port mappings for 50051
    let cmd = format!(
        "{} ps -a --format '{{{{.Ports}}}}' | grep -oP '\\d+(?=->50051)'",
        shell_escape(&s.container_runtime),
    );
    match ssh::run_command(&cmd).await {
        Ok(output) => output
            .lines()
            .filter_map(|line| line.trim().parse::<u16>().ok())
            .collect(),
        Err(_) => {
            // If the query fails, return empty set — allocate_port falls back to
            // registry-only check, which is the previous behavior
            std::collections::HashSet::new()
        }
    }
}

/// Ensure a container is running at the podman/docker level.
/// After a VM reboot, containers exist but are stopped — `podman start` restarts them.
/// Returns Ok(true) if the container was restarted, Ok(false) if already running.
pub async fn ensure_container_started(container_name: &str) -> Result<bool> {
    let s = settings();
    let escaped_name = shell_escape(container_name);

    // Check if container is running
    let inspect_cmd = format!(
        "{} inspect --format '{{{{.State.Running}}}}' {}",
        shell_escape(&s.container_runtime),
        escaped_name,
    );
    let inspect_result = ssh::run_command(&inspect_cmd).await;

    let is_running = inspect_result
        .as_ref()
        .map(|out| out.trim() == "true")
        .unwrap_or(false);

    if is_running {
        return Ok(false);
    }

    // Check if the container exists at all (inspect succeeds even for stopped containers)
    if inspect_result.is_err() {
        return Err(anyhow!(
            "Container {} does not exist on the VM (may have been removed)",
            container_name
        ));
    }

    // Container exists but is stopped — start it
    tracing::info!(
        container_name,
        "Container is stopped, starting via podman/docker"
    );
    let start_cmd = format!(
        "{} start {}",
        shell_escape(&s.container_runtime),
        escaped_name,
    );
    ssh::run_command(&start_cmd).await.map_err(|e| {
        anyhow!(
            "Failed to start stopped container {}: {}",
            container_name,
            e
        )
    })?;

    tracing::info!(container_name, "Container restarted successfully");
    Ok(true)
}

/// Check if the agent worker is healthy; if not, start it via SSH and wait.
/// Used on the fast path (container reuse) and as a retry fallback in message_processor.
async fn ensure_worker_running(container_name: &str, grpc_addr: &str) -> Result<()> {
    // Quick health check — don't wait long
    if let Ok(mut executor) = GrpcExecutor::connect(grpc_addr).await
        && let Ok((true, _)) = executor.health().await
    {
        return Ok(());
    }

    tracing::warn!(
        container_name,
        "Agent worker not responding, starting via SSH"
    );

    let s = settings();
    let start_cmd = format!(
        "devcontainer exec --docker-path {} --container-id {} \
         sh -c 'nohup python3 -m agent_worker --port 50051 > /tmp/agent-worker.log 2>&1 &'",
        shell_escape(&s.container_runtime),
        shell_escape(container_name),
    );
    ssh::run_command(&start_cmd).await?;

    // Wait for it to become healthy (15 retries, 1s each)
    crate::grpc::wait_for_health(grpc_addr, 15, Duration::from_secs(1)).await
}

/// Attempt to reconnect the gRPC stream to the agent-worker.
/// Ensures the worker is running, connects, and opens a new bidirectional stream.
async fn reconnect_stream(container_name: &str, grpc_addr: &str) -> Result<GrpcStream> {
    ensure_worker_running(container_name, grpc_addr).await?;
    let mut executor = GrpcExecutor::connect(grpc_addr).await?;
    tokio::time::timeout(Duration::from_secs(30), executor.open_stream())
        .await
        .map_err(|_| anyhow!("Reconnect stream timed out"))?
}

/// Per-session processor task. Uses an already-established gRPC stream.
///
/// Runs a continuous `select!` loop that simultaneously:
/// - Accepts user messages from `message_rx` (when not already in a turn)
/// - Reads gRPC events from the SDK stream (always, even between turns)
///
/// When the gRPC stream fails, instead of exiting, the processor enters a
/// "disconnected" state and waits for the next user message to attempt
/// reconnection. This enables auto-recovery from agent-worker crashes.
#[allow(clippy::too_many_arguments)]
async fn message_processor(
    mut message_rx: mpsc::Receiver<String>,
    output_tx: mpsc::Sender<OutputEvent>,
    session_id: String,
    is_first_message: Arc<AtomicBool>,
    claude_session_id: Arc<Mutex<Option<String>>>,
    plan_mode: Arc<AtomicBool>,
    thinking_mode: Arc<AtomicBool>,
    pending_title: Arc<AtomicBool>,
    stream: GrpcStream,
    container_name: String,
    grpc_addr: String,
    system_prompt_append: String,
) {
    use grpc::proto::agent_event;

    let mut in_turn = false;
    let mut in_background = false;
    let mut partial_buf = String::new();
    let mut event_count: u64 = 0;
    // Stream is Option: None = disconnected, Some = connected
    let mut stream: Option<GrpcStream> = Some(stream);

    loop {
        // When disconnected, only listen for user messages (to trigger reconnect)
        if stream.is_none() {
            let Some(message) = message_rx.recv().await else {
                tracing::debug!(session_id = %session_id, "Message channel closed (disconnected)");
                break;
            };

            tracing::info!(session_id = %session_id, "Attempting reconnect on user message");
            match reconnect_stream(&container_name, &grpc_addr).await {
                Ok(new_stream) => {
                    stream = Some(new_stream);
                    is_first_message.store(true, Ordering::SeqCst);
                    let _ = output_tx.send(OutputEvent::WorkerReconnected).await;

                    // Send the message as a CreateSession with resume_session_id
                    let permission_mode = if plan_mode.load(Ordering::SeqCst) {
                        "plan"
                    } else {
                        "bypassPermissions"
                    };
                    let thinking_tokens = if thinking_mode.load(Ordering::SeqCst) {
                        10000
                    } else {
                        0
                    };
                    let resume_sid = claude_session_id.lock().unwrap().clone();

                    let _ = output_tx
                        .send(OutputEvent::ProcessingStarted { input_tokens: 0 })
                        .await;

                    let send_result = stream
                        .as_mut()
                        .unwrap()
                        .create(
                            &message,
                            permission_mode,
                            std::collections::HashMap::new(),
                            &system_prompt_append,
                            thinking_tokens,
                            resume_sid.as_deref(),
                        )
                        .await;

                    if let Err(e) = send_result {
                        tracing::error!(session_id = %session_id, error = %e, "Failed to send after reconnect");
                        let _ = output_tx
                            .send(OutputEvent::ProcessDied {
                                exit_code: Some(1),
                                signal: Some(format!("Reconnect send error: {}", e)),
                            })
                            .await;
                        break;
                    }

                    in_turn = true;
                    in_background = false;
                    partial_buf.clear();
                }
                Err(e) => {
                    tracing::error!(session_id = %session_id, error = %e, "Reconnect failed");
                    let _ = output_tx
                        .send(OutputEvent::ProcessDied {
                            exit_code: Some(1),
                            signal: Some(format!("Reconnect failed: {}", e)),
                        })
                        .await;
                    break;
                }
            }
            continue;
        }

        // Connected path — normal select! loop
        let active_stream = stream.as_mut().unwrap();

        tokio::select! {
            biased;

            // Accept new user messages only when not already processing a turn
            msg = message_rx.recv(), if !in_turn => {
                let Some(message) = msg else {
                    // Channel closed — session dropped
                    tracing::debug!(session_id = %session_id, "Message channel closed");
                    break;
                };

                let stored_sid = claude_session_id.lock().unwrap().clone();
                let is_first = is_first_message.load(Ordering::SeqCst);

                let permission_mode = if plan_mode.load(Ordering::SeqCst) {
                    "plan"
                } else {
                    "bypassPermissions"
                };

                let thinking_tokens = if thinking_mode.load(Ordering::SeqCst) { 10000 } else { 0 };

                tracing::info!(
                    session_id = %session_id,
                    is_first,
                    has_session_id = stored_sid.is_some(),
                    permission_mode,
                    thinking_tokens,
                    "Sending message via gRPC"
                );

                // Emit ProcessingStarted immediately
                let _ = output_tx
                    .send(OutputEvent::ProcessingStarted { input_tokens: 0 })
                    .await;

                let _is_title_request = pending_title.swap(false, Ordering::SeqCst);

                let send_result = if is_first || stored_sid.is_none() {
                    active_stream.create(
                        &message,
                        permission_mode,
                        std::collections::HashMap::new(),
                        &system_prompt_append,
                        thinking_tokens,
                        None,
                    ).await
                } else {
                    active_stream.follow_up(&message).await
                };

                if let Err(e) = send_result {
                    tracing::error!(
                        session_id = %session_id,
                        error = %e,
                        "Failed to send gRPC request"
                    );
                    // Treat send failure as disconnection — enter reconnect mode
                    let _ = output_tx.send(OutputEvent::WorkerDisconnected {
                        reason: format!("gRPC send error: {}", e),
                    }).await;
                    stream = None;
                    in_turn = false;
                    in_background = false;
                    continue;
                }

                in_turn = true;
                in_background = false;
                partial_buf.clear();
            }

            // Continuously read gRPC events (both during and between turns)
            event_result = active_stream.next_event() => {
                match event_result {
                    Ok(Some(event)) => {
                        event_count += 1;
                        let Some(event_variant) = event.event else {
                            tracing::debug!(event_count, "gRPC: Empty event (no variant)");
                            continue;
                        };

                        // If we're between turns, this is a background event —
                        // emit ProcessingStarted once to create a LiveCard
                        if !in_turn && !in_background {
                            in_background = true;
                            partial_buf.clear();
                            let _ = output_tx
                                .send(OutputEvent::ProcessingStarted { input_tokens: 0 })
                                .await;
                        }

                        if let agent_event::Event::Text(ref t) = event_variant {
                            tracing::debug!(
                                event_count,
                                is_partial = t.is_partial,
                                text_len = t.text.len(),
                                "gRPC: Text event received"
                            );
                        }

                        match event_variant {
                            agent_event::Event::SessionInit(init) => {
                                let stored_sid = claude_session_id.lock().unwrap().clone();
                                if stored_sid.is_none() {
                                    tracing::info!(session_id_grpc = %init.session_id, "gRPC: SessionInit received");
                                    *claude_session_id.lock().unwrap() = Some(init.session_id.clone());
                                    // Emit SessionIdCaptured for DB persistence
                                    let _ = output_tx.send(OutputEvent::SessionIdCaptured(init.session_id)).await;
                                }
                            }
                            agent_event::Event::Text(text) => {
                                if text.is_partial {
                                    partial_buf.push_str(&text.text);
                                    while let Some(newline_pos) = partial_buf.find('\n') {
                                        let line = partial_buf[..newline_pos].to_string();
                                        partial_buf = partial_buf[newline_pos + 1..].to_string();
                                        if output_tx.send(OutputEvent::TextLine(line)).await.is_err() {
                                            tracing::warn!("gRPC: output_tx closed during partial text");
                                            return;
                                        }
                                    }
                                } else {
                                    // Complete text — flush partial buffer first
                                    if !partial_buf.is_empty()
                                        && output_tx
                                            .send(OutputEvent::TextLine(std::mem::take(&mut partial_buf)))
                                            .await
                                            .is_err()
                                    {
                                        return;
                                    }
                                    for line in text.text.lines() {
                                        if output_tx.send(OutputEvent::TextLine(line.to_string())).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                            agent_event::Event::ToolUse(tool) => {
                                if tool.input_json.is_empty() || tool.input_json == "{}" {
                                    continue;
                                }
                                // Flush partial text before tool action
                                if !partial_buf.is_empty() {
                                    let _ = output_tx
                                        .send(OutputEvent::TextLine(std::mem::take(&mut partial_buf)))
                                        .await;
                                }
                                let action = grpc::format_grpc_tool_action(&tool.tool_name, &tool.input_json);
                                if output_tx.send(OutputEvent::ToolAction(action)).await.is_err() {
                                    return;
                                }
                            }
                            agent_event::Event::ToolResult(_) => {
                                // Informational — not surfaced to chat
                            }
                            agent_event::Event::Subagent(sub) => {
                                let action = if sub.is_start { "started" } else { "finished" };
                                tracing::debug!(
                                    agent = %sub.agent_name,
                                    parent_tool = %sub.parent_tool_use_id,
                                    action,
                                    "Subagent event"
                                );
                            }
                            agent_event::Event::Result(result) => {
                                // Flush partial text
                                if !partial_buf.is_empty() {
                                    let _ = output_tx
                                        .send(OutputEvent::TextLine(std::mem::take(&mut partial_buf)))
                                        .await;
                                }

                                let input_tokens = result.input_tokens;
                                let output_tokens = result.output_tokens;
                                let context_tokens = result.context_tokens;
                                tracing::info!(
                                    event_count,
                                    input_tokens,
                                    output_tokens,
                                    context_tokens,
                                    is_error = result.is_error,
                                    "gRPC: Result received (turn complete)"
                                );

                                if result.is_error {
                                    let _ = output_tx
                                        .send(OutputEvent::ProcessDied {
                                            exit_code: Some(1),
                                            signal: None,
                                        })
                                        .await;
                                } else {
                                    let _ = output_tx
                                        .send(OutputEvent::ResponseComplete {
                                            input_tokens,
                                            output_tokens,
                                            context_tokens,
                                        })
                                        .await;
                                }

                                // Turn/background burst complete
                                if in_turn {
                                    is_first_message.store(false, Ordering::SeqCst);
                                }
                                in_turn = false;
                                in_background = false;
                            }
                            agent_event::Event::Error(err) => {
                                tracing::error!(
                                    event_count,
                                    error_type = %err.error_type,
                                    message = %err.message,
                                    "gRPC: Agent error"
                                );
                                let _ = output_tx
                                    .send(OutputEvent::ProcessDied {
                                        exit_code: Some(1),
                                        signal: Some(format!("{}: {}", err.error_type, err.message)),
                                    })
                                    .await;
                                in_turn = false;
                                in_background = false;
                            }
                        }
                    }
                    Ok(None) => {
                        // Stream ended (server closed) — enter disconnected state
                        tracing::warn!(event_count, session_id = %session_id, "gRPC: Response stream ended, entering disconnected state");
                        if !partial_buf.is_empty() {
                            let _ = output_tx
                                .send(OutputEvent::TextLine(std::mem::take(&mut partial_buf)))
                                .await;
                        }
                        let _ = output_tx.send(OutputEvent::WorkerDisconnected {
                            reason: "gRPC stream closed by server".to_string(),
                        }).await;
                        stream = None;
                        in_turn = false;
                        in_background = false;
                    }
                    Err(e) => {
                        // Stream error — enter disconnected state
                        tracing::warn!(event_count, error = %e, session_id = %session_id, "gRPC: Stream error, entering disconnected state");
                        let _ = output_tx.send(OutputEvent::WorkerDisconnected {
                            reason: format!("gRPC stream error: {}", e),
                        }).await;
                        stream = None;
                        in_turn = false;
                        in_background = false;
                    }
                }
            }
        }
    }

    tracing::debug!(session_id = %session_id, "Message processor exiting");
}
