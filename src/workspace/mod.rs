mod commit;
mod edit;
mod events;
mod fetch;
mod git;
mod io_helpers;
mod retrieve;
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

    /// Refresh the cached `repo_status` from `git status`. On failure, log a
    /// warning and set `repo_status_stale` instead of silently clobbering the
    /// cache with an empty default (D8): an empty status looks identical to a
    /// clean tree, which would mislead callers about what is staged/dirty. The
    /// previous (possibly-stale) status is retained so downstream logic still has
    /// its best-known view.
    pub(super) fn refresh_repo_status(&self) {
        match self.repository.status(&self.root) {
            Ok(status) => {
                *self.repo_status.write() = status;
                self.repo_status_stale.store(false, Ordering::Release);
            }
            Err(error) => {
                tracing::warn!(
                    workspace = %self.id,
                    error = %error,
                    "git status refresh failed; repo_status may be stale"
                );
                self.repo_status_stale.store(true, Ordering::Release);
            }
        }
    }

    /// Whether the cached repository status is known to be out of date because a
    /// refresh failed since the last successful `git status`.
    pub(super) fn repo_status_stale(&self) -> bool {
        self.repo_status_stale.load(Ordering::Acquire)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    pub fn snapshot(&self) -> String {
        self.snapshot_id.read().clone()
    }

    pub(crate) fn reference_index(&self) -> Arc<RwLock<CodeIndex>> {
        Arc::clone(&self.index)
    }

    pub(crate) fn reference_snapshot(&self) -> Arc<RwLock<String>> {
        Arc::clone(&self.snapshot_id)
    }

    fn reconcile_pending(&self) -> AppResult<Vec<String>> {
        if !self.needs_reconcile.load(Ordering::Acquire) {
            return Ok(Vec::new());
        }
        let _guard = self.reconcile_lock.lock();
        if !self.needs_reconcile.swap(false, Ordering::AcqRel) {
            return Ok(Vec::new());
        }
        let pending: HashSet<PathBuf> = std::mem::take(&mut *self.pending_paths.lock());
        if pending.is_empty() {
            return Ok(Vec::new());
        }

        let now = Instant::now();
        let mut relevant = HashSet::new();
        let mut external_candidates = HashSet::new();
        let mut candidates = Vec::new();
        let mut git_event = false;
        {
            let mut internal = self.internal_writes.lock();
            // Watcher delivery can be delayed on busy or network-backed filesystems.
            // Retain internal-write markers long enough to avoid misclassifying our
            // own atomic writes as external changes.
            internal.retain(|_, time| now.duration_since(*time) < Duration::from_secs(30));
            for path in pending {
                let relative = relative_string(&self.root, &path);
                if relative == ".git" || relative.starts_with(".git/") {
                    git_event = true;
                    continue;
                }
                let is_temp = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .map(|name| name.contains(".codeweave-"))
                    .unwrap_or(false);
                if relative.is_empty()
                    || relative == "."
                    || self.exclusions.is_ignored(&path, path.is_dir())
                    || is_temp
                    || path.is_dir()
                {
                    continue;
                }
                let was_internal = internal.contains_key(&path);
                candidates.push((path, relative, was_internal));
            }
        }

        for (path, relative, was_internal) in candidates {
            if was_internal {
                self.internal_writes.lock().remove(&path);
                relevant.insert(path);
                continue;
            }
            external_candidates.insert(relative);
            relevant.insert(path);
        }

        let changed = if relevant.is_empty() {
            Vec::new()
        } else {
            self.index.write().refresh_paths(
                &self.root,
                &relevant,
                self.policy.max_file_bytes,
                &self.exclusions,
            )?
        };
        let changed_set: HashSet<String> = changed.iter().cloned().collect();

        let previous_repo = self.repo_status.read().clone();
        let next_repo = if git_event || !changed.is_empty() {
            self.repository
                .status(&self.root)
                .unwrap_or_else(|_| previous_repo.clone())
        } else {
            previous_repo.clone()
        };
        let repo_changed = next_repo != previous_repo;
        let head_changed = next_repo.head != previous_repo.head;
        if repo_changed {
            *self.repo_status.write() = next_repo;
        }

        if changed.is_empty() && !repo_changed {
            return Ok(changed);
        }

        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let external: Vec<String> = external_candidates
            .into_iter()
            .filter(|path| changed_set.contains(path))
            .collect();
        if !external.is_empty() {
            let records = {
                let index = self.index.read();
                external
                    .iter()
                    .map(|path| MutationRecord {
                        mutation_id: MutationRecord::new_id(),
                        path: path.clone(),
                        before_hash: None,
                        after_hash: index.get(path).map(|file| file.hash.clone()),
                        source: "external".to_owned(),
                        request_id: "watcher".to_owned(),
                        timestamp: Utc::now(),
                        generation,
                    })
                    .collect::<Vec<_>>()
            };
            self.external_changed.lock().extend(external);
            self.record_mutations(&records)?;
        }
        if !changed.is_empty() || head_changed {
            self.recompute_snapshot();
        }
        Ok(changed)
    }

    async fn reconcile_pending_async(self: &Arc<Self>) -> AppResult<Vec<String>> {
        if !self.needs_reconcile.load(Ordering::Acquire) {
            return Ok(Vec::new());
        }
        let actor = Arc::clone(self);
        let changed = tokio::task::spawn_blocking(move || actor.reconcile_pending())
            .await
            .map_err(AppError::internal)?;
        *self.last_reconcile.lock() = Instant::now();
        changed
    }

    /// Reconcile debounce for high-frequency run polls. `bash_status`/`bash_output`/
    /// `bash_cancel` fire repeatedly while a command streams output and each one would
    /// otherwise trigger a full `refresh_paths` + `git status` subprocess whenever the
    /// running command touches the tree. Skip the refresh unless the run reached a
    /// terminal state or it has been at least `POLL_RECONCILE_DEBOUNCE` since the last
    /// reconcile, so the workspace view still converges without paying per-poll latency.
    async fn reconcile_after_poll(self: &Arc<Self>, terminal: bool) -> AppResult<Vec<String>> {
        if !self.needs_reconcile.load(Ordering::Acquire) {
            return Ok(Vec::new());
        }
        if !terminal {
            let since = self.last_reconcile.lock().elapsed();
            if since < POLL_RECONCILE_DEBOUNCE {
                return Ok(Vec::new());
            }
        }
        self.reconcile_pending_async().await
    }

    fn recompute_snapshot(&self) {
        let head = self.repo_status.read().head.clone();
        let snapshot = self.index.write().snapshot_id(&head);
        *self.snapshot_id.write() = snapshot;
    }

    fn read_reconcile_pending(&self) -> bool {
        self.needs_reconcile.load(Ordering::Acquire)
    }

    /// Lightweight alternative to `summary()` for the warm-path auto-open in
    /// `prepare()`. Returns only the identifiers that `prepare()` mines from
    /// the full summary (workspace_id, snapshot_id, generation) without
    /// iterating mutations, cloning dirty sets, or calling
    /// `summarize_changed_paths`.
    pub fn summary_ids(&self) -> AppResult<Value> {
        self.reconcile_pending()?;
        Ok(json!({
            "workspace_id": self.id,
            "snapshot_id": self.snapshot(),
            "generation": self.generation(),
        }))
    }

    pub fn diagnostics(&self) -> AppResult<Value> {
        let index = self.index.read();
        Ok(json!({
            "workspace_id": self.id,
            "root": self.root,
            "generation": self.generation(),
            "snapshot_id": self.snapshot(),
            "file_count": index.file_count(),
            "languages": index.languages(),
            "reconcile_pending": self.read_reconcile_pending(),
            "pending_path_count": self.pending_paths.lock().len(),
            "running_bash_count": self.bash.running_count(),
            "execution": {
                "bash": self.bash.readiness()
            },
            "policy": {
                "max_file_bytes": self.policy.max_file_bytes,
                "max_context_chars": self.policy.max_context_chars,
                "max_search_results": self.policy.max_search_results,
                "bash": self.policy.bash,
            }
        }))
    }

    pub fn summary(&self) -> AppResult<Value> {
        let started = Instant::now();
        let reconcile_started = Instant::now();
        self.reconcile_pending()?;
        let reconcile_ms = reconcile_started.elapsed().as_millis();
        let index = self.index.read();
        let repo = self.repo_status.read().clone();
        let dirty_set: HashSet<String> = repo
            .dirty_files
            .iter()
            .filter(|path| !self.exclusions.is_ignored(Path::new(path), false))
            .cloned()
            .collect();
        let mcp_paths: HashSet<String> = self
            .mutations
            .lock()
            .iter()
            .filter(|item| item.source == "mcp_edit" && dirty_set.contains(&item.path))
            .map(|item| item.path.clone())
            .collect();
        let external: HashSet<String> = self
            .external_changed
            .lock()
            .iter()
            .filter(|path| dirty_set.contains(*path))
            .cloned()
            .collect();
        let preexisting = &self.opened_dirty_summary;
        let mcp_changed = summarize_changed_paths(mcp_paths);
        let external = summarize_changed_paths(external);
        let repository_dirty = summarize_changed_paths(dirty_set);
        let repository = json!({
            "is_git": repo.is_git,
            "head": repo.head,
            "branch": repo.branch,
            "dirty_files": repository_dirty.paths,
            "dirty_file_count": repository_dirty.count,
            "dirty_files_truncated": repository_dirty.truncated,
            "dirty_file_groups": repository_dirty.groups
        });
        // Instruction files are inlined into every summary/open. Cap the inlined body
        // so a large AGENTS.md/CLAUDE.md cannot dominate the response; the caller can
        // read the rest with a code_retrieve path read when truncated.
        let instructions = ["AGENTS.md", "CLAUDE.md"]
            .into_iter()
            .filter_map(|path| {
                index.get(path).map(|file| {
                    let full_len = file.content.len();
                    if full_len > INSTRUCTION_INLINE_CAP {
                        let safe_cap =
                            char_boundary_at_or_before(&file.content, INSTRUCTION_INLINE_CAP);
                        let end = file.content[..safe_cap]
                            .rfind('\n')
                            .map(|idx| idx + 1)
                            .unwrap_or(safe_cap);
                        json!({
                            "path": path,
                            "content": &file.content[..end],
                            "content_truncated": true,
                            "content_bytes": full_len,
                            "guidance": "Instruction file truncated; use code_retrieve with operation=read and target=path."
                        })
                    } else {
                        json!({"path": path, "content": file.content})
                    }
                })
            })
            .collect::<Vec<_>>();
        let bash = self.bash.readiness();
        let bash_available = bash.is_ready();
        let validation_guidance = if bash_available {
            "Write-tool validate fields accept Bash command strings. Use bash(command='<command>') for standalone execution."
        } else {
            "No usable Bash implementation passed readiness checks. Fix policy.bash.executable or install Git Bash/MSYS2/Cygwin Bash."
        };
        let warnings = if bash_available {
            Vec::<String>::new()
        } else {
            vec![format!(
                "Bash execution and write-tool validation commands are unavailable: {}",
                bash.failure_reason
                    .as_deref()
                    .unwrap_or("No usable Bash implementation found")
            )]
        };
        let mut result = json!({
            "workspace_id": self.id, "name": self.name, "root": self.root, "generation": self.generation(), "snapshot_id": self.snapshot(),
            "file_count": index.file_count(), "languages": index.languages(), "repository": repository, "instructions": instructions,
            "capabilities": {
                "bash_available": bash_available,
                "bash": bash,
                "validation_guidance": validation_guidance
            },
            "warnings": warnings,
            "open_diagnostics": self.open_diagnostics,
            "dirty_ownership": {
                "preexisting_at_open": &preexisting.paths,
                "changed_by_mcp": mcp_changed.paths,
                "observed_external": external.paths,
                "counts": {
                    "preexisting_at_open": preexisting.count,
                    "changed_by_mcp": mcp_changed.count,
                    "observed_external": external.count
                },
                "truncated": {
                    "preexisting_at_open": preexisting.truncated,
                    "changed_by_mcp": mcp_changed.truncated,
                    "observed_external": external.truncated
                },
                "groups": {
                    "preexisting_at_open": &preexisting.groups,
                    "changed_by_mcp": mcp_changed.groups,
                    "observed_external": external.groups
                }
            },
            "tool_guidance": format!("This runtime has one active repository. Context and edits read cached state; call workspace refresh only after suspected missed external changes. {validation_guidance}")
        });
        add_phase_metrics(
            &mut result,
            &[
                ("reconcile", reconcile_ms),
                ("total_local", started.elapsed().as_millis()),
            ],
        );
        Ok(result)
    }

    pub fn refresh(&self, force: bool) -> AppResult<Value> {
        if force {
            let _guard = self.reconcile_lock.lock();
            self.pending_paths.lock().clear();
            self.needs_reconcile.store(false, Ordering::Release);
            *self.index.write() = CodeIndex::scan(
                &self.root,
                self.policy.max_file_bytes,
                &self.artifact_paths,
                &self.exclusions,
            )?;
            self.refresh_repo_status();
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.recompute_snapshot();
        } else {
            self.reconcile_pending()?;
        }
        self.summary()
    }

    pub(super) fn search_index(&self, params: &Value) -> AppResult<Value> {
        let reconcile_pending = self.read_reconcile_pending();
        let snapshot = self.snapshot();
        let index = self.index.read();
        execute_index_search(
            &index,
            &self.id,
            &snapshot,
            params,
            self.policy.max_search_results,
            reconcile_pending,
        )
    }

    pub fn changes(&self, params: &Value) -> AppResult<Value> {
        let started = Instant::now();
        let reconcile_started = Instant::now();
        self.reconcile_pending()?;
        let reconcile_ms = reconcile_started.elapsed().as_millis();
        let since = params
            .get("since_generation")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let source = params.get("source").and_then(Value::as_str);
        let limit = usize_value(params, "limit", 200).min(2_000);
        let records: Vec<_> = self
            .mutations
            .lock()
            .iter()
            .rev()
            .filter(|item| item.generation > since)
            .filter(|item| source.map(|value| item.source == value).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect();
        let mut result = json!({
            "workspace_id": self.id,
            "generation": self.generation(),
            "snapshot_id": self.snapshot(),
            "mutations": records
        });
        add_phase_metrics(
            &mut result,
            &[
                ("reconcile", reconcile_ms),
                ("total_local", started.elapsed().as_millis()),
            ],
        );
        Ok(result)
    }

    pub async fn run(self: &Arc<Self>, params: &Value) -> AppResult<Value> {
        let started = Instant::now();
        let action = params
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("start");
        // Run polls (status/output/cancel) never mutate the tree themselves, so a
        // pre-action reconcile only adds latency to a hot loop. Reconcile before
        // `start` (which may depend on a fresh view) and defer poll reconciles to the
        // debounced pass after the action completes.
        let is_poll = matches!(action, "status" | "output" | "cancel");
        let reconcile_started = Instant::now();
        if !is_poll {
            self.reconcile_pending_async().await?;
        }
        let reconcile_before_ms = reconcile_started.elapsed().as_millis();
        let mut run_startup_ms = None;
        let mut result = match action {
            "start" => {
                let before = self.generation();
                let before_dirty: HashSet<String> = self
                    .repo_status
                    .read()
                    .dirty_files
                    .iter()
                    .cloned()
                    .collect();
                let command = required_str(params, "command")?.to_owned();
                let run_started = Instant::now();
                let value = self
                    .bash
                    .start(
                        &self.root,
                        StartRequest {
                            command,
                            cwd: params.get("cwd").and_then(Value::as_str).map(str::to_owned),
                            background: params.get("background").and_then(Value::as_bool),
                            timeout_ms: params.get("timeout_ms").and_then(Value::as_u64),
                        },
                    )
                    .await?;
                run_startup_ms = Some(run_started.elapsed().as_millis());
                if let Some(run_id) = value.get("run_id").and_then(Value::as_str) {
                    self.bash.set_change_baseline(run_id, before, before_dirty);
                }
                value
            }
            "status" => {
                let actor = Arc::clone(self);
                let run_id = required_str(params, "run_id")?.to_owned();
                tokio::task::spawn_blocking(move || actor.bash.status(&run_id))
                    .await
                    .map_err(AppError::internal)??
            }
            "output" => {
                let actor = Arc::clone(self);
                let run_id = required_str(params, "run_id")?.to_owned();
                let continuation = params
                    .get("continuation")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let stream = params
                    .get("stream")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                tokio::task::spawn_blocking(move || {
                    actor
                        .bash
                        .output_stream(&run_id, continuation.as_deref(), stream.as_deref())
                })
                .await
                .map_err(AppError::internal)??
            }
            "cancel" => {
                let actor = Arc::clone(self);
                let run_id = required_str(params, "run_id")?.to_owned();
                tokio::task::spawn_blocking(move || actor.bash.cancel(&run_id))
                    .await
                    .map_err(AppError::internal)??
            }
            other => {
                return Err(AppError::details(
                    "INVALID_RUN_ACTION",
                    "Unknown run action",
                    json!({"action": other}),
                ))
            }
        };
        let reconcile_after_started = Instant::now();
        if is_poll {
            let terminal = result
                .get("status")
                .and_then(Value::as_str)
                .map(|status| !matches!(status, "queued" | "running" | "cancelling"))
                .unwrap_or(true);
            self.reconcile_after_poll(terminal).await?;
        } else {
            self.reconcile_pending_async().await?;
        }
        let reconcile_after_ms = reconcile_after_started.elapsed().as_millis();
        if let Some(run_id) = result
            .get("run_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            let current_dirty: HashSet<String> = self
                .repo_status
                .read()
                .dirty_files
                .iter()
                .cloned()
                .collect();
            let terminal = result
                .get("status")
                .and_then(Value::as_str)
                .map(|status| !matches!(status, "queued" | "running" | "cancelling"))
                .unwrap_or(true);
            let current_generation = self.generation();
            let mutation_snapshot = self.mutations.lock().iter().cloned().collect::<Vec<_>>();
            let (start_generation, attribution_generation, changed_paths) =
                self.bash.observe_changes(
                    &run_id,
                    current_generation,
                    current_dirty,
                    terminal,
                    |start_generation, baseline_dirty, ended_at, current_dirty| {
                        self.observed_run_changed_paths(
                            &mutation_snapshot,
                            start_generation,
                            baseline_dirty,
                            current_generation,
                            ended_at,
                            current_dirty,
                        )
                    },
                )?;
            let changed = summarize_changed_paths(changed_paths);
            if let Some(object) = result.as_object_mut() {
                object.insert(
                    "workspace_generation_before".to_owned(),
                    json!(start_generation),
                );
                object.insert(
                    "workspace_generation_after".to_owned(),
                    json!(attribution_generation),
                );
                object.insert("observed_changed_paths".to_owned(), json!(changed.paths));
                object.insert(
                    "observed_changed_path_count".to_owned(),
                    json!(changed.count),
                );
                object.insert(
                    "observed_changed_paths_truncated".to_owned(),
                    json!(changed.truncated),
                );
                object.insert(
                    "observed_changed_path_groups".to_owned(),
                    json!(changed.groups),
                );
            }
        }
        if let Some(object) = result.as_object_mut() {
            let mut phases = serde_json::Map::new();
            phases.insert("reconcile_before".to_owned(), json!(reconcile_before_ms));
            phases.insert("reconcile_after".to_owned(), json!(reconcile_after_ms));
            phases.insert(
                "total_local".to_owned(),
                json!(started.elapsed().as_millis()),
            );
            if let Some(run_startup_ms) = run_startup_ms {
                phases.insert("run_startup".to_owned(), json!(run_startup_ms));
            }
            object.insert("phase_ms".to_owned(), Value::Object(phases));
        }
        Ok(result)
    }

    fn observed_run_changed_paths(
        &self,
        mutations: &[MutationRecord],
        start_generation: u64,
        baseline_dirty: &HashSet<String>,
        end_generation: u64,
        ended_at: Option<&DateTime<Utc>>,
        current_dirty: &HashSet<String>,
    ) -> HashSet<String> {
        let mutation_paths: HashSet<String> = mutations
            .iter()
            .filter(|mutation| {
                mutation.generation > start_generation
                    && mutation.generation <= end_generation
                    && ended_at
                        .map(|ended| {
                            mutation.timestamp <= *ended
                                || self.path_modified_at_or_before(&mutation.path, ended)
                        })
                        .unwrap_or(true)
            })
            .map(|mutation| mutation.path.clone())
            .collect();
        let mut paths: HashSet<String> = current_dirty
            .symmetric_difference(baseline_dirty)
            .filter(|path| {
                ended_at.is_none()
                    || mutation_paths.contains(*path)
                    || ended_at
                        .map(|ended| self.path_modified_at_or_before(path, ended))
                        .unwrap_or(true)
            })
            .cloned()
            .collect();
        paths.extend(mutation_paths);
        paths.retain(|path| !self.exclusions.is_ignored(Path::new(path), false));
        paths
    }

    fn path_modified_at_or_before(&self, path: &str, ended_at: &DateTime<Utc>) -> bool {
        let Ok(metadata) = fs::metadata(self.root.join(path)) else {
            return false;
        };
        let Ok(modified) = metadata.modified() else {
            return false;
        };
        let modified: DateTime<Utc> = modified.into();
        modified <= *ended_at
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
