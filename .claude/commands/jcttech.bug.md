---
description: Create a Bug issue in GitHub (quick creation, prompts for parent)
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

This command creates a Bug issue in GitHub. Unlike Specs which require drafts, Bugs are created directly since they're typically urgent and straightforward.

Bugs can be attached to Stories, Specs, or Epics.

## Outline

1. **Parse user input** for:
   - Bug description/title
   - Steps to reproduce (if provided)
   - Expected vs actual behavior (if provided)

2. **Prompt for parent issue**:

   Show recent open issues:
   ```bash
   gh issue list --state open --json number,title,labels --limit 20
   ```

   Ask: "Which issue is this bug related to?"
   - Show Stories first (most common)
   - Then Specs
   - Then Epics

   > [!IMPORTANT]
   > A parent issue is required for proper hierarchy tracking.

3. **Gather bug details** (if not in input):

   - **Description**: What is the bug?
   - **Steps to reproduce**: How to trigger it?
   - **Expected behavior**: What should happen?
   - **Actual behavior**: What actually happens?
   - **Severity**: Critical / High / Medium / Low

4. **Generate bug issue body**:

   ```markdown
   **Related Issue**: #{parent_number}

   ## Bug Description

   [Description of the bug]

   ## Steps to Reproduce

   1. [Step 1]
   2. [Step 2]
   3. [Step 3]

   ## Expected Behavior

   [What should happen]

   ## Actual Behavior

   [What actually happens]

   ## Severity

   [Critical/High/Medium/Low]

   ## Environment

   - OS: [if relevant]
   - Browser: [if relevant]
   - Version: [if relevant]

   ## Additional Context

   [Screenshots, logs, etc.]
   ```

5. **Read project configuration** from `.specify/config.yml` (if exists).

6. **Create GitHub issue** with parent relationship and project:
   ```bash
   gh issue create --title "[Bug] {brief_description}" --type Bug --label "{severity_label}" --parent {parent_issue_number} --project {project_number} --body "{bug_body}"
   ```

   - Title: `[Bug] {brief_description}`
   - Type: Bug
   - Labels: severity label (e.g., `priority-high`)
   - Parent: Links to related issue via native GitHub sub-issue relationship
   - Project: Added to configured GitHub Project board
   - Body: Structured from step 4

7. **Sync index**:
   ```bash
   {SCRIPT}
   ```

8. **Report success**:
   - Bug issue number and URL
   - Parent issue reference
   - Severity assigned

## Severity Guidelines

| Severity | Description | Example |
|----------|-------------|---------|
| Critical | System unusable, data loss | Auth completely broken |
| High | Major feature broken | Can't create new users |
| Medium | Feature degraded | Slow performance |
| Low | Minor issue | Typo in UI |

## Example Interaction

```
User: /jcttech.bug Token validation crashes on null input

Claude: I'll create a bug report. Let me gather some details:

1. Which issue is this related to?
   [1] #102 [Story] JWT Token Service
   [2] #101 [Spec] JWT Authentication
   [3] #100 [Epic] User Authentication

User: 1

Claude: What's the severity?
   [1] Critical - System unusable
   [2] High - Major feature broken
   [3] Medium - Feature degraded
   [4] Low - Minor issue

User: 2

Claude: Creating bug issue...

Bug created!
#115 [Bug] Token validation crashes on null input
URL: https://github.com/owner/repo/issues/115
Related to: #102 (JWT Token Service)
Severity: High

Type: Bug | Labels: priority-high
```

## Quick Create Mode

For simple bugs, provide all info in one line:

```
/jcttech.bug #102 Token validation crashes when input is null - throws NullPointerException instead of returning error
```

This will:
- Link to #102
- Use the description as title and body
- Default to Medium severity
- Create immediately without prompts
