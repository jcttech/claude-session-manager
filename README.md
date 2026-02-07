# Claude Session Manager

Chat with Claude Code through Mattermost with network-controlled security. Web search works freely, package installs require human approval via OPNsense firewall.

## Overview

This system lets you interact with Claude Code through Mattermost while maintaining strict network control:

- **Web search**: Works freely (Anthropic API pre-approved)
- **Package installs** (pip, npm, cargo): Blocked until approved
- **Approval workflow**: Click a button in Mattermost to allow specific domains
- **Thread-based sessions**: Each session lives in its own Mattermost thread for clean isolation
- **Orchestrator mode**: Multi-agent workflows where an orchestrator spawns worker/reviewer sessions
- **Devcontainer support**: Uses `devcontainer up` CLI to honor full devcontainer.json spec
- **Context management**: In-thread commands to compact, clear, restart, and inspect sessions
- **Session health tracking**: Message counts, compaction counts, and idle time per session

## Architecture

```
┌────────────────────────────────────────────────────────────────────────────────┐
│ LAN (Kubernetes cluster)                                                       │
│                                                                                │
│  ┌─────────────────┐     ┌──────────────────────────────────────┐             │
│  │ Mattermost      │◄───►│ Session Manager                      │             │
│  │                 │     │  • Thread-based session routing       │             │
│  │                 │     │  • Relays chat via SSH to containers  │             │
│  │                 │     │  • Detects [NETWORK_REQUEST: ...]     │             │
│  │                 │     │  • Posts approval card in thread      │             │
│  │                 │     │  • Adds domain to alias on approve    │             │
│  │                 │     │  • Orchestrator multi-agent support   │             │
│  └─────────────────┘     └──────┬─────────────────┬─────────────┘             │
│                                 │                 │                            │
│                          ┌──────┘                 └──────┐                    │
│                          ▼                                ▼                    │
│               ┌─────────────────────┐        ┌──────────────────────┐        │
│               │ PostgreSQL          │        │ OPNsense             │        │
│               │  • Sessions         │        │  Alias API           │        │
│               │  • Project channels │        │  llm_approved_domains│        │
│               │  • Pending requests │        │  Firewall rules      │        │
│               │  • Audit log        │        └──────────────────────┘        │
│               └─────────────────────┘                                        │
└──────────────────────────────────────────────────────────────────────────────┘
                                  │ SSH :22
                    ┌─────────────┘
                    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│ LANLLM VM                                                                    │
│                                                                              │
│  ┌─────────────────────────┐  ┌─────────────────────────┐                   │
│  │ Claude Code container 1 │  │ Claude Code container 2 │                   │
│  │  (standard session)     │  │  (worker session)       │                   │
│  │  ✅ web search works    │  │  🌿 isolated worktree    │                   │
│  │  🔒 pip/npm blocked     │  │  🔒 pip/npm blocked     │                   │
│  └─────────────────────────┘  └─────────────────────────┘                   │
│              │                            │                                  │
│              │ outbound :443              │ outbound :443                    │
│              ▼                            ▼                                  │
│        llm_approved_domains        llm_approved_domains                     │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Session Architecture

### Thread-Based Routing

Each session is anchored to a Mattermost thread:

- **Starting a session** creates a root post in the project channel, becoming the thread anchor
- **Thread replies** are routed to the session's container via `(channel_id, thread_id)` lookup
- **Top-level messages** in a channel are routed to the single active non-worker session, or prompt users to reply in a specific thread if multiple sessions are active
- **Bot commands** (`@claude stop`, `@claude status`) are always processed first, before any message forwarding

### Session Types

| Type | Description | Created by |
|------|-------------|------------|
| `standard` | Normal interactive session | `@claude start <repo>` |
| `orchestrator` | Multi-agent coordinator, parses output markers to spawn child sessions | `@claude orchestrate <repo>` |
| `worker` | Task executor spawned by orchestrator, uses isolated worktree | Orchestrator `[CREATE_SESSION:]` marker |
| `reviewer` | Code reviewer spawned by orchestrator, uses isolated worktree | Orchestrator `[CREATE_REVIEWER:]` marker |

### Orchestrator Pattern

Orchestrator sessions can dynamically manage child sessions through output markers:

```
[CREATE_SESSION: org/repo@branch]     → Spawn a worker session
[CREATE_REVIEWER: org/repo@branch]    → Spawn a reviewer session
[SESSION_STATUS]                       → Get all active sessions as JSON
[STOP_SESSION: <session-id-prefix>]    → Stop a child session
```

The orchestrator receives structured responses:
```
[SESSION_CREATED: <id> <type> <path> <thread_id>]
[SESSION_ENDED: <id>]
[SESSION_ERROR: <message>]
[SESSIONS: <json>]
```

### Session Lifecycle

1. **Start**: Clone/worktree repo → `devcontainer up` (or generate fallback config) → exec Claude in container → create thread → persist to DB
2. **Active**: Forward messages via stdin, stream stdout to thread, handle network requests, track health metrics
3. **Cleanup**: Atomic claim guard → remove container → release repo lock → cleanup worktree → delete from DB → decrement metrics → notify parent (if child session)

Cleanup uses an atomic `claim_session()` guard to prevent double-cleanup races between explicit stop commands and natural stream-end events.

## Prerequisites

### OPNsense Firewall

1. **Create firewall alias** `llm_approved_domains` with `api.anthropic.com`
2. **Create LANLLM rules**: DNS allowed, approved domains on 443, block all else
3. **Create API user** `claude-session-manager` with firewall alias privileges

### LANLLM VM

1. **Container runtime**: podman or docker installed
2. **SSH access**: User with key-based authentication
3. **Environment**: `ANTHROPIC_API_KEY` set for Claude Code
4. **Environment**: `GH_TOKEN` set for private GitHub repos (optional)

### PostgreSQL Database

A PostgreSQL instance is required for session state, project channel mappings, pending requests, and audit logging. Tables are automatically created on startup.

### Devcontainer Support

The session manager uses the `devcontainer` CLI to start containers, honoring the full devcontainer.json specification (mounts, containerEnv, features, runArgs, lifecycle hooks). It runs `devcontainer up --docker-path <runtime> --workspace-folder <path>` and parses the JSON output to get the container ID.

Projects should provide a `.devcontainer/devcontainer.json` (or `.devcontainer.json`). JSONC format is supported (both `//` and `/* */` comments).

