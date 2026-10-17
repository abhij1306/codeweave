use super::document::DocumentType;
use crate::symbols::{language_name, Symbol};
use std::path::Path;

/// One indexed text file. Every derived field is computed from the path and
/// content at construction, so an entry is always internally consistent.
#[derive(Debug, Clone)]
pub struct FileEntry {
    path: String,
    path_lower: String,
    content: String,
    /// ASCII-lowercased `content`, used for case-insensitive text search.
    search_content: String,
    line_count: usize,
    hash: String,
    language: &'static str,
    document_type: DocumentType,
    symbols: Vec<Symbol>,
    size: u64,
    modified_ns: u128,
}

impl FileEntry {
    /// Build an entry from its content, content hash, and the symbols parsed
    /// from that content.
    pub(super) fn with_symbols(
        path: String,
        content: String,
        hash: String,
        symbols: Vec<Symbol>,
        size: u64,
        modified_ns: u128,
    ) -> Self {
        Self {
            path_lower: path.to_ascii_lowercase(),
            search_content: content.to_ascii_lowercase(),
            line_count: content.lines().count().max(1),
            language: language_name(Path::new(&path)),
            document_type: DocumentType::classify(&path),
            path,
            content,
            hash,
            symbols,
            size,
            modified_ns,
        }
    }

    /// Canonical workspace-relative path with forward slashes.
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn language(&self) -> &'static str {
        self.language
    }

    pub fn document_type(&self) -> DocumentType {
        self.document_type
    }

    pub fn symbols(&self) -> &[Symbol] {
        &self.symbols
    }

    /// Number of lines, counting an empty file as one line.
    pub fn line_count(&self) -> usize {
        self.line_count
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn modified_ns(&self) -> u128 {
        self.modified_ns
    }

    pub(super) fn path_lower(&self) -> &str {
        &self.path_lower
    }

    pub(super) fn search_content(&self) -> &str {
        &self.search_content
    }

    /// Lower bound of owned heap bytes, for the index metrics view.
    pub(super) fn heap_bytes_lower_bound(&self) -> usize {
        self.path.capacity()
            + self.path_lower.capacity()
            + self.content.capacity()
            + self.search_content.capacity()
            + self.hash.capacity()
            + self.symbols.capacity() * std::mem::size_of::<Symbol>()
            + self
                .symbols
                .iter()
                .map(|symbol| {
                    symbol.name.capacity() + symbol.kind.capacity() + symbol.signature.capacity()
                })
                .sum::<usize>()
    }
}
