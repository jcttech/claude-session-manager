use anyhow::Result;
use dashmap::DashMap;
use shell_escape::escape;
use std::borrow::Cow;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::config::settings;

/// Escape a string for safe use in shell commands
fn shell_escape(s: &str) -> Cow<'_, str> {
    escape(Cow::Borrowed(s))
}

pub struct ContainerManager {
    sessions: DashMap<String, Session>,
}

struct Session {
    name: String,
    stdin_tx: mpsc::Sender<String>,
}

impl ContainerManager {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    pub async fn start(
        &self,
        session_id: &str,
        project_path: &str,
        output_tx: mpsc::Sender<String>,
    ) -> Result<String> {
        let s = settings();
        let name = format!("claude-{}", &session_id[..8]);

        // Start container via SSH with proper escaping
        // All user-controllable inputs are escaped to prevent command injection
        let escaped_project_path = shell_escape(project_path);
        let escaped_name = shell_escape(&name);
        let escaped_network = shell_escape(&s.container_network);
        let escaped_image = shell_escape(&s.container_image);
        let escaped_config_volume = shell_escape(&s.claude_config_volume);
        let escaped_config_path = shell_escape(&s.claude_config_path);

        // Mount shared Claude config volume for authentication persistence
        // Also pass ANTHROPIC_API_KEY as fallback for first-time auth
        // Priority: stored credentials in volume > ANTHROPIC_API_KEY env var
        let container_cmd = format!(
            "{} run -d --name {} --network {} -v {}:/workspace -v {}:{} -e ANTHROPIC_API_KEY=$ANTHROPIC_API_KEY {} sleep infinity",
            shell_escape(&s.container_runtime),
            escaped_name,
            escaped_network,
            escaped_project_path,
            escaped_config_volume,
            escaped_config_path,
            escaped_image
        );

        Command::new("ssh")
            .arg("-o").arg("StrictHostKeyChecking=accept-new")
            .arg("-i").arg(&s.vm_ssh_key_path)
            .arg(format!("{}@{}", &s.vm_user, &s.vm_host))
            .arg(&container_cmd)
            .output()
            .await?;

        // Start interactive session - we specify the claude command here
        // (container image has no ENTRYPOINT, giving us control over flags)
        let exec_container_cmd = format!(
            "{} exec -i {} {}",
            shell_escape(&s.container_runtime),
            escaped_name,
            &s.claude_command  // claude_command is from config, trusted
        );

        let mut child = Command::new("ssh")
            .arg("-tt")
            .arg("-o").arg("StrictHostKeyChecking=accept-new")
            .arg("-i").arg(&s.vm_ssh_key_path)
            .arg(format!("{}@{}", &s.vm_user, &s.vm_host))
            .arg(&exec_container_cmd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let stdin = child.stdin.take().expect("Failed to get stdin");
        let stdout = child.stdout.take().expect("Failed to get stdout");

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(32);

        // Stdin writer task
        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some(text) = stdin_rx.recv().await {
                let _ = stdin.write_all(text.as_bytes()).await;
                let _ = stdin.write_all(b"\n").await;
                let _ = stdin.flush().await;
            }
        });

        // Stdout reader task
        let reader = BufReader::new(stdout);
        tokio::spawn(async move {
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if output_tx.send(line).await.is_err() {
                    break;
                }
            }
        });

        self.sessions.insert(
            session_id.to_string(),
            Session {
                name: name.clone(),
                stdin_tx,
            },
        );

        Ok(name)
    }

    pub async fn send(&self, session_id: &str, text: &str) -> Result<()> {
        if let Some(session) = self.sessions.get(session_id) {
            session.stdin_tx.send(text.to_string()).await?;
        }
        Ok(())
    }

    pub async fn stop(&self, session_id: &str) -> Result<()> {
        if let Some((_, session)) = self.sessions.remove(session_id) {
            let s = settings();
            let escaped_name = shell_escape(&session.name);
            let container_cmd = format!(
                "{} rm -f {}",
                shell_escape(&s.container_runtime),
                escaped_name
            );

            Command::new("ssh")
                .arg("-o").arg("StrictHostKeyChecking=accept-new")
                .arg("-i").arg(&s.vm_ssh_key_path)
                .arg(format!("{}@{}", &s.vm_user, &s.vm_host))
                .arg(&container_cmd)
                .output()
                .await?;
        }
        Ok(())
    }
}
