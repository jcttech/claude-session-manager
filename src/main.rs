mod config;
mod container;
mod crypto;
mod database;
mod git;
mod mattermost;
mod opnsense;
mod rate_limit;
mod ssh;

use anyhow::Result;
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use metrics::{counter, gauge};
use metrics_exporter_prometheus::PrometheusBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::container::ContainerManager;
use crate::crypto::{sign_request, verify_signature};
use crate::database::Database;
use crate::git::{GitManager, RepoRef};
use crate::mattermost::{Mattermost, Post};
use crate::opnsense::OPNsense;
use crate::rate_limit::RateLimitLayer;

struct AppState {
    mm: Mattermost,
    containers: ContainerManager,
    git: GitManager,
    opnsense: OPNsense,
    db: Database,
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
    tracing_subscriber::fmt::init();

    // Initialize Prometheus metrics recorder
    let prometheus_handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install Prometheus recorder");

    // Initialize SSH key (writes to temp file if SM_VM_SSH_KEY is set)
    ssh::init_ssh_key()?;

    let mm = Mattermost::new().await?;
    let opnsense = OPNsense::new()?;
    let containers = ContainerManager::new();
    let git = GitManager::new();
    let db = Database::new().await?;

    // Recover sessions from database on startup
    let recovered = recover_sessions(&db, &containers).await;
    tracing::info!("Recovered {} sessions from database", recovered);

    let state = Arc::new(AppState {
        mm,
        containers,
        git,
        opnsense,
        db,
    });

    // Start message listener
    let (post_tx, post_rx) = mpsc::channel::<Post>(100);
    let state_clone = state.clone();
    tokio::spawn(async move {
        let _ = state_clone.mm.listen(post_tx).await;
    });

    // Start message handler
    let state_clone = state.clone();
    tokio::spawn(async move {
        handle_messages(state_clone, post_rx).await;
    });

    // Start periodic cleanup of stale pending requests
    let state_clone = state.clone();
    tokio::spawn(async move {
        cleanup_stale_requests(state_clone).await;
    });

    // Configure rate limiting
    let s = config::settings();
    let rate_limiter = RateLimitLayer::new(s.rate_limit_rps, s.rate_limit_burst);

    // Spawn cleanup task for rate limiter
    rate_limit::spawn_cleanup_task(rate_limiter.clone());

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
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

/// Health check endpoint for Kubernetes probes
async fn health_check() -> &'static str {
    "OK"
}

/// Graceful shutdown signal handler
async fn shutdown_signal() {
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
    }

    tracing::info!("Shutdown signal received, starting graceful shutdown");
}

/// Recover active sessions from the database on startup
/// Checks if containers are still running and cleans up stale sessions
async fn recover_sessions(db: &Database, containers: &ContainerManager) -> usize {
    match db.get_all_sessions().await {
        Ok(sessions) => {
            let mut cleaned = 0;

            for session in &sessions {
                // Check if container still exists
                if containers.is_container_running(&session.container_name).await {
                    // Container is running but we can't reconnect to it
                    // (would need complex stdin/stdout reconnection)
                    // So we stop the orphaned container and clean up
                    tracing::info!(
                        session_id = %session.session_id,
                        container = %session.container_name,
                        "Stopping orphaned container from previous run"
                    );
                    let _ = containers.remove_container(&session.container_name).await;
                    let _ = db.delete_session(&session.session_id).await;
                    cleaned += 1;
                } else {
                    // Container not running, just clean up the database entry
                    tracing::debug!(
                        session_id = %session.session_id,
                        container = %session.container_name,
                        "Cleaning up stale session record"
                    );
                    let _ = db.delete_session(&session.session_id).await;
                    cleaned += 1;
                }
            }

            if cleaned > 0 {
                tracing::info!(
                    cleaned = cleaned,
                    "Cleaned up stale sessions from previous run"
                );
            }

            0 // No sessions were actually recovered (we clean up, not reconnect)
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to recover sessions");
            0
        }
    }
}

/// Periodically clean up stale pending requests
async fn cleanup_stale_requests(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600)); // Every hour
    loop {
        interval.tick().await;
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
}

