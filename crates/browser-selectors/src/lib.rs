//! Native CSS selection for `browser-rs`, built on Servo's `selectors` crate
//! and extended with the reference implementation's `::text`, `::attr()` and `:contains()`.
//!
//! See `ARCHITECTURE.md` §1.3 — we match CSS directly rather than translating
//! it to XPath.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod canonical;
pub mod error;
pub mod impl_selector;
pub mod matching;
pub mod parse;
pub mod pseudo;

// The names every consumer reaches for, hoisted to the crate root so
// `browser_selectors::SelectorGroup` works alongside the module paths (which
// stay valid). Unit 5 was not permitted to edit `lib.rs`; the integration wave
// applies what `API.md`'s "one `use` is enough" table already documents, and
// brings this crate into line with `browser-core`, `browser-dom` and
// `browser-storage`, all of which re-export from their roots.
pub use crate::error::{Result, SelectorError};
pub use crate::matching::{Match, MatchValue, TextRole};
pub use crate::parse::{
    canonical_parts, parse_selector, CssSelector, SelectorGroup, SelectorParser, SubSelector,
};
pub use crate::pseudo::{BrowserPseudoClass, BrowserPseudoElement, Tail};
