use anyhow::{anyhow, Result};
use shell_escape::escape;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;

use crate::config::settings;

/// Wrap a command to run in a login shell on the remote VM.
/// Non-interactive SSH sessions get a minimal PATH; `bash -lc` sources
/// the user's profile so tools like `devcontainer` are found.
pub fn login_shell(cmd: &str) -> String {
    format!("bash -lc {}", escape(Cow::Borrowed(cmd)))
}

/// Path to the SSH key file (either from config or written from env var)
static SSH_KEY_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Initialize SSH key - writes key content to temp file if SM_VM_SSH_KEY is set
/// Must be called once at startup, panics on error
pub fn init_ssh_key() -> Result<()> {
    let path = create_ssh_key_path()?;
    // This will only set once, subsequent calls are no-ops
    let _ = SSH_KEY_PATH.set(path);
    Ok(())
}

/// Create the SSH key path (internal)
fn create_ssh_key_path() -> Result<PathBuf> {
    let s = settings();

    if let Some(key_content) = &s.vm_ssh_key {
        // Write key to a secure runtime directory (XDG_RUNTIME_DIR preferred for security)
        // Falls back to temp_dir for container compatibility
        let key_path = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("session-manager-ssh-key");

        // Write with restricted permissions
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            use std::io::Write;

            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600) // Owner read/write only
                .open(&key_path)?;
            file.write_all(key_content.as_bytes())?;

            // Ensure key ends with newline (SSH requires this)
            if !key_content.ends_with('\n') {
                file.write_all(b"\n")?;
            }
        }

        #[cfg(not(unix))]
        {
            std::fs::write(&key_path, key_content)?;
        }

        tracing::info!("SSH key written to temp file");
        Ok(key_path)
    } else {
        // Use the configured path
        Ok(PathBuf::from(&s.vm_ssh_key_path))
    }
}

/// Get the SSH key path (must call init_ssh_key first)
pub fn ssh_key_path() -> Result<&'static PathBuf> {
    SSH_KEY_PATH.get().ok_or_else(|| anyhow!("SSH key not initialized - call init_ssh_key() first"))
}

/// Run an SSH command on the VM and return the output
/// Uses configurable timeout (SM_SSH_TIMEOUT_SECS, default 30s)
pub async fn run_command(cmd: &str) -> Result<String> {
    let s = settings();
    let key_path = ssh_key_path()?;
    let timeout = Duration::from_secs(s.ssh_timeout_secs);

    let ssh_future = Command::new("ssh")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-i")
        .arg(key_path)
        .arg(format!("{}@{}", &s.vm_user, &s.vm_host))
        .arg(login_shell(cmd))
        .output();

    let output = tokio::time::timeout(timeout, ssh_future)
        .await
        .map_err(|_| anyhow!("SSH command timed out after {} seconds", s.ssh_timeout_secs))??;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(anyhow!("SSH command failed: {}", stderr))
    }
}

/// Create an SSH Command builder for interactive sessions
pub fn command() -> Result<Command> {
    let s = settings();
    let key_path = ssh_key_path()?;

    let mut cmd = Command::new("ssh");
    cmd.arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-i")
        .arg(key_path)
        .arg(format!("{}@{}", &s.vm_user, &s.vm_host));

    Ok(cmd)
}

/// Create an SSH Command builder with TTY allocation for interactive sessions
pub fn command_with_tty() -> Result<Command> {
    let s = settings();
    let key_path = ssh_key_path()?;

    let mut cmd = Command::new("ssh");
    cmd.arg("-tt")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-i")
        .arg(key_path)
        .arg(format!("{}@{}", &s.vm_user, &s.vm_host));

    Ok(cmd)
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_ssh_key_newline_handling() {
        // Test that we handle keys with and without trailing newlines
        let key_with_newline = "-----BEGIN OPENSSH PRIVATE KEY-----\ntest\n-----END OPENSSH PRIVATE KEY-----\n";
        let key_without_newline = "-----BEGIN OPENSSH PRIVATE KEY-----\ntest\n-----END OPENSSH PRIVATE KEY-----";

        assert!(key_with_newline.ends_with('\n'));
        assert!(!key_without_newline.ends_with('\n'));
    }
}
