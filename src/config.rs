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
    /// Shared volume for Claude config (authentication)
    /// If mounted and authenticated, uses stored credentials
    /// Falls back to ANTHROPIC_API_KEY if volume is empty
    #[serde(default = "default_claude_config_volume")]
    pub claude_config_volume: String,
    /// Path inside container where Claude config is mounted
    #[serde(default = "default_claude_config_path")]
    pub claude_config_path: String,

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

    /// Shared secret for HMAC signature verification on callbacks
    pub callback_secret: String,

    /// PostgreSQL connection URL
    pub database_url: String,

    /// Rate limit: requests per second per IP
    #[serde(default = "default_rate_limit_rps")]
    pub rate_limit_rps: u64,

    /// Rate limit: burst size
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u32,
}

fn default_vm_user() -> String { "claude".into() }
fn default_ssh_key_path() -> String { "/secrets/ssh/id_ed25519".into() }
fn default_container_runtime() -> String { "podman".into() }
fn default_container_network() -> String { "isolated".into() }
fn default_container_image() -> String { "claude-code:latest".into() }
fn default_claude_command() -> String { "claude --dangerously-skip-permissions".into() }
fn default_claude_config_volume() -> String { "claude-config-shared".into() }
fn default_claude_config_path() -> String { "/home/vscode/.claude".into() }
fn default_opnsense_alias() -> String { "llm_approved_domains".into() }
fn default_opnsense_verify_tls() -> bool { true }
fn default_opnsense_timeout() -> u64 { 30 }
fn default_callback_url() -> String { "http://session-manager:8000/callback".into() }
fn default_listen_addr() -> String { "0.0.0.0:8000".into() }
fn default_bot_trigger() -> String { "@claude".into() }
fn default_rate_limit_rps() -> u64 { 10 }
fn default_rate_limit_burst() -> u32 { 20 }

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
