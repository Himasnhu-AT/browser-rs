//! Constants and header synthesis shared by the HTTP and browser engines:
//! impersonation profiles, browser header sets, default user agents, HTTP
//! status phrases and the ad-domain blocklist.
//!
//! See `ARCHITECTURE.md` §1.8.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod ad_domains;
pub mod domains;
pub mod error;
pub mod headers;
pub mod impersonate;
pub mod status;
pub mod useragent;
