---
description: Sync local issue index with GitHub issues
tools: []
scripts:
  sh: .specify/scripts/bash/jcttech/sync-issues.sh --verbose
  ps: .specify/scripts/powershell/check-prerequisites.ps1 -Json -PathsOnly
---

## User Input

```text
$ARGUMENTS
```

You **MUST** consider the user input before proceeding (if not empty).

## Overview

This command synchronizes the local `.docs/issues-index.md` with GitHub Issues. It:
1. Fetches all issues with hierarchy labels (epic, spec, story, task, bug)
2. Rebuilds the hierarchical index
3. Caches issue content for offline reference
4. Detects any local/remote conflicts

## Outline

1. **Check GitHub authentication**:
   ```bash
   gh auth status
   ```

2. **Run sync script**:
   ```bash
   {SCRIPT}
   ```

3. **Sync active worktrees**:
   ```bash
   .specify/scripts/bash/jcttech/worktree-manager.sh --list
   ```
   - Detect active worktrees for Stories in progress
   - Include worktree status (clean/dirty, modified file count)

4. **Display sync results**:
   - Number of issues fetched by type
   - Any new issues since last sync
   - Any issues that changed state
   - Active worktrees and their status

5. **Report conflicts** if any:
   - Local drafts that match pushed issues
   - Issues modified externally
   - Worktrees with unpushed changes

6. **Show sync summary**:
   - Last sync timestamp
   - Total issues cached
   - Active worktrees count
   - Index file location

## What Gets Synced

| Source | Destination |
|--------|-------------|
| GitHub Issues (Type: Epic) | issues-index.md hierarchy |
| GitHub Issues (Type: Spec) | issues-index.md hierarchy + cache |
| GitHub Issues (Type: Story) | issues-index.md hierarchy + cache |
| GitHub Issues (Type: Task) | issues-index.md hierarchy + cache |
| GitHub Issues (Type: Bug) | issues-index.md hierarchy + cache |
| Local worktrees | issues-index.md Active Worktrees section |

## Index Structure After Sync

```markdown
# Issue Index

> Last synced: 2026-01-15T14:30:00Z
> Repository: owner/repo

## Hierarchy

### Epic: User Authentication System (#100)
**Status**: open | **Type**: Epic

#### Specs
| # | Title | Status | Stories |
|---|-------|--------|---------|
| #101 | JWT Authentication | open | 3 |

##### Spec #101: JWT Authentication
| # | Story | Status | Tasks |
|---|-------|--------|-------|
| #102 | JWT Token Service | in-progress | 4 |
| #103 | Login Endpoint | open | 3 |

---

## Active Worktrees

| # | Branch | Status | Modified |
|---|--------|--------|----------|
| #102 | `102-jwt-token-service` | dirty | 3 files |

---

## Drafts (Not Yet Pushed)
| Draft | Type | Ready |
|-------|------|-------|
| 002-oauth.md | spec | no |

---

## Metadata
sync_version: 1
last_full_sync: "2026-01-15T14:30:00Z"
issues_cached: 15
drafts_pending: 1
active_worktrees: 1
```

## Cache Structure

After sync, individual issues are cached at:
```
.specify/issues/cache/
├── epic-100.md
├── spec-101.md
├── story-102.md
├── story-103.md
└── task-110.md
```

Each cached file contains:
- YAML frontmatter with metadata
- Full issue body for offline reference

## Example Output

```
Syncing with GitHub...

Fetched from owner/repo:
- Epics: 2
- Specs: 5
- Stories: 12
- Tasks: 0 (tasks are checkboxes, not issues)
- Bugs: 3

Active worktrees:
- #102 102-jwt-token-service (dirty, 3 modified)
- #103 103-login-endpoint (clean)

New since last sync:
- #115 [Story] Add OAuth Integration

Changed state:
- #102 [Story] JWT Token Service: open → closed

Index updated: .docs/issues-index.md
Sync complete: 2026-01-15T14:30:00Z
```

## When to Use

- After making changes directly in GitHub
- To catch up on team member's changes
- Before starting `/jcttech.implement`
- Periodically to keep index current

Auto-sync happens automatically after:
- `/jcttech.epic`
- `/jcttech.push`
- `/jcttech.plan`
