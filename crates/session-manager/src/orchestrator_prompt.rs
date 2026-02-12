/// Default orchestrator prompt template, embedded at compile time.
pub const DEFAULT_TEMPLATE: &str = include_str!("templates/orchestrator_default.md");

/// Substitute template variables in the prompt.
///
/// Supported variables:
/// - `{{repo_name}}` — the repository name (e.g. "org/repo")
/// - `{{session_id}}` — the orchestrator session ID
pub fn render_template(template: &str, repo_name: &str, session_id: &str) -> String {
    template
        .replace("{{repo_name}}", repo_name)
        .replace("{{session_id}}", session_id)
}

/// Load a custom orchestrator prompt template from a file path.
/// Returns `None` if the file doesn't exist or can't be read.
pub fn load_custom_template(path: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(content) if !content.trim().is_empty() => Some(content),
        Ok(_) => {
            tracing::warn!(path, "Custom orchestrator template is empty, using default");
            None
        }
        Err(e) => {
            tracing::debug!(path, error = %e, "Custom orchestrator template not found, using default");
            None
        }
    }
}

/// Build the final orchestrator prompt by checking for custom templates
/// and falling back to the embedded default.
///
/// Lookup order:
/// 1. `ORCHESTRATOR_PROMPT_PATH` environment variable
/// 2. `{project_path}/.claude/orchestrator-prompt.md` (read via SSH — not implemented here)
/// 3. Embedded default template
pub fn resolve_template(custom_path: Option<&str>) -> &str {
    if let Some(path) = custom_path {
        // Custom path is loaded at call site; this function only resolves the default
        tracing::debug!(path, "Custom orchestrator template path configured");
    }
    DEFAULT_TEMPLATE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_template_is_not_empty() {
        assert!(!DEFAULT_TEMPLATE.is_empty());
    }

    #[test]
    fn test_default_template_under_token_limit() {
        // Rough estimate: ~4 chars per token for English text
        let estimated_tokens = DEFAULT_TEMPLATE.len() / 4;
        assert!(
            estimated_tokens < 2000,
            "Template is ~{} tokens, should be under 2000",
            estimated_tokens
        );
    }

    #[test]
    fn test_default_template_contains_markers() {
        assert!(DEFAULT_TEMPLATE.contains("[CREATE_SESSION:"));
        assert!(DEFAULT_TEMPLATE.contains("[CREATE_REVIEWER:"));
        assert!(DEFAULT_TEMPLATE.contains("[SESSION_STATUS]"));
        assert!(DEFAULT_TEMPLATE.contains("[STOP_SESSION:"));
    }

    #[test]
    fn test_render_template_substitutes_variables() {
        let template = "Repo: {{repo_name}}, Session: {{session_id}}";
        let result = render_template(template, "org/myrepo", "abc123");
        assert_eq!(result, "Repo: org/myrepo, Session: abc123");
    }

    #[test]
    fn test_render_template_no_variables() {
        let template = "No variables here";
        let result = render_template(template, "org/repo", "id");
        assert_eq!(result, "No variables here");
    }

    #[test]
    fn test_load_custom_template_nonexistent_file() {
        let result = load_custom_template("/nonexistent/path/template.md");
        assert!(result.is_none());
    }
}
