# Anti-Pattern Catalog

8 standard anti-patterns that cause production incidents in long-running services. Each entry includes: definition, why it's dangerous, search strategy (grep queries), and severity classification criteria.

---

## AP1: TODO-as-Architecture

**Definition:** TODO/FIXME/HACK comments marking known bugs or missing safety mechanisms that were shipped to production. The `todo!()` macro in Rust panics at runtime.

**Why dangerous:** A TODO for a missing cap, limit, or validation is a documented time bomb. The author knew the system was unsafe and deferred the fix. Every TODO is a bet that the failure mode won't happen before someone fixes it.

**Search queries:**
```bash
# Panicking macros (Rust)
grep -rn 'todo!()\|unimplemented!()\|unreachable!()' --include='*.rs'

# Safety-related TODOs (any language)
grep -rn 'TODO.*cap\|TODO.*limit\|TODO.*bound\|TODO.*safe\|TODO.*max\|FIXME.*crash\|FIXME.*panic\|HACK\|XXX\|TEMPORARY\|WORKAROUND' --include='*.rs' --include='*.py' --include='*.ts' --include='*.js'
```

**Classification:**
- **Critical (Bomb):** TODO documents a missing safety mechanism on a reachable code path (missing size cap, missing auth check, missing rate limit)
- **High:** TODO on a code path that's reachable but not frequently exercised (admin endpoints, error recovery paths)
- **Medium (Debt):** TODO documents a missing optimization or cleanup that won't cause failure
- **Low (Inert):** TODO in dead code (`#[allow(dead_code)]`, behind feature flag), or documents a nice-to-have

---

## AP2: Retry Without Backoff

**Definition:** Loops that retry failed operations at a fixed interval regardless of failure type, failure count, or error classification.

**Why dangerous:** Turns a transient failure into a permanent resource drain. A server returning 503 gets hammered at the same rate as a healthy server. A payload that's permanently too large (413) gets retried identically forever.

**Search queries:**
```bash
# Fixed-interval retry loops
grep -rn 'loop\s*{' --include='*.rs' --include='*.py' -A 20 | grep -B 5 'sleep\|Duration::from\|time\.sleep\|asyncio\.sleep'

# Reconnect patterns without backoff
grep -rn 'reconnect\|retry\|retry_count\|attempt' --include='*.rs' --include='*.py'
```

**What to check for each hit:**
- Is there a backoff multiplier? (interval should grow: 1s, 2s, 4s, 8s...)
- Is there a max retry count or circuit breaker?
- Does it distinguish permanent vs. transient failures?
- Does it log/alert after N consecutive failures?

**Classification:**
- **Critical:** Unbounded retry at fixed interval on a hot path (< 1s interval, no max retries, no failure classification)
- **High:** Fixed retry on a moderate path (1-10s interval), or unbounded retry with >10s interval
- **Medium:** Bounded retry (max N attempts) at fixed interval — suboptimal but won't run forever
- **Low:** Fixed retry that's appropriate for context (process monitor checking every 30s)

---

## AP3: No Error Classification

**Definition:** Functions that perform I/O (network, disk, database) but return `()`, `bool`, or an untyped error that prevents the caller from distinguishing failure types.

**Why dangerous:** The caller can't choose a recovery strategy. HTTP 413 (permanent — payload too large) gets the same treatment as HTTP 503 (transient — server overloaded). Database errors get treated as "invalid input."

**Search queries:**
```bash
# Async functions returning () or bool (Rust)
grep -Pn 'pub async fn \w+\(.*\)\s*(\{|$)' --include='*.rs' -r | grep -v 'test\|#\[cfg(test)\]' | grep -v '-> '

# Error handlers that swallow the error type
grep -rn 'Err\(_\)\s*=>\|Err\(e\)\s*=>' --include='*.rs' -A 3 | grep -v 'return\|propagate\|?'

# Fire-and-forget spawns
grep -rn 'tokio::spawn\|let _ =' --include='*.rs'
```

