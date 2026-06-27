mod edit;
mod fetch;
mod io_helpers;
mod journal;
mod util;

pub use journal::MutationRecord;
use journal::{load_journal, open_journal, rotate_journal_if_needed};
use util::{stale_snapshot, summarize_changed_paths, ChangedPathSummary};

use crate::index::{content_hash, CodeIndex, ContextParams, SearchParams, WorkspaceExclusions};
use crate::model::{
    bool_value, required_str, string_list, usize_value, AppError, AppResult, PolicyConfig,
    TaskProfile, WorkspaceConfig,
};
use crate::repository::{CliGitBackend, RepoStatus, RepositoryBackend};
use crate::security::{canonical_root, relative_string, validate_relative};
use crate::tasks::{StartRequest, TaskSupervisor};
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
use uuid::Uuid;

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
    opened_dirty_summary: ChangedPathSummary,
    opened_at: DateTime<Utc>,
    external_changed: Mutex<HashSet<String>>,
    pending_paths: Arc<Mutex<HashSet<PathBuf>>>,
    needs_reconcile: Arc<AtomicBool>,
    reconcile_lock: Mutex<()>,
    internal_writes: Arc<Mutex<HashMap<PathBuf, Instant>>>,
    mutations: Mutex<VecDeque<MutationRecord>>,
    journal_file: Mutex<Option<fs::File>>,
    journal_path: PathBuf,
    tasks: TaskSupervisor,
    task_generations: Mutex<HashMap<String, u64>>,
    write_lock: Arc<tokio::sync::Mutex<()>>,
    open_diagnostics: Value,
    _watcher: Mutex<RecommendedWatcher>,
}

impl WorkspaceActor {
    pub fn root_path(&self) -> &Path {
        &self.root
    }

    pub fn running_task_count(&self) -> usize {
        self.tasks.running_count()
    }

