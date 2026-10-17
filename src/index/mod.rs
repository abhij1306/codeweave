//! The workspace index.
//!
//! - `store`: the pure in-memory index; every query takes `&self`.
//! - `scan`: builds and refreshes the index from the filesystem.
//! - `cache`: the persistent cache adapter used by `CodeIndex::scan_cached`.
//! - `search`: text, filename, symbol, outline, and repository-map queries,
//!   taking the typed queries in `query`.
//! - `windows`: windowed, per-file-capped, interleaved results shared with
//!   reference search.
//! - `metrics`: the size view used by the evaluator.

mod cache;
mod document;
mod entry;
mod exclusions;
mod handle;
mod ids;
mod lines;
mod metrics;
mod path_filter;
mod query;
mod scan;
mod search;
mod store;
mod windows;

pub use document::DocumentType;
pub use entry::FileEntry;
pub use exclusions::{ignored_workspace_path, WorkspaceExclusions};
pub use handle::{content_hash, decode_handle, encode_handle, RangeHandle};
pub use ids::{ContentHash, SnapshotId};
pub use lines::slice_lines;
pub use metrics::IndexMetrics;
pub(crate) use path_filter::PathFilterSet;
pub use query::{FilenameQuery, SearchScope, SymbolQuery, TextQuery, TextSyntax};
pub use store::{CodeIndex, SymbolDefinition};
pub(crate) use windows::{FileGroups, Window, WindowBuilder};

#[cfg(test)]
mod cache_tests;
#[cfg(test)]
mod tests;
