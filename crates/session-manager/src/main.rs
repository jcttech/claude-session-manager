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
use session_manager::database::{
    self, Database, EnqueueClearResult, StoredSession, StoredTeamRole, StoredTeamTask,
};
use session_manager::profile::{self, ActiveConfigDir};

mod admin;
use session_manager::git::{GitManager, RepoRef};
use session_manager::liveness::{LivenessState, format_duration_short};
use session_manager::opnsense::OPNsense;
use session_manager::rate_limit::{self, RateLimitLayer};
use session_manager::ssh;
use session_manager::stream_json::OutputEvent;

const APP_VERSION: &str = env!("APP_VERSION");

/// Cached regex for network request detection (compiled once on first use)
static NETWORK_REQUEST_RE: OnceLock<Regex> = OnceLock::new();
// Every team control signal — including the post-merge self-clear breakpoint
// — now arrives as a CliCommand event. No regex matchers on the LLM's text
// stream remain.

fn network_request_regex() -> &'static Regex {
    NETWORK_REQUEST_RE.get_or_init(|| {
        Regex::new(r"\[NETWORK_REQUEST:\s*([^\]]+)\]").expect("Invalid regex pattern")
    })
}

pub struct AppState {
    pub mm: Mattermost,
    pub containers: ContainerManager,
    pub git: GitManager,
    pub opnsense: OPNsense,
    pub db: Database,
    pub liveness: LivenessState,
    /// Shared coordination channel ID for team coordination logs
    pub coordination_channel_id: tokio::sync::RwLock<Option<String>>,
    /// PRs that have passed the [PR_REVIEWED] gate, keyed by "team_id:pr_number".
    /// Only PRs in this set should be merged by the Team Lead.
    pub reviewed_prs: DashMap<String, ()>,
    /// Per-team mutex to serialize `team_task_queue` drain passes. Without
    /// this, two concurrent ResponseComplete events on members of the same
    /// team could both walk the queue and double-deliver the same task_id
    /// to two different idle targets.
    pub drain_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Currently-active CLAUDE_CONFIG_DIR override. Read by container env
    /// builder on every CreateSession; written by POST /admin/profile.
    pub active_config_dir: ActiveConfigDir,
}

// `spawn_locks` was removed in v2.5.4 — its serialization role is subsumed by
// `drain_locks` since SPAWN now flows through `team_task_queue` like TO/CLEAR.
//
// `intent_dedupe` / `to_throttle` and their constants were removed in the
// CLI-grammar cutover: each `team` CLI invocation is an explicit Bash call,
// not text the LLM might re-emit, so marker-stutter dedupe is no longer
// needed. Duplicate `team to` calls just create two queue rows; the lead
// can `team cancel` if needed.

