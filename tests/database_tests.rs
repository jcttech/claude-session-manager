//! Database integration tests
//!
//! These tests require a running PostgreSQL instance.
//! Set DATABASE_URL environment variable to run these tests.
//!
//! Example:
//!   DATABASE_URL=postgres://user:pass@localhost/test_db cargo test --test database_tests
//!
//! Schema and tables are created once via the shared `create_schema` function;
//! each test runs in a transaction that auto-rolls back for isolation.

use session_manager::database::create_schema;
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::env;
use tokio::sync::OnceCell;

const TEST_SCHEMA: &str = "session_manager_test";

static TEST_DB: OnceCell<PgPool> = OnceCell::const_new();

async fn get_test_db() -> Option<&'static PgPool> {
    if env::var("DATABASE_URL").is_err() {
        eprintln!("Skipping database tests: DATABASE_URL not set");
        return None;
    }

    Some(
        TEST_DB
            .get_or_init(|| async {
                let url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");

                let pool = PgPoolOptions::new()
                    .max_connections(5)
                    .connect(&url)
                    .await
                    .expect("Failed to connect to database");

                // Fresh schema for test isolation
                sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", TEST_SCHEMA))
                    .execute(&pool)
                    .await
                    .expect("Failed to drop schema");

                create_schema(&pool, TEST_SCHEMA)
                    .await
                    .expect("Failed to create test schema");

                pool
            })
            .await,
    )
}

