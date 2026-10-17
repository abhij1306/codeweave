//! The change ledger: what changed on disk since the last reconcile, which of
//! those changes CodeWeave wrote itself, and the bounded log of mutation records.

use crate::index::WorkspaceExclusions;
use crate::security::relative_string;
use crate::workspace::events::MutationRecord;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long an internal-write marker survives. Watcher delivery can be delayed on
/// busy or network-backed filesystems; the marker must outlive that delay so our
/// own atomic writes are not misclassified as external changes.
pub(crate) const INTERNAL_WRITE_TTL: Duration = Duration::from_secs(30);

/// Maximum mutation records retained in memory.
const MAX_MUTATION_EVENTS: usize = 2_000;

/// Marker embedded in every temporary file name CodeWeave writes.
const TEMP_FILE_MARKER: &str = ".codeweave-";

/// Name of the temporary sibling used to atomically replace `name`.
pub(crate) fn temp_file_name(name: &str, unique: &str) -> String {
    format!(".{name}{TEMP_FILE_MARKER}{unique}.tmp")
}

/// Whether `path` names one of CodeWeave's own temporary files.
fn is_temp_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.contains(TEMP_FILE_MARKER))
}

struct PendingInner {
    root: PathBuf,
    exclusions: WorkspaceExclusions,
    paths: Mutex<HashSet<PathBuf>>,
    needs_reconcile: AtomicBool,
}

/// Cheap cloneable handle the file watcher uses to queue changed paths.
#[derive(Clone)]
pub(crate) struct PendingHandle {
    inner: Arc<PendingInner>,
}

impl PendingHandle {
    /// Queue watcher-reported paths that belong to the workspace and are neither
    /// excluded nor CodeWeave temporary files.
    pub(crate) fn observe(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let inner = &self.inner;
        let mut pending = inner.paths.lock();
        for path in paths {
            if !path.starts_with(&inner.root) {
                continue;
            }
            let relative = relative_string(&inner.root, &path);
            if relative.is_empty()
                || inner.exclusions.is_ignored(&path, path.is_dir())
                || is_temp_file(&path)
            {
                continue;
            }
            pending.insert(path);
            inner.needs_reconcile.store(true, Ordering::Release);
        }
    }
}

/// Pending paths drained for one reconcile, already classified.
#[derive(Debug, Default)]
pub(crate) struct PendingDrain {
    /// Something under `.git/` changed, so the repository status must be refreshed.
    pub(crate) git_event: bool,
    /// Absolute paths whose index entries must be refreshed.
    pub(crate) paths: HashSet<PathBuf>,
    /// Relative paths of those that CodeWeave did not write itself.
    pub(crate) external: HashSet<String>,
}

/// The window a Bash run's changed-path attribution covers.
pub(crate) struct RunWindow<'a> {
    pub(crate) start_generation: u64,
    pub(crate) end_generation: u64,
    /// Set once the run is terminal: changes recorded after it are excluded unless
    /// the file itself was last modified by then.
    pub(crate) ended_at: Option<&'a DateTime<Utc>>,
    pub(crate) baseline_dirty: &'a HashSet<String>,
    pub(crate) current_dirty: &'a HashSet<String>,
}

pub(crate) struct ChangeLedger {
    pending: PendingHandle,
    internal_writes: Mutex<HashMap<PathBuf, Instant>>,
    events: Mutex<VecDeque<MutationRecord>>,
    external_changed: Mutex<HashSet<String>>,
}

impl ChangeLedger {
    pub(super) fn new(root: PathBuf, exclusions: WorkspaceExclusions) -> Self {
        Self {
            pending: PendingHandle {
                inner: Arc::new(PendingInner {
                    root,
                    exclusions,
                    paths: Mutex::new(HashSet::new()),
                    needs_reconcile: AtomicBool::new(false),
                }),
            },
            internal_writes: Mutex::new(HashMap::new()),
            events: Mutex::new(VecDeque::new()),
            external_changed: Mutex::new(HashSet::new()),
        }
    }

