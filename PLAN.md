# Claude Code Mattermost + Firewall Approval System

Chat with Claude Code through Mattermost. Network controlled via OPNsense firewall alias - web search works freely, package installs require approval.

---

## Deployment Checklist

### LANLLM VM (Completed)
- [x] Container runtime (podman/docker)
- [x] SSH access configured (user + key)
- [x] ANTHROPIC_API_KEY environment variable set

### Kubernetes
- [ ] Secrets: Mattermost token, SSH key, OPNsense API credentials
- [ ] ConfigMap: project paths
- [ ] Deploy session-manager

### Test
- [ ] `@claude start <project>`
- [ ] Verify web search works (no approval needed)
- [ ] Trigger `pip install` → see timeout
- [ ] Approve domain in Mattermost
- [ ] Verify retry works
- [ ] `@claude stop`

### Session Manager
- [ ] Build session-manager binary (use devcontainer or GitHub Actions)

### DevContainer & CI (Completed)
- [x] DevContainer configuration for local development
- [x] GitHub Actions workflow for build and Docker image

### OPNsense (Completed)
- [x] Create `llm_approved_domains` alias with `api.anthropic.com`
- [x] Create LANLLM firewall rules (DNS, llm_approved_domains:443, block all)
- [x] Create LAN firewall rule (SSH to LANLLM for session-manager)
- [x] Create `claude-session-manager` API user with firewall alias privileges

### Container Image (Completed)
- [x] Build container image with Claude Code installed

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│ LAN (core) - Kubernetes cluster                                             │
│                                                                             │
│  ┌─────────────────┐     ┌─────────────────────────────────────────┐       │
│  │ Mattermost      │◀───▶│ Session Manager                         │       │
│  │                 │     │  • Relays chat via SSH to container     │       │
│  │                 │     │  • Detects [NETWORK_REQUEST: ...]       │       │
│  │                 │     │  • Posts approval card                  │       │
│  │                 │     │  • Adds domain to alias on approve      │       │
│  └─────────────────┘     └──────────────┬──────────────────────────┘       │
│                                          │                                  │
└──────────────────────────────────────────│──────────────────────────────────┘
                                           │ SSH :22
                    ┌──────────────────────┴──────────────────────┐
                    │                                             │
                    ▼ Firewall Alias API                          ▼
┌───────────────────────────────────────┐   ┌─────────────────────────────────┐
│ OPNsense                              │   │ LANLLM (192.168.12.0/23)        │
│                                       │   │                                 │
│  Alias: llm_approved_domains          │   │  ┌───────────────────────────┐  │
│    - api.anthropic.com (pre-approved) │   │  │ VM                        │  │
│    - pypi.org (added on approval)     │   │  │  └─ Claude Code container │  │
│                                       │   │  │      ✅ web search works  │  │
│  Rules (LANLLM):                      │   │  │      🔒 pip/npm blocked   │  │
│    1. PASS → OPNsense:53 (DNS)        │   │  └───────────────────────────┘  │
│    2. PASS → llm_approved_domains:443 │   │              │                  │
│    3. BLOCK → any                     │   │              │ outbound :443    │
│                                       │   │              ▼                  │
│  Rules (LAN):                         │◀──│────── llm_approved_domains      │
│    + PASS → LANLLM:22 (SSH)           │   │                                 │
└───────────────────────────────────────┘   └─────────────────────────────────┘
```

**Components:**
- **Session Manager**: Chat relay + approval cards + OPNsense Firewall API
- **OPNsense**: Core firewall with Host alias (no plugins)

## Project Structure

```
claude-approval-system/
├── session-manager/
│   ├── Cargo.toml
│   ├── Dockerfile
│   └── src/
│       ├── main.rs
│       ├── config.rs
│       ├── opnsense.rs
│       ├── mattermost.rs
│       └── container.rs
├── container-image/
│   ├── Dockerfile
│   └── CLAUDE.md
└── kubernetes/
    └── session-manager/
        ├── deployment.yaml
        └── secrets.yaml
```

---

### Dockerfile 

```dockerfile
# session-manager/Dockerfile
FROM rust:1.84-slim AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates openssh-client && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/session-manager /usr/local/bin/

EXPOSE 8000

