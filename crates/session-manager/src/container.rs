use anyhow::{anyhow, Result};
use dashmap::DashMap;
use shell_escape::escape;
use std::borrow::Cow;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::config::settings;
use crate::devcontainer;
use crate::ssh;

/// Escape a string for safe use in shell commands
fn shell_escape(s: &str) -> Cow<'_, str> {
    escape(Cow::Borrowed(s))
}

pub struct ContainerManager {
    sessions: DashMap<String, Session>,
}

/// Result of attempting to claim a session for cleanup.
/// If successful, contains the session name and worktree path.
pub struct ClaimedSession {
    pub name: String,
    pub worktree_path: Option<PathBuf>,
}

struct Session {
    name: String,
    stdin_tx: mpsc::Sender<String>,
    /// Path to worktree if session is using one (for cleanup)
    worktree_path: Option<PathBuf>,
}

impl Default for ContainerManager {
    fn default() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }
}

impl ContainerManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn start(
        &self,
        session_id: &str,
        project_path: &str,
        output_tx: mpsc::Sender<String>,
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

        // Start container via devcontainer up (honors full devcontainer.json)
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
        let container_id = json["containerId"].as_str()
            .ok_or_else(|| anyhow!("No containerId in devcontainer output"))?
            .to_string();

        // Start interactive session via podman exec
        let exec_container_cmd = format!(
            "stty -echo; {} exec -i {} {}",
            shell_escape(&s.container_runtime),
            shell_escape(&container_id),
            &s.claude_command  // claude_command is from config, trusted
        );

        let mut child = match ssh::command_with_tty()?
            .arg(ssh::login_shell(&exec_container_cmd))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                // Container is running but exec failed — clean it up
                tracing::error!(container = %container_id, error = %e, "Failed to exec into container, removing orphan");
                let rm_cmd = format!("{} rm -f {}", shell_escape(&s.container_runtime), shell_escape(&container_id));
                let _ = ssh::command().map(|mut cmd| {
                    cmd.arg(ssh::login_shell(&rm_cmd));
                    tokio::spawn(async move { let _ = cmd.output().await; })
                });
                return Err(e.into());
            }
        };

        let stdin = child.stdin.take().expect("Failed to get stdin");
        let stdout = child.stdout.take().expect("Failed to get stdout");
        let stderr = child.stderr.take().expect("Failed to get stderr");

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(32);

        // Stdin writer task
        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some(text) = stdin_rx.recv().await {
                if let Err(e) = stdin.write_all(text.as_bytes()).await {
                    tracing::warn!(error = %e, "Failed to write to container stdin");
                    break;
                }
                if let Err(e) = stdin.write_all(b"\n").await {
                    tracing::warn!(error = %e, "Failed to write newline to container stdin");
                    break;
                }
                if let Err(e) = stdin.flush().await {
                    tracing::warn!(error = %e, "Failed to flush container stdin");
                    break;
                }
            }
        });

        // Stderr drain task (prevents pipe deadlock)
        let session_id_for_stderr = session_id.to_string();
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(session_id = %session_id_for_stderr, stderr = %line, "Container stderr");
            }
        });

        // Stdout reader task + child reaper (prevents zombie)
        let reader = BufReader::new(stdout);
        tokio::spawn(async move {
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if output_tx.send(line).await.is_err() {
                    break;
                }
            }
            // After stdout closes, wait for child to prevent zombie process
            let _ = child.wait().await;
        });

        self.sessions.insert(
            session_id.to_string(),
            Session {
                name: container_id.clone(),
                stdin_tx,
                worktree_path: None,
            },
        );

        Ok(container_id)
    }

    pub async fn send(&self, session_id: &str, text: &str) -> Result<()> {
        if let Some(session) = self.sessions.get(session_id) {
            session.stdin_tx.send(text.to_string()).await?;
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
    pub fn claim_session(&self, session_id: &str) -> Option<ClaimedSession> {
        self.sessions.remove(session_id).map(|(_, session)| ClaimedSession {
            name: session.name,
            worktree_path: session.worktree_path,
        })
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

    /// Restart the Claude process inside a container.
    /// Kills the old process, re-execs with --continue, rewires pipes.
    /// Returns a new mpsc::Receiver<String> for the caller to spawn a new stream_output.
    pub async fn restart_session(&self, session_id: &str) -> Result<mpsc::Receiver<String>> {
        // Step 1: Remove session from DashMap (prevents old stdout reader from triggering cleanup)
        let old_session = self.sessions.remove(session_id)
            .ok_or_else(|| anyhow!("Session {} not found", session_id))?;
        let container_name = old_session.1.name.clone();
        let worktree_path = old_session.1.worktree_path.clone();

        // Step 2: Kill Claude process inside container
        let s = settings();
        let kill_cmd = format!(
            "{} exec {} pkill -f claude",
            shell_escape(&s.container_runtime),
            shell_escape(&container_name),
        );
        let _ = ssh::run_command(&kill_cmd).await;

        // Step 3: Wait for process to die
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Step 4: Re-exec with --continue
        let exec_cmd = format!(
            "stty -echo; {} exec -i {} {} --continue",
            shell_escape(&s.container_runtime),
            shell_escape(&container_name),
            &s.claude_command,
        );

        let mut child = ssh::command_with_tty()?
            .arg(ssh::login_shell(&exec_cmd))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let stdin = child.stdin.take().expect("Failed to get stdin");
        let stdout = child.stdout.take().expect("Failed to get stdout");
        let stderr = child.stderr.take().expect("Failed to get stderr");

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(32);
        let (output_tx, output_rx) = mpsc::channel::<String>(100);

        // Step 5: Spawn new stdin writer, stderr drain, stdout reader tasks
        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some(text) = stdin_rx.recv().await {
                if let Err(e) = stdin.write_all(text.as_bytes()).await {
                    tracing::warn!(error = %e, "Failed to write to container stdin");
                    break;
                }
                if let Err(e) = stdin.write_all(b"\n").await {
                    tracing::warn!(error = %e, "Failed to write newline to container stdin");
                    break;
                }
                if let Err(e) = stdin.flush().await {
                    tracing::warn!(error = %e, "Failed to flush container stdin");
                    break;
                }
            }
        });

        let session_id_for_stderr = session_id.to_string();
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(session_id = %session_id_for_stderr, stderr = %line, "Container stderr (restarted)");
            }
        });

        let reader = BufReader::new(stdout);
        tokio::spawn(async move {
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if output_tx.send(line).await.is_err() {
                    break;
                }
            }
            let _ = child.wait().await;
        });

        // Step 6: Re-insert session with new stdin_tx, same container name/worktree_path
        self.sessions.insert(
            session_id.to_string(),
            Session {
                name: container_name,
                stdin_tx,
                worktree_path,
            },
        );

        // Step 7: Return new receiver
        Ok(output_rx)
    }
}
