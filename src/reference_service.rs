use crate::index::{encode_handle, slice_lines, CodeIndex, FileEntry, PathFilterSet, RangeHandle};
use crate::model::{AppError, AppResult};
use crate::symbols::{identifier_occurrences_with_symbols, IdentifierOccurrence, Symbol};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::Path;

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
    pub role: String,
    pub evidence: String,
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
    pub match_type: String,
    pub evidence: String,
    pub classification_evidence: String,
    pub freshness: String,
    pub reference_kind: String,
    pub role: String,
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
    pub target_evidence: String,
    pub backend: String,
    pub freshness: String,
    pub evidence: String,
    pub evidence_caveat: String,
    #[serde(skip_serializing_if = "is_false")]
    pub semantic: bool,
    pub definition_count: usize,
    pub definitions: Vec<ReferenceDefinition>,
    pub reference_scope: String,
    pub reference_kinds: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scanned_scope: Option<ScannedScope>,
    pub occurrence_count: usize,
    pub result_count: usize,
    pub truncated: bool,
    pub results: Vec<ReferenceLocation>,
}

#[derive(Debug, Clone)]
pub struct ReferenceTargetSelection {
    target: ReferenceTarget,
    symbol: Symbol,
    target_evidence: String,
    declaration_occurrence: Option<IdentifierOccurrence>,
    selected_declaration: Option<(usize, usize)>,
}

pub struct FallbackReferenceRequest<'a> {
    pub workspace_id: &'a str,
    pub snapshot_id: &'a str,
    pub selector: &'a str,
    pub path_filters: &'a [String],
    pub max_results: usize,
    pub context_lines: usize,
    pub reference_scope: &'a str,
    pub reference_kinds: &'a [String],
    pub definition_path: Option<&'a str>,
    pub definition_line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemanticReferenceLocation {
    pub path: String,
    pub range: ReferenceRange,
}

#[derive(Debug, Clone, Copy)]
pub struct SemanticReferenceMetadata<'a> {
    pub freshness: &'a str,
    pub evidence_caveat: &'a str,
}

pub struct ReferenceService<'a> {
    index: &'a CodeIndex,
}

impl<'a> ReferenceService<'a> {
    pub fn new(index: &'a CodeIndex) -> Self {
        Self { index }
    }

    pub fn fallback(&self, request: FallbackReferenceRequest<'_>) -> AppResult<Value> {
        let target = self.resolve_symbol(
            request.selector,
            request.definition_path,
            request.definition_line,
        )?;
        self.fallback_for_target(&request, target)
    }

    pub fn fallback_at_position(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
        path: &str,
        line: usize,
        max_results: usize,
    ) -> AppResult<Value> {
        let target = self.resolve_position(path, line)?;
        let selector = target.symbol.name.clone();
        let definition_path = target.target.path.clone();
        self.fallback_for_target(
            &FallbackReferenceRequest {
                workspace_id,
                snapshot_id,
                selector: &selector,
                path_filters: &[],
                max_results,
                context_lines: 0,
                reference_scope: "all",
                reference_kinds: &[],
                definition_path: Some(&definition_path),
                definition_line: Some(target.symbol.start_line),
            },
            target,
        )
    }

