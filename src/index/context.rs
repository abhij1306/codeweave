use super::chunks;
use super::lines::{byte_to_line, excerpt_lines_with_count, fit_excerpt, line_start_byte};
use super::metadata::{
    compact_reason_codes, evidence_allowed, low_signal_context_path, query_terms,
};
use super::path_filter::PathFilterSet;
use super::{encode_handle, slice_lines, CodeIndex, FileEntry, RangeHandle, SearchMatch};
use crate::model::{AppError, AppResult};
use crate::symbols::Symbol;
use serde_json::json;
use std::collections::{BTreeMap, HashSet};

pub struct ContextParams<'a> {
    pub workspace_id: &'a str,
    pub snapshot_id: &'a str,
    pub query: &'a str,
    /// Neutral structured concepts. Unlike `query`, these are never folded into
    /// the natural-language phrase and unlike `optional_terms` they do not add
    /// coverage bonuses.
    pub terms: &'a [String],
    pub required_terms: &'a [String],
    pub optional_terms: &'a [String],
    pub exclude_terms: &'a [String],
    pub document_types: &'a [String],
    pub min_score: f64,
    pub path_filters: &'a [String],
    pub evidence: &'a [String],
    pub dirty: &'a HashSet<String>,
    pub recent_mutations: &'a HashSet<String>,
    pub budget_chars: usize,
    pub max_results: usize,
    pub symbol_detail: SymbolDetail,
    /// Retrieval ranking algorithm. `V1` is the legacy additive file scorer;
    /// `V2` is chunk-granular BM25F. Defaults to `V1` via `Ranking::default`.
    pub ranking: Ranking,
}

/// Controls how the internal evaluation ranker renders declaration bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SymbolDetail {
    Complete,
    #[default]
    Auto,
    None,
}

/// Evaluation-only retrieval ranking selector. The MCP server does not expose a ranking configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ranking {
    #[default]
    V1,
    V2,
}

/// Cap on rendered lines per v2 result. Symbols up to this length render whole
/// (`complete_symbol: true`); longer symbols render a leading window so one large
/// definition cannot dominate the character budget.
const MAX_RENDER_LINES: usize = 28;

/// Arguments for the v2 (chunk BM25F) ranker, shared out of `context()` so the
/// two rankers reuse candidate collection and file eligibility.
struct ContextV2<'a> {
    workspace_id: &'a str,
    snapshot_id: &'a str,
    candidate_files: &'a [&'a FileEntry],
    candidate_terms: &'a [String],
    required: &'a [String],
    neutral: &'a [String],
    relevance: &'a [String],
    query_lower: &'a str,
    literal_phrases: &'a [String],
    dirty: &'a HashSet<String>,
    recent_mutations: &'a HashSet<String>,
    min_score: f64,
    budget_chars: usize,
    max_results: usize,
    symbol_detail: SymbolDetail,
    eligible: &'a dyn Fn(&FileEntry) -> bool,
}

struct BaseFileScore {
    score: f64,
    first: usize,
    reasons: Vec<String>,
}

