use shell_escape::escape;
use std::borrow::Cow;

use crate::ssh;

/// Check whether a project on the VM has a devcontainer.json file.
/// Checks `.devcontainer/devcontainer.json` first, then `.devcontainer.json`.
pub async fn has_devcontainer_config(project_path: &str) -> bool {
    let escaped = escape(Cow::Borrowed(project_path));

    let cmd = format!(
        "test -f {}/.devcontainer/devcontainer.json || test -f {}/.devcontainer.json",
        escaped, escaped
    );
    ssh::run_command(&cmd).await.is_ok()
}

/// Generate a minimal devcontainer.json for projects that don't have one.
/// Uses the fallback container image and network from config.
/// Includes postStartCommand for agent worker and port mapping via runArgs.
pub fn generate_default_config(image: &str, network: &str, port: u16) -> String {
    format!(
        r#"{{
    "image": "{}",
    "mounts": [
        "source=claude-config-shared,target=/home/vscode/.claude,type=volume",
        "source=claude-mem-shared,target=/home/vscode/.claude-mem,type=volume"
    ],
    "containerEnv": {{
        "ANTHROPIC_API_KEY": "${{localEnv:ANTHROPIC_API_KEY}}"
    }},
    "postStartCommand": "python3 -m agent_worker --port 50051 &",
    "runArgs": ["--network={}", "-p", "{}:50051"]
}}"#,
        image, network, port
    )
}

/// Read the raw devcontainer.json content from a project on the VM.
/// Returns `None` if the file doesn't exist or can't be read.
pub async fn read_config_content(project_path: &str) -> Option<String> {
    let escaped = escape(Cow::Borrowed(project_path));
    let cmd = format!(
        "cat {}/.devcontainer/devcontainer.json 2>/dev/null || cat {}/.devcontainer.json 2>/dev/null",
        escaped, escaped
    );
    let content = ssh::run_command(&cmd).await.ok()?;
    if content.is_empty() {
        return None;
    }
    Some(content)
}

/// Build a complete override config by reading the existing devcontainer.json,
/// stripping JSONC comments, and merging in our port mapping and worker startup.
/// The override config replaces the entire config, so it must include all original properties.
/// Only injects port mapping and postStartCommand — preserves the repo's network settings.
pub fn build_override_config(original_content: &str, port: u16) -> anyhow::Result<String> {
    let stripped = strip_jsonc_comments(original_content);
    let mut config: serde_json::Value = serde_json::from_str(&stripped)
        .map_err(|e| anyhow::anyhow!("Failed to parse devcontainer.json: {}", e))?;

    let obj = config.as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("devcontainer.json is not a JSON object"))?;

    // Set postStartCommand for agent worker
    obj.insert(
        "postStartCommand".to_string(),
        serde_json::Value::String("python3 -m agent_worker --port 50051 &".to_string()),
    );

    // Add port mapping to runArgs, preserving all existing args (including network)
    let mut run_args: Vec<serde_json::Value> = obj
        .get("runArgs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // Remove any existing -p port:50051 args (we'll add ours with the allocated port)
    // Walk backwards to handle -p and its value as adjacent elements
    let mut i = 0;
    while i < run_args.len() {
        let s = run_args[i].as_str().unwrap_or("");
        if s == "-p" && i + 1 < run_args.len()
            && run_args[i + 1].as_str().unwrap_or("").contains(":50051")
        {
            run_args.remove(i + 1);
            run_args.remove(i);
        } else if s.contains(":50051") {
            run_args.remove(i);
        } else {
            i += 1;
        }
    }

    run_args.push(serde_json::Value::String("-p".to_string()));
    run_args.push(serde_json::Value::String(format!("{}:50051", port)));

    obj.insert("runArgs".to_string(), serde_json::Value::Array(run_args));

    serde_json::to_string_pretty(&config)
        .map_err(|e| anyhow::anyhow!("Failed to serialize override config: {}", e))
}

/// Write an override config file to the VM via SSH.
/// Returns the path on the VM for use with `--override-config`.
pub async fn write_override_config(port: u16, config: &str) -> anyhow::Result<String> {
    let path = format!("/tmp/sm-override-{}.json", port);
    let escaped_path = escape(Cow::Borrowed(&path));
    let write_cmd = format!(
        "cat > {} << 'OCEOF'\n{}\nOCEOF",
        escaped_path, config
    );
    ssh::run_command(&write_cmd).await?;
    Ok(path)
}

