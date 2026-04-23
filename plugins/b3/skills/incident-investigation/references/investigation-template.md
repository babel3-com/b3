# Investigation: {Title}

**Opened:** {DATE} UTC
**Symptom:** {One-sentence description of the observable behavior}
**Impact:** {Who/what is affected and how severely}
**Timeline:** {When it started, what changed recently}

## Error Signatures

```
{Paste exact error messages, stack traces, log excerpts}
```

## Initial Hypotheses (Pre-Evidence)

List 5-8 hypotheses BEFORE examining evidence. Each must be falsifiable by specific evidence. Probabilities should sum to ~100%.

| # | Hypothesis | Initial P | Reasoning |
|---|-----------|-----------|-----------|
| H1 | **{Short name}** — {detailed description} | ??% | {Why this might be the cause} |
| H2 | **{Short name}** — {detailed description} | ??% | {Why this might be the cause} |
| H3 | **{Short name}** — {detailed description} | ??% | {Why this might be the cause} |
| H4 | **{Short name}** — {detailed description} | ??% | {Why this might be the cause} |
| H5 | **{Short name}** — {detailed description} | ??% | {Why this might be the cause} |

## Evidence Collection Plan

List what to gather WITHOUT running experiments:

1. {Evidence source} — {what to look for}
2. {Evidence source} — {what to look for}
3. ...

### Web Research Queries (MANDATORY)

Launch background research agents for these searches:

1. {Error message or symptom} — {what to search for}
2. {Library/platform + behavior} — {what to search for}
3. ...

---

## Evidence Collected

### E1: {Evidence Title}
- {What was observed — exact values, timestamps, quotes}
- {Implication for hypotheses}

### E2: {Evidence Title}
- {What was observed}
- {Implication}

### E3: {Evidence Title}
- {What was observed}
- {Implication}

(Continue as needed — number sequentially)

---

## Revised Hypotheses (Post-Evidence)

| # | Hypothesis | Initial P | Revised P | Key Evidence |
|---|-----------|-----------|-----------|-------------|
| H1 | ... | ??% | ??% | {E# that supports/contradicts} |
| H2 | ... | ??% | ??% | {E# that supports/contradicts} |
| **H{N}** | **{New hypothesis from evidence}** | N/A | ??% | {E# that supports} |

**Decision:** If leading hypothesis > 80%, skip to Blame Assignment. If 50-80%, proceed to Experimentation. If < 50%, gather more evidence.

---

## Experimentation

Design and execute targeted experiments to confirm or refute the leading hypotheses.

### X1: {Experiment Title}
- **Tests:** H{N} ({hypothesis name})
- **Predicted outcome if H{N} is true:** {what should happen}
- **Predicted outcome if H{N} is false:** {what should happen instead}
- **Procedure:** {exact steps — must be reversible or run on a replica}
- **Actual outcome:** {what happened}
- **Conclusion:** {confirms/refutes H{N}, revised probability}

### X2: {Experiment Title}
- **Tests:** H{N}
- **Predicted if true:** {expected}
- **Predicted if false:** {expected}
- **Procedure:** {steps}
- **Actual outcome:** {result}
- **Conclusion:** {verdict}

(Continue as needed)

---

## Final Hypotheses (Post-Experiment)

| # | Hypothesis | Initial P | Post-Evidence P | Post-Experiment P | Key Experiment |
|---|-----------|-----------|-----------------|-------------------|---------------|
| H1 | ... | ??% | ??% | ??% | {X# that confirms/refutes} |
| **H{N}** | **Root cause** | N/A | ??% | **??%** | {X# that confirms} |

**Decision gate:** Proceed to Blame Assignment only when one hypothesis exceeds 90%.

## Root Cause

**{One-paragraph summary of the confirmed root cause.}**

The fix has two parts:
1. **Immediate:** {What to change right now}
2. **Structural:** {What to change to prevent recurrence}

## Blame Assignment

### Level 1: Lines of Code

| Line | File | Blame | Severity |
|------|------|-------|----------|
| **{line#}** | `{file}` | `{code snippet}` — {why it's wrong} | **CRITICAL/HIGH/MEDIUM/LOW** |

### Level 2: Programming Anti-Patterns

| Anti-Pattern | Where | Impact |
|-------------|-------|--------|
| **{Pattern name}** | {file:lines} | {Why this pattern is dangerous and how it manifested here} |

### Level 3: Coding Style Issues

| Style Issue | Evidence | Why It Matters |
|------------|----------|----------------|
| **{Style issue}** | {Where observed} | {Why this practice allowed the bug to exist} |

## Fix Applied

**Commit:** {hash}
**Changes:**
- {file}: {what changed}
- {file}: {what changed}

**Tests added:**
- {test name}: {what it verifies}

## Anti-Pattern Search Results

{Link to or include the unified audit report from parallel search agents}

## Remediation Plan

{Link to or include the phased fix plan}
