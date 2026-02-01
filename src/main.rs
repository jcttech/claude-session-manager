mod config;
mod container;
mod crypto;
mod database;
mod mattermost;
mod opnsense;
mod rate_limit;

use anyhow::Result;
use axum::{
    extract::State,
    routing::post,
    Json, Router,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::container::ContainerManager;
use crate::crypto::{sign_request, verify_signature};
use crate::database::Database;
use crate::mattermost::{Mattermost, Post};
use crate::opnsense::OPNsense;
use crate::rate_limit::RateLimitLayer;

struct AppState {
    mm: Mattermost,
    containers: ContainerManager,
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

    let mm = Mattermost::new().await?;
    let opnsense = OPNsense::new()?;
    let containers = ContainerManager::new();
    let db = Database::new().await?;

    // Recover sessions from database on startup
    let recovered = recover_sessions(&db, &containers).await;
    tracing::info!("Recovered {} sessions from database", recovered);

    let state = Arc::new(AppState {
        mm,
        containers,
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
        .layer(rate_limiter)
        .with_state(state);

    let listen_addr = &config::settings().listen_addr;
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    tracing::info!("Listening on {}", listen_addr);
    axum::serve(listener, app).await?;

    Ok(())
}

/// Recover active sessions from the database on startup
async fn recover_sessions(db: &Database, _containers: &ContainerManager) -> usize {
    match db.get_all_sessions().await {
        Ok(sessions) => {
            // Note: We log recovered sessions but don't reconnect to running containers
            // as this would require complex state reconciliation. The sessions table
            // is primarily for audit/reference. Active session state is rebuilt on restart.
            tracing::info!("Found {} sessions in database (not reconnecting)", sessions.len());
            sessions.len()
        }
        Err(e) => {
            tracing::warn!("Failed to recover sessions: {}", e);
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
            if let Some(project) = text.strip_prefix("start ").map(|s| s.trim()) {
                let s = config::settings();
                let Some(project_path) = s.projects.get(project) else {
                    let _ = state.mm.post(channel_id, &format!(
                        "Unknown project. Available: {}",
                        s.projects.keys().cloned().collect::<Vec<_>>().join(", ")
                    )).await;
                    continue;
                };

                let session_id = Uuid::new_v4().to_string();
                let _ = state.mm.post(channel_id, &format!("Starting **{}**...", project)).await;

                let (output_tx, output_rx) = mpsc::channel::<String>(100);
                match state.containers.start(&session_id, project_path, output_tx).await {
                    Ok(name) => {
                        // Persist session to database
                        if let Err(e) = state.db.create_session(&session_id, channel_id, project, &name).await {
                            tracing::error!("Failed to persist session: {}", e);
                        }

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
                        let _ = state.mm.post(channel_id, &format!("Failed to start: {}", e)).await;
                    }
                }
                continue;
            }

            // Stop session
            if text == "stop" {
                if let Ok(Some(session)) = state.db.get_session_by_channel(channel_id).await {
                    let _ = state.containers.stop(&session.session_id).await;
                    let _ = state.db.delete_session(&session.session_id).await;
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
    let _ = state.db.delete_session(&session_id).await;
    let _ = state.mm.post(&channel_id, "Session ended.").await;
}

async fn handle_network_request(state: &AppState, channel_id: &str, session_id: &str, domain: &str) {
    let request_id = Uuid::new_v4().to_string();
    let s = config::settings();

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
        tracing::info!("Domain {} approved by {}", req.domain, payload.user_name);
    } else {
        let _ = state.mm.update_post(&req.post_id, &format!("`{}` denied by @{}", req.domain, payload.user_name)).await;
        let _ = state.containers.send(&req.session_id, &format!("[NETWORK_DENIED: {}]", req.domain)).await;
        tracing::info!("Domain {} denied by {}", req.domain, payload.user_name);
    }

    Json(CallbackResponse {
        ephemeral_text: None,
        update: Some(serde_json::json!({ "message": "" })),
    })
}
