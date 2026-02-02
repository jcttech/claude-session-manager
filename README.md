# Claude Session Manager

Chat with Claude Code through Mattermost with network-controlled security. Web search works freely, package installs require human approval via OPNsense firewall.

## Overview

This system lets you interact with Claude Code through Mattermost while maintaining strict network control:

- **Web search**: Works freely (Anthropic API pre-approved)
- **Package installs** (pip, npm, cargo): Blocked until approved
- **Approval workflow**: Click a button in Mattermost to allow specific domains

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│ LAN (Kubernetes cluster)                                                    │
│                                                                             │
│  ┌─────────────────┐     ┌─────────────────────────────────────────┐       │
│  │ Mattermost      │◄───►│ Session Manager                         │       │
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
│ OPNsense                              │   │ LANLLM VM                       │
│                                       │   │                                 │
│  Alias: llm_approved_domains          │   │  ┌───────────────────────────┐  │
│    - api.anthropic.com (pre-approved) │   │  │ Claude Code container     │  │
│    - pypi.org (added on approval)     │   │  │  ✅ web search works      │  │
│                                       │   │  │  🔒 pip/npm blocked       │  │
│  Rules (LANLLM):                      │   │  └───────────────────────────┘  │
│    1. PASS → OPNsense:53 (DNS)        │   │              │                  │
│    2. PASS → llm_approved_domains:443 │   │              │ outbound :443    │
│    3. BLOCK → any                     │   │              ▼                  │
│                                       │◄──│────── llm_approved_domains      │
└───────────────────────────────────────┘   └─────────────────────────────────┘
```

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

### Container Image

Build or pull the Claude Code container image:

```dockerfile
FROM node:20-slim

RUN npm install -g @anthropic-ai/claude-code
RUN apt-get update && apt-get install -y git curl python3 python3-pip && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
```

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
export SM_VM_HOST=192.168.12.10
export SM_OPNSENSE_URL=https://opnsense.example.com
export SM_OPNSENSE_KEY=your-api-key
export SM_OPNSENSE_SECRET=your-api-secret

# Optional (with defaults)
export SM_VM_USER=claude
export SM_VM_SSH_KEY="$(cat ~/.ssh/id_ed25519)"  # Preferred for Kubernetes
# OR: export SM_VM_SSH_KEY_PATH=/secrets/ssh/id_ed25519  # Fallback: file path
export SM_CONTAINER_RUNTIME=podman
export SM_CONTAINER_IMAGE=claude-code:latest
export SM_REPOS_BASE_PATH=/home/claude/repos
export SM_WORKTREES_PATH=/home/claude/worktrees
export SM_AUTO_PULL=false
export SM_PROJECTS='{"myproject":"/home/claude/projects/myproject"}'  # Legacy
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
        - name: SM_VM_HOST
          value: "192.168.12.10"
        - name: SM_OPNSENSE_URL
          value: "https://opnsense.example.com"
```

## Usage

### Start a Session

In Mattermost, mention the bot with a GitHub repository or project name:

```
@claude start org/repo                    # Clone and use main repo
@claude start org/repo@branch             # Checkout specific branch
@claude start org/repo --worktree         # Create isolated worktree (auto-named)
@claude start org/repo --worktree=feature # Create named worktree
@claude start org/repo@branch --worktree  # Worktree from specific branch
@claude start myproject                   # Use static project mapping (legacy)
```

**Concurrency:** If the main clone is already in use by another session, you'll be prompted to use `--worktree` for an isolated working directory.

### Chat with Claude

Once started, any message in the channel goes to Claude:

```
research OAuth2 best practices then implement it
```

### Network Approval Flow

When Claude needs network access (e.g., `pip install`):

1. Claude sees a timeout and outputs `[NETWORK_REQUEST: pypi.org]`
2. Session Manager posts an approval card to Mattermost
3. Click **Approve** or **Deny**
4. If approved, domain is added to firewall and Claude retries

### Stop a Session

```
@claude stop
```

## GitHub Repository Integration

The session manager can clone GitHub repositories on-demand and optionally create isolated git worktrees for concurrent sessions.

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

Private repositories require `GH_TOKEN` to be set on the LANLLM VM. The token is used for HTTPS cloning:

```bash
export GH_TOKEN=ghp_xxxxxxxxxxxx
```

