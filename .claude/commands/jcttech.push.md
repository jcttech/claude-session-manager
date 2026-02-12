---
description: Push a draft Spec to GitHub as an issue (validates hierarchy first)
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

This command pushes a local draft to GitHub as an issue. It validates that:
1. The draft is complete (no placeholder content)
2. The parent hierarchy exists (Epic for Specs)
3. Required fields are filled

## Outline

1. **List pending drafts** from `.specify/drafts/`:
   - Show drafts with their `ready_to_push` status
   - If user specifies a draft path/name, use that

2. **Select draft to push**:
   - If only one draft, confirm with user
   - If multiple, let user choose

3. **Read and parse draft**:
   - Extract YAML frontmatter
   - Parse markdown sections
   - Get parent_epic reference

4. **Validate draft**:

   Required checks:
   - [ ] Title is set (not placeholder)
   - [ ] Overview section has content
   - [ ] Requirements section has content
   - [ ] Acceptance criteria has at least 1 item
   - [ ] Parent Epic exists in GitHub
   - [ ] No `[NEEDS CLARIFICATION]` markers

   If validation fails:
   - List specific issues
   - Suggest running `/jcttech.clarify` if ambiguities
   - Do NOT push incomplete drafts

5. **Verify parent Epic exists**:
   ```bash
   gh issue view {parent_epic} --json number,title,state
   ```

   > [!IMPORTANT]
   > If parent Epic doesn't exist or is closed, STOP and inform user.

6. **Map draft to GitHub issue format**:
   - Title: `[Spec] {title}`
   - Type: Spec
   - Labels: `status:draft`
   - Body: Structured from draft sections with parent reference

7. **Read project configuration** from `.specify/config.yml` (if exists).

8. **Create GitHub issue** with parent relationship and project:
   ```bash
   gh issue create --title "[Spec] {title}" --type Spec --label "status:draft" --parent {parent_epic} --project {project_number} --body "{formatted_body}"
   ```

   > The `--parent` flag creates a native GitHub sub-issue relationship, visible in the issue sidebar.
   > The `--project` flag adds the issue to the configured GitHub Project board.

9. **Archive draft**:
   - Move draft to `.specify/issues/cache/spec-{number}.md`
   - Update frontmatter with `github_issue: {number}`

10. **Update index**:
    ```bash
    {SCRIPT}
    ```

11. **Report success**:
    - Issue number and URL
    - Parent Epic reference
    - Next steps: "Use `/jcttech.plan` to create Stories"

## Validation Errors

If validation fails, provide specific guidance:

```
Draft validation failed:

❌ Missing content in "Overview" section
❌ Acceptance Criteria contains placeholder text
❌ Parent Epic #100 not found

Fix these issues or run /jcttech.clarify to resolve ambiguities.
```

## Draft-to-Issue Mapping

| Draft Section | Issue Section |
|--------------|---------------|
| frontmatter.title | Issue title with `[Spec]` prefix |
| Overview | ## Overview |
| Requirements | ## Requirements |
| Acceptance Criteria | ## Acceptance Criteria |
| Technical Notes | ## Technical Notes |
| frontmatter.parent_epic | `Parent Epic: #{number}` reference |

## Example Success

```
Draft pushed successfully!

Spec #101: JWT Authentication
URL: https://github.com/owner/repo/issues/101
Parent Epic: #100 (User Authentication System)
Type: Spec | Labels: status:draft

Draft archived to: .specify/issues/cache/spec-101.md

Next: Use /jcttech.plan to create Stories for this Spec
```
