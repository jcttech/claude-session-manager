use dashmap::DashMap;
use std::time::Instant;

/// Tracks output activity for each session (diagnostics only, no warnings).
pub struct LivenessState {
    entries: DashMap<String, LivenessEntry>,
}

struct LivenessEntry {
    last_output_at: Instant,
    last_event_type: String,
}

/// Info returned by `get_info` for the context/status command.
pub struct LivenessInfo {
    pub idle_duration: std::time::Duration,
    pub last_event_type: String,
}

impl Default for LivenessState {
    fn default() -> Self {
        Self::new()
    }
}

impl LivenessState {
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    /// Register a session for liveness tracking. Called when a session starts.
    pub fn register(&self, session_id: &str) {
        self.entries.insert(
            session_id.to_string(),
            LivenessEntry {
                last_output_at: Instant::now(),
                last_event_type: "registered".to_string(),
            },
        );
    }

    /// Update activity timestamp and event type. Called on every output event.
    pub fn update_activity(&self, session_id: &str, event_type: &str) {
        if let Some(mut entry) = self.entries.get_mut(session_id) {
            entry.last_output_at = Instant::now();
            entry.last_event_type = event_type.to_string();
        }
    }

    /// Remove a session from liveness tracking. Called on session cleanup.
    pub fn remove(&self, session_id: &str) {
        self.entries.remove(session_id);
    }

    /// Get liveness info for a session (used by the context/status command).
    pub fn get_info(&self, session_id: &str) -> Option<LivenessInfo> {
        self.entries.get(session_id).map(|entry| LivenessInfo {
            idle_duration: Instant::now().duration_since(entry.last_output_at),
            last_event_type: entry.last_event_type.clone(),
        })
    }
}

/// Format a Duration as a short human-readable string (e.g. "5s", "2m 30s", "1h 5m").
pub fn format_duration_short(d: std::time::Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else if mins > 0 {
        format!("{}m {}s", mins, secs)
    } else {
        format!("{}s", secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn register_and_get_info() {
        let state = LivenessState::new();
        state.register("s1");

        let info = state.get_info("s1").expect("should have info");
        assert_eq!(info.last_event_type, "registered");
        assert!(info.idle_duration < Duration::from_secs(1));
    }

    #[test]
    fn update_activity() {
        let state = LivenessState::new();
        state.register("s1");

        state.update_activity("s1", "TextLine");

        let info = state.get_info("s1").unwrap();
        assert_eq!(info.last_event_type, "TextLine");
    }

    #[test]
    fn remove_session() {
        let state = LivenessState::new();
        state.register("s1");
        assert!(state.get_info("s1").is_some());

        state.remove("s1");
        assert!(state.get_info("s1").is_none());
    }

    #[test]
    fn get_info_nonexistent() {
        let state = LivenessState::new();
        assert!(state.get_info("nope").is_none());
    }

    #[test]
    fn update_activity_nonexistent_is_noop() {
        let state = LivenessState::new();
        state.update_activity("nope", "TextLine");
    }

    #[test]
    fn format_duration_short_seconds() {
        assert_eq!(format_duration_short(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration_short(Duration::from_secs(0)), "0s");
    }

    #[test]
    fn format_duration_short_minutes() {
        assert_eq!(format_duration_short(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration_short(Duration::from_secs(60)), "1m 0s");
    }

    #[test]
    fn format_duration_short_hours() {
        assert_eq!(format_duration_short(Duration::from_secs(3665)), "1h 1m");
    }
}
