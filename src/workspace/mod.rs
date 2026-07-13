mod edit;
mod fetch;
mod git;
mod io_helpers;
mod journal;
mod retrieve;
mod util;
mod validation;

#[cfg(test)]
use git::validated_push_target;
pub use journal::MutationRecord;
use journal::{load_journal, open_journal, rotate_journal_if_needed};
use util::{summarize_changed_paths, ChangedPathSummary};

use crate::bash::{BashSupervisor, StartRequest};
use crate::index::{content_hash, CodeIndex, SearchParams, WorkspaceExclusions};
use crate::model::{
    bool_value, required_str, string_list, usize_value, AppError, AppResult, PolicyConfig,
    WorkspaceConfig,
};
use crate::repository::{CliGitBackend, RepoStatus, RepositoryBackend};
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
const PROTOCOL_MAX_RESULTS: usize = 200;

fn result_limit(
    params: &Value,
    default: usize,
    configured_max: usize,
) -> (usize, usize, Vec<Value>) {
    let requested = usize_value(params, "max_results", default);
    let applied = requested.min(PROTOCOL_MAX_RESULTS).min(configured_max);
    let mut warnings = Vec::new();
    if requested > applied {
        let limit_message = if configured_max < PROTOCOL_MAX_RESULTS {
            format!("Requested {requested} results; the effective configured maximum is {configured_max}.")
        } else {
            format!("Requested {requested} results; protocol maximum is {PROTOCOL_MAX_RESULTS}.")
        };
        warnings.push(json!({"code": "MAX_RESULTS_CLAMPED", "message": limit_message}));
    }
    (requested, applied, warnings)
}

fn add_result_limits(
    result: &mut Value,
    requested: usize,
    applied: usize,
    configured_max: usize,
    configured_default: usize,
    warnings: Vec<Value>,
) {
    if let Some(object) = result.as_object_mut() {
        object.insert(
            "limits".to_owned(),
            json!({
                "requested_results": requested,
                "protocol_max_results": PROTOCOL_MAX_RESULTS,
                "configured_max_results": configured_max,
                "applied_results": applied,
                "configured_default_results": configured_default,
            }),
        );
        if !warnings.is_empty() {
            object.insert("warnings".to_owned(), Value::Array(warnings));
        }
    }
}

pub struct WorkspaceActor {
    // Lock ordering for code that needs more than one guard:
    // write_lock -> reconcile_lock -> pending_paths -> index -> repo_status.
    // internal_writes, mutations, and journal_file are terminal locks and must not
    // be held while acquiring another workspace lock.
    pub id: String,
    pub name: String,
    root: PathBuf,
    policy: PolicyConfig,
    artifact_paths: Vec<String>,
    exclusions: WorkspaceExclusions,
    index: RwLock<CodeIndex>,
    generation: Arc<AtomicU64>,
    snapshot_id: RwLock<String>,
    repository: Arc<dyn RepositoryBackend>,
    repo_status: RwLock<RepoStatus>,
    /// Set when a `git status` refresh failed and the cached `repo_status` may be
    /// out of date. Surfaced as `repo_status_stale: true` in responses so callers
    /// don't treat a silently-empty status as "clean" (D8). Cleared on the next
    /// successful refresh.
    repo_status_stale: AtomicBool,
    opened_dirty_summary: ChangedPathSummary,
    opened_at: DateTime<Utc>,
    external_changed: Mutex<HashSet<String>>,
    pending_paths: Arc<Mutex<HashSet<PathBuf>>>,
    needs_reconcile: Arc<AtomicBool>,
    reconcile_lock: Mutex<()>,
    last_reconcile: Mutex<Instant>,
    internal_writes: Arc<Mutex<HashMap<PathBuf, Instant>>>,
    mutations: Mutex<VecDeque<MutationRecord>>,
    journal_file: Mutex<Option<fs::File>>,
    journal_path: PathBuf,
    bash: BashSupervisor,
    run_generations: Arc<Mutex<HashMap<String, RunBaseline>>>,
    run_completions: Arc<Mutex<HashMap<String, RunCompletion>>>,
    write_lock: Arc<tokio::sync::Mutex<()>>,
    open_diagnostics: Value,
    _watcher: Mutex<RecommendedWatcher>,
}

#[derive(Debug, Clone)]
struct RunBaseline {
    generation: u64,
    dirty_files: HashSet<String>,
    completion: Option<RunCompletion>,
    frozen: Option<FrozenRunAttribution>,
}

