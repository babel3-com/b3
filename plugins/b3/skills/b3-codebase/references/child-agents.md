# Child Agents

Child agents are task-scoped or short-lived agents spawned by a parent. They share the parent's user account, get their own WireGuard identity and API key, and have a scoped hive ACL — they can only message their parent, not the full agent roster.

The CLI and daemon-side implementation are open-source. The server endpoints and database schema are in `b3-server` (proprietary).

---

## Concepts

**Path identity, not UUID identity.** A child's display address is `parent_name/child_name`. The agent ID (`hc-XXXXXX`) is the durable internal handle — it stays stable even if the agent is renamed. Never hardcode display names as identifiers.

**Children cannot spawn grandchildren.** The hierarchy is strictly two levels: top-level agents → child agents. The server sets `can_spawn_children = false` for every child at creation.

**Expiration is opt-in.** Pass `--expires-in` when spawning; omit it for a persistent child. Top-level agents are never expired by the server sweep.

**Hive ACL.** A child agent's `hive_status()` returns only its parent. It cannot message siblings or other children — only the parent. Top-level agents see peers and their own children.

---

## CLI (`commands/child.rs`)

```bash
# Spawn a child agent
b3 child spawn <name>
b3 child spawn <name> --expires-in 2h   # duration: s, m, h, d

# List children
b3 child list

# Kill a child (soft-delete)
b3 child kill <name>
b3 child kill <hc-XXXXXX>   # ID also accepted
```

### What spawn does locally

`b3 child spawn` calls `POST /api/agents/:self_id/children` on the server, then writes the child's config to disk:

```
./<name>/
└── .b3/
    ├── config.json      # agent_id, api_key, api_url, relay endpoint, parent_id, ...
    └── wg/
        └── private.key  # WireGuard private key (mode 0600, dir mode 0700)
```

After spawning:
```
✓ Child agent spawned: alice/worker
  Folder: /home/agent-alice/projects/worker
  Dashboard: https://babel3.com/a/alice/worker
  To start: cd worker && b3 start
```

### Starting a child from inside a parent session

`b3 start` normally refuses to run inside an existing Babel3 session (`B3_SESSION` env var is set — same pattern as `$TMUX`). Use `--detached` to bypass the guard:

```bash
cd worker && b3 start --detached
```

`--detached` signals "this is a peer process on a separate identity, not a nested daemon." The child runs its own daemon, registers independently on EC2, and gets its own terminal and voice pipeline.

---

## OnceLock Statics: Replacing B3_* Env Vars (`fork_state.rs`)

**File:** `open-source/crates/b3-cli/src/daemon/fork_state.rs`

The old approach passed daemon config from the `b3 start` parent process to the daemon child via environment variables (`B3_CONFIG_DIR`, `B3_START_CWD`, `B3_BROWSER_DIR`, `B3_APP_PORT`, `B3_APP_PUBLIC`). These leaked into every subprocess the daemon spawned — shells, Claude Code, child scripts.

The replacement: five `OnceLock` statics set before `fork(2)`. The child inherits the parent's address space copy-on-write, so the statics are visible without any IPC. No subprocess ever sees these as env vars.

```rust
// fork_state.rs
static CONFIG_DIR:  OnceLock<PathBuf>         = OnceLock::new();
static START_CWD:   OnceLock<PathBuf>         = OnceLock::new();
static BROWSER_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
static APP_PORT:    OnceLock<Option<u16>>     = OnceLock::new();
static APP_PUBLIC:  OnceLock<bool>            = OnceLock::new();

pub fn init(config_dir, start_cwd, browser_dir, app_port, app_public) {
    let _ = CONFIG_DIR.set(config_dir);   // OnceLock::set is idempotent
    // ...
}
```

`fork_state::init()` is called in `pre_fork()` before `fork(2)`. Accessors: `fork_state::config_dir()`, `fork_state::app_port()`, etc.

**`B3_SESSION` is intentionally kept as an env var** — it's ambient state that any child process (user shell, Claude Code, scripts) reads to detect nesting. It's meant to leak to children. The others were internal IPC and should not have been env vars.

---

## Key Files (Open-Source)

| File | Role |
|------|------|
| `open-source/crates/b3-cli/src/commands/child.rs` | `b3 child` CLI — spawn, list, kill |
| `open-source/crates/b3-cli/src/commands/start.rs:20` | `--detached` flag and `B3_SESSION` nesting guard |
| `open-source/crates/b3-cli/src/daemon/fork_state.rs` | OnceLock statics replacing B3_* env vars |
