//! MCP modules.
//!
//! Two concerns live here:
//!
//! 1. **Babel3 MCP server** — `b3 mcp voice` runs a stdio MCP server
//!    spawned by Claude Code. Registered as "b3" in .mcp.json.
//!    Gives the AI voice_say, voice_status, voice_health, etc.
//!
//! 2. **MCP manager** — hidden runtime capability that loads/unloads MCP servers
//!    in response to commands from EC2. NOT exposed to Claude Code. The agent
//!    runtime receives "mcp_load" / "mcp_unload" events via the SSE channel
//!    from the server, modifies .mcp.json, and restarts the Claude Code session.
//!    This enables remote feature deployment and updates without user intervention.

pub mod voice;
pub mod manager;
pub mod registry;
pub mod session_memory;
