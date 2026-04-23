# Anti-Pattern Audit Report

**Date:** {DATE}
**Triggered by:** {Incident that prompted the audit}
**Scope:** {Codebase path and description}
**Method:** {N} parallel search agents, one per anti-pattern

---

## Executive Summary

**Total findings: {N}** across {N} anti-pattern categories.

| Severity | Count | Action Required |
|----------|-------|----------------|
| Critical | {N} | Fix immediately |
| High | {N} | Fix within 1 week |
| Medium | {N} | Track as issues |
| Low | {N} | Document as accepted debt |

The codebase has **{N} systemic architectural issues** that account for the majority of Critical findings:

1. **{Issue 1}** — {one-sentence description}
2. **{Issue 2}** — {one-sentence description}
3. **{Issue 3}** — {one-sentence description}

---

## Findings by Anti-Pattern

### AP1: TODO-as-Architecture ({N} Bombs, {N} Debt)

| # | Severity | File:Line | Issue |
|---|----------|-----------|-------|
| 1 | **Critical** | `file.rs:line` | Description |

### AP2: Retry Without Backoff ({N} High, {N} Low)

| # | Severity | File:Line | Issue |
|---|----------|-----------|-------|
| 1 | **High** | `file.rs:line` | Description |

### AP3: No Error Classification ({N} Critical, {N} High, {N} Medium)

| # | Severity | File:Line | Issue |
|---|----------|-----------|-------|

### AP4: Unbounded Accumulation ({N} Critical, {N} High, {N} Medium)

| # | Severity | File:Line | Issue |
|---|----------|-----------|-------|

### AP5: Full-State Retransmission ({N} High, {N} Medium)

| # | Severity | File:Line | Issue |
|---|----------|-----------|-------|

### AP6: Silent Failure Escalation ({N} Critical, {N} High, {N} Medium)

| # | Severity | File:Line | Issue |
|---|----------|-----------|-------|

### AP7: Hot-Path Allocation ({N} Critical, {N} High, {N} Medium)

| # | Severity | File:Line | Issue |
|---|----------|-----------|-------|

### AP8: Configuration Without Constraints ({N} Critical, {N} High, {N} Medium)

| # | Severity | File:Line | Issue |
|---|----------|-----------|-------|

**Positive findings:** (Document what's working well — patterns to replicate)

---

## Priority Fix Order

### Immediate (today)

1. **{Finding ref}: {Short description}** — {fix description, effort estimate}
2. ...

### This week

3. **{Finding ref}: {Short description}** — {fix description, effort estimate}
4. ...

### Next sprint

5. **{Finding ref}: {Short description}** — {fix description, effort estimate}
6. ...

### Accepted debt

- {Finding ref}: {Why this is acceptable} — {monitoring/mitigation in place}

---

## Cross-Cutting Themes

1. **{Theme 1}** — {Pattern that appears across multiple anti-patterns}
2. **{Theme 2}** — {Systemic issue}
3. ...

---

## Coding Style Deficiencies

Map each anti-pattern to the coding style that allowed it:

| Anti-Pattern | Coding Style Deficiency | Systemic Fix |
|-------------|------------------------|-------------|
| AP1 | {e.g., TODOs instead of tracked issues} | {e.g., Deny todo!() in clippy + CI lint} |
| AP2 | {e.g., No retry convention} | {e.g., RetryPolicy trait} |
| ... | ... | ... |

---

## Remediation Roadmap

### Phase 1: Stop the Bleeding (~{time})

| Fix | Findings Resolved | Effort |
|-----|-------------------|--------|
| {Fix description} | {AP#-Severity-#} | {time} |

**Findings resolved: {N} Critical, {N} High**

### Phase 2: Structural Hardening (~{time})

| Fix | Findings Resolved | Effort |
|-----|-------------------|--------|
| {Fix description} | {AP#-Severity-#} | {time} |

**Findings resolved: {N} Critical, {N} High**

### Phase 3: Architectural (~{time})

| Fix | Findings Resolved | Effort |
|-----|-------------------|--------|
| {Fix description} | {AP#-Severity-#} | {time} |

**Findings resolved: {N} Critical, {N} High, {N} Medium**

### Cumulative Impact

| Phase | Critical Resolved | High Resolved | Total |
|-------|-------------------|---------------|-------|
| Phase 1 | {N} / {total} | {N} / {total} | {N} |
| Phase 2 | {N} / {total} | {N} / {total} | {N} |
| Phase 3 | {N} / {total} | {N} / {total} | {N} |

---

## What NOT to Do

1. **Don't fix {N} findings individually.** That's {N} PRs, {N} reviews, {N} chances for regressions. Systemic fixes resolve multiple findings per change.
2. {Other anti-recommendations based on specific codebase}
