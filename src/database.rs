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
    pub project: String,
    pub container_name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, FromRow)]
#[allow(dead_code)]
pub struct StoredPendingRequest {
    pub request_id: String,
    pub channel_id: String,
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

        // Create schema if it doesn't exist
        sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {}", SCHEMA))
            .execute(&pool)
            .await?;

        // Run migrations
        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {}.sessions (
                session_id TEXT PRIMARY KEY,
                channel_id TEXT NOT NULL,
                project TEXT NOT NULL,
                container_name TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
            SCHEMA
        ))
        .execute(&pool)
        .await?;

        sqlx::query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {}.pending_requests (
                request_id TEXT PRIMARY KEY,
                channel_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                domain TEXT NOT NULL,
                post_id TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
            SCHEMA
        ))
        .execute(&pool)
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
            SCHEMA
        ))
        .execute(&pool)
        .await?;

        // Create indexes
        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_sessions_channel ON {}.sessions(channel_id)",
            SCHEMA
        ))
        .execute(&pool)
        .await?;

        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_pending_session ON {}.pending_requests(session_id)",
            SCHEMA
        ))
        .execute(&pool)
        .await?;

        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_audit_domain ON {}.audit_log(domain)",
            SCHEMA
        ))
        .execute(&pool)
        .await?;

        sqlx::query(&format!(
            "CREATE INDEX IF NOT EXISTS idx_audit_created ON {}.audit_log(created_at DESC)",
            SCHEMA
        ))
        .execute(&pool)
        .await?;

        Ok(Self { pool })
    }

    // Session operations
    pub async fn create_session(
        &self,
        session_id: &str,
        channel_id: &str,
        project: &str,
        container_name: &str,
    ) -> Result<()> {
        sqlx::query(&format!(
            "INSERT INTO {}.sessions (session_id, channel_id, project, container_name) VALUES ($1, $2, $3, $4)",
            SCHEMA
        ))
        .bind(session_id)
        .bind(channel_id)
        .bind(project)
        .bind(container_name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_session_by_channel(&self, channel_id: &str) -> Result<Option<StoredSession>> {
        let session = sqlx::query_as::<_, StoredSession>(&format!(
            "SELECT session_id, channel_id, project, container_name, created_at FROM {}.sessions WHERE channel_id = $1",
            SCHEMA
        ))
        .bind(channel_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(session)
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        sqlx::query(&format!("DELETE FROM {}.sessions WHERE session_id = $1", SCHEMA))
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        // Also clean up pending requests for this session
        sqlx::query(&format!("DELETE FROM {}.pending_requests WHERE session_id = $1", SCHEMA))
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_all_sessions(&self) -> Result<Vec<StoredSession>> {
        let sessions = sqlx::query_as::<_, StoredSession>(&format!(
            "SELECT session_id, channel_id, project, container_name, created_at FROM {}.sessions",
            SCHEMA
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(sessions)
    }

    // Pending request operations
    pub async fn create_pending_request(
        &self,
        request_id: &str,
        channel_id: &str,
        session_id: &str,
        domain: &str,
        post_id: &str,
    ) -> Result<()> {
        sqlx::query(&format!(
            "INSERT INTO {}.pending_requests (request_id, channel_id, session_id, domain, post_id) VALUES ($1, $2, $3, $4, $5)",
            SCHEMA
        ))
        .bind(request_id)
        .bind(channel_id)
        .bind(session_id)
        .bind(domain)
        .bind(post_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_pending_request(&self, request_id: &str) -> Result<Option<StoredPendingRequest>> {
        let request = sqlx::query_as::<_, StoredPendingRequest>(&format!(
            "SELECT request_id, channel_id, session_id, domain, post_id, created_at FROM {}.pending_requests WHERE request_id = $1",
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
    /// Returns the existing request if found
    pub async fn get_pending_request_by_domain_and_session(
        &self,
        domain: &str,
        session_id: &str,
    ) -> Result<Option<StoredPendingRequest>> {
        let request = sqlx::query_as::<_, StoredPendingRequest>(&format!(
            "SELECT request_id, channel_id, session_id, domain, post_id, created_at \
             FROM {}.pending_requests WHERE domain = $1 AND session_id = $2",
            SCHEMA
        ))
        .bind(domain)
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(request)
    }

    // Audit log operations
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

    #[allow(dead_code)]
    pub async fn get_audit_log(&self, limit: i64) -> Result<Vec<AuditLogEntry>> {
        let entries = sqlx::query_as::<_, AuditLogEntry>(&format!(
            "SELECT id, request_id, domain, action, approved_by, created_at FROM {}.audit_log ORDER BY created_at DESC LIMIT $1",
            SCHEMA
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(entries)
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
