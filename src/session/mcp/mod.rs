//! MCP server configuration: the layered resolver (`mcp_model`), global
//! overrides file, native-agent reconciliation state, and per-project
//! `.mcp.json` servers.

pub mod mcp_model;
pub mod mcp_overrides;
pub mod mcp_state;
pub mod project_mcp;
