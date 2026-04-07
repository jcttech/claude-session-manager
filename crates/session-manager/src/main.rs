use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use dashmap::DashMap;
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::PrometheusBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use mattermost_client::{Mattermost, Post, sanitize_channel_name};
use session_manager::config;
use session_manager::container::ContainerManager;
use session_manager::crypto::{sign_request, verify_signature};
use session_manager::database::{self, Database, StoredSession, StoredTeamRole};
use session_manager::git::{GitManager, RepoRef};
use session_manager::liveness::{LivenessState, format_duration_short};
use session_manager::opnsense::OPNsense;
use session_manager::rate_limit::{self, RateLimitLayer};
use session_manager::ssh;
use session_manager::stream_json::OutputEvent;

const APP_VERSION: &str = env!("APP_VERSION");

/// Cached regex for network request detection (compiled once on first use)
static NETWORK_REQUEST_RE: OnceLock<Regex> = OnceLock::new();
/// Team marker regexes (compiled once on first use)
static TEAM_SPAWN_RE: OnceLock<Regex> = OnceLock::new();
static TEAM_MSG_RE: OnceLock<Regex> = OnceLock::new();
static TEAM_BROADCAST_RE: OnceLock<Regex> = OnceLock::new();
static TEAM_MSG_END_RE: OnceLock<Regex> = OnceLock::new();
static TEAM_STATUS_RE: OnceLock<Regex> = OnceLock::new();
static PR_READY_RE: OnceLock<Regex> = OnceLock::new();
static PR_REVIEWED_RE: OnceLock<Regex> = OnceLock::new();

fn network_request_regex() -> &'static Regex {
    NETWORK_REQUEST_RE.get_or_init(|| {
        Regex::new(r"\[NETWORK_REQUEST:\s*([^\]]+)\]").expect("Invalid regex pattern")
    })
}

fn team_spawn_regex() -> &'static Regex {
    // Anchored to start-of-line: markers must be the first thing on the line
    // (with optional leading whitespace). Mid-line markers are ignored so they
    // don't swallow surrounding text or enter accumulation mode unexpectedly.
    TEAM_SPAWN_RE.get_or_init(|| {
        Regex::new(r"^\s*\[SPAWN:([^\]]+?)(?:\s+--worktree)?\]\s*(.*)").expect("Invalid regex")
    })
}

fn team_msg_regex() -> &'static Regex {
    TEAM_MSG_RE.get_or_init(|| Regex::new(r"^\s*\[TO:([^\]]+)\]\s*(.*)").expect("Invalid regex"))
}

fn team_broadcast_regex() -> &'static Regex {
    TEAM_BROADCAST_RE
        .get_or_init(|| Regex::new(r"^\s*\[BROADCAST\]\s*(.*)").expect("Invalid regex"))
}

fn team_msg_end_regex() -> &'static Regex {
    // [/MSG] is also anchored — must be on its own line (with optional whitespace).
    // This prevents false matches when [/MSG] appears inside quoted text or code blocks.
    TEAM_MSG_END_RE.get_or_init(|| Regex::new(r"^\s*\[/MSG\]\s*$").expect("Invalid regex"))
}

fn team_status_regex() -> &'static Regex {
    TEAM_STATUS_RE.get_or_init(|| Regex::new(r"\[TEAM_STATUS\]").expect("Invalid regex"))
}

fn pr_ready_regex() -> &'static Regex {
    PR_READY_RE.get_or_init(|| Regex::new(r"^\s*\[PR_READY:(\d+)\]").expect("Invalid regex"))
}

fn pr_reviewed_regex() -> &'static Regex {
    PR_REVIEWED_RE.get_or_init(|| Regex::new(r"^\s*\[PR_REVIEWED:(\d+)\]").expect("Invalid regex"))
}

struct AppState {
    mm: Mattermost,
    containers: ContainerManager,
    git: GitManager,
    opnsense: OPNsense,
    db: Database,
    liveness: LivenessState,
    /// Per-team mutex to serialize spawn operations (prevents race conditions)
    spawn_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Shared coordination channel ID for team coordination logs
    coordination_channel_id: tokio::sync::RwLock<Option<String>>,
    /// PRs that have passed the [PR_REVIEWED] gate, keyed by "team_id:pr_number".
    /// Only PRs in this set should be merged by the Team Lead.
    reviewed_prs: DashMap<String, ()>,
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

    let coordination_channel_id = s.coordination_channel_id.clone();
    let state = Arc::new(AppState {
        mm,
        containers,
        git,
        opnsense,
        db,
        liveness,
        spawn_locks: DashMap::new(),
        coordination_channel_id: tokio::sync::RwLock::new(coordination_channel_id),
        reviewed_prs: DashMap::new(),
    });

    // Sync container registry from database
    if let Err(e) = state.containers.registry.sync_from_db(&state.db).await {
        tracing::warn!(error = %e, "Failed to sync container registry from database");
    }

    // After VM reboot, containers may exist but be stopped at the podman level.
    // Probe each "running" container and restart it if needed, or evict from registry if gone.
    {
        let containers = state.containers.registry.list_all().await;
        for ((repo, branch), entry) in &containers {
            match session_manager::container::ensure_container_started(&entry.container_name).await
            {
                Ok(restarted) => {
                    if restarted {
                        tracing::info!(
                            repo = %repo, branch = %branch,
                            container = %entry.container_name,
                            "Restarted stopped container on startup (VM reboot recovery)"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        repo = %repo, branch = %branch,
                        container = %entry.container_name,
                        error = %e,
                        "Container no longer exists, removing from registry"
                    );
                    state
                        .containers
                        .registry
                        .remove_container(&state.db, repo, branch)
                        .await
                        .ok();
                }
            }
        }
    }

    // Evict orphaned CSM-managed devcontainers that exist at the podman level but are not in
    // our registry. These ghosts cause `devcontainer up` to hang on cold start because the CLI
    // tries to reuse them even though our registry doesn't know about them.
    // Only removes containers with the csm.managed=true label — manually-created devcontainers
    // are left untouched.
    {
        let s = session_manager::config::settings();
        let runtime = &s.container_runtime;
        let known_names: std::collections::HashSet<String> = state
            .containers
            .registry
            .list_all()
            .await
            .iter()
            .map(|(_, e)| e.container_name.clone())
            .collect();
        let list_cmd = format!(
            "{runtime} ps -a --filter label=csm.managed=true --format '{{{{.ID}}}} {{{{.Names}}}}'"
        );
        if let Ok(output) = session_manager::ssh::run_command(&list_cmd).await {
            for line in output.lines() {
                let parts: Vec<&str> = line.trim().splitn(2, ' ').collect();
                if parts.len() < 2 {
                    continue;
                }
                let (id, name) = (parts[0], parts[1]);
                if !known_names.contains(name) && !known_names.contains(id) {
                    tracing::warn!(
                        container_id = %id, container_name = %name,
                        "Removing orphaned devcontainer not in registry"
                    );
                    let _ =
                        session_manager::ssh::run_command(&format!("{runtime} rm -f {id}")).await;
                }
            }
        }
    }

    // Resync session counts — correct any drift between registry counts and actual active sessions
    if let Err(e) = state
        .containers
        .registry
        .resync_session_counts(&state.db)
        .await
    {
        tracing::warn!(error = %e, "Failed to resync session counts on startup");
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
                    (
                        parts[0].to_string(),
                        parts.get(1).unwrap_or(&"").to_string(),
                    )
                };
                // Get grpc_port from registry (synced from DB on startup)
                // grpc_port=0 means pre-migration container — fall back to config default
                let reconnect_grpc_port = state
                    .containers
                    .registry
                    .get_container(&reconnect_repo, &reconnect_branch)
                    .await
                    .map(|e| {
                        if e.grpc_port == 0 {
                            config::settings().grpc_port_start
                        } else {
                            e.grpc_port
                        }
                    })
                    .unwrap_or(config::settings().grpc_port_start);
                // Reconstruct system prompt for team sessions
                let reconnect_prompt = rebuild_system_prompt(
                    &state.db,
                    &session.session_type,
                    session.team_id.as_deref(),
                    session.role.as_deref(),
                )
                .await;

                if let Err(e) = state
                    .containers
                    .reconnect(
                        &session.session_id,
                        &session.container_name,
                        &session.project_path,
                        &reconnect_repo,
                        &reconnect_branch,
                        &session.session_type,
                        &state.db,
                        output_tx,
                        reconnect_grpc_port,
                        reconnect_prompt.as_deref(),
                        session.claude_session_id.as_deref(),
                    )
                    .await
                {
                    tracing::warn!(
                        session_id = %session.session_id,
                        error = %e,
                        "Failed to reconnect session (container may be gone)"
                    );
                    // Mark the session stopped so the DB doesn't remain stale — if we
                    // leave it as 'active' any incoming message on this thread will hit
                    // containers.send() and get "not found in-memory".
                    let _ = state
                        .db
                        .update_session_status(&session.session_id, "stopped")
                        .await;
                    continue;
                }

                // Register for liveness tracking
                state.liveness.register(&session.session_id);

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
                    )
                    .await;
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

    // Spawn session watchdog (stuck detection + automation nudging)
    let watchdog_state = state.clone();
    let _watchdog_handle = tokio::spawn(async move {
        session_watchdog(watchdog_state).await;
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
        .route(
            "/metrics",
            get(move || {
                let handle = prometheus_handle.clone();
                async move { handle.render() }
            }),
        )
        .layer(rate_limiter)
        .with_state(state);

    let listen_addr = &config::settings().listen_addr;
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    tracing::info!(version = APP_VERSION, "Listening on {}", listen_addr);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
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
        Err(_) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Database unavailable",
        ),
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
        let channel_id = match state
            .mm
            .get_channel_by_name(&s.mattermost_team_id, &channel_name)
            .await
        {
            Ok(Some(id)) => id,
            Ok(None) => {
                // Create new channel
                state
                    .mm
                    .create_channel(
                        &s.mattermost_team_id,
                        &channel_name,
                        &repo_ref.repo,
                        &format!("Claude sessions for {}", full_name),
                    )
                    .await?
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to look up channel, creating new one");
                state
                    .mm
                    .create_channel(
                        &s.mattermost_team_id,
                        &channel_name,
                        &repo_ref.repo,
                        &format!("Claude sessions for {}", full_name),
                    )
                    .await?
            }
        };

        // Persist project -> channel mapping
        state
            .db
            .create_project_channel(&full_name, &channel_id, &channel_name)
            .await?;

        channel_id
    };

    // Manage sidebar categories (best-effort, don't fail the whole operation)
    if let Err(e) = setup_sidebar_category(state, &channel_id, requesting_user_id).await {
        tracing::warn!(error = %e, "Failed to setup sidebar category (non-fatal)");
    }

    Ok((channel_id, channel_name, repo_ref))
}

