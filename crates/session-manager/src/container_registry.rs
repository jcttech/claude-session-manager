use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::database::Database;

/// State of a devcontainer tracked in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerState {
    Running,
    Stopping,
    Stopped,
}

impl std::fmt::Display for ContainerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContainerState::Running => write!(f, "running"),
            ContainerState::Stopping => write!(f, "stopping"),
            ContainerState::Stopped => write!(f, "stopped"),
        }
    }
}

impl ContainerState {
    pub fn from_str(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "stopping" => Self::Stopping,
            _ => Self::Stopped,
        }
    }
}

/// In-memory representation of a tracked container.
#[derive(Debug, Clone)]
pub struct ContainerEntry {
    pub container_id: i64,
    pub container_name: String,
    pub state: ContainerState,
    pub session_count: i32,
    pub last_activity_at: DateTime<Utc>,
    pub devcontainer_json_hash: Option<String>,
}

/// Key for the container registry: (repo, branch).
/// Branch is empty string for default branch.
type RegistryKey = (String, String);

/// Thread-safe in-memory registry mapping (repo, branch) to container state.
/// Backed by database persistence via `Database` methods.
#[derive(Clone)]
pub struct ContainerRegistry {
    entries: Arc<RwLock<HashMap<RegistryKey, ContainerEntry>>>,
}

impl ContainerRegistry {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Populate the registry from database state on startup.
    /// Only loads containers in "running" state.
    pub async fn sync_from_db(&self, db: &Database) -> Result<()> {
        let containers = db.get_running_containers().await?;
        let mut entries = self.entries.write().await;
        entries.clear();
        for c in containers {
            let key = (c.repo.clone(), c.branch.clone());
            entries.insert(key, ContainerEntry {
                container_id: c.id,
                container_name: c.container_name,
                state: ContainerState::from_str(&c.state),
                session_count: c.session_count,
                last_activity_at: c.last_activity_at,
                devcontainer_json_hash: c.devcontainer_json_hash,
            });
        }
        tracing::info!(count = entries.len(), "Container registry synced from database");
        Ok(())
    }

    /// Register a new container in the registry and database.
    pub async fn register_container(
        &self,
        db: &Database,
        repo: &str,
        branch: &str,
        container_name: &str,
        devcontainer_json_hash: Option<&str>,
    ) -> Result<i64> {
        let container_id = db
            .create_container(repo, branch, container_name, devcontainer_json_hash)
            .await?;

        let now = Utc::now();
        let entry = ContainerEntry {
            container_id,
            container_name: container_name.to_string(),
            state: ContainerState::Running,
            session_count: 0,
            last_activity_at: now,
            devcontainer_json_hash: devcontainer_json_hash.map(|s| s.to_string()),
        };

        let key = (repo.to_string(), branch.to_string());
        self.entries.write().await.insert(key, entry);

        tracing::info!(
            container_id,
            repo,
            branch,
            container_name,
            "Container registered"
        );
        Ok(container_id)
    }

    /// Get a container entry by (repo, branch).
    pub async fn get_container(&self, repo: &str, branch: &str) -> Option<ContainerEntry> {
        let key = (repo.to_string(), branch.to_string());
        self.entries.read().await.get(&key).cloned()
    }

    /// Increment session count for a container. Returns the new count.
    pub async fn increment_sessions(
        &self,
        db: &Database,
        repo: &str,
        branch: &str,
    ) -> Result<i32> {
        let key = (repo.to_string(), branch.to_string());
        let mut entries = self.entries.write().await;
        let entry = entries
            .get_mut(&key)
            .ok_or_else(|| anyhow::anyhow!("Container not found for {}/{}", repo, branch))?;

        entry.session_count += 1;
        entry.last_activity_at = Utc::now();
        let new_count = entry.session_count;
        let container_id = entry.container_id;
        drop(entries);

        db.update_container_session_count(container_id, new_count)
            .await?;

        Ok(new_count)
    }

    /// Decrement session count for a container. Returns the new count.
    pub async fn decrement_sessions(
        &self,
        db: &Database,
        repo: &str,
        branch: &str,
    ) -> Result<i32> {
        let key = (repo.to_string(), branch.to_string());
        let mut entries = self.entries.write().await;
        let entry = entries
            .get_mut(&key)
            .ok_or_else(|| anyhow::anyhow!("Container not found for {}/{}", repo, branch))?;

        entry.session_count = (entry.session_count - 1).max(0);
        entry.last_activity_at = Utc::now();
        let new_count = entry.session_count;
        let container_id = entry.container_id;
        drop(entries);

        db.update_container_session_count(container_id, new_count)
            .await?;

        Ok(new_count)
    }

    /// Remove a container from the registry and mark as stopped in DB.
    pub async fn remove_container(&self, db: &Database, repo: &str, branch: &str) -> Result<()> {
        let key = (repo.to_string(), branch.to_string());
        let entry = self.entries.write().await.remove(&key);
        if let Some(entry) = entry {
            db.update_container_state(entry.container_id, "stopped")
                .await?;
            tracing::info!(
                container_id = entry.container_id,
                repo,
                branch,
                "Container removed from registry"
            );
        }
        Ok(())
    }