CMD ["session-manager"]
```

---

## Session Manager

### Cargo.toml

```toml
# session-manager/Cargo.toml
[package]
name = "session-manager"
version = "0.1.0"
edition = "2024"

[dependencies]
tokio = { version = "1", features = ["full"] }
axum = { version = "0.8", features = ["macros"] }
reqwest = { version = "0.13", features = ["json"] }
tokio-tungstenite = { version = "0.28", features = ["rustls-tls-webpki-roots"] }
futures-util = "0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
russh = "0.57"
russh-keys = "0.49"
uuid = { version = "1", features = ["v4"] }
regex = "1"
dashmap = "6"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
anyhow = "1"
thiserror = "2"
config = "0.15"
```

### Config

All configuration is via environment variables with `SM_` prefix.

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `SM_MATTERMOST_URL` | Yes | - | Mattermost server URL |
| `SM_MATTERMOST_TOKEN` | Yes | - | Bot token |
| `SM_VM_HOST` | Yes | - | SSH host for container VM |
| `SM_VM_USER` | No | `claude` | SSH user |
| `SM_VM_SSH_KEY_PATH` | No | `/secrets/ssh/id_ed25519` | Path to SSH key |
| `SM_CONTAINER_RUNTIME` | No | `podman` | Container runtime |
| `SM_CONTAINER_NETWORK` | No | `isolated` | Container network |
| `SM_CONTAINER_IMAGE` | No | `claude-code:latest` | Container image |
| `SM_CLAUDE_COMMAND` | No | `claude --dangerously-skip-permissions` | Claude command to run |
| `SM_OPNSENSE_URL` | Yes | - | OPNsense API URL |
| `SM_OPNSENSE_KEY` | Yes | - | API key |
| `SM_OPNSENSE_SECRET` | Yes | - | API secret |
| `SM_OPNSENSE_ALIAS` | No | `llm_approved_domains` | Firewall alias name |
| `SM_OPNSENSE_VERIFY_TLS` | No | `false` | Verify TLS certs |
| `SM_OPNSENSE_TIMEOUT_SECS` | No | `30` | HTTP timeout |
| `SM_CALLBACK_URL` | No | `http://session-manager:8000/callback` | Callback URL |
| `SM_LISTEN_ADDR` | No | `0.0.0.0:8000` | Server bind address |
| `SM_BOT_TRIGGER` | No | `@claude` | Bot mention trigger |
| `SM_PROJECTS` | No | `{}` | Project name to path mapping |

```rust
// session-manager/src/config.rs
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub mattermost_url: String,
    pub mattermost_token: String,

    pub vm_host: String,
    #[serde(default = "default_vm_user")]
    pub vm_user: String,
    #[serde(default = "default_ssh_key_path")]
    pub vm_ssh_key_path: String,

    #[serde(default = "default_container_runtime")]
    pub container_runtime: String,
    #[serde(default = "default_container_network")]
    pub container_network: String,
    #[serde(default = "default_container_image")]
    pub container_image: String,
    #[serde(default = "default_claude_command")]
    pub claude_command: String,

    pub opnsense_url: String,
    pub opnsense_key: String,
    pub opnsense_secret: String,
    #[serde(default = "default_opnsense_alias")]
    pub opnsense_alias: String,
    #[serde(default = "default_opnsense_verify_tls")]
    pub opnsense_verify_tls: bool,
    #[serde(default = "default_opnsense_timeout")]
    pub opnsense_timeout_secs: u64,

    #[serde(default)]
    pub projects: HashMap<String, String>,
    #[serde(default = "default_callback_url")]
    pub callback_url: String,
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_bot_trigger")]
    pub bot_trigger: String,
}

fn default_vm_user() -> String { "claude".into() }
fn default_ssh_key_path() -> String { "/secrets/ssh/id_ed25519".into() }
fn default_container_runtime() -> String { "podman".into() }
fn default_container_network() -> String { "isolated".into() }
fn default_container_image() -> String { "claude-code:latest".into() }
fn default_claude_command() -> String { "claude --dangerously-skip-permissions".into() }
fn default_opnsense_alias() -> String { "llm_approved_domains".into() }
fn default_opnsense_verify_tls() -> bool { false }
fn default_opnsense_timeout() -> u64 { 30 }
fn default_callback_url() -> String { "http://session-manager:8000/callback".into() }
fn default_listen_addr() -> String { "0.0.0.0:8000".into() }
fn default_bot_trigger() -> String { "@claude".into() }

static SETTINGS: OnceLock<Settings> = OnceLock::new();

pub fn settings() -> &'static Settings {
    SETTINGS.get_or_init(|| {
        config::Config::builder()
            .add_source(config::Environment::with_prefix("SM"))
            .build()
            .expect("Failed to build config")
            .try_deserialize()
            .expect("Failed to deserialize config")
    })
}
```

