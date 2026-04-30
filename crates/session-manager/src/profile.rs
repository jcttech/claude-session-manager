//! Active Claude OAuth profile (CLAUDE_CONFIG_DIR override).
//!
//! Mirrors the membank `claude_profile_state` design: a single in-process
//! handle is read at every CreateSession spawn and atomically swapped when
//! POST /admin/profile arrives. DB is the source of truth on restart; the
//! handle is the hot-path cache.

use std::sync::Arc;
use tokio::sync::RwLock;

/// Currently-active CLAUDE_CONFIG_DIR override. `None` = no override.
/// Read by `container::message_processor`'s env builder on every send;
/// written by the admin POST handler.
pub type ActiveConfigDir = Arc<RwLock<Option<String>>>;

/// Convert the DB string ("" = no override) into the in-process Option form.
pub fn from_db_value(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}