```jsonc
{
    // This comment is supported
    "image": "ghcr.io/myorg/devcontainer-rust:latest",
    "mounts": [
        "source=claude-config-shared,target=/home/vscode/.claude,type=volume",
        "source=claude-mem-shared,target=/home/vscode/.claude-mem,type=volume"
    ],
    "containerEnv": {
        "ANTHROPIC_API_KEY": "${localEnv:ANTHROPIC_API_KEY}"
    },
    "runArgs": ["--network=isolated"]
}
```

If no devcontainer.json is found, a default one is auto-generated using `SM_CONTAINER_IMAGE` and `SM_CONTAINER_NETWORK`, with mounts for `claude-config-shared` and `claude-mem-shared` volumes.

## Deployment

### 1. Build Session Manager

```bash
cargo build --release
```

Or use the Docker image:

```bash
docker build -t session-manager .
```

### 2. Configure Environment

```bash
# Required
export SM_MATTERMOST_URL=https://mattermost.example.com
export SM_MATTERMOST_TOKEN=your-bot-token
export SM_MATTERMOST_TEAM_ID=your-team-id
export SM_VM_HOST=192.168.12.10
export SM_OPNSENSE_URL=https://opnsense.example.com
export SM_OPNSENSE_KEY=your-api-key
export SM_OPNSENSE_SECRET=your-api-secret
export SM_CALLBACK_SECRET=your-hmac-secret
export SM_DATABASE_URL=postgres://user:pass@localhost/session_manager

# Optional (with defaults)
export SM_VM_USER=claude
export SM_VM_SSH_KEY="$(cat ~/.ssh/id_ed25519)"  # Preferred for Kubernetes
# OR: export SM_VM_SSH_KEY_PATH=/secrets/ssh/id_ed25519  # Fallback: file path
export SM_CONTAINER_RUNTIME=podman
export SM_CONTAINER_IMAGE=claude-code:latest
export SM_DEFAULT_ORG=myorg                        # Allows `start repo` shorthand
export SM_CHANNEL_CATEGORY="Claude Sessions"       # Sidebar category name
export SM_REPOS_BASE_PATH=/home/claude/repos
export SM_WORKTREES_PATH=/home/claude/worktrees
export SM_AUTO_PULL=false
```

