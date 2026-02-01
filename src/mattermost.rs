use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

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

    pub async fn listen(&self, tx: mpsc::Sender<Post>) -> Result<()> {
        let s = settings();
        let ws_url = s.mattermost_url.replace("http", "ws") + "/api/v4/websocket";

        let (ws, _) = connect_async(&ws_url).await?;
        let (mut write, mut read) = ws.split();

        // Authenticate
        let auth = serde_json::json!({
            "seq": 1,
            "action": "authentication_challenge",
            "data": { "token": s.mattermost_token }
        });
        write.send(Message::Text(auth.to_string().into())).await?;

        while let Some(msg) = read.next().await {
            let Ok(Message::Text(text)) = msg else { continue };
            let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) else { continue };

            if data.get("event").and_then(|e| e.as_str()) != Some("posted") {
                continue;
            }
            let Some(post_str) = data["data"]["post"].as_str() else { continue };
            let Ok(post) = serde_json::from_str::<Post>(post_str) else { continue };
            if post.user_id != self.bot_user_id {
                let _ = tx.send(post).await;
            }
        }

        Ok(())
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
