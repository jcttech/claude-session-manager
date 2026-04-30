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
    pub automation: bool,
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

/// Outcome of `enqueue_team_clear`. Cooldown / AlreadyQueued are not errors —
/// they're normal flow-control outcomes the caller surfaces back to the Lead
/// via [ACK:CLEAR ... status=rejected reason=cooldown] / [...status=queued].
#[derive(Debug, Clone)]
pub enum EnqueueClearResult {
    Enqueued(i64),
    Cooldown { remaining_secs: i64 },
    AlreadyQueued(i64),
}

#[derive(Debug, Clone, FromRow)]
#[allow(dead_code)]
pub struct StoredTeamTask {
    pub task_id: i64,
    pub team_id: String,
    pub target_role: String,
    pub target_session_id: Option<String>,
    pub is_prefix_match: bool,
    pub sender_session_id: String,
    pub sender_role: String,
    pub message: String,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub cancelled_at: Option<chrono::DateTime<chrono::Utc>>,
    pub failure_reason: Option<String>,
    pub deliver_after: chrono::DateTime<chrono::Utc>,
    /// 'message' (default), 'spawn', or 'clear'. Distinguishes the three
    /// drain handlers: deliver-to-idle-target, claim-and-spawn-new-member,
    /// and reset-target-context. Added in v2.5.4 to consolidate three formerly
    /// inline control paths into one durable queue.
    pub task_type: String,
    /// For task_type='spawn', JSON payload with extra args (project string,
    /// initial task, etc.). For other task types, NULL.
    pub spawn_payload: Option<serde_json::Value>,
}

/// Snapshot of the per-team rate-limit pause. Only one row per team_id at a
/// time; updated in-place on every transition (allowed → allowed_warning →
/// rejected → allowed).
#[derive(Debug, Clone, FromRow)]
#[allow(dead_code)]
pub struct TeamRateLimit {
    pub team_id: String,
    pub status: String,
    pub resets_at: Option<chrono::DateTime<chrono::Utc>>,
    pub limit_type: String,
    pub utilization: Option<f64>,
    pub raw_json: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Mattermost post we keep editing in place so the team thread doesn't
    /// fill with duplicate "rate limit hit" cards on every retry.
    pub notice_post_id: Option<String>,
}