### 3. Deploy to Kubernetes

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: session-manager-secrets
stringData:
  SM_MATTERMOST_TOKEN: "your-bot-token"
  SM_OPNSENSE_KEY: "your-api-key"
  SM_OPNSENSE_SECRET: "your-api-secret"
  SM_CALLBACK_SECRET: "your-hmac-secret"
  SM_DATABASE_URL: "postgres://user:pass@postgres:5432/session_manager"
  SM_VM_SSH_KEY: |
    -----BEGIN OPENSSH PRIVATE KEY-----
    ...your-private-key...
    -----END OPENSSH PRIVATE KEY-----
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: session-manager
spec:
  replicas: 1
  selector:
    matchLabels:
      app: session-manager
  template:
    spec:
      containers:
      - name: session-manager
        image: session-manager:latest
        ports:
        - containerPort: 8000
        envFrom:
        - secretRef:
            name: session-manager-secrets
        env:
        - name: SM_MATTERMOST_URL
          value: "https://mattermost.example.com"
        - name: SM_MATTERMOST_TEAM_ID
          value: "your-team-id"
        - name: SM_VM_HOST
          value: "192.168.12.10"
        - name: SM_OPNSENSE_URL
          value: "https://opnsense.example.com"
        livenessProbe:
          httpGet:
            path: /health
            port: 8000
          initialDelaySeconds: 10
          periodSeconds: 30
        readinessProbe:
          httpGet:
            path: /health
            port: 8000
          initialDelaySeconds: 5
          periodSeconds: 10
```

## Usage

### Start a Session

In Mattermost, mention the bot with a GitHub repository:

```
@claude start org/repo                    # Clone and use main repo
@claude start org/repo@branch             # Checkout specific branch
@claude start org/repo --worktree         # Create isolated worktree (auto-named)
@claude start org/repo --worktree=feature # Create named worktree
@claude start org/repo@branch --worktree  # Worktree from specific branch
@claude start myrepo                      # Uses SM_DEFAULT_ORG if configured
```

The bot automatically creates a Mattermost channel for the project (if it doesn't exist) and starts the session as a thread within that channel.

**Concurrency:** If the main clone is already in use by another session, you'll be prompted to use `--worktree` for an isolated working directory.

### Start an Orchestrator

For multi-agent workflows:

```
@claude orchestrate org/repo              # Start orchestrator session
```

The orchestrator can spawn and manage worker/reviewer sessions through structured output markers.

### Chat with Claude

Once started, reply in the session thread to send input to Claude:

```
research OAuth2 best practices then implement it
```

Top-level messages in a project channel are automatically forwarded to the active session if there's exactly one.

### Network Approval Flow

When Claude needs network access (e.g., `pip install`):

1. Claude sees a timeout and outputs `[NETWORK_REQUEST: pypi.org]`
2. Session Manager posts an approval card in the session thread
3. Click **Approve** or **Deny**
4. If approved, domain is added to firewall and Claude retries

### Context Management (in session thread)

```
@claude compact               # Compact/summarize context window
@claude clear                 # Clear conversation history
@claude restart               # Kill & restart Claude (preserves context via --continue)
@claude context               # Show context health (messages, compactions, age, idle time)
```

### Manage Sessions

```
@claude stop <id-prefix>     # Stop a session by ID prefix
@claude stop                  # Stop the session (in a session thread)
@claude status                # List all active sessions (with message counts and idle time)
@claude help                  # Show available commands
```

## GitHub Repository Integration

The session manager clones GitHub repositories on-demand and optionally creates isolated git worktrees for concurrent sessions.

### Directory Structure on VM

```
/home/claude/repos/
└── github.com/
    └── org/
        └── repo/              # Main clone (shared)