### OPNsense Client

```rust
// session-manager/src/opnsense.rs
use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;

use crate::config::settings;

#[derive(Clone)]
pub struct OPNsense {
    client: Client,
    base_url: String,
    alias: String,
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
        let mut domains = self.get_domains().await?;

        if domains.contains(&domain.to_string()) {
            return Ok(false);
        }

        domains.push(domain.to_string());
        self.set_domains(&domains).await?;
        self.reconfigure().await?;

        Ok(true)
    }

    pub async fn remove_domain(&self, domain: &str) -> Result<bool> {
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
```

### Mattermost Client

```rust
// session-manager/src/mattermost.rs
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
        write.send(Message::Text(auth.to_string())).await?;

        while let Some(msg) = read.next().await {
            let Ok(Message::Text(text)) = msg else { continue };
            let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) else { continue };

            if data.get("event").and_then(|e| e.as_str()) == Some("posted") {
                if let Some(post_str) = data["data"]["post"].as_str() {
                    if let Ok(post) = serde_json::from_str::<Post>(post_str) {
                        if post.user_id != self.bot_user_id {
                            let _ = tx.send(post).await;
                        }
                    }
                }
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

    pub async fn post_approval(&self, channel_id: &str, request_id: &str, domain: &str) -> Result<String> {
        let s = settings();

        let props = serde_json::json!({
            "attachments": [{
                "color": "#FFA500",
                "text": format!("🔒 **Network Request:** `{}`", domain),
                "actions": [
                    {
                        "id": "approve",
                        "name": "✅ Approve",
                        "integration": {
                            "url": s.callback_url,
                            "context": { "action": "approve", "request_id": request_id }
                        }
                    },
                    {
                        "id": "deny",
                        "name": "❌ Deny",
                        "integration": {
                            "url": s.callback_url,
                            "context": { "action": "deny", "request_id": request_id }
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
```

### Container Manager

```rust
// session-manager/src/container.rs
use anyhow::Result;
use dashmap::DashMap;
use russh::*;
use russh_keys::*;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::config::settings;

pub struct ContainerManager {
    sessions: DashMap<String, Session>,
}

struct Session {
    name: String,
    stdin_tx: mpsc::Sender<String>,
}

impl ContainerManager {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    pub async fn start(
        &self,
        session_id: &str,
        project_path: &str,
        output_tx: mpsc::Sender<String>,
    ) -> Result<String> {
        let s = settings();
        let name = format!("claude-{}", &session_id[..8]);

        // Start container via SSH
        let start_cmd = format!(
            "ssh {}@{} -i {} '{} run -d --name {} --network {} -v {}:/workspace -e ANTHROPIC_API_KEY=$ANTHROPIC_API_KEY {} sleep infinity'",
            s.vm_user, s.vm_host, s.vm_ssh_key_path,
            s.container_runtime, name, s.container_network, project_path, s.container_image
        );
        Command::new("sh").arg("-c").arg(&start_cmd).output().await?;

        // Start interactive session - we specify the claude command here
        // (container image has no ENTRYPOINT, giving us control over flags)
        let exec_cmd = format!(
            "ssh -tt {}@{} -i {} '{} exec -i {} claude --dangerously-skip-permissions'",
            s.vm_user, s.vm_host, s.vm_ssh_key_path, s.container_runtime, name
        );

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&exec_cmd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let stdin = child.stdin.take().expect("Failed to get stdin");
        let stdout = child.stdout.take().expect("Failed to get stdout");

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(32);

        // Stdin writer task
        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some(text) = stdin_rx.recv().await {
                let _ = stdin.write_all(text.as_bytes()).await;
                let _ = stdin.write_all(b"\n").await;
                let _ = stdin.flush().await;
            }
        });

        // Stdout reader task
        let reader = BufReader::new(stdout);
        tokio::spawn(async move {
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if output_tx.send(line).await.is_err() {
                    break;
                }
            }
        });

        self.sessions.insert(
            session_id.to_string(),
            Session {
                name: name.clone(),
                stdin_tx,
            },
        );

        Ok(name)
    }

    pub async fn send(&self, session_id: &str, text: &str) -> Result<()> {
        if let Some(session) = self.sessions.get(session_id) {
            session.stdin_tx.send(text.to_string()).await?;
        }
        Ok(())
    }

    pub async fn stop(&self, session_id: &str) -> Result<()> {
        if let Some((_, session)) = self.sessions.remove(session_id) {
            let s = settings();
            let stop_cmd = format!(
                "ssh {}@{} -i {} '{} rm -f {}'",
                s.vm_user, s.vm_host, s.vm_ssh_key_path, s.container_runtime, session.name
            );
            Command::new("sh").arg("-c").arg(&stop_cmd).output().await?;
        }
        Ok(())
    }
}
```

