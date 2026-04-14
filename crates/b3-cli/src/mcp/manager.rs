//! MCP Manager — hidden runtime capability for remote MCP lifecycle management.
//!
//! NOT exposed to Claude Code. NOT user-facing CLI commands.
//! The MCP manager runs inside the b3 runtime and responds to server-side
//! events delivered via the SSE channel. EC2 can remotely load/unload MCPs
//! on any connected agent.
//!
//! Ported from the existing mcp-manager MCP (~900 lines Python).
//! Simplified because the b3 binary owns the PTY:
//!   - No tmux hacks for restart (just signal the child process)
//!   - No sudo for cross-user session control
//!   - Registry syncs from server API
//!
//! Event types (received via SSE from EC2):
//!   - mcp_load   { servers: ["name1", "name2"] }  — Add MCPs to .mcp.json, restart
//!   - mcp_unload { servers: ["name1"] }            — Remove from .mcp.json, restart
//!   - mcp_sync   {}                                — Re-fetch registry from server
//!
//! Config files:
//!   - ~/.b3/mcp-registry.json  — All MCPs this agent could load
//!   - .mcp.json (project root)      — Currently loaded MCPs (read by Claude Code)

#[allow(unused_imports)]
use crate::mcp::registry;

/// Handle an mcp_load event from EC2. Adds MCPs to .mcp.json and restarts session.
#[allow(dead_code)]
pub async fn handle_load(_servers: &[String]) -> anyhow::Result<()> {
    // Not yet implemented: resolve server names via registry, update .mcp.json,
    // signal Claude Code child process to restart, report back to EC2.
    anyhow::bail!("mcp_load handler not yet implemented")
}

/// Handle an mcp_unload event from EC2. Removes MCPs from .mcp.json and restarts.
#[allow(dead_code)]
pub async fn handle_unload(_servers: &[String]) -> anyhow::Result<()> {
    // Not yet implemented: refuse to remove b3 (built-in), remove entries
    // from .mcp.json, signal restart, report back to EC2.
    anyhow::bail!("mcp_unload handler not yet implemented")
}

/// Handle an mcp_sync event from EC2. Re-fetch registry and merge.
#[allow(dead_code)]
pub async fn handle_sync() -> anyhow::Result<()> {
    // Not yet implemented: GET /api/agents/{id}/mcp-registry, merge with local,
    // report what changed back to EC2.
    anyhow::bail!("mcp_sync handler not yet implemented")
}

/// Called during `b3 start` to set up initial MCP state.
#[allow(dead_code)]
pub async fn init() -> anyhow::Result<()> {
    // Not yet implemented: verify .mcp.json exists with b3 entry,
    // load registry, register SSE event handlers.
    anyhow::bail!("MCP manager init not yet implemented")
}