async fn handle_messages(state: Arc<AppState>, mut rx: mpsc::Receiver<Post>) {
    let network_re = Regex::new(r"\[NETWORK_REQUEST:\s*([^\]]+)\]").unwrap();
    let bot_trigger = &config::settings().bot_trigger;

    while let Some(post) = rx.recv().await {
        let text = post.message.trim();
        let channel_id = &post.channel_id;

        if text.starts_with(bot_trigger) {
            let text = text.trim_start_matches(bot_trigger).trim();

            // Start session
            if let Some(project_input) = text.strip_prefix("start ").map(|s| s.trim()) {
                use std::path::PathBuf;
                let session_id = Uuid::new_v4().to_string();

                // Track worktree path if created (for reference, not auto-cleanup)
                let mut worktree_path: Option<PathBuf> = None;

                // Try to parse as GitHub repo first (org/repo format)
                let project_path = if RepoRef::looks_like_repo(project_input) {
                    match RepoRef::parse(project_input) {
                        Some(repo_ref) => {
                            // Check if using worktree mode
                            if repo_ref.worktree.is_some() {
                                // Create worktree for isolation
                                let _ = state.mm.post(channel_id, &format!(
                                    "Creating worktree for **{}**...",
                                    repo_ref.full_name()
                                )).await;

                                match state.git.create_worktree(&repo_ref, &session_id).await {
                                    Ok(path) => {
                                        worktree_path = Some(path.clone());
                                        path.to_string_lossy().to_string()
                                    }
                                    Err(e) => {
                                        let _ = state.mm.post(channel_id, &format!(
                                            "Failed to create worktree: {}",
                                            e
                                        )).await;
                                        continue;
                                    }
                                }
                            } else {
                                // Using main clone - atomically try to acquire
                                if let Err(existing_session) = state.git.try_acquire_repo(&repo_ref, &session_id) {
                                    let _ = state.mm.post(channel_id, &format!(
                                        "Repository **{}** is already in use by session `{}`.\n\
                                        Use `--worktree` for an isolated working directory:\n\
                                        `@claude start {} --worktree`",
                                        repo_ref.full_name(),
                                        &existing_session[..8.min(existing_session.len())],
                                        project_input
                                    )).await;
                                    continue;
                                }

                                // Ensure repo is cloned
                                let _ = state.mm.post(channel_id, &format!(
                                    "Preparing **{}**...",
                                    repo_ref.full_name()
                                )).await;

                                match state.git.ensure_repo(&repo_ref).await {
                                    Ok(path) => {
                                        path.to_string_lossy().to_string()
                                    }
                                    Err(e) => {
                                        // Release the repo since we failed
                                        state.git.release_repo_by_session(&session_id);
                                        let _ = state.mm.post(channel_id, &format!(
                                            "Failed to prepare repository: {}",
                                            e
                                        )).await;
                                        continue;
                                    }
                                }
                            }
                        }
                        None => {
                            let _ = state.mm.post(channel_id,
                                "Invalid repository format. Use: `org/repo`, `org/repo@branch`, or `org/repo --worktree`"
                            ).await;
                            continue;
                        }
                    }
                } else {
                    // Fall back to static projects mapping
                    let s = config::settings();
                    match s.projects.get(project_input) {
                        Some(path) => path.clone(),
                        None => {
                            let available = if s.projects.is_empty() {
                                "none configured".to_string()
                            } else {
                                s.projects.keys().cloned().collect::<Vec<_>>().join(", ")
                            };
                            let _ = state.mm.post(channel_id, &format!(
                                "Unknown project `{}`. Available: {}\n\n\
                                Or use GitHub repo format: `org/repo`",
                                project_input,
                                available
                            )).await;
                            continue;
                        }
                    }
                };

                let _ = state.mm.post(channel_id, "Starting session...").await;

                let (output_tx, output_rx) = mpsc::channel::<String>(100);
                match state.containers.start(&session_id, &project_path, output_tx).await {
                    Ok(name) => {
                        // Track metrics
                        counter!("sessions_started_total").increment(1);
                        gauge!("active_sessions").increment(1.0);

                        // Track worktree path in session (for reference, not auto-cleanup)
                        if let Some(wt_path) = worktree_path {
                            state.containers.set_worktree_path(&session_id, wt_path);
                        }

                        // Persist session to database
                        if let Err(e) = state.db.create_session(&session_id, channel_id, project_input, &name).await {
                            tracing::error!(
                                session_id = %session_id,
                                error = %e,
                                "Failed to persist session"
                            );
                        }

                        tracing::info!(
                            session_id = %session_id,
                            container = %name,
                            project = %project_input,
                            "Session started"
                        );

                        let _ = state.mm.post(channel_id, &format!("Ready. Container: `{}`", name)).await;

                        // Start output streaming
                        let state_clone = state.clone();
                        let channel_id_clone = channel_id.clone();
                        let session_id_clone = session_id.clone();
                        let network_re_clone = network_re.clone();
                        tokio::spawn(async move {
                            stream_output(state_clone, channel_id_clone, session_id_clone, output_rx, network_re_clone).await;
                        });
                    }
                    Err(e) => {
                        // Release repo if we marked it in use
                        state.git.release_repo_by_session(&session_id);
                        let _ = state.mm.post(channel_id, &format!("Failed to start: {}", e)).await;
                    }
                }
                continue;
            }

            // Stop session
            if text == "stop" {
                if let Ok(Some(session)) = state.db.get_session_by_channel(channel_id).await {
                    let _ = state.containers.stop(&session.session_id).await;
                    // Release repo tracking (if using main clone)
                    state.git.release_repo_by_session(&session.session_id);
                    let _ = state.db.delete_session(&session.session_id).await;
                    gauge!("active_sessions").decrement(1.0);
                    tracing::info!(
                        session_id = %session.session_id,
                        "Session stopped by user"
                    );
                }
                let _ = state.mm.post(channel_id, "Stopped.").await;
                continue;
            }
        }

        // Forward to Claude
        if let Ok(Some(session)) = state.db.get_session_by_channel(channel_id).await {
            let _ = state.containers.send(&session.session_id, text).await;
        }
    }
}