/// Write a default devcontainer.json to a project directory on the VM.
pub async fn write_default_config(project_path: &str, config_content: &str) -> anyhow::Result<()> {
    let escaped = escape(Cow::Borrowed(project_path));

    let write_cmd = format!(
        "mkdir -p {}/.devcontainer && cat > {}/.devcontainer/devcontainer.json << 'DCEOF'\n{}\nDCEOF",
        escaped, escaped, config_content
    );
    ssh::run_command(&write_cmd).await?;
    Ok(())
}

/// Strip both `//` line comments and `/* */` block comments from JSONC content.
/// Handles comments inside strings correctly (doesn't strip those).
fn strip_jsonc_comments(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let bytes = content.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut in_string = false;
    let mut escape_next = false;

    while i < len {
        if escape_next {
            escape_next = false;
            result.push(bytes[i] as char);
            i += 1;
            continue;
        }

        match bytes[i] {
            b'\\' if in_string => {
                escape_next = true;
                result.push('\\');
                i += 1;
            }
            b'"' => {
                in_string = !in_string;
                result.push('"');
                i += 1;
            }
            b'/' if !in_string && i + 1 < len && bytes[i + 1] == b'/' => {
                // Line comment: skip to end of line
                i += 2;
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if !in_string && i + 1 < len && bytes[i + 1] == b'*' => {
                // Block comment: skip to */
                i += 2;
                while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    // Preserve newlines within block comments for correct line numbers
                    if bytes[i] == b'\n' {
                        result.push('\n');
                    }
                    i += 1;
                }
                if i + 1 < len {
                    i += 2; // Skip closing */
                }
            }
            _ => {
                result.push(bytes[i] as char);
                i += 1;
            }
        }
    }

    result
}

/// Parsed devcontainer.json configuration.
/// Used for parsing devcontainer.json content when needed.
#[derive(Debug, Default)]
pub struct DevcontainerConfig {
    pub image: Option<String>,
}

impl DevcontainerConfig {
    /// Parse devcontainer.json content (supports JSONC line and block comments).
    /// Returns `Default` on any parse failure.
    pub fn parse(content: &str) -> Self {
        let stripped = strip_jsonc_comments(content);

        let parsed: serde_json::Value = match serde_json::from_str(&stripped) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse devcontainer.json");
                return Self::default();
            }
        };

        let image = parsed
            .get("image")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());

        Self { image }
    }
}

/// Compute a SHA-256 hash of the devcontainer.json content on the VM.
/// Returns `None` if the file doesn't exist or can't be read.
pub async fn hash_config(project_path: &str) -> Option<String> {
    use sha2::{Sha256, Digest};
    let escaped = escape(Cow::Borrowed(project_path));

    // Try both locations
    let cmd = format!(
        "cat {}/.devcontainer/devcontainer.json 2>/dev/null || cat {}/.devcontainer.json 2>/dev/null",
        escaped, escaped
    );
    let content = ssh::run_command(&cmd).await.ok()?;
    if content.is_empty() {
        return None;
    }

    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    Some(format!("{:x}", hasher.finalize()))
}