/// Cooldown between any two `/clear` operations on the same session — auto or
/// manual. Prevents the runaway loop seen previously with `/compact`, and
/// prevents Lead-driven `[CLEAR:Role]` from being spammed inside one window.
const AUTO_CLEAR_COOLDOWN_SECS: u64 = 180;

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
    // Install the ring crypto provider for rustls before any TLS connections
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

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
    let git = GitManager::new();
    let db = Database::new().await?;

    // Seed the active CLAUDE_CONFIG_DIR override from DB. The singleton row
    // is created in `create_schema`, so this never misses. Sanity-check that
    // the persisted dir still has a credentials file — warn (don't fail) if
    // it's gone, since operators can recover by POSTing a fresh path.
    let initial_state = db.get_claude_profile_state().await?;
    let initial_dir = profile::from_db_value(&initial_state.claude_config_dir);
    if let Some(ref dir) = initial_dir {
        let creds = std::path::Path::new(dir).join(".credentials.json");
        if !creds.is_file() {
            tracing::warn!(
                dir = %dir,
                "Persisted CLAUDE_CONFIG_DIR has no .credentials.json — \
                 spawns will fail until the file is restored or POST /admin/profile \
                 supplies a new path",
            );
        }
    }
    let active_config_dir: ActiveConfigDir =
        Arc::new(tokio::sync::RwLock::new(initial_dir));

    let containers = ContainerManager::new(active_config_dir.clone());

    let liveness = LivenessState::new();

    let coordination_channel_id = s.coordination_channel_id.clone();
    let state = Arc::new(AppState {
        mm,
        containers,
        git,
        opnsense,
        db,
        liveness,
        coordination_channel_id: tokio::sync::RwLock::new(coordination_channel_id),
        reviewed_prs: DashMap::new(),
        drain_locks: DashMap::new(),
        active_config_dir,
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

                let reconnect_result = state
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
                    .await;

                match reconnect_result {
                    Err(e) => {
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
                        // If the session had a pending task, notify the team lead
                        if session.pending_task_from.is_some()
                            && let Some(ref tid) = session.team_id
                        {
                            let role = session.role.as_deref().unwrap_or("Unknown");
                            let task = session.current_task.as_deref().unwrap_or("unknown task");
                            notify_context_lost(&state, tid, &session.session_id, role, task).await;
                        }
                        continue;
                    }
                    Ok(cold_started) => {
                        // CSM restarted — all sessions are reconnecting.
                        // If the member had a pending task, mark context as lost
                        // so the Team Lead knows to re-assign.
                        if session.pending_task_from.is_some() {
                            let role = session.role.as_deref().unwrap_or("Unknown");
                            let task = session.current_task.as_deref().unwrap_or("unknown task");
                            let marker = if cold_started {
                                format!("Context lost (new container) — was: {}", task)
                            } else {
                                format!("Context lost (CSM restart) — was: {}", task)
                            };
                            let _ = state.db.set_pending_task(
                                &session.session_id, "", &marker,
                            ).await;
                            // Clear pending_task_from so the member shows as idle/available
                            let _ = state.db.clear_pending_task(&session.session_id).await;

                            if let Some(ref tid) = session.team_id {
                                notify_context_lost(&state, tid, &session.session_id, role, task).await;
                            }
                        }
                    }
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

    // Scheduled-drain ticker: every 30s, (a) flip any rate-limit rows whose
    // window has expired back to `allowed` and post the resume notice, then
    // (b) drain teams with at least one queued task whose deliver_after has
    // passed. Cheap (two indexed scans) and 30s latency is fine for both the
    // "deliver in N hours" path and the rate-limit resume path.
    let scheduled_state = state.clone();
    let _scheduled_drain_handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;

            // 1. Resume any teams whose rate-limit window has expired. This
            //    runs before the drain pass so newly-resumed teams get a
            //    chance to flush their backlog in the same tick.
            match scheduled_state.db.teams_with_expired_rate_limits().await {
                Ok(team_ids) => {
                    for team_id in team_ids {
                        clear_rate_limit_notice_and_drain(&scheduled_state, &team_id).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Rate-limit resume query failed");
                }
            }

            match scheduled_state.db.teams_with_ready_scheduled_tasks().await {
                Ok(team_ids) => {
                    for team_id in team_ids {
                        drain_team_queue(&scheduled_state, &team_id).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Scheduled-drain query failed");
                }
            }
        }
    });

    // Configure rate limiting
    let s = config::settings();
    let rate_limiter = RateLimitLayer::new(s.rate_limit_rps, s.rate_limit_burst);

    // Spawn cleanup task for rate limiter (also uses cancellation token)
    let cancel_clone = cancel_token.clone();
    let rate_limit_handle = rate_limit::spawn_cleanup_task(rate_limiter.clone(), cancel_clone);

    // Admin routes — separate sub-router so the bearer-token middleware only
    // gates these paths (callback, health, metrics keep their existing access).
    let admin_routes = Router::new()
        .route(
            "/admin/profile",
            get(admin::get_profile).post(admin::post_profile),
        )
        .layer(axum::middleware::from_fn(admin::require_admin_token));

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
        .merge(admin_routes)
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
    pre_session_id: Option<&str>,
) -> Result<String> {
    use std::path::PathBuf;

    // Caller may pre-allocate the session_id (used by handle_team_spawn so the
    // (team_id, role) claim and the eventual sessions row share an identity).
    let session_id = pre_session_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
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

    // Look up the session row once — we need its user_id (for unfollow) and
    // (team_id, role) (to release the role claim so the slot can be re-spawned).
    let stored_session = state
        .db
        .get_session_by_id_prefix(&session_id[..8.min(session_id.len())])
        .await
        .ok()
        .flatten();

    if let Some(ref session) = stored_session
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

    // Release the team role claim (if this was a team member) so the slot
    // becomes available for a future spawn.
    if let Some(session) = stored_session
        && let (Some(tid), Some(role)) = (session.team_id, session.role)
        && let Err(e) = state.db.release_team_role(&tid, &role).await
    {
        tracing::warn!(team_id = %tid, role = %role, error = %e,
            "Failed to release team role claim");
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

    // Release the team role claim (if any) so the slot is available for respawn.
    // 'stopped' is excluded from the partial unique index, but the claim row
    // would still block a new spawn of the same role until released.
    if let (Some(tid), Some(role)) = (session.team_id.as_deref(), session.role.as_deref())
        && let Err(e) = state.db.release_team_role(tid, role).await
    {
        tracing::warn!(team_id = %tid, role = %role, error = %e,
            "Failed to release team role claim on stop");
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

/// Parse a `/schedule <role> <time> <message>` command body (without the
/// leading `/schedule `). Returns (role, deliver_after_utc, message) or a
/// human-readable error string suitable for posting back to the thread.
fn parse_schedule_command(
    input: &str,
) -> Result<(String, chrono::DateTime<chrono::Utc>, String), String> {
    let trimmed = input.trim_start();
    if trimmed.is_empty() {
        return Err("missing <role>".into());
    }
    let (role, rest) = trimmed
        .split_once(char::is_whitespace)
        .ok_or("missing <time> and <message> after role")?;
    let rest = rest.trim_start();
    let (time_token, rest) = rest
        .split_once(char::is_whitespace)
        .ok_or("missing <message> after time")?;
    let deliver_after = parse_schedule_time(time_token)?;
    let message = rest.trim();
    if message.is_empty() {
        return Err("missing <message>".into());
    }
    Ok((role.to_string(), deliver_after, message.to_string()))
}

/// Parse a /schedule time token: `+Ns`, `+Nm`, `+Nh`, `+Nd`, or RFC3339.
fn parse_schedule_time(tok: &str) -> Result<chrono::DateTime<chrono::Utc>, String> {
    if let Some(rel) = tok.strip_prefix('+') {
        let split = rel
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| format!("bad relative time '{}'; expected e.g. +5h", tok))?;
        let (num_str, unit) = rel.split_at(split);
        let n: i64 = num_str
            .parse()
            .map_err(|_| format!("bad relative number in '{}'", tok))?;
        let dur = match unit {
            "s" => chrono::Duration::seconds(n),
            "m" => chrono::Duration::minutes(n),
            "h" => chrono::Duration::hours(n),
            "d" => chrono::Duration::days(n),
            other => {
                return Err(format!(
                    "unknown time unit '{}' in '{}'; use s/m/h/d",
                    other, tok
                ))
            }
        };
        Ok(chrono::Utc::now() + dur)
    } else {
        chrono::DateTime::parse_from_rfc3339(tok)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .map_err(|e| format!("bad RFC3339 timestamp '{}': {}", tok, e))
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
                // User-only `/schedule <role> <time> <message>` — intercepted
                // before the LLM ever sees it. Inserts directly into the
                // team_task_queue with deliver_after; the periodic scheduled
                // drain ticker delivers when it's due. Lead threads only.
                if text.starts_with("/schedule ") || text == "/schedule" {
                    if session.session_type != "team_lead" {
                        let _ = state
                            .mm
                            .post_in_thread(
                                channel_id,
                                root_id,
                                "/schedule is only available in Team Lead threads.",
                            )
                            .await;
                        continue;
                    }
                    let Some(team_id) = session.team_id.clone() else {
                        let _ = state
                            .mm
                            .post_in_thread(
                                channel_id,
                                root_id,
                                "Team Lead session has no team_id; cannot schedule.",
                            )
                            .await;
                        continue;
                    };
                    let body = text.trim_start_matches("/schedule").trim_start();
                    match parse_schedule_command(body) {
                        Ok((role, deliver_after, message)) => {
                            match state
                                .db
                                .enqueue_team_task(
                                    &team_id,
                                    &role,
                                    false,
                                    &session.session_id,
                                    "Team Lead",
                                    &message,
                                    Some(deliver_after),
                                )
                                .await
                            {
                                Ok(task_id) => {
                                    let _ = state.mm.post_in_thread(
                                        channel_id,
                                        root_id,
                                        &format!(
                                            ":alarm_clock: **Scheduled** task #{} for **{}** at {} (UTC).",
                                            task_id,
                                            role,
                                            deliver_after.to_rfc3339(),
                                        ),
                                    ).await;
                                }
                                Err(e) => {
                                    let _ = state.mm.post_in_thread(
                                        channel_id,
                                        root_id,
                                        &format!("Failed to schedule: {}", e),
                                    ).await;
                                }
                            }
                        }
                        Err(e) => {
                            let _ = state.mm.post_in_thread(
                                channel_id,
                                root_id,
                                &format!(
                                    "/schedule error: {}\nUsage: `/schedule <role> <+5h|+30m|+2d|RFC3339> <message>`",
                                    e,
                                ),
                            ).await;
                        }
                    }
                    continue;
                }

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
                        let _ = state.containers.clear(&session.session_id).await;
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
                                Ok(_cold_started) => {
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
                                None,
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

/// How long an identical [SPAWN:Role] marker is suppressed at the SQL layer
/// after first being enqueued. Wider than the in-memory window because LLM
/// turns can run for minutes — the original re-emission bug fired well past
/// the 60s in-memory dedupe.
const SPAWN_DEDUPE_WINDOW_SECS: i64 = 300;

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
    // All team control commands (spawn / to / interrupt / cancel / clear /
    // status / broadcast / pr-ready / pr-reviewed / pr-merged) flow through
    // the `team` CLI as CliCommand events — no in-stream marker parsing.

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

    // Per-session error dedupe — once the worker is stuck (rate limit or any
    // other repeating error) we keep editing the same Mattermost card instead
    // of stamping a new "Process died" post for every retry.
    let mut last_error_post_id: Option<String> = None;
    let mut last_error_signature: Option<String> = None;
    let mut last_error_at: Option<tokio::time::Instant> = None;
    // While the team is in a known rate-limit-rejected window, swallow
    // ProcessDied entirely (the rate-limit notice card is the source of truth).
    let mut rate_limited_until: Option<tokio::time::Instant> = None;

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

                        // Clear pending task on response complete for team members,
                        // then drain the queue — this member's slot just freed, and
                        // there may be a queued task waiting for them (or for any
                        // idle member of their role).
                        //
                        // Also flip the message-type queue row that drove this
                        // turn from 'running' → 'completed'. The transition is
                        // idempotent (WHERE status='running'), so a duplicate
                        // ResponseComplete for the same turn is a safe no-op.
                        if let Some((ref tid, _)) = team_info {
                            let _ = state.db.clear_pending_task(&session_id).await;
                            match state
                                .db
                                .mark_message_task_completed_for_session(&session_id)
                                .await
                            {
                                Ok(Some(task_id)) => {
                                    tracing::debug!(
                                        session_id = %session_id, task_id,
                                        "queue: running → completed on ResponseComplete",
                                    );
                                }
                                Ok(None) => {
                                    // Either this turn wasn't queue-driven
                                    // (user asked the lead something directly)
                                    // or the row was already terminal. Either
                                    // way, no action needed.
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, session_id = %session_id,
                                        "queue: failed to mark message task completed");
                                }
                            }
                            drain_team_queue(&state, tid).await;
                        }

                        counter!("tokens_input_total").increment(input_tokens);
                        counter!("tokens_output_total").increment(output_tokens);

                        // Warn when context window is getting full (>80%)
                        let ctx_max = config::settings().context_window_max;
                        let ctx_warn_threshold = ctx_max * 80 / 100;
                        let ctx_clear_threshold = ctx_max * 90 / 100;
                        let ctx_rearm_threshold = ctx_max * 70 / 100;

                        // (The previous in-memory armed-flag was removed in v2.5.4
                        // when auto-clear moved to the queue. The SQL cooldown
                        // inside `enqueue_team_clear` now provides the same
                        // anti-loop guarantee.) ctx_rearm_threshold retained
                        // for telemetry only.
                        let _ = ctx_rearm_threshold;

                        if ctx_display > ctx_warn_threshold {
                            let usage_pct = (ctx_display as f64 / ctx_max as f64 * 100.0) as u64;
                            let msg = format!(
                                ":warning: **Context window {}% full** ({} / {}k tokens) — consider using `clear` (membank will reload context)",
                                usage_pct, ctx_display, ctx_max / 1000,
                            );
                            let _ = state.mm.post_in_thread(&channel_id, &thread_id, &msg).await;

                            // Team-aware: route the warn alert into a Claude
                            // session that can act on it. For members, that's
                            // the Lead (who decides whether to [CLEAR:Role] or
                            // reassign work). For the Lead itself, it's the
                            // Lead's own session — otherwise the Lead's LLM
                            // never sees a signal that its context is filling
                            // up and just rides into the 90% auto-clear blind.
                            if let Some((ref tid, ref role)) = team_info {
                                if role == "Team Lead" {
                                    let header = build_team_context_header(&state.db, tid, "Team Lead").await;
                                    let alert_msg = format!(
                                        "{}\n[CONTEXT_ALERT] Your own context is at {}% ({}/{}k). Wrap up the current train of thought, then emit `[CLEAR:Self]` as the last marker in your response. Auto-clear will fire at 90% if you don't — preserving queued tasks and roster, but losing in-flight working memory.",
                                        header, usage_pct, format_token_count(ctx_display), ctx_max / 1000,
                                    );
                                    let _ = state.containers.send(&session_id, &alert_msg).await;
                                } else {
                                    let alert_msg = format!(
                                        "**Context Alert**: {} is at {}% context capacity ({}/{}k).\nConsider sending them a `clear` instruction (membank will reload context), or reassigning remaining work.",
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
                        }

                        // Auto-clear at 90% — enqueue a clear task. The SQL
                        // cooldown inside `enqueue_team_clear` (180s window per
                        // target_session_id) replaces the in-memory armed-flag
                        // gate that used to live here; manual + auto + self
                        // share the same gate so they can't ping-pong.
                        // Membank repopulates context after the clear lands.
                        if ctx_display > ctx_clear_threshold
                            && let Some((ref tid, ref role)) = team_info
                        {
                            let usage_pct =
                                (ctx_display as f64 / ctx_max as f64 * 100.0) as u64;
                            let role_label = role.clone();
                            let reason = format!("auto_{}pct", usage_pct);
                            match state
                                .db
                                .enqueue_team_clear(
                                    tid,
                                    &role_label,
                                    &session_id,
                                    &session_id,
                                    &role_label,
                                    &reason,
                                    AUTO_CLEAR_COOLDOWN_SECS as i64,
                                )
                                .await
                            {
                                Ok(EnqueueClearResult::Enqueued(task_id))
                                | Ok(EnqueueClearResult::AlreadyQueued(task_id)) => {
                                    tracing::info!(
                                        session_id = %session_id,
                                        ctx_display, ctx_max, task_id,
                                        "auto-clear: enqueued (queue_id={})", task_id
                                    );
                                    if role == "Team Lead" {
                                        let coord_msg = format!(
                                            ":broom: **Team Lead context auto-cleared at {}%** ({}k/{}k, queue_id={}). Expect a brief pause before the next assignment — the Lead is being re-briefed from durable state.",
                                            usage_pct, ctx_display / 1000, ctx_max / 1000, task_id,
                                        );
                                        post_to_coordination_log(&state, tid, &coord_msg).await;
                                        let broadcast_msg = format!(
                                            "**Team Lead context auto-cleared** at {}%. The Lead is briefly re-orienting from queued tasks and roster — your in-flight work continues normally; reports landing in the Lead's thread during this window may be re-acknowledged.",
                                            usage_pct,
                                        );
                                        broadcast_team_notification(&state, tid, &broadcast_msg, &session_id).await;
                                    } else if let Ok(leads) = state.db.get_sessions_by_role(tid, "Team Lead").await
                                        && let Some(lead) = leads.first()
                                    {
                                        let header = build_team_context_header(&state.db, tid, "Team Lead").await;
                                        let clear_msg = format!(
                                            "{}\n**Auto-clear** (queue_id={}): {} context auto-cleared at {}% ({}k/{}k). Membank will reload context on next message.",
                                            header, task_id, role, usage_pct,
                                            ctx_display / 1000, ctx_max / 1000,
                                        );
                                        let _ = state.containers.send(&lead.session_id, &clear_msg).await;
                                    }
                                    drain_team_queue(&state, tid).await;
                                }
                                Ok(EnqueueClearResult::Cooldown { remaining_secs }) => {
                                    tracing::debug!(
                                        session_id = %session_id, remaining_secs,
                                        "auto-clear suppressed by cooldown"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, session_id = %session_id,
                                        "Failed to enqueue auto-clear task");
                                }
                            }
                        }
                    }
                    OutputEvent::ProcessDied { exit_code, signal, raw_json } => {
                        tracing::warn!(
                            session_id = %session_id,
                            ?exit_code, ?signal,
                            raw_json_len = raw_json.as_deref().map(str::len).unwrap_or(0),
                            "stream_output: ProcessDied",
                        );

                        // While we're in a known rate-limit-rejected window
                        // every queued retry produces another error result —
                        // the rate-limit notice card already explains it, so
                        // skip stamping ProcessDied cards entirely.
                        let suppress_for_rate_limit = rate_limited_until
                            .map(|d| tokio::time::Instant::now() < d)
                            .unwrap_or(false);
                        if suppress_for_rate_limit {
                            tracing::debug!(
                                session_id = %session_id,
                                "ProcessDied suppressed: team is rate-limited"
                            );
                            // The turn died — its work was never actually
                            // applied. Re-enqueue the task so the queue
                            // redelivers it once the window reopens, and
                            // free the member's slot so it can pick up the
                            // re-queued row (or be reassigned).
                            if let Some((tid, _)) = team_info.as_ref() {
                                requeue_in_flight_after_rate_limit(&state, tid, &session_id).await;
                            }
                            // Don't broadcast `disconnected` either — the
                            // session is fine, the API window is closed.
                            continue;
                        }

                        drain_batch_to_card(&mut card, &state, &channel_id, &thread_id, &mut batch, &mut batch_bytes).await;

                        // Dedupe consecutive identical ProcessDied posts. If
                        // the same signature lands within the dedupe window,
                        // edit the existing post instead of creating a new one.
                        let signature = format!(
                            "{}|{}",
                            exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                            signal.clone().unwrap_or_default(),
                        );
                        let now = tokio::time::Instant::now();
                        let dedupe_window = std::time::Duration::from_secs(60);
                        let should_dedupe = matches!(
                            (&last_error_post_id, &last_error_signature, last_error_at),
                            (Some(_), Some(prev), Some(t))
                                if prev == &signature && now.duration_since(t) < dedupe_window
                        );

                        if should_dedupe {
                            // Bump a counter on the existing post so the user
                            // can see the retries are still happening.
                            let post_id = last_error_post_id.clone().unwrap();
                            let body = format!(
                                ":warning: Process died (exit {}{}) — repeating; last seen {}",
                                exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                                signal.as_deref().map(|s| format!(", {}", s)).unwrap_or_default(),
                                chrono::Utc::now().format("%H:%M:%S UTC"),
                            );
                            let _ = state.mm.update_post(&post_id, &body).await;
                            last_error_at = Some(now);
                        } else {
                            card.finalize_error(exit_code, &signal);
                            card.flush(&state, &channel_id, &thread_id).await;
                            last_error_post_id = card.post_id.clone();
                            last_error_signature = Some(signature);
                            last_error_at = Some(now);
                            card.post_id = None;
                        }

                        // Forward raw_json (if any) so unmodeled SDK errors
                        // surface in logs — we add typed handling later if a
                        // pattern shows up.
                        if let Some(rj) = raw_json.as_deref() {
                            tracing::warn!(
                                session_id = %session_id,
                                raw = %rj,
                                "ProcessDied raw payload",
                            );
                        }

                        // Notify team about the death
                        if let Some((ref tid, ref role)) = team_info {
                            let _ = state.db.update_session_status(&session_id, "disconnected").await;
                            notify_team_session_lost(&state, tid, &session_id, role, "process died").await;
                        }
                    }
                    OutputEvent::CliCommand { cli_id, verb, args, flags } => {
                        tracing::info!(
                            session_id = %session_id, %cli_id, %verb,
                            args_len = args.len(),
                            "stream_output: CliCommand",
                        );
                        let team_id = team_info.as_ref().map(|(t, _)| t.clone());
                        let role = team_info.as_ref().map(|(_, r)| r.clone());
                        let outcome = handle_cli_command(
                            &state, &session_id, team_id.as_deref(), role.as_deref(),
                            &verb, &args, &flags,
                        ).await;
                        if let Err(e) = state
                            .containers
                            .send_cli_response(&session_id, &cli_id,
                                outcome.exit_code, &outcome.stdout, &outcome.stderr)
                            .await
                        {
                            tracing::warn!(error = %e, %cli_id,
                                "Failed to deliver CliResponse to message_processor");
                        }
                    }
                    OutputEvent::RateLimit { status, resets_at, limit_type, utilization, raw_json } => {
                        tracing::info!(
                            session_id = %session_id,
                            %status, %limit_type, resets_at, utilization,
                            "stream_output: RateLimit",
                        );

                        // Rate-limit signals are team-scoped — same Anthropic
                        // account drives every member. If this session has no
                        // team_id, just log and move on.
                        let Some((tid, _role)) = team_info.as_ref() else {
                            continue;
                        };

                        // Persist regardless of status so we have history.
                        let util_opt = if utilization >= 0.0 { Some(utilization) } else { None };
                        if let Err(e) = state
                            .db
                            .upsert_team_rate_limit(
                                tid, &status, resets_at, &limit_type, util_opt, &raw_json,
                            )
                            .await
                        {
                            tracing::warn!(error = %e, team_id = %tid, "Failed to persist team rate limit");
                        }

                        match status.as_str() {
                            "rejected" => {
                                // Pause local ProcessDied chatter for the
                                // remainder of the window (with a sane cap so
                                // we don't sit silent forever if resets_at is
                                // bogus).
                                let pause_secs = if resets_at > 0 {
                                    let now_unix = chrono::Utc::now().timestamp();
                                    (resets_at - now_unix).clamp(60, 6 * 60 * 60) as u64
                                } else {
                                    15 * 60
                                };
                                rate_limited_until = Some(
                                    tokio::time::Instant::now()
                                        + std::time::Duration::from_secs(pause_secs),
                                );

                                // Post (or update) a single notice card on the
                                // team coordination thread.
                                post_or_update_rate_limit_notice(
                                    &state, tid, &status, resets_at, &limit_type, util_opt,
                                )
                                .await;
                            }
                            "allowed_warning" => {
                                // Soft heads-up — also surface as a notice but
                                // don't pause anything.
                                post_or_update_rate_limit_notice(
                                    &state, tid, &status, resets_at, &limit_type, util_opt,
                                )
                                .await;
                            }
                            "allowed" => {
                                rate_limited_until = None;
                                clear_rate_limit_notice_and_drain(&state, tid).await;
                            }
                            other => {
                                tracing::debug!(status = %other, "RateLimit: unknown status (no-op)");
                            }
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

/// Format a rate-limit notice body. Centralised so the "rejected" card and
/// the later "resumed" edit share one renderer.
fn format_rate_limit_notice(
    status: &str,
    resets_at_unix: i64,
    limit_type: &str,
    utilization: Option<f64>,
) -> String {
    let when = if resets_at_unix > 0 {
        chrono::DateTime::<chrono::Utc>::from_timestamp(resets_at_unix, 0)
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "unknown".to_string())
    } else {
        "unknown".to_string()
    };
    let kind = if limit_type.is_empty() {
        "session".to_string()
    } else {
        limit_type.replace('_', " ")
    };
    let util_str = utilization
        .map(|u| format!(" ({}% used)", (u * 100.0).round() as i64))
        .unwrap_or_default();
    match status {
        "rejected" => format!(
            ":no_entry: **{} limit hit**{}. Team task queue paused — resumes {}. Any turn that errors during the window is re-queued and redelivered automatically once the limit resets.",
            kind, util_str, when,
        ),
        "allowed_warning" => format!(
            ":warning: **{} limit warning**{}. Approaching cap — resets {}.",
            kind, util_str, when,
        ),
        "allowed" => format!(
            ":white_check_mark: **{} limit cleared.** Team task queue resumed.",
            kind,
        ),
        other => format!(
            ":information_source: Rate-limit status: {} (kind={}, resets {}{})",
            other, kind, when, util_str,
        ),
    }
}

/// Post a single notice card on the team coordination thread, or edit the
/// previous one in place. Updates `team_rate_limits.notice_post_id` so the
/// next transition keeps editing the same row.
async fn post_or_update_rate_limit_notice(
    state: &Arc<AppState>,
    team_id: &str,
    status: &str,
    resets_at_unix: i64,
    limit_type: &str,
    utilization: Option<f64>,
) {
    let body = format_rate_limit_notice(status, resets_at_unix, limit_type, utilization);

    // Try to edit the existing notice first (idempotent on repeated rejects).
    let existing = match state.db.get_team_rate_limit(team_id).await {
        Ok(Some(rl)) => rl.notice_post_id,
        _ => None,
    };
    if let Some(post_id) = existing {
        if state.mm.update_post(&post_id, &body).await.is_ok() {
            return;
        }
        // Edit failed (post deleted?) — fall through to create a new one.
        tracing::warn!(team_id, "Failed to edit existing rate-limit notice; reposting");
    }

    // Need a coordination thread to post into.
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

    match state
        .mm
        .post_in_thread(&coord_channel_id, &thread_id, &body)
        .await
    {
        Ok(post_id) => {
            if let Err(e) = state
                .db
                .set_team_rate_limit_post_id(team_id, Some(&post_id))
                .await
            {
                tracing::warn!(error = %e, "Failed to persist rate-limit notice post id");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to post rate-limit notice");
        }
    }
}

/// Outcome of a `team` CLI dispatch — turned into a CliResponse on the wire.
struct CliOutcome {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

impl CliOutcome {
    fn ok<S: Into<String>>(stdout: S) -> Self {
        Self { exit_code: 0, stdout: stdout.into(), stderr: String::new() }
    }
    fn err<S: Into<String>>(code: i32, stderr: S) -> Self {
        Self { exit_code: code, stdout: String::new(), stderr: stderr.into() }
    }
}

/// Dispatch a `team` CLI verb. The CLI binary inside the container has
/// already validated argv; here we just translate verb + args to existing
/// internal helpers and produce stdout/stderr/exit_code for the reply.
///
/// Currently supported verbs: `status`, `to`. Other verbs return a stub
/// non-zero exit until they're wired up in follow-up slices — the marker
/// path remains the supported way to invoke them in the meantime.
async fn handle_cli_command(
    state: &Arc<AppState>,
    sender_session_id: &str,
    team_id: Option<&str>,
    sender_role: Option<&str>,
    verb: &str,
    args: &[String],
    flags: &std::collections::HashMap<String, String>,
) -> CliOutcome {
    let want_json = flags.get("json").map(|v| v == "1").unwrap_or(false);
    match verb {
        "status" => cli_verb_status(state, team_id, want_json).await,
        "to" => cli_verb_to(state, sender_session_id, team_id, sender_role, args).await,
        "spawn" => cli_verb_spawn(state, sender_session_id, team_id, sender_role, args).await,
        "interrupt" => {
            cli_verb_interrupt(state, sender_session_id, team_id, sender_role, args).await
        }
        "cancel" => {
            cli_verb_cancel(state, sender_session_id, team_id, args, flags).await
        }
        "clear" => cli_verb_clear(state, sender_session_id, team_id, sender_role, args).await,
        "broadcast" => {
            cli_verb_broadcast(state, sender_session_id, team_id, sender_role, args).await
        }
        "pr-ready" => {
            cli_verb_pr_ready(state, sender_session_id, team_id, sender_role, args).await
        }
        "pr-reviewed" => {
            cli_verb_pr_reviewed(state, sender_session_id, team_id, sender_role, args).await
        }
        "pr-merged" => {
            cli_verb_pr_merged(state, sender_session_id, team_id, sender_role, args).await
        }
        other => CliOutcome::err(2, format!("team: unknown verb '{}'\n", other)),
    }
}

async fn cli_verb_status(
    state: &Arc<AppState>,
    team_id: Option<&str>,
    want_json: bool,
) -> CliOutcome {
    let Some(team_id) = team_id else {
        return CliOutcome::err(
            1,
            "team status: this session is not part of a team.\n".to_string(),
        );
    };

    let members = match state.db.get_team_members(team_id).await {
        Ok(m) => m,
        Err(e) => return CliOutcome::err(1, format!("team status: {}\n", e)),
    };
    let queue_counts = state
        .db
        .queued_counts_by_role(team_id)
        .await
        .unwrap_or_default();
    let rate_limit = state.db.get_team_rate_limit(team_id).await.ok().flatten();

    if want_json {
        let queue_obj: serde_json::Map<String, serde_json::Value> = queue_counts
            .into_iter()
            .map(|(role, n)| (role, serde_json::Value::from(n)))
            .collect();
        let members_json: Vec<serde_json::Value> = members
            .iter()
            .map(|m| {
                let status = if m.pending_task_from.is_some() { "running" } else { "idle" };
                serde_json::json!({
                    "session_id": m.session_id,
                    "role": m.role,
                    "status": status,
                    "current_task": m.current_task,
                    "context_tokens": m.context_tokens,
                })
            })
            .collect();
        let rl_json = rate_limit.map(|rl| {
            serde_json::json!({
                "status": rl.status,
                "resets_at": rl.resets_at.map(|t| t.to_rfc3339()),
                "limit_type": rl.limit_type,
                "utilization": rl.utilization,
            })
        }).unwrap_or(serde_json::Value::Null);
        let body = serde_json::json!({
            "team_id": team_id,
            "members": members_json,
            "queue_pending": queue_obj,
            "rate_limit": rl_json,
        });
        return CliOutcome::ok(body.to_string());
    }

    // Human-readable: roster table + queue depth + rate-limit footer.
    let mut out = String::new();
    out.push_str(&format!("Team {}\n", team_id));
    if members.is_empty() {
        out.push_str("  (no members)\n");
    } else {
        for m in &members {
            let role = m.role.as_deref().unwrap_or("<unknown>");
            let status = if m.pending_task_from.is_some() { "running" } else { "idle" };
            let task_preview = m.current_task
                .as_deref()
                .map(|t| truncate_preview(t, 60))
                .unwrap_or_default();
            if task_preview.is_empty() {
                out.push_str(&format!("  - {} ({})\n", role, status));
            } else {
                out.push_str(&format!("  - {} ({}) — {}\n", role, status, task_preview));
            }
        }
    }
    if !queue_counts.is_empty() {
        out.push_str("Pending queue:\n");
        for (role, n) in &queue_counts {
            out.push_str(&format!("  - {}: {}\n", role, n));
        }
    }
    if let Some(rl) = rate_limit
        && rl.status == "rejected"
    {
        let when = rl.resets_at
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "unknown".to_string());
        out.push_str(&format!(
            "Rate-limit: REJECTED ({}) — resumes {}\n",
            if rl.limit_type.is_empty() { "session" } else { rl.limit_type.as_str() },
            when,
        ));
    }
    CliOutcome::ok(out)
}

async fn cli_verb_to(
    state: &Arc<AppState>,
    sender_session_id: &str,
    team_id: Option<&str>,
    sender_role: Option<&str>,
    args: &[String],
) -> CliOutcome {
    let Some(team_id) = team_id else {
        return CliOutcome::err(1, "team to: this session is not part of a team.\n".to_string());
    };
    let Some(sender_role) = sender_role else {
        return CliOutcome::err(1, "team to: missing sender role.\n".to_string());
    };
    if args.len() < 2 {
        return CliOutcome::err(2, "team to: usage: team to <role> <message>\n".to_string());
    }
    let target_role = &args[0];
    let message = &args[1];
    if message.trim().is_empty() {
        return CliOutcome::err(2, "team to: message is empty.\n".to_string());
    }

    // Match the marker path's "is_prefix_match" convention: a role string
    // without a numeric suffix means "any member of this role"; with a
    // suffix it's an exact session. drain_team_queue honours that flag.
    let is_prefix = !target_role
        .rsplit_once(' ')
        .map(|(_, tail)| tail.chars().all(|c| c.is_ascii_digit()) && !tail.is_empty())
        .unwrap_or(false);

    match state
        .db
        .enqueue_team_task(
            team_id,
            target_role,
            is_prefix,
            sender_session_id,
            sender_role,
            message,
            None,
        )
        .await
    {
        Ok(task_id) => {
            // Drain inline so a freshly-queued task lands on an idle target
            // immediately when one exists. Pause checks inside drain handle
            // rate-limit gating naturally.
            drain_team_queue(state, team_id).await;
            CliOutcome::ok(format!("queue_id={} status=queued role={}\n", task_id, target_role))
        }
        Err(e) => CliOutcome::err(1, format!("team to: enqueue failed: {}\n", e)),
    }
}

async fn cli_verb_spawn(
    state: &Arc<AppState>,
    sender_session_id: &str,
    team_id: Option<&str>,
    sender_role: Option<&str>,
    args: &[String],
) -> CliOutcome {
    let Some(team_id) = team_id else {
        return CliOutcome::err(1, "team spawn: this session is not part of a team.\n");
    };
    let Some(sender_role) = sender_role else {
        return CliOutcome::err(1, "team spawn: missing sender role.\n");
    };
    if sender_role != "Team Lead" {
        return CliOutcome::err(
            3,
            "team spawn: rejected — only the Team Lead may spawn members.\n",
        );
    }
    if args.len() < 2 {
        return CliOutcome::err(2, "team spawn: usage: team spawn <role> <task>\n");
    }
    let role_name = &args[0];
    let initial_task = &args[1];
    if initial_task.trim().is_empty() {
        return CliOutcome::err(2, "team spawn: task is empty.\n");
    }

    // The spawn task needs the lead's channel/project to spawn the new
    // member into. Look up the lead's session row for that.
    let lead = match state
        .db
        .get_session_by_id_prefix(&sender_session_id[..8.min(sender_session_id.len())])
        .await
    {
        Ok(Some(s)) => s,
        Ok(None) => return CliOutcome::err(1, "team spawn: lead session not found.\n"),
        Err(e) => return CliOutcome::err(1, format!("team spawn: lookup failed: {}\n", e)),
    };

    let payload = serde_json::json!({
        "channel_id": lead.channel_id,
        "project": lead.project,
    });

    match state
        .db
        .enqueue_team_spawn(
            team_id, role_name, sender_session_id, sender_role,
            initial_task, &payload, SPAWN_DEDUPE_WINDOW_SECS,
        )
        .await
    {
        Ok(Ok(task_id)) => {
            drain_team_queue(state, team_id).await;
            CliOutcome::ok(format!(
                "queue_id={} status=queued role={}\n", task_id, role_name,
            ))
        }
        Ok(Err(existing_id)) => CliOutcome::err(
            // Exit 4 = duplicate intent (a same-role + same-task spawn is
            // already in flight). Lead can poll `team status` to see it.
            4,
            format!(
                "team spawn: rejected reason=duplicate_intent existing_queue_id={} role={}\n",
                existing_id, role_name,
            ),
        ),
        Err(e) => CliOutcome::err(1, format!("team spawn: enqueue failed: {}\n", e)),
    }
}

async fn cli_verb_interrupt(
    state: &Arc<AppState>,
    _sender_session_id: &str,
    team_id: Option<&str>,
    sender_role: Option<&str>,
    args: &[String],
) -> CliOutcome {
    let Some(team_id) = team_id else {
        return CliOutcome::err(1, "team interrupt: this session is not part of a team.\n");
    };
    let Some(sender_role) = sender_role else {
        return CliOutcome::err(1, "team interrupt: missing sender role.\n");
    };
    if sender_role != "Team Lead" {
        return CliOutcome::err(3, "team interrupt: rejected — Team Lead only.\n");
    }
    if args.is_empty() {
        return CliOutcome::err(2, "team interrupt: usage: team interrupt <role>\n");
    }
    let target_role = &args[0];

    let targets = match state.db.get_sessions_by_role(team_id, target_role).await {
        Ok(t) => t,
        Err(e) => return CliOutcome::err(1, format!("team interrupt: lookup failed: {}\n", e)),
    };
    if targets.is_empty() {
        return CliOutcome::err(
            5,
            format!("team interrupt: no member matches role '{}'.\n", target_role),
        );
    }

    let mut interrupted = 0i32;
    let mut failed = 0i32;
    for t in &targets {
        let role = t.role.as_deref().unwrap_or(target_role);
        // Flip the running queue row → 'cancelled' regardless of whether
        // the SDK actually had a turn to cancel (matches the marker
        // handler's behaviour). Idempotent.
        if let Err(e) = state
            .db
            .mark_running_message_task_cancelled_for_session(&t.session_id, "interrupted_by_lead")
            .await
        {
            tracing::warn!(error = %e, session_id = %t.session_id, role,
                "queue: failed to mark running task cancelled (cli)");
        }
        match state.containers.interrupt(&t.session_id).await {
            Ok(true) => {
                interrupted += 1;
                let _ = state
                    .mm
                    .post_in_thread(
                        &t.channel_id, &t.thread_id,
                        ":octagonal_sign: **Interrupted by Team Lead** — current turn aborted.",
                    )
                    .await;
            }
            Ok(false) => failed += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!(error = %e, session_id = %t.session_id, role, "interrupt RPC failed (cli)");
            }
        }
    }

    let exit_code = if failed > 0 && interrupted == 0 { 6 } else { 0 };
    CliOutcome { exit_code,
        stdout: format!(
            "interrupted={} failed={} role={}\n",
            interrupted, failed, target_role,
        ),
        stderr: if failed > 0 {
            format!("team interrupt: {} session(s) had no in-flight turn or RPC failed.\n", failed)
        } else { String::new() },
    }
}

async fn cli_verb_cancel(
    state: &Arc<AppState>,
    sender_session_id: &str,
    team_id: Option<&str>,
    args: &[String],
    flags: &std::collections::HashMap<String, String>,
) -> CliOutcome {
    let Some(team_id) = team_id else {
        return CliOutcome::err(1, "team cancel: this session is not part of a team.\n");
    };

    // Mode 1: cancel a single queue_id (when one is provided as positional).
    if let Some(qid_str) = args.first() {
        let task_id: i64 = match qid_str.parse() {
            Ok(n) => n,
            Err(_) => {
                return CliOutcome::err(
                    2, format!("team cancel: '{}' is not a valid queue_id.\n", qid_str),
                );
            }
        };
        match state
            .db
            .cancel_queued_task_by_id(task_id, sender_session_id)
            .await
        {
            Ok(true) => CliOutcome::ok(format!("cancelled queue_id={}\n", task_id)),
            // Already-running / delivered / wrong sender → nothing flipped.
            Ok(false) => CliOutcome::err(
                7,
                format!(
                    "team cancel: queue_id={} not cancellable (already delivered, or not yours).\n",
                    task_id,
                ),
            ),
            Err(e) => CliOutcome::err(1, format!("team cancel: {}\n", e)),
        }
    } else {
        // Mode 2: cancel everything queued from this sender, optionally
        // filtered by --role=<target_role>.
        let Some(role) = flags.get("role") else {
            return CliOutcome::err(
                2,
                "team cancel: provide a queue_id or --role=<target_role>.\n",
            );
        };
        match state
            .db
            .cancel_queued_from_sender(team_id, sender_session_id, role)
            .await
        {
            Ok(count) => CliOutcome::ok(format!(
                "cancelled count={} role={}\n",
                count, role,
            )),
            Err(e) => CliOutcome::err(1, format!("team cancel: {}\n", e)),
        }
    }
}

async fn cli_verb_clear(
    state: &Arc<AppState>,
    sender_session_id: &str,
    team_id: Option<&str>,
    sender_role: Option<&str>,
    args: &[String],
) -> CliOutcome {
    let Some(team_id) = team_id else {
        return CliOutcome::err(1, "team clear: this session is not part of a team.\n");
    };
    let Some(sender_role) = sender_role else {
        return CliOutcome::err(1, "team clear: missing sender role.\n");
    };
    if args.is_empty() {
        return CliOutcome::err(2, "team clear: usage: team clear <role|Self>\n");
    }
    let target_role = &args[0];

    // 'Self' means clear the caller. Anyone may self-clear; only the lead
    // may clear others.
    let is_self = target_role.eq_ignore_ascii_case("Self");
    if !is_self && sender_role != "Team Lead" {
        return CliOutcome::err(
            3,
            "team clear: rejected — Team Lead may clear others; anyone may 'Self' clear.\n",
        );
    }

    // Resolve target sessions.
    let targets = if is_self {
        match state
            .db
            .get_session_by_id_prefix(&sender_session_id[..8.min(sender_session_id.len())])
            .await
        {
            Ok(Some(s)) => vec![s],
            _ => return CliOutcome::err(5, "team clear: caller session not found.\n"),
        }
    } else {
        match state.db.get_sessions_by_role(team_id, target_role).await {
            Ok(t) if !t.is_empty() => t,
            Ok(_) => return CliOutcome::err(
                5,
                format!("team clear: no member matches role '{}'.\n", target_role),
            ),
            Err(e) => return CliOutcome::err(1, format!("team clear: lookup failed: {}\n", e)),
        }
    };

    let mut queued_ids: Vec<i64> = Vec::new();
    let mut cooldown_skipped = 0i32;
    let mut already_queued = 0i32;
    for t in &targets {
        let role = t.role.as_deref().unwrap_or(target_role);
        let result = state
            .db
            .enqueue_team_clear(
                team_id, role, &t.session_id, sender_session_id, sender_role,
                if is_self { "self_clear" } else { "lead_clear" },
                AUTO_CLEAR_COOLDOWN_SECS as i64,
            )
            .await;
        match result {
            Ok(EnqueueClearResult::Enqueued(id)) => queued_ids.push(id),
            Ok(EnqueueClearResult::AlreadyQueued(_)) => already_queued += 1,
            Ok(EnqueueClearResult::Cooldown { remaining_secs: _ }) => cooldown_skipped += 1,
            Err(e) => return CliOutcome::err(1, format!("team clear: enqueue failed: {}\n", e)),
        }
    }

    drain_team_queue(state, team_id).await;

    let mut out = String::new();
    if !queued_ids.is_empty() {
        out.push_str(&format!(
            "queued count={} queue_ids={:?}\n",
            queued_ids.len(), queued_ids,
        ));
    }
    if already_queued > 0 {
        out.push_str(&format!("already_queued={}\n", already_queued));
    }
    if cooldown_skipped > 0 {
        out.push_str(&format!("cooldown_skipped={}\n", cooldown_skipped));
    }
    if queued_ids.is_empty() && already_queued == 0 && cooldown_skipped == 0 {
        // No row matched any branch — defensive (shouldn't happen).
        out.push_str("no_action\n");
    }
    CliOutcome::ok(out)
}

async fn cli_verb_broadcast(
    state: &Arc<AppState>,
    sender_session_id: &str,
    team_id: Option<&str>,
    sender_role: Option<&str>,
    args: &[String],
) -> CliOutcome {
    let Some(team_id) = team_id else {
        return CliOutcome::err(1, "team broadcast: this session is not part of a team.\n");
    };
    let Some(sender_role) = sender_role else {
        return CliOutcome::err(1, "team broadcast: missing sender role.\n");
    };
    if sender_role != "Team Lead" {
        return CliOutcome::err(3, "team broadcast: rejected — Team Lead only.\n");
    }
    if args.is_empty() {
        return CliOutcome::err(2, "team broadcast: usage: team broadcast <message>\n");
    }
    let message = &args[0];
    if message.trim().is_empty() {
        return CliOutcome::err(2, "team broadcast: message is empty.\n");
    }

    // Enumerate every active member except the sender. We resolve the roster
    // here (under the same DB read) so a member who spawns mid-broadcast
    // doesn't get a partial fanout — they'll be picked up by the next
    // explicit `team to`.
    let members = match state.db.get_team_members(team_id).await {
        Ok(m) => m,
        Err(e) => return CliOutcome::err(1, format!("team broadcast: roster lookup failed: {}\n", e)),
    };
    let recipients: Vec<&StoredSession> = members
        .iter()
        .filter(|m| m.session_id != sender_session_id && m.status == "active")
        .filter(|m| m.role.is_some())
        .collect();
    if recipients.is_empty() {
        return CliOutcome::err(
            5,
            "team broadcast: no other active members on this team.\n",
        );
    }

    // One queue row per recipient → independently cancellable / pause-aware /
    // ordered alongside the rest of the queue. Each row uses the recipient's
    // exact role (not a prefix) so the drain delivers it to *that* member.
    let mut queue_ids: Vec<i64> = Vec::new();
    for m in &recipients {
        let Some(role) = m.role.as_deref() else { continue };
        match state
            .db
            .enqueue_team_task(
                team_id, role, /* is_prefix_match */ false,
                sender_session_id, sender_role, message, None,
            )
            .await
        {
            Ok(id) => queue_ids.push(id),
            Err(e) => {
                tracing::warn!(error = %e, role, "team broadcast: enqueue failed for one member");
            }
        }
    }

    if queue_ids.is_empty() {
        return CliOutcome::err(
            1, "team broadcast: every per-member enqueue failed; nothing dispatched.\n",
        );
    }

    drain_team_queue(state, team_id).await;
    CliOutcome::ok(format!(
        "broadcast count={} queue_ids={:?}\n",
        queue_ids.len(), queue_ids,
    ))
}

async fn cli_verb_pr_ready(
    state: &Arc<AppState>,
    sender_session_id: &str,
    team_id: Option<&str>,
    sender_role: Option<&str>,
    args: &[String],
) -> CliOutcome {
    let Some(team_id) = team_id else {
        return CliOutcome::err(1, "team pr-ready: this session is not part of a team.\n");
    };
    let Some(sender_role) = sender_role else {
        return CliOutcome::err(1, "team pr-ready: missing sender role.\n");
    };
    if args.is_empty() {
        return CliOutcome::err(2, "team pr-ready: usage: team pr-ready <pr_number>\n");
    }
    let pr_number = &args[0];
    if pr_number.parse::<u32>().is_err() {
        return CliOutcome::err(2, format!("team pr-ready: '{}' is not a valid PR number.\n", pr_number));
    }
    // Existing handler does the review-gate injection + Mattermost
    // visibility. Preserves all the prior side effects (system message back
    // to the sender instructing them to wait for Claude Reviewer CI, etc.)
    // — this CLI verb is a thin shim onto it.
    handle_pr_ready(state, sender_session_id, team_id, sender_role, pr_number, "").await;
    CliOutcome::ok(format!("pr-ready pr={} status=acknowledged\n", pr_number))
}

async fn cli_verb_pr_merged(
    state: &Arc<AppState>,
    sender_session_id: &str,
    team_id: Option<&str>,
    sender_role: Option<&str>,
    args: &[String],
) -> CliOutcome {
    let Some(team_id) = team_id else {
        return CliOutcome::err(1, "team pr-merged: this session is not part of a team.\n");
    };
    let Some(sender_role) = sender_role else {
        return CliOutcome::err(1, "team pr-merged: missing sender role.\n");
    };
    if sender_role != "Team Lead" {
        return CliOutcome::err(
            3,
            "team pr-merged: rejected — Team Lead only (members don't merge).\n",
        );
    }
    if args.is_empty() {
        return CliOutcome::err(2, "team pr-merged: usage: team pr-merged <pr_number>\n");
    }
    let pr_number = &args[0];
    if pr_number.parse::<u32>().is_err() {
        return CliOutcome::err(
            2, format!("team pr-merged: '{}' is not a valid PR number.\n", pr_number),
        );
    }

    // Workflow-boundary self-clear. The lead just merged a PR; the
    // per-PR review chatter is no longer load-bearing for the next phase.
    // Goes through the queue with the standard cooldown so a back-to-back
    // double-call (or coincident auto-clear) collapses cleanly.
    let reason = format!("breakpoint:pr_{}_merged", pr_number);
    match state
        .db
        .enqueue_team_clear(
            team_id, sender_role, sender_session_id, sender_session_id,
            sender_role, &reason, AUTO_CLEAR_COOLDOWN_SECS as i64,
        )
        .await
    {
        Ok(EnqueueClearResult::Enqueued(task_id)) => {
            drain_team_queue(state, team_id).await;
            CliOutcome::ok(format!(
                "pr-merged pr={} status=queued queue_id={} reason={}\n",
                pr_number, task_id, reason,
            ))
        }
        Ok(EnqueueClearResult::AlreadyQueued(task_id)) => CliOutcome::ok(format!(
            "pr-merged pr={} status=already_queued queue_id={}\n",
            pr_number, task_id,
        )),
        Ok(EnqueueClearResult::Cooldown { remaining_secs }) => CliOutcome::ok(format!(
            "pr-merged pr={} status=cooldown remaining_secs={}\n",
            pr_number, remaining_secs,
        )),
        Err(e) => CliOutcome::err(1, format!("team pr-merged: {}\n", e)),
    }
}

async fn cli_verb_pr_reviewed(
    state: &Arc<AppState>,
    sender_session_id: &str,
    team_id: Option<&str>,
    sender_role: Option<&str>,
    args: &[String],
) -> CliOutcome {
    let Some(team_id) = team_id else {
        return CliOutcome::err(1, "team pr-reviewed: this session is not part of a team.\n");
    };
    let Some(sender_role) = sender_role else {
        return CliOutcome::err(1, "team pr-reviewed: missing sender role.\n");
    };
    if args.is_empty() {
        return CliOutcome::err(2, "team pr-reviewed: usage: team pr-reviewed <pr_number>\n");
    }
    let pr_number = &args[0];
    if pr_number.parse::<u32>().is_err() {
        return CliOutcome::err(
            2, format!("team pr-reviewed: '{}' is not a valid PR number.\n", pr_number),
        );
    }
    // Member is done with the review gate. Match the marker handler's
    // behaviour: clear pending_task_from before forwarding so the member
    // shows idle while the lead reviews. Then notify the lead.
    let _ = state.db.clear_pending_task(sender_session_id).await;
    handle_pr_reviewed(state, sender_session_id, team_id, sender_role, pr_number, "").await;
    CliOutcome::ok(format!("pr-reviewed pr={} status=forwarded_to_lead\n", pr_number))
}

/// When a member's turn dies during a rate-limit pause, push the row in the
/// 'running' state back to 'queued' so the next drain pass redelivers it.
/// The original `created_at` is preserved → the task keeps its place in line
/// relative to other queued work; no orphan-row duplication.
///
/// `pending_task_from` is cleared so the member shows idle again (and is
/// eligible to pick up the re-queued row, or any other queued task, on
/// resume).
///
/// Idempotent: subsequent ProcessDied events on the same session find no
/// running row and return false — no double-bump.
async fn requeue_in_flight_after_rate_limit(
    state: &Arc<AppState>,
    team_id: &str,
    session_id: &str,
) -> bool {
    // Push delivery to (resets_at + 60s) so we don't slam the API the instant
    // the window flips. Fall back to NOW() if we don't have a reset
    // timestamp — `drain_team_queue` is paused via `is_team_rate_limited`
    // anyway.
    let deliver_after = state
        .db
        .get_team_rate_limit(team_id)
        .await
        .ok()
        .flatten()
        .and_then(|rl| rl.resets_at)
        .map(|t| t + chrono::Duration::seconds(60))
        .unwrap_or_else(chrono::Utc::now);

    let task_id = match state
        .db
        .requeue_running_message_task_for_session(session_id, deliver_after)
        .await
    {
        Ok(Some(id)) => id,
        Ok(None) => {
            // Nothing in 'running' for this session — nothing to retry.
            // Clear the pending slot defensively (a turn might have died
            // before the queue row transitioned).
            let _ = state.db.clear_pending_task(session_id).await;
            return false;
        }
        Err(e) => {
            tracing::warn!(error = %e, team_id, session_id, "requeue: UPDATE running→queued failed");
            return false;
        }
    };

    let _ = state.db.clear_pending_task(session_id).await;

    tracing::info!(
        team_id, session_id, task_id,
        deliver_after = %deliver_after,
        "Re-queued in-flight task killed by rate limit (running → queued)",
    );

    post_to_coordination_log(
        state,
        team_id,
        &format!(
            ":arrows_counterclockwise: Rate-limit re-queue: queue_id={} returned to pending; redelivers after {}",
            task_id,
            deliver_after.format("%H:%M UTC"),
        ),
    )
    .await;
    true
}

/// Resume after a rate-limit window: flip the row to `allowed`, edit the
/// existing notice, and trigger a queue drain pass for any pending tasks
/// that piled up.
async fn clear_rate_limit_notice_and_drain(state: &Arc<AppState>, team_id: &str) {
    let prior_post_id = match state.db.clear_team_rate_limit(team_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, team_id, "Failed to clear team rate limit");
            None
        }
    };
    let body = format_rate_limit_notice("allowed", 0, "", None);
    if let Some(post_id) = prior_post_id {
        let _ = state.mm.update_post(&post_id, &body).await;
    } else if let Some(coord_channel_id) = ensure_coordination_channel(state).await
        && let Ok(Some(team)) = state.db.get_team(team_id).await
        && let Some(thread_id) = team.coordination_thread_id
    {
        let _ = state.mm.post_in_thread(&coord_channel_id, &thread_id, &body).await;
    }
    drain_team_queue(state, team_id).await;
}

/// Profile-rotation handler invoked from POST /admin/profile after the active
/// CLAUDE_CONFIG_DIR has been swapped. Enqueues a CLEAR row in
/// `team_task_queue` for every active member of every active team, then
/// drains. The drain layer's existing `pending_task_from.is_some()` skip
/// (see `drain_one_clear`) leaves mid-turn members queued — their next
/// `ResponseComplete` re-fires the drain and the CLEAR delivers cleanly.
/// Idle members get cleared immediately. `enqueue_team_clear`'s coalescing
/// + cooldown gates make repeated flips safe.
///
/// Teams currently in `rejected` rate-limit state additionally have their
/// rate-limit row flipped back to `allowed` so drain isn't paused.
///
/// Standalone (non-team) sessions are out of scope: they have no queue and
/// will pick up the new profile when the user manually clears or restarts.
pub(crate) async fn rotate_active_team_sessions(state: &Arc<AppState>) -> anyhow::Result<()> {
    let rejected: std::collections::HashSet<String> = state
        .db
        .teams_in_rejected_state()
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();

    let teams = state.db.teams_with_active_sessions().await?;
    if teams.is_empty() {
        return Ok(());
    }
    tracing::info!(
        teams = teams.len(),
        rejected = rejected.len(),
        "Profile rotation: enqueueing CLEAR for every active team member"
    );

    for team_id in teams {
        let members = match state.db.get_team_members(&team_id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, %team_id, "rotate: get_team_members failed");
                continue;
            }
        };
        for m in members {
            let role = match m.role.as_deref() {
                Some(r) if !r.is_empty() => r,
                _ => continue,
            };
            // sender_session_id == target_session_id makes drain treat this
            // as a self-clear, which is the right semantic: the session is
            // being asked to wipe its own context, not at a Lead's behest.
            let result = state
                .db
                .enqueue_team_clear(
                    &team_id,
                    role,
                    &m.session_id,
                    &m.session_id,
                    "ProfileRotation",
                    "profile-rotation",
                    AUTO_CLEAR_COOLDOWN_SECS as i64,
                )
                .await;
            if let Err(e) = result {
                tracing::warn!(
                    error = %e, session_id = %m.session_id,
                    "rotate: enqueue_team_clear failed"
                );
            }
        }

        if rejected.contains(&team_id) {
            // Updates the existing rate-limit notice in place + drains.
            clear_rate_limit_notice_and_drain(state, &team_id).await;
        } else {
            drain_team_queue(state, &team_id).await;
        }
    }
    Ok(())
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
        "[TEAM: You are {}. Members: {}. Run `team status` for queue depth + rate-limit state; \
         `team to <role> <message>` to assign work; `team --help` for the full verb list.]",
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

## Team CLI (the `team` binary)
All team-control actions flow through a single CLI installed in this container.
Invoke it from the `Bash` tool exactly like `git` or `gh` — the binary returns
synchronously with the result on stdout (and a non-zero exit code on failure).
There is NO marker grammar — do not emit `[TO:...]`, `[SPAWN:...]`, `[ACK:...]`
or anything similar in your response text. Those used to be parsed out of
your output and they no longer are. Use the CLI.

Verbs:
  team status [--json]
      Roster + queue depth + rate-limit window. Always run this first when
      planning routing decisions. `--json` returns structured output.

  team to <role> "<message>"
      Enqueue a task to a role. Returns `queue_id=N status=queued role=...`.
      Role can be a prefix (any idle member of that role wins) or exact
      ("Developer 2"). Multiple `team to` calls FIFO-deliver as members free.

  team spawn <role> "<initial_task>"
      Spawn a new member with their first task baked in. Returns
      `queue_id=N status=queued role=...`. Exits 4 if the same role+task is
      already in flight (duplicate_intent dedupe over a 5min SQL window) —
      poll `team status` to see the existing queue_id. Exits 3 if invoked
      by a non-Lead.

  team interrupt <role>
      Abort a member's in-flight turn (Lead-only). Prefix match interrupts
      every member of that role; exact match targets one. Returns
      `interrupted=N failed=M role=...`. The queue row for the running
      task flips to `cancelled` automatically.

  team cancel <queue_id>
  team cancel --role=<role>
      Drop a still-queued task before delivery. With a queue_id, that one
      row only (must be yours). With --role, all queued rows from you to
      that role. Already-running tasks can't be cancelled — use
      `team interrupt` for those.

  team clear <role|Self>
      Reset a member's context (Lead-only for others; anyone may
      `team clear Self`). Goes through the queue with a 180s cooldown.
      Returns `queued count=K queue_ids=[...]` plus optional
      `cooldown_skipped=M` / `already_queued=M` lines for diagnostics.
      `team clear Self` is workflow-boundary safe: it queues the clear,
      this turn finishes streaming, then the clear fires.

  team broadcast "<message>"
      Lead-only. Fans the message out as one queue row per active member
      (excluding you). Each row is independently cancellable / drainable
      / pause-aware. Returns `broadcast count=N queue_ids=[...]`. Prefer
      this over a manual loop of `team to` calls — the membership is
      resolved atomically at dispatch, so nobody is missed if a member
      spawns mid-fanout.

  team pr-merged <pr_number>
      Lead-only. Run as the LAST step after `gh pr merge <N>`. Queues a
      self-clear at the workflow boundary (per-PR review chatter is no
      longer load-bearing for the next phase). Shares the 180s clear
      cooldown, so back-to-back calls collapse cleanly. Returns
      `pr-merged pr=N status=queued queue_id=K reason=breakpoint:pr_N_merged`,
      or `status=cooldown` / `status=already_queued` when the no-op path fires.

Reading CLI exit codes:
  0  — action completed.
  1  — internal failure (DB / lookup error). Investigate via `team status`.
  2  — usage error (bad argv). Don't retry — fix the invocation.
  3  — rejected: lead-only verb invoked by non-lead.
  4  — rejected: duplicate intent (spawn within dedupe window).
  5  — rejected: target_not_found (role missing on team).
  6  — partial failure (e.g. interrupt: some sessions had no in-flight turn).
  7  — rejected: queue_id not cancellable (already running, or not yours).
 64  — verb not implemented yet (transitional; should not appear).

Sending messages between members ALWAYS goes through `team to`. The team
context header `[TEAM: ...]` you see prepended to incoming messages is
informational — it is NOT a CLI verb you emit.

## Task Queue Model
`team to` does NOT deliver immediately — your task is enqueued and given a
queue_id. The system delivers it the moment the target role has an idle
member. State machine:

  queued ──drain success──► running ──ResponseComplete──► completed
     │                        │
     │                        ├─team interrupt──────► cancelled
     │                        │
     │                        └─rate-limit error────► queued (retry, deliver_after bumped)
     │
     └─team cancel──► cancelled

If you want to halt a member entirely:
  1. `team cancel --role=<role>` first, so the next queued task doesn't immediately
     replace what you stop.
  2. `team interrupt <role>` to abort the in-flight turn.

Messages from team members arrive prefixed with their role.
Every message you receive includes a `[TEAM: ...]` header with the current roster.

## Rate Limits
When the Anthropic CLI signals a `rate_limit_event`, the team task queue
auto-pauses team-wide. In-flight turns that error during the window are
re-queued (preserving created_at order) and redelivered automatically once
the window resets. You'll see one notice card on the team coordination
thread; you do not need to do anything about it. `team status` shows the
window in its footer.

## Scheduled Tasks (out-of-band)
The user can type `/schedule <role> <+5h|+30m|+2d|RFC3339> <message>` directly
into your thread to enqueue a delayed task — useful for things like "ping the
Developer in 5 hours to continue work after the rate-limit window." The system
intercepts these before you ever see them, inserts into the queue with a
deliver_after timestamp, and they fire when due. You'll see them arrive as
normal `From <role>` messages once delivered.

## PR Review Gate
Members signal PR lifecycle through `team pr-ready <N>` and `team pr-reviewed
<N>`. The system gates PR delivery to you on the Claude Reviewer CI passing
PLUS the member running `team pr-reviewed <N>`. Only PRs that arrive with
"review gate PASSED" in the message body are cleared for merge.

## Your Responsibilities
{responsibilities}

## Task Assignment Protocol
- `team to <role>` enqueues — the task waits in the DB until the role has an idle member.
- Pile up multiple `team to` calls; they FIFO-deliver as members free.
- Don't retry on `status=queued` — it's already in the queue.
- If you change your mind before delivery, `team cancel <queue_id>` then re-issue.
- After delivery (member starts running), only `team interrupt` can stop the work.
- Don't assign the same task to multiple developers unless you want parallel approaches.
- Run `team status` to inspect roster + queue depth + rate-limit state.
- Before spawning, ALWAYS check `team status` — `team spawn` is rejected
  (exit 4) when a same-role same-task is in flight. Prefer `team to` to
  reuse an idle existing member.

## Context Hygiene at Workflow Boundaries
Long-running coordination sessions accumulate per-PR review chatter and
exploratory diffs that bear no value once work merges. Auto-clear at 90% will
eventually fire, but it's an emergency wipe; prefer voluntary clears at phase
boundaries.

**Self-clear** — `team clear Self` as the LAST CLI call in your response after:
- Acknowledging an approved spec from the Architect, before invoking `plan`
- Finishing a `plan` tool call, before spawning developers for the new stories
- Merging a story PR, before reviewing or assigning the next one

`team clear Self` is mid-turn-safe by construction (the queued clear fires on
this turn's ResponseComplete, after the response has streamed). After the clear
you receive a `POST_CLEAR_BRIEFING` reconstructing roster + queued tasks +
member current_tasks. You wake up oriented for the next phase.

**Member-clear** — `team clear <Role>` BEFORE the next `team to <Role>` after:
- Merging a Developer's PR (story done; clear them before assigning the next story)
- The Architect finishes a spec / `architecture` / clarification round
- Any member completes a multi-turn deliverable and you're reassigning them

The canonical pattern (drain serializes — the clear fires first, then the new
task hits a clean session):
```
team clear "Developer 2"
team to "Developer 2" "start work on Story I"
```

Don't clear mid-story or mid-deliverable; only at completion boundaries.

## Development Workflow (spec-flow)
Coordinate members through these phases IN ORDER:

1. **Specify** (Architect): `team spawn Architect "use spec-flow specify..."`, `clarify` to resolve gaps
2. **Plan** (You): Once specs are approved, use the spec-flow `plan` tool to break specs into stories
3. **Implement** (Developers): Only AFTER stories exist, `team spawn Developer "..."` for each story
4. **Review** (You): Review PRs using gh CLI, delegate fixes via `team to <role>`, merge when CI passes

IMPORTANT: Do NOT spawn Developers until stories have been planned from approved specs.

## PR Review & Completion
**CRITICAL MERGE RULE**: NEVER merge a PR unless you received an explicit
"review gate PASSED" notification from the system for that specific PR number.
If a team member mentions a PR via `team to "Team Lead"` without the review
gate notification, do NOT merge it — tell them to run `team pr-ready <N>` first.

When you receive a review-gate-passed PR notification:

1. **Verify Claude Reviewer approved**: `gh pr view <number> --json reviews --jq '.reviews[] | "\(.state): \(.body)"'`
2. **Verify all CI passes**: `gh pr checks <number>` — all checks must be green
3. **Review the diff**: `gh pr view <number>` and `gh pr diff <number>`
4. **Verify architectural alignment**: complex fixes should have been escalated to the Architect during the review gate.
5. If changes needed, send SPECIFIC feedback via `team to <role> "<file:line — what to change>"`
6. Wait for fixes before re-reviewing — do not merge until Claude Reviewer has approved and all CI passes
7. After merging implementation PRs, ask Architect to run the spec-flow `architecture` tool to update project docs
8. Merge with `gh pr merge <number> --squash` ONLY when ALL of the above are satisfied
9. Run `team pr-merged <number>` as the LAST step — this queues your self-clear at the workflow boundary so the next PR review starts with a clean context"#,
    ))
}

/// Build a team member's system prompt (static, set at session creation).
fn build_team_member_prompt(role: &StoredTeamRole) -> String {
    format!(
        r#"You are the **{display_name}** on a development team.

## Team CLI (the `team` binary)
All cross-member communication flows through the `team` CLI. Invoke it from
the `Bash` tool. There is NO marker grammar — do not emit `[TO:...]` etc. in
your response text; those are no longer parsed.

Verbs you'll use:
  team status                            Roster + queue depth.
  team to "<role>" "<message>"           Send a message to another member.
  team to "Team Lead" "<summary>"        How you report results back.
  team clear Self                        Reset YOUR OWN context. Cooldown applies.

The Team Lead also has `team spawn`, `team interrupt`, `team cancel`, `team
clear <other-role>` — those are Lead-only and exit 3 if you try them.

Messages from other members arrive prefixed with their role.
Every message you receive includes a `[TEAM: ...]` header with the current roster.

## Your Responsibilities
{responsibilities}

## Workflow
- Wait for task assignment from the Team Lead before starting work.
- ALWAYS report back to the Team Lead when your task is complete via
  `team to "Team Lead" "<summary of what you did>"`.
- After completing any assigned work (reviews, architecture updates, analysis),
  notify Team Lead with results.
- Address review feedback promptly when delegated by Team Lead.
- Do NOT start new work until your current PR is merged or you are reassigned.
- One task at a time — finish current work before accepting new assignments.
- IMPORTANT: Never finish a response without `team to "Team Lead" "..."` — the
  Team Lead cannot see your work unless you explicitly send it.

## PR Review Gate (IMPORTANT)
After creating a PR, do NOT send it directly to the Team Lead via `team to`.
Use the review gate:

1. `team pr-ready <N>` (N = PR number) — signals the PR is ready and triggers the Claude Reviewer CI gate.
2. The system will instruct you to wait for the Claude Reviewer CI to post its review.
3. If the Claude Reviewer **APPROVES** → skip to step 7.
4. If it **REQUESTS CHANGES** — categorise each comment as Simple or Complex:
   - **Simple** (fix yourself): code style, naming, missing error handling, missing tests, minor localised bugs, dead code
   - **Complex** (escalate to Architect): interface/API changes, new dependencies, module restructuring, security/auth pattern changes, design pattern deviations, schema changes
5. Fix Simple issues with actual code changes (documentation-only fixes are NOT acceptable).
6. For Complex issues: `team to "Architect" "<proposed solution>"` — wait for their feedback, then implement. Push fixes and repeat from step 2 until the Claude Reviewer APPROVES.
7. `team pr-reviewed <N>` — confirms review comments are addressed and forwards the PR to the Team Lead.

Do NOT use `team to "Team Lead"` for PR notifications — use `team pr-ready <N>` and `team pr-reviewed <N>`."#,
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

/// Drain queued tasks for a team. Walks the queue oldest-first, atomically
/// claims an idle target per task, sends the message via `containers.send`,
/// and emits the delivery ACK to the sender. Per-team mutex serializes
/// overlapping calls (two ResponseCompletes from sibling members of the same
/// team won't double-deliver).
///
/// Single pass — caller is responsible for re-triggering on the events that
/// can change drain outcomes (ResponseComplete, Spawn, Interrupt).
///
/// Returns the task_ids delivered in this pass. Callers who just enqueued a
/// task (e.g. `route_team_message`) check the returned set to decide whether
/// to publish the "Queued for…" Mattermost mirror — instant deliveries skip
/// it, since the drain's "Delivered to…" post already carries the body.
///
/// Takes `&Arc<AppState>` so the spawn-task branch can clone the handle into
/// `handle_team_spawn` (which needs an owned `Arc`).
async fn drain_team_queue(state: &Arc<AppState>, team_id: &str) -> Vec<i64> {
    // Per-team serialization. Use lock().await — drain passes are short and
    // we'd rather queue up than miss a freshly-enqueued task.
    let lock = state
        .drain_locks
        .entry(team_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _guard = lock.lock().await;

    let mut delivered_ids: Vec<i64> = Vec::new();

    // Hold delivery while the upstream rate-limit window is closed. The
    // scheduled-drain ticker flips the row back to `allowed` once
    // `resets_at` has passed and re-runs this drain.
    match state.db.is_team_rate_limited(team_id).await {
        Ok(true) => {
            tracing::debug!(team_id, "drain_team_queue: skipped (rate-limited)");
            return delivered_ids;
        }
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(error = %e, team_id, "drain_team_queue: rate-limit lookup failed; proceeding");
        }
    }

    let queued = match state.db.list_queued_tasks_for_team(team_id).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, team_id, "Failed to list queued tasks for drain");
            return delivered_ids;
        }
    };
    if queued.is_empty() {
        return delivered_ids;
    }

    // Cache the member list once for header construction during this pass.
    let members = state.db.get_team_members(team_id).await.unwrap_or_default();

    for task in queued {
        let delivered = match task.task_type.as_str() {
            "spawn" => drain_one_spawn(state, team_id, &task).await,
            "clear" => drain_one_clear(state.as_ref(), team_id, &task).await,
            _ => drain_one_message(state.as_ref(), team_id, &task, &members).await,
        };
        if delivered {
            delivered_ids.push(task.task_id);
        }
    }

    delivered_ids
}

/// Drain a single message-type task. Returns true if delivered, false if
/// rolled back / left queued / failed.
async fn drain_one_message(
    state: &AppState,
    team_id: &str,
    task: &StoredTeamTask,
    members: &[StoredSession],
) -> bool {
    let role_targets = match state
        .db
        .get_sessions_by_role(team_id, &task.target_role)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, task_id = task.task_id, "Drain: role lookup failed");
            return false;
        }
    };

    // Role no longer exists on the team — fail the task so it doesn't
    // pile up forever. (Could happen if the only member of a role was
    // stopped after the task was queued.)
    if role_targets.is_empty() {
        let _ = state
            .db
            .mark_task_failed(task.task_id, "target_not_found")
            .await;
        return false;
    }

    // Idle candidates only. For exact match (is_prefix_match=false), the
    // role_targets list is just the one named member; if busy, leave queued.
    let candidates: Vec<&StoredSession> = role_targets
        .iter()
        .filter(|t| t.pending_task_from.is_none())
        .collect();
    if candidates.is_empty() {
        // No idle target right now — leave queued, next ResponseComplete
        // on this team triggers another drain pass.
        return false;
    }

    // Try to claim each idle candidate; first success wins. The atomic
    // `try_set_pending_task` (UPDATE ... WHERE pending_task_from IS NULL)
    // guarantees no two drains can both attach to the same target.
    let mut delivered_target: Option<&StoredSession> = None;
    for candidate in &candidates {
        match state
            .db
            .try_set_pending_task(&candidate.session_id, &task.sender_session_id, &task.message)
            .await
        {
            Ok(true) => {
                delivered_target = Some(*candidate);
                break;
            }
            Ok(false) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "Drain: try_set_pending_task failed");
                continue;
            }
        }
    }

    let target = match delivered_target {
        Some(t) => t,
        None => return false,
    };
    let role = target.role.as_deref().unwrap_or(&task.target_role);

    // Mattermost visibility for the recipient
    let _ = state
        .mm
        .post_in_thread(
            &target.channel_id,
            &target.thread_id,
            &format!(
                ":arrow_left: **From {}** (queue_id={}):\n{}",
                task.sender_role,
                task.task_id,
                truncate_preview(&task.message, 4000)
            ),
        )
        .await;

    let header = format_team_context_header(members, role);
    let formatted = format!("{}\n**From {}**: {}", header, task.sender_role, task.message);

    match state.containers.send(&target.session_id, &formatted).await {
        Ok(()) => {
            // Message tasks have a meaningful in-flight phase: the member is
            // working on the prompt until ResponseComplete. Park the row in
            // 'running' until that signal lands (or until rate-limit / error
            // forces it back to 'queued').
            let _ = state
                .db
                .mark_task_running(task.task_id, &target.session_id)
                .await;
            post_to_coordination_log(
                state,
                team_id,
                &format!(
                    ":arrow_right: **{} → {}** (queue_id={}): {}",
                    task.sender_role,
                    role,
                    task.task_id,
                    truncate_preview(&task.message, 4000),
                ),
            )
            .await;
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, role, task_id = task.task_id,
                "Drain: containers.send failed; rolling back claim");
            // Release the slot we just claimed and mark the task failed.
            let _ = state.db.clear_pending_task(&target.session_id).await;
            let _ = state
                .db
                .mark_task_failed(task.task_id, "delivery_unreachable")
                .await;
            false
        }
    }
}

