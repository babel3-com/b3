---
name: session-recall
description: This skill should be used when the user asks to "recall something from a previous session", "find what we discussed", "search session history", "what did we do last week", "find that conversation about X", "resurface that decision", "when did we work on X", "remember that bug we fixed", or when recalling specific decisions, discussions, code changes, or events from past Claude Code sessions. Provides an 8-step structured recall protocol using the b3 MCP's recall tools (`list_sessions`, `resurface`, etc.).
version: 0.1.0
---

# Session Recall Protocol

Structured 8-step protocol for recalling information from past Claude Code sessions. Uses the b3 MCP's recall tools (`list_sessions`, `resurface`) combined with background subagents and search tools to efficiently locate specific moments across sessions that may span hundreds of thousands of lines.

## Prerequisites

The b3 MCP must be loaded (via `claude --dangerously-load-development-channels server:b3`). Verify by calling `list_sessions(limit=1)`. If unavailable, have the agent run `b3 update --force` and restart their session.

## The 8-Step Recall Protocol

### Step 1: Index — Build a Session Map (with Persistent Cache)

Session indexes are cached at `~/.b3/session-indexes/` so they survive across conversations. See `references/index-cache.md` for the full format spec.

**1a. Check cache freshness:**

```
list_sessions(min_size_mb=1) → get current line counts for each session
```

For each session, read `~/.b3/session-indexes/{session_id}.index.md`:
- **Fresh** (`indexed_line_count == current line_count`): Use cached map directly. No subagent needed.
- **Stale** (`indexed_line_count < current line_count`): Incremental index — only scan new lines.
- **Unindexed** (no cache file): Full index from scratch.

**1b. Index unindexed sessions:**

Launch a **background sonnet subagent** per unindexed session. The subagent calls `resurface()` with:
- `frac_sample`: 0.01-0.03 (low sample rate for overview)
- `record_type`: "conversation"
- `max_word_length`: 60
- Full line range (1 to line_count)

The subagent returns a **session map** and writes it to `~/.b3/session-indexes/{session_id}.index.md` with frontmatter:

```markdown
---
session_id: {uuid}
session_file: {absolute path}
indexed_line_count: {line_count}
indexed_size_mb: {size_mb}
indexed_at: {ISO 8601}
sample_rate: {frac_sample}
last_timestamp: {from list_sessions}
---

## Session Map

L1-L5000 (Mar 23): GPU WebSocket reliability, iOS Safari pings
L5000-L12000 (Mar 24): STT streaming, garbage filter, sidecar signaling
L12000-L18000 (Mar 25-26): iOS native audio, Safari freeze investigation
...
```

**1c. Incrementally update stale sessions:**

For stale sessions, launch a subagent that scans only the new lines:
- `start_line`: `indexed_line_count + 1`
- `end_line`: current `line_count`
- `frac_sample`: 0.02-0.05 (higher rate since range is smaller)

Append new map entries to the existing index file and update the frontmatter (`indexed_line_count`, `indexed_size_mb`, `indexed_at`).

**1d. Assemble the combined map:**

Read all relevant cached index files (fresh + newly updated) and merge into a single session map for scoping.

### Step 2: Scope — Identify Target Ranges and Record Types

Based on the session map and the user's query, identify:

1. **Which line ranges** are likely to contain the answer (narrow from full session to relevant periods)
2. **Which record types** to extract:
   - `conversation` — discussions, decisions, voice messages
   - `tool_use` — all tool calls (good for finding what was tried)
   - `bash` — commands run (good for deploy/build history)
   - `write` — files created (good for "where did I put that file")
   - `read` — files read (good for "what did we look at")
   - `grep` — searches performed (good for "what were we searching for")

**Present the scoping plan to the user for review.** Example:

> Based on the session map, the WebRTC work was in L12000-L18000 (Mar 25-26). I'll extract `conversation` and `bash` records from that range. Does that sound right, or should I widen the search?

### Step 3: Dump — Extract to Output Files

Create a working directory for this recall:

```
~/tmp/recall-{timestamp}/
```

Call `resurface()` for **each record type and line range** with `output_file` set to a descriptive filename:

```
recall-{ts}/conversation-L12000-L18000.txt
recall-{ts}/bash-L12000-L18000.txt
recall-{ts}/write-L12000-L18000.txt
```

Use `frac_sample: 1.0` for the scoped ranges (full extraction, not sampled — the ranges are already narrowed). Set `max_word_length` to 100-150 for search-friendly output.

Run multiple resurface calls in parallel when extracting different record types from the same range.

### Step 4: Prompt — Draft Search Strategy

Draft a search prompt for the subagent that will search the output files. Include:

- What the user is looking for (in their words)
- Suggested grep patterns and keywords
- Which output files to search
- What constitutes a "hit" vs noise

**Present the prompt and keywords to the user for feedback.** The user may refine keywords, suggest alternative terms, or redirect the search. This step prevents wasted subagent runs.

