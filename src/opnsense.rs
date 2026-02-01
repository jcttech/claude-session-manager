use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::settings;

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

        self.client
            .post(&url)
            .basic_auth(&s.opnsense_key, Some(&s.opnsense_secret))
            .json(&serde_json::json!({
                "alias": { "content": content }
            }))
            .send()
            .await?;

        Ok(())
    }

    async fn reconfigure(&self) -> Result<()> {
        let s = settings();
        let url = format!("{}/api/firewall/alias/reconfigure", self.base_url);

        self.client
            .post(&url)
            .basic_auth(&s.opnsense_key, Some(&s.opnsense_secret))
            .send()
            .await?;

        Ok(())
    }
}
