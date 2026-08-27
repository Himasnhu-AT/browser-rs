//! The MCP server error type.
//!
//! Only start-up and transport failures live here. Everything that can go wrong
//! *during* a tool call is a [`browser_server::mcp::ToolError`], which is
//! reported to the model inside a successful JSON-RPC response — see
//! [`crate::tools::call_tool_result`] for why that distinction matters.

use std::fmt;

/// A failure that stops `brs-mcp` from serving, or ends a session it was
/// serving.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum McpServerError {
    /// The environment did not describe a usable server.
    #[error("configuration: {0}")]
    Config(#[from] browser_server::config::ConfigError),

    /// The engine could not be built — usually an unusable profile root.
    #[error("engine: {0}")]
    Engine(#[from] browser_server::error::ServerError),

    /// The stdio transport failed before or during the handshake.
    #[error("transport: {0}")]
    Transport(String),
}

impl McpServerError {
    /// A transport failure from anything with a `Display`.
    pub fn transport(error: impl fmt::Display) -> Self {
        Self::Transport(error.to_string())
    }
}