### Main

```rust
// session-manager/src/main.rs
mod config;
mod container;
mod mattermost;
mod opnsense;

use anyhow::Result;
use axum::{extract::State, routing::post, Json, Router};
use dashmap::DashMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config::settings;
use crate::container::ContainerManager;
use crate::mattermost::{Mattermost, Post};
use crate::opnsense::OPNsense;

struct AppState {
    mm: Mattermost,
    containers: ContainerManager,
    opnsense: OPNsense,
    sessions: DashMap<String, String>,                    // channel_id → session_id
    pending: DashMap<String, PendingRequest>,             // request_id → PendingRequest
}

#[derive(Clone)]
struct PendingRequest {
    channel_id: String,
    session_id: String,
    domain: String,
    post_id: String,
}

#[derive(Deserialize)]
struct CallbackPayload {
    context: CallbackContext,
    user_name: String,
}

#[derive(Deserialize)]
struct CallbackContext {
    action: String,
    request_id: String,
}

#[derive(Serialize)]
struct CallbackResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    ephemeral_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    update: Option<serde_json::Value>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mm = Mattermost::new().await?;
    let opnsense = OPNsense::new()?;
    let containers = ContainerManager::new();

    let state = Arc::new(AppState {
        mm,
        containers,
        opnsense,
        sessions: DashMap::new(),
        pending: DashMap::new(),
    });

    // Start message listener
    let (post_tx, post_rx) = mpsc::channel::<Post>(100);
    let state_clone = state.clone();
    tokio::spawn(async move {
        let _ = state_clone.mm.listen(post_tx).await;
    });

    // Start message handler
    let state_clone = state.clone();
    tokio::spawn(async move {
        handle_messages(state_clone, post_rx).await;
    });

    // Start HTTP server for callbacks
    let app = Router::new()
        .route("/callback", post(handle_callback))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await?;
    tracing::info!("Listening on :8000");
    axum::serve(listener, app).await?;

    Ok(())
}

async fn handle_messages(state: Arc<AppState>, mut rx: mpsc::Receiver<Post>) {
    let network_re = Regex::new(r"\[NETWORK_REQUEST:\s*([^\]]+)\]").unwrap();

    while let Some(post) = rx.recv().await {
        let text = post.message.trim();
        let channel_id = &post.channel_id;

        if text.starts_with("@claude") {
            let text = text.trim_start_matches("@claude").trim();

            // Start session
            if let Some(project) = text.strip_prefix("start ").map(|s| s.trim()) {
                let s = settings();
                let Some(project_path) = s.projects.get(project) else {
                    let _ = state.mm.post(channel_id, &format!(
                        "Unknown project. Available: {}",
                        s.projects.keys().cloned().collect::<Vec<_>>().join(", ")
                    )).await;
                    continue;
                };

                let session_id = Uuid::new_v4().to_string();
                let _ = state.mm.post(channel_id, &format!("🚀 Starting **{}**...", project)).await;

                let (output_tx, output_rx) = mpsc::channel::<String>(100);
                match state.containers.start(&session_id, project_path, output_tx).await {
                    Ok(name) => {
                        state.sessions.insert(channel_id.clone(), session_id.clone());
                        let _ = state.mm.post(channel_id, &format!("✅ Ready. Container: `{}`", name)).await;

                        // Start output streaming
                        let state_clone = state.clone();
                        let channel_id_clone = channel_id.clone();
                        let session_id_clone = session_id.clone();
                        let network_re_clone = network_re.clone();
                        tokio::spawn(async move {
                            stream_output(state_clone, channel_id_clone, session_id_clone, output_rx, network_re_clone).await;
                        });
                    }
                    Err(e) => {
                        let _ = state.mm.post(channel_id, &format!("❌ Failed to start: {}", e)).await;
                    }
                }
                continue;
            }

            // Stop session
            if text == "stop" {
                if let Some((_, session_id)) = state.sessions.remove(channel_id) {
                    let _ = state.containers.stop(&session_id).await;
                }
                let _ = state.mm.post(channel_id, "✅ Stopped.").await;
                continue;
            }
        }

        // Forward to Claude
        if let Some(session_id) = state.sessions.get(channel_id) {
            let _ = state.containers.send(&session_id, text).await;
        }
    }
}

async fn stream_output(
    state: Arc<AppState>,
    channel_id: String,
    session_id: String,
    mut rx: mpsc::Receiver<String>,
    network_re: Regex,
) {
    while let Some(line) = rx.recv().await {
        if let Some(caps) = network_re.captures(&line) {
            let domain = caps[1].trim();
            handle_network_request(&state, &channel_id, &session_id, domain).await;
        } else {
            let _ = state.mm.post(&channel_id, &format!("```\n{}\n```", line)).await;
        }
    }

    state.sessions.remove(&channel_id);
    let _ = state.mm.post(&channel_id, "⚠️ Session ended.").await;
}

async fn handle_network_request(state: &AppState, channel_id: &str, session_id: &str, domain: &str) {
    let request_id = Uuid::new_v4().to_string();

    match state.mm.post_approval(channel_id, &request_id, domain).await {
        Ok(post_id) => {
            state.pending.insert(
                request_id,
                PendingRequest {
                    channel_id: channel_id.to_string(),
                    session_id: session_id.to_string(),
                    domain: domain.to_string(),
                    post_id,
                },
            );
        }
        Err(e) => {
            tracing::error!("Failed to post approval: {}", e);
        }
    }
}

async fn handle_callback(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CallbackPayload>,
) -> Json<CallbackResponse> {
    let request_id = &payload.context.request_id;

    let Some((_, req)) = state.pending.remove(request_id) else {
        return Json(CallbackResponse {
            ephemeral_text: Some("Request expired.".into()),
            update: None,
        });
    };

    if payload.context.action == "approve" {
        let _ = state.opnsense.add_domain(&req.domain).await;
        let _ = state.mm.update_post(&req.post_id, &format!("✅ `{}` approved by @{}", req.domain, payload.user_name)).await;
        let _ = state.containers.send(&req.session_id, &format!("[NETWORK_APPROVED: {}]", req.domain)).await;
    } else {
        let _ = state.mm.update_post(&req.post_id, &format!("❌ `{}` denied by @{}", req.domain, payload.user_name)).await;
        let _ = state.containers.send(&req.session_id, &format!("[NETWORK_DENIED: {}]", req.domain)).await;
    }

    Json(CallbackResponse {
        ephemeral_text: None,
        update: Some(serde_json::json!({ "message": "" })),
    })
}
```

