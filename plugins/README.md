# Babel3 — MCP Server and Skills

This directory ships inside the `b3` binary and is extracted to `~/.claude/skills/` (skills) and registered in your project's `.mcp.json` (MCP server) by `b3 setup` / `b3 update`.

No Claude Code plugin system is involved. There is no marketplace step, no `claude plugin install`. The `b3` CLI manages installation directly.

## Installation

From a fresh system:

```bash
curl -fsSL https://babel3.com/install.sh | bash
```

This downloads the `b3` binary, runs `b3 setup` (on first `b3 start`), and writes:

- `.mcp.json` in your project root — registers `b3 mcp voice` as the MCP server Claude Code loads.
- `~/.claude/skills/b3-codebase/`, `~/.claude/skills/incident-investigation/`, `~/.claude/skills/session-recall/` — skill files, auto-discovered by Claude Code across all projects.
- `~/.codex/config.toml` — equivalent MCP registration for Codex users.

## Launching Claude Code

**The two things b3 is for — voice and hive — both require a launch flag right now.** Launch Claude Code in your project directory with:

```bash
claude --dangerously-load-development-channels server:b3
```

**This flag is required.** It enables Claude Code's channel notification mechanism, which is how two core b3 features deliver messages into the running session:

- **Voice transcriptions** — when you speak into the dashboard, the transcription arrives as a channel message so Claude responds to it as if you had typed it. Without the flag, transcriptions never reach the session.
- **Hive messages** — when another agent sends you a message via `hive_send`, it arrives the same way. Without the flag, hive delivery is broken.

The `.mcp.json` that `b3 setup` writes is not enough on its own — it handles tool loading, but not the inbound-message path. Both pieces need to work together.

The `dangerously-` prefix is Claude Code's internal naming — it reflects that the channel API is still unstable, not that the flag is unsafe for users. We track the Claude Code roadmap for when channel notifications graduate to a stable flag and will update this page.

## Updating

```bash
b3 update              # refreshes binary, skills, and .mcp.json
b3 update --force      # re-runs install-skills and migration even if already on the latest version
```

Skills ship embedded in the binary via `include_bytes!` — a binary of version `X` is guaranteed to ship the skills matching that version. Impossible to drift.

Users who previously installed the experimental `b3@b3-plugins` Claude Code plugin will have it automatically uninstalled on their next `b3 setup` or `b3 update` run.

## Opting into auto-update

During `b3 setup`, answer `y` to the auto-update prompt, or edit `.b3/config.json`:

```json
{
  "auto_update": true
}
```

With auto-update enabled, `b3 start` runs `b3 update` before launching the daemon — so every fresh session is on the current binary + skills without manual intervention. Network failure is non-fatal; the daemon still starts on the current binary.

## What's inside

```
plugins/
└── b3/
    ├── .mcp.json                      MCP server definition (b3 mcp voice, b3 mcp session-memory)
    └── skills/
        ├── b3-codebase/               Open-source codebase guide: daemon, browser, GPU worker
        ├── incident-investigation/    10-phase root-cause methodology
        └── session-recall/            Recall past Claude Code sessions via session-memory MCP
```

Each skill follows the standard `SKILL.md` + `references/` + `examples/` layout so Claude Code's auto-discovery picks it up from `~/.claude/skills/<skill>/`.

## Patent Notice

Babel3 includes patent-pending technology (US 64/010,742, US 64/010,891, US 64/011,207) licensed under Apache 2.0 with a royalty-free patent grant. The patents protect the ecosystem, not restrict it — using this software grants you a royalty-free license to the covered patents.

## License

Apache-2.0. See [LICENSE](../LICENSE).