**Classification:**
- **Critical:** I/O function returns `()` where the caller needs to distinguish success/failure (message delivery, auth validation, resource registration)
- **High:** Function returns `bool` or `Option` that collapses multiple failure modes into a single signal
- **Medium:** Function uses `anyhow::Error` where typed errors would enable caller decisions
- **Low:** Function where all errors genuinely have the same recovery strategy

---

## AP4: Unbounded Accumulation

**Definition:** Data structures that grow without limit based on external input. Vectors, HashMaps, Strings, channels, or buffers that only grow and never shrink, evict, or cap.

**Why dangerous:** In a long-running service, ANY unbounded accumulation is a memory leak. The growth rate depends on external input (user traffic, event frequency), making it impossible to predict when memory exhaustion occurs. It may take hours, days, or weeks.

**Search queries:**
```bash
# Growing collections (Rust)
grep -rn '\.push(\|\.extend\|\.extend_from_slice\|\.append(\|\.insert(' --include='*.rs'

# Check: does the growing collection have a corresponding shrink/evict/cap?
grep -rn '\.drain\|\.truncate\|\.clear\|\.remove\|\.retain\|cap_buffer\|evict\|reap' --include='*.rs'

# Unbounded channels
grep -rn 'unbounded_channel\|unbounded()' --include='*.rs'

# HashMaps that may grow without eviction
grep -rn 'HashMap::new\|DashMap::new\|RwLock<HashMap' --include='*.rs'
```

**For each growing collection, verify:**
1. Is it inside a loop or handler (grows per-event)?
2. Is there a corresponding drain/truncate/cap?
3. Is the growth rate bounded by internal logic or unbounded by external input?
4. In a HashMap: is there eviction for old/expired entries?

**Classification:**
- **Critical:** Unbounded growth on a hot path (per-request, per-event) with no cap — will cause OOM in production
- **High:** Unbounded growth on a moderate path (per-connection, per-session) — will cause OOM eventually
- **Medium:** Bounded channel with high capacity (>512) for large events — unlikely but possible
- **Low:** Growth bounded by internal logic (fixed number of subsystems, known-size config)

---

## AP5: Full-State Retransmission

**Definition:** Sending the entire accumulated state on every update instead of just the changes (delta). Network and CPU cost grows linearly with state size and update frequency.

**Why dangerous:** A system that pushes 1KB initially and grows to 100MB sends 100MB every update interval. Combined with a tight interval (100ms), this creates gigabytes of network traffic per second for data that's 99.99% unchanged.

**Search queries:**
```bash
# POST/PUT endpoints that send accumulated data
grep -rn '\.post(\|\.put(\|\.json(\|\.send(' --include='*.rs' --include='*.py'

# Buffer/state being encoded in full
grep -rn 'encode\|to_string\|serialize\|to_json\|json\.dumps' --include='*.rs' --include='*.py'
```

**For each POST/send, check:**
- Is it sending a growing blob or a fixed/delta-sized payload?
- Does the payload size grow with time/usage?
- Could it send only what changed since the last successful send?

**Classification:**
- **High:** Full state sent on every update interval where delta would work (>1KB growing payloads)
- **Medium:** Full state sent on specific triggers (reconnect, resize) — may be correct for initial sync
- **Low:** Full state is small and bounded — overhead is negligible

---

## AP6: Silent Failure Escalation

**Definition:** Failures that are logged to files nobody monitors, with no user-visible signal, no metric increment, no health degradation, and no alert. The system appears healthy while silently failing.

**Why dangerous:** Without observable failure, there is no feedback loop. The operator sees "everything is fine" while data is being lost, messages aren't delivering, or resources are leaking. The incident is only discovered when a user reports it — hours or days later.

**Search queries:**
```bash
# Logging to temp files (bypasses monitoring)
grep -rn 'debug_log\|OpenOptions.*append\|writeln.*log\|write.*\.log\|/tmp/' --include='*.rs' --include='*.py'

# Health endpoints that don't check anything
grep -rn '/health\|/status\|/ready\|healthz\|readyz' --include='*.rs' --include='*.py' -A 10

# Discarded results on critical operations
grep -rn 'let _ = .*send\|let _ = .*execute\|let _ = .*write\|let _ = .*delete' --include='*.rs'

# tracing::warn/error in loops (noise that gets ignored)
grep -rn 'tracing::warn\|tracing::error\|logging\.warning\|logging\.error' --include='*.rs' --include='*.py'
```

