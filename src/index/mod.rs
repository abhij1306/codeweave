mod handle;
mod lines;
mod metadata;
mod path_filter;

pub use handle::{content_hash, decode_handle, encode_handle, RangeHandle};
pub use lines::slice_lines;

use crate::model::{AppError, AppResult};
use crate::security::{relative_string, validate_relative};
use crate::symbols::{extract_symbols, language_name, Symbol};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;
use lines::{
    byte_to_line, excerpt_lines_with_count, fit_excerpt, hex, line_start_byte, line_starts,
};
use metadata::{
    build_indexed_terms, classify_document, compact_reason_codes, evidence_allowed,
    low_signal_context_path, normalize_entry, query_terms,
};
use path_filter::{normalize, PathFilterSet};
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

const INDEX_SCHEMA: &str = "codeweave-index-v5";

#[derive(Clone, Debug)]
pub struct WorkspaceExclusions {
    normalized_root: String,
    matcher: Gitignore,
    patterns: Vec<String>,
}

impl WorkspaceExclusions {
    pub fn new(root: &Path, patterns: &[String]) -> AppResult<Self> {
        let mut builder = GitignoreBuilder::new(".");
        for pattern in patterns {
            let normalized = pattern.replace('\\', "/");
            if normalized.trim().is_empty() || normalized.starts_with('!') {
                return Err(AppError::details(
                    "INVALID_EXCLUDE_PATTERN",
                    "Workspace exclude patterns must be non-empty exclusions",
                    json!({"pattern": pattern}),
                ));
            }
            builder.add_line(None, &normalized).map_err(|error| {
                AppError::details(
                    "INVALID_EXCLUDE_PATTERN",
                    error.to_string(),
                    json!({"pattern": pattern}),
                )
            })?;
        }
        let matcher = builder.build().map_err(|error| {
            AppError::details(
                "INVALID_EXCLUDE_PATTERN",
                error.to_string(),
                json!({"patterns": patterns}),
            )
        })?;
        Ok(Self {
            normalized_root: normalized_absolute_path(root),
            matcher,
            patterns: patterns.to_vec(),
        })
    }

    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let Some(relative) = self.relative_path(path) else {
            return false;
        };
        ignored_workspace_path(&relative)
            || self
                .matcher
                .matched_path_or_any_parents(Path::new(&relative), is_dir)
                .is_ignore()
    }

    fn relative_path(&self, path: &Path) -> Option<String> {
        if !path.is_absolute() {
            return Some(path.to_string_lossy().replace('\\', "/"));
        }
        let normalized = normalized_absolute_path(path);
        let root_len = self.normalized_root.len();
        if normalized.len() < root_len
            || !normalized.as_bytes()[..root_len]
                .eq_ignore_ascii_case(self.normalized_root.as_bytes())
        {
            return None;
        }
        let suffix = normalized.get(root_len..)?;
        if !suffix.is_empty() && !suffix.starts_with('/') {
            return None;
        }
        Some(suffix.trim_start_matches('/').to_owned())
    }

    fn patterns(&self) -> &[String] {
        &self.patterns
    }
}

fn normalized_absolute_path(path: &Path) -> String {
    let mut normalized = path.to_string_lossy().replace('\\', "/");
    if let Some(without_verbatim_prefix) = normalized.strip_prefix("//?/") {
        normalized = without_verbatim_prefix.to_owned();
    }
    normalized.trim_end_matches('/').to_owned()
}

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
    pub symbols: Vec<Symbol>,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub modified_ns: u128,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchMatch {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub preview: String,
    pub document_type: String,
    pub score: f64,
    pub group: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
    pub handle: String,
}

pub struct SearchParams<'a> {
    pub workspace_id: &'a str,
    pub snapshot_id: &'a str,
    pub mode: &'a str,
    pub query: &'a str,
    pub path_filters: &'a [String],
    pub case_sensitive: bool,
    pub max_results: usize,
    pub context_lines: usize,
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

