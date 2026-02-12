use serde::Deserialize;

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

// -- NDJSON deserialization types for `claude -p --verbose --output-format stream-json` --
//
// The Claude CLI outputs these top-level event types:
//   "system"    — hooks, init, tool use metadata
//   "assistant" — complete assistant response with message content
//   "result"    — final result with usage stats

/// Top-level line in the NDJSON stream.
#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
pub enum StreamLine {
    #[serde(rename = "system")]
    System {
        subtype: Option<String>,
        session_id: Option<String>,
    },
    #[serde(rename = "assistant")]
    Assistant {
        message: AssistantMessage,
        session_id: Option<String>,
    },
    #[serde(rename = "result")]
    Result {
        result: Option<String>,
        usage: Option<ResultUsage>,
        #[serde(rename = "total_cost_usd")]
        total_cost_usd: Option<f64>,
    },
    #[serde(other)]
    Unknown,
}

/// The assistant's response message
#[derive(Deserialize, Debug)]
pub struct AssistantMessage {
    pub content: Option<Vec<ContentPart>>,
    pub usage: Option<Usage>,
}

/// A content block within an assistant message
#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: Option<String>,
        content: Option<serde_json::Value>,
    },
    #[serde(other)]
    Other,
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

#[derive(Deserialize, Debug)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

/// Usage stats from the result event
#[derive(Deserialize, Debug)]
pub struct ResultUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

// -- LineBuffer: reassembles text fragments into complete lines --

pub struct LineBuffer {
    partial: String,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self {
            partial: String::new(),
        }
    }

    /// Feed a text fragment. Returns any complete lines (split on `\n`).
    /// Incomplete trailing text is buffered for the next call.
    pub fn feed(&mut self, text: &str) -> Vec<String> {
        self.partial.push_str(text);
        let mut lines = Vec::new();

        while let Some(pos) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=pos).collect();
            // Strip the trailing newline
            let line = line.trim_end_matches('\n').to_string();
            lines.push(line);
        }

        lines
    }

    /// Flush any remaining buffered text as a final line.
    /// Call this at content_block_stop boundaries.
    pub fn flush(&mut self) -> Option<String> {
        if self.partial.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.partial))
        }
    }
}