/home/claude/worktrees/
├── repo-abc12345/             # Session worktree (isolated)
└── repo-def67890/             # Another session worktree
```

### Authentication

Private repositories require `GH_TOKEN` to be set on the LANLLM VM:

```bash
export GH_TOKEN=ghp_xxxxxxxxxxxx
```

### Worktrees

Git worktrees allow multiple sessions to work on the same repository simultaneously without conflicts:

- **Main clone**: Single shared copy, blocks if another session is active
- **Worktree**: Isolated working directory, shares `.git` with main clone
- Worktrees are cleaned up automatically when sessions end

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Health check (verifies DB connectivity, returns 503 if down) |
| `/metrics` | GET | Prometheus metrics (session counts, durations, approvals) |
| `/callback` | POST | Mattermost interactive message callback (HMAC-verified) |

## Configuration Reference

All settings use the `SM_` environment variable prefix.

### Required

| Variable | Description |
|----------|-------------|
| `SM_MATTERMOST_URL` | Mattermost server URL |
| `SM_MATTERMOST_TOKEN` | Bot authentication token |
| `SM_MATTERMOST_TEAM_ID` | Team ID for channel creation and sidebar categories |
| `SM_VM_HOST` | SSH host for the container VM |
| `SM_OPNSENSE_URL` | OPNsense firewall API URL |
| `SM_OPNSENSE_KEY` | OPNsense API key |
| `SM_OPNSENSE_SECRET` | OPNsense API secret |
| `SM_CALLBACK_SECRET` | HMAC shared secret for callback signature verification |
| `SM_DATABASE_URL` | PostgreSQL connection URL |

### Optional — SSH & VM

| Variable | Default | Description |
|----------|---------|-------------|
| `SM_VM_USER` | `claude` | SSH username |
| `SM_VM_SSH_KEY` | - | SSH private key content (preferred for K8s) |
| `SM_VM_SSH_KEY_PATH` | `/secrets/ssh/id_ed25519` | Path to SSH key file (fallback) |
| `SM_SSH_TIMEOUT_SECS` | `30` | Timeout for SSH operations (container start/stop) |

### Optional — Container

| Variable | Default | Description |
|----------|---------|-------------|
| `SM_CONTAINER_RUNTIME` | `podman` | Container runtime (`podman` or `docker`) |
| `SM_CONTAINER_NETWORK` | `isolated` | Fallback container network (used in auto-generated devcontainer.json) |
| `SM_CONTAINER_IMAGE` | `claude-code:latest` | Fallback image (used in auto-generated devcontainer.json) |
| `SM_CLAUDE_COMMAND` | `claude --dangerously-skip-permissions` | Command to run inside container |
| `SM_DEVCONTAINER_TIMEOUT_SECS` | `120` | Timeout for `devcontainer up` operations |
| `SM_ORCHESTRATOR_COMPACT_THRESHOLD` | `50` | Auto-compact orchestrator sessions every N messages (0 = disabled) |

### Optional — Git

| Variable | Default | Description |
|----------|---------|-------------|
| `SM_REPOS_BASE_PATH` | `/home/claude/repos` | Base path for cloned repositories |
| `SM_WORKTREES_PATH` | `/home/claude/worktrees` | Base path for worktree directories |
| `SM_AUTO_PULL` | `false` | Auto-pull repos before starting sessions |
| `SM_DEFAULT_ORG` | - | Default GitHub org (allows `start repo` shorthand) |
| `SM_PROJECTS` | `{}` | JSON map of project name to path (legacy, deprecated) |

### Optional — Mattermost

| Variable | Default | Description |
|----------|---------|-------------|
| `SM_CHANNEL_CATEGORY` | `Claude Sessions` | Sidebar category name for auto-created channels |
| `SM_BOT_TRIGGER` | `@claude` | Bot mention trigger text |
| `SM_ALLOWED_APPROVERS` | `[]` | List of usernames allowed to approve/deny requests (empty = all) |

### Optional — Firewall

| Variable | Default | Description |
|----------|---------|-------------|
| `SM_OPNSENSE_ALIAS` | `llm_approved_domains` | Firewall alias name to manage |
| `SM_OPNSENSE_VERIFY_TLS` | `true` | Verify TLS certificates for OPNsense API |
| `SM_OPNSENSE_TIMEOUT_SECS` | `30` | HTTP timeout for OPNsense API calls |

### Optional — Server

| Variable | Default | Description |
|----------|---------|-------------|
| `SM_CALLBACK_URL` | `http://session-manager:8000/callback` | URL Mattermost uses for interactive callbacks |
| `SM_LISTEN_ADDR` | `0.0.0.0:8000` | Server bind address |
| `SM_DATABASE_POOL_SIZE` | `5` | PostgreSQL connection pool size |
| `SM_RATE_LIMIT_RPS` | `10` | Rate limit: requests per second per IP |
| `SM_RATE_LIMIT_BURST` | `20` | Rate limit: burst size |

## How It Works

1. **Container makes request** → `pip install pytest` tries to reach pypi.org
2. **Firewall blocks** → pypi.org not in `llm_approved_domains` alias
3. **Claude sees timeout** → outputs `[NETWORK_REQUEST: pypi.org]`
4. **Session Manager intercepts** → posts approval card in the session thread
5. **User clicks Approve** → Session Manager calls OPNsense API
6. **API adds domain** → alias now includes pypi.org (validated against injection)
7. **Firewall allows** → rule matches, traffic passes
8. **Claude retries** → success

## Security

- **Network isolation**: Deny-by-default firewall, containers can only reach approved domains
- **Domain validation**: Domains are validated before firewall modification (rejects wildcards, IPs, special characters)
- **HMAC-signed callbacks**: Approval buttons use HMAC-SHA256 signatures to prevent tampering
- **Path traversal prevention**: Worktree names are validated (`[a-zA-Z0-9_.-]` only)
- **SQL injection prevention**: Parameterized queries throughout, LIKE prefix validated against UUID charset
- **Shell injection prevention**: All user inputs are shell-escaped before SSH execution
- **Rate limiting**: Per-IP token bucket rate limiting on all HTTP endpoints
- **Approver restrictions**: Optional `SM_ALLOWED_APPROVERS` to restrict who can approve network requests
- **Audit logging**: All approval/denial actions are logged with user, domain, and timestamp

## License

MIT