/// Setup sidebar category for all team members (best-effort per user)
async fn setup_sidebar_category(state: &AppState, channel_id: &str, _user_id: &str) -> Result<()> {
    let s = config::settings();
    let team_id = &s.mattermost_team_id;
    let category_name = &s.channel_category;

    let member_ids = state.mm.get_team_member_ids(team_id).await?;

    for member_id in &member_ids {
        if let Err(e) = async {
            let cat_id = state
                .mm
                .ensure_sidebar_category(member_id, team_id, category_name)
                .await?;
            state
                .mm
                .add_channel_to_category(member_id, team_id, &cat_id, channel_id)
                .await?;
            Ok::<(), anyhow::Error>(())
        }
        .await
        {
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
    repo_ref: &RepoRef,
    session_type: &str,
    plan_mode: bool,
    thinking_mode: bool,
    user_id: Option<&str>,
    team_id: Option<&str>,
    role: Option<&str>,
    system_prompt: Option<&str>,
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
                repo_ref.full_name()
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
    let root_msg = format_root_label(session_type, &repo_ref.full_name(), role);
    let thread_id = state.mm.post_root(channel_id, &root_msg).await?;

    let _ = state
        .mm
        .post_in_thread(channel_id, &thread_id, "Starting session...")
        .await;
    let session_start_time = std::time::Instant::now();

    let repo_name = repo_ref.full_name();
    let branch_name = repo_ref.branch.clone().unwrap_or_default();

    let (output_tx, output_rx) = mpsc::channel::<OutputEvent>(100);
    match state
        .containers
        .start(
            &session_id,
            &project_path,
            &repo_name,
            &branch_name,
            &state.db,
            output_tx,
            plan_mode,
            thinking_mode,
            session_type,
            system_prompt,
        )
        .await
    {
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
                let _ = state
                    .mm
                    .post_in_thread(channel_id, &thread_id, warning)
                    .await;
            }

            // Register liveness tracking
            state.liveness.register(&session_id);

            // Persist session to database — if this fails, clean up everything (fix 1b)
            if let Err(e) = state
                .db
                .create_session(
                    &session_id,
                    channel_id,
                    &thread_id,
                    &repo_ref.full_name(),
                    &project_path,
                    &result.container_name,
                    session_type,
                    None, // parent_session_id reserved for future use
                    user_id,
                )
                .await
            {
                tracing::error!(
                    session_id = %session_id,
                    error = %e,
                    "Failed to persist session, cleaning up container"
                );
                cleanup_session(state, &session_id).await;
                return Err(anyhow::anyhow!(
                    "Failed to persist session to database: {}",
                    e
                ));
            }

            // Link session to team if applicable
            if let (Some(tid), Some(r)) = (team_id, role)
                && let Err(e) = state.db.set_session_team(&session_id, tid, r).await
            {
                tracing::warn!(session_id = %session_id, error = %e, "Failed to link session to team");
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
                project = %repo_ref.full_name(),
                session_type = %session_type,
                reused = result.reused,
                "Session started"
            );

            let ready_msg = if result.reused {
                format!(
                    "Container attached: `{}` (reused). Session ready.",
                    result.container_name
                )
            } else {
                format!(
                    "Container attached: `{}`. Session ready.",
                    result.container_name
                )
            };
            let _ = state
                .mm
                .post_in_thread(channel_id, &thread_id, &ready_msg)
                .await;

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
                )
                .await;
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
    if let Ok(Some(session)) = state
        .db
        .get_session_by_id_prefix(&session_id[..8.min(session_id.len())])
        .await
        && let Some(ref uid) = session.user_id
    {
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
    match state
        .containers
        .release_session(&state.db, &claimed.repo, &claimed.branch)
        .await
    {
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
            if let Err(e) = state
                .containers
                .remove_container_by_name(&claimed.name)
                .await
            {
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

/// Stop a session by ID — soft-delete (mark as "stopped" in DB) so it can be resumed.
/// Still removes from in-memory state, decrements container session count, releases repo lock.
async fn stop_session(state: &Arc<AppState>, session: &database::StoredSession) {
    // Atomic claim — only one caller will succeed
    let Some(claimed) = state.containers.claim_session(&session.session_id) else {
        tracing::debug!(session_id = %session.session_id, "Session already cleaned up by another path");
        return;
    };

    // Remove from liveness tracking
    state.liveness.remove(&session.session_id);

    // Unfollow thread for the user
    if let Some(ref uid) = session.user_id {
        let mm = state.mm.clone();
        let uid = uid.clone();
        let tid = session.thread_id.clone();
        tokio::spawn(async move {
            if let Err(e) = mm.unfollow_thread(&uid, &tid).await {
                tracing::debug!(user_id = %uid, error = %e, "Failed to auto-unfollow thread (non-fatal)");
            }
        });
    }

    // Decrement container session count
    match state
        .containers
        .release_session(&state.db, &claimed.repo, &claimed.branch)
        .await
    {
        Ok(remaining) => {
            tracing::info!(
                session_id = %session.session_id,
                repo = %claimed.repo,
                branch = %claimed.branch,
                remaining_sessions = remaining,
                "Session released from container"
            );
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session.session_id, error = %e,
                "Failed to decrement container session count"
            );
        }
    }

    // Release repo lock
    state.git.release_repo_by_session(&session.session_id);

    // Clean up worktree if present
    if let Some(ref wt_path) = claimed.worktree_path {
        state.git.cleanup_worktree_by_path(wt_path).await;
    }

    // Soft-delete: mark as "stopped" instead of deleting
    if let Err(e) = state
        .db
        .update_session_status(&session.session_id, "stopped")
        .await
    {
        tracing::warn!(session_id = %session.session_id, error = %e, "Failed to update session status to stopped");
    }

    gauge!("active_sessions").decrement(1.0);
    tracing::info!(session_id = %session.session_id, "Session stopped (soft-deleted)");
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
fn format_root_label(session_type: &str, project: &str, role: Option<&str>) -> String {
    if let Some(role) = role {
        return format!("**{}** for **{}**", role, project);
    }
    match session_type {
        "worker" => format!("**Worker session** for **{}**", project),
        "reviewer" => format!("**Reviewer session** for **{}**", project),
        _ => format!("**Session** for **{}**", project),
    }
}

async fn handle_messages(
    state: Arc<AppState>,
    mut rx: mpsc::Receiver<Post>,
    cancel_token: CancellationToken,
) {
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
                        if session.session_type == "team_lead" {
                            // Just stop the lead — team members keep running
                            if let Some(ref tid) = session.team_id {
                                broadcast_team_notification(
                                    &state, tid,
                                    "**Team Roster Update**: Team Lead session paused. Members continue working.",
                                    &session.session_id,
                                ).await;
                            }
                        } else if session.team_id.is_some() {
                            // Team member stopped — notify lead
                            if let Some(ref tid) = session.team_id {
                                let role_name = session.role.as_deref().unwrap_or("Unknown");
                                notify_team_member_left(
                                    &state,
                                    tid,
                                    role_name,
                                    &session.session_id,
                                )
                                .await;
                            }
                        }
                        stop_session(&state, &session).await;
                        let _ = state
                            .mm
                            .post_in_thread(channel_id, root_id, "Stopped.")
                            .await;
                        continue;
                    }
                    if cmd == "disband" {
                        if session.session_type != "team_lead" {
                            let _ = state
                                .mm
                                .post_in_thread(
                                    channel_id,
                                    root_id,
                                    "Only the team lead can disband the team.",
                                )
                                .await;
                            continue;
                        }
                        if let Some(ref tid) = session.team_id {
                            stop_team(&state, tid, &session).await;
                        }
                        stop_session(&state, &session).await;
                        let _ = state
                            .mm
                            .post_in_thread(channel_id, root_id, "Team disbanded.")
                            .await;
                        continue;
                    }
                    if cmd == "interrupt" {
                        match state.containers.interrupt(&session.session_id).await {
                            Ok(true) => {
                                let _ = state
                                    .mm
                                    .post_in_thread(channel_id, root_id, "Interrupted.")
                                    .await;
                            }
                            Ok(false) => {
                                let _ = state
                                    .mm
                                    .post_in_thread(
                                        channel_id,
                                        root_id,
                                        "No active session to interrupt.",
                                    )
                                    .await;
                            }
                            Err(e) => {
                                let _ = state
                                    .mm
                                    .post_in_thread(
                                        channel_id,
                                        root_id,
                                        &format!("Interrupt failed: {}", e),
                                    )
                                    .await;
                            }
                        }
                        continue;
                    }
                    if cmd == "stop --container" {
                        // Find the container's repo/branch from this session
                        let container_info = state
                            .containers
                            .get_session_info(&session.session_id)
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
                            let claimed = state
                                .containers
                                .stop_all_sessions_for_container(&repo, &branch);
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
                            if let Err(e) = state
                                .containers
                                .tear_down_container(&state.db, &repo, &branch)
                                .await
                            {
                                tracing::warn!(repo = %repo, branch = %branch, error = %e, "Failed to tear down container");
                            }

                            let _ = state
                                .mm
                                .post_in_thread(
                                    channel_id,
                                    root_id,
                                    &format!("Container stopped. {} sessions terminated.", count),
                                )
                                .await;
                        } else {
                            let _ = state
                                .mm
                                .post_in_thread(
                                    channel_id,
                                    root_id,
                                    "Could not find container for this session.",
                                )
                                .await;
                        }
                        continue;
                    }
                    if cmd == "compact" {
                        let _ = state.containers.send(&session.session_id, "/compact").await;
                        let _ = state
                            .mm
                            .post_in_thread(channel_id, root_id, "Compacting context...")
                            .await;
                        let _ = state.db.record_compaction(&session.session_id).await;
                        continue;
                    }
                    if cmd == "clear" {
                        let _ = state.containers.send(&session.session_id, "/clear").await;
                        let _ = state
                            .mm
                            .post_in_thread(channel_id, root_id, "Context cleared.")
                            .await;
                        continue;
                    }
                    if cmd == "restart" {
                        let _ = state
                            .mm
                            .post_in_thread(channel_id, root_id, "Restarting session...")
                            .await;
                        match state.containers.restart_session(&session.session_id).await {
                            Ok(()) => {
                                let _ = state
                                    .mm
                                    .post_in_thread(
                                        channel_id,
                                        root_id,
                                        "Restarted. Next message starts a fresh conversation.",
                                    )
                                    .await;
                            }
                            Err(e) => {
                                let _ = state
                                    .mm
                                    .post_in_thread(
                                        channel_id,
                                        root_id,
                                        &format!("Restart failed: {e}"),
                                    )
                                    .await;
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
                        state
                            .containers
                            .set_plan_mode(&session.session_id, new_state);
                        let msg = if new_state {
                            "Plan mode **enabled**. Claude will analyze but not modify files."
                        } else {
                            "Plan mode **disabled**. Claude can modify files."
                        };
                        let _ = state.mm.post_in_thread(channel_id, root_id, msg).await;
                        continue;
                    }
                    if cmd == "thinking" || cmd.starts_with("thinking ") {
                        let arg = cmd.strip_prefix("thinking").unwrap().trim();
                        let new_state = match arg {
                            "on" => true,
                            "off" => false,
                            "" => !state.containers.get_thinking_mode(&session.session_id),
                            _ => {
                                let _ = state.mm.post_in_thread(channel_id, root_id, "Usage: `@claude thinking` (toggle), `@claude thinking on`, `@claude thinking off`").await;
                                continue;
                            }
                        };
                        state
                            .containers
                            .set_thinking_mode(&session.session_id, new_state);
                        let msg = if new_state {
                            "Thinking mode **enabled**. Takes effect on `restart`."
                        } else {
                            "Thinking mode **disabled**. Takes effect on `restart`."
                        };
                        let _ = state.mm.post_in_thread(channel_id, root_id, msg).await;
                        continue;
                    }
                    if cmd == "auto" || cmd.starts_with("auto ") {
                        if session.session_type != "team_lead" {
                            let _ = state
                                .mm
                                .post_in_thread(
                                    channel_id,
                                    root_id,
                                    "Automation mode is only available for team leads.",
                                )
                                .await;
                            continue;
                        }
                        if let Some(ref tid) = session.team_id {
                            let arg = cmd.strip_prefix("auto").unwrap().trim();
                            let new_state = match arg {
                                "on" => true,
                                "off" => false,
                                "" => {
                                    // Toggle based on current state
                                    let team = state.db.get_team(tid).await.ok().flatten();
                                    !team.map(|t| t.automation).unwrap_or(false)
                                }
                                _ => {
                                    let _ = state.mm.post_in_thread(channel_id, root_id, "Usage: `@claude auto` (toggle), `@claude auto on`, `@claude auto off`").await;
                                    continue;
                                }
                            };
                            let _ = state.db.set_team_automation(tid, new_state).await;
                            let msg = if new_state {
                                "Automation mode **enabled**. Team lead will be nudged when idle."
                            } else {
                                "Automation mode **disabled**."
                            };
                            let _ = state.mm.post_in_thread(channel_id, root_id, msg).await;
                        } else {
                            let _ = state
                                .mm
                                .post_in_thread(channel_id, root_id, "Not in a team.")
                                .await;
                        }
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
                            let _ = state
                                .mm
                                .post_in_thread(channel_id, root_id, "_Generating title..._")
                                .await;
                        } else {
                            // Manual title
                            let label =
                                format_root_label(&session.session_type, &session.project, None);
                            let _ = state
                                .mm
                                .update_post(root_id, &format!("{} — {}", label, arg))
                                .await;
                            let _ = state
                                .mm
                                .post_in_thread(channel_id, root_id, "Title updated.")
                                .await;
                        }
                        continue;
                    }
                    // Catch-all: forward any /slash-command directly to Claude CLI
                    if cmd.starts_with('/') {
                        if let Err(e) = state.containers.send(&session.session_id, cmd).await {
                            tracing::warn!(session_id = %session.session_id, error = %e, "Failed to forward slash command");
                        }
                        let _ = state.db.touch_session(&session.session_id).await;
                        continue;
                    }
                    if cmd == "context" || cmd == "status" {
                        let age = chrono::Utc::now() - session.created_at;
                        let idle = chrono::Utc::now() - session.last_activity_at;
                        let info = state.containers.get_session_info(&session.session_id);
                        let plan_mode = info.as_ref().map(|i| i.plan_mode).unwrap_or(false);
                        let thinking_mode = info.as_ref().map(|i| i.thinking_mode).unwrap_or(false);
                        let claude_sid = info.as_ref().and_then(|i| {
                            i.claude_session_id
                                .as_deref()
                                .map(|s| format!("`{}`", &s[..8.min(s.len())]))
                        });

                        // Look up container info for this session's repo/branch
                        let parts: Vec<&str> = session.project.splitn(2, '@').collect();
                        let repo = parts[0];
                        let branch = parts.get(1).unwrap_or(&"");
                        let container_entry =
                            state.containers.registry.get_container(repo, branch).await;
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
                            Some(li) => format!(
                                "{} ago ({})",
                                format_duration_short(li.idle_duration),
                                li.last_event_type
                            ),
                            None => "_unknown_".to_string(),
                        };
                        // Context window usage
                        let last_input = info.as_ref().map(|i| i.last_input_tokens).unwrap_or(0);
                        let ctx_max = config::settings().context_window_max;
                        let context_line = if last_input > 0 {
                            let usage_pct = (last_input as f64 / ctx_max as f64 * 100.0) as u64;
                            format!(
                                "| Context | {} / {}k ({}%) |",
                                format_token_count(last_input),
                                ctx_max / 1000,
                                usage_pct
                            )
                        } else {
                            "| Context | _no data yet_ |".to_string()
                        };

                        let msg = format!(
                            "**Session Status:**\n\
| Property | Value |\n\
|---|---|\n\
| Version | {} |\n\
| Session | `{}` |\n\
| Claude ID | {} |\n\
| Type | {} |\n\
| Project | **{}** |\n\
{}\n\
{}\n\
| Messages | {} |\n\
| Compactions | {} |\n\
| Plan mode | {} |\n\
| Thinking | {} |\n\
| Age | {} |\n\
| Idle | {} |\n\
| Last active | {} |",
                            APP_VERSION,
                            &session.session_id[..8.min(session.session_id.len())],
                            claude_sid.unwrap_or_else(|| "_none_".to_string()),
                            session.session_type,
                            session.project,
                            container_line,
                            context_line,
                            session.message_count,
                            session.compaction_count,
                            if plan_mode { "on" } else { "off" },
                            if thinking_mode { "on" } else { "off" },
                            format_duration(age),
                            format_duration(idle),
                            last_active_str,
                        );
                        let _ = state.mm.post_in_thread(channel_id, root_id, &msg).await;
                        continue;
                    }
                }
                // Forward message to session (with team context if applicable)
                tracing::info!(
                    session_id = %session.session_id,
                    text_len = text.len(),
                    "Forwarding thread message to session"
                );
                let forwarded_text = if let Some(ref tid) = session.team_id {
                    let header = build_team_context_header(
                        &state.db,
                        tid,
                        session.role.as_deref().unwrap_or("Unknown"),
                    )
                    .await;
                    format!("{}\n**From User**: {}", header, text)
                } else {
                    text.to_string()
                };
                if let Err(e) = state
                    .containers
                    .send(&session.session_id, &forwarded_text)
                    .await
                {
                    tracing::warn!(
                        session_id = %session.session_id,
                        error = %e,
                        "Failed to forward message to container"
                    );
                }
                // Track activity
                let _ = state.db.touch_session(&session.session_id).await;
            } else if text.starts_with(bot_trigger) {
                let cmd = text.trim_start_matches(bot_trigger).trim();

                // Check for `resume` command on a stopped session
                if cmd == "resume" {
                    match state
                        .db
                        .get_stopped_session_by_thread(channel_id, root_id)
                        .await
                    {
                        Ok(Some(stopped)) => {
                            let _ = state
                                .mm
                                .post_in_thread(channel_id, root_id, "Resuming session...")
                                .await;

                            // Parse repo/branch from stored project
                            let parts: Vec<&str> = stopped.project.splitn(2, '@').collect();
                            let repo = parts[0].to_string();
                            let branch = parts.get(1).unwrap_or(&"").to_string();

                            // Get grpc_port from registry
                            let grpc_port = state
                                .containers
                                .registry
                                .get_container(&repo, &branch)
                                .await
                                .map(|e| {
                                    if e.grpc_port == 0 {
                                        config::settings().grpc_port_start
                                    } else {
                                        e.grpc_port
                                    }
                                })
                                .unwrap_or(config::settings().grpc_port_start);

                            let (output_tx, output_rx) = mpsc::channel::<OutputEvent>(100);
                            let new_session_id = Uuid::new_v4().to_string();

                            // Reconstruct system prompt for team sessions
                            let system_prompt = rebuild_system_prompt(
                                &state.db,
                                &stopped.session_type,
                                stopped.team_id.as_deref(),
                                stopped.role.as_deref(),
                            )
                            .await;

                            // Try to reuse the existing container
                            match state
                                .containers
                                .reconnect(
                                    &new_session_id,
                                    &stopped.container_name,
                                    &stopped.project_path,
                                    &repo,
                                    &branch,
                                    &stopped.session_type,
                                    &state.db,
                                    output_tx,
                                    grpc_port,
                                    system_prompt.as_deref(),
                                    stopped.claude_session_id.as_deref(),
                                )
                                .await
                            {
                                Ok(()) => {
                                    // Register liveness
                                    state.liveness.register(&new_session_id);

                                    // Increment container session count
                                    let _ = state
                                        .containers
                                        .registry
                                        .increment_sessions(&state.db, &repo, &branch)
                                        .await;

                                    // Create new DB session record with the stored claude_session_id
                                    if let Err(e) = state
                                        .db
                                        .create_session(
                                            &new_session_id,
                                            channel_id,
                                            root_id,
                                            &stopped.project,
                                            &stopped.project_path,
                                            &stopped.container_name,
                                            &stopped.session_type,
                                            None,
                                            stopped.user_id.as_deref(),
                                        )
                                        .await
                                    {
                                        tracing::error!(error = %e, "Failed to persist resumed session");
                                        let _ = state
                                            .mm
                                            .post_in_thread(
                                                channel_id,
                                                root_id,
                                                &format!("Resume failed: {}", e),
                                            )
                                            .await;
                                        continue;
                                    }

                                    // Copy claude_session_id from stopped session for future resume
                                    if let Some(ref csid) = stopped.claude_session_id {
                                        let _ = state
                                            .db
                                            .update_claude_session_id(&new_session_id, csid)
                                            .await;
                                    }

                                    // Delete the old stopped session record
                                    let _ = state.db.delete_session(&stopped.session_id).await;

                                    // Restore team membership if the stopped session was part of a team
                                    if let (Some(tid), Some(role)) =
                                        (&stopped.team_id, &stopped.role)
                                    {
                                        let _ = state
                                            .db
                                            .set_session_team(&new_session_id, tid, role)
                                            .await;
                                        if stopped.session_type == "team_lead" {
                                            let _ = state
                                                .db
                                                .update_team_lead_session(tid, &new_session_id)
                                                .await;
                                        }
                                        broadcast_team_notification(
                                            &state,
                                            tid,
                                            &format!(
                                                "**Team Roster Update**: {} session resumed.",
                                                role
                                            ),
                                            &new_session_id,
                                        )
                                        .await;
                                    }

                                    gauge!("active_sessions").increment(1.0);

                                    let _ = state
                                        .mm
                                        .post_in_thread(channel_id, root_id, "Session resumed.")
                                        .await;

                                    // Start output streaming
                                    let state_clone = state.clone();
                                    let channel_id_clone = channel_id.to_string();
                                    let thread_id_clone = root_id.to_string();
                                    let session_id_clone = new_session_id.clone();
                                    let session_type_clone = stopped.session_type.clone();
                                    let project_clone = stopped.project.clone();
                                    tokio::spawn(async move {
                                        stream_output(
                                            state_clone,
                                            channel_id_clone,
                                            thread_id_clone,
                                            session_id_clone,
                                            session_type_clone,
                                            project_clone,
                                            output_rx,
                                        )
                                        .await;
                                    });
                                }
                                Err(e) => {
                                    let _ = state
                                        .mm
                                        .post_in_thread(
                                            channel_id,
                                            root_id,
                                            &format!("Resume failed: {}", e),
                                        )
                                        .await;
                                }
                            }
                        }
                        Ok(None) => {
                            let _ = state
                                .mm
                                .post_in_thread(
                                    channel_id,
                                    root_id,
                                    "No stopped session in this thread to resume.",
                                )
                                .await;
                        }
                        Err(e) => {
                            let _ = state
                                .mm
                                .post_in_thread(channel_id, root_id, &format!("Error: {}", e))
                                .await;
                        }
                    }
                } else {
                    let _ = state
                        .mm
                        .post_in_thread(channel_id, root_id, "No active session in this thread. Use `@claude resume` to restart a stopped session.")
                        .await;
                }
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
                        let _ = state
                            .mm
                            .post(
                                channel_id,
                                "Usage: `@claude start <org/repo>` or `@claude start <repo>`",
                            )
                            .await;
                        continue;
                    }

                    // Parse --plan and --thinking flags from input
                    let plan_mode = project_input.split_whitespace().any(|w| w == "--plan");
                    let thinking_mode = project_input.split_whitespace().any(|w| w == "--thinking");
                    let project_input_clean = project_input
                        .split_whitespace()
                        .filter(|w| {
                            let is_flag = w.starts_with("--");
                            if is_flag {
                                tracing::debug!(flag = %w, "Stripped flag from project input");
                            }
                            !is_flag
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    let project_input = project_input_clean.as_str();

                    let s = config::settings();

                    // Check if input looks like a repo reference or has a default org
                    let has_slash = project_input
                        .split_whitespace()
                        .next()
                        .map(|p| p.split('@').next().unwrap_or(p))
                        .map(|p| p.contains('/'))
                        .unwrap_or(false);

                    if has_slash
                        || s.default_org.is_some()
                        || RepoRef::looks_like_repo(project_input)
                    {
                        match resolve_project_channel(&state, project_input, &post.user_id).await {
                            Ok((proj_channel_id, channel_name, repo_ref)) => {
                                match start_session(
                                    &state,
                                    &proj_channel_id,
                                    &repo_ref,
                                    "standard",
                                    plan_mode,
                                    thinking_mode,
                                    Some(&post.user_id),
                                    None,
                                    None,
                                    None,
                                )
                                .await
                                {
                                    Ok(session_id) => {
                                        let _ = state
                                            .mm
                                            .post(
                                                channel_id,
                                                &format!(
                                                    "Session `{}` started in ~{}",
                                                    &session_id[..8],
                                                    channel_name,
                                                ),
                                            )
                                            .await;
                                    }
                                    Err(e) => {
                                        let _ = state
                                            .mm
                                            .post(channel_id, &format!("Failed: {}", e))
                                            .await;
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

                // --- team <org/repo> [--devs N] ---
                if let Some(team_input) = cmd_text.strip_prefix("team ").map(|s| s.trim()) {
                    if team_input.is_empty() {
                        let _ = state
                            .mm
                            .post(channel_id, "Usage: `@claude team <org/repo> [--devs N]`")
                            .await;
                        continue;
                    }

                    // Parse --devs N flag
                    let words: Vec<&str> = team_input.split_whitespace().collect();
                    let mut dev_count: i32 = 1;
                    let mut project_words = Vec::new();
                    let mut i = 0;
                    while i < words.len() {
                        if words[i] == "--devs"
                            && let Some(n) = words.get(i + 1).and_then(|s| s.parse::<i32>().ok())
                        {
                            dev_count = n.clamp(1, 5);
                            i += 2;
                            continue;
                        }
                        project_words.push(words[i]);
                        i += 1;
                    }
                    let project_input = project_words.join(" ");

                    match resolve_project_channel(&state, &project_input, &post.user_id).await {
                        Ok((proj_channel_id, channel_name, repo_ref)) => {
                            let team_id = Uuid::new_v4().to_string();

                            // Build Team Lead system prompt
                            let lead_prompt =
                                match build_team_lead_prompt(&state.db, dev_count).await {
                                    Ok(p) => p,
                                    Err(e) => {
                                        let _ = state
                                            .mm
                                            .post(
                                                channel_id,
                                                &format!("Failed to build team prompt: {}", e),
                                            )
                                            .await;
                                        continue;
                                    }
                                };

                            match start_session(
                                &state,
                                &proj_channel_id,
                                &repo_ref,
                                "team_lead",
                                false,
                                false,
                                Some(&post.user_id),
                                Some(&team_id),
                                Some("Team Lead"),
                                Some(&lead_prompt),
                            )
                            .await
                            {
                                Ok(session_id) => {
                                    // Create team record
                                    let project_path = match state
                                        .db
                                        .get_session_by_id_prefix(&session_id[..8])
                                        .await
                                    {
                                        Ok(Some(s)) => s.project_path,
                                        _ => String::new(),
                                    };
                                    if let Err(e) = state
                                        .db
                                        .create_team(
                                            &team_id,
                                            &proj_channel_id,
                                            &project_input,
                                            &project_path,
                                            &session_id,
                                            dev_count,
                                        )
                                        .await
                                    {
                                        tracing::warn!(error = %e, "Failed to create team record");
                                    }

                                    // Create coordination thread for this team
                                    setup_coordination_thread(&state, &team_id, &project_input)
                                        .await;

                                    let _ = state.mm.post(
                                        channel_id,
                                        &format!(
                                            "Team started in ~{}. Send your task to the Team Lead's thread.",
                                            channel_name,
                                        ),
                                    ).await;
                                }
                                Err(e) => {
                                    let _ =
                                        state.mm.post(channel_id, &format!("Failed: {}", e)).await;
                                }
                            }
                        }
                        Err(e) => {
                            let _ = state.mm.post(channel_id, &format!("Failed: {}", e)).await;
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
                            let claimed = state
                                .containers
                                .stop_all_sessions_for_container(repo, branch);
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
                            if let Err(e) = state
                                .containers
                                .tear_down_container(&state.db, repo, branch)
                                .await
                            {
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
                                let _ = state
                                    .mm
                                    .post(
                                        channel_id,
                                        &format!("Stopped session `{}`.", &session.session_id[..8]),
                                    )
                                    .await;
                            }
                            Ok(None) => {
                                let _ = state
                                    .mm
                                    .post(
                                        channel_id,
                                        &format!("No session found matching `{}`.", short_id),
                                    )
                                    .await;
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
                            let _ = state
                                .mm
                                .post(
                                    channel_id,
                                    &format!("**Version:** {}\n\nNo active sessions.", APP_VERSION),
                                )
                                .await;
                        }
                        Ok(sessions) => {
                            let now = chrono::Utc::now();

                            // Show container info first
                            let containers = state.containers.registry.list_all().await;
                            let mut msg = format!("**Version:** {}\n\n", APP_VERSION);
                            if !containers.is_empty() {
                                msg.push_str("**Containers:**\n");
                                for ((_repo, _branch), entry) in &containers {
                                    msg.push_str(&format!(
                                        "- Container: `{}` ({}, {} sessions)\n",
                                        entry.container_name, entry.state, entry.session_count,
                                    ));
                                }
                                msg.push('\n');
                            }

                            // Show active teams
                            if let Ok(teams) = state.db.get_active_teams().await
                                && !teams.is_empty()
                            {
                                msg.push_str("**Active Teams:**\n| Team | Project | Members | Age |\n|------|---------|---------|-----|\n");
                                for t in &teams {
                                    let members = state
                                        .db
                                        .get_team_members(&t.team_id)
                                        .await
                                        .unwrap_or_default();
                                    let age = format_duration(now - t.created_at);
                                    msg.push_str(&format!(
                                        "| `{}` | {} | {} | {} |\n",
                                        &t.team_id[..8],
                                        t.project,
                                        members.len(),
                                        age,
                                    ));
                                }
                                msg.push('\n');
                            }

                            msg.push_str("**Active Sessions:**\n");
                            for s in &sessions {
                                let idle = now - s.last_activity_at;
                                let role_str = s
                                    .role
                                    .as_deref()
                                    .map(|r| format!(" [{}]", r))
                                    .unwrap_or_default();
                                msg.push_str(&format!(
                                    "- `{}` | {}{} | **{}** | {} msgs | idle {}\n",
                                    &s.session_id[..8],
                                    s.session_type,
                                    role_str,
                                    s.project,
                                    s.message_count,
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
                        - `{trigger} start <repo> --thinking` — Start with extended thinking enabled\n\
                        - `{trigger} team <org/repo> [--devs N]` — Start a team with N developers (default 1)\n\
                        - `{trigger} stop <id-prefix>` — Stop a session by ID prefix\n\
                        - `{trigger} stop --all` — Stop all sessions and tear down all containers\n\
                        - `{trigger} status` — List all active sessions and containers\n\
                        - `{trigger} help` — Show this message\n\
                        \n\
                        **In a session thread:**\n\
                        - Reply directly to send input\n\
                        - `{trigger} stop` — End the session (team members keep running)\n\
                        - `{trigger} disband` — Stop all team members and disband the team (team lead only)\n\
                        - `{trigger} stop --container` — Stop all sessions sharing this container and tear it down\n\
                        - `{trigger} interrupt` — Interrupt a running turn\n\
                        - `{trigger} compact` — Compact/summarize context\n\
                        - `{trigger} clear` — Clear conversation history\n\
                        - `{trigger} restart` — Restart Claude conversation\n\
                        - `{trigger} plan` — Toggle plan mode (read-only analysis)\n\
                        - `{trigger} thinking` — Toggle extended thinking (takes effect on restart)\n\
                        - `{trigger} auto` — Toggle automation mode (team lead only, nudges when idle)\n\
                        - `{trigger} title [text]` — Set thread title (auto-generate if no text)\n\
                        - `{trigger} status` — Show session status and context health\n\
                        - `{trigger} /command` — Forward any Claude CLI slash command",
                        trigger = bot_trigger,
                    )).await;
                    continue;
                }

                // Unknown command
                let _ = state
                    .mm
                    .post(
                        channel_id,
                        &format!("Unknown command. Try `{} help`.", bot_trigger,),
                    )
                    .await;
            } else {
                // Step 2: Non-command top-level message — route to active session in this channel
                match state
                    .db
                    .get_non_worker_sessions_by_channel(channel_id)
                    .await
                {
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

/// Drain the short-term batch buffer into the LiveCard's text content.
/// Handles overflow by finalizing the current post and starting a new one.
async fn drain_batch_to_card(
    card: &mut LiveCard,
    state: &AppState,
    channel_id: &str,
    thread_id: &str,
    batch: &mut Vec<String>,
    batch_bytes: &mut usize,
) {
    if batch.is_empty() {
        return;
    }
    let chunk = batch.join("\n");
    batch.clear();
    *batch_bytes = 0;

    // Overflow check: if appending would exceed limit, finalize and start new post
    if card.has_content() && card.would_overflow(&chunk) {
        card.flush(state, channel_id, thread_id).await;
        card.start_new_post();
    }

    card.append_text(&chunk);
}

/// Maximum batch size in bytes before flushing (14KB safety margin under Mattermost's 16KB limit)
const BATCH_MAX_BYTES: usize = 14 * 1024;
/// Maximum number of lines before flushing
const BATCH_MAX_LINES: usize = 80;
/// Batch timeout before flushing accumulated output
const BATCH_TIMEOUT: Duration = Duration::from_millis(200);
/// Minimum interval between live card updates (avoid Mattermost rate limits)
const CARD_UPDATE_MIN_INTERVAL: Duration = Duration::from_millis(500);
/// Maximum number of tool lines shown in the live card before collapsing
const CARD_MAX_TOOL_LINES: usize = 20;
/// Maximum post size before starting a new post (safety margin under Mattermost's 16KB limit)
const POST_MAX_BYTES: usize = 15 * 1024;
/// Minimum post size before a cycle-boundary split is considered
const CARD_SPLIT_MIN_BYTES: usize = 6 * 1024;
/// Fallback: maximum post updates before forcing a split (no cycle boundary needed)
const CARD_MAX_UPDATES: u32 = 200;

/// Heartbeat interval for updating the elapsed timer on the live card
const CARD_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Live card state — a single updatable Mattermost post that shows processing status
/// and accumulated response text. One post per turn: header + tools + text.
struct LiveCard {
    post_id: Option<String>,
    header: String,
    tool_lines: Vec<String>,
    /// Accumulated text content for this turn
    text_content: String,
    last_update: tokio::time::Instant,
    /// When processing started (for elapsed timer), cleared on finalize
    started_at: Option<tokio::time::Instant>,
    /// Whether the card has unsent changes (dirty flag for throttled updates)
    dirty: bool,
    /// Last known input_tokens (for context window display in headers)
    last_input_tokens: u64,
    /// Number of times the current post has been flushed (for edit-count splitting)
    flush_count: u32,
    /// Whether text has been appended since the last tool action (cycle boundary detection)
    has_text_since_tools: bool,
}

impl LiveCard {
    fn new() -> Self {
        Self {
            post_id: None,
            header: String::new(),
            tool_lines: Vec::new(),
            text_content: String::new(),
            last_update: tokio::time::Instant::now(),
            started_at: None,
            dirty: false,
            last_input_tokens: 0,
            flush_count: 0,
            has_text_since_tools: false,
        }
    }

    /// Reset for a new processing turn.
    fn start_processing(&mut self) {
        self.header = if self.last_input_tokens > 0 {
            format!(
                ":hourglass_flowing_sand: Processing... · {}",
                format_ctx_suffix(self.last_input_tokens)
            )
        } else {
            ":hourglass_flowing_sand: Processing...".to_string()
        };
        self.tool_lines.clear();
        self.text_content.clear();
        self.started_at = Some(tokio::time::Instant::now());
        self.dirty = true;
        self.flush_count = 0;
        self.has_text_since_tools = false;
    }

    /// Append a tool action line to the card.
    fn add_tool_action(&mut self, action: &str) {
        self.tool_lines.push(format!("> {}", action));
        self.has_text_since_tools = false;
        self.dirty = true;
    }

    /// Finalize the card with completion stats.
    fn finalize(&mut self, input_tokens: u64, output_tokens: u64) {
        let input_k = format_token_count(input_tokens);
        let output_k = format_token_count(output_tokens);

        // Check if response ends with a question — indicates Claude is waiting for user input
        let is_question = self
            .text_content
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.trim_end().ends_with('?'))
            .unwrap_or(false);

        self.header = if is_question {
            format!(
                ":speech_balloon: Awaiting reply ({} in / {} out · {})",
                input_k,
                output_k,
                format_ctx_suffix(input_tokens)
            )
        } else {
            format!(
                ":white_check_mark: Done ({} in / {} out · {})",
                input_k,
                output_k,
                format_ctx_suffix(input_tokens)
            )
        };
        self.last_input_tokens = input_tokens;
        self.started_at = None;
        self.dirty = true;
    }

    /// Finalize the card with an error.
    fn finalize_error(&mut self, exit_code: Option<i32>, signal: &Option<String>) {
        let code_str = exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        self.header = match signal {
            Some(sig) => format!(":warning: Process died (exit {}, {})", code_str, sig),
            None => format!(":warning: Process died (exit {})", code_str),
        };
        self.started_at = None;
        self.dirty = true;
    }

    /// Update the header with elapsed time (for heartbeat ticks).
    fn update_elapsed(&mut self) {
        if let Some(started) = self.started_at {
            let elapsed = started.elapsed();
            let secs = elapsed.as_secs();
            let elapsed_str = if secs >= 60 {
                format!("{}m {}s", secs / 60, secs % 60)
            } else {
                format!("{}s", secs)
            };
            self.header = if self.last_input_tokens > 0 {
                format!(
                    ":hourglass_flowing_sand: Processing... ({}) · {}",
                    elapsed_str,
                    format_ctx_suffix(self.last_input_tokens)
                )
            } else {
                format!(":hourglass_flowing_sand: Processing... ({})", elapsed_str)
            };
            self.dirty = true;
        }
    }

    /// Append response text to the card.
    fn append_text(&mut self, text: &str) {
        if !self.text_content.is_empty() {
            self.text_content.push('\n');
        }
        self.text_content.push_str(text);
        self.has_text_since_tools = true;
        self.dirty = true;
    }

    /// Check if the rendered content would exceed POST_MAX_BYTES after appending new text.
    fn would_overflow(&self, new_text: &str) -> bool {
        let current_len = self.render().len();
        current_len + 1 + new_text.len() > POST_MAX_BYTES
    }

    /// Finalize current post and start fresh (for overflow).
    fn start_new_post(&mut self) {
        self.post_id = None;
        self.text_content.clear();
        self.tool_lines.clear();
        self.dirty = true;
        self.flush_count = 0;
        self.has_text_since_tools = false;
    }

    /// Whether the card has any content beyond the header.
    fn has_content(&self) -> bool {
        !self.text_content.is_empty() || !self.tool_lines.is_empty()
    }

    /// Render the card as a single markdown string.
    fn render(&self) -> String {
        let mut parts = Vec::with_capacity(2 + self.tool_lines.len());
        parts.push(self.header.clone());

        if self.tool_lines.len() > CARD_MAX_TOOL_LINES {
            // Collapse older tools, show first 3 and last (MAX - 4)
            let show_tail = CARD_MAX_TOOL_LINES - 4;
            for line in &self.tool_lines[..3] {
                parts.push(line.clone());
            }
            let hidden = self.tool_lines.len() - 3 - show_tail;
            parts.push(format!("> _... {} more tools ..._", hidden));
            for line in &self.tool_lines[self.tool_lines.len() - show_tail..] {
                parts.push(line.clone());
            }
        } else {
            for line in &self.tool_lines {
                parts.push(line.clone());
            }
        }

        if !self.text_content.is_empty() {
            parts.push(String::new()); // blank line separator
            parts.push(self.text_content.clone());
        }

        parts.join("\n")
    }

    /// Whether enough time has passed since the last update to allow a new one.
    fn can_update_now(&self) -> bool {
        self.last_update.elapsed() >= CARD_UPDATE_MIN_INTERVAL
    }

    /// Whether the current post should split into a continuation.
    ///
    /// Prefers splitting at cycle boundaries (text→tool transitions) when the
    /// post has grown large enough. Falls back to a hard edit-count limit for
    /// pure-tool sessions with no text.
    fn should_split(&self) -> bool {
        if self.post_id.is_none() {
            return false;
        }
        // Cycle boundary: text was written, now a new tool is starting
        if self.has_text_since_tools && self.render().len() >= CARD_SPLIT_MIN_BYTES {
            return true;
        }
        // Hard fallback for sessions with no text between tools
        self.flush_count >= CARD_MAX_UPDATES
    }

    /// Finalize the current post with a "Continued below" header and flush it.
    async fn finalize_continuation(&mut self, state: &AppState, channel_id: &str, thread_id: &str) {
        if let Some(started) = self.started_at {
            let secs = started.elapsed().as_secs();
            let elapsed_str = if secs >= 60 {
                format!("{}m {}s", secs / 60, secs % 60)
            } else {
                format!("{}s", secs)
            };
            self.header = if self.last_input_tokens > 0 {
                format!(
                    ":arrow_down: Continued below ({}) · {}",
                    elapsed_str,
                    format_ctx_suffix(self.last_input_tokens)
                )
            } else {
                format!(":arrow_down: Continued below ({})", elapsed_str)
            };
        } else {
            self.header = ":arrow_down: Continued below".to_string();
        }
        self.dirty = true;
        self.flush(state, channel_id, thread_id).await;
    }

    /// Start a new continuation post, preserving the elapsed timer.
    fn start_continuation_post(&mut self) {
        self.post_id = None;
        self.text_content.clear();
        self.tool_lines.clear();
        self.flush_count = 0;
        self.has_text_since_tools = false;
        self.dirty = true;
        // Restore the processing header with current elapsed time
        self.update_elapsed();
    }

    /// Send the card to Mattermost (create or update).
    async fn flush(&mut self, state: &AppState, channel_id: &str, thread_id: &str) {
        if !self.dirty {
            return;
        }
        let msg = self.render();
        if let Some(ref post_id) = self.post_id {
            let _ = state.mm.update_post(post_id, &msg).await;
            self.flush_count += 1;
        } else {
            match state.mm.post_in_thread(channel_id, thread_id, &msg).await {
                Ok(post_id) => {
                    self.post_id = Some(post_id);
                    self.flush_count = 1;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to create live card post");
                }
            }
        }
        self.last_update = tokio::time::Instant::now();
        self.dirty = false;
    }
}

/// Format token count in a human-readable way (e.g. "15.2k", "832")
fn format_token_count(tokens: u64) -> String {
    if tokens >= 1000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        tokens.to_string()
    }
}

/// Format context window suffix (e.g. "45.2k/200k ctx")
fn format_ctx_suffix(input_tokens: u64) -> String {
    let ctx_max_k = config::settings().context_window_max / 1000;
    format!("{}/{}k ctx", format_token_count(input_tokens), ctx_max_k)
}

/// Fire a team spawn asynchronously (acquires spawn lock, calls handle_team_spawn).
/// Always injects a team status update into the lead's session before spawning
/// so the lead has current roster visibility.
#[allow(clippy::too_many_arguments)]
fn fire_team_spawn(
    state: &Arc<AppState>,
    team_id: &str,
    channel_id: &str,
    session_id: &str,
    sender_role: &str,
    role_name: &str,
    initial_task: &str,
    project: &str,
) {
    let spawn_state = state.clone();
    let spawn_tid = team_id.to_string();
    let spawn_cid = channel_id.to_string();
    let spawn_sid = session_id.to_string();
    let spawn_role = sender_role.to_string();
    let role_name = role_name.to_string();
    let initial_task = initial_task.to_string();
    let spawn_project = project.to_string();
    let lock = spawn_state
        .spawn_locks
        .entry(spawn_tid.clone())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    tokio::spawn(async move {
        let _guard = lock.lock().await;
        // Inject team status into the lead's session before spawning so it sees
        // the current roster (avoids duplicate spawns and stale assumptions).
        handle_team_status(&spawn_state, &spawn_sid, &spawn_tid).await;
        handle_team_spawn(
            spawn_state,
            &spawn_tid,
            &spawn_cid,
            &spawn_sid,
            &spawn_role,
            &role_name,
            &initial_task,
            &spawn_project,
        )
        .await;
    });
}

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
    let spawn_re = team_spawn_regex();
    let msg_re = team_msg_regex();
    let broadcast_re = team_broadcast_regex();
    let msg_end_re = team_msg_end_regex();
    let status_re = team_status_regex();
    let pr_ready_re = pr_ready_regex();
    let pr_reviewed_re = pr_reviewed_regex();

    // Look up team context for this session (cached once)
    let team_info: Option<(String, String)> = {
        if let Ok(Some(sess)) = state
            .db
            .get_session_by_id_prefix(&session_id[..8.min(session_id.len())])
            .await
        {
            match (sess.team_id, sess.role) {
                (Some(tid), Some(role)) => Some((tid, role)),
                _ => None,
            }
        } else {
            None
        }
    };

    let mut batch: Vec<String> = Vec::new();
    let mut batch_bytes: usize = 0;
    let batch_timer = tokio::time::sleep(BATCH_TIMEOUT);
    tokio::pin!(batch_timer);

    let heartbeat_timer = tokio::time::sleep(CARD_HEARTBEAT_INTERVAL);
    tokio::pin!(heartbeat_timer);

    let mut card = LiveCard::new();

    // Team messaging state machine
    let mut team_msg_target: Option<String> = None; // "Role Name" or "__broadcast__"
    let mut team_msg_buffer: Vec<String> = Vec::new();
    // Pending spawn: accumulates multi-line task until [/MSG] or next marker, then fires spawn.
    // (role_name, use_worktree)
    let mut pending_spawn: Option<String> = None;

    tracing::info!(session_id = %session_id, "stream_output: started");

    loop {
        tokio::select! {
            event_opt = rx.recv() => {
                let Some(event) = event_opt else {
                    // Channel closed — drain remaining batch into card and flush
                    tracing::info!(session_id = %session_id, batch_len = batch.len(), "stream_output: channel closed, flushing");
                    drain_batch_to_card(&mut card, &state, &channel_id, &thread_id, &mut batch, &mut batch_bytes).await;
                    card.flush(&state, &channel_id, &thread_id).await;
                    break;
                };

                match event {
                    OutputEvent::ProcessingStarted { .. } => {
                        state.liveness.update_activity(&session_id, "ProcessingStarted");
                        tracing::info!(session_id = %session_id, "stream_output: ProcessingStarted");
                        // Start a new live card for this turn
                        card.start_processing();
                        card.flush(&state, &channel_id, &thread_id).await;
                    }
                    OutputEvent::TextLine(line) => {
                        state.liveness.update_activity(&session_id, "TextLine");
                        tracing::debug!(session_id = %session_id, line_len = line.len(), "stream_output: TextLine received");

                        // --- Team marker interception (only for team sessions) ---
                        if let Some((tid, sender_role)) = &team_info {

                            // Multi-line SPAWN accumulation mode
                            if let Some(ref spawn_role) = pending_spawn {
                                let has_end_marker = msg_end_re.is_match(&line);
                                let end_in_line = !has_end_marker && line.contains("[/MSG]");

                                if has_end_marker || end_in_line {
                                    if end_in_line
                                        && let Some(pos) = line.find("[/MSG]")
                                    {
                                        let before = line[..pos].trim();
                                        if !before.is_empty() {
                                            team_msg_buffer.push(before.to_string());
                                        }
                                    }
                                    let task = team_msg_buffer.join("\n");
                                    team_msg_buffer.clear();
                                    let spawn_role = spawn_role.clone();
                                    pending_spawn = None;
                                    fire_team_spawn(
                                        &state, tid, &channel_id, &session_id, sender_role,
                                        &spawn_role, &task, &project,
                                    );
                                } else {
                                    team_msg_buffer.push(line);
                                }
                                continue;
                            }

                            // Multi-line message accumulation mode
                            if team_msg_target.is_some() {
                                // Check for [/MSG] — either standalone or at end of line
                                let has_end_marker = msg_end_re.is_match(&line);
                                let end_in_line = !has_end_marker && line.contains("[/MSG]");

                                if has_end_marker || end_in_line {
                                    // If [/MSG] is embedded in the line, capture text before it
                                    if end_in_line
                                        && let Some(pos) = line.find("[/MSG]")
                                    {
                                        let before = line[..pos].trim();
                                        if !before.is_empty() {
                                            team_msg_buffer.push(before.to_string());
                                        }
                                    }
                                    // Flush accumulated message
                                    let message = team_msg_buffer.join("\n");
                                    team_msg_buffer.clear();
                                    let target = team_msg_target.take().unwrap();
                                    // Clear pending task — member is responding
                                    let _ = state.db.clear_pending_task(&session_id).await;
                                    if target == "__broadcast__" {
                                        let preview = truncate_preview(&message, 80);
                                        card.append_text(&format!("> :loudspeaker: **Broadcast**: {}", preview));
                                        card.dirty = true;
                                        broadcast_team_message(&state, &session_id, sender_role, &message, tid, &channel_id).await;
                                    } else {
                                        let preview = truncate_preview(&message, 80);
                                        card.append_text(&format!("> :speech_balloon: **To {}**: {}", target, preview));
                                        card.dirty = true;
                                        route_team_message(&state, &session_id, sender_role, &target, &message, tid, &channel_id).await;
                                    }
                                } else {
                                    team_msg_buffer.push(line);
                                }
                                continue;
                            }

                            // Check for [SPAWN:Role] marker
                            if let Some(caps) = spawn_re.captures(&line) {
                                let role_name = caps[1].trim().to_string();
                                let inline_task = caps[2].trim().to_string();
                                drain_batch_to_card(&mut card, &state, &channel_id, &thread_id, &mut batch, &mut batch_bytes).await;
                                card.append_text(&format!("> :rocket: **Spawning {}**...", role_name));
                                card.dirty = true;

                                // Check for single-line spawn: [SPAWN:Role] task [/MSG]
                                if let Some(end_pos) = inline_task.find("[/MSG]") {
                                    let task = inline_task[..end_pos].trim().to_string();
                                    fire_team_spawn(
                                        &state, tid, &channel_id, &session_id, sender_role,
                                        &role_name, &task, &project,
                                    );
                                } else if inline_task.is_empty() {
                                    // No inline text and no [/MSG] — enter multi-line mode
                                    // to accumulate the task description
                                    pending_spawn = Some(role_name);
                                    team_msg_buffer.clear();
                                } else {
                                    // Has inline text but no [/MSG] — enter multi-line mode
                                    // to accumulate continuation lines
                                    pending_spawn = Some(role_name);
                                    team_msg_buffer.clear();
                                    team_msg_buffer.push(inline_task);
                                }
                                continue;
                            }

                            // Check for [TO:Role] marker
                            if let Some(caps) = msg_re.captures(&line) {
                                let target_role = caps[1].trim().to_string();
                                let inline_msg = caps[2].trim().to_string();
                                // Clear pending task — member is responding
                                let _ = state.db.clear_pending_task(&session_id).await;

                                // Check if [/MSG] is already in the inline text (single-line message).
                                // Without this, [TO:Role] message [/MSG] on one line would get stuck
                                // because [/MSG] detection only runs on subsequent TextLines.
                                if let Some(end_pos) = inline_msg.find("[/MSG]") {
                                    let message = inline_msg[..end_pos].trim().to_string();
                                    if !message.is_empty() {
                                        let preview = truncate_preview(&message, 80);
                                        card.append_text(&format!("> :speech_balloon: **To {}**: {}", target_role, preview));
                                        card.dirty = true;
                                        route_team_message(&state, &session_id, sender_role, &target_role, &message, tid, &channel_id).await;
                                    }
                                } else {
                                    // Enter multi-line accumulation mode
                                    team_msg_target = Some(target_role);
                                    team_msg_buffer.clear();
                                    if !inline_msg.is_empty() {
                                        team_msg_buffer.push(inline_msg);
                                    }
                                }
                                continue;
                            }

                            // Check for [BROADCAST] marker
                            if let Some(caps) = broadcast_re.captures(&line) {
                                let inline_msg = caps[1].trim().to_string();
                                // Clear pending task — member is responding
                                let _ = state.db.clear_pending_task(&session_id).await;

                                // Check if [/MSG] is already in the inline text (single-line message)
                                if let Some(end_pos) = inline_msg.find("[/MSG]") {
                                    let message = inline_msg[..end_pos].trim().to_string();
                                    if !message.is_empty() {
                                        let preview = truncate_preview(&message, 80);
                                        card.append_text(&format!("> :loudspeaker: **Broadcast**: {}", preview));
                                        card.dirty = true;
                                        broadcast_team_message(&state, &session_id, sender_role, &message, tid, &channel_id).await;
                                    }
                                } else {
                                    // Enter multi-line accumulation mode
                                    team_msg_target = Some("__broadcast__".to_string());
                                    team_msg_buffer.clear();
                                    if !inline_msg.is_empty() {
                                        team_msg_buffer.push(inline_msg);
                                    }
                                }
                                continue;
                            }

                            // Check for [TEAM_STATUS] marker
                            if status_re.is_match(&line) {
                                handle_team_status(&state, &session_id, tid).await;
                                continue;
                            }

                            // Check for [PR_READY:N] marker (team members signal PR is ready)
                            if let Some(caps) = pr_ready_re.captures(&line) {
                                let pr_num = caps[1].to_string();
                                card.append_text(&format!("> :mag: **PR #{} review gate**...", pr_num));
                                card.dirty = true;
                                handle_pr_ready(&state, &session_id, tid, sender_role, &pr_num, &channel_id).await;
                                continue;
                            }

                            // Check for [PR_REVIEWED:N] marker (team member confirms review comments addressed)
                            if let Some(caps) = pr_reviewed_re.captures(&line) {
                                let pr_num = caps[1].to_string();
                                card.append_text(&format!("> :white_check_mark: **PR #{} review complete** — forwarding to Team Lead", pr_num));
                                card.dirty = true;
                                // Clear pending task — member is done with review gate
                                let _ = state.db.clear_pending_task(&session_id).await;
                                handle_pr_reviewed(&state, &session_id, tid, sender_role, &pr_num, &channel_id).await;
                                continue;
                            }
                        }

                        // --- Existing marker checks ---
                        // Check for network request markers
                        if let Some(caps) = network_re.captures(&line) {
                            drain_batch_to_card(&mut card, &state, &channel_id, &thread_id, &mut batch, &mut batch_bytes).await;
                            let domain = caps[1].trim();
                            handle_network_request(&state, &channel_id, &thread_id, &session_id, domain).await;
                            continue;
                        }

                        // Accumulate line into batch
                        batch_bytes += line.len() + 1; // +1 for newline separator
                        batch.push(line);

                        // Drain if batch exceeds size or line limits
                        if batch_bytes >= BATCH_MAX_BYTES || batch.len() >= BATCH_MAX_LINES {
                            drain_batch_to_card(&mut card, &state, &channel_id, &thread_id, &mut batch, &mut batch_bytes).await;
                        }

                        // Reset timer on each new line
                        batch_timer.as_mut().reset(tokio::time::Instant::now() + BATCH_TIMEOUT);
                    }
                    OutputEvent::ToolAction(action) => {
                        state.liveness.update_activity(&session_id, "ToolAction");
                        tracing::info!(session_id = %session_id, action = %action, "stream_output: ToolAction");
                        // Drain any accumulated text into card before adding tool
                        drain_batch_to_card(&mut card, &state, &channel_id, &thread_id, &mut batch, &mut batch_bytes).await;
                        // Split into a new post if the current one has too many edits
                        if card.should_split() {
                            card.finalize_continuation(&state, &channel_id, &thread_id).await;
                            card.start_continuation_post();
                        }
                        card.add_tool_action(&action);
                        if card.can_update_now() {
                            card.flush(&state, &channel_id, &thread_id).await;
                        }
                    }
                    OutputEvent::TitleGenerated(title) => {
                        state.liveness.update_activity(&session_id, "TitleGenerated");
                        drain_batch_to_card(&mut card, &state, &channel_id, &thread_id, &mut batch, &mut batch_bytes).await;
                        let title = title.trim().trim_matches('"');
                        let label = format_root_label(&session_type, &project, None);
                        let _ = state.mm.update_post(&thread_id, &format!("{} — {}", label, title)).await;
                        let _ = state.mm.post_in_thread(&channel_id, &thread_id, "Title updated.").await;
                    }
                    OutputEvent::ResponseComplete { input_tokens, output_tokens, context_tokens } => {
                        state.liveness.update_activity(&session_id, "ResponseComplete");

                        // Flush any pending multi-line spawn (response ended without [/MSG])
                        if let Some(spawn_role) = pending_spawn.take()
                            && let Some((tid, sender_role)) = &team_info
                        {
                            let task = team_msg_buffer.join("\n");
                            team_msg_buffer.clear();
                            fire_team_spawn(
                                &state, tid, &channel_id, &session_id, sender_role,
                                &spawn_role, &task, &project,
                            );
                        }

                        // Flush any pending multi-line team message (response ended without [/MSG])
                        if let Some(target) = team_msg_target.take()
                            && let Some((tid, sender_role)) = &team_info
                        {
                            let message = team_msg_buffer.join("\n");
                            team_msg_buffer.clear();
                            let _ = state.db.clear_pending_task(&session_id).await;
                            if target == "__broadcast__" {
                                let preview = truncate_preview(&message, 80);
                                card.append_text(&format!("> :loudspeaker: **Broadcast**: {}", preview));
                                card.dirty = true;
                                broadcast_team_message(&state, &session_id, sender_role, &message, tid, &channel_id).await;
                            } else {
                                let preview = truncate_preview(&message, 80);
                                card.append_text(&format!("> :speech_balloon: **To {}**: {}", target, preview));
                                card.dirty = true;
                                route_team_message(&state, &session_id, sender_role, &target, &message, tid, &channel_id).await;
                            }
                        }

                        // context_tokens = actual context window usage (last API call)
                        // input_tokens = cumulative across all API calls (for billing)
                        let ctx_display = context_tokens;
                        tracing::info!(
                            session_id = %session_id,
                            input_tokens, output_tokens, context_tokens,
                            batch_len = batch.len(),
                            "stream_output: ResponseComplete"
                        );
                        // Drain remaining text into card, finalize, and flush
                        drain_batch_to_card(&mut card, &state, &channel_id, &thread_id, &mut batch, &mut batch_bytes).await;

                        card.finalize(ctx_display, output_tokens);
                        card.flush(&state, &channel_id, &thread_id).await;
                        // Reset card post_id so next turn creates a fresh card
                        card.post_id = None;

                        // Persist context_tokens (actual window size) for status display
                        state.containers.set_last_input_tokens(&session_id, ctx_display);
                        // Persist to database for cross-restart availability
                        let _ = state.db.update_context_tokens(&session_id, ctx_display).await;

                        // Clear pending task on response complete for team members
                        if team_info.is_some() {
                            let _ = state.db.clear_pending_task(&session_id).await;
                        }

                        counter!("tokens_input_total").increment(input_tokens);
                        counter!("tokens_output_total").increment(output_tokens);

                        // Warn when context window is getting full (>80%)
                        let ctx_max = config::settings().context_window_max;
                        let ctx_warn_threshold = ctx_max * 80 / 100;
                        let ctx_compact_threshold = ctx_max * 90 / 100;
                        if ctx_display > ctx_warn_threshold {
                            let usage_pct = (ctx_display as f64 / ctx_max as f64 * 100.0) as u64;
                            let msg = format!(
                                ":warning: **Context window {}% full** ({} / {}k tokens) — consider using `compact` or `clear`",
                                usage_pct, ctx_display, ctx_max / 1000,
                            );
                            let _ = state.mm.post_in_thread(&channel_id, &thread_id, &msg).await;

                            // Team-aware: notify Team Lead about high context usage
                            if let Some((ref tid, ref role)) = team_info
                                && role != "Team Lead"
                            {
                                let alert_msg = format!(
                                    "**Context Alert**: {} is at {}% context capacity ({}/{}k).\nConsider sending them a `compact` or `clear` instruction, or reassigning remaining work.",
                                    role, usage_pct, format_token_count(ctx_display), ctx_max / 1000,
                                );
                                if let Ok(leads) = state.db.get_sessions_by_role(tid, "Team Lead").await
                                    && let Some(lead) = leads.first()
                                {
                                    let header = build_team_context_header(&state.db, tid, "Team Lead").await;
                                    let _ = state.containers.send(&lead.session_id, &format!("{}\n{}", header, alert_msg)).await;
                                }
                            }
                        }

                        // Auto-compact at 90% context for team sessions
                        if ctx_display > ctx_compact_threshold
                            && let Some((ref tid, ref role)) = team_info
                        {
                            let _ = state.containers.send(&session_id, "/compact").await;
                            let _ = state.db.record_compaction(&session_id).await;

                            // Re-inject team status after compaction so the agent
                            // retains roster context despite losing conversation history
                            handle_team_status(&state, &session_id, tid).await;

                            if role != "Team Lead"
                                && let Ok(leads) = state.db.get_sessions_by_role(tid, "Team Lead").await
                                && let Some(lead) = leads.first()
                            {
                                let header = build_team_context_header(&state.db, tid, "Team Lead").await;
                                let compact_msg = format!(
                                    "{}\n**Auto-compact**: {} context auto-compacted at {}% ({}k/{}k).",
                                    header, role,
                                    (ctx_display as f64 / ctx_max as f64 * 100.0) as u64,
                                    ctx_display / 1000, ctx_max / 1000,
                                );
                                let _ = state.containers.send(&lead.session_id, &compact_msg).await;
                            }
                        }
                    }
                    OutputEvent::ProcessDied { exit_code, signal } => {
                        tracing::warn!(session_id = %session_id, ?exit_code, ?signal, "stream_output: ProcessDied");
                        drain_batch_to_card(&mut card, &state, &channel_id, &thread_id, &mut batch, &mut batch_bytes).await;

                        card.finalize_error(exit_code, &signal);
                        card.flush(&state, &channel_id, &thread_id).await;
                        card.post_id = None;

                        // Notify team about the death
                        if let Some((ref tid, ref role)) = team_info {
                            let _ = state.db.update_session_status(&session_id, "disconnected").await;
                            notify_team_session_lost(&state, tid, &session_id, role, "process died").await;
                        }
                    }
                    OutputEvent::WorkerDisconnected { reason } => {
                        tracing::warn!(session_id = %session_id, reason = %reason, "stream_output: WorkerDisconnected");
                        drain_batch_to_card(&mut card, &state, &channel_id, &thread_id, &mut batch, &mut batch_bytes).await;

                        card.finalize_error(None, &Some(format!("Worker disconnected: {}", reason)));
                        card.flush(&state, &channel_id, &thread_id).await;
                        card.post_id = None;

                        // Update session status to disconnected in DB
                        let _ = state.db.update_session_status(&session_id, "disconnected").await;

                        let _ = state.mm.post_in_thread(
                            &channel_id, &thread_id,
                            ":warning: Worker disconnected. Send a message to reconnect automatically.",
                        ).await;

                        // Notify team about the disconnection
                        if let Some((ref tid, ref role)) = team_info {
                            notify_team_session_lost(&state, tid, &session_id, role, "worker disconnected").await;
                        }
                    }
                    OutputEvent::WorkerReconnected => {
                        tracing::info!(session_id = %session_id, "stream_output: WorkerReconnected");
                        // Update session status back to active
                        let _ = state.db.update_session_status(&session_id, "active").await;

                        let _ = state.mm.post_in_thread(
                            &channel_id, &thread_id,
                            ":white_check_mark: Worker reconnected. Resuming session.",
                        ).await;

                        // Re-inject team status after reconnection so the agent
                        // has fresh roster context (especially for team leads).
                        if let Some((ref tid, _)) = team_info {
                            handle_team_status(&state, &session_id, tid).await;
                        }
                    }
                    OutputEvent::SessionIdCaptured(claude_sid) => {
                        tracing::info!(session_id = %session_id, claude_sid = %claude_sid, "stream_output: SessionIdCaptured");
                        let _ = state.db.update_claude_session_id(&session_id, &claude_sid).await;
                    }
                }
            }
            _ = &mut batch_timer => {
                // Timer expired — drain accumulated text into card and flush
                if !batch.is_empty() {
                    tracing::debug!(session_id = %session_id, batch_len = batch.len(), "stream_output: timer flush");
                }
                drain_batch_to_card(&mut card, &state, &channel_id, &thread_id, &mut batch, &mut batch_bytes).await;
                if card.dirty && card.can_update_now() {
                    card.flush(&state, &channel_id, &thread_id).await;
                }
                // Reset timer
                batch_timer.as_mut().reset(tokio::time::Instant::now() + BATCH_TIMEOUT);
            }
            _ = &mut heartbeat_timer => {
                // Heartbeat: update elapsed time on live card during silent periods
                if card.started_at.is_some() {
                    card.update_elapsed();
                    if card.can_update_now() {
                        card.flush(&state, &channel_id, &thread_id).await;
                    }
                }
                heartbeat_timer.as_mut().reset(tokio::time::Instant::now() + CARD_HEARTBEAT_INTERVAL);
            }
        }
    }

    // Stream ended — for team sessions, soft-delete so the session can be
    // found by team queries and potentially re-spawned.  For non-team sessions,
    // use the full cleanup path (atomic, prevents double-decrement).
    if team_info.is_some() {
        // Soft-delete: mark disconnected, release in-memory session, but preserve
        // the DB record and worktree so the Team Lead can see the member's status
        // and the work-in-progress is not lost.
        let _ = state
            .db
            .update_session_status(&session_id, "disconnected")
            .await;

        // Release the in-memory session (channel is dead anyway) but keep DB record
        if let Some(claimed) = state.containers.claim_session(&session_id) {
            state.liveness.remove(&session_id);
            let _ = state
                .containers
                .release_session(&state.db, &claimed.repo, &claimed.branch)
                .await;
            state.git.release_repo_by_session(&session_id);
            // NOTE: do NOT clean up worktree — it contains the member's work
            gauge!("active_sessions").decrement(1.0);
        }

        tracing::info!(
            session_id = %session_id,
            channel_id = %channel_id,
            "Team session stream ended (soft-delete — DB record preserved)"
        );
        let _ = state
            .mm
            .post_in_thread(&channel_id, &thread_id, "Session ended (disconnected).")
            .await;
    } else {
        // Standard session — full cleanup
        cleanup_session(&state, &session_id).await;

        tracing::info!(
            session_id = %session_id,
            channel_id = %channel_id,
            "Session stream ended"
        );
        if let Err(e) = state
            .mm
            .post_in_thread(&channel_id, &thread_id, "Session ended.")
            .await
        {
            tracing::warn!(error = %e, "Failed to post session end message");
        }
    }
}

// ==================== Team Functions ====================

/// Ensure the shared coordination channel exists, creating it if needed.
async fn ensure_coordination_channel(state: &AppState) -> Option<String> {
    // Check if we already have it cached
    {
        let cached = state.coordination_channel_id.read().await;
        if let Some(ref id) = *cached {
            return Some(id.clone());
        }
    }

    // Try to find or create the channel
    let s = config::settings();
    let channel_name = "team-coordination-log";
    let channel_id = match state
        .mm
        .get_channel_by_name(&s.mattermost_team_id, channel_name)
        .await
    {
        Ok(Some(id)) => id,
        Ok(None) => {
            // Create the channel
            match state
                .mm
                .create_channel(
                    &s.mattermost_team_id,
                    channel_name,
                    "Team Coordination Log",
                    "Shared coordination channel for all team activities",
                )
                .await
            {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create coordination channel");
                    return None;
                }
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to look up coordination channel");
            return None;
        }
    };

    // Cache it
    let mut cached = state.coordination_channel_id.write().await;
    *cached = Some(channel_id.clone());
    Some(channel_id)
}

/// Set up a per-team coordination thread in the shared coordination channel.
async fn setup_coordination_thread(state: &AppState, team_id: &str, project: &str) {
    let Some(coord_channel_id) = ensure_coordination_channel(state).await else {
        return;
    };

    let team_id_prefix = &team_id[..8.min(team_id.len())];
    let root_msg = format!(
        "**Team Coordination Log** — {} (team {})",
        project, team_id_prefix,
    );

    match state.mm.post_root(&coord_channel_id, &root_msg).await {
        Ok(post_id) => {
            if let Err(e) = state
                .db
                .update_team_coordination_thread(team_id, &post_id)
                .await
            {
                tracing::warn!(error = %e, "Failed to store coordination thread ID");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to create coordination thread root post");
        }
    }
}

/// Post a message to the team's coordination log thread.
async fn post_to_coordination_log(state: &AppState, team_id: &str, message: &str) {
    let Some(coord_channel_id) = ensure_coordination_channel(state).await else {
        return;
    };

    let team = match state.db.get_team(team_id).await {
        Ok(Some(t)) => t,
        _ => return,
    };

    let Some(thread_id) = team.coordination_thread_id else {
        return;
    };

    let _ = state
        .mm
        .post_in_thread(&coord_channel_id, &thread_id, message)
        .await;
}

/// Build the team context header prepended to every message delivered to a team member.
async fn build_team_context_header(db: &Database, team_id: &str, recipient_role: &str) -> String {
    let members = db.get_team_members(team_id).await.unwrap_or_default();
    format_team_context_header(&members, recipient_role)
}

/// Build context header from a pre-fetched member list (avoids repeated DB queries).
/// Includes task status so the recipient can see who is busy vs idle.
fn format_team_context_header(members: &[StoredSession], recipient_role: &str) -> String {
    let member_list: Vec<String> = members
        .iter()
        .filter_map(|m| {
            let role = m.role.as_deref()?;
            let status = if m.pending_task_from.is_some() {
                let task_preview = m
                    .current_task
                    .as_deref()
                    .unwrap_or("task")
                    .chars()
                    .take(40)
                    .collect::<String>();
                format!("{} (busy: {})", role, task_preview)
            } else {
                format!("{} (idle)", role)
            };
            Some(status)
        })
        .collect();
    format!(
        "[TEAM: You are {}. Members: {}. \
         Use markers to communicate: [SPAWN:Role] task, [TO:Role] message, [BROADCAST] message, [TEAM_STATUS]]",
        recipient_role,
        member_list.join(", ")
    )
}

/// Build the Team Lead's system prompt (static, set at session creation).
async fn build_team_lead_prompt(db: &Database, dev_count: i32) -> Result<String> {
    let roles = db.get_all_team_roles().await?;
    let mut roles_table = String::from(
        "| Role | Description | Multiple | Worktree |\n|------|-------------|----------|----------|\n",
    );
    for r in &roles {
        if r.role_name == "team_lead" {
            continue;
        }
        roles_table.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            r.display_name,
            r.description,
            if r.allow_multiple { "yes" } else { "no" },
            if r.default_worktree { "yes" } else { "no" },
        ));
    }

    let lead_role = roles.iter().find(|r| r.role_name == "team_lead");
    let responsibilities = lead_role.map(|r| r.responsibilities.as_str()).unwrap_or(
        "Analyze tasks, spawn team members, coordinate work, resolve conflicts, review and complete PRs"
    );

    Ok(format!(
        r#"You are the **Team Lead** of a development team coordinated through Mattermost.
The user requested {dev_count} developer(s) for this team.

## Available Roles
{roles_table}

## Spawning Team Members
IMPORTANT: Before spawning, ALWAYS check if existing team members of the same role are idle — use [TO:Role] to assign work to idle members instead of spawning new ones. The system will REJECT spawn requests when idle members of that role exist.
Run [TEAM_STATUS] first to check the current roster and avoid duplicates.
Spawn markers MUST start at the beginning of a new line. Include the task, then end with [/MSG] on its own line.
  [SPAWN:Developer] Short task
  [/MSG]
Or multi-line:
  [SPAWN:Developer]
  Detailed task description
  spanning multiple lines
  [/MSG]
  [SPAWN:Developer --worktree] Force worktree isolation
Developer roles are auto-numbered (Developer 1, Developer 2, ...). Other roles are singletons.

## Communication Markers
CRITICAL: All markers MUST start at the beginning of a new line. Do NOT embed markers in the middle of a sentence or paragraph — they will be ignored. Each marker must be the first thing on its line.
  [TO:Role Name] message [/MSG]
  [TO:Role Name]
  Multi-line message
  [/MSG]
  [BROADCAST] message [/MSG]
  [TEAM_STATUS]
[/MSG] must also be on its own line when ending a multi-line message.

Messages from team members arrive prefixed with their role.
Each message you receive will include a [TEAM: ...] header with the current roster.

**PR Review Gate**: Team members use [PR_READY:N] and [PR_REVIEWED:N] markers. The system ensures they address Claude Reviewer CI comments before the PR reaches you. You will receive a notification when a PR has passed the review gate.

## Your Responsibilities
{responsibilities}

## Task Assignment Protocol
- After assigning a task via [TO:Role] or [SPAWN:Role], WAIT for that member to respond before sending another task
- The system enforces this — sending to a member with a pending task will be rejected
- Do NOT assign the same task to multiple developers unless you explicitly want parallel approaches
- Use [TEAM_STATUS] to check member status before sending follow-up tasks
- If a member is stalled, check their status before re-assigning their task to someone else

## Development Workflow (spec-flow)
The team follows the spec-flow workflow. Coordinate members through these phases IN ORDER:

1. **Specify** (Architect): Spawn Architect first to use the spec-flow `specify` tool for spec issues, `clarify` to resolve gaps
2. **Plan** (You): Once specs are approved, use the spec-flow `plan` tool to break specs into stories
3. **Implement** (Developers): Only AFTER stories exist, spawn Developers who use the spec-flow `implement` tool for each story
4. **Review** (You): Review PRs using gh CLI, delegate fixes via [TO:Role], merge when CI passes

IMPORTANT: Do NOT spawn Developers until stories have been planned from approved specs.

## PR Review & Completion
Team members use the [PR_READY:N] / [PR_REVIEWED:N] review gate. The Claude Reviewer CI automatically reviews PRs and will APPROVE them once all comments are addressed. PRs only reach you after the Claude Reviewer has approved AND the team member has completed the gate.

**CRITICAL MERGE RULE**: NEVER merge a PR unless you received an explicit "review gate PASSED" notification from the system for that specific PR number. If a team member mentions a PR via [TO:Team Lead] without the review gate notification, do NOT merge it — tell them to use [PR_READY:N] first. Only PRs delivered to you with "review gate PASSED" in the message are cleared for merge.

When you receive a review-gate-passed PR notification:

1. **Verify Claude Reviewer approved**: `gh pr view <number> --json reviews --jq '.reviews[] | "\(.state): \(.body)"'` — confirm the Claude Reviewer has APPROVED
2. **Verify all CI passes**: `gh pr checks <number>` — all checks must be green
3. **Review the diff**: `gh pr view <number>` and `gh pr diff <number>` — sanity-check the implementation
4. **Verify architectural alignment**: Complex fixes (interface changes, new dependencies, restructuring, security patterns) should have been escalated to the Architect during the review gate. Check that the Architect approved these changes before merging. If you see architectural changes without Architect sign-off, send the PR back.
5. If changes needed, send SPECIFIC feedback via [TO:Role] with file paths, line numbers, and what needs to change
6. Wait for fixes before re-reviewing — do not merge until Claude Reviewer has approved and all CI passes
7. After merging implementation PRs, ask Architect to run the spec-flow `architecture` tool to update project docs
8. Merge with `gh pr merge <number> --squash` ONLY when ALL of the above are satisfied"#,
    ))
}

/// Build a team member's system prompt (static, set at session creation).
fn build_team_member_prompt(role: &StoredTeamRole) -> String {
    format!(
        r#"You are the **{display_name}** on a development team.

## Communication Markers
CRITICAL: All markers MUST start at the beginning of a new line. Do NOT embed markers in the middle of a sentence or paragraph — they will be ignored. Each marker must be the first thing on its line.
  [TO:Role Name] message [/MSG]
  [TO:Role Name]
  Multi-line message
  [/MSG]
  [BROADCAST] message [/MSG]
  [TEAM_STATUS]
[/MSG] must also be on its own line when ending a multi-line message.

Messages from other members arrive prefixed with their role.
Each message you receive will include a [TEAM: ...] header with the current roster.

## Your Responsibilities
{responsibilities}

## Workflow
- Wait for task assignment from the Team Lead before starting work
- ALWAYS report back to the Team Lead when your task is complete using [TO:Team Lead] with a summary of what you did
- After completing any assigned work (reviews, architecture updates, analysis), notify Team Lead: [TO:Team Lead] with results
- Address review feedback promptly when delegated by Team Lead
- Do NOT start new work until your current PR is merged or you are reassigned
- One task at a time — finish current work before accepting new assignments
- IMPORTANT: Never finish a response without sending [TO:Team Lead] — the Team Lead cannot see your work unless you explicitly send it

## PR Review Gate (IMPORTANT)
After creating a PR, do NOT send it directly to the Team Lead. Instead use the review gate:
1. Signal the PR is ready: `[PR_READY:N]` (where N is the PR number)
2. The system will instruct you to wait for the Claude Reviewer CI to post its review
3. If the Claude Reviewer **APPROVES** → skip to step 7
4. If it **REQUESTS CHANGES** — categorise each comment as Simple or Complex:
   - **Simple** (fix yourself): code style, naming, missing error handling, missing tests, minor localised bugs, dead code
   - **Complex** (escalate to Architect): interface/API changes, new dependencies, module restructuring, security/auth pattern changes, design pattern deviations, schema changes
5. Fix Simple issues with actual code changes (documentation-only fixes are NOT acceptable)
6. For Complex issues: send your proposed solution to the Architect via [TO:Architect] — wait for their feedback, then implement. Push fixes and repeat from step 2 until the Claude Reviewer APPROVES.
7. Signal completion: `[PR_REVIEWED:N]` — the system notifies the Team Lead for final review and merge
Do NOT use [TO:Team Lead] for PR notifications — use [PR_READY:N] and [PR_REVIEWED:N] instead."#,
        display_name = role.display_name,
        responsibilities = role.responsibilities,
    )
}

/// Reconstruct the system prompt for a session based on its type and team role.
/// Used on resume/reconnect to restore the session's identity and available commands.
async fn rebuild_system_prompt(
    db: &Database,
    session_type: &str,
    team_id: Option<&str>,
    role: Option<&str>,
) -> Option<String> {
    match session_type {
        "team_lead" => {
            if let Some(tid) = team_id {
                let dev_count = db
                    .get_team(tid)
                    .await
                    .ok()
                    .flatten()
                    .map(|t| t.dev_count)
                    .unwrap_or(1);
                match build_team_lead_prompt(db, dev_count).await {
                    Ok(prompt) => Some(prompt),
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to rebuild team lead prompt on resume");
                        None
                    }
                }
            } else {
                None
            }
        }
        "team_member" => {
            if let Some(role_name) = role {
                // Map display name back to role_name for DB lookup
                let role_key = match role_name.split_whitespace().next().unwrap_or(role_name) {
                    "Developer" => "developer",
                    "Architect" | "PM/Architect" => "architect",
                    "QA" => "qa_engineer",
                    "DevOps" => "devops_engineer",
                    _ => role_name,
                };
                match db.get_team_role(role_key).await {
                    Ok(Some(role_def)) => Some(build_team_member_prompt(&role_def)),
                    _ => {
                        tracing::warn!(role = %role_name, "Failed to find role definition for resume");
                        None
                    }
                }
            } else {
                None
            }
        }
        _ => None, // Standard sessions don't have a system prompt
    }
}

/// Truncate a string to at most `max_bytes` at a valid UTF-8 char boundary, appending "…" if truncated.
fn truncate_preview(s: &str, max_bytes: usize) -> String {
    if s.len() > max_bytes {
        let end = s
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= max_bytes)
            .last()
            .unwrap_or(0);
        format!("{}…", &s[..end])
    } else {
        s.to_string()
    }
}

/// Route a message from one team member to another (or all matching a role).
/// `[TO:Developer]` fans out to "Developer", "Developer 1", "Developer 2", etc.
async fn route_team_message(
    state: &AppState,
    sender_session_id: &str,
    sender_role: &str,
    target_role: &str,
    message: &str,
    team_id: &str,
    _channel_id: &str,
) {
    // Find target sessions by role (exact + prefix match).
    // Retry briefly if no target found — a just-spawned member may still be starting up.
    let mut targets = Vec::new();
    for attempt in 0..6 {
        match state.db.get_sessions_by_role(team_id, target_role).await {
            Ok(sessions) if !sessions.is_empty() => {
                targets = sessions;
                break;
            }
            Ok(_) => {
                if attempt < 5 {
                    tracing::debug!(
                        team_id,
                        target_role,
                        attempt,
                        "Target role not found yet, retrying"
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                } else {
                    tracing::warn!(
                        team_id,
                        target_role,
                        "Team message target not found after retries"
                    );
                    if let Ok(Some(sender)) = state
                        .db
                        .get_session_by_id_prefix(
                            &sender_session_id[..8.min(sender_session_id.len())],
                        )
                        .await
                    {
                        let _ = state
                            .mm
                            .post_in_thread(
                                &sender.channel_id,
                                &sender.thread_id,
                                &format!(
                                    ":warning: No active team member with role **{}**.",
                                    target_role
                                ),
                            )
                            .await;
                    }
                    return;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to look up team member by role");
                return;
            }
        }
    }

    // Deliver to matching sessions (fetch member list once)
    let members = state.db.get_team_members(team_id).await.unwrap_or_default();

    // Determine if this is a prefix match (e.g., [TO:Developer] matching Developer 1, Developer 2).
    // Exact match (e.g., [TO:Developer 1]) targets just that one session.
    let is_prefix_match = targets.len() > 1
        || (targets.len() == 1
            && targets[0]
                .role
                .as_deref()
                .map(|r| r != target_role)
                .unwrap_or(false));

    // When a prefix match hits multiple members, pick ONE idle member instead of
    // fanning out the same task to everyone. This prevents duplicate work.
    let effective_targets: Vec<&StoredSession> = if is_prefix_match && targets.len() > 1 {
        // Prefer idle members (no pending task from anyone)
        let idle: Vec<&StoredSession> = targets
            .iter()
            .filter(|t| t.pending_task_from.is_none())
            .collect();
        if idle.is_empty() {
            // All busy — reject with a clear message listing who is busy
            let busy_list: Vec<String> = targets
                .iter()
                .filter_map(|t| {
                    let role = t.role.as_deref()?;
                    let task = t.current_task.as_deref().unwrap_or("unknown task");
                    Some(format!("{} (busy: {})", role, task))
                })
                .collect();
            if let Ok(Some(sender)) = state
                .db
                .get_session_by_id_prefix(&sender_session_id[..8.min(sender_session_id.len())])
                .await
            {
                let header = format_team_context_header(&members, sender_role);
                let _ = state.containers.send(&sender.session_id, &format!(
                    "{}\nAll {} members are busy: {}. Wait for one to finish, or use [TEAM_STATUS] to check progress.",
                    header, target_role, busy_list.join(", "),
                )).await;
                let _ = state.mm.post_in_thread(
                    &sender.channel_id, &sender.thread_id,
                    &format!(":hourglass: All **{}** members are busy. Wait for a response.", target_role),
                ).await;
            }
            return;
        }
        // Pick the first idle member (deterministic, lowest instance number)
        vec![idle[0]]
    } else {
        targets.iter().collect()
    };

    let mut delivered_roles = Vec::new();
    for target in &effective_targets {
        let role = target.role.as_deref().unwrap_or(target_role);

        // Hard gate: check if target already has a pending task from the same sender
        if let Some(ref pending_from) = target.pending_task_from
            && pending_from == sender_session_id
        {
            // Reject — same sender already has a pending task
            if let Ok(Some(sender)) = state
                .db
                .get_session_by_id_prefix(&sender_session_id[..8.min(sender_session_id.len())])
                .await
            {
                let _ = state.mm.post_in_thread(
                    &sender.channel_id, &sender.thread_id,
                    &format!(":hourglass: **{}** already has a pending task. Wait for their response.", role),
                ).await;
                // Inject rejection into sender's Claude session
                let header = format_team_context_header(&members, sender_role);
                let _ = state.containers.send(&sender.session_id, &format!(
                    "{}\nTask delivery rejected: {} already has a pending task from you. Wait for their response before sending another task.",
                    header, role,
                )).await;
            }
            continue;
        }

        let header = format_team_context_header(&members, role);
        let formatted = format!("{}\n**From {}**: {}", header, sender_role, message);
        // Set pending task and post Mattermost visibility BEFORE queueing the message,
        // so the message appears in chat before the target starts processing.
        let _ = state
            .db
            .set_pending_task(&target.session_id, sender_session_id, message)
            .await;
        let _ = state
            .mm
            .post_in_thread(
                &target.channel_id,
                &target.thread_id,
                &format!(
                    ":arrow_left: **From {}:**\n{}",
                    sender_role,
                    truncate_preview(message, 4000)
                ),
            )
            .await;
        if let Err(e) = state.containers.send(&target.session_id, &formatted).await {
            tracing::warn!(error = %e, role, "Failed to deliver team message");
            // Notify sender that delivery failed — their message was lost
            if let Ok(Some(sender)) = state
                .db
                .get_session_by_id_prefix(&sender_session_id[..8.min(sender_session_id.len())])
                .await
            {
                let header = format_team_context_header(&members, sender_role);
                let fail_msg = format!(
                    "{}\n:x: **Message delivery to {} failed** — their session may have died or disconnected.\nUse [TEAM_STATUS] to check the roster. You may need to re-spawn the role or reassign the task.",
                    header, role,
                );
                let _ = state.containers.send(&sender.session_id, &fail_msg).await;
                let _ = state
                    .mm
                    .post_in_thread(
                        &sender.channel_id,
                        &sender.thread_id,
                        &format!(
                            ":x: Failed to deliver message to **{}** — session unreachable.",
                            role
                        ),
                    )
                    .await;
            }
            // Clear the pending task we just set — delivery didn't happen
            let _ = state.db.clear_pending_task(&target.session_id).await;
        } else {
            delivered_roles.push(role.to_string());
        }
    }

    // Post delivery note in sender's thread + coordination log
    if !delivered_roles.is_empty()
        && let Ok(Some(sender)) = state
            .db
            .get_session_by_id_prefix(&sender_session_id[..8.min(sender_session_id.len())])
            .await
    {
        let label = if delivered_roles.len() == 1 {
            delivered_roles[0].clone()
        } else {
            delivered_roles.join(", ")
        };
        // Show the outgoing message content so the user has full visibility
        let _ = state
            .mm
            .post_in_thread(
                &sender.channel_id,
                &sender.thread_id,
                &format!(
                    ":arrow_right: **To {}:**\n{}",
                    label,
                    truncate_preview(message, 4000)
                ),
            )
            .await;

        // Inject delivery receipt into sender's Claude session
        let header = format_team_context_header(&members, sender_role);
        let _ = state.containers.send(&sender.session_id, &format!(
            "{}\nDelivery receipt: message delivered to {}. Wait for their response before sending another task.",
            header, label,
        )).await;

        // Post to coordination log
        post_to_coordination_log(
            state,
            team_id,
            &format!(
                ":arrow_right: **{} → {}:** {}",
                sender_role,
                label,
                truncate_preview(message, 4000),
            ),
        )
        .await;
    }
}

/// Broadcast a message to all team members except the sender.
async fn broadcast_team_message(
    state: &AppState,
    sender_session_id: &str,
    sender_role: &str,
    message: &str,
    team_id: &str,
    _channel_id: &str,
) {
    let members = match state.db.get_team_members(team_id).await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "Failed to get team members for broadcast");
            return;
        }
    };

    let mut sent_count = 0;
    for member in &members {
        if member.session_id == sender_session_id {
            continue;
        }
        let target_role = member.role.as_deref().unwrap_or("Unknown");
        let header = format_team_context_header(&members, target_role);
        let formatted = format!(
            "{}\n**From {} (broadcast)**: {}",
            header, sender_role, message
        );
        if state
            .containers
            .send(&member.session_id, &formatted)
            .await
            .is_ok()
        {
            sent_count += 1;
            // Post incoming broadcast in receiver's Mattermost thread for visibility
            let _ = state
                .mm
                .post_in_thread(
                    &member.channel_id,
                    &member.thread_id,
                    &format!(
                        ":arrow_left: **From {} (broadcast):**\n{}",
                        sender_role,
                        truncate_preview(message, 4000)
                    ),
                )
                .await;
        }
    }

    // Post delivery note in sender's thread + coordination log
    if let Ok(Some(sender)) = state
        .db
        .get_session_by_id_prefix(&sender_session_id[..8.min(sender_session_id.len())])
        .await
    {
        // Show the broadcast content so the user has full visibility
        let _ = state
            .mm
            .post_in_thread(
                &sender.channel_id,
                &sender.thread_id,
                &format!(
                    ":loudspeaker: **Broadcast to {} member(s):**\n{}",
                    sent_count,
                    truncate_preview(message, 4000)
                ),
            )
            .await;

        // Inject delivery receipt into sender's Claude session
        let header = format_team_context_header(&members, sender_role);
        let _ = state.containers.send(&sender.session_id, &format!(
            "{}\nDelivery receipt: broadcast delivered to {} member(s). Wait for responses before sending follow-up tasks.",
            header, sent_count,
        )).await;
    }

    // Post to coordination log
    post_to_coordination_log(
        state,
        team_id,
        &format!(
            ":loudspeaker: **{} (broadcast):** {}",
            sender_role,
            truncate_preview(message, 4000),
        ),
    )
    .await;
}

/// Handle a [SPAWN:Role] marker from the Team Lead.
/// Uses explicit boxed future return to break the async recursion cycle:
/// start_session → stream_output → handle_team_spawn → start_session
#[allow(clippy::too_many_arguments)]
fn handle_team_spawn(
    state: Arc<AppState>,
    team_id: &str,
    channel_id: &str,
    lead_session_id: &str,
    _lead_role: &str,
    role_name: &str,
    initial_task: &str,
    project: &str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    // Own all string params for the async block
    let team_id = team_id.to_string();
    let channel_id = channel_id.to_string();
    let lead_session_id = lead_session_id.to_string();
    let role_name = role_name.to_string();
    let initial_task = initial_task.to_string();
    let project = project.to_string();
    Box::pin(async move {
        let team_id = team_id.as_str();
        let channel_id = channel_id.as_str();
        let lead_session_id = lead_session_id.as_str();
        let role_name = role_name.as_str();
        let initial_task = initial_task.as_str();
        let project = project.as_str();
        // Look up the team to get project info
        let _team = match state.db.get_team(team_id).await {
            Ok(Some(t)) => t,
            _ => {
                tracing::error!(team_id, "Team not found for spawn");
                return;
            }
        };

        // Look up the role in the team_roles table (case-insensitive fuzzy match)
        let role_def = match find_team_role(&state.db, role_name).await {
            Some(r) => r,
            None => {
                // Unknown role — create session with generic prompt
                StoredTeamRole {
                    role_name: role_name.to_lowercase().replace(' ', "_"),
                    display_name: role_name.to_string(),
                    description: format!("Custom role: {}", role_name),
                    responsibilities:
                        "Complete assigned tasks and communicate results to the team.".to_string(),
                    default_worktree: false,
                    allow_multiple: false,
                    sort_order: 99,
                }
            }
        };

        // Determine the actual role display name (with instance numbering for multi-instance roles)
        let members = state.db.get_team_members(team_id).await.unwrap_or_default();
        let display_name = if role_def.allow_multiple {
            // Before spawning a new instance, check if any existing members of this
            // role are idle (active status, no pending task).  If so, reject the
            // spawn and tell the lead to assign work to the idle member first.
            let idle_same_role: Vec<&StoredSession> = members
                .iter()
                .filter(|m| {
                    m.role
                        .as_deref()
                        .map(|r| r.starts_with(&role_def.display_name))
                        .unwrap_or(false)
                        && m.status == "active"
                        && m.pending_task_from.is_none()
                })
                .collect();
            if !idle_same_role.is_empty() {
                let idle_names: Vec<String> = idle_same_role
                    .iter()
                    .filter_map(|m| m.role.clone())
                    .collect();
                let idle_list = idle_names.join(", ");
                tracing::info!(team_id, role = %role_def.display_name, idle = %idle_list,
                "Spawn rejected: idle members of same role available");
                if let Ok(Some(lead)) = state
                    .db
                    .get_session_by_id_prefix(&lead_session_id[..8.min(lead_session_id.len())])
                    .await
                {
                    let reject_msg = format!(
                        ":warning: **Spawn rejected**: {} idle — assign work with `[TO:{}] task [/MSG]` before spawning another.",
                        idle_list, idle_names[0],
                    );
                    let _ = state
                        .mm
                        .post_in_thread(&lead.channel_id, &lead.thread_id, &reject_msg)
                        .await;
                    let header = build_team_context_header(&state.db, team_id, "Team Lead").await;
                    let _ = state.containers.send(&lead.session_id, &format!(
                    "{}\nSpawn rejected: {} is idle and available. Use [TO:{}] to assign them a task before spawning a new {}.\nThe task you wanted to assign was: {}",
                    header, idle_list, idle_names[0], role_def.display_name, initial_task,
                )).await;
                }
                return;
            }

            // Count existing instances of this base role
            let existing_count = members
                .iter()
                .filter(|m| {
                    m.role
                        .as_deref()
                        .map(|r| r.starts_with(&role_def.display_name))
                        .unwrap_or(false)
                })
                .count();
            let instance_num = existing_count + 1;
            format!("{} {}", role_def.display_name, instance_num)
        } else {
            // Singleton: check for duplicates
            let already_exists = members
                .iter()
                .any(|m| m.role.as_deref() == Some(&role_def.display_name));
            if already_exists {
                // Post error in Lead's thread
                if let Ok(Some(lead)) = state
                    .db
                    .get_session_by_id_prefix(&lead_session_id[..8.min(lead_session_id.len())])
                    .await
                {
                    let _ = state
                        .mm
                        .post_in_thread(
                            &lead.channel_id,
                            &lead.thread_id,
                            &format!(
                                ":x: **{}** is already on the team. This role is a singleton.",
                                role_def.display_name
                            ),
                        )
                        .await;
                    let header = build_team_context_header(&state.db, team_id, "Team Lead").await;
                    let _ = state
                        .containers
                        .send(
                            &lead.session_id,
                            &format!(
                                "{}\nSpawn rejected: {} is already on the team (singleton role).",
                                header, role_def.display_name,
                            ),
                        )
                        .await;
                }
                return;
            }
            role_def.display_name.clone()
        };

        // Team-spawned sessions always use worktrees for isolation.
        // The Team Lead owns the main clone; members must work in worktrees
        // to avoid repository lock conflicts (see #35).
        let use_worktree = true; // force_worktree and role_def.default_worktree are subsumed

        // Parse repo reference
        let s = config::settings();
        let repo_ref = match RepoRef::parse_with_default_org(project, s.default_org.as_deref()) {
            Some(mut r) => {
                if use_worktree {
                    r.worktree = Some(session_manager::git::WorktreeMode::Named(
                        display_name.to_lowercase().replace(' ', "-"),
                    ));
                }
                r
            }
            None => {
                tracing::error!(project, "Failed to parse repo ref for team spawn");
                return;
            }
        };

        // Build member system prompt
        let member_prompt = build_team_member_prompt(&role_def);

        // Determine session type
        let session_type = "team_member";

        // Start the member session
        let session_result: Result<String> = Box::pin(start_session(
            &state,
            channel_id,
            &repo_ref,
            session_type,
            false,
            false,
            None, // user_id — agent-spawned, no user
            Some(team_id),
            Some(&display_name),
            Some(&member_prompt),
        ))
        .await;
        match session_result {
            Ok(member_session_id) => {
                // Send initial task if provided + set pending task
                if !initial_task.is_empty() {
                    let header = build_team_context_header(&state.db, team_id, &display_name).await;
                    let formatted_task =
                        format!("{}\n**From Team Lead**: {}", header, initial_task);
                    let _ = state
                        .containers
                        .send(&member_session_id, &formatted_task)
                        .await;
                    // Track the task assignment
                    let _ = state
                        .db
                        .set_pending_task(&member_session_id, lead_session_id, initial_task)
                        .await;
                }

                // Notify Lead about successful spawn (both Mattermost and session context)
                if let Ok(Some(lead)) = state
                    .db
                    .get_session_by_id_prefix(&lead_session_id[..8.min(lead_session_id.len())])
                    .await
                {
                    // Show spawn confirmation with initial task for full visibility
                    let spawn_msg = if initial_task.is_empty() {
                        format!(":white_check_mark: Spawned **{}**", display_name)
                    } else {
                        format!(
                            ":white_check_mark: Spawned **{}:**\n{}",
                            display_name,
                            truncate_preview(initial_task, 4000)
                        )
                    };
                    let _ = state
                        .mm
                        .post_in_thread(&lead.channel_id, &lead.thread_id, &spawn_msg)
                        .await;
                    // Feed confirmation back into the lead's Claude session so it can continue
                    let header = build_team_context_header(&state.db, team_id, "Team Lead").await;
                    if let Err(e) = state.containers.send(&lead.session_id, &format!(
                    "{}\nSpawn confirmed: {} is now active and has been assigned the task. Wait for their response before sending another task.",
                    header, display_name,
                )).await {
                    tracing::warn!(error = %e, "Failed to send spawn confirmation to Team Lead session");
                }
                }

                // Post to coordination log
                let coord_msg = if initial_task.is_empty() {
                    format!(":rocket: **Spawned {}**", display_name)
                } else {
                    format!(
                        ":rocket: **Spawned {}:** {}",
                        display_name,
                        truncate_preview(initial_task, 4000)
                    )
                };
                post_to_coordination_log(&state, team_id, &coord_msg).await;

                // Broadcast roster update to all existing members
                let update_msg = format!(
                    "**Team Roster Update**: {} has joined the team.",
                    display_name
                );
                broadcast_team_notification(&state, team_id, &update_msg, &member_session_id).await;
            }
            Err(e) => {
                tracing::error!(error = %e, role = %display_name, "Failed to spawn team member");
                if let Ok(Some(lead)) = state
                    .db
                    .get_session_by_id_prefix(&lead_session_id[..8.min(lead_session_id.len())])
                    .await
                {
                    let _ = state
                        .mm
                        .post_in_thread(
                            &lead.channel_id,
                            &lead.thread_id,
                            &format!(":x: Failed to spawn **{}**: {}", display_name, e),
                        )
                        .await;
                    let header = build_team_context_header(&state.db, team_id, "Team Lead").await;
                    let _ = state
                        .containers
                        .send(
                            &lead.session_id,
                            &format!("{}\nSpawn failed for {}: {}", header, display_name, e,),
                        )
                        .await;
                }
            }
        }
    }) // close Box::pin(async move {
}

/// Find a team role by display_name (case-insensitive fuzzy match).
async fn find_team_role(db: &Database, name: &str) -> Option<StoredTeamRole> {
    let roles = db.get_all_team_roles().await.ok()?;
    let name_lower = name.to_lowercase();
    // Exact match first
    if let Some(r) = roles
        .iter()
        .find(|r| r.display_name.to_lowercase() == name_lower)
    {
        return Some(r.clone());
    }
    // Prefix match — only if unambiguous (e.g., "QA" matches "QA Engineer" but "Dev" is
    // ambiguous between "Developer" and "DevOps Engineer")
    let prefix_matches: Vec<_> = roles
        .iter()
        .filter(|r| r.display_name.to_lowercase().starts_with(&name_lower))
        .collect();
    if prefix_matches.len() == 1 {
        return Some(prefix_matches[0].clone());
    }
    // role_name match (e.g., "developer" matches)
    if let Some(r) = roles
        .iter()
        .find(|r| r.role_name.to_lowercase() == name_lower)
    {
        return Some(r.clone());
    }
    None
}

/// Handle [TEAM_STATUS] marker — send formatted status table to requesting session.
async fn handle_team_status(state: &AppState, session_id: &str, team_id: &str) {
    let members = match state.db.get_team_members(team_id).await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "Failed to get team members for status");
            return;
        }
    };

    let ctx_max = config::settings().context_window_max;
    let ctx_max_k = ctx_max / 1000;
    let stuck_timeout = config::settings().session_stuck_timeout_secs;
    let mut table = "**Team Status:**\n| Role | Status | Context | Activity | Current Task | Last Active |\n|------|--------|---------|----------|--------------|-------------|\n".to_string();
    for member in &members {
        let role = member.role.as_deref().unwrap_or("Unknown");
        let liveness = state.liveness.get_info(&member.session_id);
        let last_active = match &liveness {
            Some(li) => format!("{} ago", format_duration_short(li.idle_duration)),
            None => "unknown".to_string(),
        };

        // Activity column: show last event type + stuck warning
        let activity_str = match &liveness {
            Some(li) => {
                if stuck_timeout > 0 && li.idle_duration.as_secs() > stuck_timeout {
                    format!(":warning: idle {}", format_duration_short(li.idle_duration))
                } else {
                    li.last_event_type.clone()
                }
            }
            None => "unknown".to_string(),
        };

        // Context from in-memory or DB
        let ctx_tokens = state
            .containers
            .get_session_info(&member.session_id)
            .map(|i| i.last_input_tokens)
            .unwrap_or(member.context_tokens as u64);
        let ctx_str = if ctx_tokens > 0 {
            let pct = (ctx_tokens as f64 / ctx_max as f64 * 100.0) as u64;
            format!("{}% ({}k/{}k)", pct, ctx_tokens / 1000, ctx_max_k)
        } else {
            "—".to_string()
        };

        // Current task display
        let task_str = match (&member.current_task, &member.pending_task_from) {
            (Some(task), Some(_)) => format!("{} (pending response)", truncate_preview(task, 60)),
            (Some(task), None) => truncate_preview(task, 60),
            (None, _) => "—".to_string(),
        };

        table.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            role, member.status, ctx_str, activity_str, task_str, last_active,
        ));
    }

    // Append review-gated PRs status
    let reviewed: Vec<String> = state
        .reviewed_prs
        .iter()
        .filter_map(|entry| {
            let key = entry.key();
            if key.starts_with(&format!("{}:", team_id)) {
                Some(format!("#{}", &key[team_id.len() + 1..]))
            } else {
                None
            }
        })
        .collect();
    if !reviewed.is_empty() {
        table.push_str(&format!(
            "\n**PRs cleared for merge (review gate passed):** {}\n",
            reviewed.join(", ")
        ));
    }

    // Send back to requesting session
    if let Err(e) = state.containers.send(session_id, &table).await {
        tracing::warn!(error = %e, "Failed to send team status");
    }
}

/// Broadcast a notification to all team members (with context header).
async fn broadcast_team_notification(
    state: &AppState,
    team_id: &str,
    message: &str,
    exclude_session_id: &str,
) {
    let members = state.db.get_team_members(team_id).await.unwrap_or_default();
    for member in &members {
        if member.session_id == exclude_session_id {
            continue;
        }
        let role = member.role.as_deref().unwrap_or("Unknown");
        let header = build_team_context_header(&state.db, team_id, role).await;
        let _ = state
            .containers
            .send(&member.session_id, &format!("{}\n{}", header, message))
            .await;
    }
}

/// Handle [PR_READY:N] — team member signals their PR is ready.
/// Instead of forwarding directly to the team lead, inject review gate instructions
/// so the member addresses Claude Reviewer CI comments first.
async fn handle_pr_ready(
    state: &AppState,
    session_id: &str,
    team_id: &str,
    sender_role: &str,
    pr_number: &str,
    _channel_id: &str,
) {
    let header = build_team_context_header(&state.db, team_id, sender_role).await;

    // Look up the project for the repo owner/name
    let project = state
        .db
        .get_session_by_id_prefix(&session_id[..8.min(session_id.len())])
        .await
        .ok()
        .flatten()
        .map(|s| s.project)
        .unwrap_or_default();
    let repo_slug = if project.contains('/') {
        project.clone()
    } else {
        // Fallback — the member can figure it out from their working directory
        String::new()
    };

    let gate_msg = format!(
        r#"{header}
**PR Review Gate — PR #{pr}**

Before this PR is forwarded to the Team Lead, you MUST complete the following:

### Step 1: Wait for Claude Reviewer and check its decision
```
gh pr checks {pr} --watch
gh pr view {pr} --json reviews --jq '.reviews[] | "\(.state): \(.body)"'
```
The Claude Reviewer will either **APPROVE** the PR or **REQUEST_CHANGES** with inline comments.
- If APPROVED with no changes requested → skip to Step 5
- If CHANGES_REQUESTED → continue to Step 2

### Step 2: Fetch and categorise review comments
```
gh api repos/{repo}/pulls/{pr}/comments --jq '.[] | "[\(.path):\(.line // .original_line)] \(.body)"'
```
For EVERY review comment, categorise it as **Simple** or **Complex**:

**Simple** — fix directly yourself:
- Code style, formatting, naming conventions
- Missing or inadequate error handling (adding try/catch, nil checks, etc.)
- Missing tests or test coverage gaps
- Minor logic bugs with an obvious localised fix
- Unused imports, dead code, minor refactors within a single function

**Complex** — requires Architect input before fixing:
- Changes to public interfaces, API contracts, or function signatures used by other modules
- Adding new dependencies or replacing existing ones
- Restructuring modules, moving responsibilities between components
- Changes to security, authentication, or authorisation patterns
- Deviating from established design patterns or architectural conventions
- Performance-critical changes that alter data flow or concurrency model
- Database schema changes or migration modifications

### Step 3: Fix Simple comments
Address them with actual code changes. Rules:
- **Documentation-only fixes are NOT acceptable** — adding comments or docstrings does not resolve code quality, logic, or security issues
- If a comment is genuinely not applicable, explain why in a brief inline code comment at the relevant line

### Step 4: Escalate Complex comments to Architect
For each Complex comment, send a message to the Architect with your proposed solution:
```
[TO:Architect]
PR #{pr} review comment requires architectural input:

**Review comment**: <paste the review comment>
**Affected code**: <file:line — what the current code does>
**Proposed fix**: <your proposed approach to address this>
**Risk**: <what else could be affected by this change>

Please advise whether this approach is acceptable or suggest an alternative.
[/MSG]
```
Wait for the Architect's response, then implement their recommended approach.
If no Architect is on the team, escalate to the Team Lead instead via [TO:Team Lead] with the same format.

### Step 4b: Push and re-check
Push your fixes (this re-triggers CI and the Claude Reviewer). Go back to Step 1.
Repeat this loop until the Claude Reviewer **APPROVES** the PR.

### Step 5: Signal completion
Once the Claude Reviewer has APPROVED the PR:
```
[PR_REVIEWED:{pr}]
```
This notifies the Team Lead that the PR is ready for final review and merge.

Do NOT use [TO:Team Lead] for this PR until you have completed this gate."#,
        header = header,
        pr = pr_number,
        repo = if repo_slug.is_empty() {
            "<owner>/<repo>".to_string()
        } else {
            repo_slug
        },
    );

    let _ = state.containers.send(session_id, &gate_msg).await;

    // Post in member's Mattermost thread for visibility
    if let Ok(Some(sess)) = state
        .db
        .get_session_by_id_prefix(&session_id[..8.min(session_id.len())])
        .await
    {
        let _ = state.mm.post_in_thread(
            &sess.channel_id, &sess.thread_id,
            &format!(":mag: **PR #{} review gate**: Waiting for {} to address Claude Reviewer comments before forwarding to Team Lead.", pr_number, sender_role),
        ).await;
    }

    // Log in coordination thread
    post_to_coordination_log(
        state, team_id,
        &format!(":mag: **{}** triggered PR #{} review gate — addressing Claude Reviewer feedback before Team Lead review", sender_role, pr_number),
    ).await;
}

/// Handle [PR_REVIEWED:N] — team member confirms they've addressed all review comments.
/// Now forward the PR to the team lead for final review.
async fn handle_pr_reviewed(
    state: &AppState,
    session_id: &str,
    team_id: &str,
    sender_role: &str,
    pr_number: &str,
    _channel_id: &str,
) {
    // Record this PR as having passed the review gate
    state
        .reviewed_prs
        .insert(format!("{}:{}", team_id, pr_number), ());

    // Find the team lead session
    let members = state.db.get_team_members(team_id).await.unwrap_or_default();
    let lead = members
        .iter()
        .find(|m| m.role.as_deref() == Some("Team Lead"));

    if let Some(lead_session) = lead {
        let header = build_team_context_header(&state.db, team_id, "Team Lead").await;
        let ready_msg = format!(
            "{}\n**PR #{} from {} — Claude Reviewer Approved (review gate PASSED)**\n\n\
            The Claude Reviewer has APPROVED this PR and {} has completed the review gate.\n\
            This PR is cleared for merge. Please perform your final review:\n\
            1. `gh pr view {} --json reviews` — confirm Claude Reviewer approval\n\
            2. `gh pr checks {}` — verify all CI checks pass\n\
            3. `gh pr diff {}` — sanity-check the implementation\n\
            4. Verify any architectural changes had Architect sign-off\n\
            5. If issues remain, send back to {} via [TO:{}] with specific feedback\n\
            6. Merge with `gh pr merge {} --squash` when satisfied",
            header,
            pr_number,
            sender_role,
            sender_role,
            pr_number,
            pr_number,
            pr_number,
            sender_role,
            sender_role,
            pr_number,
        );
        let _ = state
            .containers
            .send(&lead_session.session_id, &ready_msg)
            .await;
        // Set pending task on lead so they know they have a PR to review
        let _ = state
            .db
            .set_pending_task(
                &lead_session.session_id,
                session_id,
                &format!("Review PR #{} from {}", pr_number, sender_role),
            )
            .await;

        // Notify in lead's Mattermost thread
        let _ = state.mm.post_in_thread(
            &lead_session.channel_id, &lead_session.thread_id,
            &format!(":white_check_mark: **PR #{} from {}** — Claude Reviewer comments addressed, ready for final review.", pr_number, sender_role),
        ).await;
    }

    // Confirm to the member
    if let Ok(Some(sess)) = state
        .db
        .get_session_by_id_prefix(&session_id[..8.min(session_id.len())])
        .await
    {
        let header = build_team_context_header(&state.db, team_id, sender_role).await;
        let _ = state.containers.send(session_id, &format!(
            "{}\nPR #{} has been forwarded to the Team Lead for final review. Wait for their feedback.",
            header, pr_number,
        )).await;
        let _ = state
            .mm
            .post_in_thread(
                &sess.channel_id,
                &sess.thread_id,
                &format!(
                    ":white_check_mark: **PR #{}** forwarded to Team Lead for final review.",
                    pr_number
                ),
            )
            .await;
    }

    // Log in coordination thread
    post_to_coordination_log(
        state,
        team_id,
        &format!(
            ":white_check_mark: **{}** completed PR #{} review gate — forwarded to Team Lead",
            sender_role, pr_number
        ),
    )
    .await;
}

/// Stop an entire team (all members + lead).
async fn stop_team(state: &AppState, team_id: &str, lead_session: &database::StoredSession) {
    let members = state.db.get_team_members(team_id).await.unwrap_or_default();

    // Stop all members except the lead (which is being stopped by the caller)
    for member in &members {
        if member.session_id == lead_session.session_id {
            continue;
        }
        // Post in member's thread
        let _ = state
            .mm
            .post_in_thread(
                &member.channel_id,
                &member.thread_id,
                "Team disbanded by Lead.",
            )
            .await;
        stop_session_by_ref(state, member).await;
    }

    // Mark team as completed
    let _ = state.db.update_team_status(team_id, "completed").await;
}

/// Stop a session without the mutable borrow issues — takes a reference.
async fn stop_session_by_ref(state: &AppState, session: &database::StoredSession) {
    // Atomic claim — only one caller will succeed
    let Some(claimed) = state.containers.claim_session(&session.session_id) else {
        return;
    };

    state.liveness.remove(&session.session_id);

    // Decrement container session count
    let _ = state
        .containers
        .release_session(&state.db, &claimed.repo, &claimed.branch)
        .await;

    // Release repo lock
    state.git.release_repo_by_session(&session.session_id);

    // Clean up worktree if present
    if let Some(ref wt_path) = claimed.worktree_path {
        state.git.cleanup_worktree_by_path(wt_path).await;
    }

    // Soft-delete
    let _ = state
        .db
        .update_session_status(&session.session_id, "stopped")
        .await;

    gauge!("active_sessions").decrement(1.0);
}

/// Notify team members when a member leaves.
async fn notify_team_member_left(
    state: &AppState,
    team_id: &str,
    role_name: &str,
    session_id: &str,
) {
    let msg = format!("**Team Roster Update**: {} has left the team.", role_name);
    broadcast_team_notification(state, team_id, &msg, session_id).await;
}

/// Notify the team when a session dies or disconnects.
/// If a team member died, notify the Team Lead with the member's current task.
/// If the Team Lead died, notify all team members.
async fn notify_team_session_lost(
    state: &AppState,
    team_id: &str,
    session_id: &str,
    role: &str,
    reason: &str,
) {
    // Fetch the dying session's current task for context
    let current_task = state
        .db
        .get_session_by_id_prefix(&session_id[..8.min(session_id.len())])
        .await
        .ok()
        .flatten()
        .and_then(|s| s.current_task);

    let task_ctx = current_task
        .as_deref()
        .map(|t| format!("\n**Last known task**: {}", t))
        .unwrap_or_default();

    if role == "Team Lead" {
        // Team Lead died — notify all members
        let msg = format!(
            ":rotating_light: **Team Lead session {} ({}).**\nTeam members should save their progress. The Team Lead may reconnect or be re-started.",
            reason, reason,
        );
        broadcast_team_notification(state, team_id, &msg, session_id).await;
        post_to_coordination_log(
            state,
            team_id,
            &format!(
                ":rotating_light: **Team Lead** session {} — team members notified",
                reason,
            ),
        )
        .await;
    } else {
        // Team member died — notify the Team Lead
        if let Ok(leads) = state.db.get_sessions_by_role(team_id, "Team Lead").await
            && let Some(lead) = leads.first()
        {
            let header = build_team_context_header(&state.db, team_id, "Team Lead").await;
            let alert_msg = format!(
                "{}\n:rotating_light: **{} session {}.**{}\n\nOptions:\n- Wait for auto-reconnect (if the worker recovers)\n- Re-spawn the role: [SPAWN:{}] to create a replacement\n- Reassign the work to another team member via [TO:Role]\n\nUse [TEAM_STATUS] to check the current roster.",
                header,
                role,
                reason,
                task_ctx,
                // Extract base role name (e.g., "Developer" from "Developer 1")
                role.split_whitespace().next().unwrap_or(role),
            );
            let _ = state.containers.send(&lead.session_id, &alert_msg).await;
            let _ = state
                .mm
                .post_in_thread(
                    &lead.channel_id,
                    &lead.thread_id,
                    &format!(
                        ":rotating_light: **{}** session {} — check [TEAM_STATUS]",
                        role, reason
                    ),
                )
                .await;
        }
        post_to_coordination_log(
            state,
            team_id,
            &format!(
                ":rotating_light: **{}** session {}{}",
                role, reason, task_ctx,
            ),
        )
        .await;
    }
}

/// Session watchdog: periodically checks for stuck sessions and nudges idle
/// team leads when automation mode is enabled.
async fn session_watchdog(state: Arc<AppState>) {
    // Rate-limit tracker for automation nudges
    let nudge_tracker: DashMap<String, std::time::Instant> = DashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(60));

    loop {
        tick.tick().await;

        let timeout = config::settings().session_stuck_timeout_secs;
        if timeout == 0 {
            continue;
        }

        // --- Stuck session detection ---
        for (session_id, info) in state.liveness.all_sessions() {
            // Look up session in DB
            let session = match state.db.get_session_by_id_prefix(&session_id).await {
                Ok(Some(s)) => s,
                _ => continue,
            };

            if session.status != "active" && session.status != "stuck" {
                continue;
            }

            if info.idle_duration.as_secs() > timeout && session.status == "active" {
                // Mark as stuck in DB (no Mattermost spam)
                let _ = state.db.update_session_status(&session_id, "stuck").await;
                tracing::info!(
                    session_id = %session_id,
                    idle_secs = info.idle_duration.as_secs(),
                    last_event = %info.last_event_type,
                    role = session.role.as_deref().unwrap_or("solo"),
                    "Session marked as stuck"
                );
            } else if info.idle_duration.as_secs() <= timeout && session.status == "stuck" {
                // Session recovered — mark active again
                let _ = state.db.update_session_status(&session_id, "active").await;
                tracing::info!(
                    session_id = %session_id,
                    "Session recovered from stuck state"
                );
            }
        }

        // --- Automation nudging ---
        let teams = state.db.get_active_teams().await.unwrap_or_default();
        for team in teams {
            if !team.automation {
                continue;
            }

            let Some(info) = state.liveness.get_info(&team.lead_session_id) else {
                continue;
            };

            if info.idle_duration.as_secs() <= timeout {
                continue;
            }

            // Rate-limit: only nudge once per timeout interval
            let should_nudge = match nudge_tracker.get(&team.team_id) {
                Some(last) => last.elapsed().as_secs() > timeout,
                None => true,
            };

            if !should_nudge {
                continue;
            }

            tracing::info!(
                team_id = %team.team_id,
                lead_session = %team.lead_session_id,
                idle_secs = info.idle_duration.as_secs(),
                "Nudging idle team lead (automation mode)"
            );

            let _ = state
                .containers
                .send(
                    &team.lead_session_id,
                    "Check for any approved specs that need implementation. \
                     Use [TEAM_STATUS] to review member availability, then assign work.",
                )
                .await;

            nudge_tracker.insert(team.team_id.clone(), std::time::Instant::now());
        }
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
    match state
        .db
        .get_pending_request_by_domain_and_session(domain, session_id)
        .await
    {
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

    match state
        .mm
        .post_with_props(channel_id, thread_id, "", props)
        .await
    {
        Ok(post_id) => {
            if let Err(e) = state
                .db
                .create_pending_request(
                    &request_id,
                    channel_id,
                    thread_id,
                    session_id,
                    domain,
                    &post_id,
                )
                .await
            {
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
            request_id,
            action,
            payload.user_name
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
                if let Err(e) = state
                    .db
                    .log_approval(request_id, &req.domain, action, &payload.user_name)
                    .await
                {
                    tracing::error!("Failed to log approval: {}", e);
                }
                if let Err(e) = state
                    .mm
                    .update_post(
                        &req.post_id,
                        &format!("`{}` approved by @{}", req.domain, payload.user_name),
                    )
                    .await
                {
                    tracing::warn!(error = %e, "Failed to update Mattermost post");
                }
                if let Err(e) = state
                    .containers
                    .send(
                        &req.session_id,
                        &format!("[NETWORK_APPROVED: {}]", req.domain),
                    )
                    .await
                {
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
                    ephemeral_text: Some(format!(
                        "Failed to add domain to firewall: {}. Please try again.",
                        e
                    )),
                    update: None,
                });
            }
        }
    } else {
        if let Err(e) = state.db.delete_pending_request(request_id).await {
            tracing::error!("Failed to delete pending request: {}", e);
        }
        if let Err(e) = state
            .db
            .log_approval(request_id, &req.domain, action, &payload.user_name)
            .await
        {
            tracing::error!("Failed to log denial: {}", e);
        }
        if let Err(e) = state
            .mm
            .update_post(
                &req.post_id,
                &format!("`{}` denied by @{}", req.domain, payload.user_name),
            )
            .await
        {
            tracing::warn!(error = %e, "Failed to update Mattermost post");
        }
        if let Err(e) = state
            .containers
            .send(
                &req.session_id,
                &format!("[NETWORK_DENIED: {}]", req.domain),
            )
            .await
        {
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
