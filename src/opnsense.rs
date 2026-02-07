use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::settings;

/// Validate a domain name for use in firewall rules.
/// Rejects wildcards, IP addresses, empty strings, and special characters.
pub fn validate_domain(domain: &str) -> Result<()> {
    let domain = domain.trim();

    if domain.is_empty() {
        return Err(anyhow!("Domain cannot be empty"));
    }

    // Reject wildcards
    if domain.contains('*') || domain.contains('?') {
        return Err(anyhow!("Domain cannot contain wildcards: {}", domain));
    }

    // Reject IP addresses (v4 and v6)
    if domain.parse::<std::net::IpAddr>().is_ok() {
        return Err(anyhow!("IP addresses not allowed, must be a domain name: {}", domain));
    }

    // Reject special characters (only allow alphanumeric, hyphens, dots)
    if !domain.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
        return Err(anyhow!("Domain contains invalid characters: {}", domain));
    }

    // Must contain at least one dot (e.g., "example.com")
    if !domain.contains('.') {
        return Err(anyhow!("Domain must contain at least one dot: {}", domain));
    }

    // No leading/trailing dots or hyphens
    if domain.starts_with('.') || domain.ends_with('.') || domain.starts_with('-') || domain.ends_with('-') {
        return Err(anyhow!("Domain has invalid leading/trailing character: {}", domain));
    }

    // No consecutive dots
    if domain.contains("..") {
        return Err(anyhow!("Domain contains consecutive dots: {}", domain));
    }

    Ok(())
}

#[derive(Clone)]
pub struct OPNsense {
    client: Client,
    base_url: String,
    alias: String,
    /// Mutex to prevent race conditions during read-modify-write operations
    write_lock: Arc<Mutex<()>>,
}

#[derive(Deserialize)]
struct AliasResponse {
    alias: AliasContent,
}

#[derive(Deserialize)]
struct AliasContent {
    #[serde(default)]
    content: String,
}

impl OPNsense {
    pub fn new() -> Result<Self> {
        let s = settings();
        let client = Client::builder()
            .danger_accept_invalid_certs(!s.opnsense_verify_tls)
            .timeout(std::time::Duration::from_secs(s.opnsense_timeout_secs))
            .build()?;

        Ok(Self {
            client,
            base_url: s.opnsense_url.clone(),
            alias: s.opnsense_alias.clone(),
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    pub async fn get_domains(&self) -> Result<Vec<String>> {
        let s = settings();
        let url = format!("{}/api/firewall/alias/getItem/{}", self.base_url, self.alias);

        let resp: AliasResponse = self
            .client
            .get(&url)
            .basic_auth(&s.opnsense_key, Some(&s.opnsense_secret))
            .send()
            .await?
            .json()
            .await?;

        let domains: Vec<String> = resp
            .alias
            .content
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(domains)
    }

    pub async fn add_domain(&self, domain: &str) -> Result<bool> {
        // Validate domain before modifying firewall rules
        validate_domain(domain)?;

        // Acquire lock to prevent race conditions during read-modify-write
        let _guard = self.write_lock.lock().await;

        let mut domains = self.get_domains().await?;

        if domains.contains(&domain.to_string()) {
            return Ok(false);
        }

        domains.push(domain.to_string());
        self.set_domains(&domains).await?;
        self.reconfigure().await?;

        Ok(true)
    }

    #[allow(dead_code)]
    pub async fn remove_domain(&self, domain: &str) -> Result<bool> {
        // Acquire lock to prevent race conditions during read-modify-write
        let _guard = self.write_lock.lock().await;

        let mut domains = self.get_domains().await?;

        let Some(pos) = domains.iter().position(|d| d == domain) else {
            return Ok(false);
        };

        domains.remove(pos);
        self.set_domains(&domains).await?;
        self.reconfigure().await?;

        Ok(true)
    }

    async fn set_domains(&self, domains: &[String]) -> Result<()> {
        let s = settings();
        let url = format!("{}/api/firewall/alias/setItem/{}", self.base_url, self.alias);
        let content = domains.join("\n");

        let resp = self.client
            .post(&url)
            .basic_auth(&s.opnsense_key, Some(&s.opnsense_secret))
            .json(&serde_json::json!({
                "alias": { "content": content }
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("OPNsense setItem failed: {} {}", status, body));
        }

        Ok(())
    }

    async fn reconfigure(&self) -> Result<()> {
        let s = settings();
        let url = format!("{}/api/firewall/alias/reconfigure", self.base_url);

        let resp = self.client
            .post(&url)
            .basic_auth(&s.opnsense_key, Some(&s.opnsense_secret))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("OPNsense reconfigure failed: {} {}", status, body));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_domains() {
        assert!(validate_domain("example.com").is_ok());
        assert!(validate_domain("api.github.com").is_ok());
        assert!(validate_domain("sub.domain.example.co.uk").is_ok());
        assert!(validate_domain("my-service.example.com").is_ok());
    }

    #[test]
    fn test_reject_empty() {
        assert!(validate_domain("").is_err());
        assert!(validate_domain("  ").is_err());
    }

    #[test]
    fn test_reject_wildcards() {
        assert!(validate_domain("*.example.com").is_err());
        assert!(validate_domain("example.*.com").is_err());
        assert!(validate_domain("?.example.com").is_err());
    }

    #[test]
    fn test_reject_ip_addresses() {
        assert!(validate_domain("192.168.1.1").is_err());
        assert!(validate_domain("10.0.0.1").is_err());
        assert!(validate_domain("::1").is_err());
        assert!(validate_domain("2001:db8::1").is_err());
    }

    #[test]
    fn test_reject_special_chars() {
        assert!(validate_domain("exam ple.com").is_err());
        assert!(validate_domain("example.com;rm -rf /").is_err());
        assert!(validate_domain("example.com\nmalicious").is_err());
        assert!(validate_domain("example.com|cat /etc/passwd").is_err());
    }

    #[test]
    fn test_reject_no_dot() {
        assert!(validate_domain("localhost").is_err());
    }

    #[test]
    fn test_reject_leading_trailing() {
        assert!(validate_domain(".example.com").is_err());
        assert!(validate_domain("example.com.").is_err());
        assert!(validate_domain("-example.com").is_err());
        assert!(validate_domain("example.com-").is_err());
    }

    #[test]
    fn test_reject_consecutive_dots() {
        assert!(validate_domain("example..com").is_err());
    }
}
