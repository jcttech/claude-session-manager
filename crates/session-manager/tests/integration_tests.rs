//! Integration tests for session-manager
//!
//! These tests verify the behavior of various components working together.
//! With the lib+binary crate structure, tests can import library modules directly.

use std::borrow::Cow;

// Direct imports from the library crate
use session_manager::crypto::{sign_request, verify_signature};
use session_manager::devcontainer::{DevcontainerConfig, generate_default_config};
use session_manager::git::{RepoRef, WorktreeMode};
use mattermost_client::sanitize_channel_name;
use session_manager::opnsense::validate_domain;

/// Test shell escaping for container commands
mod shell_escaping {
    use super::*;
    use shell_escape::escape;

    fn shell_escape(s: &str) -> Cow<'_, str> {
        escape(Cow::Borrowed(s))
    }

    #[test]
    fn test_simple_path() {
        let path = "/home/user/project";
        let escaped = shell_escape(path);
        assert_eq!(escaped, "/home/user/project");
    }

    #[test]
    fn test_path_with_spaces() {
        let path = "/home/user/my project";
        let escaped = shell_escape(path);
        // Should be quoted or escaped
        assert!(escaped.contains('\'') || escaped.contains('\\'));
    }

    #[test]
    fn test_path_with_special_chars() {
        let path = "/home/user/project; rm -rf /";
        let escaped = shell_escape(path);
        // The semicolon should be escaped/quoted
        assert!(escaped.contains('\'') || escaped.contains('\\'));
        // Should not be directly executable as multiple commands
        assert_ne!(escaped, path);
    }

    #[test]
    fn test_path_with_quotes() {
        let path = "/home/user/project'test";
        let escaped = shell_escape(path);
        // Single quote should be escaped
        assert!(escaped.len() > path.len());
    }

    #[test]
    fn test_path_with_backticks() {
        let path = "/home/user/`whoami`";
        let escaped = shell_escape(path);
        // Backticks should be escaped to prevent command substitution
        assert!(escaped.contains('\'') || escaped.contains('\\'));
    }

    #[test]
    fn test_path_with_dollar() {
        let path = "/home/$USER/project";
        let escaped = shell_escape(path);
        // Dollar sign should be escaped to prevent variable expansion
        assert!(escaped.contains('\'') || escaped.contains('\\'));
    }

    #[test]
    fn test_path_with_newline() {
        let path = "/home/user/project\nrm -rf /";
        let escaped = shell_escape(path);
        // Newline should be escaped
        assert!(escaped.contains('\'') || escaped.contains('\\'));
    }

    #[test]
    fn test_container_name_format() {
        // Container names are derived from UUIDs, which should be safe
        let uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        let name = format!("claude-{}", &uuid[..8]);
        let escaped = shell_escape(&name);
        // UUID-based names should not need escaping
        assert_eq!(escaped, name);
    }

    #[test]
    fn test_malicious_project_path() {
        // Simulate a malicious project path that tries to escape
        let malicious_paths = vec![
            "$(cat /etc/passwd)",
            "`cat /etc/passwd`",
            "/path; cat /etc/passwd",
            "/path && cat /etc/passwd",
            "/path || cat /etc/passwd",
            "/path | cat /etc/passwd",
            "/path\n cat /etc/passwd",
            "/path' cat /etc/passwd",
            "/path\" cat /etc/passwd",
        ];

        for path in malicious_paths {
            let escaped = shell_escape(path);
            // All these should be properly escaped
            assert!(
                escaped.contains('\'') || escaped.contains('\\') || escaped.starts_with('\''),
                "Path '{}' was not properly escaped: '{}'",
                path,
                escaped
            );
        }
    }
}

/// Test HMAC crypto operations using library imports
mod crypto_integration {
    use super::*;

    #[test]
    fn test_signature_workflow() {
        // Simulate the full approval workflow using library functions directly
        let secret = "production-secret-key-12345";
        let request_id = uuid::Uuid::new_v4().to_string();

        // Generate signatures for both actions
        let approve_sig = sign_request(secret, &request_id, "approve");
        let deny_sig = sign_request(secret, &request_id, "deny");

        // Signatures should be different
        assert_ne!(approve_sig, deny_sig);

        // Both should verify correctly
        assert!(verify_signature(secret, &request_id, "approve", &approve_sig));
        assert!(verify_signature(secret, &request_id, "deny", &deny_sig));

        // Cross-verification should fail
        assert!(!verify_signature(secret, &request_id, "deny", &approve_sig));
        assert!(!verify_signature(secret, &request_id, "approve", &deny_sig));
    }

