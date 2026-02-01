//! Integration tests for session-manager
//!
//! These tests verify the behavior of various components working together.
//! Some tests require external services (PostgreSQL) and are skipped by default.

use std::borrow::Cow;

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

/// Test HMAC crypto operations
mod crypto_integration {
    #[test]
    fn test_signature_workflow() {
        // Simulate the full approval workflow
        let secret = "production-secret-key-12345";
        let request_id = uuid::Uuid::new_v4().to_string();

        // Generate signatures for both actions (as done in handle_network_request)
        let approve_sig = hmac_sign(secret, &request_id, "approve");
        let deny_sig = hmac_sign(secret, &request_id, "deny");

        // Signatures should be different
        assert_ne!(approve_sig, deny_sig);

        // Both should verify correctly
        assert!(hmac_verify(secret, &request_id, "approve", &approve_sig));
        assert!(hmac_verify(secret, &request_id, "deny", &deny_sig));

        // Cross-verification should fail
        assert!(!hmac_verify(secret, &request_id, "deny", &approve_sig));
        assert!(!hmac_verify(secret, &request_id, "approve", &deny_sig));
    }

    fn hmac_sign(secret: &str, request_id: &str, action: &str) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        type HmacSha256 = Hmac<Sha256>;

        let message = format!("{}:{}", request_id, action);
        let mut mac =
            HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
        mac.update(message.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn hmac_verify(secret: &str, request_id: &str, action: &str, signature: &str) -> bool {
        let expected = hmac_sign(secret, request_id, action);
        expected == signature
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
                let sig = hmac_sign(&secret, &request_id, "approve");
                assert!(hmac_verify(&secret, &request_id, "approve", &sig));
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
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