pub struct ContextParams<'a> {
    pub workspace_id: &'a str,
    pub snapshot_id: &'a str,
    pub query: &'a str,
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
}
#[derive(Debug, Serialize, Deserialize)]
struct CachedIndex {
    schema: String,
    root: String,
    max_file_bytes: usize,
    artifact_paths: Vec<String>,
    #[serde(default)]
    exclude_paths: Vec<String>,
    files: Vec<FileEntry>,
}

#[derive(Debug, Default)]
pub struct CodeIndex {
    files: HashMap<String, FileEntry>,
    token_index: HashMap<String, HashSet<String>>,
    symbol_index: HashMap<String, BTreeSet<(String, usize)>>,
    snapshot_dirty: bool,
    cached_snapshot_head: Option<String>,
    cached_snapshot: Option<String>,
}

impl CodeIndex {
    pub fn scan(
        root: &Path,
        max_file_bytes: usize,
        artifact_paths: &[String],
        exclusions: &WorkspaceExclusions,
    ) -> AppResult<Self> {
        Self::scan_with_cache(root, max_file_bytes, artifact_paths, exclusions, None)
            .map(|(index, _)| index)
    }

    pub fn scan_cached(
        root: &Path,
        max_file_bytes: usize,
        artifact_paths: &[String],
        exclusions: &WorkspaceExclusions,
        cache_file: &Path,
    ) -> AppResult<(Self, bool)> {
        Self::scan_with_cache(
            root,
            max_file_bytes,
            artifact_paths,
            exclusions,
            Some(cache_file),
        )
    }