fn chunk_kind_label(kind: chunks::ChunkKind) -> &'static str {
    match kind {
        chunks::ChunkKind::Symbol => "symbol",
        chunks::ChunkKind::SymbolPart => "symbol_part",
        chunks::ChunkKind::Remainder => "remainder",
    }
}
impl CodeIndex {
    pub fn context(&self, params: ContextParams<'_>) -> AppResult<serde_json::Value> {
        let ContextParams {
            workspace_id,
            snapshot_id,
            query,
            terms,
            required_terms,
            optional_terms,
            exclude_terms,
            document_types,
            min_score,
            path_filters,
            evidence,
            dirty,
            recent_mutations,
            budget_chars,
            max_results,
            symbol_detail,
            ranking,
        } = params;
        let query_terms = query_terms(query);
        let neutral = normalized_terms(terms);
        let required = normalized_terms(required_terms);
        let optional = normalized_terms(optional_terms);
        let mut relevance = query_terms;
        relevance.extend(optional);
        relevance.sort();
        relevance.dedup();
        let excluded = normalized_terms(exclude_terms);
        let mut candidate_terms = required.clone();
        candidate_terms.extend(neutral.iter().cloned());
        candidate_terms.extend(relevance.iter().cloned());
        candidate_terms.sort();
        candidate_terms.dedup();
        if candidate_terms.is_empty() {
            return Err(AppError::details(
                "QUERY_REJECTED",
                "Query has no searchable terms",
                json!({
                    "field": "query",
                    "reason": "empty_after_normalization",
                    "retryable": true,
                    "retry_kind": "retry_with_changes"
                }),
            ));
        }
        let query_lower = query.to_ascii_lowercase();
        let literal_phrases = quoted_phrases(query);
        let path_filters = PathFilterSet::new(path_filters);
        let mut candidate_files = self.candidate_files(&candidate_terms);
        let mut candidate_paths: HashSet<&str> = candidate_files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        for path in dirty.iter().chain(recent_mutations) {
            if candidate_paths.insert(path.as_str()) {
                if let Some(file) = self.files.get(path) {
                    candidate_files.push(file);
                }
            }
        }

        // File-level eligibility, shared by both rankers: path filters, document
        // type, low-signal suppression, exclude/required terms, and evidence.
        let eligible = |file: &FileEntry| -> bool {
            if !path_filters.allows(&file.path) {
                return false;
            }
            if !document_types.is_empty()
                && !document_types
                    .iter()
                    .any(|document_type| document_type == &file.document_type)
            {
                return false;
            }
            if low_signal_context_path(&file.path)
                && !path_filters.explicitly_requests(&file.path, &query_lower)
            {
                return false;
            }
            if excluded.iter().any(|term| file_matches_term(file, term)) {
                return false;
            }
            if !required.iter().all(|term| file_matches_term(file, term)) {
                return false;
            }
            let changed = dirty.contains(&file.path) || recent_mutations.contains(&file.path);
            evidence.is_empty()
                || evidence_allowed(&file.document_type, evidence)
                || (evidence.iter().any(|item| item == "changes") && changed)
        };

        if ranking == Ranking::V2 {
            return self.context_v2(ContextV2 {
                workspace_id,
                snapshot_id,
                candidate_files: &candidate_files,
                candidate_terms: &candidate_terms,
                required: &required,
                neutral: &neutral,
                relevance: &relevance,
                query_lower: &query_lower,
                literal_phrases: &literal_phrases,
                dirty,
                recent_mutations,
                min_score,
                budget_chars,
                max_results,
                symbol_detail,
                eligible: &eligible,
            });
        }

        let mut candidates: Vec<(f64, &FileEntry, usize, Vec<String>)> = Vec::new();
        for file in candidate_files {
            if !eligible(file) {
                continue;
            }
            let Some(base) = base_file_score(
                file,
                &query_lower,
                &required,
                &neutral,
                &relevance,
                &literal_phrases,
                dirty,
                recent_mutations,
                (0.0, None),
            ) else {
                continue;
            };
            if base.score < min_score {
                continue;
            }
            candidates.push((base.score, file, base.first, base.reasons));
        }
        candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));
        let mut results = Vec::new();
        let mut used = 0usize;
        let mut groups: BTreeMap<String, usize> = BTreeMap::new();
        for (score, file, byte_offset, mut reasons) in
            candidates.into_iter().take(max_results.saturating_mul(3))
        {
            let match_line = byte_to_line(&file.line_starts, byte_offset);
            let (start_line, proposed_end) =
                excerpt_lines_with_count(match_line, file.line_count, 6);
            let remaining = budget_chars.saturating_sub(used);
            if remaining == 0 {
                break;
            }
            reasons = compact_reason_codes(reasons);
            let exact_symbols = exact_symbol_matches(file, &relevance);
            if exact_symbols.len() > 1 {
                for symbol in exact_symbols {
                    if results.len() >= max_results || used >= budget_chars {
                        break;
                    }
                    let remaining = budget_chars.saturating_sub(used);
                    let (excerpt_start, excerpt_end) =
                        excerpt_lines_with_count(symbol.start_line, file.line_count, 6);
                    let (preview, start_line, end_line, _complete_symbol) = render_context_preview(
                        file,
                        excerpt_start,
                        excerpt_end,
                        Some((symbol.start_line, symbol.end_line)),
                        symbol_detail,
                        true,
                        remaining,
                    );
                    if matches!(symbol_detail, SymbolDetail::Auto)
                        && preview.as_ref().is_none_or(String::is_empty)
                    {
                        continue;
                    }
                    used += preview.as_ref().map_or(0, String::len);
                    let handle = encode_handle(&RangeHandle {
                        version: 1,
                        workspace_id: workspace_id.to_owned(),
                        path: file.path.clone(),
                        start_line,
                        end_line,
                        content_hash: file.hash.clone(),
                    })?;
                    let mut symbol_reasons = reasons.clone();
                    symbol_reasons.push("multi_symbol_match".to_owned());
                    let symbol_reasons = compact_reason_codes(symbol_reasons);
                    let group = context_group(&file.document_type, &symbol_reasons);
                    *groups.entry(group.clone()).or_default() += 1;
                    results.push(SearchMatch {
                        path: file.path.clone(),
                        start_line,
                        end_line,
                        preview,
                        document_type: file.document_type.clone(),
                        score,
                        group,
                        reason_codes: symbol_reasons,
                        handle,
                        chunk_kind: None,
                        complete_symbol: None,
                    });
                }
                if results.len() >= max_results || used >= budget_chars {
                    break;
                }
                continue;
            }
            let symbol = file
                .symbols
                .iter()
                .find(|symbol| symbol.start_line <= match_line && symbol.end_line >= match_line)
                .cloned();
            let exact_symbol = reasons.iter().any(|reason| reason == "exact_symbol");
            let (preview, start_line, end_line, _complete_symbol) = render_context_preview(
                file,
                start_line,
                proposed_end,
                symbol
                    .as_ref()
                    .map(|symbol| (symbol.start_line, symbol.end_line)),
                symbol_detail,
                exact_symbol,
                remaining,
            );
            if matches!(symbol_detail, SymbolDetail::Auto)
                && preview.as_ref().is_none_or(String::is_empty)
            {
                continue;
            }
            used += preview.as_ref().map_or(0, String::len);
            let handle = encode_handle(&RangeHandle {
                version: 1,
                workspace_id: workspace_id.to_owned(),
                path: file.path.clone(),
                start_line,
                end_line,
                content_hash: file.hash.clone(),
            })?;
            let group = context_group(&file.document_type, &reasons);
            *groups.entry(group.clone()).or_default() += 1;
            results.push(SearchMatch {
                path: file.path.clone(),
                start_line,
                end_line,
                preview,
                document_type: file.document_type.clone(),
                score,
                group,
                reason_codes: reasons,
                handle,
                chunk_kind: None,
                // v1 intentionally preserves its historical response shape;
                // v2 is the version that advertises chunk completeness.
                complete_symbol: None,
            });
            if results.len() >= max_results || used >= budget_chars {
                break;
            }
        }
        Ok(json!({
            "snapshot_id": snapshot_id,
            "budget_chars": budget_chars,
            "used_chars": used,
            "result_count": results.len(),
            "groups": groups.into_iter().map(|(group, count)| json!({"group": group, "count": count})).collect::<Vec<_>>(),
            "results": results,
            "guidance": if results.is_empty() { "Try literal, filename, or symbol search." } else { "Fetch only ranges needing more detail." }
        }))
    }

    /// Chunk-granular BM25F ranking used by the offline evaluation harness as `v2`.
    ///
    /// Ranks *files* by aggregate BM25F over the content/symbol/path fields (so a
    /// file whose relevant terms are spread across several symbols still ranks),
    /// applies deterministic post-boosts (exact symbol, dirty, recent mutation,
    /// doc-type), then renders the best-matching chunk within each winning file as
    /// a symbol-bounded excerpt. Response shape matches v1 with two additive
    /// fields per result (`chunk_kind`, `complete_symbol`).
    fn context_v2(&self, params: ContextV2<'_>) -> AppResult<serde_json::Value> {
        let ContextV2 {
            workspace_id,
            snapshot_id,
            candidate_files,
            candidate_terms,
            required,
            neutral,
            relevance,
            query_lower,
            literal_phrases,
            dirty,
            recent_mutations,
            min_score,
            budget_chars,
            max_results,
            symbol_detail,
            eligible,
        } = params;

        // v2 reuses v1's proven per-file signal (which already ranks NL,
        // discovery, and symbol queries well) and layers on the two things v1
        // lacks: a filename-affinity boost (via the per-file path field) that
        // fixes filename-lookup queries, and symbol-bounded rendering so results
        // are whole symbols rather than fixed line windows.
        struct ScoredFile<'a> {
            score: f64,
            file: &'a FileEntry,
            first: usize,
            reasons: Vec<String>,
        }
        let mut scored: Vec<ScoredFile<'_>> = Vec::new();

        for file in candidate_files {
            let file = *file;
            if !eligible(file) {
                continue;
            }

            // Filename affinity (the v1 weakness this ranker targets): when most
            // query terms appear in the file's own path segments, the file *is*
            // very likely the target even if its body barely mentions them. Boost
            // proportionally to the fraction of query terms found in the path,
            // with an extra bump when the path covers *every* query term (a
            // near-certain filename lookup).
            let path_hits = candidate_terms
                .iter()
                .filter(|term| file.path_tf().contains_key(term.as_str()))
                .count();
            let mut filename_score = 0.0;
            let mut filename_reason = None;
            if path_hits > 0 {
                let affinity = path_hits as f64 / candidate_terms.len().max(1) as f64;
                filename_score += affinity * 22.0;
                if affinity >= 0.999 {
                    filename_score += 18.0;
                }
                if affinity >= 0.5 {
                    filename_reason = Some("filename_affinity");
                }
            }

            let Some(base) = base_file_score(
                file,
                query_lower,
                required,
                neutral,
                relevance,
                literal_phrases,
                dirty,
                recent_mutations,
                (filename_score, filename_reason),
            ) else {
                continue;
            };
            if base.score < min_score {
                continue;
            }
            scored.push(ScoredFile {
                score: base.score,
                file,
                first: base.first,
                reasons: base.reasons,
            });
        }

        scored.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.file.path.cmp(&b.file.path))
        });

        let mut results = Vec::new();
        let mut used = 0usize;
        let mut groups: BTreeMap<String, usize> = BTreeMap::new();

        for hit in scored.into_iter().take(max_results.saturating_mul(3)) {
            if results.len() >= max_results || used >= budget_chars {
                break;
            }
            let match_line = byte_to_line(&hit.file.line_starts, hit.first);
            let reasons = compact_reason_codes(hit.reasons.clone());
            let exact_symbols = exact_symbol_matches(hit.file, relevance);
            if exact_symbols.len() > 1 {
                for symbol in exact_symbols {
                    if results.len() >= max_results || used >= budget_chars {
                        break;
                    }
                    let remaining = budget_chars.saturating_sub(used);
                    let (excerpt_start, excerpt_end) =
                        excerpt_lines_with_count(symbol.start_line, hit.file.line_count, 6);
                    let (preview, start_line, end_line, complete_symbol) = render_context_preview(
                        hit.file,
                        excerpt_start,
                        excerpt_end,
                        Some((symbol.start_line, symbol.end_line)),
                        symbol_detail,
                        true,
                        remaining,
                    );
                    if matches!(symbol_detail, SymbolDetail::Auto)
                        && preview.as_ref().is_none_or(String::is_empty)
                    {
                        continue;
                    }
                    used += preview.as_ref().map_or(0, String::len);
                    let chunk = hit.file.chunks().iter().find(|chunk| {
                        chunk.start_line <= symbol.start_line && chunk.end_line >= symbol.end_line
                    });
                    let handle = encode_handle(&RangeHandle {
                        version: 1,
                        workspace_id: workspace_id.to_owned(),
                        path: hit.file.path.clone(),
                        start_line,
                        end_line,
                        content_hash: hit.file.hash.clone(),
                    })?;
                    let mut symbol_reasons = reasons.clone();
                    symbol_reasons.push("multi_symbol_match".to_owned());
                    let symbol_reasons = compact_reason_codes(symbol_reasons);
                    let group = context_group(&hit.file.document_type, &symbol_reasons);
                    *groups.entry(group.clone()).or_default() += 1;
                    results.push(SearchMatch {
                        path: hit.file.path.clone(),
                        start_line,
                        end_line,
                        preview,
                        document_type: hit.file.document_type.clone(),
                        score: hit.score,
                        group,
                        reason_codes: symbol_reasons,
                        handle,
                        chunk_kind: chunk.map(|chunk| chunk_kind_label(chunk.kind).to_owned()),
                        complete_symbol: Some(complete_symbol),
                    });
                }
                continue;
            }

            let chunk = hit
                .file
                .chunks()
                .iter()
                .find(|chunk| chunk.start_line <= match_line && chunk.end_line >= match_line)
                .or_else(|| hit.file.chunks().first());
            let Some(chunk) = chunk else {
                continue;
            };
            let exact_symbol = reasons.iter().any(|reason| reason == "exact_symbol");
            let exact_declaration = exact_symbols.first().copied();
            let symbol_range = exact_declaration
                .map(|symbol| (symbol.start_line, symbol.end_line))
                .or_else(|| {
                    chunk
                        .symbol
                        .as_ref()
                        .map(|_| (chunk.start_line, chunk.end_line))
                });
            let symbol_span = symbol_range
                .map(|(start, end)| end.saturating_sub(start) + 1)
                .unwrap_or(0);
            let wants_complete = symbol_detail == SymbolDetail::Complete
                || (symbol_detail == SymbolDetail::Auto && exact_symbol);
            let (render_start, render_end) =
                if wants_complete && symbol_span > 0 && symbol_span <= MAX_RENDER_LINES {
                    symbol_range.expect("checked symbol range")
                } else {
                    excerpt_lines_with_count(match_line, hit.file.line_count, 6)
                };
            let remaining = budget_chars.saturating_sub(used);
            if remaining == 0 {
                break;
            }
            let (preview, start_line, end_line, complete_symbol) = render_context_preview(
                hit.file,
                render_start,
                render_end,
                symbol_range,
                symbol_detail,
                exact_symbol,
                remaining,
            );
            if matches!(symbol_detail, SymbolDetail::Auto)
                && preview.as_ref().is_none_or(String::is_empty)
            {
                continue;
            }
            used += preview.as_ref().map_or(0, String::len);
            let handle = encode_handle(&RangeHandle {
                version: 1,
                workspace_id: workspace_id.to_owned(),
                path: hit.file.path.clone(),
                start_line,
                end_line,
                content_hash: hit.file.hash.clone(),
            })?;
            let group = context_group(&hit.file.document_type, &reasons);
            *groups.entry(group.clone()).or_default() += 1;
            results.push(SearchMatch {
                path: hit.file.path.clone(),
                start_line,
                end_line,
                preview,
                document_type: hit.file.document_type.clone(),
                score: hit.score,
                group,
                reason_codes: reasons,
                handle,
                chunk_kind: Some(chunk_kind_label(chunk.kind).to_owned()),
                complete_symbol: Some(complete_symbol),
            });
        }

        Ok(json!({
            "snapshot_id": snapshot_id,
            "budget_chars": budget_chars,
            "used_chars": used,
            "result_count": results.len(),
            "groups": groups.into_iter().map(|(group, count)| json!({"group": group, "count": count})).collect::<Vec<_>>(),
            "results": results,
            "guidance": if results.is_empty() { "Try literal, filename, or symbol search." } else { "Fetch only ranges needing more detail." }
        }))
    }
}