impl Default for LineBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- LineBuffer tests --

    #[test]
    fn line_buffer_single_line() {
        let mut buf = LineBuffer::new();
        let lines = buf.feed("hello world\n");
        assert_eq!(lines, vec!["hello world"]);
        assert_eq!(buf.flush(), None);
    }

    #[test]
    fn line_buffer_multiple_lines() {
        let mut buf = LineBuffer::new();
        let lines = buf.feed("line1\nline2\nline3\n");
        assert_eq!(lines, vec!["line1", "line2", "line3"]);
        assert_eq!(buf.flush(), None);
    }

    #[test]
    fn line_buffer_partial() {
        let mut buf = LineBuffer::new();
        let lines = buf.feed("hel");
        assert!(lines.is_empty());
        let lines = buf.feed("lo\nwor");
        assert_eq!(lines, vec!["hello"]);
        let lines = buf.feed("ld\n");
        assert_eq!(lines, vec!["world"]);
        assert_eq!(buf.flush(), None);
    }

    #[test]
    fn line_buffer_flush_partial() {
        let mut buf = LineBuffer::new();
        let lines = buf.feed("no newline yet");
        assert!(lines.is_empty());
        assert_eq!(buf.flush(), Some("no newline yet".to_string()));
        assert_eq!(buf.flush(), None); // second flush is empty
    }

    #[test]
    fn line_buffer_empty_lines() {
        let mut buf = LineBuffer::new();
        let lines = buf.feed("\n\n");
        assert_eq!(lines, vec!["", ""]);
    }

    // -- Serde deserialization tests for actual CLI format --

    #[test]
    fn deserialize_system_init() {
        let json = r#"{"type":"system","subtype":"init","cwd":"/workspaces/test","session_id":"abc-123","tools":["Bash"]}"#;
        let parsed: StreamLine = serde_json::from_str(json).unwrap();
        match parsed {
            StreamLine::System { subtype, session_id } => {
                assert_eq!(subtype.unwrap(), "init");
                assert_eq!(session_id.unwrap(), "abc-123");
            }
            _ => panic!("Expected System"),
        }
    }

    #[test]
    fn deserialize_system_hook() {
        let json = r#"{"type":"system","subtype":"hook_started","hook_id":"abc","hook_name":"SessionStart:startup"}"#;
        let parsed: StreamLine = serde_json::from_str(json).unwrap();
        match parsed {
            StreamLine::System { subtype, session_id } => {
                assert_eq!(subtype.unwrap(), "hook_started");
                assert!(session_id.is_none());
            }
            _ => panic!("Expected System"),
        }
    }

    #[test]
    fn deserialize_assistant() {
        let json = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello world!"}],"usage":{"input_tokens":100,"output_tokens":25}},"session_id":"abc-123"}"#;
        let parsed: StreamLine = serde_json::from_str(json).unwrap();
        match parsed {
            StreamLine::Assistant { message, session_id } => {
                assert_eq!(session_id.unwrap(), "abc-123");
                let content = message.content.unwrap();
                assert_eq!(content.len(), 1);
                match &content[0] {
                    ContentPart::Text { text } => assert_eq!(text, "Hello world!"),
                    _ => panic!("Expected Text content"),
                }
                assert_eq!(message.usage.unwrap().input_tokens.unwrap(), 100);
            }
            _ => panic!("Expected Assistant"),
        }
    }

    #[test]
    fn deserialize_assistant_multipart() {
        let json = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"line 1\nline 2\n"},{"type":"tool_use","id":"abc","name":"Read","input":{"file_path":"/test"}},{"type":"text","text":"line 3"}]}}"#;
        let parsed: StreamLine = serde_json::from_str(json).unwrap();
        match parsed {
            StreamLine::Assistant { message, .. } => {
                let content = message.content.unwrap();
                assert_eq!(content.len(), 3);
                match &content[0] {
                    ContentPart::Text { text } => assert_eq!(text, "line 1\nline 2\n"),
                    _ => panic!("Expected Text"),
                }
                match &content[1] {
                    ContentPart::ToolUse { name, .. } => assert_eq!(name, "Read"),
                    _ => panic!("Expected ToolUse"),
                }
                match &content[2] {
                    ContentPart::Text { text } => assert_eq!(text, "line 3"),
                    _ => panic!("Expected Text"),
                }
            }
            _ => panic!("Expected Assistant"),
        }
    }

    #[test]
    fn deserialize_result() {
        let json = r#"{"type":"result","subtype":"success","result":"Hello!","total_cost_usd":0.05,"usage":{"input_tokens":100,"output_tokens":25}}"#;
        let parsed: StreamLine = serde_json::from_str(json).unwrap();
        match parsed {
            StreamLine::Result { result, usage, total_cost_usd } => {
                assert_eq!(result.unwrap(), "Hello!");
                assert_eq!(total_cost_usd.unwrap(), 0.05);
                let u = usage.unwrap();
                assert_eq!(u.input_tokens.unwrap(), 100);
                assert_eq!(u.output_tokens.unwrap(), 25);
            }
            _ => panic!("Expected Result"),
        }
    }

    #[test]
    fn deserialize_unknown_type() {
        let json = r#"{"type":"something_new","data":42}"#;
        let parsed: StreamLine = serde_json::from_str(json).unwrap();
        assert!(matches!(parsed, StreamLine::Unknown));
    }

    #[test]
    fn deserialize_result_null_fields() {
        let json = r#"{"type":"result","subtype":"success"}"#;
        let parsed: StreamLine = serde_json::from_str(json).unwrap();
        match parsed {
            StreamLine::Result { result, usage, total_cost_usd } => {
                assert!(result.is_none());
                assert!(usage.is_none());
                assert!(total_cost_usd.is_none());
            }
            _ => panic!("Expected Result"),
        }
    }

    #[test]
    fn deserialize_tool_use() {
        let json = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_abc","name":"Read","input":{"file_path":"/src/main.rs"}},{"type":"text","text":"Here is the file."}]}}"#;
        let parsed: StreamLine = serde_json::from_str(json).unwrap();
        match parsed {
            StreamLine::Assistant { message, .. } => {
                let content = message.content.unwrap();
                assert_eq!(content.len(), 2);
                match &content[0] {
                    ContentPart::ToolUse { name, input } => {
                        assert_eq!(name, "Read");
                        assert_eq!(input["file_path"], "/src/main.rs");
                    }
                    _ => panic!("Expected ToolUse"),
                }
                match &content[1] {
                    ContentPart::Text { text } => assert_eq!(text, "Here is the file."),
                    _ => panic!("Expected Text"),
                }
            }
            _ => panic!("Expected Assistant"),
        }
    }

    #[test]
    fn deserialize_tool_result() {
        let json = r#"{"type":"assistant","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_abc","content":"file contents here"}]}}"#;
        let parsed: StreamLine = serde_json::from_str(json).unwrap();
        match parsed {
            StreamLine::Assistant { message, .. } => {
                let content = message.content.unwrap();
                assert_eq!(content.len(), 1);
                match &content[0] {
                    ContentPart::ToolResult { tool_use_id, .. } => {
                        assert_eq!(tool_use_id.as_deref().unwrap(), "toolu_abc");
                    }
                    _ => panic!("Expected ToolResult"),
                }
            }
            _ => panic!("Expected Assistant"),
        }
    }

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
