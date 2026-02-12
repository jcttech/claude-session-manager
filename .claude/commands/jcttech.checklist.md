---
description: Generate a requirements quality checklist for a local draft spec
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

This command generates a requirements quality checklist linked to a local draft spec. Checklists are "unit tests for English" - they validate the quality, clarity, and completeness of requirements before the draft is pushed to GitHub.

Unlike `/speckit.checklist` which creates checklists in feature directories, this creates checklists in `.specify/drafts/spec/checklists/` linked to specific drafts.

**Key Principle**: Checklists test REQUIREMENTS quality, not implementation behavior.

## Outline

1. **Select draft for checklist**:
   - If user provides draft path, number, or domain, parse accordingly
   - Input formats:
     - `security` → Create security checklist for most recent draft
     - `001 security` → Create security checklist for draft 001
     - `001-jwt-auth.md api` → Create API checklist for specific draft
   - If ambiguous, list available drafts:
     ```bash
     {SCRIPT}
     ```

2. **Clarify checklist intent** (up to 3 questions):
   - **Domain focus**: "What domain should the checklist focus on?"
     - Options: Security, UX, API, Data Model, Performance, General
   - **Depth**: "Quick sanity check or thorough review?"
     - Options: Quick (5-8 items), Standard (10-15 items), Comprehensive (20+ items)
   - **Purpose**: "Self-review or peer review?"
     - Affects phrasing and detail level

3. **Load draft context**:
   - Parse draft sections: Overview, Requirements, Acceptance Criteria
   - Extract user stories if present
   - Identify domain keywords for focus areas
   - Check for existing checklists (avoid duplication)

4. **Generate checklist items** - "Unit Tests for Requirements":

   **Checklist categories**:
   | Category | What It Tests |
   |----------|---------------|
   | Requirement Completeness | Are all necessary requirements present? |
   | Requirement Clarity | Are requirements specific and unambiguous? |
   | Requirement Consistency | Do requirements align without conflicts? |
   | Acceptance Criteria Quality | Are success criteria measurable? |
   | Edge Case Coverage | Are boundary conditions defined? |
   | Non-Functional Coverage | Performance, security, accessibility? |

5. **Create checklist file**:
   - **Directory**: `.specify/drafts/spec/checklists/`
   - **Filename**: `{draft-id}-{domain}.md`
   - Example: `spec-001-jwt-auth-security.md`

6. **Link checklist to draft**:
   - Update draft frontmatter with checklist reference:
     ```yaml
     checklists:
       - spec-001-jwt-auth-security.md
       - spec-001-jwt-auth-api.md
     ```

7. **Report completion**:
   - Checklist path
   - Item count by category
   - Reminder: "Use checklist to review draft before `/jcttech.push`"

## Item Patterns

**CORRECT patterns** (testing requirements quality):
- "Are all [requirement type] defined for [scenario]?"
- "Is '[vague term]' quantified with specific criteria?"
- "Are requirements consistent between [section A] and [section B]?"
- "Does the draft define [missing aspect]?"
- "Can acceptance criterion X be objectively verified?"

**WRONG patterns** (testing implementation - DO NOT USE):
- "Verify the button works correctly"
- "Test error handling"
- "Confirm API returns 200"
- "Check that login succeeds"

## Domain-Specific Examples

### Security Checklist Items
```markdown
- [ ] CHK-SEC-001 - Are authentication requirements explicitly defined?
- [ ] CHK-SEC-002 - Are authorization levels documented for each endpoint?
- [ ] CHK-SEC-003 - Is data encryption specified (at rest, in transit)?
- [ ] CHK-SEC-004 - Are session timeout requirements quantified?
- [ ] CHK-SEC-005 - Are input validation requirements listed?
```

### API Checklist Items
```markdown
- [ ] CHK-API-001 - Are all endpoints documented with methods and paths?
- [ ] CHK-API-002 - Are request/response schemas defined?
- [ ] CHK-API-003 - Are error response formats specified?
- [ ] CHK-API-004 - Are rate limiting requirements documented?
- [ ] CHK-API-005 - Is API versioning strategy defined?
```

### UX Checklist Items
```markdown
- [ ] CHK-UX-001 - Are user flows documented for each scenario?
- [ ] CHK-UX-002 - Are loading/error states defined?
- [ ] CHK-UX-003 - Are accessibility requirements specified (WCAG level)?
- [ ] CHK-UX-004 - Are responsive breakpoints defined?
- [ ] CHK-UX-005 - Are user feedback mechanisms documented?
```

## Generated Checklist Structure

```markdown
# Security Checklist: JWT Authentication

**Purpose**: Requirements quality validation for security
**Created**: 2026-01-15
**Draft**: 001-jwt-auth.md
**Draft ID**: spec-001-jwt-auth

---

## Requirement Completeness

- [ ] CHK001 - Are all authentication methods documented? [Completeness]
- [ ] CHK002 - Are token lifecycle requirements defined? [Completeness]

## Requirement Clarity

- [ ] CHK003 - Is "secure" defined with specific criteria? [Clarity, Spec §NFR]
- [ ] CHK004 - Are token expiration times quantified? [Clarity]

## Acceptance Criteria Quality

- [ ] CHK005 - Can "successful authentication" be objectively verified? [Measurability]

## Edge Case Coverage

- [ ] CHK006 - Are token expiration scenarios defined? [Coverage, Gap]
- [ ] CHK007 - Are invalid token scenarios documented? [Coverage]

---

## Notes

- Check items off as validated: `[x]`
- Mark gaps in draft with `[NEEDS CLARIFICATION]`
```

## Workflow Integration

```
/jcttech.specify "JWT Auth"        → Creates draft
/jcttech.clarify                   → Resolves ambiguities
/jcttech.checklist security        → Creates security checklist ← THIS
/jcttech.checklist api             → Creates API checklist
[Review draft against checklists]
/jcttech.push                      → Push validated draft to GitHub
```

## Multiple Checklists

A single draft can have multiple checklists for different domains:

```
.specify/drafts/spec/
├── 001-jwt-auth.md                  # The draft
└── checklists/
    ├── spec-001-jwt-auth-security.md
    ├── spec-001-jwt-auth-api.md
    └── spec-001-jwt-auth-general.md
```

Draft frontmatter tracks linked checklists:
```yaml
checklists:
  - spec-001-jwt-auth-security.md
  - spec-001-jwt-auth-api.md
  - spec-001-jwt-auth-general.md
```
