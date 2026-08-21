mod commit;
mod edit;
mod events;
mod fetch;
mod git;
mod io_helpers;
mod reconcile;
mod retrieve;
mod run;
mod summary;
mod util;
mod validation;

pub use events::MutationRecord;
#[cfg(test)]
use git::validated_push_target;
use util::{char_boundary_at_or_before, summarize_changed_paths, ChangedPathSummary};

use crate::bash::{BashSupervisor, StartRequest};
use crate::index::{content_hash, CodeIndex, WorkspaceExclusions};
use crate::model::{
    bool_value, required_str, usize_value, AppError, AppResult, PolicyConfig, WorkspaceConfig,
};
use crate::repository::{CliGitBackend, RepoStatus, RepositoryBackend};
use crate::retrieval::execute_index_search;
use crate::security::{canonical_root, relative_string};
use chrono::{DateTime, Utc};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

/// Minimum spacing between workspace reconciles triggered by non-terminal run polls.
const POLL_RECONCILE_DEBOUNCE: Duration = Duration::from_secs(2);

/// Maximum bytes of an instruction file (AGENTS.md/CLAUDE.md) inlined into a summary
/// response before it is truncated and the caller is pointed at a code_retrieve read.
const INSTRUCTION_INLINE_CAP: usize = 4_096;

pub struct Workspace {
    // Lock ordering for code that needs more than one guard:
    // write_lock -> reconcile_lock -> pending_paths -> index -> repo_status -> snapshot_id.
    // internal_writes, mutations, and _watcher are
    // isolated owner locks. Capture their data and release the guard before
    // acquiring another workspace lock.
    pub id: String,
    pub name: String,
    root: PathBuf,
    policy: PolicyConfig,
    artifact_paths: Vec<String>,
    exclusions: WorkspaceExclusions,
    index: Arc<RwLock<CodeIndex>>,
    generation: Arc<AtomicU64>,
    snapshot_id: Arc<RwLock<String>>,
    repository: Arc<dyn RepositoryBackend>,
    repo_status: RwLock<RepoStatus>,
    /// Set when a `git status` refresh failed and the cached `repo_status` may be
    /// out of date. Surfaced as `repo_status_stale: true` in responses so callers
    /// don't treat a silently-empty status as "clean" (D8). Cleared on the next
    /// successful refresh.
    repo_status_stale: AtomicBool,
    opened_dirty_summary: ChangedPathSummary,
    external_changed: Mutex<HashSet<String>>,
    pending_paths: Arc<Mutex<HashSet<PathBuf>>>,
    needs_reconcile: Arc<AtomicBool>,
    reconcile_lock: Mutex<()>,
    last_reconcile: Mutex<Instant>,
    internal_writes: Arc<Mutex<HashMap<PathBuf, Instant>>>,
    mutations: Mutex<VecDeque<MutationRecord>>,
    bash: BashSupervisor,
    write_lock: Arc<tokio::sync::Mutex<()>>,
    open_diagnostics: Value,
    _watcher: Mutex<RecommendedWatcher>,
}

impl Workspace {
    #[cfg(test)]
    pub fn root_path(&self) -> &Path {
        &self.root
    }

    /// Number of files currently held in the code index. Reported by `/health`
    /// so operators can confirm the eager startup scan populated the index.
    pub fn index_file_count(&self) -> usize {
        self.index.read().file_count()
    }

    /// Milliseconds since the index was last reconciled against the filesystem.
    /// A small value right after startup confirms the eager scan is fresh.
    pub fn last_reconcile_elapsed_ms(&self) -> u128 {
        self.last_reconcile.lock().elapsed().as_millis()
    }

    /// Pre-probe Bash readiness at startup so the first validated edit does not
    /// pay the discovery/probe cost inline. Returns the readiness result.
    pub fn probe_bash(&self) -> AppResult<()> {
        self.bash.ensure_available()
    }

