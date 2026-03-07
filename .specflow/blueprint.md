# Blueprint: Claude Session Manager

## Vision

A Mattermost-integrated gateway that enables teams to run interactive Claude Code sessions inside isolated devcontainers, with enterprise security controls (firewall-gated network access, audit logging, HMAC-signed callbacks) and multi-agent team coordination.

## System Overview

```
Mattermost <--WebSocket/REST--> Session Manager (Rust/K8s)
                                    |
                              gRPC (tonic)
                                    |
                          VM with Devcontainers
                          +---------------------+
                          | Agent Worker (Python)|
                          | Claude Agent SDK     |
                          +---------------------+
                                    |
                            OPNsense Firewall
                          (domain-level access control)
```

## Components

### 1. Session Manager (Rust Gateway)

**Crate:** `crates/session-manager`

The central orchestrator. Handles Mattermost WebSocket ingestion, command parsing, container lifecycle (via SSH + devcontainer CLI), session state tracking, and output batching back to Mattermost threads.

**Key modules:**
- `main.rs` — Axum HTTP server, command routing, stream output batching, Prometheus metrics
- `container.rs` — Session lifecycle, message_processor task, gRPC connection management
- `database.rs` — PostgreSQL persistence (sessions, containers, pending requests, audit logs)
- `grpc.rs` — tonic gRPC client, bidirectional Session stream, event mapping
- `container_registry.rs` — In-memory container tracking with DB sync
- `git.rs` — Repo cloning, worktree management
- `devcontainer.rs` — JSONC parsing, auto-generation with agent worker config
- `opnsense.rs` — Firewall REST API for domain allowlisting
- `idle_monitor.rs` — Automatic container teardown after inactivity
- `config.rs` — SM_* environment variable loading

### 2. Mattermost Client

**Crate:** `crates/mattermost-client`

WebSocket listener with exponential backoff reconnection and REST API client for thread replies, channel management, and sidebar categories.

### 3. Common Utilities

**Crate:** `crates/common`

Shared HMAC-SHA256 crypto (callback signing/verification) and per-IP token bucket rate limiting middleware.

### 4. Agent Worker (Python)

**Package:** `packages/agent-worker`

gRPC server (grpcio) wrapping the Claude Agent SDK. Implements bidirectional Session RPC, Interrupt, and Health endpoints. Maps SDK message types to protobuf AgentEvent messages.

**Key modules:**
- `server.py` — gRPC servicer with Session/Interrupt/Health RPCs
- `session_manager.py` — ClaudeSDKClient instance management and lifecycle
- `event_mapper.py` — SDK message → protobuf AgentEvent mapping

### 5. Protocol (Shared Contract)

**File:** `proto/agent.proto`

Defines the gRPC contract between Rust gateway and Python worker:
- `Session` — Bidirectional stream (SessionInput → AgentEvent)
- `Interrupt` — Unary RPC to stop running sessions
- `Health` — Readiness check for container startup

## Data Stores

### PostgreSQL

| Table | Purpose |
|-------|---------|
| `sessions` | Active/historic session tracking (session_id, thread_id, project, container, claude_session_id) |
| `containers` | Devcontainer lifecycle (repo+branch unique, state, session_count, grpc_port) |
| `project_channels` | Project → Mattermost channel mapping |
| `pending_requests` | Network approval queue (domain, session, post) |
| `audit_logs` | Approve/deny audit trail (who, what, when) |
| `teams` | Multi-agent team metadata (team_id, channel, project, lead_session, dev_count) |
| `team_roles` | Role reference data (name, description, responsibilities, constraints) |

## Key Architectural Decisions

### Bidirectional gRPC over NDJSON
Single persistent stream per session. First message is CreateSession, subsequent are FollowUp on the same stream. The SDK's anyio channel is reused across turns — `return` (not `break`) on ResultMessage keeps the iterator alive.

### Container reuse with registry
Containers are keyed by (repo, branch). Warm path (<5s) reuses existing containers up to max_sessions. Cold path runs `devcontainer up` via SSH (30-120s).

### Deny-by-default networking
Containers start with no internet. Network requests are detected via regex in output, validated, and require human approval via HMAC-signed Mattermost buttons before OPNsense firewall rules are updated.

### Session resume on worker death
`claude_session_id` is captured from SDK init events and persisted. On reconnect, `resume_session_id` in CreateSession allows the Claude conversation to continue where it left off.

### Multi-agent teams
Team Lead sessions can spawn specialized agents (Architect, Developer, QA, DevOps) via marker-based communication ([SPAWN:role], [TO:role], [BROADCAST]). Roles are defined in the `team_roles` database table with responsibilities injected into system prompts.

## Integration Points

| System | Protocol | Purpose |
|--------|----------|---------|
| Mattermost | WebSocket + REST | User interaction, thread management |
| PostgreSQL | SQL (sqlx) | Persistent state |
| Agent Worker | gRPC (tonic ↔ grpcio) | AI execution |
| OPNsense | REST (HMAC) | Firewall domain management |
| VM | SSH | Container lifecycle, git operations |
| GitHub | Git (via VM SSH) | Repo cloning, worktrees |
| Prometheus | HTTP /metrics | Observability |

## Security Model

- **HMAC-SHA256** callback verification with length-prefixed format
- **Domain validation** — no wildcards, IPs, or special characters
- **Shell escaping** — all user inputs escaped before SSH execution
- **Path traversal prevention** — worktree names restricted to `[a-zA-Z0-9_.-]`
- **Parameterized SQL** — sqlx compile-time checked queries
- **Per-IP rate limiting** — token bucket on all HTTP endpoints
- **Audit logging** — all approval/denial actions recorded
- **Network isolation** — containers deny-by-default, per-domain allowlisting

## Deployment

- **Session Manager**: Docker multi-stage build → `ghcr.io/jcttech/claude-session-manager`
- **Agent Worker**: PyPI wheel installed in devcontainer base images
- **CI/CD**: GitHub Actions with tag-based releases (`session-manager/v*`, `agent-worker/v*`)
- **Infrastructure**: Kubernetes (session manager) + VM (devcontainers) + PostgreSQL

## Current Version

- Session Manager: v2.2.0
- Agent Worker: v2.2.0
- Rust edition: 2021
- Python: >=3.13

## Roadmap

### v2.2.1 — Spec-Flow Migration & Team Coordination (Current)
- Migrate from spec-kit to claude-spec-flow MCP workflow
- Update team role responsibilities in `database.rs` to reference spec-flow prompts
- Clean up legacy `.claude/commands/jcttech.*.md` and `.specify/` config
- Multi-agent team coordination runtime (teams table, team_roles, marker messaging)
- Role-based system prompt injection and inter-agent communication
- Blueprint and project documentation

### Future
- Microsoft Graph integration for O365 notifications (stub exists in `graph-client`)
- Email bridge for non-Mattermost access (stub exists in `mail-bridge`)
- Devcontainer auto-rebuild on config hash changes
- Horizontal scaling exploration (session affinity, shared state)
