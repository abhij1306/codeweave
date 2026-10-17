use super::document::DocumentType;
use super::store::CodeIndex;
use serde::Serialize;

/// Deterministic index-size counters used by the production-path evaluator.
/// `estimated_heap_bytes_lower_bound` counts owned buffers and indexed values;
/// allocator and hash-table bucket overhead are intentionally excluded.
#[derive(Debug, Clone, Serialize)]
pub struct IndexMetrics {
    pub indexed_file_count: usize,
    pub indexed_loc: usize,
    pub indexed_source_loc: usize,
    pub indexed_content_bytes: usize,
    pub estimated_heap_bytes_lower_bound: usize,
    pub symbol_name_count: usize,
    pub symbol_declaration_count: usize,
}

impl CodeIndex {
    pub fn metrics(&self) -> IndexMetrics {
        let mut metrics = IndexMetrics {
            indexed_file_count: 0,
            indexed_loc: 0,
            indexed_source_loc: 0,
            indexed_content_bytes: 0,
            estimated_heap_bytes_lower_bound: 0,
            symbol_name_count: 0,
            symbol_declaration_count: 0,
        };
        for file in self.files() {
            metrics.indexed_file_count += 1;
            metrics.indexed_loc += file.line_count();
            if file.document_type() == DocumentType::Source {
                metrics.indexed_source_loc += file.line_count();
            }
            metrics.indexed_content_bytes += file.content().len();
            // The file map key is a second copy of the path.
            metrics.estimated_heap_bytes_lower_bound +=
                file.path().len() + file.heap_bytes_lower_bound();
        }
        for (name, declarations) in self.symbol_postings() {
            metrics.symbol_name_count += 1;
            metrics.symbol_declaration_count += declarations.len();
            metrics.estimated_heap_bytes_lower_bound += name.capacity()
                + declarations
                    .iter()
                    .map(|(path, _)| path.capacity() + std::mem::size_of::<usize>())
                    .sum::<usize>();
        }
        metrics
    }
}
