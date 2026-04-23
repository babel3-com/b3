---
name: incident-investigation
description: This skill should be used when the user reports a "production incident", "performance issue", "memory leak", "CPU spike", "crash", "service degradation", asks to "investigate a bug", "do a root cause analysis", "figure out what went wrong", "debug this issue", "find the root cause", "do a postmortem", or wants structured hypothesis-driven debugging with formal blame assignment. Provides a 10-phase investigation methodology with evidence preservation, experimentation, anti-pattern search, and comprehensive remediation planning.
version: 0.3.0
---

# Incident Investigation Methodology

A structured, hypothesis-driven approach to debugging production incidents. Produces a formal investigation file, assigns blame at three levels (lines of code, anti-patterns, coding style), fixes the immediate cause, then searches the codebase for the same class of bug. Web research is mandatory at every phase — search before experimenting, search after every new finding.

## When to Use

Invoke this methodology when a system exhibits unexpected behavior under load, over time, or after deployment — not for logic bugs caught in development. The distinguishing feature is that the symptom is separated from the cause by time, scale, or indirection.

## Core Principles

1. **Evidence before experiments.** Collect all available telemetry, logs, metrics, and source code before running any experiment. Experiments change state. Evidence collection does not.
2. **Hypotheses carry probabilities.** Every hypothesis gets an initial probability estimate (0-100%). Probabilities force honest assessment of what is known vs. assumed.
3. **Hypotheses must span multiple categories.** Ensure initial hypotheses cover at least 3 different root cause categories (e.g., scroll, CSS/layout, platform behavior, timing, rendering). Monoculture hypotheses are the single biggest risk to an investigation — they channel all effort into one frame while the real cause sits in an unexplored category.
4. **Three strikes triggers a mandatory category pivot.** After three failed experiments within the same hypothesis category, STOP and execute the Category Pivot Protocol (see `references/assumption-lock-in.md`). Repeated failure is evidence against the entire mental model, not just the specific implementation. This rule is non-negotiable — it exists because the natural tendency is to refine within a familiar frame rather than question the frame itself.
5. **Blame ascends three levels.** The line of code is the proximate cause. The anti-pattern is why that line was written. The coding style deficiency is why the anti-pattern wasn't caught.
6. **Web research is mandatory, not optional.** Search the web at every phase transition — after forming hypotheses, after collecting evidence, after each experiment, after each new finding. Error messages, library versions, platform limitations, and known bugs are often already documented. Experimenting without searching first wastes hours rediscovering what the community already knows. Launch background research agents for parallel searches.
7. **Fix the instance, then search for the class.** After fixing the specific bug, search the entire codebase for the same anti-pattern. The bug that caused this incident exists elsewhere.
8. **The investigation file is the artifact.** Every phase writes to a single markdown file. The file IS the deliverable — it preserves reasoning, evidence, and decisions for future investigators.

## The 10 Phases

### Phase 1: Open Investigation

Create a formal investigation file in the project's investigations directory.

```
investigations/{slug}.md
```

Record:
- **Symptom** — observable behavior (exact error messages, metrics, user-visible impact)
- **Impact** — what is broken, who is affected, severity estimate
- **Timeline** — when it started, what changed recently (deploys, config changes, traffic patterns)

See `references/investigation-template.md` for the complete file template.

### Phase 2: Initial Hypotheses

Before looking at any evidence, list 5-8 hypotheses with probability estimates. This prevents anchoring — the first log line seen shouldn't determine the conclusion.

| # | Hypothesis | Initial P | Reasoning |
|---|-----------|-----------|-----------|
| H1 | Description | 25% | Why this might be true |
| H2 | Description | 20% | Why this might be true |

**Probability discipline:** Estimates must sum to ~100% (±10%). Include an "other/unknown" hypothesis if needed. Force-ranking prevents the common mistake of investigating only the most obvious hypothesis.

**Hypothesis quality:** Each hypothesis should be falsifiable by specific evidence. "Something is wrong with the server" is not a hypothesis. "Server is rejecting connections due to thread pool exhaustion" is.

**Category diversity check:** Label each hypothesis with its root cause category (e.g., "scroll," "CSS/layout," "platform," "timing," "memory," "config"). If all hypotheses share a category, add at least 2 from different categories before proceeding. An investigation that starts with monoculture hypotheses will waste effort refining within the wrong frame when it fails.

### Phase 3: Evidence Collection (Non-Destructive)

Gather evidence WITHOUT running experiments. Read logs, check metrics, inspect source code, query databases (read-only). Do NOT restart services, change config, or deploy fixes.

Organize evidence as numbered items:

```
### E1: {Evidence Title}
- What was observed (exact values, timestamps, quotes from logs)
- What it implies for each hypothesis
- Confidence in this evidence (high/medium/low — logs can lie)
```

