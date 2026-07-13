mod chunks;
mod handle;
mod lines;
mod metadata;
mod path_filter;

pub use handle::{content_hash, decode_handle, encode_handle, RangeHandle};
pub use lines::slice_lines;

/// The deterministic tokenization used by retrieval, exposed for small
/// workspace-level intent checks such as explicit worktree prioritization.
pub fn query_terms_for_intent(query: &str) -> Vec<String> {
    metadata::query_terms(query)
}

use crate::model::{AppError, AppResult};
use crate::security::{relative_string, validate_relative};
use crate::symbols::{extract_symbols, language_name, Symbol};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;
use lines::{
    byte_to_line, excerpt_lines_with_count, fit_excerpt, hex, line_start_byte, line_starts,
};
use metadata::{
    build_indexed_terms, classify_document, classify_lifecycle, compact_reason_codes,
    evidence_allowed, low_signal_context_path, normalize_entry, query_terms,
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

const INDEX_SCHEMA: &str = "codeweave-index-v8";

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
        if let Some(relative) = self.relative_from_normalized(&normalized) {
            return Some(relative);
        }
        let canonical = path.canonicalize().ok()?;
        let normalized = normalized_absolute_path(&canonical);
        self.relative_from_normalized(&normalized)
    }

    fn relative_from_normalized(&self, normalized: &str) -> Option<String> {
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

/// Controls how `code_context` renders declaration bodies after ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SymbolDetail {
    Excerpt,
    Complete,
    #[default]
    Auto,
    None,
}

impl SymbolDetail {
    pub fn parse(value: Option<&str>) -> AppResult<Self> {
        match value.unwrap_or("auto") {
            "excerpt" => Ok(Self::Excerpt),
            "complete" => Ok(Self::Complete),
            "auto" => Ok(Self::Auto),
            "none" => Ok(Self::None),
            other => Err(AppError::details(
                "INVALID_SYMBOL_DETAIL",
                "symbol_detail must be excerpt, complete, auto, or none",
                json!({"symbol_detail": other}),
            )),
        }
    }
}

/// Retrieval ranking algorithm selector (config `index.ranking`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ranking {
    #[default]
    V1,
    V2,
}

impl Ranking {
    /// Parse a config value; unknown strings fall back to `V1`.
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "v2" => Ranking::V2,
            _ => Ranking::V1,
        }
    }
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

fn default_lifecycle() -> String {
    "current".to_owned()
}

fn chunk_kind_label(kind: chunks::ChunkKind) -> &'static str {
    match kind {
        chunks::ChunkKind::Symbol => "symbol",
        chunks::ChunkKind::SymbolPart => "symbol_part",
        chunks::ChunkKind::Remainder => "remainder",
    }
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
    /// Order-independent running digest of the file set. Each file contributes
    /// `sha256(path ‖ 0 ‖ hash ‖ 0)` XORed into this accumulator, so inserts and
    /// removals update it in O(1) instead of re-hashing the whole index on every
    /// mutation. Combined with `head` and the file count at read time.
    snapshot_acc: [u8; 32],
}

fn file_snapshot_digest(path: &str, hash: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(path.as_bytes());
    digest.update([0]);
    digest.update(hash.as_bytes());
    digest.update([0]);
    digest.finalize().into()
}