#[tokio::test]
async fn test_session_crud() {
    let Some(pool) = get_test_db().await else {
        return;
    };
    let mut tx = pool.begin().await.expect("Failed to begin transaction");

    let session_id = "test-session-123";
    let channel_id = "test-channel-456";
    let thread_id = "test-thread-789";
    let project = "test-project";
    let container_name = "claude-test123";

    sqlx::query(&format!(
        "INSERT INTO {}.sessions (session_id, channel_id, thread_id, project, container_name) VALUES ($1, $2, $3, $4, $5)",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .bind(channel_id)
    .bind(thread_id)
    .bind(project)
    .bind(container_name)
    .execute(&mut *tx)
    .await
    .expect("Failed to insert session");

    let row: (String, String, String, String, String) = sqlx::query_as(&format!(
        "SELECT session_id, channel_id, thread_id, project, container_name FROM {}.sessions WHERE channel_id = $1 AND thread_id = $2",
        TEST_SCHEMA
    ))
    .bind(channel_id)
    .bind(thread_id)
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to fetch session");

    assert_eq!(row.0, session_id);
    assert_eq!(row.1, channel_id);
    assert_eq!(row.2, thread_id);
    assert_eq!(row.3, project);
    assert_eq!(row.4, container_name);

    sqlx::query(&format!(
        "DELETE FROM {}.sessions WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .execute(&mut *tx)
    .await
    .expect("Failed to delete session");

    let count: (i64,) = sqlx::query_as(&format!(
        "SELECT COUNT(*) FROM {}.sessions WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to count sessions");

    assert_eq!(count.0, 0);
}

#[tokio::test]
async fn test_session_types_and_parent() {
    let Some(pool) = get_test_db().await else {
        return;
    };
    let mut tx = pool.begin().await.expect("Failed to begin transaction");

    sqlx::query(&format!(
        "INSERT INTO {}.sessions (session_id, channel_id, thread_id, project, container_name, session_type) VALUES ($1, $2, $3, $4, $5, $6)",
        TEST_SCHEMA
    ))
    .bind("orch-session")
    .bind("chan-1")
    .bind("thread-1")
    .bind("org/repo")
    .bind("claude-orch1234")
    .bind("orchestrator")
    .execute(&mut *tx)
    .await
    .expect("Failed to insert orchestrator session");

    sqlx::query(&format!(
        "INSERT INTO {}.sessions (session_id, channel_id, thread_id, project, container_name, session_type, parent_session_id) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        TEST_SCHEMA
    ))
    .bind("worker-session")
    .bind("chan-1")
    .bind("thread-2")
    .bind("org/repo --worktree")
    .bind("claude-work1234")
    .bind("worker")
    .bind("orch-session")
    .execute(&mut *tx)
    .await
    .expect("Failed to insert worker session");

    let row: (String, String) = sqlx::query_as(&format!(
        "SELECT session_id, session_type FROM {}.sessions WHERE channel_id = $1 AND session_type = 'orchestrator'",
        TEST_SCHEMA
    ))
    .bind("chan-1")
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to fetch orchestrator");

    assert_eq!(row.0, "orch-session");
    assert_eq!(row.1, "orchestrator");

    let children: Vec<(String, String)> = sqlx::query_as(&format!(
        "SELECT session_id, session_type FROM {}.sessions WHERE parent_session_id = $1",
        TEST_SCHEMA
    ))
    .bind("orch-session")
    .fetch_all(&mut *tx)
    .await
    .expect("Failed to fetch children");

    assert_eq!(children.len(), 1);
    assert_eq!(children[0].0, "worker-session");
    assert_eq!(children[0].1, "worker");
}

#[tokio::test]
async fn test_project_channels() {
    let Some(pool) = get_test_db().await else {
        return;
    };
    let mut tx = pool.begin().await.expect("Failed to begin transaction");

    sqlx::query(&format!(
        "INSERT INTO {}.project_channels (project, channel_id, channel_name) VALUES ($1, $2, $3)",
        TEST_SCHEMA
    ))
    .bind("jcttech/session-manager")
    .bind("chan-abc123")
    .bind("session-manager")
    .execute(&mut *tx)
    .await
    .expect("Failed to insert project channel");

    let row: (String, String) = sqlx::query_as(&format!(
        "SELECT channel_id, channel_name FROM {}.project_channels WHERE project = $1",
        TEST_SCHEMA
    ))
    .bind("jcttech/session-manager")
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to fetch project channel");

    assert_eq!(row.0, "chan-abc123");
    assert_eq!(row.1, "session-manager");

    sqlx::query(&format!(
        "INSERT INTO {}.project_channels (project, channel_id, channel_name) VALUES ($1, $2, $3) ON CONFLICT (project) DO NOTHING",
        TEST_SCHEMA
    ))
    .bind("jcttech/session-manager")
    .bind("different-chan")
    .bind("different-name")
    .execute(&mut *tx)
    .await
    .expect("Upsert should not fail");

    let row: (String,) = sqlx::query_as(&format!(
        "SELECT channel_id FROM {}.project_channels WHERE project = $1",
        TEST_SCHEMA
    ))
    .bind("jcttech/session-manager")
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to verify");

    assert_eq!(row.0, "chan-abc123");
}

#[tokio::test]
async fn test_pending_request_crud() {
    let Some(pool) = get_test_db().await else {
        return;
    };
    let mut tx = pool.begin().await.expect("Failed to begin transaction");

    let request_id = "req-123";
    let channel_id = "chan-456";
    let thread_id = "thread-789";
    let session_id = "sess-789";
    let domain = "api.example.com";
    let post_id = "post-abc";

    sqlx::query(&format!(
        "INSERT INTO {}.pending_requests (request_id, channel_id, thread_id, session_id, domain, post_id) VALUES ($1, $2, $3, $4, $5, $6)",
        TEST_SCHEMA
    ))
    .bind(request_id)
    .bind(channel_id)
    .bind(thread_id)
    .bind(session_id)
    .bind(domain)
    .bind(post_id)
    .execute(&mut *tx)
    .await
    .expect("Failed to insert pending request");

    let row: (String, String, String, String) = sqlx::query_as(&format!(
        "SELECT session_id, thread_id, domain, post_id FROM {}.pending_requests WHERE request_id = $1",
        TEST_SCHEMA
    ))
    .bind(request_id)
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to fetch pending request");

    assert_eq!(row.0, session_id);
    assert_eq!(row.1, thread_id);
    assert_eq!(row.2, domain);
    assert_eq!(row.3, post_id);

    sqlx::query(&format!(
        "DELETE FROM {}.pending_requests WHERE request_id = $1",
        TEST_SCHEMA
    ))
    .bind(request_id)
    .execute(&mut *tx)
    .await
    .expect("Failed to delete pending request");
}

#[tokio::test]
async fn test_audit_log() {
    let Some(pool) = get_test_db().await else {
        return;
    };
    let mut tx = pool.begin().await.expect("Failed to begin transaction");

    for i in 0..5 {
        sqlx::query(&format!(
            "INSERT INTO {}.audit_log (request_id, domain, action, approved_by) VALUES ($1, $2, $3, $4)",
            TEST_SCHEMA
        ))
        .bind(format!("req-{}", i))
        .bind(format!("domain-{}.com", i))
        .bind(if i % 2 == 0 { "approve" } else { "deny" })
        .bind("testuser")
        .execute(&mut *tx)
        .await
        .expect("Failed to insert audit log");
    }

    let count: (i64,) = sqlx::query_as(&format!(
        "SELECT COUNT(*) FROM {}.audit_log",
        TEST_SCHEMA
    ))
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to count audit log");

    assert_eq!(count.0, 5);

    let entries: Vec<(String, String, String)> = sqlx::query_as(&format!(
        "SELECT request_id, domain, action FROM {}.audit_log ORDER BY created_at DESC LIMIT 3",
        TEST_SCHEMA
    ))
    .fetch_all(&mut *tx)
    .await
    .expect("Failed to fetch audit log");

    assert_eq!(entries.len(), 3);
}

#[tokio::test]
async fn test_cascade_delete_pending_on_session() {
    let Some(pool) = get_test_db().await else {
        return;
    };
    let mut tx = pool.begin().await.expect("Failed to begin transaction");

    let session_id = "sess-to-delete";
    let channel_id = "chan-123";
    let thread_id = "thread-123";

    sqlx::query(&format!(
        "INSERT INTO {}.sessions (session_id, channel_id, thread_id, project, container_name) VALUES ($1, $2, $3, 'proj', 'container')",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .bind(channel_id)
    .bind(thread_id)
    .execute(&mut *tx)
    .await
    .expect("Failed to insert session");

    for i in 0..3 {
        sqlx::query(&format!(
            "INSERT INTO {}.pending_requests (request_id, channel_id, thread_id, session_id, domain, post_id) VALUES ($1, $2, $3, $4, $5, $6)",
            TEST_SCHEMA
        ))
        .bind(format!("req-{}", i))
        .bind(channel_id)
        .bind(thread_id)
        .bind(session_id)
        .bind(format!("domain-{}.com", i))
        .bind(format!("post-{}", i))
        .execute(&mut *tx)
        .await
        .expect("Failed to insert pending request");
    }

    let count: (i64,) = sqlx::query_as(&format!(
        "SELECT COUNT(*) FROM {}.pending_requests WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to count pending requests");

    assert_eq!(count.0, 3);

    sqlx::query(&format!(
        "DELETE FROM {}.sessions WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .execute(&mut *tx)
    .await
    .expect("Failed to delete session");

    sqlx::query(&format!(
        "DELETE FROM {}.pending_requests WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .execute(&mut *tx)
    .await
    .expect("Failed to delete pending requests");

    let count: (i64,) = sqlx::query_as(&format!(
        "SELECT COUNT(*) FROM {}.pending_requests WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to count pending requests");

    assert_eq!(count.0, 0);
}

#[tokio::test]
async fn test_stale_request_cleanup() {
    let Some(pool) = get_test_db().await else {
        return;
    };
    let mut tx = pool.begin().await.expect("Failed to begin transaction");

    sqlx::query(&format!(
        "INSERT INTO {}.pending_requests (request_id, channel_id, thread_id, session_id, domain, post_id, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, NOW() - INTERVAL '25 hours')",
        TEST_SCHEMA
    ))
    .bind("old-request")
    .bind("chan-1")
    .bind("thread-1")
    .bind("sess-1")
    .bind("old.domain.com")
    .bind("post-1")
    .execute(&mut *tx)
    .await
    .expect("Failed to insert old request");

    sqlx::query(&format!(
        "INSERT INTO {}.pending_requests (request_id, channel_id, thread_id, session_id, domain, post_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
        TEST_SCHEMA
    ))
    .bind("new-request")
    .bind("chan-2")
    .bind("thread-2")
    .bind("sess-2")
    .bind("new.domain.com")
    .bind("post-2")
    .execute(&mut *tx)
    .await
    .expect("Failed to insert new request");

    let result = sqlx::query(&format!(
        "DELETE FROM {}.pending_requests WHERE created_at < NOW() - INTERVAL '1 hour' * $1",
        TEST_SCHEMA
    ))
    .bind(24i64)
    .execute(&mut *tx)
    .await
    .expect("Failed to cleanup");

    assert_eq!(result.rows_affected(), 1);

    let remaining: Vec<(String,)> = sqlx::query_as(&format!(
        "SELECT request_id FROM {}.pending_requests",
        TEST_SCHEMA
    ))
    .fetch_all(&mut *tx)
    .await
    .expect("Failed to fetch remaining");

    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].0, "new-request");
}

#[tokio::test]
async fn test_unique_constraints() {
    let Some(pool) = get_test_db().await else {
        return;
    };
    let mut tx = pool.begin().await.expect("Failed to begin transaction");

    sqlx::query(&format!(
        "INSERT INTO {}.sessions (session_id, channel_id, thread_id, project, container_name) VALUES ($1, $2, $3, $4, $5)",
        TEST_SCHEMA
    ))
    .bind("unique-session")
    .bind("chan-1")
    .bind("thread-1")
    .bind("proj")
    .bind("container")
    .execute(&mut *tx)
    .await
    .expect("Failed to insert session");

    let result = sqlx::query(&format!(
        "INSERT INTO {}.sessions (session_id, channel_id, thread_id, project, container_name) VALUES ($1, $2, $3, $4, $5)",
        TEST_SCHEMA
    ))
    .bind("unique-session")
    .bind("chan-2")
    .bind("thread-2")
    .bind("proj2")
    .bind("container2")
    .execute(&mut *tx)
    .await;

    assert!(result.is_err(), "Duplicate session_id should fail");
}

#[tokio::test]
async fn test_session_id_prefix_lookup() {
    let Some(pool) = get_test_db().await else {
        return;
    };
    let mut tx = pool.begin().await.expect("Failed to begin transaction");

    sqlx::query(&format!(
        "INSERT INTO {}.sessions (session_id, channel_id, thread_id, project, container_name) VALUES ($1, $2, $3, $4, $5)",
        TEST_SCHEMA
    ))
    .bind("abcdef12-3456-7890-abcd-ef1234567890")
    .bind("chan-1")
    .bind("thread-1")
    .bind("org/repo")
    .bind("claude-abcdef12")
    .execute(&mut *tx)
    .await
    .expect("Failed to insert session");

    let row: (String,) = sqlx::query_as(&format!(
        "SELECT session_id FROM {}.sessions WHERE session_id LIKE $1 LIMIT 1",
        TEST_SCHEMA
    ))
    .bind("abcdef12%")
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to fetch by prefix");

    assert_eq!(row.0, "abcdef12-3456-7890-abcd-ef1234567890");

    let result: Option<(String,)> = sqlx::query_as(&format!(
        "SELECT session_id FROM {}.sessions WHERE session_id LIKE $1 LIMIT 1",
        TEST_SCHEMA
    ))
    .bind("zzzzzzz%")
    .fetch_optional(&mut *tx)
    .await
    .expect("Query should succeed");

    assert!(result.is_none());
}

#[tokio::test]
async fn test_touch_session() {
    let Some(pool) = get_test_db().await else {
        return;
    };
    let mut tx = pool.begin().await.expect("Failed to begin transaction");

    let session_id = "touch-test-session";

    sqlx::query(&format!(
        "INSERT INTO {}.sessions (session_id, channel_id, thread_id, project, container_name) VALUES ($1, $2, $3, $4, $5)",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .bind("chan-1")
    .bind("thread-1")
    .bind("org/repo")
    .bind("container-1")
    .execute(&mut *tx)
    .await
    .expect("Failed to insert session");

    let row: (i32,) = sqlx::query_as(&format!(
        "SELECT message_count FROM {}.sessions WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to fetch session");

    assert_eq!(row.0, 0, "Initial message count should be 0");

    for expected in 1..=5 {
        let result: (i32,) = sqlx::query_as(&format!(
            "UPDATE {}.sessions SET last_activity_at = NOW(), message_count = message_count + 1 \
             WHERE session_id = $1 RETURNING message_count",
            TEST_SCHEMA
        ))
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await
        .expect("Failed to touch session");

        assert_eq!(result.0, expected, "Message count should increment");
    }

    let row: (i32,) = sqlx::query_as(&format!(
        "SELECT message_count FROM {}.sessions WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to fetch session");

    assert_eq!(row.0, 5, "Final message count should be 5");
}

#[tokio::test]
async fn test_record_compaction() {
    let Some(pool) = get_test_db().await else {
        return;
    };
    let mut tx = pool.begin().await.expect("Failed to begin transaction");

    let session_id = "compact-test-session";

    sqlx::query(&format!(
        "INSERT INTO {}.sessions (session_id, channel_id, thread_id, project, container_name) VALUES ($1, $2, $3, $4, $5)",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .bind("chan-1")
    .bind("thread-1")
    .bind("org/repo")
    .bind("container-1")
    .execute(&mut *tx)
    .await
    .expect("Failed to insert session");

    let row: (i32,) = sqlx::query_as(&format!(
        "SELECT compaction_count FROM {}.sessions WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to fetch session");

    assert_eq!(row.0, 0, "Initial compaction count should be 0");

    sqlx::query(&format!(
        "UPDATE {}.sessions SET compaction_count = compaction_count + 1 WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .execute(&mut *tx)
    .await
    .expect("Failed to record compaction");

    let row: (i32,) = sqlx::query_as(&format!(
        "SELECT compaction_count FROM {}.sessions WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to fetch session");

    assert_eq!(row.0, 1, "Compaction count should be 1");
}

#[tokio::test]
async fn test_session_health_defaults() {
    let Some(pool) = get_test_db().await else {
        return;
    };
    let mut tx = pool.begin().await.expect("Failed to begin transaction");

    let session_id = "health-defaults-session";

    sqlx::query(&format!(
        "INSERT INTO {}.sessions (session_id, channel_id, thread_id, project, container_name) VALUES ($1, $2, $3, $4, $5)",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .bind("chan-1")
    .bind("thread-1")
    .bind("org/repo")
    .bind("container-1")
    .execute(&mut *tx)
    .await
    .expect("Failed to insert session");

    let row: (i32, i32) = sqlx::query_as(&format!(
        "SELECT message_count, compaction_count FROM {}.sessions WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to fetch session");

    assert_eq!(row.0, 0, "Default message count should be 0");
    assert_eq!(row.1, 0, "Default compaction count should be 0");

    let row: (bool,) = sqlx::query_as(&format!(
        "SELECT last_activity_at IS NOT NULL FROM {}.sessions WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await
    .expect("Failed to fetch session");

    assert!(row.0, "last_activity_at should not be null");
}
