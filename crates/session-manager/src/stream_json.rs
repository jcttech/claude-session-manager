// -- OutputEvent: the typed events sent from message_processor to stream_output --

#[derive(Debug)]
pub enum OutputEvent {
    /// Claude has started processing (emitted on message_start)
    ProcessingStarted { input_tokens: u64 },
    /// A complete line of text output
    TextLine(String),
    /// A tool action status line (formatted markdown, not wrapped in code fence)
    ToolAction(String),
    /// Response is complete (emitted on message_stop when stop_reason != "tool_use")
    ResponseComplete { input_tokens: u64, output_tokens: u64 },
    /// Generated thread title (from `title` command)
    TitleGenerated(String),
    /// Process died unexpectedly (non-zero exit, not user-initiated)
    ProcessDied { exit_code: Option<i32>, signal: Option<String> },
}

/// Map common fatal exit codes to human-readable signal names.
/// Exit codes above 128 typically indicate the process was killed by a signal,
/// where the signal number is (exit_code - 128).
pub fn signal_name(code: i32) -> &'static str {
    match code {
        134 => "SIGABRT",
        137 => "SIGKILL (possibly OOM)",
        139 => "SIGSEGV (segmentation fault)",
        143 => "SIGTERM",
        _ => "unknown signal",
    }
}

/// Format a tool_use block as a concise status line for Mattermost display.
/// e.g. "**Read** `src/main.rs`", "**Bash** `cargo test`", "**Edit** `src/lib.rs`"
pub fn format_tool_action(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Read" => {
            let path = input.get("file_path").and_then(|v| v.as_str()).unwrap_or("?");
            format!("**Read** `{}`", path)
        }
        "Write" => {
            let path = input.get("file_path").and_then(|v| v.as_str()).unwrap_or("?");
            format!("**Write** `{}`", path)
        }
        "Edit" => {
            let path = input.get("file_path").and_then(|v| v.as_str()).unwrap_or("?");
            format!("**Edit** `{}`", path)
        }
        "Bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("?");
            // Truncate long commands
            let cmd_short = if cmd.len() > 80 { &cmd[..77] } else { cmd };
            let suffix = if cmd.len() > 80 { "..." } else { "" };
            format!("**Bash** `{}{}`", cmd_short, suffix)
        }
        "Glob" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            format!("**Glob** `{}`", pattern)
        }
        "Grep" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            format!("**Grep** `{}`", pattern)
        }
        "WebFetch" => {
            let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("?");
            format!("**WebFetch** `{}`", url)
        }
        "WebSearch" => {
            let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("?");
            format!("**WebSearch** `{}`", query)
        }
        "Task" => {
            let desc = input.get("description").and_then(|v| v.as_str()).unwrap_or("subagent");
            format!("**Task** _{}_", desc)
        }
        "Skill" => {
            let skill = input.get("skill").and_then(|v| v.as_str()).unwrap_or("?");
            let args = input.get("args").and_then(|v| v.as_str());
            match args {
                Some(a) => format!("**Skill** `/{} {}`", skill, a),
                None => format!("**Skill** `/{}`", skill),
            }
        }
        "EnterPlanMode" => {
            "**EnterPlanMode**".to_string()
        }
        "NotebookEdit" => {
            let path = input.get("notebook_path").and_then(|v| v.as_str()).unwrap_or("?");
            format!("**NotebookEdit** `{}`", path)
        }
        "AskUserQuestion" => {
            "**AskUserQuestion**".to_string()
        }
        _ => {
            // For MCP tools and others, just show the name
            if name.starts_with("mcp__") {
                // Extract a readable name from mcp__server__tool format
                let parts: Vec<&str> = name.split("__").collect();
                let short_name = parts.last().unwrap_or(&name);
                format!("**MCP** _{}_", short_name)
            } else {
                format!("**{}**", name)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- format_tool_action tests --

    #[test]
    fn format_read_action() {
        let input = serde_json::json!({"file_path": "/src/main.rs"});
        assert_eq!(format_tool_action("Read", &input), "**Read** `/src/main.rs`");
    }

    #[test]
    fn format_bash_action() {
        let input = serde_json::json!({"command": "cargo test"});
        assert_eq!(format_tool_action("Bash", &input), "**Bash** `cargo test`");
    }

    #[test]
    fn format_bash_long_command() {
        let long_cmd = "a".repeat(100);
        let input = serde_json::json!({"command": long_cmd});
        let result = format_tool_action("Bash", &input);
        assert!(result.ends_with("...`"));
        assert!(result.len() < 100);
    }

    #[test]
    fn format_grep_action() {
        let input = serde_json::json!({"pattern": "fn main"});
        assert_eq!(format_tool_action("Grep", &input), "**Grep** `fn main`");
    }

    #[test]
    fn format_mcp_tool_action() {
        let input = serde_json::json!({});
        assert_eq!(format_tool_action("mcp__server__search", &input), "**MCP** _search_");
    }

    #[test]
    fn format_task_action() {
        let input = serde_json::json!({"description": "explore codebase"});
        assert_eq!(format_tool_action("Task", &input), "**Task** _explore codebase_");
    }

    // -- signal_name tests --

    #[test]
    fn signal_name_sigkill() {
        assert_eq!(signal_name(137), "SIGKILL (possibly OOM)");
    }

    #[test]
    fn signal_name_sigsegv() {
        assert_eq!(signal_name(139), "SIGSEGV (segmentation fault)");
    }

    #[test]
    fn signal_name_sigabrt() {
        assert_eq!(signal_name(134), "SIGABRT");
    }

    #[test]
    fn signal_name_sigterm() {
        assert_eq!(signal_name(143), "SIGTERM");
    }

    #[test]
    fn signal_name_unknown() {
        assert_eq!(signal_name(1), "unknown signal");
        assert_eq!(signal_name(255), "unknown signal");
        assert_eq!(signal_name(0), "unknown signal");
    }
}
