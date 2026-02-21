# Architecture Overview

Claude Session Manager is a polyglot (Rust + Python) Mattermost integration that enables interactive Claude Code sessions within Mattermost threads. The Rust gateway handles chat, container lifecycle, and routing. A Python Agent SDK worker inside each devcontainer handles AI execution via gRPC.

## System Architecture

```
Session Manager (Rust/K8s)              VM with Devcontainers
┌─────────────────────────┐            ┌──────────────────────────┐
│ • Mattermost WebSocket  │            │  Devcontainer (repo X)   │
│ • Command routing       │   gRPC     │  ┌────────────────────┐  │
│ • Container lifecycle   │◄══════════►│  │ Agent SDK Worker   │  │
│ • Database tracking     │  (tonic)   │  │ (Python)           │  │
│ • Container registry    │            │  │ • ClaudeSDKClient  │  │
│                         │            │  │ • Session resume   │  │
│ tonic gRPC client       │            │  │ • Subagents        │  │
│ per devcontainer        │            │  │ • grpcio server    │  │
└─────────────────────────┘            │  └────────────────────┘  │
                                       │  + CLAUDE.md, .claude/   │
                                       │  + Same plugins/tools    │
                                       └──────────────────────────┘
```

**Execution path** (2 hops): Mattermost → Rust gateway → gRPC → Python Agent SDK worker

## System Components

### Repo Structure

```
claude-session-manager/
├── crates/                          ← Rust workspace
│   ├── session-manager/             ← Main gateway application
│   ├── mattermost-client/           ← Mattermost WebSocket + REST
│   ├── common/                      ← Shared crypto, rate limiting
│   ├── graph-client/                ← Microsoft Graph (stub)
│   └── mail-bridge/                 ← Email bridge (stub)
├── packages/                        ← Python packages
│   └── agent-worker/                ← gRPC Agent SDK worker
├── proto/                           ← Shared protobuf definitions
│   └── agent.proto
└── .github/workflows/               ← CI/CD for both languages
```

### Crate Structure (Rust)

| Crate | Purpose |
|-------|---------|
| **session-manager** | Main application: HTTP server, session lifecycle, gRPC client |
| **mattermost-client** | WebSocket listener + HTTP REST API client for Mattermost |
| **common** | Shared utilities: HMAC-SHA256 crypto, rate limiting middleware |
| **graph-client** | Microsoft Graph API client (stub, future use) |
| **mail-bridge** | Email integration bridge (stub, future use) |

### Python Packages

| Package | Purpose |
|---------|---------|
| **agent-worker** | gRPC server wrapping Claude Agent SDK, session management, event mapping |

### Core Modules (session-manager)

| Module | File | Responsibility |
|--------|------|----------------|
| **HTTP Server & Router** | `main.rs` | Axum server, Mattermost command routing, stream output batching |
| **Session Lifecycle** | `container.rs` | Session creation/teardown, gRPC connection per container, message_processor task |
| **gRPC Client** | `grpc.rs` | tonic client mapping `AgentEvent` stream → `OutputEvent` enum |
| **Container Registry** | `container_registry.rs` | In-memory `RwLock<HashMap>` of running containers, keyed by `(repo, branch)` |
| **Database** | `database.rs` | PostgreSQL ORM via sqlx: sessions, containers, pending requests, audit logs |
| **Output Events** | `stream_json.rs` | `OutputEvent` enum, tool action formatting, signal name mapping |
| **Git Operations** | `git.rs` | Repo cloning, worktree management, `RepoRef` parsing (`org/repo@branch`) |
| **SSH Executor** | `ssh.rs` | Remote command execution on VM for container lifecycle (devcontainer up/down) |
| **OPNsense Client** | `opnsense.rs` | Firewall REST API: domain alias management, validation |
| **Devcontainer** | `devcontainer.rs` | devcontainer.json handling, auto-generation with gRPC worker config |
| **Liveness Monitor** | `liveness.rs` | Per-session activity tracking via DashMap, stale session warnings |
| **Idle Monitor** | `idle_monitor.rs` | Periodic container teardown when no active sessions |
| **Orchestrator Prompt** | `orchestrator_prompt.rs` | Template loading and variable injection for orchestrator sessions |
| **Configuration** | `config.rs` | `SM_`-prefixed env vars via `config` crate, `OnceLock` singleton |

### Agent Worker Modules (Python)