fn normalized_terms(values: &[String]) -> Vec<String> {
    let mut terms: Vec<_> = values.iter().flat_map(|value| query_terms(value)).collect();
    terms.sort();
    terms.dedup();
    terms
}

/// Render a ranked result without changing its range/handle contract. `complete`
/// intentionally omits a partial body when the declaration cannot fit; callers
/// can follow the handle with a `code_retrieve` read operation.
fn exact_symbol_matches<'a>(file: &'a FileEntry, relevance: &[String]) -> Vec<&'a Symbol> {
    let mut matches = Vec::new();
    for term in relevance {
        if let Some(symbol) = file
            .symbols
            .iter()
            .find(|symbol| symbol.name.to_ascii_lowercase() == *term)
        {
            if !matches.iter().any(|existing: &&Symbol| {
                existing.start_line == symbol.start_line && existing.end_line == symbol.end_line
            }) {
                matches.push(symbol);
            }
        }
    }
    matches.sort_by_key(|symbol| symbol.start_line);
    matches
}

fn render_context_preview(
    file: &FileEntry,
    excerpt_start: usize,
    excerpt_end: usize,
    symbol_range: Option<(usize, usize)>,
    detail: SymbolDetail,
    exact_symbol: bool,
    remaining: usize,
) -> (Option<String>, usize, usize, bool) {
    let complete_candidate = symbol_range.and_then(|(start, end)| {
        let content = slice_lines(&file.content, start, end);
        (!content.is_empty()).then_some((content, start, end))
    });
    if detail == SymbolDetail::None {
        return (None, excerpt_start, excerpt_end, false);
    }
    let wants_complete =
        detail == SymbolDetail::Complete || (detail == SymbolDetail::Auto && exact_symbol);
    if wants_complete {
        if let Some((content, start, end)) = complete_candidate {
            let auto_cap = if detail == SymbolDetail::Auto {
                12_000
            } else {
                usize::MAX
            };
            if content.len() <= remaining && content.len() <= auto_cap {
                return (Some(content), start, end, true);
            }
            if detail == SymbolDetail::Complete {
                return (None, start, end, false);
            }
        }
    }
    if remaining == 0 {
        return (None, excerpt_start, excerpt_end, false);
    }
    let (preview, end) = fit_excerpt(&file.content, excerpt_start, excerpt_end, remaining);
    if !preview.is_empty() {
        (Some(preview), excerpt_start, end, false)
    } else {
        (None, excerpt_start, excerpt_end, false)
    }
}