async fn stream_output(
    state: Arc<AppState>,
    channel_id: String,
    session_id: String,
    mut rx: mpsc::Receiver<String>,
    network_re: Regex,
) {
    while let Some(line) = rx.recv().await {
        if let Some(caps) = network_re.captures(&line) {
            let domain = caps[1].trim();
            handle_network_request(&state, &channel_id, &session_id, domain).await;
        } else {
            let _ = state.mm.post(&channel_id, &format!("```\n{}\n```", line)).await;
        }
    }

    // Clean up session from database when stream ends
    // Also release repo tracking (if using main clone)
    state.git.release_repo_by_session(&session_id);
    let _ = state.db.delete_session(&session_id).await;
    gauge!("active_sessions").decrement(1.0);
    tracing::info!(
        session_id = %session_id,
        channel_id = %channel_id,
        "Session stream ended"
    );
    let _ = state.mm.post(&channel_id, "Session ended.").await;
}

async fn handle_network_request(state: &AppState, channel_id: &str, session_id: &str, domain: &str) {
    // Check if there's already a pending request for this domain in this session
    // This prevents duplicate approval cards for the same domain
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
        Ok(None) => {
            // No existing request, proceed normally
        }
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

    // Generate HMAC signatures for both approve and deny actions
    let approve_sig = sign_request(&s.callback_secret, &request_id, "approve");
    let deny_sig = sign_request(&s.callback_secret, &request_id, "deny");

    match state.mm.post_approval(channel_id, &request_id, domain, &approve_sig, &deny_sig).await {
        Ok(post_id) => {
            // Persist pending request to database
            if let Err(e) = state.db.create_pending_request(
                &request_id,
                channel_id,
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
}

async fn handle_callback(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CallbackPayload>,
) -> Json<CallbackResponse> {
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

    // Verify HMAC signature to ensure request came from our approval buttons
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

    // Retrieve and remove pending request from database
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

    // Delete the pending request
    if let Err(e) = state.db.delete_pending_request(request_id).await {
        tracing::error!("Failed to delete pending request: {}", e);
    }

    // Log the action for audit
    if let Err(e) = state.db.log_approval(request_id, &req.domain, action, &payload.user_name).await {
        tracing::error!("Failed to log approval: {}", e);
    }

    if action == "approve" {
        let _ = state.opnsense.add_domain(&req.domain).await;
        let _ = state.mm.update_post(&req.post_id, &format!("`{}` approved by @{}", req.domain, payload.user_name)).await;
        let _ = state.containers.send(&req.session_id, &format!("[NETWORK_APPROVED: {}]", req.domain)).await;
        counter!("approvals_total", "action" => "approve").increment(1);
        tracing::info!(
            request_id = %request_id,
            domain = %req.domain,
            user = %payload.user_name,
            session_id = %req.session_id,
            "Domain approved"
        );
    } else {
        let _ = state.mm.update_post(&req.post_id, &format!("`{}` denied by @{}", req.domain, payload.user_name)).await;
        let _ = state.containers.send(&req.session_id, &format!("[NETWORK_DENIED: {}]", req.domain)).await;
        counter!("approvals_total", "action" => "deny").increment(1);
        tracing::info!(
            request_id = %request_id,
            domain = %req.domain,
            user = %payload.user_name,
            session_id = %req.session_id,
            "Domain denied"
        );
    }

    Json(CallbackResponse {
        ephemeral_text: None,
        update: Some(serde_json::json!({ "message": "" })),
    })
}
