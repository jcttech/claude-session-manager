use anyhow::Result;
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::PrometheusBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use mattermost_client::{sanitize_channel_name, Mattermost, Post};
use session_manager::config;
use session_manager::container::ContainerManager;
use session_manager::liveness::{LivenessState, format_duration_short};
use session_manager::stream_json::OutputEvent;
use session_manager::crypto::{sign_request, verify_signature};
use session_manager::database::{self, Database};
use session_manager::git::{GitManager, RepoRef};
use session_manager::opnsense::OPNsense;
use session_manager::rate_limit::{self, RateLimitLayer};
use session_manager::ssh;

/// Cached regex for network request detection (compiled once on first use)
static NETWORK_REQUEST_RE: OnceLock<Regex> = OnceLock::new();

fn network_request_regex() -> &'static Regex {
    NETWORK_REQUEST_RE.get_or_init(|| {
        Regex::new(r"\[NETWORK_REQUEST:\s*([^\]]+)\]").expect("Invalid regex pattern")
    })
}

struct AppState {
    mm: Mattermost,
    containers: ContainerManager,
    git: GitManager,
    opnsense: OPNsense,
    db: Database,
    liveness: LivenessState,
}

#[derive(Deserialize)]
struct CallbackPayload {
    context: CallbackContext,
    user_name: String,
}

#[derive(Deserialize)]
struct CallbackContext {
    action: String,
    request_id: String,
    /// HMAC signature for request validation
    signature: String,
}

#[derive(Serialize)]
struct CallbackResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    ephemeral_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    update: Option<serde_json::Value>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present (dev only, non-fatal in production)
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Initialize Prometheus metrics recorder
    let prometheus_handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install Prometheus recorder");

    // Initialize SSH key (writes to temp file if SM_VM_SSH_KEY is set)
    ssh::init_ssh_key()?;

    let s = config::settings();
    let mm = Mattermost::new(&s.mattermost_url, &s.mattermost_token).await?;
    let opnsense = OPNsense::new()?;
    let containers = ContainerManager::new();
    let git = GitManager::new();
    let db = Database::new().await?;

    let liveness = LivenessState::new();

    let state = Arc::new(AppState {
        mm,
        containers,
        git,
        opnsense,
        db,
        liveness,
    });

    // Sync container registry from database
    if let Err(e) = state.containers.registry.sync_from_db(&state.db).await {
        tracing::warn!(error = %e, "Failed to sync container registry from database");
    }

    // Reconnect any sessions that survived a restart
    match state.db.get_all_sessions().await {
        Ok(sessions) if !sessions.is_empty() => {
            tracing::info!(count = sessions.len(), "Reconnecting surviving sessions");
            for session in sessions {
                if session.project_path.is_empty() {
                    tracing::warn!(
                        session_id = %session.session_id,
                        "Skipping reconnect: no project_path stored (pre-migration session)"
                    );
                    continue;
                }
                let (output_tx, output_rx) = mpsc::channel::<OutputEvent>(100);
                // Extract repo/branch from session project for registry tracking
                let (reconnect_repo, reconnect_branch) = {
                    let parts: Vec<&str> = session.project.splitn(2, '@').collect();
                    (parts[0].to_string(), parts.get(1).unwrap_or(&"").to_string())
                };
                // Get grpc_port from registry (synced from DB on startup)
                // grpc_port=0 means pre-migration container — fall back to config default
                let reconnect_grpc_port = state.containers.registry
                    .get_container(&reconnect_repo, &reconnect_branch)
                    .await
                    .map(|e| if e.grpc_port == 0 { config::settings().grpc_port_start } else { e.grpc_port })
                    .unwrap_or(config::settings().grpc_port_start);
                state.containers.reconnect(
                    &session.session_id,
                    &session.container_name,
                    &session.project_path,
                    &reconnect_repo,
                    &reconnect_branch,
                    &session.session_type,
                    output_tx,
                    reconnect_grpc_port,
                );

                // Register for liveness tracking
                state.liveness.register(
                    &session.session_id,
                    &session.channel_id,
                    &session.thread_id,
                );

                let state_clone = state.clone();
                let channel_id = session.channel_id.clone();
                let thread_id = session.thread_id.clone();
                let session_id = session.session_id.clone();
                let session_type = session.session_type.clone();
                let project = session.project.clone();
                tokio::spawn(async move {
                    stream_output(
                        state_clone,
                        channel_id,
                        thread_id,
                        session_id,
                        session_type,
                        project,
                        output_rx,
                    ).await;
                });
            }
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "Failed to query sessions for reconnection");
        }
    }

    // Create cancellation token for graceful shutdown of background tasks
    let cancel_token = CancellationToken::new();

    // Start message listener
    let (post_tx, post_rx) = mpsc::channel::<Post>(100);
    let state_clone = state.clone();
    let cancel_clone = cancel_token.clone();
    let listener_handle = tokio::spawn(async move {
        tokio::select! {
            result = state_clone.mm.listen(post_tx) => {
                if let Err(e) = result {
                    tracing::error!(error = %e, "Message listener error");
                }
            }
            _ = cancel_clone.cancelled() => {
                tracing::info!("Message listener cancelled");
            }
        }
    });

    // Start message handler
    let state_clone = state.clone();
    let cancel_clone = cancel_token.clone();
    let handler_handle = tokio::spawn(async move {
        handle_messages(state_clone, post_rx, cancel_clone).await;
    });

    // Start periodic cleanup of stale pending requests
    let state_clone = state.clone();
    let cancel_clone = cancel_token.clone();
    let cleanup_handle = tokio::spawn(async move {
        cleanup_stale_requests(state_clone, cancel_clone).await;
    });

    // Start liveness monitor
    let state_clone = state.clone();
    let cancel_clone = cancel_token.clone();
    let liveness_handle = tokio::spawn(async move {
        spawn_liveness_monitor(state_clone, cancel_clone).await;
    });

    // Configure rate limiting
    let s = config::settings();
    let rate_limiter = RateLimitLayer::new(s.rate_limit_rps, s.rate_limit_burst);

    // Spawn cleanup task for rate limiter (also uses cancellation token)
    let cancel_clone = cancel_token.clone();
    let rate_limit_handle = rate_limit::spawn_cleanup_task(rate_limiter.clone(), cancel_clone);

    // Build router with rate limiting middleware
    let app = Router::new()
        .route("/callback", post(handle_callback))
        .route("/health", get(health_check))
        .route("/metrics", get(move || {
            let handle = prometheus_handle.clone();
            async move { handle.render() }
        }))
        .layer(rate_limiter)
        .with_state(state);

    let listen_addr = &config::settings().listen_addr;
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    tracing::info!("Listening on {}", listen_addr);
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown_signal(cancel_token.clone()))
        .await?;

    // Cancel background tasks and wait for them to complete
    tracing::info!("Cancelling background tasks...");
    cancel_token.cancel();

    // Wait for all background tasks to finish (with timeout)
    let shutdown_timeout = std::time::Duration::from_secs(10);
    let _ = tokio::time::timeout(shutdown_timeout, async {
        let _ = listener_handle.await;
        let _ = handler_handle.await;
        let _ = cleanup_handle.await;
        let _ = liveness_handle.await;
        let _ = rate_limit_handle.await;
    })
    .await;

    tracing::info!("Shutdown complete");
    Ok(())
}

