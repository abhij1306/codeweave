use super::metadata::query_terms;
use super::path_filter::PathFilterSet;
use super::{encode_handle, slice_lines, CodeIndex, FileEntry, RangeHandle};
use crate::model::{AppError, AppResult};
use crate::reference_service::{FallbackReferenceRequest, ReferenceService};
use regex::{Regex, RegexBuilder};
use serde_json::json;
use std::collections::{BTreeMap, HashSet, VecDeque};

pub struct SearchParams<'a> {
    pub workspace_id: &'a str,
    pub snapshot_id: &'a str,
    pub mode: &'a str,
    pub query: &'a str,
    pub path_filters: &'a [String],
    pub case_sensitive: bool,
    pub max_results: usize,
    pub context_lines: usize,
    pub reference_scope: &'a str,
    pub reference_kinds: &'a [String],
    pub definition_path: Option<&'a str>,
    pub definition_line: Option<usize>,
}

struct TextSearchParams<'a> {
    workspace_id: &'a str,
    snapshot_id: &'a str,
    query: &'a str,
    path_filters: &'a PathFilterSet<'a>,
    case_sensitive: bool,
    max_results: usize,
    context_lines: usize,
    regex: Option<&'a Regex>,
}

impl CodeIndex {
    pub fn search(&self, params: SearchParams<'_>) -> AppResult<serde_json::Value> {
        let SearchParams {
            workspace_id,
            snapshot_id,
            mode,
            query,
            path_filters,
            case_sensitive,
            max_results,
            context_lines,
            reference_scope,
            reference_kinds,
            definition_path,
            definition_line,
        } = params;
        let max_results = max_results.max(1);
        let raw_path_filters = path_filters;
        let path_filters = PathFilterSet::new(raw_path_filters);
        match mode {
            "literal" => self.search_text(TextSearchParams {
                workspace_id,
                snapshot_id,
                query,
                path_filters: &path_filters,
                case_sensitive,
                max_results,
                context_lines,
                regex: None,
            }),
            "regex" => {
                let regex = RegexBuilder::new(query)
                    .case_insensitive(!case_sensitive)
                    .build()
                    .map_err(|error| {
                        AppError::details(
                            "INVALID_REGEX",
                            error.to_string(),
                            json!({"query_length": query.len()}),
                        )
                    })?;
                self.search_text(TextSearchParams {
                    workspace_id,
                    snapshot_id,
                    query,
                    path_filters: &path_filters,
                    case_sensitive,
                    max_results,
                    context_lines,
                    regex: Some(&regex),
                })
            }
            "filename" => {
                let matcher = FilenameMatcher::new(query, case_sensitive)?;
                let mut paths: Vec<_> = self
                    .files
                    .values()
                    .filter(|file| path_filters.allows(&file.path))
                    .filter(|file| matcher.matches(file))
                    .map(|file| {
                        json!({
                            "path": file.path,
                            "match_type": matcher.semantics(),
                            "score": 1.0,
                            "reason_codes": ["filename_match"]
                        })
                    })
                    .collect();
                paths.sort_by_key(|value| {
                    value
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned()
                });
                let total = paths.len();
                paths.truncate(max_results);
                Ok(json!({
                    "mode": mode,
                    "match_semantics": matcher.semantics(),
                    "snapshot_id": snapshot_id,
                    "result_count": paths.len(),
                    "truncated": total > paths.len(),
                    "results": paths
                }))
            }
            "symbol" => {
                let results =
                    self.symbol_results(workspace_id, query, &path_filters, max_results, false);
                Ok(
                    json!({"mode": mode, "snapshot_id": snapshot_id, "result_count": results.len(), "results": results}),
                )
            }
            "references" => ReferenceService::new(self).fallback(FallbackReferenceRequest {
                workspace_id,
                snapshot_id,
                selector: query,
                path_filters: raw_path_filters,
                max_results,
                context_lines,
                reference_scope,
                reference_kinds,
                definition_path,
                definition_line,
            }),
            "outline" => {
                let file = self.get(query).ok_or_else(|| {
                    AppError::details(
                        "PATH_NOT_INDEXED",
                        "Outline path is not indexed",
                        json!({"path": query}),
                    )
                })?;
                Ok(
                    json!({"mode": mode, "path": file.path, "hash": file.hash, "symbols": file.symbols}),
                )
            }
            "repo_map" => Ok(self.repo_map(snapshot_id, max_results, &path_filters)),
            _ => Err(AppError::details(
                "INVALID_MODE",
                "Unknown search mode",
                json!({"mode": mode}),
            )),
        }
    }

