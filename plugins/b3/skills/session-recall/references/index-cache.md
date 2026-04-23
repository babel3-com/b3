# Session Index Cache

Persistent storage for session map indexes so they don't need to be regenerated every recall.

## Storage Location

```
~/.b3/session-indexes/
```

This directory is created on first use. Each indexed session gets one file.

## File Naming

```
{session_id}.index.md
```

Example: `e01ff420-5820-4af0-b0ab-14fc0e922c7d.index.md`

## File Format

```markdown
---
session_id: {uuid}
session_file: {absolute path to .jsonl}
indexed_line_count: {line_count at time of indexing}
indexed_size_mb: {size_mb at time of indexing}
indexed_at: {ISO 8601 timestamp}
sample_rate: {frac_sample used}
last_timestamp: {last_timestamp from list_sessions, or null}
---

## Session Map

L1-L5000 (Mar 23): GPU WebSocket reliability, iOS Safari pings
L5000-L12000 (Mar 24): STT streaming, garbage filter, sidecar signaling
L12000-L18000 (Mar 25-26): iOS native audio, Safari freeze investigation
...
```

## Staleness Detection

A cached index is **stale** when the session has grown since indexing. Compare:

```
current_line_count > indexed_line_count
```

Line count is the authoritative staleness signal because:
- Sessions only append (JSONL), never shrink
- `list_sessions()` returns `line_count` and `size_mb` for every session
- A session that hasn't grown has the same content — the index is still valid

When stale, only the **new portion** needs indexing:

```
resurface(start_line=indexed_line_count+1, end_line=current_line_count, ...)
```

The new entries are appended to the existing session map, and `indexed_line_count` is updated.

## Freshness Check Algorithm

```
for each session from list_sessions():
    index_file = ~/.b3/session-indexes/{session_id}.index.md
    if index_file does not exist:
        status = "unindexed"
    else:
        parse indexed_line_count from frontmatter
        if session.line_count > indexed_line_count:
            status = "stale"
            new_lines = session.line_count - indexed_line_count
        else:
            status = "fresh"
```

## Incremental Update

When a session is stale:

1. Read the existing index file
2. Call `resurface()` on only the new line range (indexed_line_count+1 to current_line_count)
3. Have the subagent produce map entries for the new range only
4. Append the new map entries to the `## Session Map` section
5. Update `indexed_line_count`, `indexed_size_mb`, and `indexed_at` in frontmatter

This means a 535MB session that grew by 5000 lines only needs those 5000 lines indexed, not a full re-scan.

## Example Workflow

```
# Step 1: Check cache
list_sessions(min_size_mb=1) → get current line counts

# Step 2: For each session, check index
Read ~/.b3/session-indexes/{id}.index.md
Compare indexed_line_count vs current line_count

# Step 3: Only re-index what changed
- Fresh sessions: read cached map directly
- Stale sessions: incremental index of new lines only
- Unindexed sessions: full index (frac_sample 0.01-0.03)

# Step 4: Merge all maps and proceed to Step 2 (Scope) of recall protocol
```