fn xor_accumulator(acc: &mut [u8; 32], contribution: &[u8; 32]) {
    for (slot, byte) in acc.iter_mut().zip(contribution.iter()) {
        *slot ^= *byte;
    }
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
            xor_accumulator(
                &mut self.snapshot_acc,
                &file_snapshot_digest(&previous.path, &previous.hash),
            );
            self.remove_from_token_index(&previous);
            self.remove_from_symbol_index(&previous);
        }
        xor_accumulator(
            &mut self.snapshot_acc,
            &file_snapshot_digest(&entry.path, &entry.hash),
        );
        self.add_to_token_index(&entry);
        self.add_to_symbol_index(&entry);
        self.files.insert(entry.path.clone(), entry);
        self.snapshot_dirty = true;
    }

    fn remove_entry(&mut self, path: &str) -> Option<FileEntry> {
        let removed = self.files.remove(path)?;
        xor_accumulator(
            &mut self.snapshot_acc,
            &file_snapshot_digest(&removed.path, &removed.hash),
        );
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
        paths
            .iter()
            .filter_map(|path| self.files.get(path))
            .collect()
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
    #[cfg(test)]
    pub fn find_symbol(&self, path: Option<&str>, name: &str) -> Option<(String, Symbol, String)> {
        self.find_symbols(path, name).into_iter().next()
    }

    /// Return every exact declaration matching `name`, optionally constrained to
    /// one workspace-relative path. This keeps fetch callers from depending on
    /// the incidental ordering of the symbol index.
    pub fn find_symbols(&self, path: Option<&str>, name: &str) -> Vec<(String, Symbol, String)> {
        if let Some(path) = path {
            let Some(file) = self.files.get(normalize(path).as_ref()) else {
                return Vec::new();
            };
            return file
                .symbols
                .iter()
                .filter(|symbol| symbol.name == name)
                .cloned()
                .map(|symbol| (file.path.clone(), symbol, file.hash.clone()))
                .collect();
        }
        let mut matches = Vec::new();
        for (path, symbol_index) in self
            .symbol_index
            .get(name)
            .into_iter()
            .flat_map(|entries| entries.iter())
        {
            let Some(file) = self.files.get(path) else {
                continue;
            };
            let Some(symbol) = file.symbols.get(*symbol_index) else {
                continue;
            };
            matches.push((file.path.clone(), symbol.clone(), file.hash.clone()));
        }
        matches
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
        digest.update(self.snapshot_acc);
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
            reference_scope,
            reference_kinds,
            definition_path,
            definition_line,
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
                let results =
                    self.symbol_results(workspace_id, query, &path_filters, max_results, false);
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
                reference_scope,
                reference_kinds,
                definition_path,
                definition_line,
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
            if matches!(symbol_detail, SymbolDetail::Excerpt | SymbolDetail::Auto)
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
                symbol: symbol.map(|symbol| symbol.name),
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

    /// Chunk-granular BM25F ranking (`index.ranking: "v2"`).
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
            // Render the chunk enclosing the match, so the excerpt is a whole
            // symbol rather than a fixed line window.
            let match_line = byte_to_line(&hit.file.line_starts, hit.first);
            let chunk = hit
                .file
                .chunks()
                .iter()
                .find(|chunk| chunk.start_line <= match_line && chunk.end_line >= match_line)
                .or_else(|| hit.file.chunks().first());
            let Some(chunk) = chunk else {
                continue;
            };

            // Render the whole chunk when it fits the line cap (so small symbols
            // come back complete); for a larger symbol fall back to a window
            // centered on the match, so the excerpt stays relevant and the budget
            // is not blown by one big definition.
            let chunk_span = chunk.end_line.saturating_sub(chunk.start_line) + 1;
            let (render_start, render_end) = if chunk_span <= MAX_RENDER_LINES {
                (chunk.start_line.max(1), chunk.end_line.max(1))
            } else {
                excerpt_lines_with_count(match_line, hit.file.line_count, MAX_RENDER_LINES / 2)
            };
            let remaining = budget_chars.saturating_sub(used);
            if remaining == 0 {
                break;
            }
            let reasons = hit.reasons.clone();
            let reasons = compact_reason_codes(reasons);
            let exact_symbol = reasons.iter().any(|reason| reason == "exact_symbol");
            let symbol_range = chunk
                .symbol
                .as_ref()
                .map(|_| (chunk.start_line, chunk.end_line));
            let (preview, start_line, end_line, complete_symbol) = render_context_preview(
                hit.file,
                render_start,
                render_end,
                symbol_range,
                symbol_detail,
                exact_symbol,
                remaining,
            );
            if matches!(symbol_detail, SymbolDetail::Excerpt | SymbolDetail::Auto)
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
                symbol: chunk.symbol.clone(),
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

    #[allow(clippy::too_many_arguments)]
    fn reference_results(
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
                    symbol: Some(symbol_name.to_owned()),
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
                let kind = classify_reference_kind(file, line, symbol_name);
                if !reference_kinds.is_empty()
                    && !reference_kinds.iter().any(|wanted| wanted == kind)
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
                let line = file
                    .content
                    .lines()
                    .nth(line_number.saturating_sub(1))
                    .unwrap_or_default();
                let reference_kind = classify_reference_kind(file, line, symbol_name);
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
                    "evidence": "lexical",
                    "classification_evidence": classification_evidence,
                    "reference_kind": reference_kind,
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
            "definitions": definitions,
            "results": results,
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
                    version: 1,
                    workspace_id: workspace_id.to_owned(),
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

/// Render a ranked result without changing its range/handle contract. `complete`
/// intentionally omits a partial body when the declaration cannot fit; callers
/// can follow the handle with `code_fetch`.
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
    if implementation_intent(query_lower) {
        match file.lifecycle.as_str() {
            "historical_plan" => {
                score -= 45.0;
                reasons.push("historical_plan".to_owned());
            }
            "active_plan" => score -= 8.0,
            _ => {}
        }
        if file.document_type == "source" && !file.path.starts_with("docs/") {
            score += 6.0;
            reasons.push("implementation_authority".to_owned());
        }
        if canonical_configuration_path(&file.path) {
            score += 8.0;
            reasons.push("canonical_config".to_owned());
        }
    }
    if score <= 0.0 {
        return None;
    }
    let size_units = file.content.len().max(100) as f64 / 8_192.0;
    score /= 1.0 + size_units.ln_1p().min(4.0) * 0.18;

    Some(BaseFileScore {
        score,
        first: first.unwrap_or(0),
        reasons,
    })
}

fn implementation_intent(query_lower: &str) -> bool {
    if ["documentation", "history", "historical", "plan", "plans"]
        .iter()
        .any(|word| query_lower.contains(word))
    {
        return false;
    }
    [
        "implement",
        "implementation",
        "owner",
        "ownership",
        "resolve",
        "resolution",
        "runtime",
        "error",
        "where",
        "persist",
        "fallback",
    ]
    .iter()
    .any(|word| query_lower.contains(word))
}

fn canonical_configuration_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    (lower.contains("/config/")
        || lower.ends_with(".toml")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml"))
        && !lower.ends_with(".lock")
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
    let lifecycle = classify_lifecycle(&relative, &content);
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
    let file_chunks = chunks::build_chunks(&content, &symbols);
    let path_tf = chunks::path_field(&path_lower);
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
        lifecycle,
        symbols,
        size: metadata.len(),
        modified_ns,
        chunks: file_chunks,
        path_tf,
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