    fn search_text(&self, params: TextSearchParams<'_>) -> AppResult<serde_json::Value> {
        let TextSearchParams {
            workspace_id,
            snapshot_id,
            query,
            path_filters,
            case_sensitive,
            max_results,
            context_lines,
            regex,
        } = params;
        if query.is_empty() {
            return Err(AppError::invalid("query is required for text search"));
        }
        let needle = if case_sensitive {
            query.to_owned()
        } else {
            query.to_ascii_lowercase()
        };
        let terms = if regex.is_none() {
            query_terms(query)
        } else {
            Vec::new()
        };
        let mut files = if regex.is_some() {
            self.files.values().collect::<Vec<_>>()
        } else {
            self.candidate_files(&terms)
        };
        files.sort_by(|left, right| left.path.cmp(&right.path));

        let per_file_limit = if path_filters.len() == 1 {
            max_results
        } else {
            max_results.min(8)
        };
        let mut groups: Vec<VecDeque<serde_json::Value>> = Vec::new();
        let mut total_windows = 0usize;
        for file in files {
            if !path_filters.allows(&file.path) {
                continue;
            }
            let mut windows: Vec<(usize, usize, usize)> = Vec::new();
            let mut record_match = |index: usize| {
                let start = index.saturating_sub(context_lines) + 1;
                let end = (index + context_lines + 1).min(file.line_count);
                if let Some((_, _, previous_end)) = windows.last_mut() {
                    if start <= previous_end.saturating_add(1) {
                        *previous_end = (*previous_end).max(end);
                        return;
                    }
                }
                windows.push((index + 1, start, end));
            };
            if let Some(regex) = regex {
                for (index, line) in file.content.lines().enumerate() {
                    if regex.is_match(line) {
                        record_match(index);
                    }
                }
            } else if case_sensitive {
                for (index, line) in file.content.lines().enumerate() {
                    if line.contains(&needle) {
                        record_match(index);
                    }
                }
            } else {
                for (index, line) in file.search_content.lines().enumerate() {
                    if line.contains(&needle) {
                        record_match(index);
                    }
                }
            }
            total_windows += windows.len();
            let mut file_results = Vec::new();
            for (line, start, end) in windows.into_iter().take(per_file_limit) {
                let handle = encode_handle(&RangeHandle {
                    workspace_id: workspace_id.to_owned(),
                    path: file.path.clone(),
                    start_line: start,
                    end_line: end,
                    content_hash: file.hash.clone(),
                })?;
                file_results.push(json!({
                    "path": file.path,
                    "line": line,
                    "start_line": start,
                    "end_line": end,
                    "preview": slice_lines(&file.content, start, end),
                    "handle": handle,
                    "match_type": if regex.is_some() {"regex"} else {"literal"},
                    "score": 1.0,
                    "reason_codes": [if regex.is_some() {"regex_match"} else {"literal_match"}]
                }));
            }
            if !file_results.is_empty() {
                groups.push(file_results.into());
            }
        }

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
            "mode": if regex.is_some() {"regex"} else {"literal"},
            "snapshot_id": snapshot_id,
            "result_count": results.len(),
            "truncated": total_windows > results.len(),
            "results": results
        }))
    }

    fn symbol_results(
        &self,
        workspace_id: &str,
        query: &str,
        path_filters: &PathFilterSet<'_>,
        max_results: usize,
        exact: bool,
    ) -> Vec<serde_json::Value> {
        let needle = query.to_ascii_lowercase();
        let mut results = Vec::new();
        for name in self.symbol_index.keys() {
            let normalized_name = name.to_ascii_lowercase();
            if (exact && normalized_name != needle)
                || (!exact && !normalized_name.contains(&needle))
            {
                continue;
            }
            let rank = if normalized_name == needle {
                0
            } else if normalized_name.starts_with(&needle) {
                1
            } else {
                2
            };
            for (file, symbol) in self.indexed_symbols(name) {
                if !path_filters.allows(&file.path) {
                    continue;
                }
                let handle = encode_handle(&RangeHandle {
                    workspace_id: workspace_id.to_owned(),
                    path: file.path.clone(),
                    start_line: symbol.start_line,
                    end_line: symbol.end_line,
                    content_hash: file.hash.clone(),
                })
                .unwrap_or_default();
                results.push((
                    rank,
                    file.path.clone(),
                    symbol.start_line,
                    json!({
                        "path": file.path,
                        "symbol": symbol,
                        "handle": handle,
                        "match_type": if rank == 0 {"exact_symbol"} else if rank == 1 {"prefix_symbol"} else {"contains_symbol"},
                        "score": match rank {
                            0 => 1.0,
                            1 => 0.8,
                            _ => 0.6,
                        },
                        "reason_codes": [if rank == 0 {"exact_symbol"} else {"symbol_match"}]
                    }),
                ));
            }
        }
        results.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        results
            .into_iter()
            .take(max_results)
            .map(|(_, _, _, value)| value)
            .collect()
    }

    fn repo_map(
        &self,
        snapshot_id: &str,
        limit: usize,
        path_filters: &PathFilterSet<'_>,
    ) -> serde_json::Value {
        let mut directories: BTreeMap<String, (usize, HashSet<String>)> = BTreeMap::new();
        let mut scoped_file_count = 0usize;
        for file in self.files.values() {
            if !path_filters.allows(&file.path) {
                continue;
            }
            scoped_file_count += 1;
            let directory = file
                .path
                .rsplit_once('/')
                .map(|(dir, _)| dir)
                .unwrap_or(".")
                .to_owned();
            let value = directories
                .entry(directory)
                .or_insert_with(|| (0, HashSet::new()));
            value.0 += 1;
            value.1.insert(file.language.clone());
        }
        let entries: Vec<_> = directories
            .into_iter()
            .take(limit)
            .map(|(path, (files, languages))| {
                let mut languages: Vec<_> = languages.into_iter().collect();
                languages.sort();
                json!({"path": path, "files": files, "languages": languages})
            })
            .collect();
        json!({
            "mode": "repo_map",
            "snapshot_id": snapshot_id,
            "directories": entries,
            "file_count": scoped_file_count,
            "total_file_count": self.file_count(),
            "scope_applied": path_filters.len() > 0
        })
    }
}

