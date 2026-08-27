//! **browser-mcp** — a Model Context Protocol server over [`browser_server::engine`].
//!
//! Two transports carry the same tools: **stdio**, which is what a desktop agent
//! runtime spawns, and **streamable HTTP**, which `browser-server` mounts at
//! `/mcp` for remote or containerised agents.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod server;
pub mod tools;
