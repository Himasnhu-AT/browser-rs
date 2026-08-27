//! Headless-Chromium fetchers for `browser-rs`, driven over raw CDP.
//!
//! `config`, `flags`, `pool` and `route` are pure decision logic with no CDP at
//! all; everything that talks to a browser lives under [`cdp`]. Patchright's
//! stealth is unnecessary here because we simply never emit the tell-tale
//! commands. See `ARCHITECTURE.md` §1.10 and §2.3.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod cdp;
pub mod cloudflare;
pub mod config;
pub mod error;
pub mod flags;
pub mod input;
pub mod pool;
pub mod response;
pub mod route;
pub mod screencast;
pub mod session;

#[cfg(test)]
mod live_u14;
