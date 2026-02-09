use anyhow::Result;
use sqlx::{postgres::PgPoolOptions, FromRow, PgPool};

use crate::config::settings;

#[derive(Clone)]
pub struct Database {
    pool: PgPool,
}

#[derive(Debug, FromRow)]
#[allow(dead_code)]
pub struct StoredSession {
    pub session_id: String,
    pub channel_id: String,
    pub thread_id: String,
    pub project: String,
    pub project_path: String,
    pub container_name: String,
    pub session_type: String,
    pub parent_session_id: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_activity_at: chrono::DateTime<chrono::Utc>,
    pub message_count: i32,
    pub compaction_count: i32,
}

#[derive(Debug, FromRow)]
#[allow(dead_code)]
pub struct StoredProjectChannel {
    pub project: String,
    pub channel_id: String,
    pub channel_name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, FromRow)]
#[allow(dead_code)]
pub struct StoredPendingRequest {
    pub request_id: String,
    pub channel_id: String,
    pub thread_id: String,
    pub session_id: String,
    pub domain: String,
    pub post_id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, FromRow)]
#[allow(dead_code)]
pub struct AuditLogEntry {
    pub id: i64,
    pub request_id: String,
    pub domain: String,
    pub action: String,
    pub approved_by: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

const SCHEMA: &str = "session_manager";

/// Create schema and all tables/indexes for the given schema name.
/// Used by both production initialization and integration tests.
pub async fn create_schema(pool: &PgPool, schema: &str) -> Result<()> {
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {}", schema))
        .execute(pool)
        .await?;

    sqlx::query(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS {}.sessions (
            session_id TEXT PRIMARY KEY,
            channel_id TEXT NOT NULL,
            thread_id TEXT NOT NULL,
            project TEXT NOT NULL,
            project_path TEXT NOT NULL DEFAULT '',
            container_name TEXT NOT NULL,
            session_type TEXT NOT NULL DEFAULT 'standard',
            parent_session_id TEXT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            last_activity_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            message_count INTEGER NOT NULL DEFAULT 0,
            compaction_count INTEGER NOT NULL DEFAULT 0
        )
        "#,
        schema
    ))
    .execute(pool)
    .await?;

    // Migration: add project_path column to existing sessions table
    sqlx::query(&format!(
        "ALTER TABLE {}.sessions ADD COLUMN IF NOT EXISTS project_path TEXT NOT NULL DEFAULT ''",
        schema
    ))
    .execute(pool)
    .await?;

    sqlx::query(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS {}.project_channels (
            project TEXT PRIMARY KEY,
            channel_id TEXT NOT NULL,
            channel_name TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
        schema
    ))
    .execute(pool)
    .await?;

    sqlx::query(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS {}.pending_requests (
            request_id TEXT PRIMARY KEY,
            channel_id TEXT NOT NULL,
            thread_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            domain TEXT NOT NULL,
            post_id TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
        schema
    ))
    .execute(pool)
    .await?;

    sqlx::query(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS {}.audit_log (
            id BIGSERIAL PRIMARY KEY,
            request_id TEXT NOT NULL,
            domain TEXT NOT NULL,
            action TEXT NOT NULL,
            approved_by TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
        schema
    ))
    .execute(pool)
    .await?;

    sqlx::query(&format!(
        "CREATE INDEX IF NOT EXISTS idx_sessions_thread ON {}.sessions(channel_id, thread_id)",
        schema
    ))
    .execute(pool)
    .await?;

    sqlx::query(&format!(
        "CREATE INDEX IF NOT EXISTS idx_sessions_parent ON {}.sessions(parent_session_id)",
        schema
    ))
    .execute(pool)
    .await?;

    sqlx::query(&format!(
        "CREATE INDEX IF NOT EXISTS idx_pending_session ON {}.pending_requests(session_id)",
        schema
    ))
    .execute(pool)
    .await?;

    sqlx::query(&format!(
        "CREATE INDEX IF NOT EXISTS idx_audit_domain ON {}.audit_log(domain)",
        schema
    ))
    .execute(pool)
    .await?;