    pub fn resolve_position(&self, path: &str, line: usize) -> AppResult<ReferenceTargetSelection> {
        let file = self.index.get(path).ok_or_else(|| {
            AppError::details(
                "PATH_NOT_INDEXED",
                "Reference target path is not indexed",
                json!({"path": path}),
            )
        })?;
        let mut candidates = file
            .symbols
            .iter()
            .filter(|symbol| symbol.start_line <= line && symbol.end_line >= line)
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by_key(|symbol| {
            (
                symbol.start_line != line,
                symbol.end_line.saturating_sub(symbol.start_line),
                symbol.start_line,
                symbol.end_line,
                symbol.name.clone(),
            )
        });
        let symbol = candidates.into_iter().next().ok_or_else(|| {
            AppError::details(
                "SYMBOL_NOT_FOUND",
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

        let definition_path = target.target.path.clone();
        let symbol_name = target.symbol.name.clone();
        let mut results = Vec::with_capacity(locations.len());
        for location in locations {
            let occurrence = ReferenceOccurrence {
                range: location.range.clone(),
                role: "other".to_owned(),
                evidence: "semantic".to_owned(),
                enclosing_symbol: None,
            };
            let indexed = self.index.get(&location.path);
            let preview = indexed.map(|file| {
                slice_lines(
                    &file.content,
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
                        content_hash: file.hash.clone(),
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
                match_type: "reference".to_owned(),
                evidence: "semantic".to_owned(),
                classification_evidence: "semantic".to_owned(),
                freshness: metadata.freshness.to_owned(),
                reference_kind: "other".to_owned(),
                role: "other".to_owned(),
                enclosing_symbol: None,
                occurrences: vec![occurrence],
                score: 1.0,
                reason_codes: vec!["semantic_reference".to_owned()],
            });
        }

        self.serialize(ReferenceResponse {
            mode: "references",
            operation: "references",
            symbol: symbol_name,
            snapshot_id: snapshot_id.to_owned(),
            target: target.target,
            target_evidence: target.target_evidence,
            backend: "semantic".to_owned(),
            freshness: metadata.freshness.to_owned(),
            evidence: "semantic".to_owned(),
            evidence_caveat: metadata.evidence_caveat.to_owned(),
            semantic: true,
            definition_count: 1,
            definitions: vec![ReferenceDefinition {
                path: definition_path,
                symbol: target.symbol,
            }],
            reference_scope: "all".to_owned(),
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
        request: &FallbackReferenceRequest<'_>,
        target: ReferenceTargetSelection,
    ) -> AppResult<Value> {
        let path_filters = PathFilterSet::new(request.path_filters);
        let target_path = target.target.path.clone();
        let target_file = self.index.get(&target_path).ok_or_else(|| {
            AppError::details(
                "SYMBOL_NOT_FOUND",
                "Reference target file is no longer indexed",
                json!({"path": target_path}),
            )
        })?;

        let mut files = self
            .index
            .files()
            .filter(|file| path_filters.allows(&file.path))
            .filter(|file| reference_scope_allows(request.reference_scope, file))
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let scanned_scope = ScannedScope {
            file_count: files.len(),
            byte_count: files.iter().map(|file| file.content.len()).sum(),
        };

        let per_file_limit = request.max_results.clamp(1, 3);
        let mut groups: Vec<VecDeque<ReferenceLocation>> = Vec::new();
        if request
            .reference_kinds
            .iter()
            .any(|kind| kind == "declaration")
            && path_filters.allows(&target_path)
            && reference_scope_allows(request.reference_scope, target_file)
        {
            groups.push(VecDeque::from([declaration_location(
                request.workspace_id,
                &target_path,
                target_file,
                &target.symbol,
                target.declaration_occurrence.as_ref(),
            )?]));
        }

        let mut total_windows = 0usize;
        let mut total_occurrences = 0usize;
        for file in files {
            let occurrences = identifier_occurrences_with_symbols(
                Path::new(&file.path),
                &file.content,
                &target.symbol.name,
                &file.symbols,
            );
            if occurrences.is_empty() {
                continue;
            }
            let mut windows: Vec<ReferenceWindow> = Vec::new();
            for occurrence in occurrences {
                if file.path == target_path
                    && target.selected_declaration.is_some_and(|selected| {
                        selected == (occurrence.start_byte, occurrence.end_byte)
                    })
                {
                    continue;
                }
                if !request.reference_kinds.is_empty()
                    && !request
                        .reference_kinds
                        .iter()
                        .any(|wanted| wanted == occurrence.role)
                {
                    continue;
                }
                total_occurrences += 1;
                let start_line = occurrence
                    .start_line
                    .saturating_sub(request.context_lines)
                    .max(1);
                let end_line = (occurrence.end_line + request.context_lines).min(file.line_count);
                if let Some(previous) = windows.last_mut() {
                    if start_line <= previous.end_line.saturating_add(1)
                        && previous.occurrences.len() < 3
                    {
                        previous.end_line = previous.end_line.max(end_line);
                        if reference_kind_priority(occurrence.role)
                            < reference_kind_priority(previous.reference_kind)
                        {
                            previous.reference_kind = occurrence.role;
                            previous.classification_evidence = occurrence.evidence;
                            previous.enclosing_symbol = occurrence.enclosing_symbol.clone();
                        }
                        previous.occurrences.push(occurrence);
                        continue;
                    }
                }
                windows.push(ReferenceWindow {
                    line_number: occurrence.start_line,
                    start_line,
                    end_line,
                    reference_kind: occurrence.role,
                    classification_evidence: occurrence.evidence,
                    enclosing_symbol: occurrence.enclosing_symbol.clone(),
                    occurrences: vec![occurrence],
                });
            }
            total_windows += windows.len();
            windows.sort_by(|left, right| {
                reference_kind_priority(left.reference_kind)
                    .cmp(&reference_kind_priority(right.reference_kind))
                    .then_with(|| left.line_number.cmp(&right.line_number))
            });
            let mut file_results = VecDeque::new();
            for window in windows.into_iter().take(per_file_limit) {
                file_results.push_back(reference_location(request.workspace_id, file, window)?);
            }
            if !file_results.is_empty() {
                groups.push(file_results);
            }
        }

        groups.sort_by_key(|group| {
            group
                .front()
                .map(|result| reference_kind_priority(&result.reference_kind))
                .unwrap_or(u8::MAX)
        });
        let mut results = Vec::new();
        while results.len() < request.max_results.max(1) {
            let mut added = false;
            for group in &mut groups {
                if let Some(result) = group.pop_front() {
                    results.push(result);
                    added = true;
                    if results.len() >= request.max_results.max(1) {
                        break;
                    }
                }
            }
            if !added {
                break;
            }
        }

        let definitions = vec![ReferenceDefinition {
            path: target_path,
            symbol: target.symbol.clone(),
        }];
        self.serialize(ReferenceResponse {
            mode: "references",
            operation: "references",
            symbol: request.selector.to_owned(),
            snapshot_id: request.snapshot_id.to_owned(),
            target: target.target,
            target_evidence: target.target_evidence,
            backend: "fallback".to_owned(),
            freshness: "current".to_owned(),
            evidence: "lexical".to_owned(),
            evidence_caveat: "Complete exact-identifier fallback scan; same-named unrelated identifiers may be included, while aliases and dynamic references may be missed.".to_owned(),
            semantic: false,
            definition_count: definitions.len(),
            definitions,
            reference_scope: request.reference_scope.to_owned(),
            reference_kinds: request.reference_kinds.to_vec(),
            scanned_scope: Some(scanned_scope),
            occurrence_count: total_occurrences,
            result_count: results.len(),
            truncated: total_windows > results.len(),
            results,
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
            .filter(|(_, symbol, _)| definition_line.is_none_or(|line| line == symbol.start_line))
            .map(|(path, symbol, _)| (path, symbol))
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.start_line.cmp(&right.1.start_line))
                .then_with(|| left.1.end_line.cmp(&right.1.end_line))
        });
        if definitions.is_empty() {
            return Err(AppError::details(
                "SYMBOL_NOT_FOUND",
                "Reference search requires an indexed symbol definition",
                json!({
                    "symbol": selector,
                    "suggested_action": "Use literal search for arbitrary text or symbol search to find the declaration.",
                }),
            ));
        }
        if definitions.len() > 1 {
            let candidates = definitions
                .iter()
                .map(|(path, symbol)| json!({"path": path, "symbol": symbol}))
                .collect::<Vec<_>>();
            return Err(AppError::details(
                "AMBIGUOUS_SYMBOL",
                "Reference search found multiple declarations; specify definition_path and definition_line",
                json!({"symbol": selector, "candidates": candidates}),
            ));
        }

        let (path, symbol) = definitions.pop().expect("one reference target");
        let file = self.index.get(&path).ok_or_else(|| {
            AppError::details(
                "SYMBOL_NOT_FOUND",
                "Reference target file is no longer indexed",
                json!({"path": path}),
            )
        })?;
        let occurrences = identifier_occurrences_with_symbols(
            Path::new(&path),
            &file.content,
            &symbol.name,
            &file.symbols,
        );
        let declaration_occurrence = occurrences
            .iter()
            .find(|occurrence| {
                occurrence.role == "declaration"
                    && occurrence.start_line >= symbol.start_line
                    && occurrence.start_line <= symbol.end_line
            })
            .or_else(|| {
                occurrences.iter().find(|occurrence| {
                    occurrence.start_line >= symbol.start_line
                        && occurrence.start_line <= symbol.end_line
                })
            })
            .cloned();
        let selected_declaration = declaration_occurrence
            .as_ref()
            .map(|occurrence| (occurrence.start_byte, occurrence.end_byte));
        let range = declaration_occurrence
            .as_ref()
            .map_or_else(|| symbol_range(&symbol), occurrence_range);
        let target_evidence = declaration_occurrence
            .as_ref()
            .map_or("syntactic", |occurrence| occurrence.evidence)
            .to_owned();
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

    fn serialize(&self, response: ReferenceResponse) -> AppResult<Value> {
        serde_json::to_value(response).map_err(AppError::internal)
    }
}

struct ReferenceWindow {
    line_number: usize,
    start_line: usize,
    end_line: usize,
    reference_kind: &'static str,
    classification_evidence: &'static str,
    enclosing_symbol: Option<String>,
    occurrences: Vec<IdentifierOccurrence>,
}

fn reference_location(
    workspace_id: &str,
    file: &FileEntry,
    window: ReferenceWindow,
) -> AppResult<ReferenceLocation> {
    let handle = encode_handle(&RangeHandle {
        workspace_id: workspace_id.to_owned(),
        path: file.path.clone(),
        start_line: window.start_line,
        end_line: window.end_line,
        content_hash: file.hash.clone(),
    })?;
    let occurrences = window
        .occurrences
        .iter()
        .map(reference_occurrence)
        .collect::<Vec<_>>();
    let range = occurrences
        .first()
        .map(|occurrence| occurrence.range.clone())
        .expect("reference window contains an occurrence");
    Ok(ReferenceLocation {
        path: file.path.clone(),
        line: window.line_number,
        start_line: window.start_line,
        end_line: window.end_line,
        range,
        preview: Some(slice_lines(
            &file.content,
            window.start_line,
            window.end_line,
        )),
        handle: Some(handle),
        match_type: "reference".to_owned(),
        evidence: "lexical".to_owned(),
        classification_evidence: window.classification_evidence.to_owned(),
        freshness: "current".to_owned(),
        reference_kind: window.reference_kind.to_owned(),
        role: window.reference_kind.to_owned(),
        enclosing_symbol: window.enclosing_symbol,
        occurrences,
        score: reference_kind_score(window.reference_kind),
        reason_codes: vec![format!("{}_reference", window.reference_kind)],
    })
}

fn declaration_location(
    workspace_id: &str,
    path: &str,
    file: &FileEntry,
    symbol: &Symbol,
    occurrence: Option<&IdentifierOccurrence>,
) -> AppResult<ReferenceLocation> {
    let line = occurrence.map_or(symbol.start_line, |item| item.start_line);
    let range = occurrence.map_or_else(|| symbol_range(symbol), occurrence_range);
    let enclosing_symbol = occurrence.and_then(|item| item.enclosing_symbol.clone());
    let occurrences = occurrence
        .map(reference_occurrence)
        .into_iter()
        .collect::<Vec<_>>();
    let handle = encode_handle(&RangeHandle {
        workspace_id: workspace_id.to_owned(),
        path: path.to_owned(),
        start_line: symbol.start_line,
        end_line: symbol.end_line,
        content_hash: file.hash.clone(),
    })?;
    Ok(ReferenceLocation {
        path: path.to_owned(),
        line,
        start_line: symbol.start_line,
        end_line: symbol.end_line,
        range,
        preview: Some(slice_lines(
            &file.content,
            symbol.start_line,
            symbol.end_line,
        )),
        handle: Some(handle),
        match_type: "declaration".to_owned(),
        evidence: "syntactic".to_owned(),
        classification_evidence: "syntactic".to_owned(),
        freshness: "current".to_owned(),
        reference_kind: "declaration".to_owned(),
        role: "declaration".to_owned(),
        enclosing_symbol,
        occurrences,
        score: 1.0,
        reason_codes: vec!["declaration_match".to_owned()],
    })
}

fn reference_occurrence(occurrence: &IdentifierOccurrence) -> ReferenceOccurrence {
    ReferenceOccurrence {
        range: occurrence_range(occurrence),
        role: occurrence.role.to_owned(),
        evidence: occurrence.evidence.to_owned(),
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

fn reference_scope_allows(scope: &str, file: &FileEntry) -> bool {
    match scope {
        "all" | "" => true,
        "tests" => file.document_type == "test",
        "production" => {
            file.document_type == "source"
                && !file.path.starts_with("docs/")
                && !file.path.starts_with("examples/")
        }
        _ => false,
    }
}

fn reference_kind_priority(kind: &str) -> u8 {
    match kind {
        "declaration" => 0,
        "call" => 1,
        "write" => 2,
        "type" => 3,
        "import" => 4,
        "read" => 5,
        _ => 6,
    }
}

fn reference_kind_score(kind: &str) -> f64 {
    match kind {
        "declaration" | "call" => 1.0,
        "write" => 0.9,
        "type" => 0.85,
        "import" => 0.75,
        "read" => 0.65,
        _ => 0.5,
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}