#[derive(Debug, Clone)]
struct RunCompletion {
    generation: u64,
    ended_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct FrozenRunAttribution {
    generation: u64,
    changed: ChangedPathSummary,
}

impl RunBaseline {
    fn new(generation: u64, dirty_files: HashSet<String>) -> Self {
        Self {
            generation,
            dirty_files,
            completion: None,
            frozen: None,
        }
    }
}

impl WorkspaceActor {
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
        let journal_started = Instant::now();
        let journal_path = workspace_cache.join("mutations.jsonl");
        rotate_journal_if_needed(&journal_path)?;
        let mutations = load_journal(&journal_path);
        let persisted_generation = mutations
            .iter()
            .map(|mutation| mutation.generation)
            .max()
            .unwrap_or(1);
        generation.store(persisted_generation.max(1), Ordering::Release);
        let journal_file = open_journal(&journal_path)?;
        let run_completions = Arc::new(Mutex::new(HashMap::new()));
        let completion_generation = generation.clone();
        let completion_store = run_completions.clone();
        let bash = BashSupervisor::new(workspace_cache, policy.clone())?;
        bash.set_completion_observer(Arc::new(move |run_id, ended_at| {
            completion_store.lock().insert(
                run_id.to_owned(),
                RunCompletion {
                    generation: completion_generation.load(Ordering::Acquire),
                    ended_at,
                },
            );
        }));
        let journal_ms = journal_started.elapsed().as_millis();
        let open_diagnostics = json!({
            "cache_hit": index_cache_hit,
            "total_ms": opened_started.elapsed().as_millis(),
            "phases_ms": {
                "canonicalize": canonicalize_ms,
                "git": git_ms,
                "index": index_ms,
                "watcher": watcher_ms,
            "journal_and_bash": journal_ms
            }
        });
        Ok(Self {
            id: config.id.clone(),
            name: config.name.clone(),
            root,
            policy,
            artifact_paths: config.artifact_paths.clone(),
            exclusions,
            index: RwLock::new(index),
            generation,
            snapshot_id: RwLock::new(snapshot_id),
            repository,
            repo_status: RwLock::new(repo_status),
            repo_status_stale: AtomicBool::new(false),
            opened_dirty_summary: summarize_changed_paths(opened_dirty),
            opened_at: Utc::now(),
            external_changed: Mutex::new(HashSet::new()),
            pending_paths,
            needs_reconcile,
            reconcile_lock: Mutex::new(()),
            last_reconcile: Mutex::new(Instant::now()),
            internal_writes,
            mutations: Mutex::new(mutations),
            journal_file: Mutex::new(Some(journal_file)),
            journal_path,
            bash,
            run_generations: Arc::new(Mutex::new(HashMap::new())),
            run_completions,
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
            let mut known = self.external_changed.lock();
            let mut records = Vec::new();
            for path in external {
                known.insert(path.clone());
                let after_hash = self.index.read().get(&path).map(|file| file.hash.clone());
                records.push(MutationRecord {
                    mutation_id: MutationRecord::new_id(),
                    session_id: "external".to_owned(),
                    path,
                    before_hash: None,
                    after_hash,
                    source: "external".to_owned(),
                    request_id: "watcher".to_owned(),
                    timestamp: Utc::now(),
                    generation,
                });
            }
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
        *self.snapshot_id.write() = self.index.write().snapshot_id(&head);
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

    pub fn code_capabilities(&self) -> AppResult<Value> {
        let bash = self.bash.readiness();
        let bash_available = bash.is_ready();
        Ok(json!({
            "workspace_id": self.id,
            "root": self.root,
            "generation": self.generation(),
            "snapshot_id": self.snapshot(),
            "retrieval": {
                "tool": "code_retrieve",
                "max_operations": 12,
                "operations": ["find_file", "find_symbol", "search_text", "find_references", "symbols_overview", "repo_map", "read"],
                "read_targets": ["path", "handle", "symbol", "metadata", "bash_status", "bash_log", "continuation"],
                "text_syntax": ["literal", "regex"],
                "reference_scopes": ["all", "production", "tests"],
                "reference_kinds": ["declaration", "call", "import", "type", "read", "write", "other"],
                "supports_qualified_symbols": true,
                "ambiguous_symbols_return_candidates": true,
                "supports_snapshot_precondition": true,
                "malformed_operations_return_item_errors": true
            },
            "editing": {
                "supports_preview": true,
                "supports_transaction": true,
                "supports_single_file_wrappers": true,
                "supports_handle_range_replace": true,
                "handle_edits_must_be_only_change_for_file": true,
                "full_line_replacements_preserve_terminal_line_ending": true,
                "supports_bash_validation_commands": bash_available,
                "supports_rollback_on_failure": true,
                "detached_validation_requires_rollback_disabled": true
            },
            "change_contracts": {
                "create": {"required": ["kind", "path", "content"], "optional": ["overwrite", "expected_hash"]},
                "replace": {"required": ["kind", "path", "old_text", "new_text"], "optional": ["expected_replacements", "expected_hash", "handle"]},
                "replace_range": {"required": ["kind", "path", "handle", "new_text"], "optional": []},
                "insert": {"required": ["kind", "path", "content", "anchor_symbol", "position"], "optional": ["expected_hash"]},
                "delete": {"required": ["kind", "path"], "optional": ["expected_hash"]},
                "rename": {"required": ["kind", "path", "to"], "optional": ["expected_hash"]}
            },
            "execution": {
                "bash": bash
            },
            "limits": {
                "max_file_bytes": self.policy.max_file_bytes,
                "max_context_chars": self.policy.max_context_chars,
                "max_search_results": self.policy.max_search_results,
                "protocol_max_search_results": PROTOCOL_MAX_RESULTS,
                "max_bash_output_chars": self.policy.bash.max_output_chars,
                "max_bash_timeout_ms": self.policy.bash.max_timeout_ms
            },
            "known_limitations": [
                "find_references uses indexed lexical or syntactic evidence; use code_intelligence when semantic resolution is required",
                "include_imports returns lexical import prelude only, not inferred dependency usage",
                "hosted connector lazy-loading behavior is outside the server-side MCP list_tools contract"
            ]
        }))
    }
    pub fn summary(&self, session_id: &str, stateless_session: bool) -> AppResult<Value> {
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
            .filter(|item| {
                item.session_id == session_id
                    && item.source == "mcp_edit"
                    && item.timestamp >= self.opened_at
                    && dirty_set.contains(&item.path)
            })
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
        } else if self.policy.bash.enabled {
            "Bash execution is enabled but no usable Bash implementation passed readiness checks. Fix policy.bash.executable or install Git Bash/MSYS2/Cygwin Bash; WSL is used only when explicitly configured and ready."
        } else {
            "Bash execution is disabled. Set policy.bash.enabled to true and restart CodeWeave to use bash and write-tool validation commands."
        };
        let warnings = if bash_available {
            Vec::<String>::new()
        } else if self.policy.bash.enabled {
            vec![format!(
                "Bash execution and write-tool validation commands are unavailable: {}",
                bash.failure_reason
                    .as_deref()
                    .unwrap_or("No usable Bash implementation found")
            )]
        } else {
            vec!["Bash execution and write-tool validation commands are unavailable until policy.bash.enabled is true and CodeWeave is restarted.".to_owned()]
        };
        let mut warnings = warnings;
        if stateless_session {
            warnings.push("Stateless HTTP requests share one legacy workspace key; enable server.statefulMode for isolated chat sessions.".to_owned());
        }
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
            "tool_guidance": format!("This MCP session has one active repository. Context and edits read cached state; call workspace refresh only after suspected missed external changes. {validation_guidance}")
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

    pub fn refresh(
        &self,
        force: bool,
        session_id: &str,
        stateless_session: bool,
    ) -> AppResult<Value> {
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
        self.summary(session_id, stateless_session)
    }

    pub(super) fn search_index(&self, params: &Value) -> AppResult<Value> {
        let started = Instant::now();
        let reconcile_pending = self.read_reconcile_pending();
        let mode = params
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("literal");
        let queries = if let Some(values) = params.get("queries").and_then(Value::as_array) {
            values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .ok_or_else(|| AppError::invalid("queries must contain strings"))
                })
                .collect::<AppResult<Vec<_>>>()?
        } else {
            vec![params
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()]
        };
        if queries.is_empty() {
            return Err(AppError::invalid("queries cannot be empty"));
        }
        let paths = string_list(params, "paths");
        let reference_scope = params
            .get("reference_scope")
            .and_then(Value::as_str)
            .unwrap_or("all");
        if !matches!(reference_scope, "all" | "production" | "tests") {
            return Err(AppError::invalid(
                "reference_scope must be all, production, or tests",
            ));
        }
        let reference_kinds = string_list(params, "reference_kinds");
        if reference_kinds.iter().any(|kind| {
            !matches!(
                kind.as_str(),
                "declaration" | "call" | "import" | "type" | "read" | "write" | "other"
            )
        }) {
            return Err(AppError::invalid(
                "reference_kinds contains an unsupported kind",
            ));
        }
        let definition_path = params.get("definition_path").and_then(Value::as_str);
        let definition_line = params
            .get("definition_line")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        let (requested_results, applied_results, limit_warnings) =
            result_limit(params, 20, self.policy.max_search_results);
        let snapshot = self.snapshot();
        let index = self.index.read();
        if mode == "outline" && queries.len() == 1 && queries[0].is_empty() && paths.len() > 1 {
            let search_started = Instant::now();
            let mut results = Vec::new();
            let mut errors = Vec::new();
            for (index_number, path) in paths.iter().enumerate() {
                match index.search(SearchParams {
                    workspace_id: &self.id,
                    snapshot_id: &snapshot,
                    mode,
                    query: path,
                    path_filters: &[],
                    case_sensitive: bool_value(params, "case_sensitive", false),
                    max_results: applied_results,
                    context_lines: usize_value(params, "context_lines", 2).min(20),
                    reference_scope,
                    reference_kinds: &reference_kinds,
                    definition_path,
                    definition_line,
                }) {
                    Ok(result) => results.push(result),
                    Err(error) => errors.push(json!({
                        "index": index_number,
                        "path": path,
                        "error": error.0
                    })),
                }
            }
            let mut result = json!({
                "mode": mode,
                "snapshot_id": snapshot,
                "result_count": results.len(),
                "error_count": errors.len(),
                "partial_success": !results.is_empty() && !errors.is_empty(),
                "results": results,
                "errors": errors,
            });
            add_reconcile_pending(&mut result, reconcile_pending);
            add_result_limits(
                &mut result,
                requested_results,
                applied_results,
                self.policy.max_search_results,
                20,
                limit_warnings,
            );
            add_phase_metrics(
                &mut result,
                &[
                    ("index_search", search_started.elapsed().as_millis()),
                    ("total_local", started.elapsed().as_millis()),
                ],
            );
            return Ok(result);
        }
        let run_search = |query: &str| {
            let effective_query = if mode == "outline" && query.is_empty() {
                if paths.len() == 1 {
                    paths[0].as_str()
                } else {
                    return Err(AppError::details(
                        "INVALID_OUTLINE_PATH",
                        "Outline requires a file path in query or exactly one paths entry",
                        json!({
                            "paths_count": paths.len(),
                            "retryable": true,
                            "retry_kind": "retry_with_changes",
                            "suggested_calls": paths.iter().map(|path| json!({
                                "mode": "outline",
                                "paths": [path]
                            })).collect::<Vec<_>>()
                        }),
                    ));
                }
            } else {
                query
            };
            index.search(SearchParams {
                workspace_id: &self.id,
                snapshot_id: &snapshot,
                mode,
                query: effective_query,
                path_filters: &paths,
                case_sensitive: bool_value(params, "case_sensitive", false),
                max_results: applied_results,
                context_lines: usize_value(params, "context_lines", 2).min(20),
                reference_scope,
                reference_kinds: &reference_kinds,
                definition_path,
                definition_line,
            })
        };
        if queries.len() == 1 {
            let search_started = Instant::now();
            let mut result = run_search(queries[0])?;
            add_reconcile_pending(&mut result, reconcile_pending);
            add_result_limits(
                &mut result,
                requested_results,
                applied_results,
                self.policy.max_search_results,
                20,
                limit_warnings,
            );
            add_phase_metrics(
                &mut result,
                &[
                    ("index_search", search_started.elapsed().as_millis()),
                    ("total_local", started.elapsed().as_millis()),
                ],
            );
            return Ok(result);
        }
        let mut results = Vec::new();
        let mut errors = Vec::new();
        let search_started = Instant::now();
        for query in &queries {
            match run_search(query) {
                Ok(result) => results.push(json!({"query": query, "result": result})),
                Err(error) => errors.push(json!({"query": query, "error": error.0})),
            }
        }
        let mut result = json!({
            "mode": mode,
            "snapshot_id": snapshot,
            "query_count": queries.len(),
            "result_count": results.len(),
            "error_count": errors.len(),
            "partial_success": !results.is_empty() && !errors.is_empty(),
            "results": results,
            "errors": errors,
        });
        add_reconcile_pending(&mut result, reconcile_pending);
        add_result_limits(
            &mut result,
            requested_results,
            applied_results,
            self.policy.max_search_results,
            20,
            limit_warnings,
        );
        add_phase_metrics(
            &mut result,
            &[
                ("index_search", search_started.elapsed().as_millis()),
                ("total_local", started.elapsed().as_millis()),
            ],
        );
        Ok(result)
    }