**Evidence sources (priority order):**
1. Application logs (timestamped, grep for error signatures)
2. System metrics (CPU, memory, disk, network — check the specific process, not just the host)
3. Source code (the code that's actually running, not what's on main — check deployed version)
4. Other instances/agents (is this instance-specific or systemic?)
5. External dependencies (upstream services, DNS, CDN, reverse proxy logs)
6. Configuration state (what config is actually loaded, not what's in the file)
7. **Web research (MANDATORY)** — Search for every error message, library name, and platform behavior in the hypothesis list. Launch background research agents in parallel — one per hypothesis category. Search for: known bugs, correct configuration, API documentation, community reports, Stack Overflow, GitHub issues. Document findings as evidence items. This is not optional — it runs at this phase AND repeats after every subsequent phase that produces new information.

**Web research protocol:**
- Search for exact error messages in quotes
- Search for library name + version + symptom
- Search for platform + behavior (e.g., "iOS AVAudioEngine echo cancellation")
- Search for configuration keywords (e.g., "coturn denied-peer-ip AWS")
- Launch multiple background agents for parallel research
- Document each finding as an evidence item (E-prefixed) with source URL

**Critical rule:** If any evidence would be destroyed by restarting the service, capture it FIRST. Pipe logs, screenshot dashboards, dump process state. The investigation file preserves what the restart erases.

### Phase 4: Revised Hypotheses (Post-Evidence)

Update the hypothesis table with post-evidence probabilities. Add new hypotheses that emerged from evidence. Mark hypotheses as confirmed, eliminated, or subsumed.

| # | Hypothesis | Initial P | Revised P | Key Evidence |
|---|-----------|-----------|-----------|-------------|
| H1 | ... | 25% | 5% | E3 contradicts |
| H9 | **New: root cause** | N/A | 70% | E2, E5, E6 suggest |

**Web research checkpoint (MANDATORY):** Before proceeding to any next phase, search the web for the leading hypotheses. The answer may already exist — a documented bug, a configuration guide, a WWDC session, a GitHub issue. Searching takes minutes; experimenting takes hours.

If one hypothesis exceeds 80%, proceed directly to Phase 6 (Blame Assignment) — experimentation is unnecessary when passive evidence is conclusive. If the leading hypothesis is between 50-80%, design experiments to confirm or refute it. If no hypothesis exceeds 50%, more evidence is needed before experimenting — return to Phase 3.

### Phase 5: Experimentation

Design and execute targeted experiments to test the leading hypotheses. Unlike Phase 3 (passive observation), experiments actively change system state. Each experiment must be:

- **Targeted** — tests exactly one hypothesis
- **Reversible** — system returns to its prior state afterward (or the experiment is run on a replica)
- **Documented** — predicted outcome recorded BEFORE execution, actual outcome recorded AFTER

```
### X1: {Experiment Title}
- **Tests:** H{N} ({hypothesis name})
- **Predicted outcome if H{N} is true:** {what should happen}
- **Predicted outcome if H{N} is false:** {what should happen instead}
- **Procedure:** {exact steps}
- **Actual outcome:** {what happened}
- **Conclusion:** {confirms/refutes H{N}, revised probability}
```

**Experiment design principles:**

1. **Predict before executing.** Write the expected outcome for both "hypothesis true" and "hypothesis false" BEFORE running the experiment. This prevents post-hoc rationalization — interpreting ambiguous results as confirming whatever was already believed.
2. **Smallest possible intervention.** A restart clears all state. A config change affects one variable. Prefer the smallest intervention that distinguishes the hypotheses.
3. **Preserve the crime scene.** If the experiment will destroy evidence (e.g., restarting a service clears in-memory state), capture all remaining evidence from Phase 3 first. Document what was lost.
4. **One variable at a time.** Changing two things simultaneously makes it impossible to attribute the result. If an experiment requires multiple changes, break it into sequential sub-experiments.
5. **Replicas over production.** When possible, reproduce the issue in a test environment and experiment there. When production experimentation is unavoidable, prefer read-only probes (health checks, test requests to non-critical endpoints) over mutations.

**Three-strike check (MANDATORY):** Before designing each experiment, count how many previous experiments targeted the same hypothesis category. If the count is 3 or more, STOP — do not design another experiment in this category. Instead, execute the Category Pivot Protocol from `references/assumption-lock-in.md`. Write in the investigation file: "Three strikes reached for category [X]. Pivoting." Then generate hypotheses from at least 2 new categories before continuing.

**Common experiment types:**
- **Restart test** — does restarting the service clear the symptom? (Tests: transient state accumulation vs. persistent misconfiguration)
- **Isolation test** — does the issue reproduce with one client? (Tests: load-dependent vs. client-specific)
- **Config change** — does adjusting a limit/timeout resolve the symptom? (Tests: specific config hypothesis)
- **Curl probe** — does a direct HTTP request to the endpoint succeed? (Tests: client-side vs. server-side)
- **Reproduction** — can the issue be triggered in a controlled environment? (Tests: environmental vs. code-level)

### Phase 6: Final Hypothesis Revision

Update the hypothesis table a second time with post-experiment probabilities. The experiment results should push the leading hypothesis above 90% or eliminate it and promote an alternative.

| # | Hypothesis | Initial P | Post-Evidence P | Post-Experiment P | Key Experiment |
|---|-----------|-----------|-----------------|-------------------|---------------|
| H1 | ... | 25% | 5% | 2% | X1 refutes |
| H9 | **Root cause** | N/A | 70% | **95%** | X2 confirms |

**Web research checkpoint (MANDATORY):** After each experiment, search the web for the specific behavior observed. Unexpected results often match known bugs or documented platform behavior. An experiment result of "still broken after fix" is a search query — someone else likely hit the same wall.

**Decision gate:** Proceed to Phase 7 (Blame Assignment) only when one hypothesis exceeds 90%. If experimentation was inconclusive, design additional experiments or gather more evidence. Do not assign blame to an unconfirmed hypothesis — a wrong root cause leads to wrong fixes.

### Phase 7: Blame Assignment (Three Levels)

This is the diagnostic core. Assign blame at three ascending levels of abstraction:

**Level 1: Lines of Code** — Which specific lines are responsible? Create a table with file:line, the code, why it's wrong, and severity (Critical/High/Medium/Low).

**Level 2: Anti-Patterns** — What programming pattern made those lines possible? Common categories:

See `references/anti-pattern-catalog.md` for the 8 standard anti-patterns and their search strategies.

**Level 3: Coding Style** — What development practice allowed those anti-patterns to persist? Examples: no failure-path tests, functions returning `()` instead of `Result`, debug logging to temp files, health endpoints that lie.

The three levels produce three different fix scopes:
- Level 1 → fix the specific lines (minutes to hours)
- Level 2 → search for and fix all instances of the anti-pattern (hours to days)
- Level 3 → change development practices to prevent recurrence (ongoing)

### Phase 8: Immediate Fix

Fix the code-level blame (Level 1) immediately. The investigation file documents what to change.

- Write the fix
- Write tests for the failure path (not just the happy path — the failure path is what broke)
- Commit with a reference to the investigation file
- Verify the fix resolves the original symptom

The fix should be minimal and targeted — address exactly the lines identified in Level 1 blame. Broader improvements belong in the remediation plan (Phase 10), not the incident fix.

### Phase 9: Anti-Pattern Search

Search for the anti-pattern (Level 2) across the entire codebase. Launch parallel search agents — one per anti-pattern identified. Each agent:

1. Receives specific grep/search queries from `references/anti-pattern-catalog.md`
2. Classifies each finding by severity (Critical/High/Medium/Low)
3. Returns a structured findings list

Merge results into a unified audit report. See `references/audit-report-template.md` for the format.

This phase is distinct from the fix because it operates at a different scope — the fix addresses the specific incident, the search addresses the class of bug. The search often reveals 10-100x more findings than the original incident.

### Phase 10: Comprehensive Remediation Plan

Map each coding style deficiency (Level 3) to a systemic fix that addresses the entire category, not just individual findings. The plan should:

1. **Prioritize by severity** — Critical findings first, accepted debt last
2. **Phase by effort** — immediate (today), this week, next sprint, accepted debt
3. **Account for codebase evolution** — later phases assume earlier phases are complete
4. **Include "what NOT to do"** — prevent over-engineering and lateral moves

Structure the plan as:
- Phase 1: Stop the bleeding (Critical fixes, <1 day)
- Phase 2: Structural hardening (High fixes + preventive tooling, ~3 days)
- Phase 3: Architectural (systemic redesign, ~1 week)
- Accepted debt: Findings that are correct for context

## Running Anti-Pattern Searches

Launch parallel Task agents (one per anti-pattern) using the `Explore` subagent type. Provide each agent with:
- The anti-pattern name and definition
- Specific search queries (grep patterns, file globs)
- Classification criteria (what makes a finding Critical vs. Low)
- The codebase root path

Example prompt for an agent:

```
Search the codebase at {path} for Anti-Pattern: {name}.
{definition}

Search queries:
{queries}

For each finding, classify severity:
- Critical: can cause production failure
- High: will cause issues under load or over time
- Medium: suboptimal but not dangerous
- Low: accepted debt, document reasoning

Return a markdown table: | # | Severity | File:Line | Issue |
```

## Additional Resources

### Reference Files

- **`references/investigation-template.md`** — Complete investigation file template with all sections pre-formatted. Copy this to start a new investigation.
- **`references/anti-pattern-catalog.md`** — The 8 standard anti-patterns with definitions, search strategies (grep queries), and classification criteria.
- **`references/audit-report-template.md`** — Template for the unified audit report that merges findings from parallel search agents.
- **`references/assumption-lock-in.md`** — The Category Pivot Protocol. How to recognize when an investigation is stuck in the wrong hypothesis category, the three-strike rule, and the real-world iOS terminal investigation that motivated this guidance.

### Example Files

- **`examples/pusher-buffer-investigation.md`** — A real investigation of a 164MB buffer accumulation incident that demonstrated this full methodology. Shows hypotheses evolving from 8 candidates to a single root cause with 95% confidence.
