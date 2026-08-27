//! **browser-server** — the `browser-rs` server layer.
//!
//! One process, one port. It hosts three surfaces over the same engine:
//!
//! | Path | Surface |
//! |---|---|
//! | `/api/*` | the REST API, for any HTTP client |
//! | `/mcp` | the Model Context Protocol transport, for AI agents |
//! | `/`, `/ws` | the operator dashboard and its live-view socket |
//!
//! Everything an operation does lives in [`engine`], never in a route handler,
//! because the REST layer and the MCP layer must call exactly the same code.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod api;
pub mod config;
pub mod dashboard;
pub mod engine;
pub mod error;
pub mod mcp;
pub mod session;
pub mod ws;