| Module | File | Responsibility |
|--------|------|----------------|
| **gRPC Server** | `server.py` | `AgentWorkerServicer` implementing Execute, SendMessage, Interrupt, Health RPCs |
| **Session Manager** | `session_manager.py` | Manages `ClaudeSDKClient` instances keyed by session_id |
| **Event Mapper** | `event_mapper.py` | Maps SDK message types → protobuf `AgentEvent` messages |

### External Integrations

| System | Protocol | Purpose |
|--------|----------|---------|
| **PostgreSQL** | SQL (sqlx) | Persistent state: sessions, containers, pending requests, audit logs |
| **Mattermost** | WebSocket + HTTP REST | Message ingestion, thread replies, channel management, interactive buttons |
| **OPNsense Firewall** | HTTP REST (HMAC-signed) | Dynamic domain allowlisting for container network access |
| **VM** | SSH | Container lifecycle: devcontainer up/down, git clone |
| **Agent Worker** | gRPC (tonic ↔ grpcio) | AI execution via Claude Agent SDK inside devcontainers |
| **Git (GitHub)** | SSH (via VM) | Repository cloning, worktree creation/deletion |
| **Docker/Podman** | Via devcontainer CLI | Container runtime for development environments |

## gRPC Service Definition

The `proto/agent.proto` defines the communication contract between the Rust gateway and Python worker:

| RPC | Direction | Purpose |
|-----|-----------|---------|
| **Execute** | Gateway → Worker (server stream) | Start new session, stream `AgentEvent`s back |
| **SendMessage** | Gateway → Worker (server stream) | Continue existing session, stream response events |
| **Interrupt** | Gateway → Worker (unary) | Interrupt a running session |
| **Health** | Gateway → Worker (unary) | Check worker readiness after container start |

### AgentEvent Types

| Event | Maps to OutputEvent | Purpose |
|-------|---------------------|---------|
| `SessionInit` | _(captured internally)_ | Provides session_id for follow-up messages |
| `TextContent` | `TextLine` | Response text from Claude |
| `ToolUse` | `ToolAction` | Tool invocation (Read, Bash, Edit, etc.) |
| `ToolResult` | _(not surfaced)_ | Tool execution result (informational) |
| `SubagentEvent` | _(logged)_ | Subagent start/finish (native SDK feature) |
| `SessionResult` | `ResponseComplete` / `ProcessDied` | Final result with token usage |
| `AgentError` | `ProcessDied` | Error with type and message |

## Data Flow

### Message Lifecycle

```
Mattermost WebSocket
    │
    ▼
Message Handler (main.rs)
    │
    ├── Command (@claude start/stop/etc) ──► Process immediately
    │
    └── Regular message in thread ──► Session message_tx channel
                                          │
                                          ▼
                                    message_processor (container.rs)
                                          │
                                    ┌─────┴─────┐
                                    │ First msg  │ Follow-up
                                    ▼            ▼
                              gRPC Execute  gRPC SendMessage
                                    │            │
                                    └─────┬──────┘
                                          ▼
                                    AgentEvent stream (grpc.rs)
                                          │
                                          ▼
                                    OutputEvent channel
                                          │
                                          ▼
                                    stream_output() batching (main.rs)
                                          │
                                    ┌─────┴─────┐
                                    │           │
                                    ▼           ▼
                              Mattermost   Network Request
                              thread reply  Detection
```

### Session Startup

```
@claude start org/repo@branch
    │
    ▼
RepoRef::parse()
    │
    ▼
GitManager::clone_or_get_repo()  ──► SSH clone to /repos/github.com/org/repo
    │                                 (+ optional worktree)
    ▼
ContainerRegistry::lookup(repo, branch)
    │
    ├── Hit: reuse existing container (fast path)
    │
    └── Miss: cold_start()
          │
          ├── devcontainer up (via SSH)
          ├── wait_for_health() ──► gRPC Health RPC (30 retries, 1s interval)
          └── Register container in registry
               │
               ▼
    Session::create_internal()
        ├── Allocate mpsc channels (message_tx, output_tx)
        ├── Spawn message_processor (connects GrpcExecutor)
        ├── Register in liveness tracker
        └── Persist to PostgreSQL
```

### Network Approval Workflow

```
Claude outputs [NETWORK_REQUEST: pypi.org]
    │
    ▼
stream_output() regex detection
    │
    ▼
Validate domain (no wildcards, IPs, special chars)
    │
    ▼
Create PendingRequest in database
    │
    ▼
Post interactive message (Approve/Deny buttons with HMAC signatures)
    │
    ├── User clicks Approve ──► Verify HMAC → OPNsense add domain → Audit log
    │
    └── User clicks Deny ──► Audit log
```

### Multi-Agent Orchestration

Subagents are handled natively by the Claude Agent SDK within each devcontainer:

