---
description: Clarify underspecified areas in a local draft spec before pushing to GitHub
tools: []
scripts:
  sh: .specify/scripts/bash/jcttech/list-drafts.sh --type spec --json
  ps: .specify/scripts/powershell/check-prerequisites.ps1 -Json -PathsOnly
---

## User Input

```text
$ARGUMENTS
```

You **MUST** consider the user input before proceeding (if not empty).

## Overview

This command identifies and resolves ambiguities in local draft specs before they are pushed to GitHub. Unlike `/speckit.clarify` which works on feature-based spec files, this operates on drafts in `.specify/drafts/spec/`.

**Key Difference**: Speckit commands expect local `spec.md` files as source of truth. This command works on drafts that will become GitHub Issues.

## Outline

1. **Select draft to clarify**:
   - If user provides draft path, number, or name, use that
   - Otherwise, list available drafts:
     ```bash
     {SCRIPT}
     ```
   - Show drafts with their validation status and ready_to_push flag

2. **Load draft content**:
   - Parse YAML frontmatter for metadata
   - Parse markdown sections for content
   - Check for existing `## Clarifications` section from previous sessions

3. **Check for previous clarifications**:
   - Display any existing Q&A from previous sessions
   - Avoid asking questions that have already been answered

4. **Perform ambiguity scan** using this taxonomy:

   | Category | What to Check |
   |----------|---------------|
   | **Functional Scope** | Core goals, success criteria, out-of-scope |
   | **Domain & Data** | Entities, relationships, lifecycle states |
   | **Non-Functional** | Performance targets, scalability, security |
   | **Edge Cases** | Failure scenarios, error handling, recovery |
   | **Terminology** | Undefined terms, vague adjectives |

5. **Detect ambiguity signals**:
   - Vague adjectives: "fast", "secure", "scalable", "intuitive", "flexible"
   - Placeholder markers: `[Requirement 1]`, `[Criterion 1]`
   - Explicit markers: `[NEEDS CLARIFICATION]`
   - Unchecked open questions in `## Open Questions` section
   - Missing quantified targets in NFRs

6. **Generate clarification questions** (max 5):
   - Prioritize by: `Impact × Uncertainty` heuristic
   - Skip questions already answered in previous sessions
   - Each question must be answerable with:
     - Multiple-choice options (with recommendation)
     - Short text answer
     - "Defer" to leave for later

7. **Sequential questioning loop**:
   - Present ONE question at a time
   - For multiple-choice: provide recommendation with reasoning
   - After each answer:
     a. Record in `## Clarifications` → `### Session YYYY-MM-DD`
     b. Update relevant section if appropriate (Requirements, Acceptance Criteria, etc.)
     c. Save draft immediately (atomic write)
     d. Update `modified` timestamp in frontmatter
   - Ask if user wants to continue or stop after each question

8. **Update draft validation**:
   - After all questions (or user stops):
   - Re-run validation checks
   - Update `validation.passed` and `validation.issues` in frontmatter
   - Update `ready_to_push` if validation now passes

9. **Report completion**:
   - Questions asked/answered count
   - Sections touched
   - Validation status (READY or remaining issues)
   - Suggest `/jcttech.push` if ready, or `/jcttech.checklist` for deeper review

## Draft Modification

Clarifications are recorded in this structure:

```markdown
## Clarifications

### Session 2026-01-15

- Q: What authentication method should be used for JWT?
  - A: RS256 asymmetric keys with 2048-bit RSA
- Q: What is the token expiration time?
  - A: 15 minutes for access tokens, 7 days for refresh tokens

### Session 2026-01-14

- Q: Should the API support multiple authentication providers?
  - A: Yes, OAuth2 (Google, GitHub) and local username/password
```

When answers affect requirements, update the relevant section:
- Add to `## Requirements` → `### Functional Requirements` or `### Non-Functional Requirements`
- Add to `## Acceptance Criteria`
- Resolve items in `## Open Questions` by checking them off

## Question Patterns

**Good clarification questions**:
- "What latency target should 'fast' be quantified as? (a) <100ms, (b) <500ms, (c) <1s"
- "How should the system handle token expiration? (a) Silent refresh, (b) Redirect to login, (c) Show error"
- "What is the expected concurrent user load? (a) <100, (b) <1000, (c) <10000"

**Bad questions** (too open-ended):
- "What do you want the system to do?"
- "Can you describe the requirements?"

## Validation Integration

After clarifications, these checks determine `ready_to_push`:
- [ ] No `[NEEDS CLARIFICATION]` markers remain
- [ ] No placeholder content like `[Requirement 1]`
- [ ] Required sections have substantive content (not just templates)
- [ ] Parent epic reference exists
- [ ] Open questions are resolved (checked off)

## Workflow

```
/jcttech.specify "JWT Auth"        → Creates draft in .specify/drafts/spec/
/jcttech.clarify                   → This command - resolves ambiguities
/jcttech.clarify 001               → Clarify specific draft by number
[Continue editing draft]
/jcttech.checklist security        → Create checklist for deeper review
/jcttech.push                      → Push validated draft to GitHub
```
