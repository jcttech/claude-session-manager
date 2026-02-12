use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Exponential backoff configuration
const INITIAL_BACKOFF_SECS: u64 = 1;
const MAX_BACKOFF_SECS: u64 = 60;
const BACKOFF_MULTIPLIER: u64 = 2;

#[derive(Clone)]
pub struct Mattermost {
    client: Client,
    base_url: String,
    token: String,
    ws_url: String,
    pub bot_user_id: String,
}

#[derive(Deserialize)]
struct UserResponse {
    id: String,
}

#[derive(Deserialize)]
struct PostResponse {
    id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Post {
    pub id: String,
    pub channel_id: String,
    pub user_id: String,
    pub message: String,
    #[serde(default)]
    pub root_id: String,
}

#[derive(Serialize)]
struct PostRequest<'a> {
    channel_id: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    props: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ChannelResponse {
    id: String,
}

#[derive(Deserialize)]
struct TeamMember {
    user_id: String,
}

#[derive(Deserialize)]
struct SidebarCategory {
    id: String,
    display_name: String,
    #[serde(default)]
    channel_ids: Vec<String>,
}

#[derive(Deserialize)]
struct SidebarCategoriesResponse {
    categories: Vec<SidebarCategory>,
}

impl Mattermost {
    pub async fn new(url: &str, token: &str) -> Result<Self> {
        let client = Client::builder().build()?;
        let base_url = format!("{}/api/v4", url);
        let ws_url = url.replace("http", "ws") + "/api/v4/websocket";

        let resp: UserResponse = client
            .get(format!("{}/users/me", base_url))
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await?
            .json()
            .await?;

        Ok(Self {
            client,
            base_url,
            token: token.to_string(),
            ws_url,
            bot_user_id: resp.id,
        })
    }

    /// Listen for messages with automatic reconnection on disconnect
    /// Uses exponential backoff with jitter to prevent thundering herd
    pub async fn listen(&self, tx: mpsc::Sender<Post>) -> Result<()> {
        let mut backoff_secs = INITIAL_BACKOFF_SECS;
        let mut consecutive_failures = 0u32;

        loop {
            // connect_and_listen returns Ok(true) if it connected successfully before failing
            // This allows us to reset backoff after a successful connection
            match self.connect_and_listen(&tx).await {
                Ok(connected_successfully) => {
                    if connected_successfully {
                        // Connection was working, reset backoff
                        backoff_secs = INITIAL_BACKOFF_SECS;
                        consecutive_failures = 0;
                        tracing::info!("WebSocket connection closed, reconnecting immediately");
                        continue;
                    } else {
                        // Clean exit without successful connection - shouldn't normally happen
                        tracing::info!("WebSocket connection closed normally");
                        break Ok(());
                    }
                }
                Err(e) => {
                    consecutive_failures += 1;

                    // Add jitter (0-25% of backoff time) to prevent thundering herd
                    let jitter_ms = backoff_secs * 250; // 25% max jitter
                    let jitter = if jitter_ms > 0 {
                        // Simple deterministic jitter based on current time
                        (std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64)
                            % jitter_ms
                    } else {
                        0
                    };

                    let delay = Duration::from_secs(backoff_secs) + Duration::from_millis(jitter);

                    tracing::warn!(
                        error = %e,
                        backoff_secs = backoff_secs,
                        jitter_ms = jitter,
                        consecutive_failures = consecutive_failures,
                        "WebSocket disconnected, reconnecting after backoff"
                    );

                    tokio::time::sleep(delay).await;

                    // Increase backoff for next failure (exponential)
                    backoff_secs = (backoff_secs * BACKOFF_MULTIPLIER).min(MAX_BACKOFF_SECS);
                }
            }
        }
    }