---

## What You See

```
#claude-client-a

@james: @claude start client-a
🚀 Starting **client-a**...
✅ Ready. Container: `claude-abc12345`

@james: research OAuth2 best practices then implement it

```
I'll research OAuth2 best practices...

[web search results - no approval needed, api.anthropic.com already allowed]

Based on current best practices, I'll use authlib...

$ pip install authlib
ERROR: Connection timed out to pypi.org

[NETWORK_REQUEST: pypi.org]
```

🔒 **Network Request:** `pypi.org`
[✅ Approve] [❌ Deny]

✅ `pypi.org` approved by @james

```
[NETWORK_APPROVED: pypi.org]

$ pip install authlib
Successfully installed authlib-1.3.0

Implementing OAuth2...
```

@james: @claude stop
✅ Stopped.
```

---

## How It Works

1. **Container makes request** → `pip install pytest` tries to reach pypi.org
2. **Firewall blocks** → pypi.org not in `llm_approved_domains` alias
3. **Claude sees timeout** → outputs `[NETWORK_REQUEST: pypi.org]`
4. **Session Manager intercepts** → posts approval card to Mattermost
5. **You click Approve** → Session Manager calls OPNsense API
6. **API adds domain** → `llm_approved_domains` alias now includes pypi.org (hardcoded, cannot modify other aliases)
7. **OPNsense resolves** → pypi.org → IP addresses
8. **Firewall allows** → rule 2 now matches
9. **Claude retries** → success