    pub fn open(
        config: &WorkspaceConfig,
        policy: PolicyConfig,
        tasks: HashMap<String, TaskProfile>,
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
        let journal_file = open_journal(&journal_path)?;
        let tasks = TaskSupervisor::new(workspace_cache, policy.clone(), tasks)?;
        let journal_ms = journal_started.elapsed().as_millis();
        let open_diagnostics = json!({
            "cache_hit": index_cache_hit,
            "total_ms": opened_started.elapsed().as_millis(),
            "phases_ms": {
                "canonicalize": canonicalize_ms,
                "git": git_ms,
                "index": index_ms,
                "watcher": watcher_ms,
                "journal_and_tasks": journal_ms
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
            opened_dirty_summary: summarize_changed_paths(opened_dirty),
            opened_at: Utc::now(),
            external_changed: Mutex::new(HashSet::new()),
            pending_paths,
            needs_reconcile,
            reconcile_lock: Mutex::new(()),
            internal_writes,
            mutations: Mutex::new(mutations),
            journal_file: Mutex::new(Some(journal_file)),
            journal_path,
            tasks,
            task_generations: Mutex::new(HashMap::new()),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            open_diagnostics,
            _watcher: Mutex::new(watcher),
        })
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
                    mutation_id: format!("mut_{}", Uuid::new_v4().simple()),
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
        tokio::task::spawn_blocking(move || actor.reconcile_pending())
            .await
            .map_err(AppError::internal)?
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

    pub fn summary(&self, session_id: &str, stateless_session: bool) -> AppResult<Value> {
        let started = Instant::now();
        let reconcile_started = Instant::now();
        self.reconcile_pending()?;
        let reconcile_ms = reconcile_started.elapsed().as_millis();
        let index = self.index.read();
        let repo = self.repo_status.read().clone();
        let mcp_paths: HashSet<String> = self
            .mutations
            .lock()
            .iter()
            .filter(|item| {
                item.session_id == session_id
                    && item.source == "mcp_edit"
                    && item.timestamp >= self.opened_at
            })
            .map(|item| item.path.clone())
            .collect();
        let external = self.external_changed.lock().clone();
        let preexisting = &self.opened_dirty_summary;
        let mcp_changed = summarize_changed_paths(mcp_paths);
        let external = summarize_changed_paths(external);
        let repository_dirty = summarize_changed_paths(
            repo.dirty_files
                .iter()
                .filter(|path| !self.exclusions.is_ignored(Path::new(path), false))
                .cloned()
                .collect(),
        );
        let repository = json!({
            "is_git": repo.is_git,
            "head": repo.head,
            "branch": repo.branch,
            "dirty_files": repository_dirty.paths,
            "dirty_file_count": repository_dirty.count,
            "dirty_files_truncated": repository_dirty.truncated,
            "dirty_file_groups": repository_dirty.groups
        });
        let instructions = ["AGENTS.md", "CLAUDE.md"]
            .into_iter()
            .filter_map(|path| {
                index
                    .get(path)
                    .map(|file| json!({"path": path, "content": file.content}))
            })
            .collect::<Vec<_>>();
        let task_profiles = self.tasks.profile_names();
        let profile_validation_available = !task_profiles.is_empty();
        let raw_commands_available = !self.policy.allowed_commands.is_empty();
        let validation_guidance = if profile_validation_available {
            "Write-tool validate fields accept configured task profile names only. Use run(profile='<name>') for standalone profile execution."
        } else {
            "No validation profiles are configured. Omit validate on write tools and call run(action='start', command=[...]) with a policy-allowed command after applying the edit."
        };
        let warnings = if profile_validation_available {
            Vec::<String>::new()
        } else {
            vec!["No task profiles are configured; profile-based write validation is unavailable, but policy-allowed raw commands remain available through run(command=[...]).".to_owned()]
        };
        let mut warnings = warnings;
        if stateless_session {
            warnings.push("Stateless HTTP requests share one legacy workspace key; enable server.statefulMode for isolated chat sessions.".to_owned());
        }
        let mut result = json!({
            "workspace_id": self.id, "name": self.name, "root": self.root, "generation": self.generation(), "snapshot_id": self.snapshot(),
            "file_count": index.file_count(), "languages": index.languages(), "repository": repository, "instructions": instructions,
            "task_profiles": task_profiles,
            "capabilities": {
                "profile_validation_available": profile_validation_available,
                "raw_commands_available": raw_commands_available,
                "allowed_commands": self.policy.allowed_commands,
                "validation_guidance": validation_guidance
            },
            "warnings": warnings,
            "open_diagnostics": self.open_diagnostics,
            "dirty_ownership": {
                "preexisting_at_open": preexisting.paths,
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
                    "preexisting_at_open": preexisting.groups,
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
            *self.repo_status.write() = self.repository.status(&self.root).unwrap_or_default();
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.recompute_snapshot();
        } else {
            self.reconcile_pending()?;
        }
        self.summary(session_id, stateless_session)
    }

    pub fn code_context(&self, params: &Value) -> AppResult<Value> {
        let started = Instant::now();
        let reconcile_pending = self.read_reconcile_pending();
        let query = required_str(params, "query")?;
        if let Some(expected) = params.get("snapshot_id").and_then(Value::as_str) {
            let current = self.snapshot();
            if expected != current {
                return Err(stale_snapshot(expected, &current));
            }
        }
        let budget = match params
            .get("budget")
            .and_then(Value::as_str)
            .unwrap_or("medium")
        {
            "small" => 8_000,
            "large" => self.policy.max_context_chars,
            _ => 20_000.min(self.policy.max_context_chars),
        };
        let paths = string_list(params, "paths");
        let evidence = string_list(params, "evidence");
        let max_results = usize_value(params, "max_results", 10).min(50);
        let mut dirty: HashSet<String> = self
            .repo_status
            .read()
            .dirty_files
            .iter()
            .cloned()
            .collect();
        let external = self.external_changed.lock().clone();
        dirty.extend(external.iter().cloned());
        let recent: HashSet<String> = self
            .mutations
            .lock()
            .iter()
            .rev()
            .take(100)
            .filter(|item| item.source != "external" || !external.contains(&item.path))
            .map(|item| item.path.clone())
            .collect();
        let snapshot = self.snapshot();
        let index_started = Instant::now();
        let mut result = self.index.read().context(ContextParams {
            workspace_id: &self.id,
            snapshot_id: &snapshot,
            query,
            path_filters: &paths,
            evidence: &evidence,
            dirty: &dirty,
            recent_mutations: &recent,
            budget_chars: budget,
            max_results,
        })?;
        let index_ms = index_started.elapsed().as_millis();
        let task_failures = self.tasks.recent_failures(query, 3);
        if let Some(object) = result.as_object_mut() {
            object.insert("recent_task_failures".to_owned(), json!(task_failures));
            object.insert("reconcile_pending".to_owned(), json!(reconcile_pending));
        }
        add_phase_metrics(
            &mut result,
            &[
                ("index_context", index_ms),
                ("total_local", started.elapsed().as_millis()),
            ],
        );
        Ok(result)
    }

    pub fn code_search(&self, params: &Value) -> AppResult<Value> {
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
        let snapshot = self.snapshot();
        let index = self.index.read();
        let run_search = |query: &str| {
            let effective_query = if mode == "outline" && query.is_empty() {
                if paths.len() == 1 {
                    paths[0].as_str()
                } else {
                    return Err(AppError::details(
                        "INVALID_OUTLINE_PATH",
                        "Outline requires a file path in query or exactly one paths entry",
                        json!({"paths_count": paths.len()}),
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
                max_results: usize_value(params, "max_results", 20)
                    .min(self.policy.max_search_results),
                context_lines: usize_value(params, "context_lines", 2).min(20),
            })
        };
        if queries.len() == 1 {
            let search_started = Instant::now();
            let mut result = run_search(queries[0])?;
            add_reconcile_pending(&mut result, reconcile_pending);
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

    pub fn git(&self, params: &Value) -> AppResult<Value> {
        let started = Instant::now();
        let reconcile_started = Instant::now();
        self.reconcile_pending()?;
        let reconcile_ms = reconcile_started.elapsed().as_millis();
        let action = required_str(params, "action")?;
        let paths = string_list(params, "paths");
        for path in &paths {
            validate_relative(path)?;
        }
        let staged = bool_value(params, "staged", false);
        let git_started = Instant::now();
        let result = match action {
            "status" => {
                let status = self.repository.status(&self.root)?;
                let git_ms = git_started.elapsed().as_millis();
                *self.repo_status.write() = status.clone();
                self.recompute_snapshot();
                let mut result = json!({
                    "action": action,
                    "status": status,
                    "generation": self.generation(),
                    "snapshot_id": self.snapshot()
                });
                add_phase_metrics(
                    &mut result,
                    &[
                        ("reconcile", reconcile_ms),
                        ("git_status", git_ms),
                        ("total_local", started.elapsed().as_millis()),
                    ],
                );
                return Ok(result);
            }
            "diff" => {
                self.repository
                    .diff(&self.root, staged, &paths, self.policy.max_context_chars)?
            }
            "log" => self
                .repository
                .log(&self.root, usize_value(params, "limit", 20).min(200))?,
            "show" => self.repository.show(
                &self.root,
                params.get("ref").and_then(Value::as_str).unwrap_or("HEAD"),
                self.policy.max_context_chars,
            )?,
            "blame" => {
                let path = paths
                    .first()
                    .ok_or_else(|| AppError::invalid("git blame requires one path"))?;
                self.repository.blame(
                    &self.root,
                    path,
                    params
                        .get("start_line")
                        .and_then(Value::as_u64)
                        .map(|v| v as usize),
                    params
                        .get("end_line")
                        .and_then(Value::as_u64)
                        .map(|v| v as usize),
                    self.policy.max_context_chars,
                )?
            }
            "stage" => self.repository.stage(&self.root, &paths)?,
            "commit" => self.repository.commit(
                &self.root,
                params
                    .get("message")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AppError::invalid("git commit requires message"))?,
            )?,
            "restore" => {
                if !bool_value(params, "confirm", false) {
                    return Err(AppError::new(
                        "CONFIRMATION_REQUIRED",
                        "git restore requires confirm=true",
                    ));
                }
                let output = self.repository.restore(&self.root, &paths, staged)?;
                let _ = self.refresh(true, "git", false)?;
                output
            }
            _ => {
                return Err(AppError::details(
                    "INVALID_GIT_ACTION",
                    "Unknown Git action",
                    json!({"action": action}),
                ))
            }
        };
        let git_ms = git_started.elapsed().as_millis();
        if matches!(action, "stage" | "commit") {
            *self.repo_status.write() = self.repository.status(&self.root).unwrap_or_default();
            self.recompute_snapshot();
        }
        let mut response = json!({
            "action": action,
            "output": result,
            "generation": self.generation(),
            "snapshot_id": self.snapshot()
        });
        add_phase_metrics(
            &mut response,
            &[
                ("reconcile", reconcile_ms),
                ("git", git_ms),
                ("total_local", started.elapsed().as_millis()),
            ],
        );
        Ok(response)
    }

    pub async fn run(self: &Arc<Self>, session_id: &str, params: &Value) -> AppResult<Value> {
        let started = Instant::now();
        let reconcile_started = Instant::now();
        self.reconcile_pending_async().await?;
        let reconcile_before_ms = reconcile_started.elapsed().as_millis();
        let action = params
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("start");
        let mut run_startup_ms = None;
        let mut result = match action {
            "start" => {
                let before = self.generation();
                let command = params
                    .get("command")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    });
                let run_started = Instant::now();
                let value = self
                    .tasks
                    .start(
                        &self.root,
                        StartRequest {
                            profile: params
                                .get("profile")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            command,
                            cwd: params.get("cwd").and_then(Value::as_str).map(str::to_owned),
                            shell: bool_value(params, "shell", false),
                            background: params.get("background").and_then(Value::as_bool),
                            timeout_ms: params.get("timeout_ms").and_then(Value::as_u64),
                        },
                    )
                    .await?;
                run_startup_ms = Some(run_started.elapsed().as_millis());
                if let Some(task_id) = value.get("task_id").and_then(Value::as_str) {
                    let retained = self.tasks.retained_task_ids();
                    let mut generations = self.task_generations.lock();
                    generations.retain(|known_task, _| retained.contains(known_task));
                    generations.insert(task_id.to_owned(), before);
                }
                value
            }
            "status" => {
                let actor = Arc::clone(self);
                let task_id = required_str(params, "task_id")?.to_owned();
                tokio::task::spawn_blocking(move || actor.tasks.status(&task_id))
                    .await
                    .map_err(AppError::internal)??
            }
            "output" => {
                let actor = Arc::clone(self);
                let task_id = required_str(params, "task_id")?.to_owned();
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
                        .tasks
                        .output_stream(&task_id, continuation.as_deref(), stream.as_deref())
                })
                .await
                .map_err(AppError::internal)??
            }
            "cancel" => {
                let actor = Arc::clone(self);
                let task_id = required_str(params, "task_id")?.to_owned();
                tokio::task::spawn_blocking(move || actor.tasks.cancel(&task_id))
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
        self.reconcile_pending_async().await?;
        let reconcile_after_ms = reconcile_after_started.elapsed().as_millis();
        if let Some(task_id) = result.get("task_id").and_then(Value::as_str) {
            let start = self
                .task_generations
                .lock()
                .get(task_id)
                .copied()
                .unwrap_or(self.generation());
            let paths: HashSet<String> = self
                .mutations
                .lock()
                .iter()
                .filter(|item| {
                    item.generation > start
                        && (item.session_id == session_id || item.source == "external")
                })
                .map(|item| item.path.clone())
                .collect();
            let changed = summarize_changed_paths(paths);
            if let Some(object) = result.as_object_mut() {
                object.insert("workspace_generation_before".to_owned(), json!(start));
                object.insert(
                    "workspace_generation_after".to_owned(),
                    json!(self.generation()),
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
                object.insert(
                    "task_profiles".to_owned(),
                    json!(self.tasks.profile_names()),
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

#[cfg(test)]
mod tests;