/// Find the start index of a `//` line comment outside of a JSON string.
/// Returns `None` if no comment found.
#[cfg(test)]
fn find_line_comment(line: &str) -> Option<usize> {
    let mut in_string = false;
    let mut escape_next = false;
    let bytes = line.as_bytes();

    for i in 0..bytes.len() {
        if escape_next {
            escape_next = false;
            continue;
        }
        match bytes[i] {
            b'\\' if in_string => {
                escape_next = true;
            }
            b'"' => {
                in_string = !in_string;
            }
            b'/' if !in_string && i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                return Some(i);
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_image() {
        let json = r#"{ "image": "ghcr.io/jcttech/devcontainer-rust:latest" }"#;
        let config = DevcontainerConfig::parse(json);
        assert_eq!(
            config.image.as_deref(),
            Some("ghcr.io/jcttech/devcontainer-rust:latest")
        );
    }

    #[test]
    fn test_parse_no_image_field() {
        let json = r#"{ "build": { "dockerfile": "Dockerfile" } }"#;
        let config = DevcontainerConfig::parse(json);
        assert!(config.image.is_none());
    }

    #[test]
    fn test_parse_empty_image() {
        let json = r#"{ "image": "" }"#;
        let config = DevcontainerConfig::parse(json);
        assert!(config.image.is_none());
    }

    #[test]
    fn test_parse_invalid_json() {
        let config = DevcontainerConfig::parse("not json at all");
        assert!(config.image.is_none());
    }

    #[test]
    fn test_parse_jsonc_with_comments() {
        let jsonc = r#"{
            // This is a comment
            "image": "myimage:v1",
            // Another comment
            "name": "test"
        }"#;
        let config = DevcontainerConfig::parse(jsonc);
        assert_eq!(config.image.as_deref(), Some("myimage:v1"));
    }

    #[test]
    fn test_parse_url_in_string_not_stripped() {
        // Ensure // inside a string value (like a URL) is NOT treated as a comment
        let json = r#"{ "image": "ghcr.io//double-slash:latest" }"#;
        let config = DevcontainerConfig::parse(json);
        assert_eq!(
            config.image.as_deref(),
            Some("ghcr.io//double-slash:latest")
        );
    }

    #[test]
    fn test_find_line_comment() {
        assert_eq!(find_line_comment("// comment"), Some(0));
        assert_eq!(find_line_comment("  // indented"), Some(2));
        assert_eq!(find_line_comment(r#""url://foo" // comment"#), Some(12));
        assert_eq!(find_line_comment(r#""no comment here""#), None);
        assert_eq!(find_line_comment(r#""image": "ghcr.io//test""#), None);
    }

    #[test]
    fn test_parse_jsonc_with_block_comments() {
        let jsonc = r#"{
            /* This is a block comment */
            "image": "myimage:v2",
            /* Multi-line
               block comment */
            "name": "test"
        }"#;
        let config = DevcontainerConfig::parse(jsonc);
        assert_eq!(config.image.as_deref(), Some("myimage:v2"));
    }

    #[test]
    fn test_parse_jsonc_with_mixed_comments() {
        let jsonc = r#"{
            // Line comment
            /* Block comment */
            "image": "mixed:v1"
        }"#;
        let config = DevcontainerConfig::parse(jsonc);
        assert_eq!(config.image.as_deref(), Some("mixed:v1"));
    }

    #[test]
    fn test_block_comment_in_string_not_stripped() {
        let json = r#"{ "image": "/* not a comment */" }"#;
        let config = DevcontainerConfig::parse(json);
        assert_eq!(config.image.as_deref(), Some("/* not a comment */"));
    }

    #[test]
    fn test_generate_default_config() {
        let config = generate_default_config("myimage:latest", "isolated", 50051);
        assert!(config.contains("myimage:latest"));
        assert!(config.contains("isolated"));
        assert!(config.contains("claude-config-shared"));
        assert!(config.contains("claude-mem-shared"));
        assert!(config.contains("ANTHROPIC_API_KEY"));
        assert!(config.contains("postStartCommand"));
        assert!(config.contains("50051:50051"));
    }

    #[test]
    fn test_generate_default_config_custom_port() {
        let config = generate_default_config("myimage:latest", "isolated", 50053);
        assert!(config.contains("50053:50051"));
        assert!(!config.contains("50051:50051"));
    }

    #[test]
    fn test_build_override_config_merges_properties() {
        let original = r#"{
            "image": "ghcr.io/org/repo:latest",
            "containerEnv": { "FOO": "bar" }
        }"#;
        let result = build_override_config(original, 50053).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        // Preserves original properties
        assert_eq!(parsed["image"].as_str(), Some("ghcr.io/org/repo:latest"));
        assert_eq!(parsed["containerEnv"]["FOO"].as_str(), Some("bar"));
        // Adds our properties
        assert!(parsed["postStartCommand"].as_str().unwrap().contains("agent_worker"));
        let run_args: Vec<&str> = parsed["runArgs"].as_array().unwrap()
            .iter().filter_map(|v| v.as_str()).collect();
        assert!(run_args.contains(&"-p"));
        assert!(run_args.contains(&"50053:50051"));
    }

    #[test]
    fn test_build_override_config_replaces_existing_port() {
        let original = r#"{
            "image": "test:v1",
            "runArgs": ["--network=old-net", "-p", "50051:50051", "--cap-add=SYS_PTRACE"]
        }"#;
        let result = build_override_config(original, 50055).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let run_args: Vec<&str> = parsed["runArgs"].as_array().unwrap()
            .iter().filter_map(|v| v.as_str()).collect();
        // Existing network preserved, old port replaced, custom args preserved
        assert!(run_args.contains(&"--network=old-net"));
        assert!(run_args.contains(&"50055:50051"));
        assert!(run_args.contains(&"--cap-add=SYS_PTRACE"));
        assert!(!run_args.contains(&"50051:50051"));
    }

    #[test]
    fn test_build_override_config_handles_jsonc() {
        let jsonc = r#"{
            // This repo uses a custom image
            "image": "myimage:v2",
            /* block comment */
            "name": "test"
        }"#;
        let result = build_override_config(jsonc, 50052).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["image"].as_str(), Some("myimage:v2"));
        assert!(parsed["postStartCommand"].as_str().is_some());
    }
}