fn file_matches_term(file: &FileEntry, term: &str) -> bool {
    file.search_content.contains(term)
        || file.path_lower.contains(term)
        || file
            .symbols
            .iter()
            .any(|symbol| symbol.name.to_ascii_lowercase().contains(term))
}

#[allow(clippy::too_many_arguments)]
fn base_file_score(
    file: &FileEntry,
    query_lower: &str,
    required: &[String],
    neutral: &[String],
    relevance: &[String],
    literal_phrases: &[String],
    dirty: &HashSet<String>,
    recent_mutations: &HashSet<String>,
    layer: (f64, Option<&str>),
) -> Option<BaseFileScore> {
    let (layered_score, layered_reason) = layer;
    let lower = &file.search_content;
    let path_lower = &file.path_lower;
    let mut score = 0.0;
    let mut first = None;
    let mut reasons = Vec::new();
    let mut matched_relevance_terms = 0usize;

    if !query_lower.is_empty() && lower.contains(query_lower) {
        score += 60.0;
        reasons.push("exact_phrase".to_owned());
        first = lower.find(query_lower);
    }
    for phrase in literal_phrases {
        if path_lower.contains(phrase) {
            score += 120.0;
            reasons.push("exact_path_literal".to_owned());
            first = first.or(Some(0));
        } else if lower.contains(phrase) {
            score += 85.0;
            reasons.push("exact_literal".to_owned());
            first = first.or_else(|| lower.find(phrase));
        }
    }
    for term in required {
        if file_matches_term(file, term) {
            score += 15.0;
            reasons.push("required_term".to_owned());
            first = first.or_else(|| lower.find(term).or_else(|| path_lower.find(term)));
        }
    }
    for term in neutral {
        if file_matches_term(file, term) {
            score += 1.5;
            reasons.push("neutral_term".to_owned());
            first = first.or_else(|| lower.find(term).or_else(|| path_lower.find(term)));
        }
    }
    for term in relevance {
        let count = lower.match_indices(term).take(50).count();
        if count > 0 {
            matched_relevance_terms += 1;
            score += (count as f64).ln_1p() * 3.0;
            first = first.or_else(|| lower.find(term));
        }
        if path_lower.contains(term) {
            score += 5.0;
            reasons.push("path_match".to_owned());
        }
        if let Some(symbol) = file
            .symbols
            .iter()
            .find(|symbol| symbol.name.to_ascii_lowercase() == *term)
        {
            score += 25.0;
            reasons.push("exact_symbol".to_owned());
            first = Some(line_start_byte(&file.content, symbol.start_line));
        } else if let Some(symbol) = file
            .symbols
            .iter()
            .find(|symbol| symbol.name.to_ascii_lowercase().contains(term))
        {
            score += 7.0;
            reasons.push("symbol_match".to_owned());
            first = first.or_else(|| Some(line_start_byte(&file.content, symbol.start_line)));
        }
    }
    if matched_relevance_terms > 0 {
        let coverage = matched_relevance_terms as f64 / relevance.len().max(1) as f64;
        score += coverage * 10.0;
        if matched_relevance_terms == relevance.len() {
            reasons.push("full_term_coverage".to_owned());
        }
    }

    score += layered_score;
    if let Some(reason) = layered_reason {
        reasons.push(reason.to_owned());
    }
    if dirty.contains(&file.path) {
        score += 7.0;
        reasons.push("dirty_file".to_owned());
    }
    if recent_mutations.contains(&file.path) {
        score += 5.0;
        reasons.push("recent_mutation".to_owned());
    }
    match file.document_type.as_str() {
        "runtime_evidence" => {
            score *= 1.25;
            reasons.push("runtime_evidence".to_owned());
        }
        "test" => score *= 0.9,
        "source" => score *= 1.1,
        _ => {}
    }
    if score <= 0.0 {
        return None;
    }
    let size_units = file.content.len().max(100) as f64 / 8_192.0;
    score /= 1.0 + size_units.ln_1p().min(4.0) * 0.18;

    let exact_symbol = reasons.iter().any(|reason| reason == "exact_symbol");
    let first = if exact_symbol {
        first
    } else {
        best_match_offset(
            file,
            query_lower,
            required,
            neutral,
            relevance,
            literal_phrases,
        )
        .or(first)
    };

    Some(BaseFileScore {
        score,
        first: first.unwrap_or(0),
        reasons,
    })
}