/// Health check endpoint for Kubernetes probes.
/// Verifies database connectivity and returns 503 if dependencies are down.
async fn health_check(
    State(state): State<Arc<AppState>>,
) -> (axum::http::StatusCode, &'static str) {
    match state.db.health_check().await {
        Ok(()) => (axum::http::StatusCode::OK, "OK"),
        Err(_) => (axum::http::StatusCode::SERVICE_UNAVAILABLE, "Database unavailable"),
    }
}

/// Graceful shutdown signal handler
async fn shutdown_signal(cancel_token: CancellationToken) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl+c");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
        _ = cancel_token.cancelled() => {},
    }

    tracing::info!("Shutdown signal received, starting graceful shutdown");
}

/// Periodically clean up stale pending requests
async fn cleanup_stale_requests(state: Arc<AppState>, cancel_token: CancellationToken) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600)); // Every hour
    loop {
        tokio::select! {
            _ = interval.tick() => {
                match state.db.cleanup_stale_requests(24).await {
                    Ok(count) if count > 0 => {
                        tracing::info!("Cleaned up {} stale pending requests", count);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to cleanup stale requests: {}", e);
                    }
                    _ => {}
                }
            }
            _ = cancel_token.cancelled() => {
                tracing::info!("Cleanup task cancelled");
                break;
            }
        }
    }
}

/// Liveness monitor: periodically checks for sessions with no recent output
/// and posts a warning in the thread.
async fn spawn_liveness_monitor(state: Arc<AppState>, cancel_token: CancellationToken) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let timeout = config::settings().session_liveness_timeout_secs;
                if timeout == 0 {
                    continue;
                }
                for stale in state.liveness.get_stale(timeout) {
                    let msg = format!(
                        ":warning: No output activity for **{}**. Session may be unresponsive. Use `stop` to end it or wait for it to resume.",
                        format_duration_short(stale.idle_duration)
                    );
                    let _ = state.mm.post_in_thread(&stale.channel_id, &stale.thread_id, &msg).await;
                    state.liveness.mark_warned(&stale.session_id);
                    tracing::warn!(
                        session_id = %stale.session_id,
                        idle_secs = stale.idle_duration.as_secs(),
                        "Liveness warning posted"
                    );
                }
            }
            _ = cancel_token.cancelled() => {
                tracing::info!("Liveness monitor cancelled");
                break;
            }
        }
    }
}

/// Resolve a project channel: look up or create the channel for a project.
/// Also manages sidebar categories for bot and requesting user.
/// Returns (channel_id, channel_name, repo_ref).
async fn resolve_project_channel(
    state: &AppState,
    project_name: &str,
    requesting_user_id: &str,
) -> Result<(String, String, RepoRef)> {
    let s = config::settings();

    // Parse the repo reference (with default org)
    let repo_ref = RepoRef::parse_with_default_org(project_name, s.default_org.as_deref())
        .ok_or_else(|| anyhow::anyhow!(
            "Invalid repository format. Use: `org/repo`, `repo` (with default org), `org/repo@branch`, or add `--worktree`"
        ))?;

    let full_name = repo_ref.full_name();
    let channel_name = sanitize_channel_name(&repo_ref.repo);

    // Check if we already have a channel for this project
    let channel_id = if let Some(pc) = state.db.get_project_channel(&full_name).await? {
        pc.channel_id
    } else {
        // Try to find existing channel by name first
        let channel_id = match state.mm.get_channel_by_name(&s.mattermost_team_id, &channel_name).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                // Create new channel
                state.mm.create_channel(
                    &s.mattermost_team_id,
                    &channel_name,
                    &repo_ref.repo,
                    &format!("Claude sessions for {}", full_name),
                ).await?
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to look up channel, creating new one");
                state.mm.create_channel(
                    &s.mattermost_team_id,
                    &channel_name,
                    &repo_ref.repo,
                    &format!("Claude sessions for {}", full_name),
                ).await?
            }
        };

        // Persist project -> channel mapping
        state.db.create_project_channel(&full_name, &channel_id, &channel_name).await?;

        channel_id
    };

    // Manage sidebar categories (best-effort, don't fail the whole operation)
    if let Err(e) = setup_sidebar_category(state, &channel_id, requesting_user_id).await {
        tracing::warn!(error = %e, "Failed to setup sidebar category (non-fatal)");
    }

    Ok((channel_id, channel_name, repo_ref))
}

/// Setup sidebar category for all team members (best-effort per user)
async fn setup_sidebar_category(
    state: &AppState,
    channel_id: &str,
    _user_id: &str,
) -> Result<()> {
    let s = config::settings();
    let team_id = &s.mattermost_team_id;
    let category_name = &s.channel_category;

    let member_ids = state.mm.get_team_member_ids(team_id).await?;

    for member_id in &member_ids {
        if let Err(e) = async {
            let cat_id = state.mm.ensure_sidebar_category(member_id, team_id, category_name).await?;
            state.mm.add_channel_to_category(member_id, team_id, &cat_id, channel_id).await?;
            Ok::<(), anyhow::Error>(())
        }.await {
            tracing::debug!(user_id = %member_id, error = %e, "Failed to setup sidebar category for user");
        }
    }

    Ok(())
}

