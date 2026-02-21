# Claude Session Manager

Chat with Claude Code through Mattermost with network-controlled security. A Rust gateway manages sessions and container lifecycle, while a Python Agent SDK worker inside each devcontainer handles AI execution via gRPC.

## Overview

- **Thread-based sessions**: Each session lives in its own Mattermost thread for clean isolation
- **Devcontainer support**: Uses `devcontainer up` CLI to honor full devcontainer.json spec
- **gRPC execution**: Structured communication between gateway and Agent SDK worker (no NDJSON parsing)
- **Network-controlled security**: Deny-by-default firewall, domain-level approval via OPNsense
- **Multi-session containers**: Up to 5 sessions per container, keyed by `(repo, branch)`
- **Native subagents**: Multi-agent orchestration via Claude Agent SDK (`.claude/agents/*.md`)
- **Context management**: In-thread commands to compact, clear, restart, and inspect sessions

## Architecture

```
Session Manager (Rust/K8s)              VM with Devcontainers
┌─────────────────────────┐            ┌──────────────────────────┐
│ • Mattermost WebSocket  │            │  Devcontainer (repo X)   │
│ • Command routing       │   gRPC     │  ┌────────────────────┐  │
│ • Container lifecycle   │◄══════════►│  │ Agent SDK Worker   │  │
│ • Database tracking     │  (tonic)   │  │ (Python)           │  │
│ • Prometheus metrics    │            │  │ • ClaudeSDKClient  │  │
│                         │            │  │ • Session resume   │  │
│ tonic gRPC client       │  SSH :22   │  │ • Subagents        │  │
│ per devcontainer        │───────────►│  │ • grpcio server    │  │
└─────────────────────────┘ lifecycle  │  └────────────────────┘  │
                                       │  + CLAUDE.md, .claude/   │
          ┌──────────┐                 │  + MCP servers/tools     │
          │PostgreSQL │                 └──────────────────────────┘
          └──────────┘
          ┌──────────┐
          │ OPNsense │  ← domain allowlisting
          └──────────┘
```

**Execution path**: Mattermost → Rust gateway → gRPC → Python Agent SDK worker

**Container lifecycle**: Rust gateway → SSH → `devcontainer up/down` on VM

## Prerequisites

### OPNsense Firewall

1. **Create firewall alias** `llm_approved_domains` with `api.anthropic.com`
2. **Create LANLLM rules**: DNS allowed, approved domains on 443, block all else
3. **Create API user** `claude-session-manager` with firewall alias privileges

### VM (Container Host)

1. **Container runtime**: podman or docker installed
2. **SSH access**: Key-based authentication from Kubernetes
3. **`devcontainer` CLI**: Installed and available in PATH
4. **Environment**: `ANTHROPIC_API_KEY` set (passed to containers via `localEnv`)
5. **Environment**: `GH_TOKEN` set for private GitHub repos (optional)
6. **Agent worker**: `pip install agent-worker` in devcontainer base images

### PostgreSQL Database

A PostgreSQL instance for session state, project channel mappings, pending requests, and audit logging. Tables are automatically created on startup.

### Firewall Rules

| Rule | From | To | Port | Purpose |
|------|------|----|------|---------|
| SSH | K8s pod network | VM | 22/tcp | Container lifecycle |
| gRPC | K8s pod network | VM | 50051-50099/tcp | Agent worker communication |

## Deployment

### Kubernetes

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: claude-session-manager
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
  name: claude-session-manager
spec:
  replicas: 1
  selector:
    matchLabels:
      app: claude-session-manager
  template:
    metadata:
      labels:
        app: claude-session-manager
    spec:
      containers:
      - name: claude-session-manager
        image: ghcr.io/jcttech/claude-session-manager:2.0.0
        ports:
        - containerPort: 8000
        envFrom:
        - secretRef:
            name: claude-session-manager
        env:
        - name: SM_MATTERMOST_URL
          value: "https://mattermost.example.com"
        - name: SM_MATTERMOST_TEAM_ID
          value: "your-team-id"
        - name: SM_VM_HOST
          value: "your-vm-host-ip"
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

### Build from Source

```bash
# Rust gateway
cargo build --release

# Python agent worker
cd packages/agent-worker
uv build
```

Or use the Docker image:

