---
description: Quick capture for backlog items (no parent required)
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

This command provides quick capture for backlog items that don't yet fit into the normal Epic → Spec → Story hierarchy. Ideas are:

- **Unplanned work**: Future enhancements, tech debt, exploratory concepts
- **Minimal friction**: No parent required, rapid capture
- **Triageable**: Later reviewed via `/jcttech.triage` to prioritize or promote to Specs

This is an **exception** to the normal hierarchy rules - Ideas can exist without a parent Epic.

## Outline

1. **Parse user input** to extract:
   - Idea title/description
   - Optional area hint (architecture, tech-debt, enhancement, etc.)

2. **Verify Git remote** is a GitHub URL:
   ```bash
   git config --get remote.origin.url
   ```

   > [!CAUTION]
   > ONLY PROCEED IF THE REMOTE IS A GITHUB URL

3. **Prompt for area** (if not provided in input):

   Ask: "What area does this idea relate to?"
   - `architecture` - System design, patterns, structure
   - `tech-debt` - Code cleanup, refactoring, maintenance
   - `enhancement` - New features, improvements
   - `performance` - Speed, efficiency, optimization
   - `security` - Auth, permissions, vulnerabilities
   - `ux` - User experience, UI improvements
   - `other` - Doesn't fit other categories

4. **Generate idea issue body**:

   ```markdown
   ## Idea

   [User-provided description]

   ## Context

   _Why is this worth considering?_

   ## Potential Value

   _What benefit would this provide?_

   ## Notes

   _Any additional context, links, or considerations._

   ---
   _Captured via `/jcttech.idea`. Review with `/jcttech.triage`._
   ```

5. **Read project configuration** from `.specify/config.yml`:
   ```yaml
   github:
     project: "PROJECT_NUMBER"
   ```

6. **Create GitHub issue** with backlog labels and project:
   ```bash
   gh issue create --title "[Idea] {title}" --type Idea --label "backlog,area:{area}" --project {project_number} --body "{idea_body}"
   ```

   - Title: `[Idea] {title}`
   - Type: Idea
   - Labels: `backlog`, `area:{selected_area}`
   - Project: Added to configured GitHub Project (Inbox column)
   - Body: Generated from step 4
   - **No parent required**

7. **Sync index**:
   ```bash
   {SCRIPT}
   ```

8. **Report success**:
   - Idea issue number and URL
   - Area label assigned
   - Next steps: "Review backlog with `/jcttech.triage` to prioritize"

## Area Descriptions

| Area | Description | Examples |
|------|-------------|----------|
| `architecture` | System design, patterns | "Consider event sourcing", "Microservices split" |
| `tech-debt` | Code cleanup, maintenance | "Refactor auth module", "Update dependencies" |
| `enhancement` | New features, improvements | "Add dark mode", "Export to CSV" |
| `performance` | Speed, efficiency | "Optimize queries", "Add caching" |
| `security` | Auth, permissions | "Add 2FA", "Audit logging" |
| `ux` | User experience, UI | "Improve onboarding", "Better error messages" |
| `other` | Miscellaneous | "Research competitor X", "Team training" |

## Example Interaction

```
User: /jcttech.idea Add support for SSO authentication

Claude: I'll capture this idea for the backlog. What area does it relate to?
   [1] security - Auth, permissions, vulnerabilities
   [2] enhancement - New features, improvements
   [3] architecture - System design, patterns

User: 1

Claude: Creating idea...

Idea captured!
#150 [Idea] Add support for SSO authentication
URL: https://github.com/owner/repo/issues/150
Area: security
Labels: backlog, area:security

This idea is now in your backlog. Use `/jcttech.triage` to review and prioritize.
```

## Quick Capture Mode

For rapid capture, provide area in the input:

```
/jcttech.idea [enhancement] Add bulk export feature for reports
```

This will:
- Create immediately with `area:enhancement` label
- Skip the area selection prompt
- Minimal friction for capturing ideas quickly

## Labels Created

- `backlog` - Identifies item as unplanned/future work
- `area:{area}` - Categorizes the idea for filtering

## Workflow Integration

```
/jcttech.idea ──► Idea (backlog, Inbox)
                      │
/jcttech.triage ──────┤──► Set effort/priority
                      │
                      └──► Promote to Spec ──► Normal Epic→Spec→Story flow
```
