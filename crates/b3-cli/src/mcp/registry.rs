//! MCP Registry management — load, save, merge, resolve hierarchies.
//!
//! Three-layer configuration:
//!   1. Server registry (API) — all MCPs in the catalog
//!   2. Local registry (~/.b3/mcp-registry.json) — cached on agent
//!   3. Active config (.mcp.json) — what Claude Code actually loads
//!
//! Hierarchy resolution:
//!   "voice"       → all MCPs in the voice hierarchy
//!   "voice.core"  → ["b3"]
//!   Dots force hierarchy path interpretation.
//!   No dots: check direct name first, then hierarchy root.
//!
//! Token estimation:
//!   Each MCP has an estimated token cost (tool definitions in context).
//!   Defaults from registry, overridden by observed values from /context parsing.
//!   Cached in ~/.b3/token-estimates.json.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;

/// All MCPs this agent could load.
pub struct Registry {
    pub servers: HashMap<String, McpServerConfig>,
    pub hierarchies: HashMap<String, HierarchyNode>,
    pub token_estimates: HashMap<String, u64>,
}

/// Configuration for a single MCP server.
pub struct McpServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub builtin: bool,
    pub always_loaded: bool,
    pub description: String,
    pub token_estimate: u64,
}

/// A node in the hierarchy tree — either a leaf (MCP names) or a branch (children).
pub enum HierarchyNode {
    Leaf(Vec<String>),
    Branch(HashMap<String, HierarchyNode>),
}

impl Registry {
    pub fn load(_path: &PathBuf) -> anyhow::Result<Self> {
        anyhow::bail!("MCP registry loading not yet implemented")
    }

    pub fn resolve(&self, _name: &str) -> Vec<String> {
        // Not yet implemented — return empty for now
        Vec::new()
    }

    pub fn collect_all_mcps(_node: &HierarchyNode) -> Vec<String> {
        // Not yet implemented — return empty for now
        Vec::new()
    }
}
