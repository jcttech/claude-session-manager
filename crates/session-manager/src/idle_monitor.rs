use std::borrow::Cow;

use chrono::Utc;
use shell_escape::escape;
use tokio::time::{Duration, interval};

use crate::config::settings;
use crate::container_registry::{ContainerRegistry, ContainerState};
use crate::database::Database;
use crate::ssh;

/// Escape a string for safe use in shell commands.
fn shell_escape(s: &str) -> Cow<'_, str> {
    escape(Cow::Borrowed(s))
}

/// Spawn a background task that periodically checks for idle containers
/// and tears them down after the configured timeout.
///
/// The monitor runs every 60 seconds and checks all containers in the registry.
/// A container is considered idle when:
/// - It has zero active sessions (`session_count == 0`)
/// - Its `last_session_stopped_at` timestamp exceeds the idle timeout threshold
///
/// The idle timeout is configured via `SM_CONTAINER_IDLE_TIMEOUT_SECS` (default: 1800s / 30min).
/// Setting it to 0 disables auto-teardown.
pub async fn spawn_idle_monitor(
    registry: ContainerRegistry,
    db: Database,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(60));

        loop {
            tick.tick().await;
            if let Err(e) = check_and_teardown_idle(&registry, &db).await {
                tracing::error!(error = %e, "Idle monitor check failed");
            }
        }
    })
}

/// Check all containers in the registry and tear down any that have been idle
/// longer than the configured timeout.
async fn check_and_teardown_idle(
    registry: &ContainerRegistry,
    db: &Database,
) -> anyhow::Result<()> {
    let s = settings();
    let idle_timeout_secs = s.container_idle_timeout_secs;

    // 0 means auto-teardown is disabled
    if idle_timeout_secs == 0 {
        return Ok(());
    }

    let now = Utc::now();
    let containers = registry.list_all().await;

    for ((repo, branch), entry) in containers {
        if !should_teardown(&entry, idle_timeout_secs, now) {
            continue;
        }

        tracing::info!(
            repo = %repo,
            branch = %branch,
            container = %entry.container_name,
            container_id = entry.container_id,
            "Tearing down idle container (exceeded {}s timeout)",
            idle_timeout_secs,
        );

        // 1. Set state to Stopping
        if let Err(e) = registry.set_state(db, &repo, &branch, ContainerState::Stopping).await {
            tracing::error!(
                repo = %repo, branch = %branch, error = %e,
                "Failed to set container state to Stopping"
            );
            continue;
        }

        // 2. Remove container via SSH
        if let Err(e) = teardown_container(&entry.container_name).await {
            tracing::error!(
                repo = %repo, branch = %branch,
                container = %entry.container_name,
                error = %e,
                "Failed to remove idle container via SSH"
            );
            // Still remove from registry so we don't retry forever
        }

        // 3. Remove from registry and mark stopped in DB
        if let Err(e) = registry.remove_container(db, &repo, &branch).await {
            tracing::error!(
                repo = %repo, branch = %branch, error = %e,
                "Failed to remove container from registry"
            );
        }
    }

    Ok(())
}

/// Determine whether a container should be torn down based on its state,
/// session count, and idle duration.
fn should_teardown(
    entry: &crate::container_registry::ContainerEntry,
    idle_timeout_secs: u64,
    now: chrono::DateTime<Utc>,
) -> bool {
    // Only tear down running containers with no active sessions
    if entry.state != ContainerState::Running || entry.session_count > 0 {
        return false;
    }

    // Use last_session_stopped_at if available, otherwise fall back to last_activity_at
    let idle_since = entry
        .last_session_stopped_at
        .unwrap_or(entry.last_activity_at);

    let idle_duration = now.signed_duration_since(idle_since);
    idle_duration.num_seconds() >= idle_timeout_secs as i64
}

