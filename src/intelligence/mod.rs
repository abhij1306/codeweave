//! Optional language-server intelligence with evidence-labeled fallback.
//!
//! - `request`: the typed `code_intelligence` contract.
//! - `session`: the fresh-document lifecycle, the freshness invariant, and the
//!   semantic-or-fallback decision.
//! - `backend`: the semantic seam; `worker` is its LSP adapter and `fallback`
//!   the Tree-sitter/index adapter.
//! - `uri` and `position`: the URI <-> workspace path codec and position
//!   normalization.
//! - `live_index`: the only reader of the reconciled index and snapshot.
mod backend;
mod fallback;
mod live_index;
mod position;
mod preset;
mod protocol;
mod request;
mod service;
mod session;
mod sync;
mod transport;
mod uri;
mod worker;
mod workspace_edit;

pub use service::{IntelligenceResponse, IntelligenceService, RenamePreview};

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