    sqlx::query(&format!(
        "CREATE INDEX IF NOT EXISTS idx_audit_created ON {}.audit_log(created_at DESC)",
        schema
    ))
    .execute(pool)
    .await?;

    Ok(())
}

impl Database {
    pub async fn new() -> Result<Self> {
        let s = settings();

        let pool = PgPoolOptions::new()
            .max_connections(s.database_pool_size)
            .connect(&s.database_url)
            .await?;

        tracing::info!(
            pool_size = s.database_pool_size,
            "Database connection pool initialized"
        );

        create_schema(&pool, SCHEMA).await?;

        Ok(Self { pool })
    }

    // --- Session operations ---

    #[allow(clippy::too_many_arguments)]
    pub async fn create_session(
        &self,
        session_id: &str,
        channel_id: &str,
        thread_id: &str,
        project: &str,
        project_path: &str,
        container_name: &str,
        session_type: &str,
        parent_session_id: Option<&str>,
    ) -> Result<()> {
        sqlx::query(&format!(
            "INSERT INTO {}.sessions (session_id, channel_id, thread_id, project, project_path, container_name, session_type, parent_session_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            SCHEMA
        ))
        .bind(session_id)
        .bind(channel_id)
        .bind(thread_id)
        .bind(project)
        .bind(project_path)
        .bind(container_name)
        .bind(session_type)
        .bind(parent_session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Primary routing query: find session by channel + thread
    pub async fn get_session_by_thread(
        &self,
        channel_id: &str,
        thread_id: &str,
    ) -> Result<Option<StoredSession>> {
        let session = sqlx::query_as::<_, StoredSession>(&format!(
            "SELECT session_id, channel_id, thread_id, project, project_path, container_name, session_type, parent_session_id, created_at, last_activity_at, message_count, compaction_count \
             FROM {}.sessions WHERE channel_id = $1 AND thread_id = $2",
            SCHEMA
        ))
        .bind(channel_id)
        .bind(thread_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(session)
    }

    /// Find session by ID prefix (for `stop <short-id>` commands).
    /// Validates that prefix contains only UUID characters to prevent LIKE injection.
    pub async fn get_session_by_id_prefix(&self, prefix: &str) -> Result<Option<StoredSession>> {
        // Validate prefix contains only hex digits and hyphens (UUID chars)
        if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Err(anyhow::anyhow!("Invalid session ID prefix: must contain only hex digits and hyphens"));
        }

        let session = sqlx::query_as::<_, StoredSession>(&format!(
            "SELECT session_id, channel_id, thread_id, project, project_path, container_name, session_type, parent_session_id, created_at, last_activity_at, message_count, compaction_count \
             FROM {}.sessions WHERE session_id LIKE $1 LIMIT 1",
            SCHEMA
        ))
        .bind(format!("{}%", prefix))
        .fetch_optional(&self.pool)
        .await?;
        Ok(session)
    }

    /// Find non-worker sessions in a channel (standard + orchestrator).
    /// Used for top-level message routing.
    pub async fn get_non_worker_sessions_by_channel(
        &self,
        channel_id: &str,
    ) -> Result<Vec<StoredSession>> {
        let sessions = sqlx::query_as::<_, StoredSession>(&format!(
            "SELECT session_id, channel_id, thread_id, project, project_path, container_name, session_type, parent_session_id, created_at, last_activity_at, message_count, compaction_count \
             FROM {}.sessions WHERE channel_id = $1 AND session_type != 'worker'",
            SCHEMA
        ))
        .bind(channel_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(sessions)
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(&format!("DELETE FROM {}.pending_requests WHERE session_id = $1", SCHEMA))
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        sqlx::query(&format!("DELETE FROM {}.sessions WHERE session_id = $1", SCHEMA))
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    pub async fn get_all_sessions(&self) -> Result<Vec<StoredSession>> {
        let sessions = sqlx::query_as::<_, StoredSession>(&format!(
            "SELECT session_id, channel_id, thread_id, project, project_path, container_name, session_type, parent_session_id, created_at, last_activity_at, message_count, compaction_count \
             FROM {}.sessions",
            SCHEMA
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(sessions)
    }

    /// Update session activity: increment message count and update last_activity_at.
    /// Returns the updated message count.
    pub async fn touch_session(&self, session_id: &str) -> Result<i32> {
        let row: (i32,) = sqlx::query_as(&format!(
            "UPDATE {}.sessions SET last_activity_at = NOW(), message_count = message_count + 1 \
             WHERE session_id = $1 RETURNING message_count",
            SCHEMA
        ))
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Record a compaction event for a session.
    pub async fn record_compaction(&self, session_id: &str) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.sessions SET compaction_count = compaction_count + 1 WHERE session_id = $1",
            SCHEMA
        ))
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // --- Project channel operations ---

    pub async fn get_project_channel(&self, project: &str) -> Result<Option<StoredProjectChannel>> {
        let channel = sqlx::query_as::<_, StoredProjectChannel>(&format!(
            "SELECT project, channel_id, channel_name, created_at FROM {}.project_channels WHERE project = $1",
            SCHEMA
        ))
        .bind(project)
        .fetch_optional(&self.pool)
        .await?;
        Ok(channel)
    }

    pub async fn create_project_channel(
        &self,
        project: &str,
        channel_id: &str,
        channel_name: &str,
    ) -> Result<()> {
        sqlx::query(&format!(
            "INSERT INTO {}.project_channels (project, channel_id, channel_name) VALUES ($1, $2, $3) ON CONFLICT (project) DO NOTHING",
            SCHEMA
        ))
        .bind(project)
        .bind(channel_id)
        .bind(channel_name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // --- Pending request operations ---

    pub async fn create_pending_request(
        &self,
        request_id: &str,
        channel_id: &str,
        thread_id: &str,
        session_id: &str,
        domain: &str,
        post_id: &str,
    ) -> Result<()> {
        sqlx::query(&format!(
            "INSERT INTO {}.pending_requests (request_id, channel_id, thread_id, session_id, domain, post_id) VALUES ($1, $2, $3, $4, $5, $6)",
            SCHEMA
        ))
        .bind(request_id)
        .bind(channel_id)
        .bind(thread_id)
        .bind(session_id)
        .bind(domain)
        .bind(post_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_pending_request(&self, request_id: &str) -> Result<Option<StoredPendingRequest>> {
        let request = sqlx::query_as::<_, StoredPendingRequest>(&format!(
            "SELECT request_id, channel_id, thread_id, session_id, domain, post_id, created_at FROM {}.pending_requests WHERE request_id = $1",
            SCHEMA
        ))
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(request)
    }

    pub async fn delete_pending_request(&self, request_id: &str) -> Result<()> {
        sqlx::query(&format!("DELETE FROM {}.pending_requests WHERE request_id = $1", SCHEMA))
            .bind(request_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Check if there's already a pending request for a domain in a session
    pub async fn get_pending_request_by_domain_and_session(
        &self,
        domain: &str,
        session_id: &str,
    ) -> Result<Option<StoredPendingRequest>> {
        let request = sqlx::query_as::<_, StoredPendingRequest>(&format!(
            "SELECT request_id, channel_id, thread_id, session_id, domain, post_id, created_at \
             FROM {}.pending_requests WHERE domain = $1 AND session_id = $2",
            SCHEMA
        ))
        .bind(domain)
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(request)
    }

    // --- Audit log operations ---

    pub async fn log_approval(
        &self,
        request_id: &str,
        domain: &str,
        action: &str,
        approved_by: &str,
    ) -> Result<()> {
        sqlx::query(&format!(
            "INSERT INTO {}.audit_log (request_id, domain, action, approved_by) VALUES ($1, $2, $3, $4)",
            SCHEMA
        ))
        .bind(request_id)
        .bind(domain)
        .bind(action)
        .bind(approved_by)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Health check: verify the database connection is alive
    pub async fn health_check(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Clean up stale pending requests (older than specified hours)
    pub async fn cleanup_stale_requests(&self, max_age_hours: i64) -> Result<u64> {
        let result = sqlx::query(&format!(
            "DELETE FROM {}.pending_requests WHERE created_at < NOW() - INTERVAL '1 hour' * $1",
            SCHEMA
        ))
        .bind(max_age_hours)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
