---
description: Analyze consistency and quality across synced spec and story issues
tools: []
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

This command performs a non-destructive consistency and quality analysis across GitHub issues (specs and stories) that have been synced to the local cache. Unlike `/speckit.analyze` which analyzes feature files, this analyzes the issue hierarchy.

**Input Source**: `.specify/issues/cache/` (synced from GitHub)
**Output**: Console report (read-only, no file modifications)

**Key Difference**: Speckit commands analyze local `spec.md`, `plan.md`, `tasks.md` files. This command analyzes GitHub Issues stored in the cache.

## Prerequisites

Before running, ensure issues are synced:
```bash
/jcttech.sync
```

Or the script will sync automatically:
```bash
{SCRIPT}
```

## Outline

1. **Sync issues** (ensure cache is current):
   - If cache is stale (>1 hour) or user requests: run sync
   - Load issues from `.specify/issues/cache/`

2. **Parse scope** from user input:
   - `analyze` → Analyze all cached issues
   - `analyze #100` → Analyze Epic #100 and its hierarchy
   - `analyze spec #101` → Analyze Spec #101 and its stories
   - If scope issue not found, report error

3. **Load cached issues** from `.specify/issues/cache/`:
   - Epic files: `epic-{num}.md`
   - Spec files: `spec-{num}.md`
   - Story files: `story-{num}.md`
   - Task files: `task-{num}.md`
   - Bug files: `bug-{num}.md`

4. **Build semantic models**:

   From specs:
   - Requirements inventory (functional + non-functional)
   - Acceptance criteria list
   - Parent epic linkage

   From stories:
   - User story statements
   - Task checklists
   - Acceptance criteria
   - Parent spec linkage

5. **Run detection passes**:

   | Pass | What It Detects | Severity |
   |------|-----------------|----------|
   | **Coverage Analysis** | Spec requirements with no mapped stories | HIGH |
   | **Hierarchy Validation** | Missing parent, broken references, orphans | CRITICAL/HIGH |
   | **Terminology Drift** | Inconsistent terms (user vs account vs customer) | MEDIUM |
   | **Vague Language** | "fast", "secure", "scalable" without metrics | MEDIUM |
   | **Placeholders** | [NEEDS CLARIFICATION], [TODO], [TBD] | HIGH |

6. **Calculate metrics**:
   - Total issues by type
   - Overall coverage percentage
   - Findings by severity

7. **Generate analysis report** (console output):

   ```markdown
   ## Issue Hierarchy Analysis Report

   **Scope**: [All | Epic #100 | Spec #101]
   **Analyzed**: 2026-01-15T14:30:00Z
   **Cache**: 15 issues (synced 2 hours ago)

   ### Coverage Summary

   | Spec | Title | Requirements | Stories | Coverage |
   |------|-------|--------------|---------|----------|
   | #101 | JWT Auth | 5 | 3 | 60% |
   | #105 | OAuth | 3 | 0 | 0% |

   ### Findings

   | Severity | Issue | Finding | Recommendation |
   |----------|-------|---------|----------------|
   | CRITICAL | #102 | Broken parent reference | Sync issues or fix reference |
   | HIGH | #101 | Requirement "rate limiting" uncovered | Create story |
   | MEDIUM | #103 | "fast" lacks metrics | Quantify latency target |

   ### Metrics

   - Total Issues: 15 (2 epics, 4 specs, 8 stories, 1 task)
   - Overall Coverage: 65%
   - Critical: 1, High: 3, Medium: 5, Low: 2
   ```

8. **Suggest remediations** based on findings:
   - Coverage gaps: "Create stories for requirements X, Y"
   - Terminology drift: "Standardize on term Z"
   - Missing hierarchy: "Link issue #N to parent"
   - Placeholders: "Run /jcttech.clarify on draft"

## Detection Categories

### Coverage Analysis
- Spec requirements → Story mapping
- Story → Task breakdown coverage
- Acceptance Criteria coverage
- Stories not linked to specs (orphans)

### Hierarchy Validation
- Parent-child integrity (Epic → Spec → Story)
- Orphan detection (issues with no parent)
- Broken references (parent issue not in cache)
- Specs without Epic parent
- Stories without Spec parent

### Terminology Analysis
- Inconsistent terms within same concept
- Examples: "user" vs "account", "token" vs "jwt", "api" vs "endpoint"

### Quality Analysis
- Vague adjectives: "fast", "secure", "scalable", "intuitive"
- Placeholders: [NEEDS CLARIFICATION], [TODO], ???, [TBD]
- Empty sections
- Template content not filled in

## Example Output

```
Issue Hierarchy Analysis
========================

Scope: All cached issues
Synced: 2026-01-15T14:30:00Z (2 hours ago)

Epic #100: User Authentication System
├── Spec #101: JWT Authentication (3/5 requirements covered)
│   ├── Story #102: Token Service ✓
│   ├── Story #103: Login Endpoint ✓
│   └── Story #104: Refresh Flow ✓
└── Spec #105: OAuth Integration (0/3 requirements covered) ⚠️

Findings:
┌──────────┬───────┬─────────────────────────────────────────┐
│ Severity │ Issue │ Finding                                 │
├──────────┼───────┼─────────────────────────────────────────┤
│ HIGH     │ #105  │ No stories - run /jcttech.plan          │
│ HIGH     │ #101  │ 2 uncovered requirements                │
│ MEDIUM   │ #102  │ Contains vague "fast" - add latency     │
│ MEDIUM   │ -     │ Terminology: "user" vs "account"        │
└──────────┴───────┴─────────────────────────────────────────┘

Metrics:
- Issues: 8 (1 epic, 2 specs, 5 stories)
- Coverage: 60% (6/10 requirements)
- Critical: 0, High: 2, Medium: 2

Next Actions:
1. Run /jcttech.plan for Spec #105 to create stories
2. Create 2 more stories for Spec #101 uncovered requirements
3. Quantify "fast" in Story #102 with specific latency target
```

## Severity Levels

| Severity | Description | Action Required |
|----------|-------------|-----------------|
| **CRITICAL** | Broken hierarchy, missing issues | Immediate fix required |
| **HIGH** | Missing stories, uncovered requirements | Create missing artifacts |
| **MEDIUM** | Vague language, terminology drift | Should address before implementation |
| **LOW** | Style issues, minor gaps | Nice to fix |

## Workflow

```
/jcttech.epic "Auth System"        → Creates Epic #100
/jcttech.specify "JWT Auth"        → Creates draft
/jcttech.push                      → Creates Spec #101
/jcttech.plan                      → Creates Stories #102-104
/jcttech.sync                      → Syncs all issues to cache
/jcttech.analyze                   → This command - analyzes everything ← THIS
/jcttech.analyze #100              → Analyze only Epic #100 hierarchy
```

## When to Run

- **After planning**: Verify specs have adequate story coverage
- **Before implementation**: Check for placeholders and vague requirements
- **During review**: Ensure consistency across the hierarchy
- **Periodically**: Monitor coverage and quality metrics