enum FilenameMatcher {
    Substring {
        needle: String,
        case_sensitive: bool,
    },
    Glob {
        regex: Regex,
    },
}

impl FilenameMatcher {
    fn new(query: &str, case_sensitive: bool) -> AppResult<Self> {
        if has_glob_wildcards(query) {
            let pattern = glob_pattern_to_regex(query);
            let regex = RegexBuilder::new(&pattern)
                .case_insensitive(!case_sensitive)
                .build()
                .map_err(AppError::internal)?;
            return Ok(Self::Glob { regex });
        }
        Ok(Self::Substring {
            needle: if case_sensitive {
                query.to_owned()
            } else {
                query.to_ascii_lowercase()
            },
            case_sensitive,
        })
    }

    fn matches(&self, file: &FileEntry) -> bool {
        match self {
            Self::Substring {
                needle,
                case_sensitive,
            } => {
                let hay = if *case_sensitive {
                    &file.path
                } else {
                    &file.path_lower
                };
                hay.contains(needle)
            }
            Self::Glob { regex } => regex.is_match(&file.path),
        }
    }

    fn semantics(&self) -> &'static str {
        match self {
            Self::Substring { .. } => "substring",
            Self::Glob { .. } => "glob",
        }
    }
}

fn has_glob_wildcards(query: &str) -> bool {
    query.contains('*') || query.contains('?')
}

fn glob_pattern_to_regex(pattern: &str) -> String {
    let mut regex = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            _ => regex.push_str(&regex::escape(&ch.to_string())),
        }
    }
    regex.push('$');
    regex
}
