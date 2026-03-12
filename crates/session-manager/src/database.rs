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
    pub container_id: Option<i64>,
    pub session_type: String,
    pub parent_session_id: Option<String>,
    pub user_id: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_activity_at: chrono::DateTime<chrono::Utc>,
    pub message_count: i32,
    pub compaction_count: i32,
    pub claude_session_id: Option<String>,
    pub status: String,
    pub team_id: Option<String>,
    pub role: Option<String>,
    pub context_tokens: i64,
    pub pending_task_from: Option<String>,
    pub current_task: Option<String>,
}

#[derive(Debug, FromRow)]
#[allow(dead_code)]
pub struct StoredTeam {
    pub team_id: String,
    pub channel_id: String,
    pub project: String,
    pub project_path: String,
    pub lead_session_id: String,
    pub dev_count: i32,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub coordination_thread_id: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
#[allow(dead_code)]
pub struct StoredTeamRole {
    pub role_name: String,
    pub display_name: String,
    pub description: String,
    pub responsibilities: String,
    pub default_worktree: bool,
    pub allow_multiple: bool,
    pub sort_order: i32,
}

#[derive(Debug, FromRow)]
#[allow(dead_code)]
pub struct StoredContainer {
    pub id: i64,
    pub repo: String,
    pub branch: String,
    pub container_name: String,
    pub state: String,
    pub session_count: i32,
    pub grpc_port: i32,
    pub last_activity_at: chrono::DateTime<chrono::Utc>,
    pub devcontainer_json_hash: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
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
    // Validate schema name to prevent SQL injection — only allow valid PG identifiers
    if !schema.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        || schema.is_empty()
        || schema.starts_with(|c: char| c.is_ascii_digit())
    {
        anyhow::bail!("Invalid schema name: {}", schema);
    }

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

    // Containers table: tracks devcontainers independently from sessions
    sqlx::query(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS {}.containers (
            id BIGSERIAL PRIMARY KEY,
            repo TEXT NOT NULL,
            branch TEXT NOT NULL DEFAULT '',
            container_name TEXT NOT NULL,
            state TEXT NOT NULL DEFAULT 'running',
            session_count INTEGER NOT NULL DEFAULT 0,
            last_activity_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            devcontainer_json_hash TEXT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE(repo, branch)
        )
        "#,
        schema
    ))
    .execute(pool)
    .await?;

    // Migration: add grpc_port column to containers table
    sqlx::query(&format!(
        "ALTER TABLE {}.containers ADD COLUMN IF NOT EXISTS grpc_port INTEGER NOT NULL DEFAULT 0",
        schema
    ))
    .execute(pool)
    .await?;

    // Migration: add container_id foreign key to sessions table
    sqlx::query(&format!(
        "ALTER TABLE {}.sessions ADD COLUMN IF NOT EXISTS container_id BIGINT",
        schema
    ))
    .execute(pool)
    .await?;

    // Migration: add user_id column to sessions table (for thread follow/unfollow)
    sqlx::query(&format!(
        "ALTER TABLE {}.sessions ADD COLUMN IF NOT EXISTS user_id TEXT",
        schema
    ))
    .execute(pool)
    .await?;

    // Migration: add claude_session_id column (for session resume after worker death)
    sqlx::query(&format!(
        "ALTER TABLE {}.sessions ADD COLUMN IF NOT EXISTS claude_session_id TEXT",
        schema
    ))
    .execute(pool)
    .await?;