    pub(crate) fn pending_handle(&self) -> PendingHandle {
        self.pending.clone()
    }

    pub(crate) fn needs_reconcile(&self) -> bool {
        self.pending.inner.needs_reconcile.load(Ordering::Acquire)
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.pending.inner.paths.lock().len()
    }

    /// Queue paths whose index entries are known to be out of date.
    pub(crate) fn mark_pending(&self, paths: impl IntoIterator<Item = PathBuf>) {
        self.pending.inner.paths.lock().extend(paths);
        self.pending
            .inner
            .needs_reconcile
            .store(true, Ordering::Release);
    }

    /// Drop every pending path (a full rescan supersedes them).
    pub(crate) fn clear_pending(&self) {
        self.pending.inner.paths.lock().clear();
        self.pending
            .inner
            .needs_reconcile
            .store(false, Ordering::Release);
    }

    /// Take the pending paths and classify them. Returns `None` when no reconcile
    /// was requested. Consumes the internal-write markers of drained paths.
    pub(crate) fn drain_pending(&self, now: Instant) -> Option<PendingDrain> {
        let inner = &self.pending.inner;
        if !inner.needs_reconcile.swap(false, Ordering::AcqRel) {
            return None;
        }
        let pending = std::mem::take(&mut *inner.paths.lock());
        let mut drain = PendingDrain::default();
        for path in pending {
            let relative = relative_string(&inner.root, &path);
            if relative == ".git" || relative.starts_with(".git/") {
                drain.git_event = true;
                continue;
            }
            if relative.is_empty()
                || relative == "."
                || inner.exclusions.is_ignored(&path, path.is_dir())
                || is_temp_file(&path)
                || path.is_dir()
            {
                continue;
            }
            if !self.consume_internal(&path, now) {
                drain.external.insert(relative);
            }
            drain.paths.insert(path);
        }
        Some(drain)
    }

    /// Mark `path` as about to be written by CodeWeave.
    pub(crate) fn record_internal(&self, path: PathBuf, now: Instant) {
        let mut internal = self.internal_writes.lock();
        internal.retain(|_, written| now.saturating_duration_since(*written) < INTERNAL_WRITE_TTL);
        internal.insert(path, now);
    }

    /// Withdraw a marker for a write that did not happen.
    pub(crate) fn forget_internal(&self, path: &Path) {
        self.internal_writes.lock().remove(path);
    }

    /// Whether a live marker says CodeWeave wrote `path`. The marker is consumed.
    pub(crate) fn consume_internal(&self, path: &Path, now: Instant) -> bool {
        self.internal_writes
            .lock()
            .remove(path)
            .is_some_and(|written| now.saturating_duration_since(written) < INTERNAL_WRITE_TTL)
    }

    /// Append mutation records to the bounded log.
    pub(crate) fn record(&self, records: &[MutationRecord]) {
        let mut events = self.events.lock();
        events.extend(records.iter().cloned());
        while events.len() > MAX_MUTATION_EVENTS {
            events.pop_front();
        }
    }

    /// Record external changes: appended to the log and remembered as observed
    /// external paths for dirty-ownership reporting.
    pub(crate) fn record_external(&self, records: &[MutationRecord]) {
        self.external_changed
            .lock()
            .extend(records.iter().map(|record| record.path.clone()));
        self.record(records);
    }

    /// Newest-first records with a generation above `since`, optionally of one source.
    pub(crate) fn records_since(
        &self,
        since: u64,
        source: Option<&str>,
        limit: usize,
    ) -> Vec<MutationRecord> {
        self.events
            .lock()
            .iter()
            .rev()
            .filter(|item| item.generation > since)
            .filter(|item| source.is_none_or(|value| item.source == value))
            .take(limit)
            .cloned()
            .collect()
    }