### Step 5: Search — Subagent Grep

Launch a **sonnet subagent** with the approved prompt. The subagent:

- Uses `Grep` on the output files with the specified patterns
- Collects line numbers of all hits
- Groups hits by proximity (hits within 20 lines of each other likely belong to the same conversation turn)
- Returns a structured list: `{file, line_number, matching_text, context_group}`

### Step 6: Rank — Best Hits and Recommendations

The search subagent ranks its hits by relevance and recommends:

- **Top 3-5 hits** with the matching text and why they seem relevant
- **Ranges to read in full** — for each hit, a window of ~200-500 lines around it (enough to capture the full conversation turn and surrounding context)
- **Which ranges overlap** — consolidate overlapping windows

### Step 7: Summarize — Deep Read via Subagent

Launch a **sonnet subagent** to read the recommended ranges in full. This subagent:

- Calls `resurface()` on each recommended range with `frac_sample: 1.0` and `record_type: "conversation"` (or "all" if tool context matters)
- Reads the full conversation in those windows
- Writes a summary: what was discussed, what decisions were made, what the outcome was
- Recommends the **specific line ranges** (narrowed further) that the main agent should read directly

The subagent returns its summary plus line-number recommendations.

### Step 8: Read — Main Agent Direct Read

Read the final recommended ranges using `resurface()` directly (not via subagent). This brings the actual conversation into the main context where the user can see the results and the agent can act on them.

Use `frac_sample: 1.0`, `record_type: "conversation"` or `"all"`, and reasonable `max_word_length` (150+).

Present findings to the user with:
- Direct quotes from the session where relevant
- Dates and approximate times
- Decisions made and their rationale
- Any action items that were mentioned

## Efficiency Guidelines

- **Parallel where possible:** Steps 1 can run in background. Step 3 extracts can run in parallel. Steps 5-6 are one subagent. Step 7 is one subagent.
- **Token budget awareness:** A 500MB session has ~165k lines. At `frac_sample: 0.01`, the index is ~1.5k moments. At `frac_sample: 1.0` on a 5k-line range, expect ~1k moments. Always redirect to files when the output would exceed ~2k moments.
- **Iterative narrowing:** The protocol narrows at each step: full session → relevant period → specific hits → exact ranges. Resist the urge to read everything.
- **User in the loop:** Steps 2 and 4 require user confirmation. Do not skip these — the user's memory of what they're looking for is more accurate than keyword guessing.

## Record Type Selection Guide

| Looking for... | Primary type | Secondary type |
|---|---|---|
| A decision or discussion | `conversation` | — |
| A specific command run | `bash` | `tool_use` |
| A file that was created | `write` | `conversation` |
| What code was read/reviewed | `read` | `conversation` |
| A deploy or build | `bash` | `conversation` |
| An error or investigation | `conversation` | `bash` |
| Everything about a topic | `all` (use sparingly) | — |

## MCP Configuration

Session recall tools ship in the b3 binary. Served by the unified `b3 mcp` process alongside voice, hive, and browser tools. Auto-discovers session directories.

### Environment Variables

| Variable | Default | Description |
|---|---|---|
| `SESSION_DIRS` | *(auto-discover)* | Colon-separated list of directories containing session JSONL files. Overrides auto-discovery. |
| `SESSION_EXTRA_DIRS` | `""` | Additional colon-separated dirs to search. Appended to auto-discovered dirs. Use for remote session dirs. |
| `SESSION_INDEX_FILE` | `""` | Path to session index JSON file (for tag enrichment). Optional. |
| `SESSION_AGENT_NAME` | *(from $USER)* | Override agent display name in output. Default: capitalize username after `agent-` prefix. |

### Auto-Discovery

When `SESSION_DIRS` is not set, the server scans `~/.claude/projects/` for subdirectories containing `.jsonl` files. This covers all projects the agent has worked on.

For remote sessions (e.g. from `.claude-remote/projects/`), pass `SESSION_EXTRA_DIRS` to the unified `b3 mcp` entry:

```json
{
  "b3": {
    "command": "b3",
    "args": ["mcp", "serve"],
    "env": {
      "SESSION_EXTRA_DIRS": "/path/to/project/.claude-remote/projects/-project-path"
    }
  }
}
```

### Multi-Agent Setup

Each agent instance runs its own `b3 mcp` process. The `SESSION_AGENT_NAME` controls the display name in resurfaced output. If unset, it derives from `$USER` (e.g. `agent-alice` → `Alice`, `agent-bob` → `Bob`).

To cross-reference another agent's sessions, add their session directory to `SESSION_EXTRA_DIRS`.

## Additional Resources

### Reference Files

- **`references/resurface-api.md`** — Complete `resurface()` and `list_sessions()` parameter reference with examples
- **`references/configuration.md`** — Detailed configuration guide with examples for different setups
- **`references/index-cache.md`** — Persistent session index cache format, staleness detection, and incremental update protocol
