//! Active Claude OAuth profile selection.
//!
//! The `claude_profiles` table holds the catalog (name → claude_config_dir)
//! with exactly one row marked active at a time. The DB is the source of
//! truth on restart; an in-process `ActiveProfile` handle is the hot-path
//! cache, swapped atomically by the admin POST handler.

use std::sync::Arc;
use tokio::sync::RwLock;

/// Snapshot of the currently-active profile, read by the env builder on every
/// CreateSession spawn and replaced by POST /admin/profile.
///
/// `claude_config_dir == None` means the active profile is the no-override
/// sentinel (e.g. `default` with empty path) — the env builder injects no
/// `CLAUDE_CONFIG_DIR` and the CLI falls back to `~/.claude` or inherited
/// env auth.
#[derive(Debug, Clone)]
pub struct ActiveProfileSnapshot {
    pub name: String,
    pub claude_config_dir: Option<String>,
}

/// In-process handle to the active profile. `Arc<RwLock<...>>` so the env
/// builder can read on every spawn without copying and the admin handler
/// can swap atomically.
pub type ActiveProfile = Arc<RwLock<ActiveProfileSnapshot>>;

/// Build a snapshot from a DB row. The "" sentinel for `claude_config_dir`
/// becomes `None` so the env builder treats it as "no override".
pub fn snapshot_from_db(name: &str, claude_config_dir: &str) -> ActiveProfileSnapshot {
    ActiveProfileSnapshot {
        name: name.to_string(),
        claude_config_dir: if claude_config_dir.is_empty() {
            None
        } else {
            Some(claude_config_dir.to_string())
        },
    }
}

/// Expand `~`, `~/...`, `$HOME`, and `${HOME}` in an incoming
/// `claude_config_dir` so callers can POST or seed shell-style paths instead
/// of having to know the absolute home dir of the session-manager process.
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

/// Parse the `CSM_PROFILES` env var into `(name, claude_config_dir)` pairs.
/// Format: `name1:path1,name2:path2`. Whitespace around names/paths is
/// trimmed; entries with empty names are skipped (silently — the operator
/// pasted a stray comma).
///
/// The `default` profile is reserved: it's seeded by the migration with an
/// empty path, and `CSM_PROFILES` cannot redefine it (an entry named
/// `default` is dropped with a warning logged by the caller).
pub fn parse_profiles_env(input: &str) -> Vec<(String, String)> {
    input
        .split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let (name, path) = entry.split_once(':')?;
            let name = name.trim();
            let path = path.trim();
            if name.is_empty() {
                return None;
            }
            Some((name.to_string(), path.to_string()))
        })
        .collect()
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
        assert_eq!(resolve_with_home("/foo/~/bar", Some(HOME)), "/foo/~/bar");
    }

    #[test]
    fn no_home_returns_input_unchanged() {
        assert_eq!(resolve_with_home("~/.claude", None), "~/.claude");
        assert_eq!(resolve_with_home("$HOME/.claude", None), "$HOME/.claude");
    }

    #[test]
    fn snapshot_empty_path_is_none() {
        let s = snapshot_from_db("default", "");
        assert_eq!(s.name, "default");
        assert_eq!(s.claude_config_dir, None);
    }

    #[test]
    fn snapshot_nonempty_path_is_some() {
        let s = snapshot_from_db("alpha", "/home/alice/.claude2");
        assert_eq!(s.name, "alpha");
        assert_eq!(s.claude_config_dir, Some("/home/alice/.claude2".into()));
    }

    #[test]
    fn parses_simple_profiles() {
        let v = parse_profiles_env("personal:/home/x/.claude,team:/home/x/.claude2");
        assert_eq!(
            v,
            vec![
                ("personal".to_string(), "/home/x/.claude".to_string()),
                ("team".to_string(), "/home/x/.claude2".to_string()),
            ]
        );
    }

    #[test]
    fn trims_whitespace_around_entries() {
        let v = parse_profiles_env(" personal : /home/x/.claude , team : /home/x/.claude2 ");
        assert_eq!(
            v,
            vec![
                ("personal".to_string(), "/home/x/.claude".to_string()),
                ("team".to_string(), "/home/x/.claude2".to_string()),
            ]
        );
    }

    #[test]
    fn skips_empty_entries() {
        let v = parse_profiles_env(",,personal:/p,,team:/t,");
        assert_eq!(
            v,
            vec![
                ("personal".to_string(), "/p".to_string()),
                ("team".to_string(), "/t".to_string()),
            ]
        );
    }

    #[test]
    fn skips_entries_without_colon() {
        let v = parse_profiles_env("good:/p,no-colon-here,also:/q");
        assert_eq!(
            v,
            vec![
                ("good".to_string(), "/p".to_string()),
                ("also".to_string(), "/q".to_string()),
            ]
        );
    }

    #[test]
    fn empty_input_is_empty() {
        assert!(parse_profiles_env("").is_empty());
        assert!(parse_profiles_env("   ").is_empty());
    }
}