    pub fn open(
        config: &WorkspaceConfig,
        policy: PolicyConfig,
        cache_root: PathBuf,
    ) -> AppResult<Self> {
        let opened_started = Instant::now();
        let phase_started = Instant::now();
        let root = canonical_root(Path::new(&config.path))?;
        let canonicalize_ms = phase_started.elapsed().as_millis();
        let cache_key = content_hash(&root.to_string_lossy());
        let workspace_cache = cache_root.join("repos").join(cache_key);
        fs::create_dir_all(&workspace_cache)?;
        let exclusions = WorkspaceExclusions::new(&root, &config.exclude_paths)?;

        let phase_started = Instant::now();
        let repository: Arc<dyn RepositoryBackend> = Arc::new(CliGitBackend);
        let repo_status = repository.status(&root).unwrap_or_default();
        let git_ms = phase_started.elapsed().as_millis();
        let opened_dirty: HashSet<String> = repo_status
            .dirty_files
            .iter()
            .filter(|path| !exclusions.is_ignored(Path::new(path), false))
            .cloned()
            .collect();

        let phase_started = Instant::now();
        let index_cache = workspace_cache.join("index.json");
        let (mut index, index_cache_hit) = CodeIndex::scan_cached(
            &root,
            policy.max_file_bytes,
            &config.artifact_paths,
            &exclusions,
            &index_cache,
        )?;
        let index_ms = phase_started.elapsed().as_millis();
        let snapshot_id = index.snapshot_id(&repo_status.head);
        let generation = Arc::new(AtomicU64::new(1));
        let pending_paths = Arc::new(Mutex::new(HashSet::new()));
        let needs_reconcile = Arc::new(AtomicBool::new(false));
        let internal_writes = Arc::new(Mutex::new(HashMap::new()));
        let pending_for_watcher = pending_paths.clone();
        let reconcile_for_watcher = needs_reconcile.clone();
        let root_for_watcher = root.clone();
        let exclusions_for_watcher = exclusions.clone();
        let watcher_started = Instant::now();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else {
                    return;
                };
                let mut pending = pending_for_watcher.lock();
                for path in event.paths {
                    if !path.starts_with(&root_for_watcher) {
                        continue;
                    }
                    let relative = relative_string(&root_for_watcher, &path);
                    let is_temp = path
                        .file_name()
                        .and_then(|value| value.to_str())
                        .map(|name| name.contains(".codeweave-"))
                        .unwrap_or(false);
                    if relative.is_empty()
                        || exclusions_for_watcher.is_ignored(&path, path.is_dir())
                        || is_temp
                    {
                        continue;
                    }
                    pending.insert(path);
                    reconcile_for_watcher.store(true, Ordering::Release);
                }
            })
            .map_err(AppError::internal)?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(AppError::internal)?;
        let watcher_ms = watcher_started.elapsed().as_millis();
        let runtime_started = Instant::now();
        let mutations = VecDeque::new();
        let bash = BashSupervisor::new(workspace_cache, policy.clone())?;
        let runtime_ms = runtime_started.elapsed().as_millis();
        let open_diagnostics = json!({
            "cache_hit": index_cache_hit,
            "total_ms": opened_started.elapsed().as_millis(),
            "phases_ms": {
                "canonicalize": canonicalize_ms,
                "git": git_ms,
                "index": index_ms,
                "watcher": watcher_ms,
            "runtime": runtime_ms
            }
        });
        Ok(Self {
            id: config.id.clone(),
            name: config.name.clone(),
            root,
            policy,
            artifact_paths: config.artifact_paths.clone(),
            exclusions,
            index: Arc::new(RwLock::new(index)),
            generation,
            snapshot_id: Arc::new(RwLock::new(snapshot_id)),
            repository,
            repo_status: RwLock::new(repo_status),
            repo_status_stale: AtomicBool::new(false),
            opened_dirty_summary: summarize_changed_paths(opened_dirty),
            external_changed: Mutex::new(HashSet::new()),
            pending_paths,
            needs_reconcile,
            reconcile_lock: Mutex::new(()),
            last_reconcile: Mutex::new(Instant::now()),
            internal_writes,
            mutations: Mutex::new(mutations),
            bash,
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            open_diagnostics,
            _watcher: Mutex::new(watcher),
        })
    }
}

#[allow(dead_code)]
pub(super) fn add_reconcile_pending(value: &mut Value, pending: bool) {
    if let Some(object) = value.as_object_mut() {
        object.insert("reconcile_pending".to_owned(), json!(pending));
    }
}

pub(super) fn add_phase_metrics(value: &mut Value, phases: &[(&str, u128)]) {
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "phase_ms".to_owned(),
            Value::Object(
                phases
                    .iter()
                    .map(|(name, elapsed)| ((*name).to_owned(), json!(elapsed)))
                    .collect(),
            ),
        );
    }
}

#[cfg(test)]
mod tests;