/// Start a session: clone/worktree repo, start container, create thread, persist to DB.
/// Returns session_id on success.
#[allow(clippy::too_many_arguments)]
async fn start_session(
    state: &Arc<AppState>,
    channel_id: &str,
    project_input: &str,
    repo_ref: &RepoRef,
    session_type: &str,
    plan_mode: bool,
    user_id: Option<&str>,
) -> Result<String> {
    use std::path::PathBuf;

    let session_id = Uuid::new_v4().to_string();
    let mut worktree_path: Option<PathBuf> = None;

    // Resolve project path (clone/worktree)
    let project_path = if repo_ref.worktree.is_some() {
        match state.git.create_worktree(repo_ref, &session_id).await {
            Ok(path) => {
                worktree_path = Some(path.clone());
                path.to_string_lossy().to_string()
            }
            Err(e) => return Err(anyhow::anyhow!("Failed to create worktree: {}", e)),
        }
    } else {
        // Using main clone - atomically try to acquire
        if let Err(existing_session) = state.git.try_acquire_repo(repo_ref, &session_id) {
            return Err(anyhow::anyhow!(
                "Repository **{}** is already in use by session `{}`.\n\
                Use `--worktree` for an isolated working directory:\n\
                `@claude start {} --worktree`",
                repo_ref.full_name(),
                &existing_session[..8.min(existing_session.len())],
                project_input
            ));
        }

        match state.git.ensure_repo(repo_ref).await {
            Ok(path) => path.to_string_lossy().to_string(),
            Err(e) => {
                state.git.release_repo_by_session(&session_id);
                return Err(anyhow::anyhow!("Failed to prepare repository: {}", e));
            }
        }
    };

    // Post root message to create thread anchor
    let root_msg = match session_type {
        "worker" => format!("**Worker session** for **{}**", repo_ref.full_name()),
        "reviewer" => format!("**Reviewer session** for **{}**", repo_ref.full_name()),
        _ => format!("**Session** for **{}**", repo_ref.full_name()),
    };
    let thread_id = state.mm.post_root(channel_id, &root_msg).await?;

    let _ = state.mm.post_in_thread(channel_id, &thread_id, "Starting session...").await;
    let session_start_time = std::time::Instant::now();

    let repo_name = repo_ref.full_name();
    let branch_name = repo_ref.branch.clone().unwrap_or_default();

    let (output_tx, output_rx) = mpsc::channel::<OutputEvent>(100);
    match state.containers.start(
        &session_id, &project_path, &repo_name, &branch_name,
        &state.db, output_tx, plan_mode, session_type,
    ).await {
        Ok(result) => {
            let session_duration = session_start_time.elapsed();
            histogram!("session_start_duration_seconds").record(session_duration.as_secs_f64());
            counter!("sessions_started_total").increment(1);
            gauge!("active_sessions").increment(1.0);

            if let Some(wt_path) = worktree_path {
                state.containers.set_worktree_path(&session_id, wt_path);
            }

            // Post same-branch warning if applicable
            if let Some(ref warning) = result.warning {
                let _ = state.mm.post_in_thread(channel_id, &thread_id, warning).await;
            }

            // Register liveness tracking
            state.liveness.register(&session_id, channel_id, &thread_id);

            // Persist session to database — if this fails, clean up everything (fix 1b)
            if let Err(e) = state.db.create_session(
                &session_id,
                channel_id,
                &thread_id,
                project_input,
                &project_path,
                &result.container_name,
                session_type,
                None, // parent_session_id reserved for future use
                user_id,
            ).await {
                tracing::error!(
                    session_id = %session_id,
                    error = %e,
                    "Failed to persist session, cleaning up container"
                );
                cleanup_session(state, &session_id).await;
                return Err(anyhow::anyhow!("Failed to persist session to database: {}", e));
            }

            // Auto-follow thread for the requesting user (fire-and-forget)
            if let Some(uid) = user_id {
                let mm = state.mm.clone();
                let uid = uid.to_string();
                let tid = thread_id.clone();
                tokio::spawn(async move {
                    if let Err(e) = mm.follow_thread(&uid, &tid).await {
                        tracing::debug!(user_id = %uid, error = %e, "Failed to auto-follow thread (non-fatal)");
                    }
                });
            }

            tracing::info!(
                session_id = %session_id,
                container = %result.container_name,
                project = %project_input,
                session_type = %session_type,
                reused = result.reused,
                "Session started"
            );

            let ready_msg = if result.reused {
                format!("Ready. Container: `{}` (reused)", result.container_name)
            } else {
                format!("Ready. Container: `{}`", result.container_name)
            };
            let _ = state.mm.post_in_thread(channel_id, &thread_id, &ready_msg).await;

            // Start output streaming
            let state_clone = state.clone();
            let channel_id_clone = channel_id.to_string();
            let thread_id_clone = thread_id.clone();
            let session_id_clone = session_id.clone();
            let session_type_clone = session_type.to_string();
            let project_clone = repo_ref.full_name();
            tokio::spawn(async move {
                stream_output(
                    state_clone,
                    channel_id_clone,
                    thread_id_clone,
                    session_id_clone,
                    session_type_clone,
                    project_clone,
                    output_rx,
                ).await;
            });

            Ok(session_id)
        }
        Err(e) => {
            state.git.release_repo_by_session(&session_id);
            Err(anyhow::anyhow!("Failed to start container: {}", e))
        }
    }
}

/// Centralized session cleanup. Uses atomic `claim_session()` as a guard:
/// whichever caller (stop command vs stream-end) claims first does cleanup;
/// the other is a no-op. This prevents double-decrement of metrics.
///
/// With multi-session containers: decrements the container's session count
/// instead of immediately removing the container. The container stays alive
/// for other sessions or until the idle timeout expires.
async fn cleanup_session(state: &AppState, session_id: &str) {
    // Atomic claim — only one caller will succeed
    let Some(claimed) = state.containers.claim_session(session_id) else {
        tracing::debug!(session_id = %session_id, "Session already cleaned up by another path");
        return;
    };

    // Remove from liveness tracking
    state.liveness.remove(session_id);

    // Unfollow thread for the user who started the session (fire-and-forget)
    if let Ok(Some(session)) = state.db.get_session_by_id_prefix(&session_id[..8.min(session_id.len())]).await && let Some(ref uid) = session.user_id {
        let mm = state.mm.clone();
        let uid = uid.clone();
        let tid = session.thread_id.clone();
        tokio::spawn(async move {
            if let Err(e) = mm.unfollow_thread(&uid, &tid).await {
                tracing::debug!(user_id = %uid, error = %e, "Failed to auto-unfollow thread (non-fatal)");
            }
        });
    }

    // Decrement container session count in registry (don't remove the container)
    match state.containers.release_session(&state.db, &claimed.repo, &claimed.branch).await {
        Ok(remaining) => {
            tracing::info!(
                session_id = %session_id,
                repo = %claimed.repo,
                branch = %claimed.branch,
                remaining_sessions = remaining,
                "Session released from container"
            );
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id, error = %e,
                "Failed to decrement container session count, falling back to container removal"
            );
            // Fallback: remove container directly if registry decrement fails
            if let Err(e) = state.containers.remove_container_by_name(&claimed.name).await {
                tracing::warn!(session_id = %session_id, error = %e, "Failed to remove container");
            }
        }
    }

    // Release repo lock
    state.git.release_repo_by_session(session_id);

    // Clean up worktree if present
    if let Some(ref wt_path) = claimed.worktree_path {
        state.git.cleanup_worktree_by_path(wt_path).await;
    }

    // Delete session from database
    if let Err(e) = state.db.delete_session(session_id).await {
        tracing::warn!(session_id = %session_id, error = %e, "Failed to delete session from database");
    }

    gauge!("active_sessions").decrement(1.0);
    tracing::info!(session_id = %session_id, "Session cleaned up");

}