    #[test]
    fn test_many_concurrent_signatures() {
        use std::sync::Arc;
        use std::thread;

        let secret = Arc::new("shared-secret".to_string());
        let mut handles = vec![];

        for i in 0..100 {
            let secret = secret.clone();
            handles.push(thread::spawn(move || {
                let request_id = format!("request-{}", i);
                let sig = sign_request(&secret, &request_id, "approve");
                assert!(verify_signature(&secret, &request_id, "approve", &sig));
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn test_tampered_request_id_rejected() {
        let secret = "test-secret";
        let sig = sign_request(secret, "legit-request", "approve");
        assert!(!verify_signature(secret, "tampered-request", "approve", &sig));
    }

    #[test]
    fn test_different_secrets_incompatible() {
        let sig = sign_request("secret-a", "request-1", "approve");
        assert!(!verify_signature("secret-b", "request-1", "approve", &sig));
    }
}

/// Test configuration parsing
mod config_parsing {
    use std::collections::HashMap;

    #[test]
    fn test_projects_map_parsing() {
        // Simulate parsing a projects map from environment
        let projects_json = r#"{"project1":"/path/to/project1","project2":"/path/to/project2"}"#;
        let projects: HashMap<String, String> = serde_json::from_str(projects_json).unwrap();

        assert_eq!(projects.len(), 2);
        assert_eq!(projects.get("project1"), Some(&"/path/to/project1".to_string()));
        assert_eq!(projects.get("project2"), Some(&"/path/to/project2".to_string()));
    }

    #[test]
    fn test_empty_projects_map() {
        let projects_json = "{}";
        let projects: HashMap<String, String> = serde_json::from_str(projects_json).unwrap();
        assert!(projects.is_empty());
    }

    #[test]
    fn test_database_url_formats() {
        // Valid PostgreSQL connection URLs
        let valid_urls = vec![
            "postgres://user:pass@localhost:5432/db",
            "postgres://user:pass@localhost/db",
            "postgresql://user:pass@host.example.com:5432/database?sslmode=require",
            "postgres://user:p%40ss@localhost/db", // URL-encoded password
        ];

        for url in valid_urls {
            assert!(
                url.starts_with("postgres://") || url.starts_with("postgresql://"),
                "URL should be a PostgreSQL URL: {}",
                url
            );
        }
    }
}

/// Test UUID generation for session IDs
mod session_ids {
    use uuid::Uuid;

    #[test]
    fn test_session_id_uniqueness() {
        let mut ids: Vec<String> = (0..1000).map(|_| Uuid::new_v4().to_string()).collect();
        let original_len = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), original_len, "All session IDs should be unique");
    }

    #[test]
    fn test_session_id_format() {
        let id = Uuid::new_v4().to_string();
        // UUID v4 format: 8-4-4-4-12 hex digits
        assert_eq!(id.len(), 36);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn test_container_name_from_session_id() {
        let session_id = Uuid::new_v4().to_string();
        let container_name = format!("claude-{}", &session_id[..8]);

        assert!(container_name.starts_with("claude-"));
        assert_eq!(container_name.len(), 15); // "claude-" (7) + 8 chars
        // Container names should be valid for Docker/Podman
        assert!(container_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}

/// Test request/response JSON structures
mod json_structures {
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize)]
    struct CallbackPayload {
        context: CallbackContext,
        user_name: String,
    }

    #[derive(Deserialize)]
    struct CallbackContext {
        action: String,
        request_id: String,
        signature: String,
    }

    #[derive(Serialize)]
    struct CallbackResponse {
        #[serde(skip_serializing_if = "Option::is_none")]
        ephemeral_text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        update: Option<serde_json::Value>,
    }

    #[test]
    fn test_callback_payload_parsing() {
        let json = r#"{
            "context": {
                "action": "approve",
                "request_id": "123e4567-e89b-12d3-a456-426614174000",
                "signature": "abc123def456"
            },
            "user_name": "testuser"
        }"#;

        let payload: CallbackPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.context.action, "approve");
        assert_eq!(payload.context.request_id, "123e4567-e89b-12d3-a456-426614174000");
        assert_eq!(payload.context.signature, "abc123def456");
        assert_eq!(payload.user_name, "testuser");
    }

    #[test]
    fn test_callback_response_serialization() {
        let response = CallbackResponse {
            ephemeral_text: Some("Error message".to_string()),
            update: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("ephemeral_text"));
        assert!(!json.contains("update")); // None should be skipped
    }

    #[test]
    fn test_callback_response_with_update() {
        let response = CallbackResponse {
            ephemeral_text: None,
            update: Some(serde_json::json!({ "message": "" })),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(!json.contains("ephemeral_text")); // None should be skipped
        assert!(json.contains("update"));
    }

    #[test]
    fn test_mattermost_approval_card_structure() {
        let callback_url = "http://session-manager:8000/callback";
        let request_id = "test-request-id";
        let domain = "api.example.com";
        let approve_sig = "approve-signature";
        let deny_sig = "deny-signature";

        let props = serde_json::json!({
            "attachments": [{
                "color": "#FFA500",
                "text": format!("**Network Request:** `{}`", domain),
                "actions": [
                    {
                        "id": "approve",
                        "name": "Approve",
                        "integration": {
                            "url": callback_url,
                            "context": {
                                "action": "approve",
                                "request_id": request_id,
                                "signature": approve_sig
                            }
                        }
                    },
                    {
                        "id": "deny",
                        "name": "Deny",
                        "integration": {
                            "url": callback_url,
                            "context": {
                                "action": "deny",
                                "request_id": request_id,
                                "signature": deny_sig
                            }
                        }
                    }
                ]
            }]
        });

        // Verify structure
        let attachments = props["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 1);

        let actions = attachments[0]["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 2);

        assert_eq!(actions[0]["name"], "Approve");
        assert_eq!(actions[1]["name"], "Deny");

        // Verify signatures are included
        assert_eq!(actions[0]["integration"]["context"]["signature"], approve_sig);
        assert_eq!(actions[1]["integration"]["context"]["signature"], deny_sig);
    }
}

/// Test channel name sanitization using library import
mod channel_naming {
    use super::*;

    #[test]
    fn test_simple_name() {
        assert_eq!(sanitize_channel_name("session-manager"), "session-manager");
    }

    #[test]
    fn test_uppercase_converted() {
        assert_eq!(sanitize_channel_name("Session-Manager"), "session-manager");
    }

    #[test]
    fn test_special_chars_replaced() {
        assert_eq!(sanitize_channel_name("my.repo_name"), "my-repo-name");
    }

    #[test]
    fn test_consecutive_hyphens_collapsed() {
        assert_eq!(sanitize_channel_name("my--repo---name"), "my-repo-name");
    }

    #[test]
    fn test_leading_trailing_hyphens_removed() {
        assert_eq!(sanitize_channel_name("-repo-name-"), "repo-name");
    }

    #[test]
    fn test_max_length_64() {
        let long_name = "a".repeat(100);
        let result = sanitize_channel_name(&long_name);
        assert!(result.len() <= 64);
    }

    #[test]
    fn test_empty_special_chars_returns_fallback() {
        assert_eq!(sanitize_channel_name("!!!"), "claude-session");
    }
}

/// Test orchestrator output marker regex patterns
mod orchestrator_markers {
    use regex::Regex;

    #[test]
    fn test_create_session_marker() {
        let re = Regex::new(r"\[CREATE_SESSION:\s*([^\]]+)\]").unwrap();

        let caps = re.captures("[CREATE_SESSION: org/repo --worktree]").unwrap();
        assert_eq!(caps[1].trim(), "org/repo --worktree");

        assert!(re.captures("[CREATE_SESSION: repo-name]").is_some());
        assert!(re.captures("no marker here").is_none());
    }

    #[test]
    fn test_stop_session_marker() {
        let re = Regex::new(r"\[STOP_SESSION:\s*([^\]]+)\]").unwrap();

        let caps = re.captures("[STOP_SESSION: abcdef12]").unwrap();
        assert_eq!(caps[1].trim(), "abcdef12");

        assert!(re.captures("[STOP_SESSION:]").is_none()); // empty
    }

    #[test]
    fn test_session_status_marker() {
        let re = Regex::new(r"\[SESSION_STATUS\]").unwrap();
        assert!(re.is_match("[SESSION_STATUS]"));
        assert!(!re.is_match("[SESSION_STATUS: extra]"));
    }

    #[test]
    fn test_create_reviewer_marker() {
        let re = Regex::new(r"\[CREATE_REVIEWER:\s*([^\]]+)\]").unwrap();

        let caps = re.captures("[CREATE_REVIEWER: org/repo@branch]").unwrap();
        assert_eq!(caps[1].trim(), "org/repo@branch");

        assert!(re.captures("[CREATE_REVIEWER: ]").is_some()); // whitespace-only content
        assert!(re.captures("[CREATE_REVIEWER:]").is_none()); // no content
    }
}

/// Test network request regex matching
mod network_request_parsing {
    use regex::Regex;

    #[test]
    fn test_network_request_pattern() {
        let network_re = Regex::new(r"\[NETWORK_REQUEST:\s*([^\]]+)\]").unwrap();

        let test_cases = vec![
            ("[NETWORK_REQUEST: api.github.com]", Some("api.github.com")),
            ("[NETWORK_REQUEST:api.github.com]", Some("api.github.com")),
            ("[NETWORK_REQUEST:  api.github.com  ]", Some("api.github.com  ")),
            ("Some text [NETWORK_REQUEST: example.com] more text", Some("example.com")),
            ("[NETWORK_REQUEST: ]", Some("")),
            ("No network request here", None),
            ("[NETWORK_APPROVED: domain.com]", None),
        ];

        for (input, expected) in test_cases {
            let result = network_re.captures(input).map(|c| c[1].trim().to_string());
            let expected = expected.map(|s| s.trim().to_string());
            assert_eq!(
                result, expected,
                "Failed for input: '{}'",
                input
            );
        }
    }

    #[test]
    fn test_network_approved_not_matched() {
        let network_re = Regex::new(r"\[NETWORK_REQUEST:\s*([^\]]+)\]").unwrap();

        // These should NOT match the request pattern
        let non_matches = vec![
            "[NETWORK_APPROVED: example.com]",
            "[NETWORK_DENIED: example.com]",
            "NETWORK_REQUEST: example.com",
            "[network_request: example.com]", // case sensitive
        ];

        for input in non_matches {
            assert!(
                network_re.captures(input).is_none(),
                "Should not match: '{}'",
                input
            );
        }
    }
}

/// Test RepoRef parsing using library import
mod repo_ref_parsing {
    use super::*;

    #[test]
    fn test_parse_basic() {
        let r = RepoRef::parse("org/repo").unwrap();
        assert_eq!(r.org, "org");
        assert_eq!(r.repo, "repo");
        assert!(r.branch.is_none());
        assert!(r.worktree.is_none());
    }

    #[test]
    fn test_parse_with_branch_and_worktree() {
        let r = RepoRef::parse("myorg/myrepo@feature --worktree=wt-name").unwrap();
        assert_eq!(r.org, "myorg");
        assert_eq!(r.repo, "myrepo");
        assert_eq!(r.branch, Some("feature".to_string()));
        assert!(matches!(r.worktree, Some(WorktreeMode::Named(ref n)) if n == "wt-name"));
    }

    #[test]
    fn test_parse_with_default_org() {
        let r = RepoRef::parse_with_default_org("myrepo@main", Some("defaultorg")).unwrap();
        assert_eq!(r.org, "defaultorg");
        assert_eq!(r.repo, "myrepo");
        assert_eq!(r.branch, Some("main".to_string()));
    }

    #[test]
    fn test_parse_rejects_path_traversal() {
        assert!(RepoRef::parse("org/repo --worktree=../../etc/passwd").is_none());
        assert!(RepoRef::parse("org/repo --worktree=/tmp/evil").is_none());
        assert!(RepoRef::parse("org/repo --worktree=.hidden").is_none());
    }

    #[test]
    fn test_full_name() {
        let r = RepoRef::parse("acme/webapp@main").unwrap();
        assert_eq!(r.full_name(), "acme/webapp");
    }

    #[test]
    fn test_looks_like_repo() {
        assert!(RepoRef::looks_like_repo("org/repo"));
        assert!(!RepoRef::looks_like_repo("just-a-name"));
        assert!(!RepoRef::looks_like_repo("/absolute/path"));
        assert!(!RepoRef::looks_like_repo("a/b/c"));
    }
}

/// Test devcontainer.json parsing using library import
mod devcontainer_parsing {
    use super::*;

    #[test]
    fn test_parse_standard_json() {
        let config = DevcontainerConfig::parse(r#"{ "image": "node:20" }"#);
        assert_eq!(config.image.as_deref(), Some("node:20"));
    }

    #[test]
    fn test_parse_jsonc_line_comments() {
        let config = DevcontainerConfig::parse(r#"{
            // Development container
            "image": "rust:1.80"
        }"#);
        assert_eq!(config.image.as_deref(), Some("rust:1.80"));
    }

    #[test]
    fn test_parse_jsonc_block_comments() {
        let config = DevcontainerConfig::parse(r#"{
            /* Multi-line
               block comment */
            "image": "python:3.12"
        }"#);
        assert_eq!(config.image.as_deref(), Some("python:3.12"));
    }

    #[test]
    fn test_url_in_string_preserved() {
        let config = DevcontainerConfig::parse(
            r#"{ "image": "ghcr.io//double-slash:latest" }"#,
        );
        assert_eq!(config.image.as_deref(), Some("ghcr.io//double-slash:latest"));
    }

    #[test]
    fn test_empty_image_treated_as_none() {
        let config = DevcontainerConfig::parse(r#"{ "image": "" }"#);
        assert!(config.image.is_none());
    }

    #[test]
    fn test_invalid_json_returns_default() {
        let config = DevcontainerConfig::parse("not json");
        assert!(config.image.is_none());
    }

    #[test]
    fn test_no_image_field() {
        let config = DevcontainerConfig::parse(r#"{ "build": { "dockerfile": "Dockerfile" } }"#);
        assert!(config.image.is_none());
    }

    #[test]
    fn test_generate_default_config_contains_required_fields() {
        let config = generate_default_config("myimage:latest", "isolated");
        assert!(config.contains("myimage:latest"));
        assert!(config.contains("isolated"));
        assert!(config.contains("claude-config-shared"));
        assert!(config.contains("claude-mem-shared"));
        assert!(config.contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn test_generate_default_config_is_valid_json() {
        let config = generate_default_config("test:v1", "bridge");
        let parsed = DevcontainerConfig::parse(&config);
        assert_eq!(parsed.image.as_deref(), Some("test:v1"));
    }

    #[test]
    fn test_generate_default_config_special_chars_in_image() {
        let config = generate_default_config("ghcr.io/org/image:sha-abc123", "custom-net");
        let parsed = DevcontainerConfig::parse(&config);
        assert_eq!(parsed.image.as_deref(), Some("ghcr.io/org/image:sha-abc123"));
    }
}

/// Test domain validation using library import
mod domain_validation {
    use super::*;

    #[test]
    fn test_valid_domains_accepted() {
        assert!(validate_domain("example.com").is_ok());
        assert!(validate_domain("api.github.com").is_ok());
        assert!(validate_domain("sub.domain.co.uk").is_ok());
        assert!(validate_domain("my-service.example.com").is_ok());
    }

    #[test]
    fn test_wildcards_rejected() {
        assert!(validate_domain("*.example.com").is_err());
        assert!(validate_domain("?.example.com").is_err());
    }

    #[test]
    fn test_ip_addresses_rejected() {
        assert!(validate_domain("192.168.1.1").is_err());
        assert!(validate_domain("::1").is_err());
    }

    #[test]
    fn test_special_chars_rejected() {
        assert!(validate_domain("example.com; rm -rf /").is_err());
        assert!(validate_domain("example.com\nmalicious").is_err());
    }

    #[test]
    fn test_empty_rejected() {
        assert!(validate_domain("").is_err());
        assert!(validate_domain("  ").is_err());
    }

    #[test]
    fn test_no_dot_rejected() {
        assert!(validate_domain("localhost").is_err());
    }

    #[test]
    fn test_leading_trailing_rejected() {
        assert!(validate_domain(".example.com").is_err());
        assert!(validate_domain("example.com.").is_err());
        assert!(validate_domain("-example.com").is_err());
    }

    #[test]
    fn test_consecutive_dots_rejected() {
        assert!(validate_domain("example..com").is_err());
    }
}

/// Test end-to-end approval workflow (crypto + domain validation + channel naming)
mod e2e_workflows {
    use super::*;

    #[test]
    fn test_approval_workflow() {
        // 1. Validate domain
        let domain = "pypi.org";
        assert!(validate_domain(domain).is_ok());

        // 2. Generate approval signatures
        let secret = "test-secret";
        let request_id = uuid::Uuid::new_v4().to_string();
        let approve_sig = sign_request(secret, &request_id, "approve");
        let deny_sig = sign_request(secret, &request_id, "deny");

        // 3. Verify signatures
        assert!(verify_signature(secret, &request_id, "approve", &approve_sig));
        assert!(verify_signature(secret, &request_id, "deny", &deny_sig));
        assert!(!verify_signature(secret, &request_id, "approve", &deny_sig));
    }

    #[test]
    fn test_session_setup_workflow() {
        // 1. Parse repo reference
        let repo = RepoRef::parse("acme/webapp@feature-branch --worktree").unwrap();
        assert_eq!(repo.org, "acme");
        assert_eq!(repo.repo, "webapp");
        assert_eq!(repo.branch, Some("feature-branch".to_string()));
        assert!(matches!(repo.worktree, Some(WorktreeMode::Auto)));

        // 2. Sanitize channel name
        let channel = sanitize_channel_name(&repo.repo);
        assert_eq!(channel, "webapp");

        // 3. Resolve container image from devcontainer
        let config = DevcontainerConfig::parse(r#"{
            // Custom image for this project
            "image": "ghcr.io/acme/devcontainer:latest"
        }"#);
        assert_eq!(config.image.as_deref(), Some("ghcr.io/acme/devcontainer:latest"));
    }

    #[test]
    fn test_malicious_domain_in_network_request() {
        // Domains extracted from container output should be validated
        let malicious_domains = vec![
            "*.evil.com",
            "192.168.1.1",
            "example.com; rm -rf /",
            "",
            "localhost",
        ];

        for domain in malicious_domains {
            assert!(
                validate_domain(domain).is_err(),
                "Domain '{}' should be rejected",
                domain
            );
        }
    }

    #[test]
    fn test_default_devcontainer_fallback_workflow() {
        // When a project has no devcontainer.json, we generate a default one
        let config_str = generate_default_config("claude-code:latest", "isolated");

        // The generated config should be valid and parseable
        let config = DevcontainerConfig::parse(&config_str);
        assert_eq!(config.image.as_deref(), Some("claude-code:latest"));

        // It should include claude-mem volume mount
        assert!(config_str.contains("claude-mem-shared"));
        assert!(config_str.contains("claude-config-shared"));
    }
}

/// Test context management command patterns
mod context_commands {
    #[test]
    fn test_command_parsing() {
        let bot_trigger = "@claude";

        let test_cases = vec![
            ("@claude compact", "compact"),
            ("@claude clear", "clear"),
            ("@claude restart", "restart"),
            ("@claude context", "context"),
            ("@claude stop", "stop"),
        ];

        for (input, expected_cmd) in test_cases {
            assert!(input.starts_with(bot_trigger));
            let cmd = input.trim_start_matches(bot_trigger).trim();
            assert_eq!(cmd, expected_cmd, "Failed for input: '{}'", input);
        }
    }

    #[test]
    fn test_command_with_extra_spaces() {
        let bot_trigger = "@claude";

        let input = "@claude  compact";
        let cmd = input.trim_start_matches(bot_trigger).trim();
        assert_eq!(cmd, "compact");
    }

    #[test]
    fn test_unknown_command_not_matched() {
        let known_commands = ["stop", "compact", "clear", "restart", "context"];
        let cmd = "unknown";
        assert!(!known_commands.contains(&cmd));
    }

    #[test]
    fn test_format_duration_helper() {
        // Simulate the format_duration logic from main.rs
        fn format_duration(total_secs: i64) -> String {
            let total_secs = total_secs.max(0);
            let hours = total_secs / 3600;
            let mins = (total_secs % 3600) / 60;
            let secs = total_secs % 60;

            if hours > 0 {
                format!("{}h {}m", hours, mins)
            } else if mins > 0 {
                format!("{}m", mins)
            } else {
                format!("{}s", secs)
            }
        }

        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(30), "30s");
        assert_eq!(format_duration(60), "1m");
        assert_eq!(format_duration(90), "1m");
        assert_eq!(format_duration(3600), "1h 0m");
        assert_eq!(format_duration(3661), "1h 1m");
        assert_eq!(format_duration(7200), "2h 0m");
        assert_eq!(format_duration(-5), "0s"); // negative clamped to 0
    }
}
