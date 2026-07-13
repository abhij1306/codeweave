//! Chunk-granular retrieval model (ranking `v2`).
//!
//! v1 scores whole files and renders a fixed 6-line excerpt around the match. v2
//! reuses v1's exact scoring loop but splits each file into symbol-bounded chunks
//! so a result can point at the *complete enclosing symbol* rather than a slice,
//! and adds a filename-affinity boost (via the per-file `path_tf`) so a
//! file/dir-name query ranks the right file first.
//!
//! The chunk set is computed once at index time and stored on `FileEntry`
//! (`#[serde(skip)]`, rebuilt on load), so a per-file incremental refresh
//! replaces a file's chunks as a unit.

use super::metadata::query_terms;
use crate::symbols::Symbol;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Symbols longer than this are split into sequential sub-chunks so a single huge
/// function does not dominate one rendered result.
const MAX_CHUNK_LINES: usize = 150;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    /// A chunk bounded by a single top-level symbol.
    Symbol,
    /// One sequential slice of a symbol longer than `MAX_CHUNK_LINES`.
    SymbolPart,
    /// Content outside any symbol (imports, module docs, config, prose).
    Remainder,
}

/// A symbol-bounded (or remainder) slice of a file. Line numbers are 1-based and
/// inclusive. The chunk exists to bound *rendering* at a whole symbol; scoring
/// stays at the file level (see `CodeIndex::context_v2`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub kind: ChunkKind,
    /// Enclosing symbol name, when the chunk is bounded by one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
}

impl Chunk {
    /// True when this chunk represents a whole symbol (not a split part or
    /// remainder) — used to report `complete_symbol` on results.
    pub fn is_complete_symbol(&self) -> bool {
        matches!(self.kind, ChunkKind::Symbol)
    }
}

/// Build the chunk set for one file. Each top-level symbol becomes one `Symbol`
/// chunk (or several `SymbolPart` chunks when longer than `MAX_CHUNK_LINES`); the
/// gaps between symbols become `Remainder` chunks.
pub(super) fn build_chunks(content: &str, symbols: &[Symbol]) -> Vec<Chunk> {
    let line_count = content.lines().count().max(1);

    // Collect top-level symbols (those not contained by another symbol). Nested
    // symbols stay inside their parent's chunk.
    let mut top_level: Vec<&Symbol> = Vec::new();
    for symbol in symbols {
        let nested = symbols.iter().any(|other| {
            !std::ptr::eq(other, symbol)
                && other.start_line <= symbol.start_line
                && other.end_line >= symbol.end_line
                && (other.start_line < symbol.start_line || other.end_line > symbol.end_line)
        });
        if !nested {
            top_level.push(symbol);
        }
    }
    top_level.sort_by_key(|symbol| symbol.start_line);

    let mut chunks = Vec::new();
    let mut covered = vec![false; line_count + 2];

    for symbol in &top_level {
        let start = symbol.start_line.max(1);
        let end = symbol.end_line.min(line_count).max(start);
        for line in start..=end {
            if line < covered.len() {
                covered[line] = true;
            }
        }

        if end - start < MAX_CHUNK_LINES {
            chunks.push(Chunk {
                kind: ChunkKind::Symbol,
                symbol: Some(symbol.name.clone()),
                start_line: start,
                end_line: end,
            });
        } else {
            // Split the long symbol into sequential parts, each retaining the
            // owning symbol's identity.
            let mut part_start = start;
            while part_start <= end {
                let part_end = (part_start + MAX_CHUNK_LINES - 1).min(end);
                chunks.push(Chunk {
                    kind: ChunkKind::SymbolPart,
                    symbol: Some(symbol.name.clone()),
                    start_line: part_start,
                    end_line: part_end,
                });
                part_start = part_end + 1;
            }
        }
    }

    // Remainder chunks: maximal runs of uncovered lines (imports, module docs,
    // config files, markdown). Emitted as one chunk per contiguous run.
    let lines: Vec<&str> = content.lines().collect();
    let mut run_start: Option<usize> = None;
    for line in 1..=line_count {
        let is_covered = covered.get(line).copied().unwrap_or(false);
        match (run_start, is_covered) {
            (None, false) => run_start = Some(line),
            (Some(start), true) => {
                push_remainder(&mut chunks, &lines, start, line - 1);
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run_start {
        push_remainder(&mut chunks, &lines, start, line_count);
    }

    if chunks.is_empty() {
        // Empty file: a single zero-length remainder keeps every file renderable.
        chunks.push(Chunk {
            kind: ChunkKind::Remainder,
            symbol: None,
            start_line: 1,
            end_line: 1,
        });
    }
    chunks.sort_by_key(|chunk| chunk.start_line);
    chunks
}

fn push_remainder(chunks: &mut Vec<Chunk>, lines: &[&str], start: usize, end: usize) {
    let lo = start.saturating_sub(1);
    let hi = end.min(lines.len());
    if lo >= hi || lines[lo..hi].join("\n").trim().is_empty() {
        return;
    }
    chunks.push(Chunk {
        kind: ChunkKind::Remainder,
        symbol: None,
        start_line: start,
        end_line: end,
    });
}

/// Path segments as a term set (file/dir names split on separators). The ranker
/// uses this for the filename-affinity boost: how many query terms hit the path.
pub(super) fn path_field(path_lower: &str) -> HashMap<String, u32> {
    let separated: String = path_lower
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | '_' | '.' | '-') { ' ' } else { c })
        .collect();
    let mut counts: HashMap<String, u32> = HashMap::new();
    for word in separated.split(|c: char| c.is_whitespace()) {
        if word.is_empty() {
            continue;
        }
        // Normalize with the same rules the query side uses, so a path term and a
        // query term compare equal.
        for token in query_terms(word) {
            *counts.entry(token).or_default() += 1;
        }
    }
    counts
}

/// Accessors used by the ranker to read a file's chunk set and path field.
impl super::FileEntry {
    pub(super) fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }
    pub(super) fn path_tf(&self) -> &HashMap<String, u32> {
        &self.path_tf
    }
}
