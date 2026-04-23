//! MCP modules.
//!
//! 1. **Unified b3 MCP server** — `b3 mcp` (or legacy `b3 mcp voice`) runs a stdio
//!    MCP server spawned by Claude Code. Registered as "b3" in .mcp.json.
//!    Serves all 24 tools: voice/hive/browser tools + session-memory recall tools.
//!
//! 2. **MCP manager** — hidden runtime capability that loads/unloads MCP servers
//!    in response to commands from EC2. NOT exposed to Claude Code.

pub mod b3;
pub mod manager;
pub mod registry;
pub mod session_memory;
