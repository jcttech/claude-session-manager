use anyhow::Result;
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

use crate::config::settings;

#[derive(Clone)]
pub struct Mattermost {
    client: Client,
    base_url: String,
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
}

#[derive(Serialize)]
struct PostRequest<'a> {
    channel_id: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    props: Option<serde_json::Value>,
}

impl Mattermost {
    pub async fn new() -> Result<Self> {
        let s = settings();
        let client = Client::builder().build()?;
        let base_url = format!("{}/api/v4", s.mattermost_url);

        let resp: UserResponse = client
            .get(format!("{}/users/me", base_url))
            .header("Authorization", format!("Bearer {}", s.mattermost_token))
            .send()
            .await?
            .json()
            .await?;

        Ok(Self {
            client,
            base_url,
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
        let s = settings();
        let ws_url = s.mattermost_url.replace("http", "ws") + "/api/v4/websocket";

        tracing::info!("Connecting to Mattermost WebSocket");
        let (ws, _) = connect_async(&ws_url).await?;
        let (mut write, mut read) = ws.split();

        // Authenticate
        let auth = serde_json::json!({
            "seq": 1,
            "action": "authentication_challenge",
            "data": { "token": s.mattermost_token }
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
            if post.user_id != self.bot_user_id
                && let Err(e) = tx.send(post).await
            {
                tracing::warn!(error = %e, "Failed to send post to handler channel");
            }
        }

        // Return true if we were connected and processing messages
        Ok(received_any_message)
    }

    pub async fn post(&self, channel_id: &str, message: &str) -> Result<()> {
        let s = settings();

        self.client
            .post(format!("{}/posts", self.base_url))
            .header("Authorization", format!("Bearer {}", s.mattermost_token))
            .json(&PostRequest {
                channel_id,
                message,
                props: None,
            })
            .send()
            .await?;

        Ok(())
    }

    pub async fn post_approval(
        &self,
        channel_id: &str,
        request_id: &str,
        domain: &str,
        approve_signature: &str,
        deny_signature: &str,
    ) -> Result<String> {
        let s = settings();

        let props = serde_json::json!({
            "attachments": [{
                "color": "#FFA500",
                "text": format!("**Network Request:** `{}`", domain),
                "actions": [
                    {
                        "id": "approve",
                        "name": "Approve",
                        "integration": {
                            "url": s.callback_url,
                            "context": {
                                "action": "approve",
                                "request_id": request_id,
                                "signature": approve_signature
                            }
                        }
                    },
                    {
                        "id": "deny",
                        "name": "Deny",
                        "integration": {
                            "url": s.callback_url,
                            "context": {
                                "action": "deny",
                                "request_id": request_id,
                                "signature": deny_signature
                            }
                        }
                    }
                ]
            }]
        });

        let resp: PostResponse = self
            .client
            .post(format!("{}/posts", self.base_url))
            .header("Authorization", format!("Bearer {}", s.mattermost_token))
            .json(&serde_json::json!({
                "channel_id": channel_id,
                "message": "",
                "props": props
            }))
            .send()
            .await?
            .json()
            .await?;

        Ok(resp.id)
    }

    pub async fn update_post(&self, post_id: &str, message: &str) -> Result<()> {
        let s = settings();

        self.client
            .put(format!("{}/posts/{}", self.base_url, post_id))
            .header("Authorization", format!("Bearer {}", s.mattermost_token))
            .json(&serde_json::json!({
                "id": post_id,
                "message": message,
                "props": { "attachments": [] }
            }))
            .send()
            .await?;

        Ok(())
    }
}