```bash
docker build -f Dockerfile.session-manager -t claude-session-manager .
```

## Usage

### Start a Session

```
@claude start org/repo                    # Clone and use main repo
@claude start org/repo@branch             # Checkout specific branch
@claude start org/repo --worktree         # Create isolated worktree (auto-named)
@claude start org/repo --worktree=feature # Create named worktree
@claude start org/repo --plan             # Start in plan mode
@claude start myrepo                      # Uses SM_DEFAULT_ORG if configured
```

The bot creates a Mattermost channel for the project (if needed) and starts the session as a thread.

**Container reuse**: If a container already exists for `(repo, branch)`, the session attaches to it (fast path, <5s). Otherwise, `devcontainer up` runs first.

**Concurrency**: Up to `SM_CONTAINER_MAX_SESSIONS` (default 5) sessions share a container. If the main clone is in use, you'll be prompted to use `--worktree`.

### Chat with Claude

Reply in the session thread to send messages to Claude:

```
research OAuth2 best practices then implement it
```

Top-level messages in a project channel are forwarded to the active session if there's exactly one.

### Network Approval Flow

When Claude needs network access (e.g., `pip install`):

1. Claude sees a timeout and outputs `[NETWORK_REQUEST: pypi.org]`
2. Session Manager posts an approval card in the session thread
3. Click **Approve** or **Deny**
4. If approved, domain is added to firewall and Claude retries

### In-Thread Commands

```
@claude stop                  # Stop this session
@claude stop --container      # Stop session and tear down container
@claude stop --all            # Stop all sessions on this container
@claude compact               # Compact/summarize context window
@claude clear                 # Clear conversation history
@claude restart               # Kill and restart session
@claude plan                  # Toggle plan mode on/off
@claude title                 # Capture next response as thread title
@claude context               # Show context health (messages, tokens, idle time)
```

### Top-Level Commands

```
@claude start org/repo        # Start a new session
@claude stop <id-prefix>      # Stop a session by ID prefix
@claude status                # List all running containers and sessions
@claude help                  # Show available commands
```

## Session Types

| Type | Description | Created by |
|------|-------------|------------|
| `standard` | Normal interactive session | `@claude start <repo>` |
| `worker` | Task executor with isolated worktree | `@claude start <repo> --worktree` |
| `reviewer` | Code review session with isolated worktree | `@claude start <repo> --worktree` |

### Multi-Agent Orchestration

Subagents are handled natively by the Claude Agent SDK inside each devcontainer:

- Defined via `.claude/agents/*.md` files in the repo
- Invoked automatically by Claude via the Task tool
- Run in the same devcontainer with context isolation
- Worker reports `SubagentEvent` (start/finish) to the gateway

No special commands needed — Claude manages subagents autonomously.

## Devcontainer Support

Projects should provide a `.devcontainer/devcontainer.json` (JSONC supported):

```jsonc
{
    "image": "ghcr.io/myorg/devcontainer-rust:latest",
    "forwardPorts": [50051],
    "postStartCommand": "python -m agent_worker.server --port 50051 &",
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

If no devcontainer.json is found, a default one is auto-generated using `SM_CONTAINER_IMAGE` and `SM_CONTAINER_NETWORK`, with the agent worker configured automatically.

## Directory Structure on VM

```
/home/user/repos/
└── github.com/
    └── org/
        └── repo/              # Main clone (shared across sessions)