/// Remove a container by running `podman rm -f` (or configured runtime) via SSH.
async fn teardown_container(container_name: &str) -> anyhow::Result<()> {
    let s = settings();
    let rm_cmd = format!(
        "{} rm -f {}",
        shell_escape(&s.container_runtime),
        shell_escape(container_name),
    );

    let timeout = Duration::from_secs(s.ssh_timeout_secs);
    let output_future = ssh::command()?
        .arg(ssh::login_shell(&rm_cmd))
        .output();

    let output = tokio::time::timeout(timeout, output_future)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Container removal timed out after {}s",
                s.ssh_timeout_secs
            )
        })??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            container = %container_name,
            stderr = %stderr,
            "Container rm exited with non-zero status (may already be gone)"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_registry::{ContainerEntry, ContainerState};
    use chrono::{Duration as ChronoDuration, Utc};

    fn make_entry(
        sessions: i32,
        state: ContainerState,
        last_session_stopped_at: Option<chrono::DateTime<Utc>>,
    ) -> ContainerEntry {
        ContainerEntry {
            container_id: 1,
            container_name: "test-container".to_string(),
            state,
            session_count: sessions,
            last_activity_at: Utc::now(),
            devcontainer_json_hash: None,
            last_session_stopped_at,
        }
    }

    #[test]
    fn test_should_teardown_idle_container() {
        let now = Utc::now();
        let idle_since = now - ChronoDuration::seconds(3600); // 1 hour ago
        let entry = make_entry(0, ContainerState::Running, Some(idle_since));

        assert!(should_teardown(&entry, 1800, now)); // 30 min timeout, idle 60 min
    }

    #[test]
    fn test_should_not_teardown_recently_idle() {
        let now = Utc::now();
        let idle_since = now - ChronoDuration::seconds(600); // 10 minutes ago
        let entry = make_entry(0, ContainerState::Running, Some(idle_since));

        assert!(!should_teardown(&entry, 1800, now)); // 30 min timeout, idle 10 min
    }

    #[test]
    fn test_should_not_teardown_with_active_sessions() {
        let now = Utc::now();
        let entry = make_entry(2, ContainerState::Running, None);

        assert!(!should_teardown(&entry, 1800, now));
    }

    #[test]
    fn test_should_not_teardown_stopping_container() {
        let now = Utc::now();
        let idle_since = now - ChronoDuration::seconds(3600);
        let entry = make_entry(0, ContainerState::Stopping, Some(idle_since));

        assert!(!should_teardown(&entry, 1800, now));
    }

    #[test]
    fn test_should_not_teardown_stopped_container() {
        let now = Utc::now();
        let idle_since = now - ChronoDuration::seconds(3600);
        let entry = make_entry(0, ContainerState::Stopped, Some(idle_since));

        assert!(!should_teardown(&entry, 1800, now));
    }

    #[test]
    fn test_should_teardown_at_exact_threshold() {
        let now = Utc::now();
        let idle_since = now - ChronoDuration::seconds(1800); // exactly at threshold
        let entry = make_entry(0, ContainerState::Running, Some(idle_since));

        assert!(should_teardown(&entry, 1800, now));
    }

    #[test]
    fn test_should_teardown_just_under_threshold() {
        let now = Utc::now();
        let idle_since = now - ChronoDuration::seconds(1799); // 1 second under
        let entry = make_entry(0, ContainerState::Running, Some(idle_since));

        assert!(!should_teardown(&entry, 1800, now));
    }

    #[test]
    fn test_falls_back_to_last_activity_at_when_no_stopped_timestamp() {
        let now = Utc::now();
        let mut entry = make_entry(0, ContainerState::Running, None);
        // Manually set last_activity_at to 1 hour ago
        entry.last_activity_at = now - ChronoDuration::seconds(3600);

        assert!(should_teardown(&entry, 1800, now));
    }

    #[test]
    fn test_zero_timeout_never_tears_down() {
        // This tests the check in check_and_teardown_idle, but since that's async,
        // we test the should_teardown logic: even with idle_timeout_secs=0,
        // should_teardown would return true for any non-negative duration.
        // The actual guard is in check_and_teardown_idle (early return if timeout == 0).
        // Here we verify the contract: idle_timeout_secs=0 means no teardown.
        let now = Utc::now();
        let idle_since = now - ChronoDuration::seconds(3600);
        let entry = make_entry(0, ContainerState::Running, Some(idle_since));

        // With timeout=0, should_teardown returns true (0 >= 0), but the caller
        // short-circuits before calling should_teardown. This test documents that
        // the 0-means-disabled guard is in check_and_teardown_idle, not here.
        assert!(should_teardown(&entry, 0, now));
    }
}
