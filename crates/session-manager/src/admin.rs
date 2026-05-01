//! Admin HTTP endpoints. `/admin/profile` is the external surface for the
//! profile-manager (claude-usage-monitor) to flip the active Claude OAuth
//! profile by logical name.
//!
//! Auth: bearer token from `ADMIN_BEARER_TOKEN`. Refused entirely (503) if
//! the env var is unset, so the route is never accidentally open.
//!
//! GET returns the active profile plus the full registered catalog so ops
//! can see what's available. POST accepts a `profile_name` and atomically
//! flips activation. Pre-population of the catalog is via the
//! `CSM_PROFILES` env var at session-manager startup.

use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::database::ClaudeProfile;

#[derive(Serialize)]
pub struct ProfileEntry {
    pub profile_name: String,
    /// `None` when the row's stored path is the empty-string sentinel (e.g.
    /// the always-present `default` row). Surfaced as JSON `null` so callers
    /// can distinguish "no override" from a literal empty path.
    pub claude_config_dir: Option<String>,
    pub is_active: bool,
    pub event_id: Option<uuid::Uuid>,
    pub swapped_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub updated_by: Option<String>,
}

impl From<ClaudeProfile> for ProfileEntry {
    fn from(p: ClaudeProfile) -> Self {
        Self {
            profile_name: p.profile_name,
            claude_config_dir: if p.claude_config_dir.is_empty() {
                None
            } else {
                Some(p.claude_config_dir)
            },
            is_active: p.is_active,
            event_id: p.event_id,
            swapped_at: p.swapped_at,
            updated_at: p.updated_at,
            updated_by: p.updated_by,
        }
    }
}

#[derive(Serialize)]
pub struct GetProfileResponse {
    pub active: ProfileEntry,
    pub profiles: Vec<ProfileEntry>,
}

#[derive(Deserialize)]
pub struct SetProfileRequest {
    /// Name of the profile to activate. Must already exist in the catalog
    /// (seeded via `CSM_PROFILES` or the always-present `default`).
    pub profile_name: String,
    /// Audit pass-through, echoed back on the activated row.
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
) -> Result<Json<GetProfileResponse>, (StatusCode, &'static str)> {
    let active = state
        .db
        .get_active_claude_profile()
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "db_error"))?;
    let profiles = state
        .db
        .list_claude_profiles()
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "db_error"))?;
    Ok(Json(GetProfileResponse {
        active: active.into(),
        profiles: profiles.into_iter().map(Into::into).collect(),
    }))
}

pub async fn post_profile(
    State(state): State<Arc<crate::AppState>>,
    Json(req): Json<SetProfileRequest>,
) -> Result<Json<ProfileEntry>, (StatusCode, &'static str)> {
    if req.profile_name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "MISSING_PROFILE_NAME"));
    }

    // 1. Atomically flip is_active in DB. 404 if the named profile is not
    //    in the catalog — pre-population happens at startup via CSM_PROFILES,
    //    so a missing name is an operator misconfiguration, not something
    //    we silently auto-create.
    let row = state
        .db
        .set_active_claude_profile(
            &req.profile_name,
            req.event_id,
            req.swapped_at,
            req.updated_by.as_deref(),
        )
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "db_error"))?
        .ok_or((StatusCode::NOT_FOUND, "PROFILE_NOT_FOUND"))?;

    // 2. Hot-swap the in-process handle so the next CreateSession spawn
    //    sees the new value without a restart.
    let snapshot = crate::profile::snapshot_from_db(&row.profile_name, &row.claude_config_dir);
    *state.active_profile.write().await = snapshot;

    // 3. Enqueue CLEAR for every active team member so they pick up the new
    //    profile on their next turn. Drain handles the timing: idle members
    //    clear immediately, mid-turn members wait until ResponseComplete
    //    (see drain_one_clear's pending_task_from skip). Best-effort — log
    //    on failure but still return success for the swap.
    if let Err(e) = crate::rotate_active_team_sessions(&state).await {
        tracing::warn!(error = %e, "profile swap: rotation enqueue failed");
    }

    Ok(Json(row.into()))
}
