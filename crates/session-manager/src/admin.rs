//! Admin HTTP endpoints. Currently only `/admin/profile` for the external
//! profile-manager (claude-usage-monitor) to flip the active CLAUDE_CONFIG_DIR.
//!
//! Auth: bearer token from `ADMIN_BEARER_TOKEN`. Refused entirely (503) if the
//! env var is unset, so the route is never accidentally open.
//!
//! Payload shape mirrors membank's `/api/admin/profile` so the same caller
//! payload works against both services.

use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::database::ClaudeProfileState;

#[derive(Serialize)]
pub struct ProfileResponse {
    pub claude_config_dir: Option<String>,
    pub event_id: Option<uuid::Uuid>,
    pub swapped_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub updated_by: Option<String>,
}

impl From<ClaudeProfileState> for ProfileResponse {
    fn from(s: ClaudeProfileState) -> Self {
        Self {
            claude_config_dir: if s.claude_config_dir.is_empty() {
                None
            } else {
                Some(s.claude_config_dir)
            },
            event_id: s.event_id,
            swapped_at: s.swapped_at,
            updated_at: s.updated_at,
            updated_by: s.updated_by,
        }
    }
}

#[derive(Deserialize)]
pub struct SetProfileRequest {
    /// Absolute path to a Claude config dir, or null/empty to clear.
    pub claude_config_dir: Option<String>,
    /// Audit pass-through. Echoed back in the response for log correlation.
    pub event_id: Option<uuid::Uuid>,
    pub swapped_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_by: Option<String>,
}

/// Bearer-token middleware. Fail-closed if `ADMIN_BEARER_TOKEN` is unset.
pub async fn require_admin_token(
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let expected = std::env::var("ADMIN_BEARER_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let supplied = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if !constant_time_eq(supplied.as_bytes(), expected.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(request).await)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn get_profile(
    State(state): State<Arc<crate::AppState>>,
) -> Result<Json<ProfileResponse>, (StatusCode, &'static str)> {
    let row = state
        .db
        .get_claude_profile_state()
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "db_error"))?;
    Ok(Json(row.into()))
}

pub async fn post_profile(
    State(state): State<Arc<crate::AppState>>,
    Json(req): Json<SetProfileRequest>,
) -> Result<Json<ProfileResponse>, (StatusCode, &'static str)> {
    let dir = req.claude_config_dir.unwrap_or_default();

    if !dir.is_empty() {
        let creds = std::path::Path::new(&dir).join(".credentials.json");
        if !creds.is_file() {
            return Err((StatusCode::BAD_REQUEST, "INVALID_CONFIG_DIR"));
        }
    }

    // 1. Persist + atomically swap the in-process handle so the next
    //    CreateSession spawn sees the new value.
    let row = state
        .db
        .set_claude_profile_state(
            &dir,
            req.event_id,
            req.swapped_at,
            req.updated_by.as_deref(),
        )
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "db_error"))?;

    let new_value = if dir.is_empty() { None } else { Some(dir) };
    *state.active_config_dir.write().await = new_value;

    // 2. Enqueue CLEAR for every active team member so they pick up the new
    //    profile on their next turn. Drain handles the timing: idle members
    //    clear immediately, mid-turn members wait until ResponseComplete
    //    (see drain_one_clear's pending_task_from skip). Best-effort — log
    //    on failure but still return success for the swap.
    if let Err(e) = crate::rotate_active_team_sessions(&state).await {
        tracing::warn!(error = %e, "profile swap: rotation enqueue failed");
    }

    Ok(Json(row.into()))
}
