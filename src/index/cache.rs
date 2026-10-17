//! Persistent index cache. The cache stores only what is expensive to
//! recompute, the parsed symbols per content hash; file content is always read
//! from disk during a scan, so a stale cache can never serve stale text.

use super::store::CodeIndex;
use crate::error::{AppError, AppResult};
use crate::symbols::Symbol;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};

const CACHE_SCHEMA: &str = "codeweave-index-cache-v2";

/// Scan settings a cache was built with; a cache is reused only on an exact match.
pub(super) struct CacheKey<'a> {
    pub(super) root: String,
    pub(super) max_file_bytes: usize,
    pub(super) artifact_paths: &'a [String],
    pub(super) exclude_paths: &'a [String],
}

/// Previously parsed symbols, keyed by path and valid only for the same hash.
#[derive(Debug, Default)]
pub(super) struct SymbolCache {
    files: HashMap<String, CachedFile>,
}

impl SymbolCache {
    pub(super) fn symbols_for(&self, path: &str, hash: &str) -> Option<Vec<Symbol>> {
        self.files
            .get(path)
            .filter(|cached| cached.hash == hash)
            .map(|cached| cached.symbols.clone())
    }
}

#[derive(Deserialize)]
struct CacheDocument {
    schema: String,
    root: String,
    max_file_bytes: usize,
    artifact_paths: Vec<String>,
    exclude_paths: Vec<String>,
    files: Vec<CachedFile>,
}

#[derive(Debug, Deserialize)]
struct CachedFile {
    path: String,
    hash: String,
    symbols: Vec<Symbol>,
}

#[derive(Serialize)]
struct CacheDocumentRef<'a> {
    schema: &'static str,
    root: &'a str,
    max_file_bytes: usize,
    artifact_paths: &'a [String],
    exclude_paths: &'a [String],
    files: Vec<CachedFileRef<'a>>,
}

#[derive(Serialize)]
struct CachedFileRef<'a> {
    path: &'a str,
    hash: &'a str,
    symbols: &'a [Symbol],
}

/// Load the cache at `path`. A missing, unreadable, corrupt, or mismatched
/// cache is a miss; the scan then parses every file and rewrites the cache.
pub(super) fn load(path: &Path, key: &CacheKey<'_>) -> Option<SymbolCache> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "index cache is unreadable; rebuilding");
            return None;
        }
    };
    let document: CacheDocument = match serde_json::from_slice(&bytes) {
        Ok(document) => document,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "index cache is corrupt; rebuilding");
            return None;
        }
    };
    let matches = document.schema == CACHE_SCHEMA
        && document.root == key.root
        && document.max_file_bytes == key.max_file_bytes
        && document.artifact_paths == key.artifact_paths
        && document.exclude_paths == key.exclude_paths;
    if !matches {
        tracing::debug!(path = %path.display(), "index cache settings changed; rebuilding");
        return None;
    }
    Some(SymbolCache {
        files: document
            .files
            .into_iter()
            .map(|file| (file.path.clone(), file))
            .collect(),
    })
}

/// Write the cache for `index` atomically: a temporary sibling file is fully
/// written and synced, then renamed over the previous cache.
pub(super) fn store(path: &Path, key: &CacheKey<'_>, index: &CodeIndex) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let document = CacheDocumentRef {
        schema: CACHE_SCHEMA,
        root: &key.root,
        max_file_bytes: key.max_file_bytes,
        artifact_paths: key.artifact_paths,
        exclude_paths: key.exclude_paths,
        files: index
            .files()
            .map(|file| CachedFileRef {
                path: file.path(),
                hash: file.hash(),
                symbols: file.symbols(),
            })
            .collect(),
    };
    let temp = temp_path(path);
    let written = write_synced(&temp, &document)
        .and_then(|()| fs::rename(&temp, path).map_err(AppError::from));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

fn write_synced(path: &Path, document: &CacheDocumentRef<'_>) -> AppResult<()> {
    let mut writer = BufWriter::new(fs::File::create(path)?);
    serde_json::to_writer(&mut writer, document)?;
    writer.flush()?;
    let file = writer
        .into_inner()
        .map_err(|error| AppError::from(error.into_error()))?;
    file.sync_all()?;
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}
