---
description: Review and categorize backlog items
# GitHub: Prefer gh CLI; fallback to GitHub MCP if gh unavailable
scripts:
  sh: .specify/scripts/bash/jcttech/sync-issues.sh --json
  ps: .specify/scripts/powershell/check-prerequisites.ps1 -Json -PathsOnly
---

## User Input

```text
$ARGUMENTS
```

You **MUST** consider the user input before proceeding (if not empty).

## Overview

This command reviews backlog items (Ideas) and helps categorize, prioritize, or promote them. The triage workflow:

1. Lists Ideas in the Inbox (uncategorized backlog)
2. Sets Effort and Priority for each item
3. Optionally promotes Ideas to Specs (joining normal hierarchy)
4. Moves triaged items to "Triaged" status

## Outline

1. **Parse user input** for:
   - Specific issue number to triage
   - Filter by area (e.g., "tech-debt", "security")
   - Action (e.g., "promote #150")

2. **Fetch backlog items** from GitHub:
   ```bash
   gh issue list --type Idea --label backlog --state open --json number,title,labels,createdAt
   ```

   Filter to show Inbox items (not yet triaged).

3. **Display backlog summary**:

   ```
   Backlog Items (Inbox)
   ─────────────────────
   # | Title                              | Area        | Age
   ──┼────────────────────────────────────┼─────────────┼─────
   150 | Add SSO authentication           | security    | 3d
   151 | Bulk export for reports          | enhancement | 1d
   152 | Refactor auth module             | tech-debt   | 5d
   ```

4. **For each item** (or selected item), prompt for:

   **Effort** - How much work is this?
   - `XS` - Extra small (< 2 hours)
   - `S` - Small (2-4 hours)
   - `M` - Medium (1-2 days)
   - `L` - Large (3-5 days)
   - `XL` - Extra large (1-2 weeks)

   **Priority** - How important is this?
   - `Critical` - Must do immediately
   - `High` - Important, schedule soon
   - `Medium` - Normal priority
   - `Low` - Nice to have, when time permits

5. **Ask for action**:

   - **Keep in backlog**: Add effort/priority labels, move to "Triaged"
   - **Promote to Spec**: Convert to a proper Spec under an Epic
   - **Close**: Close as won't-do or duplicate
   - **Skip**: Move to next item

6. **If promoting to Spec**:

   a. List available Epics:
   ```bash
   gh issue list --type Epic --state open --json number,title
   ```

   b. Prompt for parent Epic selection

   c. Create new Spec issue with Idea content:
   ```bash
   gh issue create --title "[Spec] {title_without_idea_prefix}" --type Spec --parent {epic_number} --project {project_number} --body "{spec_body}"
   ```

   d. Close original Idea with reference:
   ```bash
   gh issue close {idea_number} --comment "Promoted to Spec #{spec_number}"
   ```

7. **If keeping in backlog**:

   Add labels for effort and priority:
   ```bash
   gh issue edit {issue_number} --add-label "effort:{effort},priority:{priority}"
   ```

   Update project status to "Triaged" (via GraphQL if configured).

8. **Sync index**:
   ```bash
   {SCRIPT}
   ```

9. **Report summary**:
   - Items triaged: N
   - Promoted to Specs: N
   - Remaining in backlog: N

## Effort Guidelines

| Effort | Time Estimate | Description |
|--------|---------------|-------------|
| `XS` | < 2 hours | Quick fix, simple change |
| `S` | 2-4 hours | Small feature, minor refactor |
| `M` | 1-2 days | Medium feature, moderate complexity |
| `L` | 3-5 days | Large feature, significant work |
| `XL` | 1-2 weeks | Major initiative, needs breakdown |

> [!TIP]
> If an item is `XL`, it likely should be promoted to a Spec and broken into Stories.

## Priority Guidelines

| Priority | Description | Action |
|----------|-------------|--------|
| `Critical` | Blocking or urgent | Schedule immediately |
| `High` | Important business need | Schedule within sprint |
| `Medium` | Normal priority | Plan when capacity allows |
| `Low` | Nice to have | Consider for future |

## Example Interaction

```
User: /jcttech.triage

Claude: Fetching backlog items...

Backlog Items (Inbox)
─────────────────────
# | Title                              | Area        | Age
──┼────────────────────────────────────┼─────────────┼─────
150 | Add SSO authentication           | security    | 3d
151 | Bulk export for reports          | enhancement | 1d
152 | Refactor auth module             | tech-debt   | 5d

Let's triage #150: Add SSO authentication

What's the effort estimate?
   [1] XS - Less than 2 hours
   [2] S - 2-4 hours
   [3] M - 1-2 days
   [4] L - 3-5 days
   [5] XL - 1-2 weeks

User: 4

Claude: What's the priority?
   [1] Critical - Must do immediately
   [2] High - Important, schedule soon
   [3] Medium - Normal priority
   [4] Low - Nice to have

User: 2

Claude: What action for this item?
   [1] Keep in backlog (with effort:L, priority:high)
   [2] Promote to Spec (under an Epic)
   [3] Close (won't do)
   [4] Skip

User: 2

Claude: Which Epic should this Spec belong to?
   [1] #100 [Epic] User Authentication System
   [2] #105 [Epic] Security Enhancements

User: 1

Claude: Creating Spec and closing Idea...

Promoted to Spec!
- Closed: #150 [Idea] Add SSO authentication
- Created: #155 [Spec] Add SSO authentication
- Parent: #100 [Epic] User Authentication System

Continue triaging? (2 items remaining)
```

## Filtered Triage

Triage specific areas:

```
/jcttech.triage area:security
```

Triage a specific item:

```
/jcttech.triage #150
```

Batch promote:

```
/jcttech.triage promote #150 #151 --epic 100
```

## Labels Applied

When triaging:
- `effort:{xs|s|m|l|xl}` - Size estimate
- `priority:{critical|high|medium|low}` - Importance

## Workflow Integration

```
Ideas (Inbox)
     │
     ├──► Triage ──► Set effort/priority ──► Triaged (backlog)
     │
     └──► Promote ──► Spec ──► Normal Epic→Spec→Story flow
```
