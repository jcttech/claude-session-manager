---
description: Record an Architecture Decision Record (ADR) in decisions.md
tools: []
scripts:
  sh: .specify/scripts/bash/jcttech/add-decision.sh --json
  ps: .specify/scripts/powershell/check-prerequisites.ps1 -Json -PathsOnly
---

## User Input

```text
$ARGUMENTS
```

You **MUST** consider the user input before proceeding (if not empty).

## Overview

This command records architectural decisions in `.docs/decisions.md` using the ADR (Architecture Decision Record) format. ADRs help track why certain technical choices were made.

## Outline

1. **Parse user input** for:
   - Decision title (required)
   - Context/reason (optional, can prompt)
   - Options considered (optional)
   - Related issues (optional, e.g., "#101, #102")

2. **Gather decision details** through conversation:

   Ask user for:
   - **Context**: Why is this decision needed?
   - **Options**: What alternatives were considered?
   - **Decision**: What was chosen and why?
   - **Consequences**: What are the trade-offs?
   - **Material impact**: Does this affect architecture diagrams?

3. **Run add-decision script**:
   ```bash
   {SCRIPT} --title "{title}" --status accepted --related "{issues}" [--material]
   ```

4. **Update decisions.md**:
   - Add entry to index table
   - Append full ADR at end of file

5. **If material impact**:
   - Prompt user to update `.docs/architecture.excalidraw`
   - Suggest running `/jcttech.architecture`

6. **Report success**:
   - ADR number assigned
   - File location
   - Reminder about diagram if material

## ADR Format

Each decision follows this structure:

```markdown
## ADR-001: [Decision Title]

**Status**: proposed | accepted | deprecated | superseded
**Date**: 2026-01-15
**Related Issues**: #101, #102

### Context
[Why is this decision needed? What problem does it solve?]

### Options Considered
1. **Option A** - Description
   - Pros: ...
   - Cons: ...

2. **Option B** - Description
   - Pros: ...
   - Cons: ...

### Decision
[What was decided and why? Be specific about the choice made.]

### Consequences
- [Positive consequence 1]
- [Negative consequence 1 - and mitigation]

### Architecture Impact
- **Material**: Yes/No
- **Diagram Update**: Required/Not Required
```

## Example Interaction

```
User: /jcttech.decision Use jose library for JWT

Claude: I'll record this decision. Let me gather some details:

1. What's the context for choosing a JWT library?
   User: We need JWT handling for auth, and jose is well-maintained with good TypeScript support.

2. What alternatives did you consider?
   User: jsonwebtoken (more popular but less TypeScript support), node-jose (complex API)

3. Does this affect the architecture diagram?
   User: No, it's an internal implementation detail.

Claude: Recording ADR-003...

ADR-003 added: Use jose library for JWT
File: .docs/decisions.md
Status: accepted
Related Issues: #101, #102

Decision recorded! Since this doesn't have material architecture impact,
no diagram update is needed.
```

## Status Values

- **proposed**: Decision is being discussed
- **accepted**: Decision has been made and is in effect
- **deprecated**: Decision is no longer recommended but may exist in code
- **superseded**: Decision has been replaced by another ADR (link to new one)

## When to Record Decisions

Record an ADR when:
- Choosing a library or framework
- Deciding on a design pattern
- Making trade-offs (performance vs. simplicity)
- Choosing between architectural approaches
- Settling debates that arose during implementation

## Material vs Non-Material

**Material decisions** (require diagram update):
- Adding new services/components
- Changing data flow between components
- Adding external integrations
- Major refactoring of structure

**Non-material decisions** (no diagram update):
- Library choices
- Code style decisions
- Internal implementation patterns
- Testing approaches