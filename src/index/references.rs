use super::metadata::query_terms;
use super::path_filter::{normalize, PathFilterSet};
use super::{encode_handle, slice_lines, CodeIndex, FileEntry, RangeHandle};
use crate::model::{AppError, AppResult};
use regex::Regex;
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};

impl CodeIndex {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn reference_results(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
        query: &str,
        path_filters: &PathFilterSet<'_>,
        max_results: usize,
        context_lines: usize,
        reference_scope: &str,
        reference_kinds: &[String],
        definition_path: Option<&str>,
        definition_line: Option<usize>,
    ) -> AppResult<serde_json::Value> {
        let symbol_name = query.trim();
        if symbol_name.is_empty() {
            return Err(AppError::invalid("query is required for reference search"));
        }
        let mut definitions = Vec::new();
        let mut declaration_lines: HashMap<String, HashSet<usize>> = HashMap::new();
        for (file, symbol) in self.indexed_symbols(symbol_name) {
            if definition_path.is_some_and(|path| normalize(path) != file.path)
                || definition_line.is_some_and(|line| line != symbol.start_line)
            {
                continue;
            }
            declaration_lines
                .entry(file.path.clone())
                .or_default()
                .insert(symbol.start_line);
            definitions.push(json!({
                "path": file.path,
                "symbol": symbol,
            }));
        }
        if definitions.is_empty() {
            return Err(AppError::details(
                "SYMBOL_NOT_FOUND",
                "Reference search requires an indexed symbol definition",
                json!({
                    "symbol": symbol_name,
                    "suggested_action": "Use literal search for arbitrary text or symbol search to find the declaration.",
                }),
            ));
        }
        if definition_path.is_none() && definition_line.is_none() && definitions.len() > 1 {
            return Err(AppError::details(
                "AMBIGUOUS_SYMBOL",
                "Reference search found multiple declarations; specify definition_path and optional definition_line",
                json!({"symbol": symbol_name, "candidates": definitions}),
            ));
        }
        definitions.sort_by(|a, b| {
            a.get("path")
                .and_then(serde_json::Value::as_str)
                .cmp(&b.get("path").and_then(serde_json::Value::as_str))
        });
        let identifier = Regex::new(&format!(r"\b{}\b", regex::escape(symbol_name)))
            .map_err(AppError::internal)?;
        let terms = query_terms(symbol_name);
        let mut files: Vec<_> = if terms.is_empty() {
            self.files.values().collect()
        } else {
            let mut paths = HashSet::new();
            for term in &terms {
                if let Some(matches) = self.token_index.get(term) {
                    paths.extend(matches.iter().cloned());
                }
            }
            paths
                .iter()
                .filter_map(|path| self.files.get(path))
                .collect()
        };
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let per_file_limit = max_results.clamp(1, 3);
        let mut groups: Vec<VecDeque<serde_json::Value>> = Vec::new();
        if reference_kinds.iter().any(|kind| kind == "declaration") {
            let mut declarations = VecDeque::new();
            for definition in &definitions {
                let Some(path) = definition.get("path").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let Some(symbol) = definition.get("symbol") else {
                    continue;
                };
                let start_line = symbol
                    .get("start_line")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1) as usize;
                let end_line = symbol
                    .get("end_line")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(start_line as u64) as usize;
                let Some(file) = self.get(path) else {
                    continue;
                };
                if !path_filters.allows(path) || !reference_scope_allows(reference_scope, file) {
                    continue;
                }
                let handle = encode_handle(&RangeHandle {
                    version: 1,
                    workspace_id: workspace_id.to_owned(),
                    path: path.to_owned(),
                    start_line,
                    end_line,
                    content_hash: file.hash.clone(),
                })?;
                declarations.push_back(json!({
                    "path": path,
                    "line": start_line,
                    "start_line": start_line,
                    "end_line": end_line,
                    "preview": slice_lines(&file.content, start_line, end_line),
                    "handle": handle,
                    "match_type": "declaration",
                    "evidence": "syntactic",
                    "classification_evidence": "syntactic",
                    "reference_kind": "declaration",
                    "score": 1.0,
                    "reason_codes": ["declaration_match"]
                }));
            }
            if !declarations.is_empty() {
                groups.push(declarations);
            }
        }
        let mut total_windows = 0usize;
        for file in files {
            if !path_filters.allows(&file.path) {
                continue;
            }
            if !reference_scope_allows(reference_scope, file) {
                continue;
            }
            let mut windows: Vec<(usize, usize, usize, &'static str)> = Vec::new();
            let mut lines_in_current_window = 0usize;
            for (index, line) in file.content.lines().enumerate() {
                let line_number = index + 1;
                if declaration_lines
                    .get(&file.path)
                    .is_some_and(|lines| lines.contains(&line_number))
                    || !identifier.is_match(line)
                {
                    continue;
                }
                let kind = classify_reference_kind(file, line, symbol_name);
                if !reference_kinds.is_empty()
                    && !reference_kinds.iter().any(|wanted| wanted == kind)
                {
                    continue;
                }
                let start = index.saturating_sub(context_lines) + 1;
                let end = (index + context_lines + 1).min(file.line_count);
                if let Some((_, _, previous_end, previous_kind)) = windows.last_mut() {
                    if start <= previous_end.saturating_add(1) && lines_in_current_window < 3 {
                        *previous_end = (*previous_end).max(end);
                        if reference_kind_priority(kind) < reference_kind_priority(previous_kind) {
                            *previous_kind = kind;
                        }
                        lines_in_current_window += 1;
                        continue;
                    }
                }
                windows.push((line_number, start, end, kind));
                lines_in_current_window = 1;
            }
            total_windows += windows.len();
            windows.sort_by(|a, b| {
                reference_kind_priority(a.3)
                    .cmp(&reference_kind_priority(b.3))
                    .then_with(|| a.0.cmp(&b.0))
            });
            let mut file_results = VecDeque::new();
            for (line_number, start, end, reference_kind) in
                windows.into_iter().take(per_file_limit)
            {
                let classification_evidence =
                    if file.language != "text" && file.language != "markdown" {
                        "syntactic"
                    } else {
                        "lexical"
                    };
                let handle = encode_handle(&RangeHandle {
                    version: 1,
                    workspace_id: workspace_id.to_owned(),
                    path: file.path.clone(),
                    start_line: start,
                    end_line: end,
                    content_hash: file.hash.clone(),
                })?;
                file_results.push_back(json!({
                    "path": file.path,
                    "line": line_number,
                    "start_line": start,
                    "end_line": end,
                    "preview": slice_lines(&file.content, start, end),
                    "handle": handle,
                    "match_type": "reference",
                    "evidence": "lexical",
                    "classification_evidence": classification_evidence,
                    "reference_kind": reference_kind,
                    "score": reference_kind_score(reference_kind),
                    "reason_codes": [format!("{reference_kind}_reference")]
                }));
            }
            if !file_results.is_empty() {
                groups.push(file_results);
            }
        }
        groups.sort_by_key(|group| {
            group
                .front()
                .and_then(|result| result.get("reference_kind"))
                .and_then(serde_json::Value::as_str)
                .map(reference_kind_priority)
                .unwrap_or(u8::MAX)
        });
        let mut results = Vec::new();
        while results.len() < max_results {
            let mut added = false;
            for group in &mut groups {
                if let Some(result) = group.pop_front() {
                    results.push(result);
                    added = true;
                    if results.len() >= max_results {
                        break;
                    }
                }
            }
            if !added {
                break;
            }
        }
        Ok(json!({
            "mode": "references",
            "symbol": symbol_name,
            "snapshot_id": snapshot_id,
            // A5 honesty: reference lookup is a whole-word lexical scan (\b<name>\b).
            // It cannot distinguish overloads, shadowing, or same-named identifiers
            // in other scopes. Label the evidence so callers don't treat these as
            // resolver-grade (semantic) references.
            "evidence": "lexical",
            "evidence_caveat": "Lexical whole-word matches; may include unrelated identifiers with the same name and miss aliased or dynamically referenced uses.",
            "definition_count": definitions.len(),
            "definitions": definitions,
            "reference_scope": reference_scope,
            "reference_kinds": reference_kinds,
            "result_count": results.len(),
            "truncated": total_windows > results.len(),
            "results": results,
        }))
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

/// Tree-sitter is the parser used to index this file. This lightweight node
/// classification intentionally stays conservative: resolution remains lexical
/// until the optional LSP backend supplies a semantic target.
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

fn classify_reference_kind(file: &FileEntry, line: &str, symbol: &str) -> &'static str {
    let trimmed = line.trim();
    if trimmed.starts_with("use ")
        || trimmed.starts_with("import ")
        || trimmed.starts_with("from ")
        || trimmed.starts_with("#include")
    {
        return "import";
    }
    let escaped = regex::escape(symbol);
    let call = Regex::new(&format!(r"\b{escaped}\s*\(")).ok();
    if call.is_some_and(|pattern| pattern.is_match(trimmed)) {
        return "call";
    }
    let write = Regex::new(&format!(r"\b{escaped}\s*(?:=|\+=|-=|\*=|/=)")).ok();
    if write.is_some_and(|pattern| pattern.is_match(trimmed)) {
        return "write";
    }
    let type_reference = Regex::new(&format!(r"(?::|->|as\s+)\s*{escaped}\b")).ok();
    if type_reference.is_some_and(|pattern| pattern.is_match(trimmed)) {
        return "type";
    }
    if file.language == "text" || file.language == "markdown" {
        "other"
    } else {
        "read"
    }
}