/// Stop a session by ID, cleaning up container and database.
async fn stop_session(state: &Arc<AppState>, session: &database::StoredSession) {
    cleanup_session(state, &session.session_id).await;
}

/// Format a chrono::Duration as a human-readable string (e.g. "2h 15m", "5m", "30s")
fn format_duration(d: chrono::Duration) -> String {
    let total_secs = d.num_seconds().max(0);
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else if mins > 0 {
        format!("{}m", mins)
    } else {
        format!("{}s", secs)
    }
}

/// Format the root post label for a session thread (e.g. "**Session** for **org/repo**")
fn format_root_label(session_type: &str, project: &str) -> String {
    match session_type {
        "worker" => format!("**Worker session** for **{}**", project),
        "reviewer" => format!("**Reviewer session** for **{}**", project),
        _ => format!("**Session** for **{}**", project),
    }
}

async fn handle_messages(state: Arc<AppState>, mut rx: mpsc::Receiver<Post>, cancel_token: CancellationToken) {
    let bot_trigger = &config::settings().bot_trigger;

    loop {
        let post = tokio::select! {
            post = rx.recv() => {
                match post {
                    Some(p) => p,
                    None => break, // Channel closed
                }
            }
            _ = cancel_token.cancelled() => {
                tracing::info!("Message handler cancelled");
                break;
            }
        };
        let text = post.message.trim();
        let channel_id = &post.channel_id;

        if !post.root_id.is_empty() {
            // --- Thread reply: route to session by thread ---
            let root_id = &post.root_id;

            if let Ok(Some(session)) = state.db.get_session_by_thread(channel_id, root_id).await {
                // Check for in-thread commands
                if text.starts_with(bot_trigger) {
                    let cmd = text.trim_start_matches(bot_trigger).trim();
                    if cmd == "stop" {
                        stop_session(&state, &session).await;
                        let _ = state.mm.post_in_thread(channel_id, root_id, "Stopped.").await;
                        continue;
                    }
                    if cmd == "stop --container" {
                        // Find the container's repo/branch from this session
                        let container_info = state.containers.get_session_info(&session.session_id)
                            .map(|_| {
                                // Get repo/branch from the session's container entry
                                // We need to look up the session in the DashMap for repo/branch
                                let parts: Vec<&str> = session.project.splitn(2, '@').collect();
                                let repo = parts[0].to_string();
                                let branch = parts.get(1).unwrap_or(&"").to_string();
                                (repo, branch)
                            });

                        if let Some((repo, branch)) = container_info {
                            // Stop all sessions sharing this container
                            let claimed = state.containers.stop_all_sessions_for_container(&repo, &branch);
                            let count = claimed.len();

                            // Clean up each claimed session (git locks, worktrees, DB, metrics)
                            for (sid, claimed_session) in &claimed {
                                state.git.release_repo_by_session(sid);
                                if let Some(ref wt_path) = claimed_session.worktree_path {
                                    state.git.cleanup_worktree_by_path(wt_path).await;
                                }
                                if let Err(e) = state.db.delete_session(sid).await {
                                    tracing::warn!(session_id = %sid, error = %e, "Failed to delete session from database");
                                }
                                gauge!("active_sessions").decrement(1.0);
                            }

                            // Tear down the container
                            if let Err(e) = state.containers.tear_down_container(&state.db, &repo, &branch).await {
                                tracing::warn!(repo = %repo, branch = %branch, error = %e, "Failed to tear down container");
                            }

                            let _ = state.mm.post_in_thread(
                                channel_id, root_id,
                                &format!("Container stopped. {} sessions terminated.", count),
                            ).await;
                        } else {
                            let _ = state.mm.post_in_thread(channel_id, root_id, "Could not find container for this session.").await;
                        }
                        continue;
                    }
                    if cmd == "compact" {
                        let _ = state.containers.send(&session.session_id, "/compact").await;
                        let _ = state.mm.post_in_thread(channel_id, root_id, "Compacting context...").await;
                        let _ = state.db.record_compaction(&session.session_id).await;
                        continue;
                    }
                    if cmd == "clear" {
                        let _ = state.containers.send(&session.session_id, "/clear").await;
                        let _ = state.mm.post_in_thread(channel_id, root_id, "Context cleared.").await;
                        continue;
                    }
                    if cmd == "restart" {
                        let _ = state.mm.post_in_thread(channel_id, root_id, "Restarting session...").await;
                        match state.containers.restart_session(&session.session_id).await {
                            Ok(()) => {
                                let _ = state.mm.post_in_thread(channel_id, root_id, "Restarted. Next message starts a fresh conversation.").await;
                            }
                            Err(e) => {
                                let _ = state.mm.post_in_thread(channel_id, root_id, &format!("Restart failed: {e}")).await;
                            }
                        }
                        continue;
                    }
                    if cmd == "plan" || cmd.starts_with("plan ") {
                        let arg = cmd.strip_prefix("plan").unwrap().trim();
                        let new_state = match arg {
                            "on" => true,
                            "off" => false,
                            "" => !state.containers.get_plan_mode(&session.session_id),
                            _ => {
                                let _ = state.mm.post_in_thread(channel_id, root_id, "Usage: `@claude plan` (toggle), `@claude plan on`, `@claude plan off`").await;
                                continue;
                            }
                        };
                        state.containers.set_plan_mode(&session.session_id, new_state);
                        let msg = if new_state {
                            "Plan mode **enabled**. Claude will analyze but not modify files."
                        } else {
                            "Plan mode **disabled**. Claude can modify files."
                        };
                        let _ = state.mm.post_in_thread(channel_id, root_id, msg).await;
                        continue;
                    }
                    if cmd == "title" || cmd.starts_with("title ") {
                        let arg = cmd.strip_prefix("title").unwrap().trim();
                        if arg.is_empty() {
                            // Auto-generate: ask Claude to summarize
                            state.containers.set_pending_title(&session.session_id);
                            let _ = state.containers.send(
                                &session.session_id,
                                "Summarize this conversation in 5-10 words as a thread title. Output ONLY the title text, nothing else. No quotes, no punctuation at the end.",
                            ).await;
                            let _ = state.mm.post_in_thread(channel_id, root_id, "_Generating title..._").await;
                        } else {
                            // Manual title
                            let label = format_root_label(&session.session_type, &session.project);
                            let _ = state.mm.update_post(root_id, &format!("{} — {}", label, arg)).await;
                            let _ = state.mm.post_in_thread(channel_id, root_id, "Title updated.").await;
                        }
                        continue;
                    }
                    if cmd == "context" || cmd == "status" {
                        let age = chrono::Utc::now() - session.created_at;
                        let idle = chrono::Utc::now() - session.last_activity_at;
                        let info = state.containers.get_session_info(&session.session_id);
                        let plan_mode = info.as_ref().map(|i| i.plan_mode).unwrap_or(false);
                        let claude_sid = info.as_ref().and_then(|i| i.claude_session_id.as_deref().map(|s| format!("`{}`", &s[..8.min(s.len())])));

                        // Look up container info for this session's repo/branch
                        let parts: Vec<&str> = session.project.splitn(2, '@').collect();
                        let repo = parts[0];
                        let branch = parts.get(1).unwrap_or(&"");
                        let container_entry = state.containers.registry.get_container(repo, branch).await;
                        let container_line = match container_entry {
                            Some(entry) => format!(
                                "| Container | `{}` ({}, {} sessions) |",
                                entry.container_name, entry.state, entry.session_count,
                            ),
                            None => "| Container | _unknown_ |".to_string(),
                        };

                        // Liveness info
                        let liveness_info = state.liveness.get_info(&session.session_id);
                        let last_active_str = match &liveness_info {
                            Some(li) => format!("{} ago ({})", format_duration_short(li.idle_duration), li.last_event_type),
                            None => "_unknown_".to_string(),
                        };
                        let liveness_str = match &liveness_info {
                            Some(li) if li.warning_posted => ":warning: warning posted".to_string(),
                            Some(_) => "ok".to_string(),
                            None => "_untracked_".to_string(),
                        };

                        let msg = format!(
"**Session Status:**\n\
| Property | Value |\n\
|---|---|\n\
| Session | `{}` |\n\
| Claude ID | {} |\n\
| Type | {} |\n\
| Project | **{}** |\n\
{}\n\
| Messages | {} |\n\
| Compactions | {} |\n\
| Plan mode | {} |\n\
| Age | {} |\n\
| Idle | {} |\n\
| Last active | {} |\n\
| Liveness | {} |",
                            &session.session_id[..8.min(session.session_id.len())],
                            claude_sid.unwrap_or_else(|| "_none_".to_string()),
                            session.session_type,
                            session.project,
                            container_line,
                            session.message_count,
                            session.compaction_count,
                            if plan_mode { "on" } else { "off" },
                            format_duration(age),
                            format_duration(idle),
                            last_active_str,
                            liveness_str,
                        );
                        let _ = state.mm.post_in_thread(channel_id, root_id, &msg).await;
                        continue;
                    }
                }
                // Forward message to session
                tracing::info!(
                    session_id = %session.session_id,
                    text_len = text.len(),
                    "Forwarding thread message to session"
                );
                if let Err(e) = state.containers.send(&session.session_id, text).await {
                    tracing::warn!(
                        session_id = %session.session_id,
                        error = %e,
                        "Failed to forward message to container"
                    );
                }
                // Track activity
                let _ = state.db.touch_session(&session.session_id).await;
            } else if text.starts_with(bot_trigger) {
                let _ = state.mm.post_in_thread(
                    channel_id,
                    root_id,
                    "No active session in this thread.",
                ).await;
            }
            // Non-bot thread replies to non-session threads are silently ignored
        } else {
            // --- Top-level post: check for bot commands FIRST, then route ---

            // Step 1: Check for bot command trigger before any routing
            if text.starts_with(bot_trigger) {
                let cmd_text = text.trim_start_matches(bot_trigger).trim();

                // --- start <project> [--plan] ---
                if let Some(project_input) = cmd_text.strip_prefix("start ").map(|s| s.trim()) {
                    if project_input.is_empty() {
                        let _ = state.mm.post(channel_id, "Usage: `@claude start <org/repo>` or `@claude start <repo>`").await;
                        continue;
                    }

                    // Parse --plan flag from input
                    let plan_mode = project_input.split_whitespace().any(|w| w == "--plan");
                    let project_input_clean = project_input.split_whitespace()
                        .filter(|w| *w != "--plan")
                        .collect::<Vec<_>>()
                        .join(" ");
                    let project_input = project_input_clean.as_str();

                    let s = config::settings();

                    // Check if input looks like a repo reference or has a default org
                    let has_slash = project_input.split_whitespace().next()
                        .map(|p| p.split('@').next().unwrap_or(p))
                        .map(|p| p.contains('/'))
                        .unwrap_or(false);

                    if has_slash || s.default_org.is_some() || RepoRef::looks_like_repo(project_input) {
                        match resolve_project_channel(&state, project_input, &post.user_id).await {
                            Ok((proj_channel_id, channel_name, repo_ref)) => {
                                match start_session(
                                    &state,
                                    &proj_channel_id,
                                    project_input,
                                    &repo_ref,
                                    "standard",
                                    plan_mode,
                                    Some(&post.user_id),
                                ).await {
                                    Ok(session_id) => {
                                        let _ = state.mm.post(channel_id, &format!(
                                            "Session `{}` started in ~{}",
                                            &session_id[..8],
                                            channel_name,
                                        )).await;
                                    }
                                    Err(e) => {
                                        let _ = state.mm.post(channel_id, &format!("Failed: {}", e)).await;
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = state.mm.post(channel_id, &format!("Failed: {}", e)).await;
                            }
                        }
                    } else {
                        // Fall back to static projects mapping
                        let s = config::settings();
                        match s.projects.get(project_input) {
                            Some(_path) => {
                                let _ = state.mm.post(
                                    channel_id,
                                    "Static project mapping is deprecated. Use `org/repo` format or configure `SM_DEFAULT_ORG`.",
                                ).await;
                            }
                            None => {
                                let _ = state.mm.post(channel_id, &format!(
                                    "Unknown project `{}`. Use `org/repo` format or configure `SM_DEFAULT_ORG`.",
                                    project_input,
                                )).await;
                            }
                        }
                    }
                    continue;
                }

                // --- stop [short-id | --all | --container <id>] ---
                if cmd_text == "stop" || cmd_text.starts_with("stop ") {
                    let short_id = cmd_text.strip_prefix("stop").unwrap().trim();

                    if short_id == "--all" {
                        // Stop all sessions and tear down all containers
                        let containers = state.containers.registry.list_all().await;
                        let mut total_sessions = 0;

                        for ((repo, branch), _entry) in &containers {
                            let claimed = state.containers.stop_all_sessions_for_container(repo, branch);
                            total_sessions += claimed.len();

                            // Clean up each claimed session
                            for (sid, claimed_session) in &claimed {
                                state.git.release_repo_by_session(sid);
                                if let Some(ref wt_path) = claimed_session.worktree_path {
                                    state.git.cleanup_worktree_by_path(wt_path).await;
                                }
                                if let Err(e) = state.db.delete_session(sid).await {
                                    tracing::warn!(session_id = %sid, error = %e, "Failed to delete session from database");
                                }
                                gauge!("active_sessions").decrement(1.0);
                            }

                            // Tear down the container
                            if let Err(e) = state.containers.tear_down_container(&state.db, repo, branch).await {
                                tracing::warn!(repo = %repo, branch = %branch, error = %e, "Failed to tear down container");
                            }
                        }

                        let _ = state.mm.post(channel_id, &format!(
                            "All sessions and containers stopped. ({} sessions, {} containers)",
                            total_sessions, containers.len(),
                        )).await;
                    } else if short_id.is_empty() {
                        // No short-id: show help
                        let _ = state.mm.post(channel_id, "Usage: `@claude stop <session-id-prefix>`, `@claude stop --all`, or reply `@claude stop` in a session thread.").await;
                    } else {
                        // Stop by ID prefix
                        match state.db.get_session_by_id_prefix(short_id).await {
                            Ok(Some(session)) => {
                                stop_session(&state, &session).await;
                                let _ = state.mm.post(channel_id, &format!(
                                    "Stopped session `{}`.",
                                    &session.session_id[..8]
                                )).await;
                            }
                            Ok(None) => {
                                let _ = state.mm.post(channel_id, &format!(
                                    "No session found matching `{}`.",
                                    short_id
                                )).await;
                            }
                            Err(e) => {
                                let _ = state.mm.post(channel_id, &format!("Error: {}", e)).await;
                            }
                        }
                    }
                    continue;
                }

                // --- status ---
                if cmd_text == "status" {
                    match state.db.get_all_sessions().await {
                        Ok(sessions) if sessions.is_empty() => {
                            let _ = state.mm.post(channel_id, "No active sessions.").await;
                        }
                        Ok(sessions) => {
                            let now = chrono::Utc::now();

                            // Show container info first
                            let containers = state.containers.registry.list_all().await;
                            let mut msg = String::new();
                            if !containers.is_empty() {
                                msg.push_str("**Containers:**\n");
                                for ((_repo, _branch), entry) in &containers {
                                    msg.push_str(&format!(
                                        "- Container: `{}` ({}, {} sessions)\n",
                                        entry.container_name,
                                        entry.state,
                                        entry.session_count,
                                    ));
                                }
                                msg.push('\n');
                            }

                            msg.push_str("**Active Sessions:**\n");
                            for s in &sessions {
                                let idle = now - s.last_activity_at;
                                msg.push_str(&format!(
                                    "- `{}` | {} | **{}** | {} msgs | {} compactions | idle {}\n",
                                    &s.session_id[..8],
                                    s.session_type,
                                    s.project,
                                    s.message_count,
                                    s.compaction_count,
                                    format_duration(idle),
                                ));
                            }
                            let _ = state.mm.post(channel_id, &msg).await;
                        }
                        Err(e) => {
                            let _ = state.mm.post(channel_id, &format!("Error: {}", e)).await;
                        }
                    }
                    continue;
                }

                // --- help ---
                if cmd_text == "help" {
                    let _ = state.mm.post(channel_id, &format!(
                        "**Commands:**\n\
                        - `{trigger} start <org/repo>` — Start a standard session\n\
                        - `{trigger} start <repo> --worktree` — Start with isolated worktree\n\
                        - `{trigger} start <repo> --plan` — Start in plan mode (read-only analysis)\n\
                        - `{trigger} stop <id-prefix>` — Stop a session by ID prefix\n\
                        - `{trigger} stop --all` — Stop all sessions and tear down all containers\n\
                        - `{trigger} status` — List all active sessions and containers\n\
                        - `{trigger} help` — Show this message\n\
                        \n\
                        **In a session thread:**\n\
                        - Reply directly to send input\n\
                        - `{trigger} stop` — End the session\n\
                        - `{trigger} stop --container` — Stop all sessions sharing this container and tear it down\n\
                        - `{trigger} compact` — Compact/summarize context\n\
                        - `{trigger} clear` — Clear conversation history\n\
                        - `{trigger} restart` — Restart Claude conversation\n\
                        - `{trigger} plan` — Toggle plan mode (read-only analysis)\n\
                        - `{trigger} title [text]` — Set thread title (auto-generate if no text)\n\
                        - `{trigger} status` — Show session status and context health",
                        trigger = bot_trigger,
                    )).await;
                    continue;
                }

                // Unknown command
                let _ = state.mm.post(channel_id, &format!(
                    "Unknown command. Try `{} help`.",
                    bot_trigger,
                )).await;
            } else {
                // Step 2: Non-command top-level message — route to active session in this channel
                match state.db.get_non_worker_sessions_by_channel(channel_id).await {
                    Ok(sessions) if sessions.len() == 1 => {
                        // Exactly one session — forward the message to it
                        let session = &sessions[0];
                        if let Err(e) = state.containers.send(&session.session_id, text).await {
                            tracing::warn!(
                                session_id = %session.session_id,
                                error = %e,
                                "Failed to forward top-level message to session"
                            );
                        }
                        let _ = state.db.touch_session(&session.session_id).await;
                    }
                    Ok(sessions) if sessions.len() > 1 => {
                        // Multiple sessions — guide user to reply in thread
                        let _ = state.mm.post(
                            channel_id,
                            "Multiple sessions active in this channel. Please reply in the specific session thread.",
                        ).await;
                    }
                    _ => {
                        // No sessions or error — silently ignore non-bot top-level messages
                    }
                }
            }
        }
    }
}

/// Flush accumulated output lines as a single Mattermost message.
async fn flush_batch(
    state: &AppState,
    channel_id: &str,
    thread_id: &str,
    session_id: &str,
    batch: &mut Vec<String>,
) {
    if batch.is_empty() {
        return;
    }
    let content = batch.join("\n");
    batch.clear();
    if let Err(e) = state.mm.post_in_thread(channel_id, thread_id, &content).await {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "Failed to post batched output to Mattermost"
        );
    }
}

/// Maximum batch size in bytes before flushing (14KB safety margin under Mattermost's 16KB limit)
const BATCH_MAX_BYTES: usize = 14 * 1024;
/// Maximum number of lines before flushing
const BATCH_MAX_LINES: usize = 80;
/// Batch timeout before flushing accumulated output
const BATCH_TIMEOUT: Duration = Duration::from_millis(200);

#[allow(clippy::too_many_arguments)]
async fn stream_output(
    state: Arc<AppState>,
    channel_id: String,
    thread_id: String,
    session_id: String,
    session_type: String,
    project: String,
    mut rx: mpsc::Receiver<OutputEvent>,
) {
    let network_re = network_request_regex();

    let mut batch: Vec<String> = Vec::new();
    let mut batch_bytes: usize = 0;
    let batch_timer = tokio::time::sleep(BATCH_TIMEOUT);
    tokio::pin!(batch_timer);

    // Single status post that accumulates processing info and tool actions
    let mut status_post_id: Option<String> = None;
    let mut status_lines: Vec<String> = Vec::new();

    loop {
        tokio::select! {
            event_opt = rx.recv() => {
                let Some(event) = event_opt else {
                    // Channel closed — flush remaining batch and exit
                    flush_batch(&state, &channel_id, &thread_id, &session_id, &mut batch).await;
                    break;
                };

                match event {
                    OutputEvent::ProcessingStarted { input_tokens } => {
                        state.liveness.update_activity(&session_id, "ProcessingStarted");
                        // Start a new status post (reset from previous turn)
                        status_lines.clear();
                        status_lines.push(format!("_Processing... (context: {} tokens)_", input_tokens));
                        let msg = status_lines.join("\n");
                        match state.mm.post_in_thread(&channel_id, &thread_id, &msg).await {
                            Ok(post_id) => { status_post_id = Some(post_id); }
                            Err(_) => { status_post_id = None; }
                        }
                    }
                    OutputEvent::TextLine(line) => {
                        state.liveness.update_activity(&session_id, "TextLine");
                        // Check for markers — flush batch before processing
                        if let Some(caps) = network_re.captures(&line) {
                            flush_batch(&state, &channel_id, &thread_id, &session_id, &mut batch).await;
                            batch_bytes = 0;
                            let domain = caps[1].trim();
                            handle_network_request(&state, &channel_id, &thread_id, &session_id, domain).await;
                            continue;
                        }

                        // Accumulate line into batch
                        batch_bytes += line.len() + 1; // +1 for newline separator
                        batch.push(line);

                        // Flush if batch exceeds size or line limits
                        if batch_bytes >= BATCH_MAX_BYTES || batch.len() >= BATCH_MAX_LINES {
                            flush_batch(&state, &channel_id, &thread_id, &session_id, &mut batch).await;
                            batch_bytes = 0;
                        }

                        // Reset timer on each new line
                        batch_timer.as_mut().reset(tokio::time::Instant::now() + BATCH_TIMEOUT);
                    }
                    OutputEvent::ToolAction(action) => {
                        state.liveness.update_activity(&session_id, "ToolAction");
                        // Flush any accumulated text batch before updating status
                        flush_batch(&state, &channel_id, &thread_id, &session_id, &mut batch).await;
                        batch_bytes = 0;
                        // Append tool action to the status post
                        status_lines.push(format!("> {}", action));
                        let msg = status_lines.join("\n");
                        if let Some(ref post_id) = status_post_id {
                            let _ = state.mm.update_post(post_id, &msg).await;
                        } else {
                            // No status post yet — create one
                            if let Ok(post_id) = state.mm.post_in_thread(&channel_id, &thread_id, &msg).await {
                                status_post_id = Some(post_id);
                            }
                        }
                    }
                    OutputEvent::TitleGenerated(title) => {
                        state.liveness.update_activity(&session_id, "TitleGenerated");
                        flush_batch(&state, &channel_id, &thread_id, &session_id, &mut batch).await;
                        batch_bytes = 0;
                        let title = title.trim().trim_matches('"');
                        let label = format_root_label(&session_type, &project);
                        let _ = state.mm.update_post(&thread_id, &format!("{} — {}", label, title)).await;
                        let _ = state.mm.post_in_thread(&channel_id, &thread_id, "Title updated.").await;
                    }
                    OutputEvent::ResponseComplete { input_tokens, output_tokens } => {
                        state.liveness.update_activity(&session_id, "ResponseComplete");
                        flush_batch(&state, &channel_id, &thread_id, &session_id, &mut batch).await;
                        batch_bytes = 0;
                        counter!("tokens_input_total").increment(input_tokens);
                        counter!("tokens_output_total").increment(output_tokens);
                        // Warn when context window is getting full (>80% of 200k)
                        let usage_pct = (input_tokens as f64 / 200_000.0 * 100.0) as u64;
                        if input_tokens > 160_000 {
                            let msg = format!(
                                ":warning: **Context window {}% full** ({} / 200k tokens) — consider using `compact` or `clear`",
                                usage_pct, input_tokens
                            );
                            let _ = state.mm.post_in_thread(&channel_id, &thread_id, &msg).await;
                        }
                    }
                    OutputEvent::ProcessDied { exit_code, signal } => {
                        flush_batch(&state, &channel_id, &thread_id, &session_id, &mut batch).await;
                        batch_bytes = 0;
                        let code_str = exit_code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "unknown".to_string());
                        let msg = match signal {
                            Some(ref sig) => format!(
                                ":warning: Session process died unexpectedly (exit code {}, {}). Use `start` to begin a new session.",
                                code_str, sig
                            ),
                            None => format!(
                                ":warning: Session process died unexpectedly (exit code {}). Use `start` to begin a new session.",
                                code_str
                            ),
                        };
                        // Fire-and-forget notification
                        let _ = state.mm.post_in_thread(&channel_id, &thread_id, &msg).await;
                    }
                }
            }
            _ = &mut batch_timer => {
                // Timer expired — flush accumulated batch
                flush_batch(&state, &channel_id, &thread_id, &session_id, &mut batch).await;
                batch_bytes = 0;
                // Reset timer far into the future (only re-armed when lines arrive)
                batch_timer.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(86400));
            }
        }
    }

    // Stream ended — use centralized cleanup (atomic, prevents double-decrement)
    cleanup_session(&state, &session_id).await;

    tracing::info!(
        session_id = %session_id,
        channel_id = %channel_id,
        "Session stream ended"
    );
    if let Err(e) = state.mm.post_in_thread(&channel_id, &thread_id, "Session ended.").await {
        tracing::warn!(error = %e, "Failed to post session end message");
    }
}