    fn scan_with_cache(
        root: &Path,
        max_file_bytes: usize,
        artifact_paths: &[String],
        exclusions: &WorkspaceExclusions,
        cache_file: Option<&Path>,
    ) -> AppResult<(Self, bool)> {
        let root_key = root.to_string_lossy().into_owned();
        let cached = cache_file
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<CachedIndex>(&bytes).ok())
            .filter(|cache| {
                cache.schema == INDEX_SCHEMA
                    && cache.root == root_key
                    && cache.max_file_bytes == max_file_bytes
                    && cache.artifact_paths == artifact_paths
                    && cache.exclude_paths == exclusions.patterns()
            });
        let cache_hit = cached.is_some();
        let cached_files: HashMap<String, FileEntry> = cached
            .map(|cache| {
                cache
                    .files
                    .into_iter()
                    .map(|mut file| {
                        normalize_entry(&mut file);
                        (file.path.clone(), file)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut index = Self::default();
        scan_directory(
            &mut index,
            root,
            root,
            max_file_bytes,
            true,
            exclusions,
            &cached_files,
        )?;
        for relative in artifact_paths {
            let relative = validate_relative(relative)?;
            let candidate = root.join(relative);
            if !candidate.exists() {
                continue;
            }
            let resolved = candidate.canonicalize()?;
            if !resolved.starts_with(root) {
                return Err(AppError::new(
                    "OUTSIDE_ROOT",
                    "Artifact path resolves outside workspace",
                ));
            }
            scan_directory(
                &mut index,
                root,
                &resolved,
                max_file_bytes,
                false,
                exclusions,
                &cached_files,
            )?;
        }
        if let Some(cache_file) = cache_file {
            if let Some(parent) = cache_file.parent() {
                fs::create_dir_all(parent)?;
            }
            let cache = CachedIndex {
                schema: INDEX_SCHEMA.to_owned(),
                root: root_key,
                max_file_bytes,
                artifact_paths: artifact_paths.to_vec(),
                exclude_paths: exclusions.patterns().to_vec(),
                files: index.files.values().cloned().collect(),
            };
            let temp = cache_file.with_extension("json.tmp");
            let file = fs::File::create(&temp)?;
            let mut writer = std::io::BufWriter::new(file);
            serde_json::to_writer(&mut writer, &cache)?;
            std::io::Write::flush(&mut writer)?;
            drop(writer);
            if cache_file.exists() {
                let _ = fs::remove_file(cache_file);
            }
            if let Err(e) = fs::rename(&temp, cache_file) {
                let _ = fs::remove_file(&temp);
                return Err(e.into());
            }
        }
        Ok((index, cache_hit))
    }

    fn insert_entry(&mut self, mut entry: FileEntry) {
        normalize_entry(&mut entry);
        if let Some(previous) = self.files.remove(&entry.path) {
            self.remove_from_token_index(&previous);
            self.remove_from_symbol_index(&previous);
        }
        self.add_to_token_index(&entry);
        self.add_to_symbol_index(&entry);
        self.files.insert(entry.path.clone(), entry);
        self.snapshot_dirty = true;
    }

    fn remove_entry(&mut self, path: &str) -> Option<FileEntry> {
        let removed = self.files.remove(path)?;
        self.remove_from_token_index(&removed);
        self.remove_from_symbol_index(&removed);
        self.snapshot_dirty = true;
        Some(removed)
    }

    fn add_to_token_index(&mut self, file: &FileEntry) {
        for term in &file.indexed_terms {
            self.token_index
                .entry(term.clone())
                .or_default()
                .insert(file.path.clone());
        }
    }

    fn remove_from_token_index(&mut self, file: &FileEntry) {
        let mut empty = Vec::new();
        for term in &file.indexed_terms {
            if let Some(paths) = self.token_index.get_mut(term) {
                paths.remove(&file.path);
                if paths.is_empty() {
                    empty.push(term);
                }
            }
        }
        for term in empty {
            self.token_index.remove(term);
        }
    }

    fn add_to_symbol_index(&mut self, file: &FileEntry) {
        for (index, symbol) in file.symbols.iter().enumerate() {
            self.symbol_index
                .entry(symbol.name.clone())
                .or_default()
                .insert((file.path.clone(), index));
        }
    }

    fn remove_from_symbol_index(&mut self, file: &FileEntry) {
        let names: HashSet<_> = file
            .symbols
            .iter()
            .map(|symbol| symbol.name.clone())
            .collect();
        let mut empty = Vec::new();
        for name in names {
            if let Some(entries) = self.symbol_index.get_mut(&name) {
                entries.retain(|(path, _)| path != &file.path);
                if entries.is_empty() {
                    empty.push(name);
                }
            }
        }
        for name in empty {
            self.symbol_index.remove(&name);
        }
    }

    fn candidate_files<'a>(&'a self, terms: &[String]) -> Vec<&'a FileEntry> {
        if terms.is_empty() {
            return self.files.values().collect();
        }
        let mut paths = HashSet::new();
        for term in terms {
            if let Some(matches) = self.token_index.get(term) {
                paths.extend(matches.iter().cloned());
            }
        }
        if paths.is_empty() {
            self.files.values().collect()
        } else {
            paths
                .iter()
                .filter_map(|path| self.files.get(path))
                .collect()
        }
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }
    pub fn languages(&self) -> Vec<String> {
        let mut values: Vec<String> = self
            .files
            .values()
            .map(|file| file.language.clone())
            .filter(|value| value != "text")
            .collect();
        values.sort();
        values.dedup();
        values
    }
    pub fn get(&self, path: &str) -> Option<&FileEntry> {
        self.files.get(normalize(path).as_ref())
    }
    pub fn find_symbol(&self, path: Option<&str>, name: &str) -> Option<(String, Symbol, String)> {
        if let Some(path) = path {
            let file = self.files.get(normalize(path).as_ref())?;
            let symbol = file.symbols.iter().find(|symbol| symbol.name == name)?;
            return Some((file.path.clone(), symbol.clone(), file.hash.clone()));
        }
        for (path, symbol_index) in self.symbol_index.get(name)? {
            let Some(file) = self.files.get(path) else {
                continue;
            };
            let Some(symbol) = file.symbols.get(*symbol_index) else {
                continue;
            };
            return Some((file.path.clone(), symbol.clone(), file.hash.clone()));
        }
        None
    }

    fn indexed_symbols<'a>(
        &'a self,
        name: &str,
    ) -> impl Iterator<Item = (&'a FileEntry, &'a Symbol)> + 'a {
        self.symbol_index
            .get(name)
            .into_iter()
            .flat_map(|entries| entries.iter())
            .filter_map(|(path, symbol_index)| {
                let file = self.files.get(path)?;
                let symbol = file.symbols.get(*symbol_index)?;
                Some((file, symbol))
            })
    }

    pub fn refresh_paths(
        &mut self,
        root: &Path,
        paths: &HashSet<PathBuf>,
        max_file_bytes: usize,
        exclusions: &WorkspaceExclusions,
    ) -> AppResult<Vec<String>> {
        let mut changed = Vec::new();
        for absolute in paths {
            let relative = relative_string(root, absolute);
            if relative.is_empty() || relative == "." {
                continue;
            }
            if !absolute.exists() || !absolute.is_file() {
                let prefix = format!("{relative}/");
                let removed: Vec<String> = self
                    .files
                    .keys()
                    .filter(|path| *path == &relative || path.starts_with(&prefix))
                    .cloned()
                    .collect();
                for path in removed {
                    self.remove_entry(&path);
                    changed.push(path);
                }
                continue;
            }
            if exclusions.is_ignored(absolute, false) || excluded_path(absolute) {
                continue;
            }
            if let Some(entry) = read_entry(root, absolute, max_file_bytes, &HashMap::new())? {
                let is_changed = self
                    .files
                    .get(&entry.path)
                    .map(|old| old.hash != entry.hash)
                    .unwrap_or(true);
                if is_changed {
                    changed.push(entry.path.clone());
                }
                self.insert_entry(entry);
            }
        }
        Ok(changed)
    }

    pub fn snapshot_id(&mut self, head: &str) -> String {
        if !self.snapshot_dirty && self.cached_snapshot_head.as_deref() == Some(head) {
            if let Some(ref cached) = self.cached_snapshot {
                return cached.clone();
            }
        }
        let mut digest = Sha256::new();
        digest.update(INDEX_SCHEMA.as_bytes());
        digest.update([0]);
        digest.update(head.as_bytes());
        digest.update([0]);
        digest.update((self.files.len() as u64).to_le_bytes());
        let mut paths: Vec<_> = self.files.keys().collect();
        paths.sort();
        for path in paths {
            let file = &self.files[path];
            digest.update(path.as_bytes());
            digest.update([0]);
            digest.update(file.hash.as_bytes());
            digest.update([0]);
        }
        let result = format!("snap_{}", hex(&digest.finalize()));
        self.cached_snapshot_head = Some(head.to_owned());
        self.cached_snapshot = Some(result.clone());
        self.snapshot_dirty = false;
        result
    }

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
        } = params;
        let max_results = max_results.max(1);
        let path_filters = PathFilterSet::new(path_filters);
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
                let results = self.symbol_results(
                    workspace_id,
                    snapshot_id,
                    query,
                    &path_filters,
                    max_results,
                    false,
                );
                Ok(
                    json!({"mode": mode, "snapshot_id": snapshot_id, "result_count": results.len(), "results": results}),
                )
            }
            "references" => self.reference_results(
                workspace_id,
                snapshot_id,
                query,
                &path_filters,
                max_results,
                context_lines,
            ),
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
                    version: 1,
                    workspace_id: workspace_id.to_owned(),
                    snapshot_id: snapshot_id.to_owned(),
                    path: file.path.clone(),
                    start_line: start,
                    end_line: end,
                    content_hash: file.hash.clone(),
                    symbol: None,
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

    pub fn context(&self, params: ContextParams<'_>) -> AppResult<serde_json::Value> {
        let ContextParams {
            workspace_id,
            snapshot_id,
            query,
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
        } = params;
        let legacy_terms = query_terms(query);
        let required = normalized_terms(required_terms);
        let optional = if optional_terms.is_empty() {
            legacy_terms.clone()
        } else {
            normalized_terms(optional_terms)
        };
        let excluded = normalized_terms(exclude_terms);
        let mut candidate_terms = required.clone();
        candidate_terms.extend(optional.iter().cloned());
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
        let mut candidates: Vec<(f64, &FileEntry, usize, Vec<String>)> = Vec::new();
        for file in candidate_files {
            if !path_filters.allows(&file.path) {
                continue;
            }
            if !document_types.is_empty()
                && !document_types
                    .iter()
                    .any(|document_type| document_type == &file.document_type)
            {
                continue;
            }
            if low_signal_context_path(&file.path)
                && !path_filters.explicitly_requests(&file.path, &query_lower)
            {
                continue;
            }
            if excluded.iter().any(|term| file_matches_term(file, term)) {
                continue;
            }
            if !required.iter().all(|term| file_matches_term(file, term)) {
                continue;
            }
            let changed = dirty.contains(&file.path) || recent_mutations.contains(&file.path);
            let evidence_match = evidence.is_empty()
                || evidence_allowed(&file.document_type, evidence)
                || (evidence.iter().any(|item| item == "changes") && changed);
            if !evidence_match {
                continue;
            }
            let lower = &file.search_content;
            let path_lower = &file.path_lower;
            let mut score = 0.0;
            let mut first = None;
            let mut reasons = Vec::new();
            let mut matched_optional_terms = 0usize;
            if !query_lower.is_empty() && lower.contains(&query_lower) {
                score += 12.0;
                reasons.push("exact_phrase".to_owned());
                first = lower.find(&query_lower);
            }
            for term in &required {
                if file_matches_term(file, term) {
                    score += 15.0;
                    reasons.push("required_term".to_owned());
                    first = first.or_else(|| lower.find(term).or_else(|| path_lower.find(term)));
                }
            }
            for term in &optional {
                let count = lower.match_indices(term).take(50).count();
                if count > 0 {
                    matched_optional_terms += 1;
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
                    first =
                        first.or_else(|| Some(line_start_byte(&file.content, symbol.start_line)));
                }
            }
            if matched_optional_terms > 0 {
                let coverage = matched_optional_terms as f64 / optional.len().max(1) as f64;
                score += coverage * 10.0;
                if matched_optional_terms == optional.len() {
                    reasons.push("full_term_coverage".to_owned());
                }
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
                continue;
            }
            let size_units = file.content.len().max(100) as f64 / 8_192.0;
            score /= 1.0 + size_units.ln_1p().min(4.0) * 0.18;
            if score < min_score {
                continue;
            }
            candidates.push((score, file, first.unwrap_or(0), reasons));
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
            let (excerpt, end_line) =
                fit_excerpt(&file.content, start_line, proposed_end, remaining);
            if excerpt.is_empty() {
                continue;
            }
            used += excerpt.len();
            reasons = compact_reason_codes(reasons);
            let symbol = file
                .symbols
                .iter()
                .find(|symbol| symbol.start_line <= match_line && symbol.end_line >= match_line)
                .map(|symbol| symbol.name.clone());
            let handle = encode_handle(&RangeHandle {
                version: 1,
                workspace_id: workspace_id.to_owned(),
                snapshot_id: snapshot_id.to_owned(),
                path: file.path.clone(),
                start_line,
                end_line,
                content_hash: file.hash.clone(),
                symbol,
            })?;
            let group = context_group(&file.document_type, &reasons);
            *groups.entry(group.clone()).or_default() += 1;
            results.push(SearchMatch {
                path: file.path.clone(),
                start_line,
                end_line,
                preview: excerpt,
                document_type: file.document_type.clone(),
                score,
                group,
                reason_codes: reasons,
                handle,
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

    fn reference_results(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
        query: &str,
        path_filters: &PathFilterSet<'_>,
        max_results: usize,
        context_lines: usize,
    ) -> AppResult<serde_json::Value> {
        let symbol_name = query.trim();
        if symbol_name.is_empty() {
            return Err(AppError::invalid("query is required for reference search"));
        }
        let mut definitions = Vec::new();
        let mut declaration_lines: HashMap<String, HashSet<usize>> = HashMap::new();
        for (file, symbol) in self.indexed_symbols(symbol_name) {
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
        definitions.sort_by(|a, b| {
            a.get("path")
                .and_then(serde_json::Value::as_str)
                .cmp(&b.get("path").and_then(serde_json::Value::as_str))
        });
        let identifier = Regex::new(&format!(r"\b{}\b", regex::escape(symbol_name)))
            .map_err(AppError::internal)?;
        let terms = query_terms(symbol_name);
        let mut paths = HashSet::new();
        for term in &terms {
            if let Some(matches) = self.token_index.get(term) {
                paths.extend(matches.iter().cloned());
            }
        }
        let mut files: Vec<_> = if paths.is_empty() {
            self.files.values().collect()
        } else {
            paths
                .iter()
                .filter_map(|path| self.files.get(path))
                .collect()
        };
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let per_file_limit = max_results.clamp(1, 3);
        let mut groups: Vec<VecDeque<serde_json::Value>> = Vec::new();
        let mut total_windows = 0usize;
        for file in files {
            if !path_filters.allows(&file.path) {
                continue;
            }
            let mut windows: Vec<(usize, usize, usize)> = Vec::new();
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
                let start = index.saturating_sub(context_lines) + 1;
                let end = (index + context_lines + 1).min(file.line_count);
                if let Some((_, _, previous_end)) = windows.last_mut() {
                    if start <= previous_end.saturating_add(1) && lines_in_current_window < 3 {
                        *previous_end = (*previous_end).max(end);
                        lines_in_current_window += 1;
                        continue;
                    }
                }
                windows.push((line_number, start, end));
                lines_in_current_window = 1;
            }
            total_windows += windows.len();
            let mut file_results = VecDeque::new();
            for (line_number, start, end) in windows.into_iter().take(per_file_limit) {
                let handle = encode_handle(&RangeHandle {
                    version: 1,
                    workspace_id: workspace_id.to_owned(),
                    snapshot_id: snapshot_id.to_owned(),
                    path: file.path.clone(),
                    start_line: start,
                    end_line: end,
                    content_hash: file.hash.clone(),
                    symbol: Some(symbol_name.to_owned()),
                })?;
                file_results.push_back(json!({
                    "path": file.path,
                    "line": line_number,
                    "start_line": start,
                    "end_line": end,
                    "preview": slice_lines(&file.content, start, end),
                    "handle": handle,
                    "match_type": "reference",
                    "score": 1.0,
                    "reason_codes": ["reference_match"]
                }));
            }
            if !file_results.is_empty() {
                groups.push(file_results);
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
            "mode": "references",
            "symbol": symbol_name,
            "snapshot_id": snapshot_id,
            "definition_count": definitions.len(),
            "result_count": results.len(),
            "truncated": total_windows > results.len(),
            "definitions": definitions,
            "results": results,
        }))
    }

    fn symbol_results(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
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
                    version: 1,
                    workspace_id: workspace_id.to_owned(),
                    snapshot_id: snapshot_id.to_owned(),
                    path: file.path.clone(),
                    start_line: symbol.start_line,
                    end_line: symbol.end_line,
                    content_hash: file.hash.clone(),
                    symbol: Some(symbol.name.clone()),
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

fn normalized_terms(values: &[String]) -> Vec<String> {
    let mut terms: Vec<_> = values.iter().flat_map(|value| query_terms(value)).collect();
    terms.sort();
    terms.dedup();
    terms
}

fn file_matches_term(file: &FileEntry, term: &str) -> bool {
    file.search_content.contains(term)
        || file.path_lower.contains(term)
        || file
            .symbols
            .iter()
            .any(|symbol| symbol.name.to_ascii_lowercase().contains(term))
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

fn scan_directory(
    index: &mut CodeIndex,
    root: &Path,
    scan_root: &Path,
    max_file_bytes: usize,
    respect_ignores: bool,
    exclusions: &WorkspaceExclusions,
    cached_files: &HashMap<String, FileEntry>,
) -> AppResult<()> {
    let mut builder = WalkBuilder::new(scan_root);
    builder
        .hidden(false)
        .git_ignore(respect_ignores)
        .git_exclude(respect_ignores)
        .ignore(respect_ignores)
        .follow_links(false);
    let scan_exclusions = exclusions.clone();
    builder.filter_entry(move |entry| {
        !excluded_dir(entry.path())
            && !scan_exclusions.is_ignored(
                entry.path(),
                entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false),
            )
    });
    let paths: Vec<PathBuf> = builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| {
            let path = entry.path();
            entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
                && !excluded_path(path)
                && !exclusions.is_ignored(path, false)
        })
        .map(|entry| entry.into_path())
        .collect();

    let parsed: AppResult<Vec<FileEntry>> = paths
        .par_iter()
        .map(|path| read_entry(root, path, max_file_bytes, cached_files))
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<AppResult<Vec<_>>>()
        .map(|entries| entries.into_iter().flatten().collect());

    for entry in parsed? {
        index.insert_entry(entry);
    }
    Ok(())
}

fn read_entry(
    root: &Path,
    path: &Path,
    max_file_bytes: usize,
    cached_files: &HashMap<String, FileEntry>,
) -> AppResult<Option<FileEntry>> {
    let metadata = match fs::metadata(path) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if metadata.len() as usize > max_file_bytes {
        return Ok(None);
    }
    let relative = relative_string(root, path);
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let bytes = match fs::read(path) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if bytes.iter().take(8_192).any(|byte| *byte == 0) {
        return Ok(None);
    }
    let content = match String::from_utf8(bytes) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let path_lower = relative.to_ascii_lowercase();
    let language = language_name(path).to_owned();
    let document_type = classify_document(&relative);
    let hash = content_hash(&content);
    if let Some(cached) = cached_files.get(&relative) {
        if cached.hash == hash {
            let mut cached = cached.clone();
            cached.size = metadata.len();
            cached.modified_ns = modified_ns;
            return Ok(Some(cached));
        }
    }
    let symbols = extract_symbols(path, &content);
    let search_content = content.to_ascii_lowercase();
    let indexed_terms = build_indexed_terms(&search_content, &path_lower, &symbols);
    let line_count = content.lines().count().max(1);
    let line_starts = line_starts(&content);
    Ok(Some(FileEntry {
        path: relative,
        path_lower,
        content,
        search_content,
        line_count,
        line_starts,
        indexed_terms,
        hash,
        language,
        document_type,
        symbols,
        size: metadata.len(),
        modified_ns,
    }))
}

fn excluded_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(ignored_path_component)
        .unwrap_or(false)
}
fn excluded_path(path: &Path) -> bool {
    if path
        .file_name()
        .and_then(|value| value.to_str())
        .map(|name| name.contains(".codeweave-"))
        .unwrap_or(false)
    {
        return true;
    }
    path.components().any(|part| {
        part.as_os_str()
            .to_str()
            .map(ignored_path_component)
            .unwrap_or(false)
    })
}
pub fn ignored_workspace_path(path: &str) -> bool {
    path.replace('\\', "/")
        .split('/')
        .any(ignored_path_component)
}
fn ignored_path_component(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | ".venv"
            | "venv"
            | ".codeweave-cache"
    ) || name.starts_with("target-")
        || name.starts_with("target_")
}
#[cfg(test)]
mod tests;
