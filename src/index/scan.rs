//! Building and refreshing the index from the filesystem.

use super::cache::{self, CacheKey, SymbolCache};
use super::entry::FileEntry;
use super::exclusions::{excluded_dir, excluded_path, WorkspaceExclusions};
use super::handle::content_hash;
use super::store::CodeIndex;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::security::{RelPath, WorkspaceRoot};
use crate::symbols::extract_symbols;
use ignore::WalkBuilder;
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

impl CodeIndex {
    pub fn scan(
        root: &WorkspaceRoot,
        max_file_bytes: usize,
        artifact_paths: &[String],
        exclusions: &WorkspaceExclusions,
    ) -> AppResult<Self> {
        scan_workspace(
            root,
            max_file_bytes,
            artifact_paths,
            exclusions,
            &SymbolCache::default(),
        )
    }

    /// Scan using the persistent cache at `cache_file`, then rewrite it.
    /// Returns the index and whether a matching cache was found.
    pub fn scan_cached(
        root: &WorkspaceRoot,
        max_file_bytes: usize,
        artifact_paths: &[String],
        exclusions: &WorkspaceExclusions,
        cache_file: &Path,
    ) -> AppResult<(Self, bool)> {
        let key = CacheKey {
            root: root.path().to_string_lossy().into_owned(),
            max_file_bytes,
            artifact_paths,
            exclude_paths: exclusions.patterns(),
        };
        let cached = cache::load(cache_file, &key);
        let cache_hit = cached.is_some();
        let index = scan_workspace(
            root,
            max_file_bytes,
            artifact_paths,
            exclusions,
            &cached.unwrap_or_default(),
        )?;
        cache::store(cache_file, &key, &index)?;
        Ok((index, cache_hit))
    }

    /// Re-read the given absolute paths. Missing paths remove the file or the
    /// directory tree they named. Returns the paths whose indexed content changed.
    pub fn refresh_paths(
        &mut self,
        root: &WorkspaceRoot,
        paths: &HashSet<PathBuf>,
        max_file_bytes: usize,
        exclusions: &WorkspaceExclusions,
    ) -> AppResult<Vec<String>> {
        let mut changed = Vec::new();
        for absolute in paths {
            let relative = root.relative(absolute);
            if relative.is_empty() || relative == "." {
                continue;
            }
            let Ok(file) = RelPath::new(&relative) else {
                continue;
            };
            if !root.is_regular_file(&file) {
                changed.extend(self.remove_tree(&relative));
                continue;
            }
            if exclusions.is_ignored(absolute, false) || excluded_path(absolute) {
                continue;
            }
            if let Some(entry) =
                read_entry(root, absolute, max_file_bytes, &SymbolCache::default())?
            {
                let is_changed = self
                    .get(entry.path())
                    .is_none_or(|old| old.hash() != entry.hash());
                if is_changed {
                    changed.push(entry.path().to_owned());
                }
                self.insert(entry);
            }
        }
        Ok(changed)
    }
}

fn scan_workspace(
    root: &WorkspaceRoot,
    max_file_bytes: usize,
    artifact_paths: &[String],
    exclusions: &WorkspaceExclusions,
    cache: &SymbolCache,
) -> AppResult<CodeIndex> {
    let mut index = CodeIndex::default();
    scan_directory(
        &mut index,
        root,
        root.path(),
        max_file_bytes,
        true,
        exclusions,
        cache,
    )?;
    // `WorkspaceRoot::open` canonicalized the root.
    let canonical_root = root.path();
    for relative in artifact_paths {
        let relative = RelPath::new(relative)?;
        let candidate = canonical_root.join(relative.as_path());
        if !candidate.exists() {
            continue;
        }
        let resolved = candidate.canonicalize()?;
        if !resolved.starts_with(canonical_root) {
            return Err(AppError::new(
                ErrorCode::OutsideRoot,
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
            cache,
        )?;
    }
    Ok(index)
}

fn scan_directory(
    index: &mut CodeIndex,
    root: &WorkspaceRoot,
    scan_root: &Path,
    max_file_bytes: usize,
    respect_ignores: bool,
    exclusions: &WorkspaceExclusions,
    cache: &SymbolCache,
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

    let entries = paths
        .par_iter()
        .map(|path| read_entry(root, path, max_file_bytes, cache))
        .collect::<AppResult<Vec<_>>>()?;
    for entry in entries.into_iter().flatten() {
        index.insert(entry);
    }
    Ok(())
}

/// Read one file confined to `root`. Binary, non-UTF-8, oversized, and
/// non-regular files are skipped. Cached symbols are reused only when the
/// content hash matches.
fn read_entry(
    root: &WorkspaceRoot,
    path: &Path,
    max_file_bytes: usize,
    cache: &SymbolCache,
) -> AppResult<Option<FileEntry>> {
    let Ok(relative) = RelPath::new(&root.relative(path)) else {
        return Ok(None);
    };
    let Some(file) = root.read_file(&relative, max_file_bytes)? else {
        return Ok(None);
    };
    let relative = relative.into_string();
    if file.bytes.iter().take(8_192).any(|byte| *byte == 0) {
        return Ok(None);
    }
    let Ok(content) = String::from_utf8(file.bytes) else {
        return Ok(None);
    };
    let hash = content_hash(&content);
    let symbols = cache
        .symbols_for(&relative, &hash)
        .unwrap_or_else(|| extract_symbols(path, &content));
    Ok(Some(FileEntry::with_symbols(
        relative,
        content,
        hash,
        symbols,
        file.size,
        file.modified_ns,
    )))
}