async fn handle_network_request(
    state: &AppState,
    channel_id: &str,
    thread_id: &str,
    session_id: &str,
    domain: &str,
) {
    let start_time = std::time::Instant::now();

    // Deduplicate: check for existing pending request for same domain in session
    match state.db.get_pending_request_by_domain_and_session(domain, session_id).await {
        Ok(Some(existing)) => {
            tracing::debug!(
                domain = %domain,
                session_id = %session_id,
                existing_request_id = %existing.request_id,
                "Skipping duplicate network request - already pending"
            );
            counter!("network_requests_deduplicated_total").increment(1);
            return;
        }
        Err(e) => {
            tracing::warn!(
                domain = %domain,
                session_id = %session_id,
                error = %e,
                "Failed to check for duplicate request, proceeding anyway"
            );
        }
        Ok(None) => {}
    }

    let request_id = Uuid::new_v4().to_string();
    let s = config::settings();

    counter!("network_requests_total").increment(1);
    tracing::info!(
        request_id = %request_id,
        session_id = %session_id,
        domain = %domain,
        "Network request received, awaiting approval"
    );

    let approve_sig = sign_request(&s.callback_secret, &request_id, "approve");
    let deny_sig = sign_request(&s.callback_secret, &request_id, "deny");

    let props = serde_json::json!({
        "attachments": [{
            "color": "#FFA500",
            "text": format!("**Network Request:** `{}`", domain),
            "actions": [
                {
                    "id": "approve",
                    "name": "Approve",
                    "integration": {
                        "url": s.callback_url,
                        "context": {
                            "action": "approve",
                            "request_id": request_id,
                            "signature": approve_sig
                        }
                    }
                },
                {
                    "id": "deny",
                    "name": "Deny",
                    "integration": {
                        "url": s.callback_url,
                        "context": {
                            "action": "deny",
                            "request_id": request_id,
                            "signature": deny_sig
                        }
                    }
                }
            ]
        }]
    });

    match state.mm.post_with_props(channel_id, thread_id, "", props).await {
        Ok(post_id) => {
            if let Err(e) = state.db.create_pending_request(
                &request_id,
                channel_id,
                thread_id,
                session_id,
                domain,
                &post_id,
            ).await {
                tracing::error!("Failed to persist pending request: {}", e);
            }
        }
        Err(e) => {
            tracing::error!("Failed to post approval: {}", e);
        }
    }

    let duration = start_time.elapsed();
    histogram!("network_request_duration_seconds").record(duration.as_secs_f64());
}

