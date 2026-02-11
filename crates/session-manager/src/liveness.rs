use dashmap::DashMap;
use std::time::Instant;

/// Tracks output activity for each session to detect unresponsive sessions.
pub struct LivenessState {
    entries: DashMap<String, LivenessEntry>,
}

struct LivenessEntry {
    last_output_at: Instant,
    last_event_type: String,
    warning_posted: bool,
    channel_id: String,
    thread_id: String,
}

/// Info returned by `get_stale` for sessions needing a warning.
pub struct StaleSession {
    pub session_id: String,
    pub channel_id: String,
    pub thread_id: String,
    pub idle_duration: std::time::Duration,
}

/// Info returned by `get_info` for the context/status command.
pub struct LivenessInfo {
    pub idle_duration: std::time::Duration,
    pub last_event_type: String,
    pub warning_posted: bool,
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
    pub fn register(&self, session_id: &str, channel_id: &str, thread_id: &str) {
        self.entries.insert(
            session_id.to_string(),
            LivenessEntry {
                last_output_at: Instant::now(),
                last_event_type: "registered".to_string(),
                warning_posted: false,
                channel_id: channel_id.to_string(),
                thread_id: thread_id.to_string(),
            },
        );
    }

    /// Update activity timestamp and event type. Called on every output event.
    pub fn update_activity(&self, session_id: &str, event_type: &str) {
        if let Some(mut entry) = self.entries.get_mut(session_id) {
            entry.last_output_at = Instant::now();
            entry.last_event_type = event_type.to_string();
            entry.warning_posted = false;
        }
    }

    /// Remove a session from liveness tracking. Called on session cleanup.
    pub fn remove(&self, session_id: &str) {
        self.entries.remove(session_id);
    }

    /// Return sessions that have been idle longer than `timeout_secs` and
    /// have not yet had a warning posted.
    pub fn get_stale(&self, timeout_secs: u64) -> Vec<StaleSession> {
        let timeout = std::time::Duration::from_secs(timeout_secs);
        let now = Instant::now();
        let mut stale = Vec::new();

        for entry in self.entries.iter() {
            let idle = now.duration_since(entry.last_output_at);
            if idle >= timeout && !entry.warning_posted {
                stale.push(StaleSession {
                    session_id: entry.key().clone(),
                    channel_id: entry.channel_id.clone(),
                    thread_id: entry.thread_id.clone(),
                    idle_duration: idle,
                });
            }
        }

        stale
    }

    /// Mark that a liveness warning has been posted for this session.
    pub fn mark_warned(&self, session_id: &str) {
        if let Some(mut entry) = self.entries.get_mut(session_id) {
            entry.warning_posted = true;
        }
    }

    /// Get liveness info for a session (used by the context/status command).
    pub fn get_info(&self, session_id: &str) -> Option<LivenessInfo> {
        self.entries.get(session_id).map(|entry| LivenessInfo {
            idle_duration: Instant::now().duration_since(entry.last_output_at),
            last_event_type: entry.last_event_type.clone(),
            warning_posted: entry.warning_posted,
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
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn register_and_get_info() {
        let state = LivenessState::new();
        state.register("s1", "ch1", "th1");

        let info = state.get_info("s1").expect("should have info");
        assert_eq!(info.last_event_type, "registered");
        assert!(!info.warning_posted);
        // idle_duration should be very small (just registered)
        assert!(info.idle_duration < Duration::from_secs(1));
    }

    #[test]
    fn update_activity_resets_warning() {
        let state = LivenessState::new();
        state.register("s1", "ch1", "th1");
        state.mark_warned("s1");

        let info = state.get_info("s1").unwrap();
        assert!(info.warning_posted);

        state.update_activity("s1", "TextLine");

        let info = state.get_info("s1").unwrap();
        assert!(!info.warning_posted);
        assert_eq!(info.last_event_type, "TextLine");
    }

    #[test]
    fn get_stale_returns_idle_sessions() {
        let state = LivenessState::new();
        state.register("s1", "ch1", "th1");

        // With a 0-second timeout, everything is stale immediately
        // (since some time passes between register and get_stale)
        sleep(Duration::from_millis(10));
        let stale = state.get_stale(0);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].session_id, "s1");
        assert_eq!(stale[0].channel_id, "ch1");
        assert_eq!(stale[0].thread_id, "th1");
    }

    #[test]
    fn get_stale_skips_recently_active() {
        let state = LivenessState::new();
        state.register("s1", "ch1", "th1");

        // With a very high timeout, nothing should be stale
        let stale = state.get_stale(9999);
        assert!(stale.is_empty());
    }

    #[test]
    fn get_stale_skips_warned_sessions() {
        let state = LivenessState::new();
        state.register("s1", "ch1", "th1");
        state.mark_warned("s1");

        sleep(Duration::from_millis(10));
        let stale = state.get_stale(0);
        assert!(stale.is_empty());
    }

    #[test]
    fn remove_session() {
        let state = LivenessState::new();
        state.register("s1", "ch1", "th1");
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
        // Should not panic
        state.update_activity("nope", "TextLine");
    }

    #[test]
    fn mark_warned_nonexistent_is_noop() {
        let state = LivenessState::new();
        // Should not panic
        state.mark_warned("nope");
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

    #[test]
    fn multiple_sessions_stale() {
        let state = LivenessState::new();
        state.register("s1", "ch1", "th1");
        state.register("s2", "ch2", "th2");
        state.register("s3", "ch3", "th3");

        // Mark s2 as warned
        state.mark_warned("s2");

        sleep(Duration::from_millis(10));
        let stale = state.get_stale(0);
        // s2 is warned, so only s1 and s3 should be stale
        assert_eq!(stale.len(), 2);
        let ids: Vec<&str> = stale.iter().map(|s| s.session_id.as_str()).collect();
        assert!(ids.contains(&"s1"));
        assert!(ids.contains(&"s3"));
    }
}
