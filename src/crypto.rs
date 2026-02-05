use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Generate an HMAC-SHA256 signature for a request
/// Uses length-prefixed format to prevent collision when request_id contains ':'
/// Format: "{len_request_id}:{request_id}:{len_action}:{action}"
pub fn sign_request(secret: &str, request_id: &str, action: &str) -> String {
    let message = format!(
        "{}:{}:{}:{}",
        request_id.len(),
        request_id,
        action.len(),
        action
    );
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Verify an HMAC-SHA256 signature
pub fn verify_signature(secret: &str, request_id: &str, action: &str, signature: &str) -> bool {
    let expected = sign_request(secret, request_id, action);
    // Constant-time comparison to prevent timing attacks
    constant_time_eq(expected.as_bytes(), signature.as_bytes())
}

/// Constant-time byte comparison
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_and_verify() {
        let secret = "test-secret-key";
        let request_id = "123e4567-e89b-12d3-a456-426614174000";
        let action = "approve";

        let signature = sign_request(secret, request_id, action);
        assert!(verify_signature(secret, request_id, action, &signature));
    }

    #[test]
    fn test_invalid_signature() {
        let secret = "test-secret-key";
        let request_id = "123e4567-e89b-12d3-a456-426614174000";

        assert!(!verify_signature(secret, request_id, "approve", "invalid"));
    }

    #[test]
    fn test_wrong_action() {
        let secret = "test-secret-key";
        let request_id = "123e4567-e89b-12d3-a456-426614174000";

        let signature = sign_request(secret, request_id, "approve");
        assert!(!verify_signature(secret, request_id, "deny", &signature));
    }

    #[test]
    fn test_wrong_secret() {
        let request_id = "123e4567-e89b-12d3-a456-426614174000";
        let action = "approve";

        let signature = sign_request("secret-one", request_id, action);
        assert!(!verify_signature("secret-two", request_id, action, &signature));
    }

    #[test]
    fn test_wrong_request_id() {
        let secret = "test-secret-key";
        let action = "approve";

        let signature = sign_request(secret, "request-1", action);
        assert!(!verify_signature(secret, "request-2", action, &signature));
    }

    #[test]
    fn test_signature_is_deterministic() {
        let secret = "test-secret-key";
        let request_id = "123e4567-e89b-12d3-a456-426614174000";
        let action = "approve";

        let sig1 = sign_request(secret, request_id, action);
        let sig2 = sign_request(secret, request_id, action);
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn test_signature_format() {
        let secret = "test-secret-key";
        let request_id = "test-id";
        let action = "approve";

        let signature = sign_request(secret, request_id, action);
        // HMAC-SHA256 produces 32 bytes = 64 hex characters
        assert_eq!(signature.len(), 64);
        assert!(signature.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_empty_inputs() {
        let signature = sign_request("", "", "");
        assert_eq!(signature.len(), 64);
        assert!(verify_signature("", "", "", &signature));
    }

    #[test]
    fn test_unicode_inputs() {
        let secret = "секрет-ключ-🔐";
        let request_id = "請求-αβγ";
        let action = "承認";

        let signature = sign_request(secret, request_id, action);
        assert!(verify_signature(secret, request_id, action, &signature));
    }

    #[test]
    fn test_constant_time_eq_same() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    #[test]
    fn test_constant_time_eq_different() {
        assert!(!constant_time_eq(b"hello", b"world"));
    }

    #[test]
    fn test_constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"short", b"longer string"));
    }

    #[test]
    fn test_constant_time_eq_empty() {
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn test_no_collision_with_colon_in_request_id() {
        // This test verifies that "a:b" with action "c" produces a different
        // signature than "a" with action "b:c" - which was a vulnerability
        // with the old simple concatenation format
        let secret = "test-secret";

        let sig1 = sign_request(secret, "a:b", "c");
        let sig2 = sign_request(secret, "a", "b:c");

        // With length-prefixed format, these should produce different signatures
        assert_ne!(sig1, sig2, "Signatures should differ to prevent collision attack");
    }
}