    /// Paths in `dirty` that CodeWeave edits touched.
    pub(crate) fn edited_paths_within(&self, dirty: &HashSet<String>) -> HashSet<String> {
        self.events
            .lock()
            .iter()
            .filter(|item| item.source == "mcp_edit" && dirty.contains(&item.path))
            .map(|item| item.path.clone())
            .collect()
    }

    /// Paths in `dirty` observed changing outside CodeWeave.
    pub(crate) fn external_paths_within(&self, dirty: &HashSet<String>) -> HashSet<String> {
        self.external_changed
            .lock()
            .iter()
            .filter(|path| dirty.contains(*path))
            .cloned()
            .collect()
    }

    /// Paths a Bash run changed: mutation records inside the run's generation window
    /// plus paths whose dirty state flipped. Once the run has ended, a change is
    /// attributed only if it was recorded, or the file last modified, by then.
    /// Reads file metadata, so call it off the async executor.
    pub(crate) fn attribute_since(&self, window: &RunWindow<'_>) -> HashSet<String> {
        let root = &self.pending.inner.root;
        let exclusions = &self.pending.inner.exclusions;
        let candidates: Vec<MutationRecord> = self
            .events
            .lock()
            .iter()
            .filter(|mutation| {
                mutation.generation > window.start_generation
                    && mutation.generation <= window.end_generation
            })
            .cloned()
            .collect();
        let modified_by_end = |path: &str, ended: &DateTime<Utc>| {
            fs::metadata(root.join(path))
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| DateTime::<Utc>::from(modified) <= *ended)
        };
        let mutation_paths: HashSet<String> = candidates
            .into_iter()
            .filter(|mutation| {
                window.ended_at.is_none_or(|ended| {
                    mutation.timestamp <= *ended || modified_by_end(&mutation.path, ended)
                })
            })
            .map(|mutation| mutation.path)
            .collect();
        let mut paths: HashSet<String> = window
            .current_dirty
            .symmetric_difference(window.baseline_dirty)
            .filter(|path| {
                mutation_paths.contains(*path)
                    || window
                        .ended_at
                        .is_none_or(|ended| modified_by_end(path, ended))
            })
            .cloned()
            .collect();
        paths.extend(mutation_paths);
        paths.retain(|path| !exclusions.is_ignored(Path::new(path), false));
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(root: &Path) -> ChangeLedger {
        ChangeLedger::new(
            root.to_path_buf(),
            WorkspaceExclusions::new(root, &[]).unwrap(),
        )
    }

    #[test]
    fn internal_write_marker_expires_after_its_ttl() {
        let root = tempfile::tempdir().unwrap();
        let ledger = ledger(root.path());
        let path = root.path().join("value.rs");
        let written = Instant::now();

        ledger.record_internal(path.clone(), written);
        assert!(ledger.consume_internal(&path, written + Duration::from_secs(29)));
        assert!(!ledger.consume_internal(&path, written + Duration::from_secs(29)));

        ledger.record_internal(path.clone(), written);
        assert!(!ledger.consume_internal(&path, written + Duration::from_secs(31)));
    }

    #[test]
    fn drained_paths_split_internal_writes_from_external_changes() {
        let root = tempfile::tempdir().unwrap();
        let ledger = ledger(root.path());
        let ours = root.path().join("ours.rs");
        let theirs = root.path().join("theirs.rs");
        let now = Instant::now();
        ledger.record_internal(ours.clone(), now);

        ledger.pending_handle().observe([
            ours.clone(),
            theirs.clone(),
            root.path().join(temp_file_name("ours.rs", "abc")),
        ]);
        let drain = ledger.drain_pending(now).unwrap();

        assert_eq!(drain.paths, HashSet::from([ours, theirs]));
        assert_eq!(drain.external, HashSet::from(["theirs.rs".to_owned()]));
        assert!(!drain.git_event);
        assert!(ledger.drain_pending(now).is_none());
    }
}
