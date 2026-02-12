---
description: Create a Spec draft locally (prompts for parent Epic, push separately)
tools: []
scripts:
  sh: .specify/scripts/bash/jcttech/create-draft.sh --type spec --json
  ps: .specify/scripts/powershell/check-prerequisites.ps1 -Json -PathsOnly
---

## User Input

```text
$ARGUMENTS
```

You **MUST** consider the user input before proceeding (if not empty).

## Overview

This command creates a local Spec draft in `.specify/drafts/spec/`. Unlike upstream `/speckit.specify` which creates files directly, this command:
1. Prompts for a parent Epic (required)
2. Creates a draft that can be reviewed before pushing to GitHub
3. Integrates with the issue-centric workflow

## Outline

1. **Parse user input** to extract:
   - Spec title/name
   - Optional description

2. **List available Epics** from `.docs/issues-index.md` or by running:
   ```bash
   gh issue list --type Epic --state open --json number,title
   ```

3. **Prompt user to select parent Epic**:
   - Show list of open Epics with numbers
   - User can provide Epic number directly in input
   - If no Epics exist, prompt to create one first with `/jcttech.epic`

   > [!IMPORTANT]
   > A parent Epic is REQUIRED. Do not proceed without one.

4. **Generate draft** using script:
   ```bash
   {SCRIPT} --title "{title}" --parent-epic {epic_number}
   ```

5. **Validate draft location**:
   - Draft created at `.specify/drafts/spec/{NNN}-{short-name}.md`
   - Contains YAML frontmatter with parent_epic reference

6. **Report success**:
   - Draft path
   - Parent Epic reference
   - Next steps: "Edit the draft, then use `/jcttech.push` to create the GitHub issue"

## Draft Structure

The created draft will have this structure:

```markdown
---
draft_id: spec-001-feature-name
type: spec
title: "Feature Name"
created: "2026-01-15T..."
modified: "2026-01-15T..."
status: draft
ready_to_push: false
parent_epic: 100
validation:
  passed: false
  issues: []
---

# Spec: Feature Name

## Overview
[Description...]

## Requirements
### Functional Requirements
- [ ] [Requirement 1]

### Non-Functional Requirements
- [ ] [Performance requirement]

## Acceptance Criteria
- [ ] [Criterion 1]

## Technical Notes
[Considerations...]

## Open Questions
- [ ] [Question]
```

## Workflow

```
/jcttech.epic "Auth System"        → Creates Epic #100
/jcttech.specify "JWT Auth"        → Creates draft, prompts for Epic #100
[Edit draft as needed]
/jcttech.push                      → Creates Spec #101 linked to Epic #100
/jcttech.plan                      → Creates Stories under Spec #101
```
