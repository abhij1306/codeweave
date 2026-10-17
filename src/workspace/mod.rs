mod commit;
mod edit;
mod events;
mod fetch;
mod git;
mod io_helpers;
mod reconcile;
mod retrieve;
mod run;
mod state;
mod summary;
mod util;
mod validation;

pub use events::{MutationRecord, MutationSource};
#[cfg(test)]
use git::validated_push_target;
pub use state::{Stamp, StateReader, StateView};
use util::{summarize_changed_paths, ChangedPathSummary};

use state::ReconciledState;

use crate::bash::BashSupervisor;
use crate::error::{AppError, AppResult};
use crate::index::{content_hash, CodeIndex, SnapshotId, WorkspaceExclusions};
use crate::model::{PolicyConfig, WorkspaceConfig};
use crate::repository::{CliGitBackend, RepositoryBackend};
use crate::retrieval::{execute_index_search, SearchRequest};
use crate::security::WorkspaceRoot;
use chrono::Utc;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Minimum spacing between workspace reconciles triggered by non-terminal run polls.
const POLL_RECONCILE_DEBOUNCE: Duration = Duration::from_secs(2);

/// Maximum bytes of an instruction file (AGENTS.md/CLAUDE.md) inlined into a summary
/// response before it is truncated and the caller is pointed at a code_retrieve read.
const INSTRUCTION_INLINE_CAP: usize = 4_096;

/// One open repository. The reconciled state (index, repository status, stamp),
/// the mutation gate and the change ledger live in the `state` module; see its docs
/// for the lock order.
pub struct Workspace {
    pub id: String,
    pub name: String,
    root: WorkspaceRoot,
    policy: PolicyConfig,
    artifact_paths: Vec<String>,
    exclusions: WorkspaceExclusions,
    state: ReconciledState,
    repository: Arc<dyn RepositoryBackend>,
    opened_dirty_summary: ChangedPathSummary,
    last_reconcile: Mutex<Instant>,
    bash: BashSupervisor,
    open_diagnostics: Value,
    _watcher: Mutex<RecommendedWatcher>,
}

impl Workspace {
    #[cfg(test)]
    pub fn root_path(&self) -> &Path {
        self.root.path()
    }

    /// The workspace root handle every file read and write goes through.
    pub fn root(&self) -> &WorkspaceRoot {
        &self.root
    }

    /// Number of files currently held in the code index. Reported by `/health`
    /// so operators can confirm the eager startup scan populated the index.
    pub fn index_file_count(&self) -> usize {
        self.state.read().index().file_count()
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
        Self::open_with_repository(config, policy, cache_root, Arc::new(CliGitBackend))
    }

    pub(crate) fn open_with_repository(
        config: &WorkspaceConfig,
        policy: PolicyConfig,
        cache_root: PathBuf,
        repository: Arc<dyn RepositoryBackend>,
    ) -> AppResult<Self> {
        let opened_started = Instant::now();
        let phase_started = Instant::now();
        let root = WorkspaceRoot::open(Path::new(&config.path))?;
        let canonicalize_ms = phase_started.elapsed().as_millis();
        let cache_key = content_hash(&root.path().to_string_lossy()).into_string();
        let workspace_cache = cache_root.join("repos").join(cache_key);
        fs::create_dir_all(&workspace_cache)?;
        let exclusions = WorkspaceExclusions::new(root.path(), &config.exclude_paths)?;

        let phase_started = Instant::now();
        let repo_status = repository.status(&root);
        let git_ms = phase_started.elapsed().as_millis();
        let opened_dirty: HashSet<String> = repo_status
            .as_ref()
            .map(|status| status.dirty_files.as_slice())
            .unwrap_or_default()
            .iter()
            .filter(|path| !exclusions.is_ignored(Path::new(path), false))
            .cloned()
            .collect();

        let phase_started = Instant::now();
        let index_cache = workspace_cache.join("index.json");
        let (index, index_cache_hit) = CodeIndex::scan_cached(
            &root,
            policy.max_file_bytes,
            &config.artifact_paths,
            &exclusions,
            &index_cache,
        )?;
        let index_ms = phase_started.elapsed().as_millis();
        let state = ReconciledState::new(
            &config.id,
            root.clone(),
            exclusions.clone(),
            index,
            repo_status,
        );
        let pending = state.ledger().pending_handle();
        let watcher_started = Instant::now();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if let Ok(event) = event {
                    pending.observe(event.paths);
                }
            })
            .map_err(AppError::internal)?;
        watcher
            .watch(root.path(), RecursiveMode::Recursive)
            .map_err(AppError::internal)?;
        let watcher_ms = watcher_started.elapsed().as_millis();
        let runtime_started = Instant::now();
        let bash = BashSupervisor::new(policy.clone())?;
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
            state,
            repository,
            opened_dirty_summary: summarize_changed_paths(opened_dirty),
            last_reconcile: Mutex::new(Instant::now()),
            bash,
            open_diagnostics,
            _watcher: Mutex::new(watcher),
        })
    }
}

#[cfg(test)]
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