**Classification:**
- **Critical:** Health endpoint returns "ok" without checking subsystem health; critical operation (message delivery, payment, auth) silently discards failures
- **High:** Fire-and-forget spawn on an operation that should have delivery guarantees; logging to unmonitored location
- **Medium:** Warning in a loop without rate limiting (becomes noise); error logged but no escalation path
- **Low:** Benign silent drop (broadcast with no subscribers)

---

## AP7: Hot-Path Allocation

**Definition:** Allocating large or frequent heap objects on performance-critical paths (inside tight loops, on every request, on every timer tick).

**Why dangerous:** Memory allocation is not free. Allocating 200MB every 100ms means the allocator processes 2GB/second of allocations and deallocations. This consumes CPU, fragments the heap, and in garbage-collected languages triggers GC pauses.

**Search queries:**
```bash
# Base64 encoding (creates new String proportional to input)
grep -rn 'base64.*encode\|btoa\|b64encode' --include='*.rs' --include='*.py' --include='*.js'

# String formatting in loops
grep -rn 'format!\|\.to_string()\|String::from\|\.clone()' --include='*.rs'

# JSON serialization in loops
grep -rn 'serde_json::to_string\|serde_json::to_vec\|json\.dumps\|JSON\.stringify' --include='*.rs' --include='*.py' --include='*.js'

# Regex compilation in hot paths (should be compiled once)
grep -rn 'Regex::new\|re\.compile' --include='*.rs' --include='*.py'
```

**For each hit on a hot path (inside a loop, in a frequently-called handler):**
- Does it allocate proportional to input size?
- Is the input size bounded?
- Could it reuse a buffer, encode in-place, or compute once (OnceLock/lazy_static)?

**Classification:**
- **Critical:** Full-buffer encode/decode on every tick of a tight loop (100ms-1s interval), allocation proportional to unbounded input
- **High:** Per-request serialization of large objects; regex compilation per call
- **Medium:** String clone/format per request for small bounded strings (agent_id, auth header)
- **Low:** One-time allocation during startup or on rare events

---

## AP8: Configuration Without Constraints

**Definition:** Configuration values that interact with runtime state but have no validation, bounds checking, or documentation of safe ranges. Constants that couple across deployment boundaries (client ↔ server ↔ reverse proxy) without co-location or compile-time verification.

**Why dangerous:** A buffer size constant in the client depends on a body size limit in nginx depends on a timeout in the upstream proxy. Change one without updating the others and the system breaks — but only under load, making it hard to test.

**Search queries:**
```bash
# Config struct fields
grep -rn 'pub.*interval\|pub.*timeout\|pub.*max\|pub.*limit\|pub.*size' --include='*.rs' --include='*.py' | grep -i 'config\|setting'

# Hardcoded limits that should be documented
grep -rn 'const.*TIMEOUT\|const.*LIMIT\|const.*MAX\|const.*INTERVAL\|const.*SIZE\|const.*CAPACITY' --include='*.rs' --include='*.py'

# Security-sensitive defaults
grep -rn 'default.*secret\|default.*password\|default.*key\|unwrap_or.*secret' --include='*.rs' --include='*.py'
```

**For each config value, check:**
- Is it validated on load (min/max bounds)?
- Does it interact with a system limit in another component?
- Is there documentation about safe ranges?
- Is the default value safe for production?

**Classification:**
- **Critical:** Security-sensitive default that's unsafe in production (default JWT secret, no body size limit); cross-component coupling without compile-time verification
- **High:** Config value with no validation (could be zero, negative, or absurdly large); coupled constants in different repos
- **Medium:** Hardcoded value that should be configurable but has a reasonable default
- **Low:** Constant that's correct and documented, just not in a shared location