/// Drain a single spawn-type task. Calls handle_team_spawn (which does the
/// claim + start_session + initial-task send + Lead ACK) and marks the queue
/// row based on the outcome.
async fn drain_one_spawn(state: &Arc<AppState>, team_id: &str, task: &StoredTeamTask) -> bool {
    let payload = task.spawn_payload.as_ref();
    let channel_id = payload
        .and_then(|p| p.get("channel_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let project = payload
        .and_then(|p| p.get("project"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if channel_id.is_empty() || project.is_empty() {
        tracing::error!(task_id = task.task_id, "Spawn task missing channel_id/project payload");
        let _ = state.db.mark_task_failed(task.task_id, "bad_payload").await;
        return false;
    }

    let outcome = handle_team_spawn(
        state.clone(),
        team_id,
        &channel_id,
        &task.sender_session_id,
        &task.sender_role,
        &task.target_role,
        &task.message,
        &project,
        Some(task.task_id),
    )
    .await;

    match outcome {
        SpawnOutcome::Delivered => {
            // We don't have a single target_session_id to record (the new
            // member's session_id is internal to handle_team_spawn). Record
            // the sender's session_id as a placeholder; mark_task_completed
            // requires a non-null target. The semantic value here is "the
            // spawn finished — task is no longer pending."
            let _ = state
                .db
                .mark_task_completed(task.task_id, &task.sender_session_id)
                .await;
            true
        }
        SpawnOutcome::Rejected => {
            let _ = state.db.mark_task_failed(task.task_id, "rejected").await;
            false
        }
        SpawnOutcome::Failed => {
            let _ = state.db.mark_task_failed(task.task_id, "spawn_failed").await;
            false
        }
    }
}

/// Drain a single clear-type task. Disconnects the SDK client for
/// `target_session_id` and re-injects [TEAM_STATUS]. The cooldown was already
/// enforced at enqueue time (`enqueue_team_clear`), so no further gating.
async fn drain_one_clear(state: &AppState, team_id: &str, task: &StoredTeamTask) -> bool {
    let target_sid = match task.target_session_id.as_deref() {
        Some(s) => s,
        None => {
            tracing::error!(task_id = task.task_id, "Clear task missing target_session_id");
            let _ = state.db.mark_task_failed(task.task_id, "bad_payload").await;
            return false;
        }
    };
    let target = match state.db.get_session_by_id_prefix(&target_sid[..8.min(target_sid.len())]).await {
        Ok(Some(s)) => s,
        _ => {
            let _ = state
                .db
                .mark_task_failed(task.task_id, "target_not_found")
                .await;
            return false;
        }
    };
    // Skip if the target is mid-turn (claude is still processing). Leave
    // queued; the next ResponseComplete will trigger a fresh drain.
    if target.pending_task_from.is_some() {
        return false;
    }

    let target_role_label = target.role.as_deref().unwrap_or(&task.target_role);
    let reason = task.message.as_str();
    let is_self = task.sender_session_id == target.session_id;

    match state.containers.clear(&target.session_id).await {
        Ok(_) => {
            let _ = state
                .db
                .mark_task_completed(task.task_id, &target.session_id)
                .await;
            // Re-inject roster so the cleared session retains team context.
            handle_team_status(state, &target.session_id, team_id).await;

            // POST_CLEAR_BRIEFING reconstructs in-flight working state from
            // durable DB sources (queue, sessions.current_task) so the cleared
            // session wakes up oriented. Lead gets the rich workflow-aware
            // version; members get a one-line "your next assignment is coming"
            // since their useful context is per-task and arrives via [TO:].
            let is_lead = target.role.as_deref() == Some("Team Lead");
            let briefing = build_post_clear_briefing(state, team_id, &target, reason, is_lead).await;
            let _ = state.containers.send(&target.session_id, &briefing).await;

            // Mattermost mirror so the human reader sees the clear.
            let mm_msg = if is_self {
                ":broom: **Context auto-cleared** — membank will reload baseline context on next message.".to_string()
            } else {
                ":broom: **Context cleared by Team Lead** — membank will reload baseline context on your next message.".to_string()
            };
            let _ = state
                .mm
                .post_in_thread(&target.channel_id, &target.thread_id, &mm_msg)
                .await;

            post_to_coordination_log(
                state,
                team_id,
                &format!(
                    ":broom: **{} cleared {}** (queue_id={}, reason={})",
                    task.sender_role, target_role_label, task.task_id, reason,
                ),
            )
            .await;
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, session_id = %target.session_id, target_role = target_role_label,
                "Drain: clear RPC failed");
            let _ = state.db.mark_task_failed(task.task_id, "clear_failed").await;
            false
        }
    }
}


/// Build the [POST_CLEAR_BRIEFING] payload injected into a freshly-cleared
/// session. Reconstructs in-flight working state from durable DB sources so
/// the cleared session wakes up oriented rather than amnesiac.
///
/// Two flavors:
/// - **Lead** (workflow-aware): roster, queued tasks per-role, member
///   current_tasks, breakpoint reason. The Lead's job is coordination — it
///   needs to know what's in flight to continue routing.
/// - **Member** (minimal): one line acknowledging the clear and noting that
///   the next assignment will arrive via the queue. Members coordinate per-
///   task; they don't carry workflow state across tasks.
async fn build_post_clear_briefing(
    state: &AppState,
    team_id: &str,
    target: &StoredSession,
    reason: &str,
    is_lead: bool,
) -> String {
    if !is_lead {
        let role = target.role.as_deref().unwrap_or("you");
        return format!(
            "[POST_CLEAR_BRIEFING]\nReason: {}.\nYour next assignment from the Team Lead will arrive shortly via the task queue. Continue normally — membank reload will refresh your role-specific context on the next message you receive.",
            describe_clear_reason_full(reason, role),
        );
    }

    let members = state.db.get_team_members(team_id).await.unwrap_or_default();
    let mut roster_lines: Vec<String> = Vec::new();
    for m in &members {
        let role = m.role.as_deref().unwrap_or("Unknown");
        let task_blurb = match m.current_task.as_deref() {
            Some(t) if !t.is_empty() => {
                format!(": \"{}\"", truncate_preview(t, 80))
            }
            _ => " (idle)".to_string(),
        };
        let status = if m.pending_task_from.is_some() {
            "busy"
        } else {
            "idle"
        };
        roster_lines.push(format!("  - {} [{}]{}", role, status, task_blurb));
    }

    let queue_counts = state
        .db
        .queued_counts_by_role(team_id)
        .await
        .unwrap_or_default();
    let queue_block = if queue_counts.is_empty() {
        "  (none)".to_string()
    } else {
        queue_counts
            .iter()
            .map(|(r, n)| format!("  - {}: {} queued", r, n))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let next_line = breakpoint_next_hint(reason);
    format!(
        "[POST_CLEAR_BRIEFING]\nReason: {}.\n\nRoster (durable from DB):\n{}\n\nQueued tasks awaiting drain:\n{}\n\n{}\n\nUse [TEAM_STATUS] for live detail; pending queued items will deliver as members free up — you do not need to re-issue anything that already has a queue_id.",
        describe_clear_reason_full(reason, "Team Lead"),
        roster_lines.join("\n"),
        queue_block,
        next_line,
    )
}

/// Render the `reason` field into a human sentence for the briefing.
/// Recognises `auto_NNpct`, `manual`, `self`, and `breakpoint:pr_N_merged`
/// shapes. Returns an owned String because breakpoint reasons embed dynamic
/// values (PR number, breakpoint name).
fn describe_clear_reason_full(reason: &str, _who: &str) -> String {
    if let Some(rest) = reason.strip_prefix("auto_") {
        format!("context auto-cleared at the {} high-water threshold", rest)
    } else if reason == "self" {
        "self-clear at a workflow boundary".to_string()
    } else if let Some(detail) = reason.strip_prefix("breakpoint:") {
        if let Some(pr_part) = detail.strip_prefix("pr_")
            && let Some(n) = pr_part.strip_suffix("_merged")
        {
            return format!("workflow-breakpoint clear after merging PR #{}", n);
        }
        format!("workflow-breakpoint clear ({})", detail)
    } else if reason == "manual" {
        "Lead-initiated [CLEAR:Role]".to_string()
    } else {
        format!("context cleared ({})", reason)
    }
}

/// "Next:" suggestion line for the Lead briefing, tailored to the breakpoint.
/// PR-merge breakpoints suggest reviewing the next story; auto-clears suggest
/// resuming whatever was in flight; spec/plan breakpoints suggest the obvious
/// next phase.
fn breakpoint_next_hint(reason: &str) -> &'static str {
    if let Some(detail) = reason.strip_prefix("breakpoint:") {
        if detail.starts_with("pr_") && detail.ends_with("_merged") {
            return "Next: review the next open PR in the queue, OR pick up the next story for assignment. Check [TEAM_STATUS] for who's idle.";
        }
        if detail.starts_with("plan") {
            return "Next: spawn Developers (one per story) using [SPAWN:Developer] for the highest-priority items.";
        }
        if detail.starts_with("spec") {
            return "Next: invoke the spec-flow `plan` tool on the approved spec to create stories.";
        }
        return "Next: continue the workflow from the relevant phase.";
    }
    if reason.starts_with("auto_") {
        return "Next: resume the most recent task — check the user's last message in the thread and the queued items below.";
    }
    if reason == "self" {
        return "Next: continue the workflow from the relevant phase.";
    }
    "Next: continue normally."
}



/// Spawn outcome reported back to the drain. The drain uses this to decide
/// whether to mark the queue row delivered, cancelled, or failed.
#[derive(Debug, Clone, Copy)]
enum SpawnOutcome {
    /// Member spawned successfully. Drain marks task delivered.
    Delivered,
    /// Spawn rejected by policy (idle member exists, singleton already
    /// claimed). ACK already sent to Lead. Drain marks task cancelled.
    Rejected,
    /// Spawn failed (claim error, start_session error). ACK already sent.
    /// Drain marks task failed.
    Failed,
}

/// Handle a [SPAWN:Role] marker. Today only invoked via the team_task_queue
/// drain — the marker handler enqueues, the drain calls this. `queue_id` is
/// woven into every ACK so the Lead can correlate.
///
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
    queue_id: Option<i64>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = SpawnOutcome> + Send>> {
    // Own all string params for the async block
    let team_id = team_id.to_string();
    let channel_id = channel_id.to_string();
    let lead_session_id = lead_session_id.to_string();
    let role_name = role_name.to_string();
    let initial_task = initial_task.to_string();
    let project = project.to_string();
    let queue_suffix = match queue_id {
        Some(id) => format!(" queue_id={}", id),
        None => String::new(),
    };
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
                return SpawnOutcome::Failed;
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

        // Soft pre-check: if the role allows multiple and an idle member of the
        // same role exists, reject so the Lead reuses the idle one rather than
        // spawning yet another. This is best-effort UX — the atomic claim below
        // is what guarantees no duplicate (team_id, role) ever exists.
        if role_def.allow_multiple {
            let members = state.db.get_team_members(team_id).await.unwrap_or_default();
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
                    let ack = format!(
                        "[ACK:SPAWN role=\"{}\" status=rejected reason=idle_available idle=\"{}\"{}]",
                        role_def.display_name, idle_names[0], queue_suffix,
                    );
                    let _ = state.containers.send(&lead.session_id, &format!(
                    "{}\n{}\nSpawn rejected: {} is idle and available. Use [TO:{}] to assign them a task before spawning a new {}.\nThe task you wanted to assign was: {}",
                    ack, header, idle_list, idle_names[0], role_def.display_name, initial_task,
                )).await;
                }
                return SpawnOutcome::Rejected;
            }
        }

        // Atomically claim the (team_id, role) slot before any expensive work.
        // For singletons, returns None if the role is taken. For multi-instance,
        // serializes via advisory lock and assigns the next monotonic instance
        // number (e.g. "Developer 3"). The pre-allocated session_id is reused
        // when start_session creates the row, so the claim and the session row
        // share an identity.
        let pre_session_id = Uuid::new_v4().to_string();
        let display_name = match state
            .db
            .claim_team_role(
                team_id,
                &role_def.display_name,
                role_def.allow_multiple,
                &pre_session_id,
            )
            .await
        {
            Ok(Some(name)) => name,
            Ok(None) => {
                tracing::info!(
                    team_id,
                    role = %role_def.display_name,
                    "Spawn rejected: role slot already claimed (singleton)"
                );
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
                    let ack = format!(
                        "[ACK:SPAWN role=\"{}\" status=rejected reason=already_exists{}]",
                        role_def.display_name, queue_suffix,
                    );
                    let _ = state
                        .containers
                        .send(
                            &lead.session_id,
                            &format!(
                                "{}\n{}\nSpawn rejected: {} is already on the team (singleton role).",
                                ack, header, role_def.display_name,
                            ),
                        )
                        .await;
                }
                return SpawnOutcome::Rejected;
            }
            Err(e) => {
                tracing::error!(team_id, role = %role_def.display_name, error = %e,
                    "Failed to claim team role slot");
                return SpawnOutcome::Failed;
            }
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
                return SpawnOutcome::Failed;
            }
        };

        // Build member system prompt
        let member_prompt = build_team_member_prompt(&role_def);

        // Determine session type
        let session_type = "team_member";

        // Start the member session, reusing the pre-allocated session_id from
        // the claim so claim_team_role.session_id matches sessions.session_id.
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
            Some(&pre_session_id),
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
                    let ack = format!(
                        "[ACK:SPAWN role=\"{}\" status=ok{}]",
                        display_name, queue_suffix
                    );
                    if let Err(e) = state.containers.send(&lead.session_id, &format!(
                    "{}\n{}\nSpawn confirmed: {} is now active and has been assigned the task. Wait for their response before sending another task.",
                    ack, header, display_name,
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

                // Note: previously called drain_team_queue here to pick up any
                // queued [TO:Role] tasks for the new member. That's now redundant
                // — when handle_team_spawn is invoked from the drain, the outer
                // drain loop continues iterating after this call returns.
                SpawnOutcome::Delivered
            }
            Err(e) => {
                tracing::error!(error = %e, role = %display_name, "Failed to spawn team member");
                // Release the claim so a retry isn't blocked by a leaked slot.
                if let Err(re) = state.db.release_team_role(team_id, &display_name).await {
                    tracing::warn!(team_id, role = %display_name, error = %re,
                        "Failed to release team role claim after spawn failure");
                }
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
                    let ack = format!(
                        "[ACK:SPAWN role=\"{}\" status=failed reason=spawn_failed{}]",
                        display_name, queue_suffix,
                    );
                    let _ = state
                        .containers
                        .send(
                            &lead.session_id,
                            &format!(
                                "{}\n{}\nSpawn failed for {}: {}",
                                ack, header, display_name, e,
                            ),
                        )
                        .await;
                }
                SpawnOutcome::Failed
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

    // Queue depth per role — helps the Lead see what's waiting before
    // emitting another [TO:Role] (or amending with [CANCEL_QUEUED:Role]).
    if let Ok(counts) = state.db.queued_counts_by_role(team_id).await
        && !counts.is_empty()
    {
        let parts: Vec<String> = counts
            .iter()
            .map(|(role, n)| format!("{}: {}", role, n))
            .collect();
        let total: i64 = counts.iter().map(|(_, n)| *n).sum();
        table.push_str(&format!(
            "\n**Queued tasks ({} total):** {}\n",
            total,
            parts.join(", ")
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

/// Notify the Team Lead that a member lost context and needs task re-assignment.
/// Called on CSM restart or when a session reconnects via cold start.
async fn notify_context_lost(
    state: &AppState,
    team_id: &str,
    _session_id: &str,
    role: &str,
    previous_task: &str,
) {
    if let Ok(leads) = state.db.get_sessions_by_role(team_id, "Team Lead").await
        && let Some(lead) = leads.first()
    {
        let header = build_team_context_header(&state.db, team_id, "Team Lead").await;
        let alert_msg = format!(
            "{}\n:recycle: **{} lost context** (session reconnected on new container).\n\
            **Previous task**: {}\n\n\
            This member is now idle and needs a new task assignment. \
            Their previous work-in-progress may be partially committed.\n\
            - Check their branch for any uncommitted work: use [TO:{}] to ask them to report status\n\
            - Re-assign the task via [TO:{}] if it still needs to be done\n\
            - Use [TEAM_STATUS] to see the full roster",
            header, role, previous_task, role, role,
        );
        let _ = state.containers.send(&lead.session_id, &alert_msg).await;
        let _ = state
            .mm
            .post_in_thread(
                &lead.channel_id,
                &lead.thread_id,
                &format!(
                    ":recycle: **{}** lost context — was working on: {}. Now idle, needs re-assignment.",
                    role, truncate_preview(previous_task, 200),
                ),
            )
            .await;
    }
    post_to_coordination_log(
        state,
        team_id,
        &format!(
            ":recycle: **{}** lost context (reconnect). Previous task: {}",
            role, truncate_preview(previous_task, 200),
        ),
    )
    .await;
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