fn quoted_phrases(query: &str) -> Vec<String> {
    let mut phrases = Vec::new();
    for delimiter in ['"', '\'', '`'] {
        let mut rest = query;
        while let Some(start) = rest.find(delimiter) {
            let after = &rest[start + delimiter.len_utf8()..];
            let Some(end) = after.find(delimiter) else {
                break;
            };
            let phrase = after[..end].trim().to_ascii_lowercase();
            if phrase.len() >= 3
                && (phrase.contains(' ') || phrase.contains('/') || phrase.contains('.'))
            {
                phrases.push(phrase);
            }
            rest = &after[end + delimiter.len_utf8()..];
        }
    }
    phrases.sort();
    phrases.dedup();
    phrases
}

fn best_match_offset(
    file: &FileEntry,
    query_lower: &str,
    required: &[String],
    neutral: &[String],
    relevance: &[String],
    literal_phrases: &[String],
) -> Option<usize> {
    let lines = file.search_content.lines().collect::<Vec<_>>();
    let mut best: Option<(f64, usize)> = None;

    for (index, line) in lines.iter().enumerate() {
        let direct_match = (!query_lower.is_empty() && line.contains(query_lower))
            || literal_phrases.iter().any(|phrase| line.contains(phrase))
            || required.iter().any(|term| line.contains(term))
            || neutral.iter().any(|term| line.contains(term))
            || relevance.iter().any(|term| line.contains(term));
        if !direct_match {
            continue;
        }

        let start = index.saturating_sub(6);
        let end = (index + 7).min(lines.len());
        let window = lines[start..end].join("\n");
        let mut score = 0.0;
        if !query_lower.is_empty() && window.contains(query_lower) {
            score += 40.0;
        }
        for phrase in literal_phrases {
            if window.contains(phrase) {
                score += 60.0;
            }
            if line.contains(phrase) {
                score += 20.0;
            }
        }
        for term in required {
            if window.contains(term) {
                score += 16.0;
            }
            if line.contains(term) {
                score += 5.0;
            }
        }
        for term in relevance {
            if window.contains(term) {
                score += 8.0;
            }
            if line.contains(term) {
                score += 3.0;
            }
        }
        for term in neutral {
            if window.contains(term) {
                score += 2.0;
            }
        }
        if file
            .symbols
            .iter()
            .any(|symbol| symbol.start_line == index + 1)
        {
            score += 4.0;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("import ")
            || trimmed.starts_with("from ")
            || trimmed.starts_with("use ")
            || trimmed.starts_with("#include")
        {
            score *= 0.45;
        }

        match best {
            Some((best_score, best_index))
                if score < best_score || (score == best_score && index >= best_index) => {}
            _ => best = Some((score, index)),
        }
    }

    best.map(|(_, index)| line_start_byte(&file.content, index + 1))
}

fn context_group(document_type: &str, reasons: &[String]) -> String {
    if reasons.iter().any(|reason| reason == "exact_symbol") {
        "symbol".to_owned()
    } else if reasons.iter().any(|reason| reason == "required_term") {
        "required".to_owned()
    } else {
        document_type.to_owned()
    }
}
