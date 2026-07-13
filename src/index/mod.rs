mod chunks;
mod context;
mod handle;
mod lines;
mod metadata;
mod path_filter;
mod references;
mod scan;
mod search;

pub use context::{ContextParams, Ranking, SymbolDetail};
pub use handle::{content_hash, decode_handle, encode_handle, RangeHandle};
pub use lines::slice_lines;
pub use scan::{ignored_workspace_path, WorkspaceExclusions};
pub use search::SearchParams;

use crate::symbols::Symbol;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

#[cfg(test)]
use crate::symbols::{extract_symbols, language_name};
#[cfg(test)]
use lines::line_starts;
#[cfg(test)]
use metadata::{build_indexed_terms, classify_document, classify_lifecycle};
#[cfg(test)]
use scan::{read_entry, CachedIndex};
#[cfg(test)]
use std::{fs, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub path_lower: String,
    pub content: String,
    #[serde(skip, default)]
    pub search_content: String,
    #[serde(default)]
    pub line_count: usize,
    #[serde(skip, default)]
    line_starts: Vec<usize>,
    #[serde(default)]
    indexed_terms: Vec<String>,
    pub hash: String,
    pub language: String,
    pub document_type: String,
    #[serde(default = "default_lifecycle")]
    pub lifecycle: String,
    pub symbols: Vec<Symbol>,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub modified_ns: u128,
    /// Symbol-bounded chunks for BM25F ranking (`v2`). Derived from `content` +
    /// `symbols`; not persisted — rebuilt by `normalize_entry` on load/insert.
    #[serde(skip, default)]
    chunks: Vec<chunks::Chunk>,
    /// Path field term frequencies, shared by every chunk of this file. Derived
    /// from `path_lower`; not persisted.
    #[serde(skip, default)]
    path_tf: HashMap<String, u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchMatch {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    pub document_type: String,
    pub score: f64,
    pub group: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
    pub handle: String,
    /// Chunk provenance (v2 ranking only): `symbol`, `symbol_part`, or
    /// `remainder`. Absent under v1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_kind: Option<String>,
    /// True when the excerpt spans a complete symbol (v2 ranking only). Absent
    /// under v1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complete_symbol: Option<bool>,
}

fn default_lifecycle() -> String {
    "current".to_owned()
}

#[derive(Debug, Default)]
pub struct CodeIndex {
    files: HashMap<String, FileEntry>,
    token_index: HashMap<String, HashSet<String>>,
    symbol_index: HashMap<String, BTreeSet<(String, usize)>>,
    snapshot_dirty: bool,
    cached_snapshot_head: Option<String>,
    cached_snapshot: Option<String>,
    /// Order-independent running digest of the file set. Each file contributes
    /// `sha256(path ‖ 0 ‖ hash ‖ 0)` XORed into this accumulator, so inserts and
    /// removals update it in O(1) instead of re-hashing the whole index on every
    /// mutation. Combined with `head` and the file count at read time.
    snapshot_acc: [u8; 32],
}

fn qualified_symbol_parts(name: &str) -> Option<(&str, &str)> {
    let dot = name.rfind('.').map(|index| (index, 1));
    let colon = name.rfind("::").map(|index| (index, 2));
    let (index, width) = match (dot, colon) {
        (Some(dot), Some(colon)) => {
            if dot.0 > colon.0 {
                dot
            } else {
                colon
            }
        }
        (Some(dot), None) => dot,
        (None, Some(colon)) => colon,
        (None, None) => return None,
    };
    let qualifier = &name[..index];
    let leaf = &name[index + width..];
    (!qualifier.is_empty() && !leaf.is_empty()).then_some((qualifier, leaf))
}

fn symbol_matches_qualified_name(file: &FileEntry, symbol: &Symbol, requested: &str) -> bool {
    if symbol.name == requested {
        return true;
    }
    let Some((qualifier, leaf)) = qualified_symbol_parts(requested) else {
        return false;
    };
    if symbol.name != leaf {
        return false;
    }
    let owner = qualified_symbol_parts(qualifier)
        .map(|(_, leaf)| leaf)
        .unwrap_or(qualifier);
    file.symbols.iter().any(|candidate| {
        candidate.name == owner
            && candidate.start_line <= symbol.start_line
            && candidate.end_line >= symbol.end_line
            && (candidate.start_line < symbol.start_line || candidate.end_line > symbol.end_line)
    })
}

#[cfg(test)]
mod tests;