---

## Container Image (Completed)

```dockerfile
# container-image/Dockerfile
FROM node:20-slim

RUN npm install -g @anthropic-ai/claude-code
RUN apt-get update && apt-get install -y git curl python3 python3-pip && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
COPY CLAUDE.md /CLAUDE.md

# No ENTRYPOINT - session-manager specifies the command when starting containers
```

```markdown
# container-image/CLAUDE.md

## Web Search

Web search works normally - use it freely for research.

## Network Access (Package Installs, Git, etc.)

Direct network access (pip, npm, git clone) is blocked by default.
When you get a connection error (timeout, connection refused, etc.):

1. Output a network request on its own line:
   ```
   [NETWORK_REQUEST: domain.com]
   ```

2. Wait for the response:
   - `[NETWORK_APPROVED: domain.com]` → retry your command
   - `[NETWORK_DENIED: domain.com]` → find alternative approach

Example:
```
$ pip install pytest
ERROR: Could not connect to pypi.org

[NETWORK_REQUEST: pypi.org]

[NETWORK_APPROVED: pypi.org]

$ pip install pytest
Successfully installed pytest-8.0.0
```

Common domains you may need to request:
- pypi.org, files.pythonhosted.org - pip
- registry.npmjs.org - npm
- github.com, objects.githubusercontent.com - git
- crates.io - cargo
```

---

## OPNsense Setup (Completed)

### 1. Create Alias

```
Firewall → Aliases → Add

Name: llm_approved_domains
Type: Host(s)
Content: api.anthropic.com
Description: Domains approved for LLM containers (managed by session-manager)

Save
```

The alias automatically resolves hostnames to IPs and updates them periodically.

### 2. Create Firewall Rules

**LANLLM Rules** (outbound from container VM):
```
Firewall → Rules → LANLLM

Rule 1 - Allow DNS:
  Action: Pass
  Source: LANLLM net
  Destination: This Firewall
  Dest Port: 53
  Protocol: TCP/UDP
  Description: Allow DNS to OPNsense

Rule 2 - Allow approved domains:
  Action: Pass
  Source: LANLLM net
  Destination: llm_approved_domains (alias)
  Dest Port: 443
  Protocol: TCP
  Description: Allow HTTPS to LLM approved domains

Rule 3 - Block all else:
  Action: Block
  Source: LANLLM net
  Destination: any
  Description: Default deny

Apply Changes
```

**LAN Rules** (allow session-manager to reach container VM):
```
Firewall → Rules → LAN

Rule - Allow SSH to LANLLM:
  Action: Pass
  Source: LAN net (or specific session-manager IP)
  Destination: LANLLM net
  Dest Port: 22
  Protocol: TCP
  Description: Allow session-manager SSH to container VM

Apply Changes
```

### 3. Create API User

```
System → Access → Users → Add

Username: claude-session-manager
Password: (generate)

API Keys → + (create key)
→ Download credentials (key + secret)

System → Access → Groups → Add
Name: firewall-api
Member: claude-session-manager
Privileges:
  - Firewall: Alias: Edit
  - Firewall: Rules: Edit
```

**Note:** While the `claude-session-manager` user has privileges to modify any firewall alias,
the application hardcodes `llm_approved_domains` as the only alias it will ever modify.
This provides defense-in-depth - even if the application is compromised, it cannot modify
other firewall aliases.