    pub fn changes(&self, session_id: &str, params: &Value) -> AppResult<Value> {
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
            .filter(|item| item.session_id == session_id)
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

    pub async fn run(self: &Arc<Self>, session_id: &str, params: &Value) -> AppResult<Value> {
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
                    .start_for_session(
                        &self.root,
                        session_id,
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
                    let retained = self.bash.retained_run_ids();
                    let mut generations = self.run_generations.lock();
                    generations.retain(|known_run, _| retained.contains(known_run));
                    self.run_completions
                        .lock()
                        .retain(|known_run, _| retained.contains(known_run));
                    let completion = self.run_completions.lock().remove(run_id);
                    let mut baseline = RunBaseline::new(before, before_dirty);
                    baseline.completion = completion;
                    generations.insert(run_id.to_owned(), baseline);
                }
                value
            }
            "status" => {
                let actor = Arc::clone(self);
                let session_id = session_id.to_owned();
                let run_id = required_str(params, "run_id")?.to_owned();
                tokio::task::spawn_blocking(move || {
                    actor.bash.status_for_session(&session_id, &run_id)
                })
                .await
                .map_err(AppError::internal)??
            }
            "output" => {
                let actor = Arc::clone(self);
                let session_id = session_id.to_owned();
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
                    actor.bash.output_stream_for_session(
                        &session_id,
                        &run_id,
                        continuation.as_deref(),
                        stream.as_deref(),
                    )
                })
                .await
                .map_err(AppError::internal)??
            }
            "cancel" => {
                let actor = Arc::clone(self);
                let session_id = session_id.to_owned();
                let run_id = required_str(params, "run_id")?.to_owned();
                tokio::task::spawn_blocking(move || {
                    actor.bash.cancel_for_session(&session_id, &run_id)
                })
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
            let result_ended_at = run_result_ended_at(&result);
            let (start_generation, attribution_generation, changed) = {
                let completion = self.run_completions.lock().remove(&run_id);
                let mut generations = self.run_generations.lock();
                let baseline = generations
                    .entry(run_id.clone())
                    .or_insert_with(|| RunBaseline::new(self.generation(), current_dirty.clone()));
                if baseline.completion.is_none() {
                    baseline.completion = completion;
                }
                if terminal && baseline.completion.is_none() {
                    if let Some(ended_at) = result_ended_at {
                        baseline.completion = Some(RunCompletion {
                            generation: self.generation(),
                            ended_at,
                        });
                    }
                }
                if terminal {
                    if baseline.frozen.is_none() {
                        let completion =
                            baseline
                                .completion
                                .clone()
                                .unwrap_or_else(|| RunCompletion {
                                    generation: self.generation(),
                                    ended_at: result_ended_at.unwrap_or_else(Utc::now),
                                });
                        let baseline_snapshot = baseline.clone();
                        let paths = self.observed_run_changed_paths(
                            &baseline_snapshot,
                            completion.generation,
                            Some(&completion.ended_at),
                            &current_dirty,
                        );
                        baseline.frozen = Some(FrozenRunAttribution {
                            generation: completion.generation,
                            changed: summarize_changed_paths(paths),
                        });
                    }
                    let frozen = baseline
                        .frozen
                        .clone()
                        .expect("terminal run attribution is frozen");
                    (baseline.generation, frozen.generation, frozen.changed)
                } else {
                    let paths = self.observed_run_changed_paths(
                        baseline,
                        self.generation(),
                        None,
                        &current_dirty,
                    );
                    (
                        baseline.generation,
                        self.generation(),
                        summarize_changed_paths(paths),
                    )
                }
            };
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
        baseline: &RunBaseline,
        end_generation: u64,
        ended_at: Option<&DateTime<Utc>>,
        current_dirty: &HashSet<String>,
    ) -> HashSet<String> {
        let mutation_paths: HashSet<String> = self
            .mutations
            .lock()
            .iter()
            .filter(|mutation| {
                mutation.generation > baseline.generation
                    && mutation.generation <= end_generation
                    && ended_at
                        .map(|ended| mutation.timestamp <= *ended)
                        .unwrap_or(true)
            })
            .map(|mutation| mutation.path.clone())
            .collect();
        let mut paths: HashSet<String> = current_dirty
            .symmetric_difference(&baseline.dirty_files)
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

fn run_result_ended_at(result: &Value) -> Option<DateTime<Utc>> {
    result
        .get("ended_at")
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

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

fn char_boundary_at_or_before(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests;
