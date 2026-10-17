//! Reference search shared by `code_retrieve` and `code_intelligence`.
//!
//! The lexical fallback scans every in-scope file for exact identifier
//! occurrences; the semantic path shapes language-server locations into the
//! same response.

use crate::error::{AppError, AppResult, ErrorCode};
use crate::index::{
    encode_handle, slice_lines, CodeIndex, DocumentType, FileEntry, FileGroups, PathFilterSet,
    RangeHandle, SearchScope, SymbolDefinition, Window, WindowBuilder,
};
use crate::symbols::{
    identifier_occurrences_with_symbols, Evidence, IdentifierOccurrence, ReferenceKind, Symbol,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;

/// Identifier occurrences merged into one reference window at most.
const OCCURRENCES_PER_WINDOW: usize = 3;
/// Reference windows returned per file at most.
const WINDOWS_PER_FILE: usize = 3;
const FALLBACK_CAVEAT: &str = "Complete exact-identifier fallback scan; same-named unrelated identifiers may be included, while aliases and dynamic references may be missed.";

/// Which files a reference search covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceScope {
    All,
    /// Source files outside `docs/` and `examples/`.
    Production,
    Tests,
}

impl ReferenceScope {
    pub const ALL: [Self; 3] = [Self::All, Self::Production, Self::Tests];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Production => "production",
            Self::Tests => "tests",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|scope| scope.as_str() == value)
    }

    fn allows(self, file: &FileEntry) -> bool {
        match self {
            Self::All => true,
            Self::Tests => file.document_type() == DocumentType::Test,
            Self::Production => {
                file.document_type() == DocumentType::Source
                    && !file.path().starts_with("docs/")
                    && !file.path().starts_with("examples/")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    /// Computed from the live index.
    Current,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceBackend {
    Semantic,
    Fallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceMatchType {
    Reference,
    Declaration,
}

/// A lexical reference search for one declared symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceQuery {
    /// The declaration selector: a symbol name, optionally qualified as
    /// `Owner.name` or `Owner::name`.
    pub symbol: String,
    pub scope: ReferenceScope,
    /// Only occurrences of these kinds; empty means every kind. Including
    /// `Declaration` also returns the target declaration itself.
    pub kinds: Vec<ReferenceKind>,
    pub definition_path: Option<String>,
    pub definition_line: Option<usize>,
    pub context_lines: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReferencePosition {
    pub line: usize,
    pub column: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte: Option<usize>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReferenceRange {
    pub start: ReferencePosition,
    pub end: ReferencePosition,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReferenceTarget {
    pub name: String,
    pub path: String,
    pub range: ReferenceRange,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReferenceOccurrence {
    pub range: ReferenceRange,
    pub role: ReferenceKind,
    pub evidence: Evidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enclosing_symbol: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReferenceDefinition {
    pub path: String,
    pub symbol: Symbol,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReferenceLocation {
    pub path: String,
    pub line: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub range: ReferenceRange,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    pub match_type: ReferenceMatchType,
    pub evidence: Evidence,
    pub classification_evidence: Evidence,
    pub freshness: Freshness,
    pub reference_kind: ReferenceKind,
    pub role: ReferenceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enclosing_symbol: Option<String>,
    pub occurrences: Vec<ReferenceOccurrence>,
    pub score: f64,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScannedScope {
    pub file_count: usize,
    pub byte_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReferenceResponse {
    pub mode: &'static str,
    pub operation: &'static str,
    pub symbol: String,
    pub snapshot_id: String,
    pub target: ReferenceTarget,
    pub target_evidence: Evidence,
    pub backend: ReferenceBackend,
    pub freshness: Freshness,
    pub evidence: Evidence,
    pub evidence_caveat: String,
    #[serde(skip_serializing_if = "is_false")]
    pub semantic: bool,
    pub definition_count: usize,
    pub definitions: Vec<ReferenceDefinition>,
    pub reference_scope: ReferenceScope,
    pub reference_kinds: Vec<ReferenceKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scanned_scope: Option<ScannedScope>,
    pub occurrence_count: usize,
    pub result_count: usize,
    pub truncated: bool,
    pub results: Vec<ReferenceLocation>,
}

/// A resolved reference target: one declaration and its name occurrence.
#[derive(Debug, Clone)]
pub struct ReferenceTargetSelection {
    target: ReferenceTarget,
    symbol: Symbol,
    target_evidence: Evidence,
    declaration_occurrence: Option<IdentifierOccurrence>,
    selected_declaration: Option<(usize, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemanticReferenceLocation {
    pub path: String,
    pub range: ReferenceRange,
}

#[derive(Debug, Clone, Copy)]
pub struct SemanticReferenceMetadata<'a> {
    pub freshness: Freshness,
    pub evidence_caveat: &'a str,
}

pub struct ReferenceService<'a> {
    index: &'a CodeIndex,
}

impl<'a> ReferenceService<'a> {
    pub fn new(index: &'a CodeIndex) -> Self {
        Self { index }
    }

    /// Lexical references to the declaration selected by `query`.
    pub fn fallback(&self, scope: &SearchScope<'_>, query: &ReferenceQuery) -> AppResult<Value> {
        let target = self.resolve_symbol(
            &query.symbol,
            query.definition_path.as_deref(),
            query.definition_line,
        )?;
        self.fallback_for_target(scope, query, target)
    }

    /// Lexical references to the innermost declaration containing `line`.
    pub fn fallback_at_position(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
        path: &str,
        line: usize,
        max_results: usize,
    ) -> AppResult<Value> {
        let target = self.resolve_position(path, line)?;
        let query = ReferenceQuery {
            symbol: target.symbol.name.clone(),
            scope: ReferenceScope::All,
            kinds: Vec::new(),
            definition_path: Some(target.target.path.clone()),
            definition_line: Some(target.symbol.start_line),
            context_lines: 0,
        };
        let scope = SearchScope {
            workspace_id,
            snapshot_id,
            paths: &[],
            max_results,
        };
        self.fallback_for_target(&scope, &query, target)
    }

    pub fn resolve_position(&self, path: &str, line: usize) -> AppResult<ReferenceTargetSelection> {
        let file = self.index.get(path).ok_or_else(|| {
            AppError::with_details(
                ErrorCode::PathNotIndexed,
                "Reference target path is not indexed",
                json!({"path": path}),
            )
        })?;
        let symbol = file
            .symbols()
            .iter()
            .filter(|symbol| symbol.start_line <= line && symbol.end_line >= line)
            .min_by_key(|symbol| {
                (
                    symbol.start_line != line,
                    symbol.end_line.saturating_sub(symbol.start_line),
                    symbol.start_line,
                    symbol.end_line,
                    symbol.name.as_str(),
                )
            })
            .ok_or_else(|| {
                AppError::with_details(
                    ErrorCode::SymbolNotFound,
                    "No indexed declaration contains the requested position",
                    json!({"path": path, "line": line}),
                )
            })?;
        self.resolve_symbol(&symbol.name, Some(path), Some(symbol.start_line))
    }

    pub fn semantic(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
        target: ReferenceTargetSelection,
        mut locations: Vec<SemanticReferenceLocation>,
        max_results: usize,
        metadata: SemanticReferenceMetadata<'_>,
    ) -> AppResult<Value> {
        locations.sort();
        locations.dedup();
        locations.retain(|location| {
            location.path != target.target.path || location.range != target.target.range
        });
        let total = locations.len();
        locations.truncate(max_results.max(1));

        let mut results = Vec::with_capacity(locations.len());
        for location in locations {
            let occurrence = ReferenceOccurrence {
                range: location.range.clone(),
                role: ReferenceKind::Other,
                evidence: Evidence::Semantic,
                enclosing_symbol: None,
            };
            let indexed = self.index.get(&location.path);
            let preview = indexed.map(|file| {
                slice_lines(
                    file.content(),
                    location.range.start.line,
                    location.range.end.line,
                )
            });
            let handle = indexed
                .map(|file| {
                    encode_handle(&RangeHandle {
                        workspace_id: workspace_id.to_owned(),
                        path: location.path.clone(),
                        start_line: location.range.start.line,
                        end_line: location.range.end.line,
                        content_hash: file.hash().to_owned(),
                    })
                })
                .transpose()?;
            results.push(ReferenceLocation {
                path: location.path,
                line: location.range.start.line,
                start_line: location.range.start.line,
                end_line: location.range.end.line,
                range: location.range,
                preview,
                handle,
                match_type: ReferenceMatchType::Reference,
                evidence: Evidence::Semantic,
                classification_evidence: Evidence::Semantic,
                freshness: metadata.freshness,
                reference_kind: ReferenceKind::Other,
                role: ReferenceKind::Other,
                enclosing_symbol: None,
                occurrences: vec![occurrence],
                score: 1.0,
                reason_codes: vec!["semantic_reference".to_owned()],
            });
        }

        serialize(ReferenceResponse {
            mode: "references",
            operation: "references",
            symbol: target.symbol.name.clone(),
            snapshot_id: snapshot_id.to_owned(),
            definitions: vec![ReferenceDefinition {
                path: target.target.path.clone(),
                symbol: target.symbol,
            }],
            target: target.target,
            target_evidence: target.target_evidence,
            backend: ReferenceBackend::Semantic,
            freshness: metadata.freshness,
            evidence: Evidence::Semantic,
            evidence_caveat: metadata.evidence_caveat.to_owned(),
            semantic: true,
            definition_count: 1,
            reference_scope: ReferenceScope::All,
            reference_kinds: Vec::new(),
            scanned_scope: None,
            occurrence_count: total,
            result_count: results.len(),
            truncated: total > results.len(),
            results,
        })
    }

    fn fallback_for_target(
        &self,
        scope: &SearchScope<'_>,
        query: &ReferenceQuery,
        target: ReferenceTargetSelection,
    ) -> AppResult<Value> {
        let path_filters = PathFilterSet::new(scope.paths);
        let target_path = target.target.path.clone();
        let target_file = self.index.get(&target_path).ok_or_else(|| {
            AppError::with_details(
                ErrorCode::SymbolNotFound,
                "Reference target file is no longer indexed",
                json!({"path": target_path}),
            )
        })?;

        let mut files = self
            .index
            .files()
            .filter(|file| path_filters.allows(file.path()))
            .filter(|file| query.scope.allows(file))
            .collect::<Vec<_>>();
        files.sort_unstable_by(|left, right| left.path().cmp(right.path()));
        let scanned_scope = ScannedScope {
            file_count: files.len(),
            byte_count: files.iter().map(|file| file.content().len()).sum(),
        };

        let limit = scope.limit();
        let mut groups = FileGroups::new();
        if query.kinds.contains(&ReferenceKind::Declaration)
            && path_filters.allows(&target_path)
            && query.scope.allows(target_file)
        {
            groups.push_extra(declaration_location(
                scope.workspace_id,
                target_file,
                &target.symbol,
                target.declaration_occurrence.as_ref(),
            )?);
        }

        let mut total_occurrences = 0usize;
        for file in files {
            let occurrences = identifier_occurrences_with_symbols(
                Path::new(file.path()),
                file.content(),
                &target.symbol.name,
                file.symbols(),
            );
            let mut builder = WindowBuilder::new(
                query.context_lines,
                file.line_count(),
                OCCURRENCES_PER_WINDOW,
            );
            for occurrence in occurrences {
                let is_target_declaration = file.path() == target_path
                    && target.selected_declaration
                        == Some((occurrence.start_byte, occurrence.end_byte));
                let kind_selected =
                    query.kinds.is_empty() || query.kinds.contains(&occurrence.role);
                if is_target_declaration || !kind_selected {
                    continue;
                }
                total_occurrences += 1;
                builder.push(occurrence.start_line, occurrence.end_line, occurrence);
            }
            let mut windows = builder.finish();
            windows.sort_by_key(|window| {
                (kind_priority(leading_occurrence(window).role), window.line)
            });
            groups.push_file(windows, limit.min(WINDOWS_PER_FILE), |window| {
                reference_location(scope.workspace_id, file, window)
            })?;
        }
        groups.sort_by_first(|location| kind_priority(location.reference_kind));
        let interleaved = groups.interleave(limit);

        serialize(ReferenceResponse {
            mode: "references",
            operation: "references",
            symbol: query.symbol.clone(),
            snapshot_id: scope.snapshot_id.to_owned(),
            target: target.target,
            target_evidence: target.target_evidence,
            backend: ReferenceBackend::Fallback,
            freshness: Freshness::Current,
            evidence: Evidence::Lexical,
            evidence_caveat: FALLBACK_CAVEAT.to_owned(),
            semantic: false,
            definition_count: 1,
            definitions: vec![ReferenceDefinition {
                path: target_path,
                symbol: target.symbol,
            }],
            reference_scope: query.scope,
            reference_kinds: query.kinds.clone(),
            scanned_scope: Some(scanned_scope),
            occurrence_count: total_occurrences,
            result_count: interleaved.results.len(),
            truncated: interleaved.truncated,
            results: interleaved.results,
        })
    }

    fn resolve_symbol(
        &self,
        selector: &str,
        definition_path: Option<&str>,
        definition_line: Option<usize>,
    ) -> AppResult<ReferenceTargetSelection> {
        let selector = selector.trim();
        if selector.is_empty() {
            return Err(AppError::invalid("query is required for reference search"));
        }
        let mut definitions = self
            .index
            .find_symbols(definition_path, selector)
            .into_iter()
            .filter(|found| definition_line.is_none_or(|line| line == found.symbol.start_line))
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.symbol.start_line.cmp(&right.symbol.start_line))
                .then_with(|| left.symbol.end_line.cmp(&right.symbol.end_line))
        });
        let SymbolDefinition { path, symbol, .. } = match definitions.len() {
            0 => {
                return Err(AppError::with_details(
                    ErrorCode::SymbolNotFound,
                    "Reference search requires an indexed symbol definition",
                    json!({
                        "symbol": selector,
                        "suggested_action": "Use literal search for arbitrary text or symbol search to find the declaration.",
                    }),
                ))
            }
            1 => definitions.remove(0),
            _ => {
                let candidates = definitions
                    .iter()
                    .map(|found| json!({"path": found.path, "symbol": found.symbol}))
                    .collect::<Vec<_>>();
                return Err(AppError::with_details(
                    ErrorCode::AmbiguousSymbol,
                    "Reference search found multiple declarations; specify definition_path and definition_line",
                    json!({"symbol": selector, "candidates": candidates}),
                ));
            }
        };

        let file = self.index.get(&path).ok_or_else(|| {
            AppError::with_details(
                ErrorCode::SymbolNotFound,
                "Reference target file is no longer indexed",
                json!({"path": path}),
            )
        })?;
        let occurrences = identifier_occurrences_with_symbols(
            Path::new(&path),
            file.content(),
            &symbol.name,
            file.symbols(),
        );
        let within_symbol = |occurrence: &&IdentifierOccurrence| {
            occurrence.start_line >= symbol.start_line && occurrence.start_line <= symbol.end_line
        };
        let declaration_occurrence = occurrences
            .iter()
            .filter(within_symbol)
            .find(|occurrence| occurrence.role == ReferenceKind::Declaration)
            .or_else(|| occurrences.iter().find(within_symbol))
            .cloned();
        let selected_declaration = declaration_occurrence
            .as_ref()
            .map(|occurrence| (occurrence.start_byte, occurrence.end_byte));
        let range = declaration_occurrence
            .as_ref()
            .map_or_else(|| symbol_range(&symbol), occurrence_range);
        let target_evidence = declaration_occurrence
            .as_ref()
            .map_or(Evidence::Syntactic, |occurrence| occurrence.evidence);
        let qualified_name = (selector != symbol.name).then(|| selector.to_owned());
        Ok(ReferenceTargetSelection {
            target: ReferenceTarget {
                name: symbol.name.clone(),
                path,
                range,
                kind: symbol.kind.clone(),
                qualified_name,
            },
            symbol,
            target_evidence,
            declaration_occurrence,
            selected_declaration,
        })
    }
}

fn serialize(response: ReferenceResponse) -> AppResult<Value> {
    serde_json::to_value(response).map_err(AppError::internal)
}

/// The occurrence that classifies a window: the first one of the
/// highest-priority kind.
fn leading_occurrence(window: &Window<IdentifierOccurrence>) -> &IdentifierOccurrence {
    window
        .hits
        .iter()
        .min_by_key(|occurrence| kind_priority(occurrence.role))
        .expect("a reference window holds at least one occurrence")
}

fn reference_location(
    workspace_id: &str,
    file: &FileEntry,
    window: Window<IdentifierOccurrence>,
) -> AppResult<ReferenceLocation> {
    let handle = encode_handle(&RangeHandle {
        workspace_id: workspace_id.to_owned(),
        path: file.path().to_owned(),
        start_line: window.start_line,
        end_line: window.end_line,
        content_hash: file.hash().to_owned(),
    })?;
    let lead = leading_occurrence(&window);
    let kind = lead.role;
    let classification_evidence = lead.evidence;
    let enclosing_symbol = lead.enclosing_symbol.clone();
    let occurrences = window
        .hits
        .iter()
        .map(reference_occurrence)
        .collect::<Vec<_>>();
    Ok(ReferenceLocation {
        path: file.path().to_owned(),
        line: window.line,
        start_line: window.start_line,
        end_line: window.end_line,
        range: occurrences[0].range.clone(),
        preview: Some(slice_lines(
            file.content(),
            window.start_line,
            window.end_line,
        )),
        handle: Some(handle),
        match_type: ReferenceMatchType::Reference,
        evidence: Evidence::Lexical,
        classification_evidence,
        freshness: Freshness::Current,
        reference_kind: kind,
        role: kind,
        enclosing_symbol,
        occurrences,
        score: kind_score(kind),
        reason_codes: vec![format!("{}_reference", kind.as_str())],
    })
}

fn declaration_location(
    workspace_id: &str,
    file: &FileEntry,
    symbol: &Symbol,
    occurrence: Option<&IdentifierOccurrence>,
) -> AppResult<ReferenceLocation> {
    let handle = encode_handle(&RangeHandle {
        workspace_id: workspace_id.to_owned(),
        path: file.path().to_owned(),
        start_line: symbol.start_line,
        end_line: symbol.end_line,
        content_hash: file.hash().to_owned(),
    })?;
    Ok(ReferenceLocation {
        path: file.path().to_owned(),
        line: occurrence.map_or(symbol.start_line, |item| item.start_line),
        start_line: symbol.start_line,
        end_line: symbol.end_line,
        range: occurrence.map_or_else(|| symbol_range(symbol), occurrence_range),
        preview: Some(slice_lines(
            file.content(),
            symbol.start_line,
            symbol.end_line,
        )),
        handle: Some(handle),
        match_type: ReferenceMatchType::Declaration,
        evidence: Evidence::Syntactic,
        classification_evidence: Evidence::Syntactic,
        freshness: Freshness::Current,
        reference_kind: ReferenceKind::Declaration,
        role: ReferenceKind::Declaration,
        enclosing_symbol: occurrence.and_then(|item| item.enclosing_symbol.clone()),
        occurrences: occurrence.map(reference_occurrence).into_iter().collect(),
        score: 1.0,
        reason_codes: vec!["declaration_match".to_owned()],
    })
}

fn reference_occurrence(occurrence: &IdentifierOccurrence) -> ReferenceOccurrence {
    ReferenceOccurrence {
        range: occurrence_range(occurrence),
        role: occurrence.role,
        evidence: occurrence.evidence,
        enclosing_symbol: occurrence.enclosing_symbol.clone(),
    }
}

fn occurrence_range(occurrence: &IdentifierOccurrence) -> ReferenceRange {
    ReferenceRange {
        start: ReferencePosition {
            line: occurrence.start_line,
            column: occurrence.start_column,
            byte: Some(occurrence.start_byte),
        },
        end: ReferencePosition {
            line: occurrence.end_line,
            column: occurrence.end_column,
            byte: Some(occurrence.end_byte),
        },
    }
}

fn symbol_range(symbol: &Symbol) -> ReferenceRange {
    ReferenceRange {
        start: ReferencePosition {
            line: symbol.start_line,
            column: 1,
            byte: None,
        },
        end: ReferencePosition {
            line: symbol.end_line,
            column: 1,
            byte: None,
        },
    }
}

/// Result order of reference kinds; lower ranks first.
fn kind_priority(kind: ReferenceKind) -> u8 {
    match kind {
        ReferenceKind::Declaration => 0,
        ReferenceKind::Call => 1,
        ReferenceKind::Write => 2,
        ReferenceKind::Type => 3,
        ReferenceKind::Import => 4,
        ReferenceKind::Read => 5,
        ReferenceKind::Other => 6,
    }
}

fn kind_score(kind: ReferenceKind) -> f64 {
    match kind {
        ReferenceKind::Declaration | ReferenceKind::Call => 1.0,
        ReferenceKind::Write => 0.9,
        ReferenceKind::Type => 0.85,
        ReferenceKind::Import => 0.75,
        ReferenceKind::Read => 0.65,
        ReferenceKind::Other => 0.5,
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests;