- Defined via `.claude/agents/*.md` files in the repo
- Invoked automatically by Claude via the Task tool
- Run in the same devcontainer with context isolation
- Worker reports `SubagentEvent` (start/finish) — gateway logs but does not intercept

Cross-repo orchestration (future): custom MCP tools in the worker that call back to the gateway API.

## Database Schema

5 tables managed via `database.rs` with auto-initialization:

| Table | Purpose | Key Fields |
|-------|---------|------------|
| **sessions** | Track active/historic sessions | `session_id`, `channel_id`, `thread_id`, `project`, `container_name`, `session_type` |
| **containers** | Track devcontainer lifecycle | `repo` + `branch` (unique), `container_name`, `state` (running/stopping/stopped), `session_count` |
| **project_channels** | Map projects to Mattermost channels | `project` (PK), `channel_id`, `channel_name` |
| **pending_requests** | Network approval queue | `request_id`, `session_id`, `domain`, `post_id` |
| **audit_logs** | Approval/denial audit trail | `request_id`, `domain`, `action`, `approved_by` |

## Session Types

| Type | Purpose | Worktree |
|------|---------|----------|
| **standard** | Interactive user session | Shared repo |
| **orchestrator** | Multi-agent coordinator using SDK subagents | Shared repo |
| **worker** | Task executor, created via `start` command | Isolated worktree |
| **reviewer** | Code review session, created via `start` command | Isolated worktree |

## Background Tasks

5 concurrent Tokio tasks with `CancellationToken` for graceful shutdown (10s timeout):

1. **WebSocket Listener** - Mattermost message ingestion with exponential backoff reconnection
2. **Message Handler** - Command parsing and session routing (mpsc channel consumer)
3. **Stream Output** - Per-session `OutputEvent` batching, network request detection, Mattermost posting
4. **Liveness Monitor** - Periodic stale session detection and warning (default: 120s timeout)
5. **Idle Monitor** - Periodic container teardown when session_count=0 (default: 1800s timeout)

## Security Architecture

| Layer | Implementation |
|-------|----------------|
| **Callback Integrity** | HMAC-SHA256 with length-prefixed format preventing collision attacks |
| **Domain Validation** | Reject wildcards, IPs, special chars; require dots; no leading/trailing dots or hyphens |
| **Path Traversal Prevention** | Worktree names validated to `[a-zA-Z0-9_.-]` only |
| **SQL Injection** | Parameterized queries throughout via sqlx |
| **Shell Injection** | All user inputs escaped via `shell_escape` crate before SSH execution |
| **Rate Limiting** | Per-IP token bucket on all HTTP endpoints (configurable rps + burst) |
| **Approver Control** | Optional `SM_ALLOWED_APPROVERS` restricts network approval permissions |
| **Audit Trail** | All approve/deny actions logged with user, domain, timestamp |
| **Network Isolation** | Containers have no internet by default; OPNsense firewall controls per-domain access |
| **Container Isolation** | Each repo runs in its own devcontainer; worker process sandboxed inside |

## Technology Stack

- **Language**: Rust (2021 edition) + Python 3.13
- **Async Runtime**: Tokio (Rust), asyncio (Python)
- **Web Framework**: Axum 0.8
- **gRPC**: tonic 0.12 / prost 0.13 (Rust), grpcio (Python)
- **AI Engine**: Claude Agent SDK (`claude-agent-sdk` via Python worker)
- **Database**: PostgreSQL (sqlx with runtime-tokio)
- **Serialization**: serde / serde_json (Rust), protobuf (cross-language)
- **HTTP Client**: reqwest
- **WebSocket**: tokio-tungstenite (via mattermost-client)
- **Container Runtime**: Docker/Podman via devcontainer CLI
- **Python Tooling**: uv, hatch-vcs

## Configuration

All settings use `SM_` environment variable prefix, loaded via `OnceLock` singleton:

**Required** (9): `MATTERMOST_URL`, `MATTERMOST_TOKEN`, `MATTERMOST_TEAM_ID`, `VM_HOST`, `OPNSENSE_URL`, `OPNSENSE_KEY`, `OPNSENSE_SECRET`, `CALLBACK_SECRET`, `DATABASE_URL`

**Optional** (20+): VM user/port, container idle timeout, liveness timeout, rate limit params, repos base path, firewall alias name, server bind address, `GRPC_PORT_START` (default: 50051), etc.

## Diagrams

See `architecture.excalidraw` for visual component diagrams.

---
_Last updated: 2026-02-21_
_Generated by /jcttech.architecture_