/// Active Claude OAuth profile override. Singleton row keyed by `id = 1`.
/// `claude_config_dir == ""` means no override (CLI falls back to ~/.claude
/// or inherited CLAUDE_CODE_OAUTH_TOKEN). Mirrors membank's shape so the
/// external profile-manager can POST identical payloads to either service.
#[derive(Debug, Clone, FromRow)]
#[allow(dead_code)]
pub struct ClaudeProfileState {
    pub claude_config_dir: String,
    pub event_id: Option<uuid::Uuid>,
    pub swapped_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub updated_by: Option<String>,
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

/// Truncate a task summary to 500 chars on a UTF-8 boundary, with an ellipsis.
fn truncate_task_summary(task: &str) -> String {
    if task.len() <= 500 {
        return task.to_string();
    }
    let end = task
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= 500)
        .last()
        .unwrap_or(0);
    format!("{}…", &task[..end])
}

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

    // Migration: add automation column to teams table
    sqlx::query(&format!(
        "ALTER TABLE {}.teams ADD COLUMN IF NOT EXISTS automation BOOLEAN NOT NULL DEFAULT FALSE",
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

    // Migrate pm_architect → architect
    sqlx::query(&format!(
        "UPDATE {schema}.sessions SET role = 'Architect' WHERE role = 'PM/Architect'"
    ))
    .execute(pool)
    .await?;
    sqlx::query(&format!(
        "DELETE FROM {schema}.team_roles WHERE role_name = 'pm_architect'"
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
- Use the spec-flow `plan` tool to generate implementation stories from approved specs and delegate stories to Developers
- Use the spec-flow `analyze` tool to verify consistency and quality across specs and stories — run after planning to ensure stories align with the spec, and re-run after changes
- After assigning a task via [TO:Role] or [SPAWN:Role], WAIT for that member to respond before sending another task
- The system enforces this — sending to a member with a pending task will be rejected
- Use [TEAM_STATUS] to check member status and current task assignments before sending follow-up tasks
- Do NOT assign the same task to multiple developers unless you explicitly want parallel approaches
- Monitor progress by checking `[TEAM_STATUS]` periodically and following up on stalled members
- Resolve conflicts when team members disagree or produce incompatible work
- Review and manage PRs as described in the PR Review & Completion section below — this includes performing QA review yourself for straightforward PRs
- Only spawn a QA Engineer when the task has significant complexity warranting dedicated testing
- PRs arrive via the review gate: team members address Claude Reviewer comments before you see the PR — verify they did so with actual code changes, not documentation-only fixes
- When PRs have review comments or requested changes, delegate the fixes back to the relevant Developer or Architect
- Autonomously progress open spec and implementation PRs — review, request changes, or merge as appropriate
- Synthesize the team''s work into a final summary for the user when the task is complete
- Wind down the team when all work is done — don''t leave idle members running',
             false, false, 0),
            ('architect', 'Architect', 'Designs architecture and manages requirements',
             '- Use the spec-flow `blueprint` tool to define project architecture, roadmap, and milestones
- Use the spec-flow `architecture` tool to maintain the project''s architecture documentation
- Use the spec-flow `specify` tool to create spec issues from requirements — iterate with the `clarify` tool to resolve ambiguities before approving
- Identify technical risks (performance bottlenecks, security concerns, compatibility issues) and propose mitigations
- Define coding conventions, patterns, and directory structure for the team to follow
- Use the spec-flow `decision` tool to record important Architecture Decision Records (ADRs) with justification
- Review Developer PRs for architectural consistency when delegated by Team Lead — flag deviations from the agreed design
- Run the spec-flow `architecture` tool to update project docs after implementation PRs are merged
- Verify implementations match the spec before approving PRs
- **PR Review Escalation**: Developers will escalate Complex review comments to you during the PR review gate. When you receive a proposed fix from a Developer:
  1. Evaluate whether the proposed approach maintains architectural alignment (interfaces, patterns, security, dependencies)
  2. Check the proposal against the project''s architecture docs and ADRs
  3. Either approve the approach, suggest a better alternative, or reject with explanation
  4. Respond promptly via [TO:Developer N] — the Developer is blocked until you respond
  5. If the fix requires an ADR, record it with the `decision` tool before approving',
             true, false, 1),
            ('developer', 'Developer', 'Implements features end-to-end',
             '- Wait for task assignment from the Team Lead before starting work
- Use the spec-flow `implement` tool to pull assigned Stories from GitHub, create a worktree, implement the tasks, and create a PR when complete
- Write clean, idiomatic code that follows the project''s existing conventions and patterns
- Write unit tests for all new code; aim for meaningful coverage of edge cases, not just happy paths
- Fix bugs by first reproducing the issue, identifying the root cause, then implementing and testing the fix
- Communicate blockers to the Team Lead immediately — don''t spin on problems silently
- After creating a PR, use [PR_READY:N] to trigger the review gate — do NOT send directly to Team Lead
- Categorise each review comment as Simple (fix yourself) or Complex (escalate to Architect):
  Simple: code style, naming, missing error handling, missing tests, minor localised bugs, dead code
  Complex: interface/API changes, new dependencies, module restructuring, security/auth patterns, design pattern deviations, schema changes
- Fix Simple comments with actual code changes — documentation-only fixes are NOT acceptable
- Escalate Complex comments to the Architect via [TO:Architect] with your proposed solution, the affected code, and the risk — wait for their feedback before implementing
- Address PR review feedback promptly when delegated by the Team Lead
- Do NOT start new work until your current PR is merged or you are reassigned
- One task at a time — finish current work before accepting new assignments',
             true, true, 2),
            ('qa_engineer', 'QA Engineer', 'Tests and validates quality',
             '- Use the spec-flow `checklist` tool to generate and validate requirements quality checklists against spec issues
- Use the spec-flow `bug` tool to file defects as GitHub Bug issues with clear reproduction steps, expected vs actual behavior, and severity
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

    // Atomic-claim table: prevents two concurrent spawns from both winning the
    // same (team_id, role) slot. handle_team_spawn inserts here BEFORE doing any
    // expensive container work; ON CONFLICT means the loser bails immediately.
    sqlx::query(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS {}.team_role_claims (
            team_id TEXT NOT NULL,
            role TEXT NOT NULL,
            session_id TEXT NOT NULL,
            claimed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            PRIMARY KEY (team_id, role)
        )
        "#,
        schema
    ))
    .execute(pool)
    .await?;

    // Defense-in-depth: even if claim/release ever desync from sessions, the DB
    // physically refuses two live sessions sharing (team_id, role).
    sqlx::query(&format!(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_team_role_active \
         ON {}.sessions(team_id, role) \
         WHERE team_id IS NOT NULL AND role IS NOT NULL \
           AND status IN ('active','disconnected','stuck')",
        schema
    ))
    .execute(pool)
    .await?;

    // Persistent task queue. Replaces the previous fire-immediately path so the
    // session-manager — not claude-code's stdin buffer — owns delivery timing.
    // The Lead can amend/cancel queued tasks before they hit the member; once a
    // row flips to status='running', the only escape is [INTERRUPT:Role].
    // Lifecycle: queued → running → completed (success) | cancelled (interrupt
    // / cancel_queued) | failed (delivery error). Rate-limit retry flips
    // running → queued, preserving created_at order.
    sqlx::query(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS {}.team_task_queue (
            task_id           BIGSERIAL PRIMARY KEY,
            team_id           TEXT NOT NULL,
            target_role       TEXT NOT NULL,
            target_session_id TEXT,
            is_prefix_match   BOOLEAN NOT NULL,
            sender_session_id TEXT NOT NULL,
            sender_role       TEXT NOT NULL,
            message           TEXT NOT NULL,
            status            TEXT NOT NULL DEFAULT 'queued',
            created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            delivered_at      TIMESTAMPTZ,
            cancelled_at      TIMESTAMPTZ,
            failure_reason    TEXT,
            deliver_after     TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
        schema
    ))
    .execute(pool)
    .await?;

    // Upgrade path for pre-existing tables created without deliver_after.
    sqlx::query(&format!(
        "ALTER TABLE {}.team_task_queue \
         ADD COLUMN IF NOT EXISTS deliver_after TIMESTAMPTZ NOT NULL DEFAULT NOW()",
        schema
    ))
    .execute(pool)
    .await?;

    // task_type discriminates message / spawn / clear rows. Default 'message'
    // preserves existing rows. Added in v2.5.4.
    sqlx::query(&format!(
        "ALTER TABLE {}.team_task_queue \
         ADD COLUMN IF NOT EXISTS task_type TEXT NOT NULL DEFAULT 'message'",
        schema
    ))
    .execute(pool)
    .await?;

    // spawn_payload carries spawn-specific args (project string, etc.) for
    // task_type='spawn' rows. NULL for message/clear.
    sqlx::query(&format!(
        "ALTER TABLE {}.team_task_queue ADD COLUMN IF NOT EXISTS spawn_payload JSONB",
        schema
    ))
    .execute(pool)
    .await?;

    // Status rename: pre-state-machine refactor, terminal success was
    // 'delivered' (overloaded with "dispatched"). Now 'running' covers the
    // dispatched-and-in-flight phase, and 'completed' is the terminal success
    // state. This idempotently migrates existing rows.
    sqlx::query(&format!(
        "UPDATE {}.team_task_queue SET status = 'completed' WHERE status = 'delivered'",
        schema
    ))
    .execute(pool)
    .await?;

    // Drain hot path: oldest queued row per team+role, partial index keeps it tight.
    sqlx::query(&format!(
        "CREATE INDEX IF NOT EXISTS idx_team_task_queue_drain \
         ON {}.team_task_queue (team_id, target_role, created_at) \
         WHERE status = 'queued'",
        schema
    ))
    .execute(pool)
    .await?;

    // Lookup by sender for [CANCEL_QUEUED:Role] / TEAM_STATUS visibility.
    sqlx::query(&format!(
        "CREATE INDEX IF NOT EXISTS idx_team_task_queue_sender \
         ON {}.team_task_queue (team_id, sender_session_id, status)",
        schema
    ))
    .execute(pool)
    .await?;

    // Spawn dedupe lookup: enqueue_team_spawn checks for an existing spawn row
    // with the same team+role+md5(message) within a 5-minute window to
    // suppress LLM re-emissions of the same intent.
    sqlx::query(&format!(
        "CREATE INDEX IF NOT EXISTS idx_team_task_queue_spawn_dedupe \
         ON {}.team_task_queue (team_id, target_role, created_at) \
         WHERE task_type = 'spawn'",
        schema
    ))
    .execute(pool)
    .await?;

    // Clear cooldown lookup: enqueue_team_clear computes MAX(delivered_at)
    // per target_session_id to enforce the AUTO_CLEAR_COOLDOWN_SECS window.
    sqlx::query(&format!(
        "CREATE INDEX IF NOT EXISTS idx_team_task_queue_clear_history \
         ON {}.team_task_queue (target_session_id, delivered_at DESC) \
         WHERE task_type = 'clear'",
        schema
    ))
    .execute(pool)
    .await?;

    // Team-level rate-limit pause. One row per team while the CLI is in
    // `rejected` (or `allowed_warning`) state; the row is deleted (or status
    // flipped to `allowed`) when the limit window resets. drain_team_queue
    // skips work whenever a row exists with status='rejected' AND
    // resets_at > NOW().
    sqlx::query(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS {}.team_rate_limits (
            team_id     TEXT PRIMARY KEY,
            status      TEXT NOT NULL,
            resets_at   TIMESTAMPTZ,
            limit_type  TEXT NOT NULL DEFAULT '',
            utilization DOUBLE PRECISION,
            raw_json    TEXT NOT NULL DEFAULT '',
            updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            notice_post_id TEXT
        )
        "#,
        schema
    ))
    .execute(pool)
    .await?;

    // Cheap scan to find pauses whose window has expired.
    sqlx::query(&format!(
        "CREATE INDEX IF NOT EXISTS idx_team_rate_limits_resets_at \
         ON {}.team_rate_limits (resets_at) \
         WHERE status = 'rejected'",
        schema
    ))
    .execute(pool)
    .await?;

    // Active Claude OAuth profile (CLAUDE_CONFIG_DIR override). Singleton row
    // updated by the external profile-manager via POST /admin/profile. Mirrors
    // membank's claude_profile_state shape so the same payload works on both.
    // Empty string = no override (CLI falls back to ~/.claude or inherited
    // CLAUDE_CODE_OAUTH_TOKEN).
    sqlx::query(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS {}.claude_profile_state (
            id                INT PRIMARY KEY CHECK (id = 1),
            claude_config_dir TEXT NOT NULL DEFAULT '',
            event_id          UUID,
            swapped_at        TIMESTAMPTZ,
            updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_by        TEXT
        )
        "#,
        schema
    ))
    .execute(pool)
    .await?;

    // Seed the singleton so reads at startup never miss.
    sqlx::query(&format!(
        "INSERT INTO {}.claude_profile_state (id) VALUES (1) ON CONFLICT (id) DO NOTHING",
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
             FROM {}.sessions WHERE channel_id = $1 AND thread_id = $2 AND status IN ('active', 'disconnected', 'stuck')",
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

        // Also drop any team role claim tied to this session so the slot can
        // be re-spawned even if cleanup_session didn't release it explicitly.
        sqlx::query(&format!("DELETE FROM {}.team_role_claims WHERE session_id = $1", SCHEMA))
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

    /// Update the container name stored on a session (used when a cold-start recovery
    /// creates a new container for an existing session).
    pub async fn update_session_container_name(
        &self,
        session_id: &str,
        container_name: &str,
    ) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.sessions SET container_name = $1 WHERE session_id = $2",
            SCHEMA
        ))
        .bind(container_name)
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

    /// Create or update a container record. Returns the ID.
    /// Uses upsert so that a stale record (e.g. left as "stopped" after a
    /// restart) is refreshed in-place rather than causing a duplicate key error.
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
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (repo, branch) DO UPDATE SET \
               container_name = EXCLUDED.container_name, \
               devcontainer_json_hash = EXCLUDED.devcontainer_json_hash, \
               grpc_port = EXCLUDED.grpc_port, \
               state = 'running', \
               session_count = 0, \
               last_activity_at = NOW() \
             RETURNING id",
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

    /// Count active sessions per container_id (status IN active/disconnected/stuck).
    /// Used to resync the session_count in the containers table.
    pub async fn count_active_sessions_per_container(&self) -> Result<Vec<(i64, i32)>> {
        let rows: Vec<(i64, i64)> = sqlx::query_as(&format!(
            "SELECT container_id, COUNT(*) \
             FROM {}.sessions \
             WHERE status IN ('active', 'disconnected', 'stuck') AND container_id IS NOT NULL \
             GROUP BY container_id",
            SCHEMA
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id, count)| (id, count as i32)).collect())
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
            "SELECT team_id, channel_id, project, project_path, lead_session_id, dev_count, status, created_at, coordination_thread_id, automation \
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
             FROM {}.sessions WHERE team_id = $1 AND status IN ('active', 'disconnected', 'stuck')",
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

    /// Atomically claim a (team_id, role) slot before spawning a team member.
    ///
    /// For singleton roles, attempts an `INSERT ... ON CONFLICT DO NOTHING` —
    /// returns `Ok(None)` if another caller already holds the slot.
    ///
    /// For multi-instance roles (e.g. Developer), takes a Postgres
    /// transaction-scoped advisory lock keyed by (team_id, base_role), picks
    /// the next instance number as `MAX(trailing_digits) + 1` (monotonic — never
    /// reuses a number even after a member exits), and inserts the claim.
    /// Numbering is monotonic so a re-spawn after a member exits won't collide
    /// with the partial unique index on sessions(team_id, role).
    ///
    /// Returns the assigned display name (e.g. "Architect", "Developer 3") on
    /// success, or `None` if the slot was already taken.
    pub async fn claim_team_role(
        &self,
        team_id: &str,
        base_role: &str,
        allow_multiple: bool,
        session_id: &str,
    ) -> Result<Option<String>> {
        let mut tx = self.pool.begin().await?;

        // Serialize concurrent claims for the same role family within a team
        // (across processes too — the lock lives in Postgres, not in-memory).
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("{}::{}", team_id, base_role))
            .execute(&mut *tx)
            .await?;

        let display_name = if allow_multiple {
            let like_pattern = format!(
                "{} %",
                base_role.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
            );
            let next_n: i32 = sqlx::query_scalar(&format!(
                "SELECT COALESCE(MAX((SUBSTRING(role FROM '\\d+$'))::INTEGER), 0) + 1 \
                 FROM {}.team_role_claims \
                 WHERE team_id = $1 AND (role = $2 OR role LIKE $3)",
                SCHEMA
            ))
            .bind(team_id)
            .bind(base_role)
            .bind(like_pattern)
            .fetch_one(&mut *tx)
            .await?;
            format!("{} {}", base_role, next_n)
        } else {
            base_role.to_string()
        };

        let inserted = sqlx::query(&format!(
            "INSERT INTO {}.team_role_claims (team_id, role, session_id) \
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            SCHEMA
        ))
        .bind(team_id)
        .bind(&display_name)
        .bind(session_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        tx.commit().await?;

        if inserted == 0 {
            Ok(None)
        } else {
            Ok(Some(display_name))
        }
    }

    /// Release a (team_id, role) claim — called from cleanup paths so the slot
    /// becomes available for a future spawn.
    pub async fn release_team_role(&self, team_id: &str, role: &str) -> Result<()> {
        sqlx::query(&format!(
            "DELETE FROM {}.team_role_claims WHERE team_id = $1 AND role = $2",
            SCHEMA
        ))
        .bind(team_id)
        .bind(role)
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
             FROM {}.sessions WHERE team_id = $1 AND (role = $2 OR role LIKE $3) AND status IN ('active', 'disconnected', 'stuck')",
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
        let truncated = truncate_task_summary(task_summary);
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

    /// Atomically claim a member's pending-task slot. Only succeeds if the
    /// target has no pending task. Returns true if this caller won the claim,
    /// false if another sender beat us to it.
    ///
    /// This is the race-safe variant used by route_team_message when
    /// dispatching a [TO:Role] to one of multiple idle candidates — two
    /// concurrent assignments can't both attach to the same idle member.
    pub async fn try_set_pending_task(
        &self,
        target_session_id: &str,
        sender_session_id: &str,
        task_summary: &str,
    ) -> Result<bool> {
        let truncated = truncate_task_summary(task_summary);
        let result = sqlx::query(&format!(
            "UPDATE {}.sessions \
             SET pending_task_from = $1, current_task = $2 \
             WHERE session_id = $3 AND pending_task_from IS NULL",
            SCHEMA
        ))
        .bind(sender_session_id)
        .bind(&truncated)
        .bind(target_session_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
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
            "SELECT team_id, channel_id, project, project_path, lead_session_id, dev_count, status, created_at, coordination_thread_id, automation \
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

    /// Update the team lead session ID (used when team lead resumes with a new session ID).
    pub async fn update_team_lead_session(
        &self,
        team_id: &str,
        new_lead_session_id: &str,
    ) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.teams SET lead_session_id = $1 WHERE team_id = $2",
            SCHEMA
        ))
        .bind(new_lead_session_id)
        .bind(team_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set automation mode for a team.
    pub async fn set_team_automation(
        &self,
        team_id: &str,
        enabled: bool,
    ) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.teams SET automation = $1 WHERE team_id = $2",
            SCHEMA
        ))
        .bind(enabled)
        .bind(team_id)
        .execute(&self.pool)
        .await?;
        Ok(())
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

    // --- Team task queue operations ---

    /// Enqueue a [TO:Role] task for delivery. Returns the new task_id.
    /// `is_prefix_match` records whether the original marker was a prefix
    /// (e.g. `[TO:Developer]`) vs exact (e.g. `[TO:Developer 2]`); the drain
    /// uses it to decide whether to fan to any idle member of the role or
    /// hold for the named one.
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_team_task(
        &self,
        team_id: &str,
        target_role: &str,
        is_prefix_match: bool,
        sender_session_id: &str,
        sender_role: &str,
        message: &str,
        deliver_after: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(&format!(
            "INSERT INTO {}.team_task_queue \
                 (team_id, target_role, is_prefix_match, sender_session_id, sender_role, message, deliver_after) \
             VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, NOW())) \
             RETURNING task_id",
            SCHEMA
        ))
        .bind(team_id)
        .bind(target_role)
        .bind(is_prefix_match)
        .bind(sender_session_id)
        .bind(sender_role)
        .bind(message)
        .bind(deliver_after)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Enqueue a [SPAWN:Role] task. Performs an atomic SQL-level dedupe: if
    /// an existing spawn row for the same (team, role, md5(message)) is
    /// queued or recently delivered, returns the existing task_id without
    /// inserting. This catches LLM re-emissions that slip past in-memory
    /// dedupe windows (the original bug: re-emit after dedupe TTL expires
    /// while Developer 1 is still busy with the same task → spawned
    /// Developer 2 by accident).
    ///
    /// Returns Ok(Ok(task_id)) on insert, Ok(Err(existing_id)) on dedupe hit.
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_team_spawn(
        &self,
        team_id: &str,
        target_role: &str,
        sender_session_id: &str,
        sender_role: &str,
        initial_task: &str,
        spawn_payload: &serde_json::Value,
        dedupe_window_secs: i64,
    ) -> Result<std::result::Result<i64, i64>> {
        // Dedupe window. Same role + same task content = same intent, suppressed.
        let existing: Option<(i64,)> = sqlx::query_as(&format!(
            "SELECT task_id FROM {}.team_task_queue \
             WHERE team_id = $1 AND target_role = $2 AND task_type = 'spawn' \
               AND md5(message) = md5($3) \
               AND status IN ('queued', 'running', 'completed') \
               AND created_at > NOW() - make_interval(secs => $4) \
             ORDER BY task_id DESC LIMIT 1",
            SCHEMA
        ))
        .bind(team_id)
        .bind(target_role)
        .bind(initial_task)
        .bind(dedupe_window_secs as f64)
        .fetch_optional(&self.pool)
        .await?;
        if let Some((existing_id,)) = existing {
            return Ok(Err(existing_id));
        }

        let row: (i64,) = sqlx::query_as(&format!(
            "INSERT INTO {}.team_task_queue \
                 (team_id, target_role, is_prefix_match, sender_session_id, sender_role, \
                  message, task_type, spawn_payload) \
             VALUES ($1, $2, false, $3, $4, $5, 'spawn', $6) \
             RETURNING task_id",
            SCHEMA
        ))
        .bind(team_id)
        .bind(target_role)
        .bind(sender_session_id)
        .bind(sender_role)
        .bind(initial_task)
        .bind(spawn_payload)
        .fetch_one(&self.pool)
        .await?;
        Ok(Ok(row.0))
    }

    /// Result of an enqueue_team_clear call: Enqueued(task_id), Cooldown(secs
    /// remaining), or AlreadyQueued(existing_task_id).
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_team_clear(
        &self,
        team_id: &str,
        target_role: &str,
        target_session_id: &str,
        sender_session_id: &str,
        sender_role: &str,
        reason: &str,
        cooldown_secs: i64,
    ) -> Result<EnqueueClearResult> {
        // Cooldown: if a clear was delivered to this session within
        // cooldown_secs, reject. Auto + manual + self-clear all share this
        // gate, so two ResponseCompletes back-to-back can't ping-pong /clear.
        let last: Option<(Option<chrono::DateTime<chrono::Utc>>,)> = sqlx::query_as(&format!(
            "SELECT MAX(delivered_at) FROM {}.team_task_queue \
             WHERE target_session_id = $1 AND task_type = 'clear' AND status = 'completed'",
            SCHEMA
        ))
        .bind(target_session_id)
        .fetch_optional(&self.pool)
        .await?;
        if let Some((Some(last_delivered),)) = last {
            let elapsed = (chrono::Utc::now() - last_delivered).num_seconds();
            if elapsed < cooldown_secs {
                return Ok(EnqueueClearResult::Cooldown {
                    remaining_secs: cooldown_secs - elapsed,
                });
            }
        }

        // Coalesce: if a clear is already queued for this session, return its
        // task_id rather than enqueueing a duplicate. The Lead may emit
        // [CLEAR:Self] twice in one turn — second one is a no-op.
        let pending: Option<(i64,)> = sqlx::query_as(&format!(
            "SELECT task_id FROM {}.team_task_queue \
             WHERE target_session_id = $1 AND task_type = 'clear' AND status = 'queued'",
            SCHEMA
        ))
        .bind(target_session_id)
        .fetch_optional(&self.pool)
        .await?;
        if let Some((existing_id,)) = pending {
            return Ok(EnqueueClearResult::AlreadyQueued(existing_id));
        }

        let row: (i64,) = sqlx::query_as(&format!(
            "INSERT INTO {}.team_task_queue \
                 (team_id, target_role, target_session_id, is_prefix_match, \
                  sender_session_id, sender_role, message, task_type) \
             VALUES ($1, $2, $3, false, $4, $5, $6, 'clear') \
             RETURNING task_id",
            SCHEMA
        ))
        .bind(team_id)
        .bind(target_role)
        .bind(target_session_id)
        .bind(sender_session_id)
        .bind(sender_role)
        .bind(reason)
        .fetch_one(&self.pool)
        .await?;
        Ok(EnqueueClearResult::Enqueued(row.0))
    }

    /// Oldest-first list of queued tasks for a team that are ready to deliver
    /// now (deliver_after has passed). Drain orchestration walks this list
    /// and tries to claim a target for each row.
    pub async fn list_queued_tasks_for_team(&self, team_id: &str) -> Result<Vec<StoredTeamTask>> {
        let rows = sqlx::query_as::<_, StoredTeamTask>(&format!(
            "SELECT task_id, team_id, target_role, target_session_id, is_prefix_match, \
                    sender_session_id, sender_role, message, status, \
                    created_at, delivered_at, cancelled_at, failure_reason, deliver_after, \
                    task_type, spawn_payload \
             FROM {}.team_task_queue \
             WHERE team_id = $1 AND status = 'queued' AND deliver_after <= NOW() \
             ORDER BY created_at ASC",
            SCHEMA
        ))
        .bind(team_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Distinct team_ids with at least one queued task whose deliver_after has
    /// just become eligible. The periodic scheduled-drain ticker uses this to
    /// know which teams need a drain pass.
    pub async fn teams_with_ready_scheduled_tasks(&self) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(&format!(
            "SELECT DISTINCT team_id \
             FROM {}.team_task_queue \
             WHERE status = 'queued' AND deliver_after <= NOW()",
            SCHEMA
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(t,)| t).collect())
    }

    /// Mark a synchronous task (spawn / clear) as terminally completed.
    /// Caller has already pushed the side effect (started a member, cleared
    /// a context). These task types have no asynchronous in-flight phase:
    /// the side effect *is* the work, so they go queued → completed
    /// directly without visiting 'running'.
    ///
    /// Note: `delivered_at` here is the column name (when the work was
    /// dispatched), not the status value — the column predates the rename
    /// and is reused as a "dispatched at" timestamp for both running and
    /// completed rows.
    pub async fn mark_task_completed(
        &self,
        task_id: i64,
        target_session_id: &str,
    ) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.team_task_queue \
             SET status = 'completed', delivered_at = NOW(), target_session_id = $2 \
             WHERE task_id = $1",
            SCHEMA
        ))
        .bind(task_id)
        .bind(target_session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a message-type task as in-flight on `target_session_id`. The
    /// previous design jumped straight to 'delivered' here; that conflated
    /// "dispatched" with "presumed completed" and left no way to distinguish
    /// a turn that's still running from one that finished. The new state
    /// machine is queued → running → completed, with running → queued
    /// available for retry on rate-limit pause.
    pub async fn mark_task_running(
        &self,
        task_id: i64,
        target_session_id: &str,
    ) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.team_task_queue \
             SET status = 'running', delivered_at = NOW(), target_session_id = $2 \
             WHERE task_id = $1",
            SCHEMA
        ))
        .bind(task_id)
        .bind(target_session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Flip the running message-task on `target_session_id` to terminal
    /// 'completed'. Called from the `ResponseComplete` handler — that's the
    /// canonical "the turn this task started actually finished" signal.
    /// Idempotent: a second call is a no-op because the WHERE clause only
    /// matches `status='running'`. Returns the task_id we transitioned, if
    /// any (useful for logs / metrics).
    pub async fn mark_message_task_completed_for_session(
        &self,
        target_session_id: &str,
    ) -> Result<Option<i64>> {
        let row: Option<(i64,)> = sqlx::query_as(&format!(
            "UPDATE {}.team_task_queue \
             SET status = 'completed' \
             WHERE target_session_id = $1 \
               AND status = 'running' \
               AND task_type = 'message' \
             RETURNING task_id",
            SCHEMA
        ))
        .bind(target_session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(t,)| t))
    }

    /// Flip the running message-task on `target_session_id` to terminal
    /// 'cancelled'. Called from the interrupt path so an aborted turn
    /// doesn't get racy-promoted to 'completed' if a `ResponseComplete`
    /// happens to fire after the cancel signal. Idempotent.
    pub async fn mark_running_message_task_cancelled_for_session(
        &self,
        target_session_id: &str,
        reason: &str,
    ) -> Result<Option<i64>> {
        let row: Option<(i64,)> = sqlx::query_as(&format!(
            "UPDATE {}.team_task_queue \
             SET status = 'cancelled', \
                 cancelled_at = NOW(), \
                 failure_reason = $2 \
             WHERE target_session_id = $1 \
               AND status = 'running' \
               AND task_type = 'message' \
             RETURNING task_id",
            SCHEMA
        ))
        .bind(target_session_id)
        .bind(reason)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(t,)| t))
    }

    /// Push the running message-task on `target_session_id` back to 'queued'
    /// so it gets redelivered on the next drain pass. Preserves `created_at`
    /// (and therefore queue ordering relative to other tasks). Used by the
    /// rate-limit pause path: a turn that errored because the API window
    /// closed needs to be retried, but keeping it ahead of (or behind) other
    /// queued tasks should follow the original arrival order.
    ///
    /// `deliver_after` lets the caller hold redelivery until the rate-limit
    /// window actually reopens — drain ignores rows whose `deliver_after`
    /// is in the future.
    pub async fn requeue_running_message_task_for_session(
        &self,
        target_session_id: &str,
        deliver_after: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<i64>> {
        let row: Option<(i64,)> = sqlx::query_as(&format!(
            "UPDATE {}.team_task_queue \
             SET status = 'queued', \
                 delivered_at = NULL, \
                 target_session_id = NULL, \
                 deliver_after = $2 \
             WHERE target_session_id = $1 \
               AND status = 'running' \
               AND task_type = 'message' \
             RETURNING task_id",
            SCHEMA
        ))
        .bind(target_session_id)
        .bind(deliver_after)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(t,)| t))
    }

    // ---- team_rate_limits --------------------------------------------------

    /// Upsert the team's rate-limit state. Pass `resets_at_unix=0` if the SDK
    /// didn't include a reset timestamp. `notice_post_id` is preserved across
    /// updates (we keep editing the same Mattermost card).
    pub async fn upsert_team_rate_limit(
        &self,
        team_id: &str,
        status: &str,
        resets_at_unix: i64,
        limit_type: &str,
        utilization: Option<f64>,
        raw_json: &str,
    ) -> Result<()> {
        let resets_at = if resets_at_unix > 0 {
            chrono::DateTime::<chrono::Utc>::from_timestamp(resets_at_unix, 0)
        } else {
            None
        };
        sqlx::query(&format!(
            "INSERT INTO {}.team_rate_limits \
                 (team_id, status, resets_at, limit_type, utilization, raw_json, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, NOW()) \
             ON CONFLICT (team_id) DO UPDATE SET \
                 status = EXCLUDED.status, \
                 resets_at = EXCLUDED.resets_at, \
                 limit_type = EXCLUDED.limit_type, \
                 utilization = EXCLUDED.utilization, \
                 raw_json = EXCLUDED.raw_json, \
                 updated_at = NOW()",
            SCHEMA
        ))
        .bind(team_id)
        .bind(status)
        .bind(resets_at)
        .bind(limit_type)
        .bind(utilization)
        .bind(raw_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist (or clear) the Mattermost post id for the team's rate-limit
    /// notice card so subsequent transitions update the same post.
    pub async fn set_team_rate_limit_post_id(
        &self,
        team_id: &str,
        post_id: Option<&str>,
    ) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.team_rate_limits SET notice_post_id = $2 WHERE team_id = $1",
            SCHEMA
        ))
        .bind(team_id)
        .bind(post_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_team_rate_limit(&self, team_id: &str) -> Result<Option<TeamRateLimit>> {
        let row = sqlx::query_as::<_, TeamRateLimit>(&format!(
            "SELECT team_id, status, resets_at, limit_type, utilization, raw_json, \
                    updated_at, notice_post_id \
             FROM {}.team_rate_limits WHERE team_id = $1",
            SCHEMA
        ))
        .bind(team_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// True if this team is currently rate-limit-rejected and the reset window
    /// has not yet elapsed. Drain orchestration uses this to skip delivery.
    pub async fn is_team_rate_limited(&self, team_id: &str) -> Result<bool> {
        let row: Option<(bool,)> = sqlx::query_as(&format!(
            "SELECT TRUE FROM {}.team_rate_limits \
             WHERE team_id = $1 AND status = 'rejected' \
               AND (resets_at IS NULL OR resets_at > NOW()) \
             LIMIT 1",
            SCHEMA
        ))
        .bind(team_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Team_ids currently in `rejected` state, regardless of resets_at. Used
    /// by the reactive profile-rotation path: when the active profile flips,
    /// every team stuck on the previous account gets cleared so the next
    /// message takes the CreateSession path (which picks up the new env).
    pub async fn teams_in_rejected_state(&self) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(&format!(
            "SELECT team_id FROM {}.team_rate_limits WHERE status = 'rejected'",
            SCHEMA
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(t,)| t).collect())
    }

    /// Team_ids whose `rejected` window has passed; the scheduled-drain
    /// ticker walks these to flip them back to `allowed` and trigger a drain.
    pub async fn teams_with_expired_rate_limits(&self) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(&format!(
            "SELECT team_id FROM {}.team_rate_limits \
             WHERE status = 'rejected' AND resets_at IS NOT NULL AND resets_at <= NOW()",
            SCHEMA
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(t,)| t).collect())
    }

    /// Mark a previously-rejected team back to `allowed`. Returns the post id
    /// of the existing notice (if any) so the caller can edit it in place.
    pub async fn clear_team_rate_limit(&self, team_id: &str) -> Result<Option<String>> {
        let row: Option<(Option<String>,)> = sqlx::query_as(&format!(
            "UPDATE {}.team_rate_limits \
             SET status = 'allowed', resets_at = NULL, updated_at = NOW() \
             WHERE team_id = $1 \
             RETURNING notice_post_id",
            SCHEMA
        ))
        .bind(team_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(p,)| p))
    }

    // ---- claude_profile_state ---------------------------------------------

    /// Read the singleton row. Always returns a row — the seed runs in
    /// create_schema.
    pub async fn get_claude_profile_state(&self) -> Result<ClaudeProfileState> {
        let row = sqlx::query_as::<_, ClaudeProfileState>(&format!(
            "SELECT claude_config_dir, event_id, swapped_at, updated_at, updated_by \
             FROM {}.claude_profile_state WHERE id = 1",
            SCHEMA
        ))
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Update the singleton. `claude_config_dir == ""` clears the override.
    /// Returns the row as written for echoing in the admin response.
    pub async fn set_claude_profile_state(
        &self,
        claude_config_dir: &str,
        event_id: Option<uuid::Uuid>,
        swapped_at: Option<chrono::DateTime<chrono::Utc>>,
        updated_by: Option<&str>,
    ) -> Result<ClaudeProfileState> {
        let row = sqlx::query_as::<_, ClaudeProfileState>(&format!(
            "UPDATE {}.claude_profile_state \
                SET claude_config_dir = $1, \
                    event_id          = $2, \
                    swapped_at        = $3, \
                    updated_at        = NOW(), \
                    updated_by        = $4 \
              WHERE id = 1 \
              RETURNING claude_config_dir, event_id, swapped_at, updated_at, updated_by",
            SCHEMA
        ))
        .bind(claude_config_dir)
        .bind(event_id)
        .bind(swapped_at)
        .bind(updated_by)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Mark a queued task as failed (e.g. delivery_unreachable). Used by drain
    /// when `containers.send` fails so we don't keep retrying a dead session.
    pub async fn mark_task_failed(&self, task_id: i64, reason: &str) -> Result<()> {
        sqlx::query(&format!(
            "UPDATE {}.team_task_queue \
             SET status = 'failed', failure_reason = $2 \
             WHERE task_id = $1",
            SCHEMA
        ))
        .bind(task_id)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Cancel every still-queued task from `sender_session_id` aimed at
    /// `target_role` (exact role string match). Only rows with status='queued'
    /// flip — already-delivered rows are left alone (Lead must INTERRUPT).
    /// Returns the number of cancelled rows.
    pub async fn cancel_queued_from_sender(
        &self,
        team_id: &str,
        sender_session_id: &str,
        target_role: &str,
    ) -> Result<u64> {
        let result = sqlx::query(&format!(
            "UPDATE {}.team_task_queue \
             SET status = 'cancelled', cancelled_at = NOW() \
             WHERE team_id = $1 AND sender_session_id = $2 AND target_role = $3 \
               AND status = 'queued'",
            SCHEMA
        ))
        .bind(team_id)
        .bind(sender_session_id)
        .bind(target_role)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Cancel a specific queued task by id. Sender check ensures Lead can't
    /// cancel another sender's queue (currently only the Lead enqueues, but
    /// the check is cheap defence-in-depth). Returns true on success.
    pub async fn cancel_queued_task_by_id(
        &self,
        task_id: i64,
        sender_session_id: &str,
    ) -> Result<bool> {
        let result = sqlx::query(&format!(
            "UPDATE {}.team_task_queue \
             SET status = 'cancelled', cancelled_at = NOW() \
             WHERE task_id = $1 AND sender_session_id = $2 AND status = 'queued'",
            SCHEMA
        ))
        .bind(task_id)
        .bind(sender_session_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Per-role queued count for a team. Used by [TEAM_STATUS] to surface
    /// queue depth without dumping every row.
    pub async fn queued_counts_by_role(&self, team_id: &str) -> Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(&format!(
            "SELECT target_role, COUNT(*)::BIGINT \
             FROM {}.team_task_queue \
             WHERE team_id = $1 AND status = 'queued' \
             GROUP BY target_role \
             ORDER BY target_role",
            SCHEMA
        ))
        .bind(team_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
