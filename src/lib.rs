//! Retrieval-facing library surface.
//!
//! The CodeWeave server itself is a binary (`main.rs`) with a private module
//! tree. This thin library re-exposes only the modules the offline evaluation
//! harness needs to exercise the *real* retrieval engine — no logic is
//! duplicated; the same source files back both targets. Keeping it minimal
//! avoids turning the whole server into a library (the P2 "no reorg" decision).

#[path = "index/mod.rs"]
pub mod index;
#[path = "model.rs"]
pub mod model;
#[path = "security.rs"]
pub mod security;
#[path = "symbols.rs"]
pub mod symbols;
