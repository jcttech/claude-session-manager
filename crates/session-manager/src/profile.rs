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

/// Expand `~`, `~/...`, `$HOME`, and `${HOME}` in an incoming
/// `claude_config_dir` so callers can POST shell-style paths instead of
/// having to know the absolute home dir of the session-manager process.
/// Falls back to leaving the literal in place when `HOME` is unset, so we
/// never paper over a misconfigured environment.
pub fn resolve_config_dir(input: &str) -> String {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty());
    resolve_with_home(input, home.as_deref())
}

fn resolve_with_home(input: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return input.to_string();
    };

    let tilde_expanded = if input == "~" {
        home.to_string()
    } else if let Some(rest) = input.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        input.to_string()
    };

    tilde_expanded
        .replace("${HOME}", home)
        .replace("$HOME", home)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/alice";

    #[test]
    fn expands_bare_tilde() {
        assert_eq!(resolve_with_home("~", Some(HOME)), "/home/alice");
    }

    #[test]
    fn expands_tilde_prefix() {
        assert_eq!(
            resolve_with_home("~/.claude", Some(HOME)),
            "/home/alice/.claude"
        );
    }

    #[test]
    fn expands_dollar_home_forms() {
        assert_eq!(
            resolve_with_home("$HOME/.claude", Some(HOME)),
            "/home/alice/.claude"
        );
        assert_eq!(
            resolve_with_home("${HOME}/.claude", Some(HOME)),
            "/home/alice/.claude"
        );
    }

    #[test]
    fn leaves_absolute_path_alone() {
        assert_eq!(resolve_with_home("/etc/claude", Some(HOME)), "/etc/claude");
    }

    #[test]
    fn does_not_expand_mid_path_tilde() {
        // Only a leading `~` (or `~/`) is expanded; embedded tildes are literal.
        assert_eq!(
            resolve_with_home("/foo/~/bar", Some(HOME)),
            "/foo/~/bar"
        );
    }

    #[test]
    fn no_home_returns_input_unchanged() {
        assert_eq!(resolve_with_home("~/.claude", None), "~/.claude");
        assert_eq!(resolve_with_home("$HOME/.claude", None), "$HOME/.claude");
    }
}
