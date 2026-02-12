---
description: Generate implementation plan from Spec, create Story issues in GitHub
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

This command generates an implementation plan from a Spec and creates Story issues in GitHub. Each Story will contain task checkboxes (not separate Task issues).

This is the key command that transforms specifications into implementable work units.

## Outline

1. **Identify the Spec**:
   - If user provides Spec number directly (e.g., "#101"), use that
   - Otherwise, list recent Specs from index and prompt user to select

2. **Fetch Spec content** from GitHub:
   ```bash
   gh issue view {spec_number} --json title,body,labels
   ```

3. **Analyze Spec** to break down into Stories:
   - Review requirements and acceptance criteria
   - Group related functionality into Stories
   - Each Story should be 1-3 days of work
   - Each Story should be independently testable

4. **Generate Stories** (typically 2-5 per Spec):

   For each Story, create:
   ```markdown
   **Parent Spec**: #{spec_number}

   ## User Story

   As a [user type], I want [action] so that [benefit].

   ## Description

   [Detailed description of what this story accomplishes]

   ## Tasks

   - [ ] [Specific task 1]
   - [ ] [Specific task 2]
   - [ ] [Specific task 3]

   ## Acceptance Criteria

   - [ ] [Criterion 1]
   - [ ] [Criterion 2]

   ## Technical Notes

   [Implementation guidance]
   ```

5. **Read project configuration** from `.specify/config.yml` (if exists).

6. **Create Story issues** in GitHub with parent relationship and project:
   ```bash
   gh issue create --title "[Story] {story_title}" --type Story --label "status:draft" --parent {spec_number} --project {project_number} --body "{story_body}"
   ```

   - Title: `[Story] {story_title}`
   - Type: Story
   - Labels: `status:draft`
   - Parent: Links to Spec via native GitHub sub-issue relationship
   - Project: Added to configured GitHub Project board

7. **Update Spec issue** to list child Stories:
   Add comment or update body with links to created Stories.

8. **Update architecture** if needed:
   - Check if new components/services are introduced
   - If material changes, update `.docs/architecture.md`
   - Prompt to update `.docs/architecture.excalidraw`

9. **Sync index**:
   ```bash
   {SCRIPT}
   ```

10. **Report success**:
   - List of created Story issues
   - Summary: "Created N Stories with M total tasks"
   - Next steps: "Use `/jcttech.implement` to work on a Story"

## Story Guidelines

- **Independent**: Each Story can be implemented and tested alone
- **Valuable**: Each Story delivers user-visible value
- **Estimable**: Stories should be small enough to estimate
- **Tasks as checkboxes**: Tasks are checkboxes IN the Story, not separate issues

## Example Output

```
Plan generated for Spec #101: JWT Authentication

Created Stories:
- #102 [Story] Implement JWT Token Service (4 tasks)
- #103 [Story] Create Login Endpoint (3 tasks)
- #104 [Story] Add Token Refresh Flow (3 tasks)

Total: 3 Stories, 10 tasks

Architecture updated: .docs/architecture.md
- Added: AuthService component

Next: Use /jcttech.implement to start on Story #102
```

## Architecture Updates

When creating Stories, check for architecture impact:
- New services or components mentioned → Update architecture.md
- New external integrations → Update architecture.md
- Database schema changes → Note in architecture.md

If architecture changes detected, also prompt user to update the Excalidraw diagram.
