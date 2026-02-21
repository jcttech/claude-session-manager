use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub mattermost_url: String,
    pub mattermost_token: String,
    /// Mattermost team ID (required for channel creation and sidebar categories)
    pub mattermost_team_id: String,

    /// Sidebar category display name for auto-created channels
    #[serde(default = "default_channel_category")]
    pub channel_category: String,

    /// Default GitHub org — allows `start repo-name` instead of `start org/repo`
    #[serde(default)]
    pub default_org: Option<String>,

    pub vm_host: String,
    #[serde(default = "default_vm_user")]
    pub vm_user: String,
    /// SSH private key content (preferred for Kubernetes)
    /// If set, takes priority over vm_ssh_key_path
    #[serde(default)]
    pub vm_ssh_key: Option<String>,
    /// Path to SSH private key file (fallback if vm_ssh_key not set)
    #[serde(default = "default_ssh_key_path")]
    pub vm_ssh_key_path: String,

    #[serde(default = "default_container_runtime")]
    pub container_runtime: String,
    /// Fallback container image used when a repo has no devcontainer.json
    #[serde(default = "default_container_image")]
    pub container_image: String,
    /// Fallback container network used when a repo has no devcontainer.json
    #[serde(default = "default_container_network")]
    pub container_network: String,
    /// Timeout for devcontainer up operations (image pull + feature install + lifecycle hooks)
    #[serde(default = "default_devcontainer_timeout_secs")]
    pub devcontainer_timeout_secs: u64,

    /// Message count threshold for auto-compacting orchestrator sessions (0 = disabled)
    #[serde(default = "default_orchestrator_compact_threshold")]
    pub orchestrator_compact_threshold: i32,

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

    /// Base path on VM where GitHub repos are cloned
    #[serde(default = "default_repos_base_path")]
    pub repos_base_path: String,
    /// Base path on VM where worktrees are created
    #[serde(default = "default_worktrees_path")]
    pub worktrees_path: String,
    /// Whether to auto-pull repos before starting sessions
    #[serde(default)]
    pub auto_pull: bool,

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

    /// Database connection pool size
    #[serde(default = "default_database_pool_size")]
    pub database_pool_size: u32,

    /// SSH command timeout in seconds
    #[serde(default = "default_ssh_timeout_secs")]
    pub ssh_timeout_secs: u64,

    /// Rate limit: requests per second per IP
    #[serde(default = "default_rate_limit_rps")]
    pub rate_limit_rps: u64,

    /// Rate limit: burst size
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u32,

    /// Optional list of Mattermost usernames allowed to approve/deny requests
    /// If empty, any user can approve/deny (default behavior)
    #[serde(default)]
    pub allowed_approvers: Vec<String>,

    /// Maximum concurrent sessions per container (0 = unlimited)
    #[serde(default = "default_container_max_sessions")]
    pub container_max_sessions: i32,

    /// Idle timeout in seconds before tearing down empty containers (0 = no auto-teardown)
    #[serde(default = "default_container_idle_timeout_secs")]
    pub container_idle_timeout_secs: u64,

    /// Seconds of inactivity before posting a liveness warning in the thread (0 = disabled)
    #[serde(default = "default_session_liveness_timeout_secs")]
    pub session_liveness_timeout_secs: u64,

    /// Starting port number for gRPC agent worker connections
    #[serde(default = "default_grpc_port_start")]
    pub grpc_port_start: u16,
}

fn default_vm_user() -> String { "claude".into() }
fn default_ssh_key_path() -> String { "/secrets/ssh/id_ed25519".into() }
fn default_container_runtime() -> String { "podman".into() }
fn default_container_image() -> String { "claude-code:latest".into() }
fn default_container_network() -> String { "isolated".into() }
fn default_devcontainer_timeout_secs() -> u64 { 120 }
fn default_orchestrator_compact_threshold() -> i32 { 50 }
fn default_channel_category() -> String { "CLAUDE-SESSIONS".into() }
fn default_opnsense_alias() -> String { "llm_approved_domains".into() }
fn default_opnsense_verify_tls() -> bool { true }
fn default_opnsense_timeout() -> u64 { 30 }
fn default_callback_url() -> String { "http://session-manager:8000/callback".into() }
fn default_listen_addr() -> String { "0.0.0.0:8000".into() }
fn default_bot_trigger() -> String { "@claude".into() }
fn default_rate_limit_rps() -> u64 { 10 }
fn default_rate_limit_burst() -> u32 { 20 }
fn default_database_pool_size() -> u32 { 5 }
fn default_ssh_timeout_secs() -> u64 { 30 }
fn default_repos_base_path() -> String { "/home/claude/repos".into() }
fn default_worktrees_path() -> String { "/home/claude/worktrees".into() }
fn default_container_max_sessions() -> i32 { 5 }
fn default_container_idle_timeout_secs() -> u64 { 1800 }
fn default_session_liveness_timeout_secs() -> u64 { 120 }
fn default_grpc_port_start() -> u16 { 50051 }

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
