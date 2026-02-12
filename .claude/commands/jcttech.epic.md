---
description: Create a new Epic issue in GitHub (top-level initiative)
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

This command creates a new Epic issue in GitHub. Epics are top-level initiatives that contain Specs, which contain Stories.

## Outline

1. **Parse user input** to extract:
   - Epic title/name
   - Optional overview/description
   - Optional objectives

2. **Check for existing Epics** by running sync:
   ```bash
   {SCRIPT}
   ```
   Show the user any existing epics they might want to use instead.

3. **Verify Git remote** is a GitHub URL:
   ```bash
   git config --get remote.origin.url
   ```

   > [!CAUTION]
   > ONLY PROCEED IF THE REMOTE IS A GITHUB URL

4. **Load Epic template** from `.specify/org-.specify/templates/epic.yml` if available.

5. **Generate Epic issue body**:

   ```markdown
   ## Overview

   [User-provided description or placeholder]

   ## Objectives & Goals

   - [Objective 1]
   - [Objective 2]

   ## Success Criteria

   - [ ] [Criterion 1]
   - [ ] [Criterion 2]

   ## Scope

   ### In Scope
   - [Item 1]

   ### Out of Scope
   - [Item 1]

   ## Specifications

   _Specs will be linked here as they are created._
   ```

6. **Read project configuration** from `.specify/config.yml`:
   ```yaml
   github:
     project: "PROJECT_NUMBER"
   ```

7. **Create the Epic issue** with project association:
   ```bash
   gh issue create --title "[Epic] {title}" --type Epic --project {project_number} --body "{epic_body}"
   ```

   - Title: `[Epic] {title}`
   - Type: Epic
   - Project: Added to configured GitHub Project
   - Body: Generated from step 5

8. **Update local index** by running sync again.

9. **Report success**:
   - Epic issue number and URL
   - Instructions to create Specs: "Use `/jcttech.specify` to create specs under this epic"

## Example

User input: "User Authentication System"

Creates:
- Title: `[Epic] User Authentication System`
- Type: Epic
- Issue body with structured sections