    /// Internal method to connect and process WebSocket messages
    /// Returns Ok(true) if we successfully connected and processed messages before disconnecting
    /// Returns Ok(false) for clean shutdown without ever connecting
    /// Returns Err for connection failures
    async fn connect_and_listen(&self, tx: &mpsc::Sender<Post>) -> Result<bool> {
        tracing::info!("Connecting to Mattermost WebSocket");
        let (ws, _) = connect_async(&self.ws_url).await?;
        let (mut write, mut read) = ws.split();

        // Authenticate
        let auth = serde_json::json!({
            "seq": 1,
            "action": "authentication_challenge",
            "data": { "token": self.token }
        });
        write.send(Message::Text(auth.to_string().into())).await?;
        tracing::info!("WebSocket connected and authenticated");

        // Track that we successfully connected
        let mut received_any_message = false;

        while let Some(msg) = read.next().await {
            received_any_message = true;
            let Ok(Message::Text(text)) = msg else { continue };
            let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) else { continue };

            if data.get("event").and_then(|e| e.as_str()) != Some("posted") {
                continue;
            }
            let Some(post_str) = data["data"]["post"].as_str() else { continue };
            let Ok(post) = serde_json::from_str::<Post>(post_str) else { continue };
            if post.user_id != self.bot_user_id {
                match tx.try_send(post) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!("Message handler channel full, dropping message to prevent backpressure");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        tracing::warn!("Message handler channel closed");
                        break;
                    }
                }
            }
        }

        // Return true if we were connected and processing messages
        Ok(received_any_message)
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// Post a message to a channel, returns the post ID
    pub async fn post(&self, channel_id: &str, message: &str) -> Result<String> {
        let resp: PostResponse = self
            .client
            .post(format!("{}/posts", self.base_url))
            .header("Authorization", self.auth_header())
            .json(&PostRequest {
                channel_id,
                message,
                root_id: None,
                props: None,
            })
            .send()
            .await?
            .json()
            .await?;

        Ok(resp.id)
    }

    /// Post a root message to a channel (becomes thread anchor), returns post ID
    pub async fn post_root(&self, channel_id: &str, message: &str) -> Result<String> {
        self.post(channel_id, message).await
    }

    /// Post a reply in a thread, returns post ID
    pub async fn post_in_thread(
        &self,
        channel_id: &str,
        root_id: &str,
        message: &str,
    ) -> Result<String> {
        let resp: PostResponse = self
            .client
            .post(format!("{}/posts", self.base_url))
            .header("Authorization", self.auth_header())
            .json(&PostRequest {
                channel_id,
                message,
                root_id: Some(root_id),
                props: None,
            })
            .send()
            .await?
            .json()
            .await?;

        Ok(resp.id)
    }

    /// Post a message with custom props (e.g. interactive attachments) as a thread reply
    pub async fn post_with_props(
        &self,
        channel_id: &str,
        root_id: &str,
        message: &str,
        props: serde_json::Value,
    ) -> Result<String> {
        let resp: PostResponse = self
            .client
            .post(format!("{}/posts", self.base_url))
            .header("Authorization", self.auth_header())
            .json(&serde_json::json!({
                "channel_id": channel_id,
                "root_id": root_id,
                "message": message,
                "props": props
            }))
            .send()
            .await?
            .json()
            .await?;

        Ok(resp.id)
    }

    pub async fn update_post(&self, post_id: &str, message: &str) -> Result<()> {
        self.client
            .put(format!("{}/posts/{}", self.base_url, post_id))
            .header("Authorization", self.auth_header())
            .json(&serde_json::json!({
                "id": post_id,
                "message": message,
                "props": { "attachments": [] }
            }))
            .send()
            .await?;

        Ok(())
    }

    // --- Channel management ---

    /// Create a channel in a team, returns channel_id
    pub async fn create_channel(
        &self,
        team_id: &str,
        name: &str,
        display_name: &str,
        purpose: &str,
    ) -> Result<String> {
        let resp = self
            .client
            .post(format!("{}/channels", self.base_url))
            .header("Authorization", self.auth_header())
            .json(&serde_json::json!({
                "team_id": team_id,
                "name": name,
                "display_name": display_name,
                "purpose": purpose,
                "type": "O"
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Failed to create channel '{}': {} {}", name, status, body));
        }

        let channel: ChannelResponse = resp.json().await?;
        Ok(channel.id)
    }

    /// Get a channel by name in a team, returns channel_id or None if not found
    pub async fn get_channel_by_name(
        &self,
        team_id: &str,
        name: &str,
    ) -> Result<Option<String>> {
        let resp = self
            .client
            .get(format!(
                "{}/teams/{}/channels/name/{}",
                self.base_url, team_id, name
            ))
            .header("Authorization", self.auth_header())
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Failed to get channel '{}': {} {}", name, status, body));
        }

        let channel: ChannelResponse = resp.json().await?;
        Ok(Some(channel.id))
    }

    // --- Team member management ---

    /// Get all user IDs for a team (paginates automatically)
    pub async fn get_team_member_ids(&self, team_id: &str) -> Result<Vec<String>> {
        let mut user_ids = Vec::new();
        let mut page = 0;
        let per_page = 200;

        loop {
            let resp = self
                .client
                .get(format!(
                    "{}/teams/{}/members?page={}&per_page={}",
                    self.base_url, team_id, page, per_page
                ))
                .header("Authorization", self.auth_header())
                .send()
                .await?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(anyhow!("Failed to get team members: {} {}", status, body));
            }

            let members: Vec<TeamMember> = resp.json().await?;
            let count = members.len();
            user_ids.extend(members.into_iter().map(|m| m.user_id));

            if count < per_page {
                break;
            }
            page += 1;
        }

        Ok(user_ids)
    }

    // --- Sidebar category management ---

    /// Ensure a sidebar category exists for a user, returns category_id
    pub async fn ensure_sidebar_category(
        &self,
        user_id: &str,
        team_id: &str,
        category_name: &str,
    ) -> Result<String> {
        // Get existing categories
        let resp = self
            .client
            .get(format!(
                "{}/users/{}/teams/{}/channels/categories",
                self.base_url, user_id, team_id
            ))
            .header("Authorization", self.auth_header())
            .send()
            .await?;

        if resp.status().is_success() {
            let categories: SidebarCategoriesResponse = resp.json().await?;

            // Look for existing category by name
            if let Some(cat) = categories
                .categories
                .iter()
                .find(|c| c.display_name == category_name)
            {
                return Ok(cat.id.clone());
            }
        }

        // Create new category
        let resp = self
            .client
            .post(format!(
                "{}/users/{}/teams/{}/channels/categories",
                self.base_url, user_id, team_id
            ))
            .header("Authorization", self.auth_header())
            .json(&serde_json::json!({
                "user_id": user_id,
                "team_id": team_id,
                "display_name": category_name,
                "type": "custom",
                "channel_ids": []
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Failed to create sidebar category '{}': {} {}",
                category_name,
                status,
                body
            ));
        }

        let cat: SidebarCategory = resp.json().await?;
        Ok(cat.id)
    }

    // --- Thread follow/unfollow ---

    /// Follow a thread on behalf of a user.
    pub async fn follow_thread(&self, user_id: &str, thread_id: &str) -> Result<()> {
        let resp = self
            .client
            .put(format!(
                "{}/users/{}/threads/{}/following",
                self.base_url, user_id, thread_id
            ))
            .header("Authorization", self.auth_header())
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Failed to follow thread: {} {}", status, body));
        }

        Ok(())
    }

    /// Unfollow a thread on behalf of a user.
    pub async fn unfollow_thread(&self, user_id: &str, thread_id: &str) -> Result<()> {
        let resp = self
            .client
            .delete(format!(
                "{}/users/{}/threads/{}/following",
                self.base_url, user_id, thread_id
            ))
            .header("Authorization", self.auth_header())
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Failed to unfollow thread: {} {}", status, body));
        }

        Ok(())
    }

    /// Add a channel to a user's sidebar category
    pub async fn add_channel_to_category(
        &self,
        user_id: &str,
        team_id: &str,
        category_id: &str,
        channel_id: &str,
    ) -> Result<()> {
        // First get current category to preserve existing channels
        let resp = self
            .client
            .get(format!(
                "{}/users/{}/teams/{}/channels/categories/{}",
                self.base_url, user_id, team_id, category_id
            ))
            .header("Authorization", self.auth_header())
            .send()
            .await?;

        let mut channel_ids: Vec<String> = if resp.status().is_success() {
            let cat: SidebarCategory = resp.json().await?;
            cat.channel_ids
        } else {
            vec![]
        };

        // Add channel if not already present
        if !channel_ids.contains(&channel_id.to_string()) {
            channel_ids.push(channel_id.to_string());
        } else {
            return Ok(()); // Already in category
        }

        // Update category with new channel list
        self.client
            .put(format!(
                "{}/users/{}/teams/{}/channels/categories/{}",
                self.base_url, user_id, team_id, category_id
            ))
            .header("Authorization", self.auth_header())
            .json(&serde_json::json!({
                "id": category_id,
                "channel_ids": channel_ids
            }))
            .send()
            .await?;

        Ok(())
    }
}

/// Sanitize a string for use as a Mattermost channel name.
/// Lowercase, `[a-z0-9-]` only, max 64 chars.
pub fn sanitize_channel_name(name: &str) -> String {
    let sanitized: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();

    // Collapse consecutive hyphens and trim leading/trailing hyphens
    let mut result = String::new();
    let mut prev_hyphen = true; // treat start as hyphen to skip leading
    for c in sanitized.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push(c);
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }

    // Trim trailing hyphen
    if result.ends_with('-') {
        result.pop();
    }

    // Truncate to 64 chars
    if result.len() > 64 {
        result.truncate(64);
        // Don't leave a trailing hyphen after truncation
        while result.ends_with('-') {
            result.pop();
        }
    }

    // Fallback for all-special-char input that sanitizes to empty string
    if result.is_empty() {
        return "claude-session".to_string();
    }

    result
}