async fn handle_callback(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CallbackPayload>,
) -> Json<CallbackResponse> {
    let start_time = std::time::Instant::now();
    let request_id = &payload.context.request_id;
    let action = &payload.context.action;
    let signature = &payload.context.signature;
    let s = config::settings();

    // Check if user is authorized to approve/deny requests
    if !s.allowed_approvers.is_empty() && !s.allowed_approvers.contains(&payload.user_name) {
        tracing::warn!(
            user = %payload.user_name,
            request_id = %request_id,
            "Unauthorized user attempted to process request"
        );
        return Json(CallbackResponse {
            ephemeral_text: Some("You are not authorized to approve or deny requests.".into()),
            update: None,
        });
    }

    // Verify HMAC signature
    if !verify_signature(&s.callback_secret, request_id, action, signature) {
        tracing::warn!(
            "Invalid signature for request_id={} action={} from user={}",
            request_id, action, payload.user_name
        );
        return Json(CallbackResponse {
            ephemeral_text: Some("Invalid signature. Request rejected.".into()),
            update: None,
        });
    }

    // Retrieve pending request
    let req = match state.db.get_pending_request(request_id).await {
        Ok(Some(req)) => req,
        Ok(None) => {
            return Json(CallbackResponse {
                ephemeral_text: Some("Request expired or already processed.".into()),
                update: None,
            });
        }
        Err(e) => {
            tracing::error!("Database error: {}", e);
            return Json(CallbackResponse {
                ephemeral_text: Some("Internal error.".into()),
                update: None,
            });
        }
    };

    if action == "approve" {
        match state.opnsense.add_domain(&req.domain).await {
            Ok(_added) => {
                if let Err(e) = state.db.delete_pending_request(request_id).await {
                    tracing::error!("Failed to delete pending request: {}", e);
                }
                if let Err(e) = state.db.log_approval(request_id, &req.domain, action, &payload.user_name).await {
                    tracing::error!("Failed to log approval: {}", e);
                }
                if let Err(e) = state.mm.update_post(&req.post_id, &format!("`{}` approved by @{}", req.domain, payload.user_name)).await {
                    tracing::warn!(error = %e, "Failed to update Mattermost post");
                }
                if let Err(e) = state.containers.send(&req.session_id, &format!("[NETWORK_APPROVED: {}]", req.domain)).await {
                    tracing::warn!(error = %e, "Failed to notify container");
                }
                counter!("approvals_total", "action" => "approve").increment(1);
                tracing::info!(
                    request_id = %request_id,
                    domain = %req.domain,
                    user = %payload.user_name,
                    session_id = %req.session_id,
                    "Domain approved"
                );
            }
            Err(e) => {
                tracing::error!(
                    request_id = %request_id,
                    domain = %req.domain,
                    error = %e,
                    "Failed to add domain to OPNsense - request NOT approved"
                );
                return Json(CallbackResponse {
                    ephemeral_text: Some(format!("Failed to add domain to firewall: {}. Please try again.", e)),
                    update: None,
                });
            }
        }
    } else {
        if let Err(e) = state.db.delete_pending_request(request_id).await {
            tracing::error!("Failed to delete pending request: {}", e);
        }
        if let Err(e) = state.db.log_approval(request_id, &req.domain, action, &payload.user_name).await {
            tracing::error!("Failed to log denial: {}", e);
        }
        if let Err(e) = state.mm.update_post(&req.post_id, &format!("`{}` denied by @{}", req.domain, payload.user_name)).await {
            tracing::warn!(error = %e, "Failed to update Mattermost post");
        }
        if let Err(e) = state.containers.send(&req.session_id, &format!("[NETWORK_DENIED: {}]", req.domain)).await {
            tracing::warn!(error = %e, "Failed to notify container");
        }
        counter!("approvals_total", "action" => "deny").increment(1);
        tracing::info!(
            request_id = %request_id,
            domain = %req.domain,
            user = %payload.user_name,
            session_id = %req.session_id,
            "Domain denied"
        );
    }

    let duration = start_time.elapsed();
    histogram!("callback_duration_seconds").record(duration.as_secs_f64());

    Json(CallbackResponse {
        ephemeral_text: None,
        update: Some(serde_json::json!({ "message": "" })),
    })
}

