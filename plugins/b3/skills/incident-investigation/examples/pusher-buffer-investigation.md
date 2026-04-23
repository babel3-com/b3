# Example: Pusher Buffer Investigation

This is a condensed version of a real investigation that demonstrated the full 10-phase methodology. A daemon accumulated 164MB of terminal output in memory, consuming 38% CPU and 2.6GB RAM.

---

## Phase 1: Open Investigation

**Symptom:** Daemon's pusher connection to server fails with HTTP/2 errors every ~1 second. SSE puller times out every ~60 seconds. 2,861 connection errors since daemon start.
**Impact:** Intermittent browser connection failures, voice delivery shows 0 clients.

## Phase 2: Initial Hypotheses

8 hypotheses were listed with probabilities summing to 100%:

| # | Hypothesis | Initial P |
|---|-----------|-----------|
| H1 | EC2 server rejecting connections (post-deploy regression) | 25% |
| H2 | EC2 relay rate limiting persistent connections | 20% |
| H3 | HTTP/2 multiplexing conflict | 15% |
| H4 | Daemon binary version mismatch (v0.1.167 vs v0.1.190) | 15% |
| H5 | WSL network stack issues | 10% |
| H6 | Memory pressure (2.3GB RAM) | 5% |
| H7 | SSE endpoint broken by v190 deploy | 5% |
| H8 | Systemic server issue affecting all agents | 5% |

## Phase 3: Evidence Collection

8 evidence items collected non-destructively:

- **E2 (SMOKING GUN):** Pusher log showed payload grew from 174 bytes to 164MB. Last successful push: 1.5MB. First failure: 1.7MB (HTTP 413). 46,742 push attempts, 65% failure rate.
- **E4:** Other agents (Agent-B, Agent-C) were NOT affected — the affected agent used 190x more CPU than Agent-C.
- **E5:** EC2 server was healthy, only 20MB RAM. nginx confirmed rejecting 93MB+ payloads.
- **E6:** Source code comment: `// For now, keep growing. TODO: cap at some max size and truncate front.`

## Phase 4: Revised Hypotheses (Post-Evidence)

| # | Hypothesis | Initial P | Revised P |
|---|-----------|-----------|-----------|
| H9 | **Unbounded pusher buffer exceeds nginx limit** | N/A | **95%** |
| H1 | Server rejecting connections | 25% | 3% |
| H2 | EC2 relay rate limiting | 20% | 1% |
| H3 | HTTP/2 conflict | 15% | Subsumed by H9 |
| H4-H8 | All others | 35% | 0-1% |

**Decision:** H9 exceeded 80% from passive evidence alone. The pusher log (E2) was a smoking gun — payload sizes, HTTP 413 responses, and the TODO comment in source code (E6) left no ambiguity. **Skipped Phase 5 (Experimentation)** — passive evidence was conclusive.

## Phase 5: Experimentation (Skipped)

Not needed. H9 at 95% from evidence alone. The pusher log showed the exact growth trajectory (174 bytes → 164MB), the exact failure point (HTTP 413 at 1.7MB), and the source code contained a TODO comment documenting the missing cap. No experiment could add more certainty than the log data already provided.

## Phase 6: Final Hypothesis Revision (Skipped)

H9 remained at 95%. Proceeded directly to blame assignment.

## Phase 7: Blame Assignment

**Level 1 (Lines):** 9 lines identified. Top 4 Critical: unbounded `extend_from_slice`, the TODO comment itself, base64 encode of full buffer on every 100ms tick, and error handler that discards all failure types identically.

**Level 2 (Anti-Patterns):** 8 patterns identified: TODO-as-architecture, retry without backoff, no error classification, unbounded accumulation, full-state retransmission, silent failure escalation, hot-path allocation, configuration without constraints.

**Level 3 (Style):** 8 coding style deficiencies identified: TODO instead of tracked issues, I/O functions returning `()`, debug logging to temp files, no monitoring hooks, magic numbers, what-not-why comments, no failure path tests, unmanaged HTTP client pools.

## Phase 8: Immediate Fix

Added `cap_buffer()` function (truncate front when exceeding 8MB), `PushResult` enum (Ok/PermanentFailure/TransientFailure/NetworkError), exponential backoff with failure classification. 11 tests including failure paths. Committed, deployed to all 4 platforms.

## Phase 9: Anti-Pattern Search

8 parallel agents searched the full codebase. Found 96 instances:
- 20 Critical, 25 High, 30 Medium, 21 Low
- 3 systemic architectural issues: base64-as-storage, no HashMap eviction, fire-and-forget spawns

## Phase 10: Remediation Plan

Three-phase plan:
- **Stop the bleeding (today):** 8 Critical + 2 High fixes (buffer cap, remove todo!() bombs, JWT guard, body limit)
- **Structural hardening (this week):** Shared limits module, server reaper task, puller backoff, health registry, error types
- **Architectural (next sprint):** Raw byte storage + delta protocol, retry middleware, failure test suite

The plan mapped each of the 8 coding style deficiencies to a systemic fix that addresses the entire category (not individual findings), yielding 35+ findings resolved through ~10 changes.

---

## Key Lessons from This Investigation

1. **The TODO was the root cause.** The author knew the buffer needed a cap, wrote a comment, and shipped without it. 7 hours later, 164MB accumulated.
2. **Evidence E4 (other agents unaffected) eliminated 5 hypotheses instantly.** Comparing affected vs. unaffected systems is the highest-value evidence.
3. **The smoking gun was in the log, not the code.** The source code showed the mechanism. The pusher log showed it happening — 174 bytes growing to 164MB with exact timestamps.
4. **Experimentation was unnecessary when evidence was conclusive.** The Phase 5 skip saved time. The decision gate (>80% from passive evidence) correctly identified that experiments would add cost without adding certainty.
5. **The fix template was IN the codebase.** The pusher's `PushResult` enum became the pattern replicated across the codebase. The best remediation comes from finding what's already working and generalizing it.
6. **8 anti-patterns x 1 incident = 96 findings.** The single incident revealed a codebase-wide pattern. Searching for the class (not just the instance) multiplied the value of the investigation 96x.