### Worktrees

Git worktrees allow multiple sessions to work on the same repository simultaneously without conflicts:

- **Main clone**: Single shared copy, blocks if another session is active
- **Worktree**: Isolated working directory, shares `.git` with main clone

Worktrees persist after session stop (not auto-deleted), allowing you to resume work or manually clean up.

### Backward Compatibility

The `org/repo` syntax takes precedence. If input doesn't match that pattern, it falls back to `SM_PROJECTS` static mappings:

```bash
export SM_PROJECTS='{"myproject":"/home/claude/projects/myproject"}'
```

## Example Conversation

```
#claude-project

@james: @claude start acme/webapp
📦 Preparing **acme/webapp**...
🚀 Starting session...
✅ Ready. Container: `claude-abc12345`

@james: add pytest to the project

```
I'll add pytest to the project dependencies...

$ pip install pytest
ERROR: Connection timed out to pypi.org

[NETWORK_REQUEST: pypi.org]
```

🔒 **Network Request:** `pypi.org`
[✅ Approve] [❌ Deny]

✅ `pypi.org` approved by @james

```
[NETWORK_APPROVED: pypi.org]

$ pip install pytest
Successfully installed pytest-8.0.0
```

@james: @claude stop
✅ Stopped.
```

### Concurrent Sessions with Worktrees

```
#claude-project

@sarah: @claude start acme/webapp
⚠️ Repository **acme/webapp** is already in use by session `abc12345`.
Use `--worktree` for an isolated working directory:
`@claude start acme/webapp --worktree`

@sarah: @claude start acme/webapp --worktree
🌿 Creating worktree for **acme/webapp**...
🚀 Starting session...
✅ Ready. Container: `claude-def67890`
```

## Configuration Reference

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `SM_MATTERMOST_URL` | Yes | - | Mattermost server URL |
| `SM_MATTERMOST_TOKEN` | Yes | - | Bot token |
| `SM_VM_HOST` | Yes | - | SSH host for container VM |
| `SM_VM_USER` | No | `claude` | SSH user |
| `SM_VM_SSH_KEY` | No | - | SSH private key content (preferred for K8s) |
| `SM_VM_SSH_KEY_PATH` | No | `/secrets/ssh/id_ed25519` | Path to SSH key file (fallback) |
| `SM_CONTAINER_RUNTIME` | No | `podman` | `podman` or `docker` |
| `SM_CONTAINER_NETWORK` | No | `isolated` | Container network name |
| `SM_CONTAINER_IMAGE` | No | `claude-code:latest` | Container image |
| `SM_OPNSENSE_URL` | Yes | - | OPNsense API URL |
| `SM_OPNSENSE_KEY` | Yes | - | API key |
| `SM_OPNSENSE_SECRET` | Yes | - | API secret |
| `SM_OPNSENSE_ALIAS` | No | `llm_approved_domains` | Firewall alias name |
| `SM_REPOS_BASE_PATH` | No | `/home/claude/repos` | Where GitHub repos are cloned |
| `SM_WORKTREES_PATH` | No | `/home/claude/worktrees` | Where worktrees are created |
| `SM_AUTO_PULL` | No | `false` | Pull repos before starting sessions |
| `SM_PROJECTS` | No | `{}` | JSON map of project name to path (legacy) |
| `SM_CALLBACK_URL` | No | `http://session-manager:8000/callback` | Mattermost callback URL |
| `SM_LISTEN_ADDR` | No | `0.0.0.0:8000` | Server bind address |

## How It Works

1. **Container makes request** → `pip install pytest` tries to reach pypi.org
2. **Firewall blocks** → pypi.org not in `llm_approved_domains` alias
3. **Claude sees timeout** → outputs `[NETWORK_REQUEST: pypi.org]`
4. **Session Manager intercepts** → posts approval card to Mattermost
5. **User clicks Approve** → Session Manager calls OPNsense API
6. **API adds domain** → alias now includes pypi.org
7. **Firewall allows** → rule matches, traffic passes
8. **Claude retries** → success

## Security Notes

- The session manager only modifies the `llm_approved_domains` alias (hardcoded)
- Each Claude session runs in an isolated container
- Network access is deny-by-default
- All approvals are logged in Mattermost with the approving user

## License

MIT