    // Migration: add status column (active/stopped/disconnected)
    sqlx::query(&format!(
        "ALTER TABLE {}.sessions ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'active'",
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

    sqlx::query(&format!(
        "CREATE INDEX IF NOT EXISTS idx_containers_repo_branch ON {}.containers(repo, branch)",
        schema
    ))
    .execute(pool)
    .await?;

    sqlx::query(&format!(
        "CREATE INDEX IF NOT EXISTS idx_sessions_container_id ON {}.sessions(container_id)",
        schema
    ))
    .execute(pool)
    .await?;

    // Migration: add team_id and role columns to sessions table
    sqlx::query(&format!(
        "ALTER TABLE {}.sessions ADD COLUMN IF NOT EXISTS team_id TEXT",
        schema
    ))
    .execute(pool)
    .await?;

    sqlx::query(&format!(
        "ALTER TABLE {}.sessions ADD COLUMN IF NOT EXISTS role TEXT",
        schema
    ))
    .execute(pool)
    .await?;

    // Migration: add context_tokens column to sessions table
    sqlx::query(&format!(
        "ALTER TABLE {}.sessions ADD COLUMN IF NOT EXISTS context_tokens BIGINT NOT NULL DEFAULT 0",
        schema
    ))
    .execute(pool)
    .await?;

    // Migration: add pending_task_from and current_task columns to sessions table
    sqlx::query(&format!(
        "ALTER TABLE {}.sessions ADD COLUMN IF NOT EXISTS pending_task_from TEXT",
        schema
    ))
    .execute(pool)
    .await?;

    sqlx::query(&format!(
        "ALTER TABLE {}.sessions ADD COLUMN IF NOT EXISTS current_task TEXT",
        schema
    ))
    .execute(pool)
    .await?;

    // Teams table
    sqlx::query(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS {}.teams (
            team_id TEXT PRIMARY KEY,
            channel_id TEXT NOT NULL,
            project TEXT NOT NULL,
            project_path TEXT NOT NULL DEFAULT '',
            lead_session_id TEXT NOT NULL,
            dev_count INTEGER NOT NULL DEFAULT 1,
            status TEXT NOT NULL DEFAULT 'active',
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
        schema
    ))
    .execute(pool)
    .await?;

    // Migration: add coordination_thread_id column to teams table
    sqlx::query(&format!(
        "ALTER TABLE {}.teams ADD COLUMN IF NOT EXISTS coordination_thread_id TEXT",
        schema
    ))
    .execute(pool)
    .await?;

    // Team roles table (reference data)
    sqlx::query(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS {}.team_roles (
            role_name TEXT PRIMARY KEY,
            display_name TEXT NOT NULL,
            description TEXT NOT NULL,
            responsibilities TEXT NOT NULL,
            default_worktree BOOLEAN NOT NULL DEFAULT FALSE,
            allow_multiple BOOLEAN NOT NULL DEFAULT FALSE,
            sort_order INTEGER NOT NULL DEFAULT 0
        )
        "#,
        schema
    ))
    .execute(pool)
    .await?;

    // Seed team_roles with default data
    sqlx::query(&format!(
        r#"
        INSERT INTO {schema}.team_roles (role_name, display_name, description, responsibilities, default_worktree, allow_multiple, sort_order)
        VALUES
            ('team_lead', 'Team Lead', 'Coordinates the team, reviews PRs, and synthesizes results',
             '- Interpret user requests and determine the right course of action — spawn an Architect for spec work, Developers for implementation, or both
- Decide which roles to spawn based on the task''s requirements; prefer fewer roles when the task is simple
- Use `/plan` to generate implementation stories from approved specs and delegate stories to Developers
- Use `/analyze` to verify consistency and quality across specs and stories — run after planning to ensure stories align with the spec, and re-run after changes
- After assigning a task via [TO:Role] or [SPAWN:Role], WAIT for that member to respond before sending another task
- The system enforces this — sending to a member with a pending task will be rejected
- Use [TEAM_STATUS] to check member status and current task assignments before sending follow-up tasks
- Do NOT assign the same task to multiple developers unless you explicitly want parallel approaches
- Monitor progress by checking `[TEAM_STATUS]` periodically and following up on stalled members
- Resolve conflicts when team members disagree or produce incompatible work
- Review and manage PRs as described in the PR Review & Completion section below — this includes performing QA review yourself for straightforward PRs
- Only spawn a QA Engineer when the task has significant complexity warranting dedicated testing
- When PRs have review comments or requested changes, delegate the fixes back to the relevant Developer or Architect
- Autonomously progress open spec and implementation PRs — review, request changes, or merge as appropriate
- Synthesize the team''s work into a final summary for the user when the task is complete
- Wind down the team when all work is done — don''t leave idle members running',
             false, false, 0),
            ('pm_architect', 'PM/Architect', 'Designs architecture and manages requirements',
             '- Use `/blueprint` to define project architecture, roadmap, and milestones
- Use `/architecture` to maintain the project''s architecture documentation
- Use `/specify` to create spec issues from requirements — iterate with `/clarify` to resolve ambiguities before approving
- Identify technical risks (performance bottlenecks, security concerns, compatibility issues) and propose mitigations
- Define coding conventions, patterns, and directory structure for the team to follow
- Use `/decision` to record important Architecture Decision Records (ADRs) with justification
- Review Developer PRs for architectural consistency when delegated by Team Lead — flag deviations from the agreed design
- Run `/architecture` to update project docs after implementation PRs are merged
- Verify implementations match the spec before approving PRs',
             true, false, 1),
            ('developer', 'Developer', 'Implements features end-to-end',
             '- Wait for task assignment from the Team Lead before starting work
- Use `/implement` to pull assigned Stories from GitHub, create a worktree, implement the tasks, and create a PR when complete
- Write clean, idiomatic code that follows the project''s existing conventions and patterns
- Write unit tests for all new code; aim for meaningful coverage of edge cases, not just happy paths
- Fix bugs by first reproducing the issue, identifying the root cause, then implementing and testing the fix
- Communicate blockers to the Team Lead immediately — don''t spin on problems silently
- After creating a PR, notify Team Lead: [TO:Team Lead] PR #N ready for review
- Address PR review feedback promptly when delegated by the Team Lead
- Do NOT start new work until your current PR is merged or you are reassigned
- One task at a time — finish current work before accepting new assignments',
             true, true, 2),
            ('qa_engineer', 'QA Engineer', 'Tests and validates quality',
             '- Use `/checklist` to generate and validate requirements quality checklists against spec issues
- Use `/bug` to file defects as GitHub Bug issues with clear reproduction steps, expected vs actual behavior, and severity
- Write integration and end-to-end tests that validate the feature works as specified
- Review Developer and Architect PRs for bugs, edge cases, error handling gaps, and security vulnerabilities
- Validate that implementations match the original requirements — flag any deviations
- Verify that fixes actually resolve reported defects and don''t introduce regressions',
             false, false, 3),
            ('devops_engineer', 'DevOps Engineer', 'Manages infrastructure and CI/CD',
             '- Configure CI/CD pipelines for automated building, testing, and deployment
- Write infrastructure as code (Terraform, Docker, Kubernetes manifests) following best practices
- Set up environment configurations for development, staging, and production
- Ensure security best practices: secrets management, least-privilege access, dependency scanning
- Optimize build and deployment times — identify and fix bottlenecks in the pipeline
- Document deployment procedures and runbooks for the team',
             true, false, 4)
        ON CONFLICT (role_name) DO NOTHING
        "#,
    ))
    .execute(pool)
    .await?;

    sqlx::query(&format!(
        "CREATE INDEX IF NOT EXISTS idx_sessions_team_id ON {}.sessions(team_id)",
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
        user_id: Option<&str>,
    ) -> Result<()> {
        sqlx::query(&format!(
            "INSERT INTO {}.sessions (session_id, channel_id, thread_id, project, project_path, container_name, session_type, parent_session_id, user_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
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
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Primary routing query: find session by channel + thread.
    /// Only returns active or disconnected sessions (not stopped).
    pub async fn get_session_by_thread(
        &self,
        channel_id: &str,
        thread_id: &str,
    ) -> Result<Option<StoredSession>> {
        let session = sqlx::query_as::<_, StoredSession>(&format!(
            "SELECT session_id, channel_id, thread_id, project, project_path, container_name, container_id, session_type, parent_session_id, user_id, created_at, last_activity_at, message_count, compaction_count, claude_session_id, status, team_id, role, context_tokens, pending_task_from, current_task \
             FROM {}.sessions WHERE channel_id = $1 AND thread_id = $2 AND status IN ('active', 'disconnected')",
            SCHEMA
        ))
        .bind(channel_id)
        .bind(thread_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(session)
    }

    /// Find a stopped session by thread (for `resume` command).
    pub async fn get_stopped_session_by_thread(
        &self,
        channel_id: &str,
        thread_id: &str,
    ) -> Result<Option<StoredSession>> {
        let session = sqlx::query_as::<_, StoredSession>(&format!(
            "SELECT session_id, channel_id, thread_id, project, project_path, container_name, container_id, session_type, parent_session_id, user_id, created_at, last_activity_at, message_count, compaction_count, claude_session_id, status, team_id, role, context_tokens, pending_task_from, current_task \
             FROM {}.sessions WHERE channel_id = $1 AND thread_id = $2 AND status = 'stopped'",
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
            "SELECT session_id, channel_id, thread_id, project, project_path, container_name, container_id, session_type, parent_session_id, user_id, created_at, last_activity_at, message_count, compaction_count, claude_session_id, status, team_id, role, context_tokens, pending_task_from, current_task \
             FROM {}.sessions WHERE session_id LIKE $1 LIMIT 1",
            SCHEMA
        ))
        .bind(format!("{}%", prefix))
        .fetch_optional(&self.pool)
        .await?;
        Ok(session)
    }

    /// Find non-worker sessions in a channel.
    /// Used for top-level message routing.
    pub async fn get_non_worker_sessions_by_channel(
        &self,
        channel_id: &str,
    ) -> Result<Vec<StoredSession>> {
        let sessions = sqlx::query_as::<_, StoredSession>(&format!(
            "SELECT session_id, channel_id, thread_id, project, project_path, container_name, container_id, session_type, parent_session_id, user_id, created_at, last_activity_at, message_count, compaction_count, claude_session_id, status, team_id, role, context_tokens, pending_task_from, current_task \
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
            "SELECT session_id, channel_id, thread_id, project, project_path, container_name, container_id, session_type, parent_session_id, user_id, created_at, last_activity_at, message_count, compaction_count, claude_session_id, status, team_id, role, context_tokens, pending_task_from, current_task \
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

    /// Store the Claude SDK session ID (for resume after worker death).
    pub async fn update_claude_session_id(
        &self,
        session_id: &str,
        claude_session_id: &str,
    ) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.sessions SET claude_session_id = $1 WHERE session_id = $2",
            SCHEMA
        ))
        .bind(claude_session_id)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update session status (active/stopped/disconnected).
    pub async fn update_session_status(
        &self,
        session_id: &str,
        status: &str,
    ) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.sessions SET status = $1 WHERE session_id = $2",
            SCHEMA
        ))
        .bind(status)
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

    // --- Container operations ---

    /// Create a new container record. Returns the generated ID.
    pub async fn create_container(
        &self,
        repo: &str,
        branch: &str,
        container_name: &str,
        devcontainer_json_hash: Option<&str>,
        grpc_port: u16,
    ) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(&format!(
            "INSERT INTO {}.containers (repo, branch, container_name, devcontainer_json_hash, grpc_port) \
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
            SCHEMA
        ))
        .bind(repo)
        .bind(branch)
        .bind(container_name)
        .bind(devcontainer_json_hash)
        .bind(grpc_port as i32)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Get a container by repo and branch.
    pub async fn get_container_by_repo(
        &self,
        repo: &str,
        branch: &str,
    ) -> Result<Option<StoredContainer>> {
        let container = sqlx::query_as::<_, StoredContainer>(&format!(
            "SELECT id, repo, branch, container_name, state, session_count, grpc_port, last_activity_at, devcontainer_json_hash, created_at \
             FROM {}.containers WHERE repo = $1 AND branch = $2",
            SCHEMA
        ))
        .bind(repo)
        .bind(branch)
        .fetch_optional(&self.pool)
        .await?;
        Ok(container)
    }

    /// Get all containers in "running" state (for registry sync on startup).
    pub async fn get_running_containers(&self) -> Result<Vec<StoredContainer>> {
        let containers = sqlx::query_as::<_, StoredContainer>(&format!(
            "SELECT id, repo, branch, container_name, state, session_count, grpc_port, last_activity_at, devcontainer_json_hash, created_at \
             FROM {}.containers WHERE state = 'running'",
            SCHEMA
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(containers)
    }

    /// Update the session count for a container.
    pub async fn update_container_session_count(
        &self,
        container_id: i64,
        session_count: i32,
    ) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.containers SET session_count = $1, last_activity_at = NOW() WHERE id = $2",
            SCHEMA
        ))
        .bind(session_count)
        .bind(container_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update container state (running, stopping, stopped).
    pub async fn update_container_state(
        &self,
        container_id: i64,
        state: &str,
    ) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.containers SET state = $1, last_activity_at = NOW() WHERE id = $2",
            SCHEMA
        ))
        .bind(state)
        .bind(container_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get all containers (for status display).
    pub async fn get_all_containers(&self) -> Result<Vec<StoredContainer>> {
        let containers = sqlx::query_as::<_, StoredContainer>(&format!(
            "SELECT id, repo, branch, container_name, state, session_count, grpc_port, last_activity_at, devcontainer_json_hash, created_at \
             FROM {}.containers",
            SCHEMA
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(containers)
    }

    /// Delete a container record.
    pub async fn delete_container(&self, container_id: i64) -> Result<()> {
        sqlx::query(&format!(
            "DELETE FROM {}.containers WHERE id = $1",
            SCHEMA
        ))
        .bind(container_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get sessions associated with a specific container.
    pub async fn get_sessions_by_container_id(
        &self,
        container_id: i64,
    ) -> Result<Vec<StoredSession>> {
        let sessions = sqlx::query_as::<_, StoredSession>(&format!(
            "SELECT session_id, channel_id, thread_id, project, project_path, container_name, container_id, session_type, parent_session_id, user_id, created_at, last_activity_at, message_count, compaction_count, claude_session_id, status, team_id, role, context_tokens, pending_task_from, current_task \
             FROM {}.sessions WHERE container_id = $1",
            SCHEMA
        ))
        .bind(container_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(sessions)
    }

    /// Link a session to a container.
    pub async fn set_session_container_id(
        &self,
        session_id: &str,
        container_id: i64,
    ) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.sessions SET container_id = $1 WHERE session_id = $2",
            SCHEMA
        ))
        .bind(container_id)
        .bind(session_id)
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

    // --- Team operations ---

    /// Create a new team record.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_team(
        &self,
        team_id: &str,
        channel_id: &str,
        project: &str,
        project_path: &str,
        lead_session_id: &str,
        dev_count: i32,
    ) -> Result<()> {
        sqlx::query(&format!(
            "INSERT INTO {}.teams (team_id, channel_id, project, project_path, lead_session_id, dev_count) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            SCHEMA
        ))
        .bind(team_id)
        .bind(channel_id)
        .bind(project)
        .bind(project_path)
        .bind(lead_session_id)
        .bind(dev_count)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get a team by ID.
    pub async fn get_team(&self, team_id: &str) -> Result<Option<StoredTeam>> {
        let team = sqlx::query_as::<_, StoredTeam>(&format!(
            "SELECT team_id, channel_id, project, project_path, lead_session_id, dev_count, status, created_at, coordination_thread_id \
             FROM {}.teams WHERE team_id = $1",
            SCHEMA
        ))
        .bind(team_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(team)
    }

    /// Get all team members (sessions) for a team.
    pub async fn get_team_members(&self, team_id: &str) -> Result<Vec<StoredSession>> {
        let sessions = sqlx::query_as::<_, StoredSession>(&format!(
            "SELECT session_id, channel_id, thread_id, project, project_path, container_name, container_id, session_type, parent_session_id, user_id, created_at, last_activity_at, message_count, compaction_count, claude_session_id, status, team_id, role, context_tokens, pending_task_from, current_task \
             FROM {}.sessions WHERE team_id = $1 AND status IN ('active', 'disconnected')",
            SCHEMA
        ))
        .bind(team_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(sessions)
    }

    /// Update team status (active/completed).
    pub async fn update_team_status(&self, team_id: &str, status: &str) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.teams SET status = $1 WHERE team_id = $2",
            SCHEMA
        ))
        .bind(status)
        .bind(team_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Link a session to a team with a role.
    pub async fn set_session_team(
        &self,
        session_id: &str,
        team_id: &str,
        role: &str,
    ) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.sessions SET team_id = $1, role = $2 WHERE session_id = $3",
            SCHEMA
        ))
        .bind(team_id)
        .bind(role)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Find team members by role name. Matches exact role or role prefix
    /// (e.g., "Developer" matches "Developer 1", "Developer 2").
    pub async fn get_sessions_by_role(
        &self,
        team_id: &str,
        role: &str,
    ) -> Result<Vec<StoredSession>> {
        let sessions = sqlx::query_as::<_, StoredSession>(&format!(
            "SELECT session_id, channel_id, thread_id, project, project_path, container_name, container_id, session_type, parent_session_id, user_id, created_at, last_activity_at, message_count, compaction_count, claude_session_id, status, team_id, role, context_tokens, pending_task_from, current_task \
             FROM {}.sessions WHERE team_id = $1 AND (role = $2 OR role LIKE $3) AND status IN ('active', 'disconnected')",
            SCHEMA
        ))
        .bind(team_id)
        .bind(role)
        .bind(format!("{} %", role.replace('%', "\\%").replace('_', "\\_")))
        .fetch_all(&self.pool)
        .await?;
        Ok(sessions)
    }

    /// Update context tokens for a session.
    pub async fn update_context_tokens(
        &self,
        session_id: &str,
        tokens: u64,
    ) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.sessions SET context_tokens = $1 WHERE session_id = $2",
            SCHEMA
        ))
        .bind(i64::try_from(tokens).unwrap_or(i64::MAX))
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set pending task state on a session (hard gate for duplicate task prevention).
    pub async fn set_pending_task(
        &self,
        target_session_id: &str,
        sender_session_id: &str,
        task_summary: &str,
    ) -> Result<()> {
        // Truncate task summary to 500 chars
        let truncated = if task_summary.len() > 500 {
            let end = task_summary.char_indices()
                .map(|(i, _)| i)
                .take_while(|&i| i <= 500)
                .last()
                .unwrap_or(0);
            format!("{}…", &task_summary[..end])
        } else {
            task_summary.to_string()
        };
        sqlx::query(&format!(
            "UPDATE {}.sessions SET pending_task_from = $1, current_task = $2 WHERE session_id = $3",
            SCHEMA
        ))
        .bind(sender_session_id)
        .bind(&truncated)
        .bind(target_session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Clear pending task state (called when member responds).
    /// Only clears pending_task_from; current_task persists until next assignment.
    pub async fn clear_pending_task(&self, session_id: &str) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.sessions SET pending_task_from = NULL WHERE session_id = $1",
            SCHEMA
        ))
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update the coordination thread ID for a team.
    pub async fn update_team_coordination_thread(
        &self,
        team_id: &str,
        thread_id: &str,
    ) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.teams SET coordination_thread_id = $1 WHERE team_id = $2",
            SCHEMA
        ))
        .bind(thread_id)
        .bind(team_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get all active teams.
    pub async fn get_active_teams(&self) -> Result<Vec<StoredTeam>> {
        let teams = sqlx::query_as::<_, StoredTeam>(&format!(
            "SELECT team_id, channel_id, project, project_path, lead_session_id, dev_count, status, created_at, coordination_thread_id \
             FROM {}.teams WHERE status = 'active'",
            SCHEMA
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(teams)
    }

    // --- Team role operations ---

    /// Get all team roles (ordered by sort_order).
    pub async fn get_all_team_roles(&self) -> Result<Vec<StoredTeamRole>> {
        let roles = sqlx::query_as::<_, StoredTeamRole>(&format!(
            "SELECT role_name, display_name, description, responsibilities, default_worktree, allow_multiple, sort_order \
             FROM {}.team_roles ORDER BY sort_order",
            SCHEMA
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(roles)
    }

    /// Get a specific team role by name.
    pub async fn get_team_role(&self, role_name: &str) -> Result<Option<StoredTeamRole>> {
        let role = sqlx::query_as::<_, StoredTeamRole>(&format!(
            "SELECT role_name, display_name, description, responsibilities, default_worktree, allow_multiple, sort_order \
             FROM {}.team_roles WHERE role_name = $1",
            SCHEMA
        ))
        .bind(role_name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(role)
    }
}