    /// Update container state (e.g., Running → Stopping).
    pub async fn set_state(
        &self,
        db: &Database,
        repo: &str,
        branch: &str,
        state: ContainerState,
    ) -> Result<()> {
        let key = (repo.to_string(), branch.to_string());
        let mut entries = self.entries.write().await;
        if let Some(entry) = entries.get_mut(&key) {
            let state_str = state.to_string();
            entry.state = state;
            let container_id = entry.container_id;
            drop(entries);
            db.update_container_state(container_id, &state_str).await?;
        }
        Ok(())
    }

    /// List all containers in the registry.
    pub async fn list_all(&self) -> Vec<((String, String), ContainerEntry)> {
        self.entries
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Get the number of containers tracked.
    pub async fn count(&self) -> usize {
        self.entries.read().await.len()
    }
}

impl Default for ContainerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(id: i64, name: &str, sessions: i32) -> ContainerEntry {
        ContainerEntry {
            container_id: id,
            container_name: name.to_string(),
            state: ContainerState::Running,
            session_count: sessions,
            last_activity_at: Utc::now(),
            devcontainer_json_hash: None,
        }
    }

    #[tokio::test]
    async fn test_registry_insert_and_get() {
        let registry = ContainerRegistry::new();
        let key = ("org/repo".to_string(), "main".to_string());
        let entry = make_entry(1, "claude-abc123", 0);

        registry.entries.write().await.insert(key, entry);

        let result = registry.get_container("org/repo", "main").await;
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.container_id, 1);
        assert_eq!(result.container_name, "claude-abc123");
        assert_eq!(result.session_count, 0);
        assert_eq!(result.state, ContainerState::Running);
    }

    #[tokio::test]
    async fn test_registry_get_nonexistent() {
        let registry = ContainerRegistry::new();
        let result = registry.get_container("org/repo", "main").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_registry_list_all() {
        let registry = ContainerRegistry::new();
        {
            let mut entries = registry.entries.write().await;
            entries.insert(
                ("org/repo1".to_string(), "main".to_string()),
                make_entry(1, "container-1", 2),
            );
            entries.insert(
                ("org/repo2".to_string(), "dev".to_string()),
                make_entry(2, "container-2", 1),
            );
        }

        let all = registry.list_all().await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_registry_count() {
        let registry = ContainerRegistry::new();
        assert_eq!(registry.count().await, 0);

        registry.entries.write().await.insert(
            ("org/repo".to_string(), "main".to_string()),
            make_entry(1, "container-1", 0),
        );
        assert_eq!(registry.count().await, 1);
    }

    #[tokio::test]
    async fn test_registry_remove() {
        let registry = ContainerRegistry::new();
        registry.entries.write().await.insert(
            ("org/repo".to_string(), "main".to_string()),
            make_entry(1, "container-1", 0),
        );

        assert_eq!(registry.count().await, 1);
        registry.entries.write().await.remove(&("org/repo".to_string(), "main".to_string()));
        assert_eq!(registry.count().await, 0);
    }

    #[tokio::test]
    async fn test_container_state_display() {
        assert_eq!(ContainerState::Running.to_string(), "running");
        assert_eq!(ContainerState::Stopping.to_string(), "stopping");
        assert_eq!(ContainerState::Stopped.to_string(), "stopped");
    }

    #[tokio::test]
    async fn test_container_state_from_str() {
        assert_eq!(ContainerState::from_str("running"), ContainerState::Running);
        assert_eq!(ContainerState::from_str("stopping"), ContainerState::Stopping);
        assert_eq!(ContainerState::from_str("stopped"), ContainerState::Stopped);
        assert_eq!(ContainerState::from_str("unknown"), ContainerState::Stopped);
    }

    #[tokio::test]
    async fn test_session_count_manual_increment_decrement() {
        let registry = ContainerRegistry::new();
        let key = ("org/repo".to_string(), "main".to_string());
        registry.entries.write().await.insert(key.clone(), make_entry(1, "c1", 0));

        // Increment
        {
            let mut entries = registry.entries.write().await;
            let entry = entries.get_mut(&key).unwrap();
            entry.session_count += 1;
            assert_eq!(entry.session_count, 1);
        }

        // Increment again
        {
            let mut entries = registry.entries.write().await;
            let entry = entries.get_mut(&key).unwrap();
            entry.session_count += 1;
            assert_eq!(entry.session_count, 2);
        }

        // Decrement
        {
            let mut entries = registry.entries.write().await;
            let entry = entries.get_mut(&key).unwrap();
            entry.session_count = (entry.session_count - 1).max(0);
            assert_eq!(entry.session_count, 1);
        }

        // Decrement to zero
        {
            let mut entries = registry.entries.write().await;
            let entry = entries.get_mut(&key).unwrap();
            entry.session_count = (entry.session_count - 1).max(0);
            assert_eq!(entry.session_count, 0);
        }

        // Decrement past zero should clamp to 0
        {
            let mut entries = registry.entries.write().await;
            let entry = entries.get_mut(&key).unwrap();
            entry.session_count = (entry.session_count - 1).max(0);
            assert_eq!(entry.session_count, 0);
        }
    }

    #[tokio::test]
    async fn test_default_impl() {
        let registry = ContainerRegistry::default();
        assert_eq!(registry.count().await, 0);
    }
}
