use super::lines::{hex, line_starts};
use super::metadata::{build_indexed_terms, classify_document, normalize_entry};
use super::path_filter::normalize;
use super::{
    content_hash, qualified_symbol_parts, symbol_matches_qualified_name, CodeIndex, FileEntry,
    IndexMetrics,
};
use crate::model::{AppError, AppResult};
use crate::security::{relative_string, validate_relative};
use crate::symbols::{extract_symbols, language_name, Symbol};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const INDEX_SCHEMA: &str = "codeweave-index";

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

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct CachedIndex {
    schema: String,
    root: String,
    max_file_bytes: usize,
    artifact_paths: Vec<String>,
    #[serde(default)]
    exclude_paths: Vec<String>,
    pub(super) files: Vec<FileEntry>,
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
        let canonical_root = root.canonicalize()?;
        for relative in artifact_paths {
            let relative = validate_relative(relative)?;
            let candidate = canonical_root.join(relative);
            if !candidate.exists() {
                continue;
            }
            let resolved = candidate.canonicalize()?;
            if !resolved.starts_with(&canonical_root) {
                return Err(AppError::new(
                    "OUTSIDE_ROOT",
                    "Artifact path resolves outside workspace",
                ));
            }
            scan_directory(
                &mut index,
                &canonical_root,
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

    pub(super) fn insert_entry(&mut self, mut entry: FileEntry) {
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

    pub(super) fn candidate_files<'a>(&'a self, terms: &[String]) -> Vec<&'a FileEntry> {
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

    pub fn metrics(&self) -> IndexMetrics {
        let indexed_loc = self.files.values().map(|file| file.line_count).sum();
        let indexed_source_loc = self
            .files
            .values()
            .filter(|file| file.document_type == "source")
            .map(|file| file.line_count)
            .sum();
        let indexed_content_bytes = self.files.values().map(|file| file.content.len()).sum();
        let token_posting_count = self.token_index.values().map(HashSet::len).sum();
        let symbol_declaration_count = self.symbol_index.values().map(BTreeSet::len).sum();

        let mut estimated_heap_bytes_lower_bound = 0usize;
        for (key, file) in &self.files {
            estimated_heap_bytes_lower_bound += key.capacity();
            estimated_heap_bytes_lower_bound += file.path.capacity()
                + file.path_lower.capacity()
                + file.content.capacity()
                + file.search_content.capacity()
                + file.hash.capacity()
                + file.language.capacity()
                + file.document_type.capacity();
            estimated_heap_bytes_lower_bound +=
                file.line_starts.capacity() * std::mem::size_of::<usize>();
            estimated_heap_bytes_lower_bound +=
                file.indexed_terms.capacity() * std::mem::size_of::<String>();
            estimated_heap_bytes_lower_bound += file
                .indexed_terms
                .iter()
                .map(String::capacity)
                .sum::<usize>();
            estimated_heap_bytes_lower_bound +=
                file.symbols.capacity() * std::mem::size_of::<Symbol>();
            estimated_heap_bytes_lower_bound += file
                .symbols
                .iter()
                .map(|symbol| {
                    symbol.name.capacity() + symbol.kind.capacity() + symbol.signature.capacity()
                })
                .sum::<usize>();
        }
        for (term, paths) in &self.token_index {
            estimated_heap_bytes_lower_bound += term.capacity();
            estimated_heap_bytes_lower_bound += paths.iter().map(String::capacity).sum::<usize>();
        }
        for (name, declarations) in &self.symbol_index {
            estimated_heap_bytes_lower_bound += name.capacity();
            estimated_heap_bytes_lower_bound += declarations
                .iter()
                .map(|(path, _)| path.capacity() + std::mem::size_of::<usize>())
                .sum::<usize>();
        }
        estimated_heap_bytes_lower_bound += self
            .cached_snapshot_head
            .as_ref()
            .map_or(0, String::capacity);
        estimated_heap_bytes_lower_bound +=
            self.cached_snapshot.as_ref().map_or(0, String::capacity);

        IndexMetrics {
            indexed_file_count: self.files.len(),
            indexed_loc,
            indexed_source_loc,
            indexed_content_bytes,
            estimated_heap_bytes_lower_bound,
            token_count: self.token_index.len(),
            token_posting_count,
            symbol_name_count: self.symbol_index.len(),
            symbol_declaration_count,
        }
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

    pub(crate) fn files(&self) -> impl Iterator<Item = &FileEntry> {
        self.files.values()
    }

    #[cfg(test)]
    pub fn find_symbol(&self, path: Option<&str>, name: &str) -> Option<(String, Symbol, String)> {
        self.find_symbols(path, name).into_iter().next()
    }

    /// Return every declaration matching `name`, optionally constrained to one
    /// workspace-relative path. Besides leaf names, this accepts qualified method
    /// names such as `BrowserAttemptRunner.run` and `BrowserAttemptRunner::run`.
    pub fn find_symbols(&self, path: Option<&str>, name: &str) -> Vec<(String, Symbol, String)> {
        let leaf_name = qualified_symbol_parts(name)
            .map(|(_, leaf)| leaf)
            .unwrap_or(name);
        if let Some(path) = path {
            let Some(file) = self.files.get(normalize(path).as_ref()) else {
                return Vec::new();
            };
            return file
                .symbols
                .iter()
                .filter(|symbol| symbol.name == leaf_name)
                .filter(|symbol| symbol_matches_qualified_name(file, symbol, name))
                .cloned()
                .map(|symbol| (file.path.clone(), symbol, file.hash.clone()))
                .collect();
        }
        let mut matches = Vec::new();
        for (path, symbol_index) in self
            .symbol_index
            .get(leaf_name)
            .into_iter()
            .flat_map(|entries| entries.iter())
        {
            let Some(file) = self.files.get(path) else {
                continue;
            };
            let Some(symbol) = file.symbols.get(*symbol_index) else {
                continue;
            };
            if symbol_matches_qualified_name(file, symbol, name) {
                matches.push((file.path.clone(), symbol.clone(), file.hash.clone()));
            }
        }
        matches
    }

    pub(super) fn indexed_symbols<'a>(
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

pub(super) fn read_entry(
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
