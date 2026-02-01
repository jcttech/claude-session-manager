//! Database integration tests
//!
//! These tests require a running PostgreSQL instance.
//! Set DATABASE_URL environment variable to run these tests.
//!
//! Example:
//!   DATABASE_URL=postgres://user:pass@localhost/test_db cargo test --test database_tests
//!
//! The tests will create and clean up their own schema.

use sqlx::{postgres::PgPoolOptions, PgPool};
use std::env;

const TEST_SCHEMA: &str = "session_manager_test";

async fn setup_test_db() -> Option<PgPool> {
    let url = match env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("Skipping database tests: DATABASE_URL not set");
            return None;
        }
    };

    let pool = match PgPoolOptions::new().max_connections(2).connect(&url).await {
        Ok(pool) => pool,
        Err(e) => {
            eprintln!("Skipping database tests: Could not connect to database: {}", e);
            return None;
        }
    };

    // Create test schema
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", TEST_SCHEMA))
        .execute(&pool)
        .await
        .ok();

    sqlx::query(&format!("CREATE SCHEMA {}", TEST_SCHEMA))
        .execute(&pool)
        .await
        .ok();

    // Create tables
    sqlx::query(&format!(
        r#"
        CREATE TABLE {}.sessions (
            session_id TEXT PRIMARY KEY,
            channel_id TEXT NOT NULL,
            project TEXT NOT NULL,
            container_name TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
        TEST_SCHEMA
    ))
    .execute(&pool)
    .await
    .ok();

    sqlx::query(&format!(
        r#"
        CREATE TABLE {}.pending_requests (
            request_id TEXT PRIMARY KEY,
            channel_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            domain TEXT NOT NULL,
            post_id TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
        TEST_SCHEMA
    ))
    .execute(&pool)
    .await
    .ok();

    sqlx::query(&format!(
        r#"
        CREATE TABLE {}.audit_log (
            id BIGSERIAL PRIMARY KEY,
            request_id TEXT NOT NULL,
            domain TEXT NOT NULL,
            action TEXT NOT NULL,
            approved_by TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
        TEST_SCHEMA
    ))
    .execute(&pool)
    .await
    .ok();

    Some(pool)
}

async fn cleanup_test_db(pool: &PgPool) {
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", TEST_SCHEMA))
        .execute(pool)
        .await
        .ok();
}

#[tokio::test]
async fn test_session_crud() {
    let Some(pool) = setup_test_db().await else {
        return;
    };

    // Create session
    let session_id = "test-session-123";
    let channel_id = "test-channel-456";
    let project = "test-project";
    let container_name = "claude-test123";

    sqlx::query(&format!(
        "INSERT INTO {}.sessions (session_id, channel_id, project, container_name) VALUES ($1, $2, $3, $4)",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .bind(channel_id)
    .bind(project)
    .bind(container_name)
    .execute(&pool)
    .await
    .expect("Failed to insert session");

    // Read session
    let row: (String, String, String, String) = sqlx::query_as(&format!(
        "SELECT session_id, channel_id, project, container_name FROM {}.sessions WHERE channel_id = $1",
        TEST_SCHEMA
    ))
    .bind(channel_id)
    .fetch_one(&pool)
    .await
    .expect("Failed to fetch session");

    assert_eq!(row.0, session_id);
    assert_eq!(row.1, channel_id);
    assert_eq!(row.2, project);
    assert_eq!(row.3, container_name);

    // Delete session
    sqlx::query(&format!(
        "DELETE FROM {}.sessions WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .execute(&pool)
    .await
    .expect("Failed to delete session");

    // Verify deletion
    let count: (i64,) = sqlx::query_as(&format!(
        "SELECT COUNT(*) FROM {}.sessions WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("Failed to count sessions");

    assert_eq!(count.0, 0);

    cleanup_test_db(&pool).await;
}

#[tokio::test]
async fn test_pending_request_crud() {
    let Some(pool) = setup_test_db().await else {
        return;
    };

    let request_id = "req-123";
    let channel_id = "chan-456";
    let session_id = "sess-789";
    let domain = "api.example.com";
    let post_id = "post-abc";

    // Create pending request
    sqlx::query(&format!(
        "INSERT INTO {}.pending_requests (request_id, channel_id, session_id, domain, post_id) VALUES ($1, $2, $3, $4, $5)",
        TEST_SCHEMA
    ))
    .bind(request_id)
    .bind(channel_id)
    .bind(session_id)
    .bind(domain)
    .bind(post_id)
    .execute(&pool)
    .await
    .expect("Failed to insert pending request");

    // Fetch pending request
    let row: (String, String, String) = sqlx::query_as(&format!(
        "SELECT session_id, domain, post_id FROM {}.pending_requests WHERE request_id = $1",
        TEST_SCHEMA
    ))
    .bind(request_id)
    .fetch_one(&pool)
    .await
    .expect("Failed to fetch pending request");

    assert_eq!(row.0, session_id);
    assert_eq!(row.1, domain);
    assert_eq!(row.2, post_id);

    // Delete pending request
    sqlx::query(&format!(
        "DELETE FROM {}.pending_requests WHERE request_id = $1",
        TEST_SCHEMA
    ))
    .bind(request_id)
    .execute(&pool)
    .await
    .expect("Failed to delete pending request");

    cleanup_test_db(&pool).await;
}

#[tokio::test]
async fn test_audit_log() {
    let Some(pool) = setup_test_db().await else {
        return;
    };

    // Insert audit entries
    for i in 0..5 {
        sqlx::query(&format!(
            "INSERT INTO {}.audit_log (request_id, domain, action, approved_by) VALUES ($1, $2, $3, $4)",
            TEST_SCHEMA
        ))
        .bind(format!("req-{}", i))
        .bind(format!("domain-{}.com", i))
        .bind(if i % 2 == 0 { "approve" } else { "deny" })
        .bind("testuser")
        .execute(&pool)
        .await
        .expect("Failed to insert audit log");
    }

    // Count entries
    let count: (i64,) = sqlx::query_as(&format!(
        "SELECT COUNT(*) FROM {}.audit_log",
        TEST_SCHEMA
    ))
    .fetch_one(&pool)
    .await
    .expect("Failed to count audit log");

    assert_eq!(count.0, 5);

    // Fetch latest entries
    let entries: Vec<(String, String, String)> = sqlx::query_as(&format!(
        "SELECT request_id, domain, action FROM {}.audit_log ORDER BY created_at DESC LIMIT 3",
        TEST_SCHEMA
    ))
    .fetch_all(&pool)
    .await
    .expect("Failed to fetch audit log");

    assert_eq!(entries.len(), 3);

    cleanup_test_db(&pool).await;
}

#[tokio::test]
async fn test_cascade_delete_pending_on_session() {
    let Some(pool) = setup_test_db().await else {
        return;
    };

    let session_id = "sess-to-delete";
    let channel_id = "chan-123";

    // Create session
    sqlx::query(&format!(
        "INSERT INTO {}.sessions (session_id, channel_id, project, container_name) VALUES ($1, $2, 'proj', 'container')",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .bind(channel_id)
    .execute(&pool)
    .await
    .expect("Failed to insert session");

    // Create pending requests for this session
    for i in 0..3 {
        sqlx::query(&format!(
            "INSERT INTO {}.pending_requests (request_id, channel_id, session_id, domain, post_id) VALUES ($1, $2, $3, $4, $5)",
            TEST_SCHEMA
        ))
        .bind(format!("req-{}", i))
        .bind(channel_id)
        .bind(session_id)
        .bind(format!("domain-{}.com", i))
        .bind(format!("post-{}", i))
        .execute(&pool)
        .await
        .expect("Failed to insert pending request");
    }

    // Verify pending requests exist
    let count: (i64,) = sqlx::query_as(&format!(
        "SELECT COUNT(*) FROM {}.pending_requests WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("Failed to count pending requests");

    assert_eq!(count.0, 3);

    // Delete session and its pending requests (simulating the app behavior)
    sqlx::query(&format!(
        "DELETE FROM {}.sessions WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .execute(&pool)
    .await
    .expect("Failed to delete session");

    sqlx::query(&format!(
        "DELETE FROM {}.pending_requests WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .execute(&pool)
    .await
    .expect("Failed to delete pending requests");

    // Verify pending requests are gone
    let count: (i64,) = sqlx::query_as(&format!(
        "SELECT COUNT(*) FROM {}.pending_requests WHERE session_id = $1",
        TEST_SCHEMA
    ))
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("Failed to count pending requests");

    assert_eq!(count.0, 0);

    cleanup_test_db(&pool).await;
}

#[tokio::test]
async fn test_stale_request_cleanup() {
    let Some(pool) = setup_test_db().await else {
        return;
    };

    // Insert an old pending request (manually set created_at to past)
    sqlx::query(&format!(
        "INSERT INTO {}.pending_requests (request_id, channel_id, session_id, domain, post_id, created_at)
         VALUES ($1, $2, $3, $4, $5, NOW() - INTERVAL '25 hours')",
        TEST_SCHEMA
    ))
    .bind("old-request")
    .bind("chan-1")
    .bind("sess-1")
    .bind("old.domain.com")
    .bind("post-1")
    .execute(&pool)
    .await
    .expect("Failed to insert old request");

    // Insert a recent pending request
    sqlx::query(&format!(
        "INSERT INTO {}.pending_requests (request_id, channel_id, session_id, domain, post_id)
         VALUES ($1, $2, $3, $4, $5)",
        TEST_SCHEMA
    ))
    .bind("new-request")
    .bind("chan-2")
    .bind("sess-2")
    .bind("new.domain.com")
    .bind("post-2")
    .execute(&pool)
    .await
    .expect("Failed to insert new request");

    // Run cleanup (delete requests older than 24 hours)
    let result = sqlx::query(&format!(
        "DELETE FROM {}.pending_requests WHERE created_at < NOW() - INTERVAL '1 hour' * $1",
        TEST_SCHEMA
    ))
    .bind(24i64)
    .execute(&pool)
    .await
    .expect("Failed to cleanup");

    assert_eq!(result.rows_affected(), 1);

    // Verify only new request remains
    let remaining: Vec<(String,)> = sqlx::query_as(&format!(
        "SELECT request_id FROM {}.pending_requests",
        TEST_SCHEMA
    ))
    .fetch_all(&pool)
    .await
    .expect("Failed to fetch remaining");

    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].0, "new-request");

    cleanup_test_db(&pool).await;
}

#[tokio::test]
async fn test_unique_constraints() {
    let Some(pool) = setup_test_db().await else {
        return;
    };

    // Insert session
    sqlx::query(&format!(
        "INSERT INTO {}.sessions (session_id, channel_id, project, container_name) VALUES ($1, $2, $3, $4)",
        TEST_SCHEMA
    ))
    .bind("unique-session")
    .bind("chan-1")
    .bind("proj")
    .bind("container")
    .execute(&pool)
    .await
    .expect("Failed to insert session");

    // Try to insert duplicate session_id - should fail
    let result = sqlx::query(&format!(
        "INSERT INTO {}.sessions (session_id, channel_id, project, container_name) VALUES ($1, $2, $3, $4)",
        TEST_SCHEMA
    ))
    .bind("unique-session") // Same session_id
    .bind("chan-2")
    .bind("proj2")
    .bind("container2")
    .execute(&pool)
    .await;

    assert!(result.is_err(), "Duplicate session_id should fail");

    cleanup_test_db(&pool).await;
}