/home/user/worktrees/
├── repo-abc12345/             # Session worktree (isolated)
└── repo-def67890/             # Another session worktree
```

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Health check (verifies DB connectivity) |
| `/metrics` | GET | Prometheus metrics (sessions, tokens, approvals) |
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
| `SM_CALLBACK_SECRET` | HMAC shared secret for callback verification |
| `SM_DATABASE_URL` | PostgreSQL connection URL |

### Optional — SSH & VM

| Variable | Default | Description |
|----------|---------|-------------|
| `SM_VM_USER` | `claude` | SSH username |
| `SM_VM_SSH_KEY` | - | SSH private key content (preferred for K8s) |
| `SM_VM_SSH_KEY_PATH` | `/secrets/ssh/id_ed25519` | Path to SSH key file (fallback) |
| `SM_SSH_TIMEOUT_SECS` | `30` | Timeout for SSH operations |

### Optional — Container

| Variable | Default | Description |
|----------|---------|-------------|
| `SM_CONTAINER_RUNTIME` | `podman` | Container runtime (`podman` or `docker`) |
| `SM_CONTAINER_NETWORK` | `isolated` | Fallback container network |
| `SM_CONTAINER_IMAGE` | `claude-code:latest` | Fallback container image |
| `SM_DEVCONTAINER_TIMEOUT_SECS` | `120` | Timeout for `devcontainer up` |
| `SM_CONTAINER_MAX_SESSIONS` | `5` | Maximum concurrent sessions per container |
| `SM_CONTAINER_IDLE_TIMEOUT_SECS` | `1800` | Auto-teardown empty containers after N seconds |
| `SM_GRPC_PORT_START` | `50051` | Starting port for gRPC worker connections |

### Optional — Git

| Variable | Default | Description |
|----------|---------|-------------|
| `SM_REPOS_BASE_PATH` | `/home/claude/repos` | Base path for cloned repositories |
| `SM_WORKTREES_PATH` | `/home/claude/worktrees` | Base path for worktree directories |
| `SM_AUTO_PULL` | `false` | Auto-pull repos before starting sessions |
| `SM_DEFAULT_ORG` | - | Default GitHub org (allows `start repo` shorthand) |

### Optional — Mattermost

| Variable | Default | Description |
|----------|---------|-------------|
| `SM_CHANNEL_CATEGORY` | `Claude Sessions` | Sidebar category for auto-created channels |
| `SM_BOT_TRIGGER` | `@claude` | Bot mention trigger text |
| `SM_ALLOWED_APPROVERS` | `[]` | Usernames allowed to approve/deny requests (empty = all) |

### Optional — Firewall

| Variable | Default | Description |
|----------|---------|-------------|
| `SM_OPNSENSE_ALIAS` | `llm_approved_domains` | Firewall alias name to manage |
| `SM_OPNSENSE_VERIFY_TLS` | `true` | Verify TLS for OPNsense API |
| `SM_OPNSENSE_TIMEOUT_SECS` | `30` | HTTP timeout for OPNsense API |

### Optional — Server

| Variable | Default | Description |
|----------|---------|-------------|
| `SM_CALLBACK_URL` | `http://session-manager:8000/callback` | Callback URL for Mattermost |
| `SM_LISTEN_ADDR` | `0.0.0.0:8000` | Server bind address |
| `SM_DATABASE_POOL_SIZE` | `5` | PostgreSQL connection pool size |
| `SM_RATE_LIMIT_RPS` | `10` | Requests per second per IP |
| `SM_RATE_LIMIT_BURST` | `20` | Rate limit burst size |
| `SM_SESSION_LIVENESS_TIMEOUT_SECS` | `120` | Idle time before posting stale session warning |

## Security

- **Network isolation**: Deny-by-default firewall, containers can only reach approved domains
- **Domain validation**: Rejects wildcards, IPs, special characters before firewall modification
- **HMAC-signed callbacks**: Approval buttons use HMAC-SHA256 to prevent tampering
- **Path traversal prevention**: Worktree names validated to `[a-zA-Z0-9_.-]` only
- **SQL injection prevention**: Parameterized queries throughout (sqlx)
- **Shell injection prevention**: All user inputs escaped before SSH execution
- **Rate limiting**: Per-IP token bucket on all HTTP endpoints
- **Approver restrictions**: Optional `SM_ALLOWED_APPROVERS` to restrict who can approve
- **Audit logging**: All approval/denial actions logged with user, domain, and timestamp
- **Container isolation**: Each repo runs in its own devcontainer with network restrictions

## Development

### Rust (gateway)

```bash
cargo test          # Run all tests (190 Rust tests)
cargo clippy        # Lint
cargo build         # Build
```

### Python (agent worker)

```bash
cd packages/agent-worker
uv sync             # Install dependencies
uv run pytest       # Run tests (16 Python tests)
uv run ruff check . # Lint
uv build            # Build wheel
```

### Releasing

Tag-based releases via GitHub Actions:

```bash
git tag session-manager/v2.1.0 && git push origin session-manager/v2.1.0
git tag agent-worker/v2.1.0 && git push origin agent-worker/v2.1.0
```

## License

MIT
