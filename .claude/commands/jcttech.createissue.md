---
description: Create a GitHub issue using organization templates with proper field mapping
# GitHub: Prefer gh CLI; fallback to GitHub MCP if gh unavailable
scripts:
  sh: .specify/scripts/bash/check-prerequisites.sh --json --paths-only
  ps: .specify/scripts/powershell/check-prerequisites.ps1 -Json -PathsOnly
---

## User Input

```text
$ARGUMENTS
```

You **MUST** consider the user input before proceeding (if not empty).

## Overview

This command creates GitHub issues using organization-defined templates from `.specify/org-.specify/templates/`. It ensures issues follow the correct structure and include appropriate labels.

## Outline

1. Run `{SCRIPT}` from repo root and parse REPO_ROOT path. All paths must be absolute.

2. Load organization templates from `$REPO_ROOT/.specify/org-.specify/templates/`:
   - `epic.yml` - For cross-cutting initiatives
   - `spec.yml` - For technical specifications
   - `story.yml` - For implementable work units
   - `task.yml` - For granular work items
   - `bug.yml` - For defects and regressions

3. Get the Git remote by running:

```bash
git config --get remote.origin.url
```

> [!CAUTION]
> ONLY PROCEED TO NEXT STEPS IF THE REMOTE IS A GITHUB URL

4. Determine the issue type from user input or context:
   - Look for explicit type markers: `[Epic]`, `[Spec]`, `[Story]`, `[Task]`, `[Bug]`
   - If no marker, infer from content or ask user

5. Parse the corresponding organization template to extract:
   - Required fields and their types (input, textarea, dropdown, checkboxes)
   - Default labels from the template
   - Title prefix pattern

6. Structure the issue body to match template field order:
   - Map user-provided content to template sections
   - Include all required fields
   - Apply template-defined labels

7. Create the issue using the `gh` CLI:
   ```bash
   gh issue create --title "[Type] {title}" --type {Type} --label "{labels}" --body "{body}"
   ```
   - Set title with appropriate prefix (e.g., `[Spec] Feature Name`)
   - Include template-structured body
   - Apply labels from template + any user-specified labels

   > If `gh` CLI is unavailable, fall back to GitHub MCP if configured.

> [!CAUTION]
> UNDER NO CIRCUMSTANCES EVER CREATE ISSUES IN REPOSITORIES THAT DO NOT MATCH THE REMOTE URL

## Template Field Mapping

When creating issues, map content to template fields:

### Epic Template Fields
- `overview` - High-level description of the initiative
- `success-criteria` - How success will be measured
- `child-specs` - List of specifications under this epic

### Spec Template Fields
- `parent-epic` - Reference to parent epic issue number
- `overview` - What this specification covers
- `requirements` - Functional and non-functional requirements
- `acceptance-criteria` - Criteria for completion

### Story Template Fields
- `parent-spec` - Reference to parent spec issue number
- `user-story` - As a [role], I want [feature], so that [benefit]
- `acceptance-criteria` - Checklist of requirements
- `technical-notes` - Implementation guidance

### Task Template Fields
- `parent-story` - Reference to parent story issue number
- `description` - What needs to be done
- `definition-of-done` - Checklist for completion

### Bug Template Fields
- `parent` - Reference to parent issue (Story, Spec, or Epic)
- `description` - What is the bug
- `steps-to-reproduce` - How to reproduce
- `expected-behavior` - What should happen
- `actual-behavior` - What actually happens

## Example Issue Creation

For a Spec issue:

```markdown
## Overview
[Content mapped from user input]

## Requirements
[Content mapped from user input]

## Acceptance Criteria
- [ ] Criterion 1
- [ ] Criterion 2

---
Parent Epic: #42
```

Type: Spec | Labels: `status:draft`
