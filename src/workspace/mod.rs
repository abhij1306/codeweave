mod edit;
mod fetch;
mod io_helpers;
mod journal;
mod util;

pub use journal::MutationRecord;
use journal::{load_journal, rotate_journal_if_needed};
use util::{stale_snapshot, summarize_changed_paths};

use crate::index::{content_hash, ignored_workspace_path, CodeIndex, ContextParams, SearchParams};
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
use std::fs::{self, OpenOptions};
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
    index: RwLock<CodeIndex>,
    generation: Arc<AtomicU64>,
    snapshot_id: RwLock<String>,
    repository: Arc<dyn RepositoryBackend>,
    repo_status: RwLock<RepoStatus>,
    opened_dirty_summary: (Vec<String>, usize, bool),
    opened_at: DateTime<Utc>,
    session_id: String,
    external_changed: Mutex<HashSet<String>>,
    pending_paths: Arc<Mutex<HashSet<PathBuf>>>,
    needs_reconcile: Arc<AtomicBool>,
    reconcile_lock: Mutex<()>,
    internal_writes: Arc<Mutex<HashMap<PathBuf, Instant>>>,
    mutations: Mutex<VecDeque<MutationRecord>>,
    journal_file: Mutex<fs::File>,
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

        let phase_started = Instant::now();
        let repository: Arc<dyn RepositoryBackend> = Arc::new(CliGitBackend);
        let repo_status = repository.status(&root).unwrap_or_default();
        let git_ms = phase_started.elapsed().as_millis();
        let opened_dirty: HashSet<String> = repo_status.dirty_files.iter().cloned().collect();

        let phase_started = Instant::now();
        let index_cache = workspace_cache.join("index.json");
        let (mut index, index_cache_hit) = CodeIndex::scan_cached(
            &root,
            policy.max_file_bytes,
            &config.artifact_paths,
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
                    if relative.is_empty() || ignored_workspace_path(&relative) || is_temp {
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
        let session_id = format!("session_{}", Uuid::new_v4().simple());
        let journal_path = workspace_cache.join("mutations.jsonl");
        rotate_journal_if_needed(&journal_path)?;
        let mutations = load_journal(&journal_path);
        let journal_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&journal_path)?;
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
            index: RwLock::new(index),
            generation,
            snapshot_id: RwLock::new(snapshot_id),
            repository,
            repo_status: RwLock::new(repo_status),
            opened_dirty_summary: summarize_changed_paths(opened_dirty),
            opened_at: Utc::now(),
            session_id,
            external_changed: Mutex::new(HashSet::new()),
            pending_paths,
            needs_reconcile,
            reconcile_lock: Mutex::new(()),
            internal_writes,
            mutations: Mutex::new(mutations),
            journal_file: Mutex::new(journal_file),
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
                    || ignored_workspace_path(&relative)
                    || is_temp
                    || path.is_dir()
                {
                    continue;
                }
                if !internal.contains_key(&path) {
                    external_candidates.insert(relative);
                }
                relevant.insert(path);
            }
        }

        let changed = if relevant.is_empty() {
            Vec::new()
        } else {
            self.index
                .write()
                .refresh_paths(&self.root, &relevant, self.policy.max_file_bytes)?
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
                    session_id: self.session_id.clone(),
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

    pub fn summary(&self) -> AppResult<Value> {
        self.reconcile_pending()?;
        let index = self.index.read();
        let repo = self.repo_status.read().clone();
        let mcp_paths: HashSet<String> = self
            .mutations
            .lock()
            .iter()
            .filter(|item| {
                item.session_id == self.session_id
                    && item.source == "mcp_edit"
                    && item.timestamp >= self.opened_at
            })
            .map(|item| item.path.clone())
            .collect();
        let external = self.external_changed.lock().clone();
        let (ref preexisting_paths, preexisting_count, preexisting_truncated) =
            self.opened_dirty_summary;
        let (mcp_changed_paths, mcp_changed_count, mcp_changed_truncated) =
            summarize_changed_paths(mcp_paths);
        let (external_paths, external_count, external_truncated) =
            summarize_changed_paths(external);
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
        Ok(json!({
            "workspace_id": self.id, "name": self.name, "root": self.root, "generation": self.generation(), "snapshot_id": self.snapshot(),
            "file_count": index.file_count(), "languages": index.languages(), "repository": repo, "instructions": instructions,
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
                "preexisting_at_open": preexisting_paths,
                "changed_by_mcp": mcp_changed_paths,
                "observed_external": external_paths,
                "counts": {
                    "preexisting_at_open": preexisting_count,
                    "changed_by_mcp": mcp_changed_count,
                    "observed_external": external_count
                },
                "truncated": {
                    "preexisting_at_open": preexisting_truncated,
                    "changed_by_mcp": mcp_changed_truncated,
                    "observed_external": external_truncated
                }
            },
            "tool_guidance": format!("This server process has one active repository. Context and edits read cached state; call workspace refresh only after suspected missed external changes. {validation_guidance}")
        }))
    }

    pub fn refresh(&self, force: bool) -> AppResult<Value> {
        if force {
            let _guard = self.reconcile_lock.lock();
            self.pending_paths.lock().clear();
            self.needs_reconcile.store(false, Ordering::Release);
            *self.index.write() =
                CodeIndex::scan(&self.root, self.policy.max_file_bytes, &self.artifact_paths)?;
            *self.repo_status.write() = self.repository.status(&self.root).unwrap_or_default();
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.recompute_snapshot();
        } else {
            self.reconcile_pending()?;
        }
        self.summary()
    }

    pub fn code_context(&self, params: &Value) -> AppResult<Value> {
        self.reconcile_pending()?;
        let query = required_str(params, "query")?;
        if let Some(expected) = params.get("snapshot_id").and_then(Value::as_str) {
            if expected != self.snapshot() {
                return Err(stale_snapshot(expected, &self.snapshot()));
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
        let task_failures = self.tasks.recent_failures(query, 3);
        if let Some(object) = result.as_object_mut() {
            object.insert("recent_task_failures".to_owned(), json!(task_failures));
        }
        Ok(result)
    }

    pub fn code_search(&self, params: &Value) -> AppResult<Value> {
        self.reconcile_pending()?;
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
            return run_search(queries[0]);
        }
        let mut results = Vec::new();
        let mut errors = Vec::new();
        for query in &queries {
            match run_search(query) {
                Ok(result) => results.push(json!({"query": query, "result": result})),
                Err(error) => errors.push(json!({"query": query, "error": error.0})),
            }
        }
        Ok(json!({
            "mode": mode,
            "snapshot_id": snapshot,
            "query_count": queries.len(),
            "result_count": results.len(),
            "error_count": errors.len(),
            "partial_success": !results.is_empty() && !errors.is_empty(),
            "results": results,
            "errors": errors,
        }))
    }

    pub fn changes(&self, params: &Value) -> AppResult<Value> {
        self.reconcile_pending()?;
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
            .filter(|item| item.session_id == self.session_id)
            .filter(|item| item.generation > since)
            .filter(|item| source.map(|value| item.source == value).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect();
        Ok(
            json!({"workspace_id": self.id, "generation": self.generation(), "snapshot_id": self.snapshot(), "mutations": records}),
        )
    }

    pub fn git(&self, params: &Value) -> AppResult<Value> {
        self.reconcile_pending()?;
        let action = required_str(params, "action")?;
        let paths = string_list(params, "paths");
        for path in &paths {
            validate_relative(path)?;
        }
        let staged = bool_value(params, "staged", false);
        let result = match action {
            "status" => {
                let status = self.repository.status(&self.root)?;
                *self.repo_status.write() = status.clone();
                self.recompute_snapshot();
                return Ok(
                    json!({"action": action, "status": status, "generation": self.generation(), "snapshot_id": self.snapshot()}),
                );
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
                let _ = self.refresh(true)?;
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
        if matches!(action, "stage" | "commit") {
            *self.repo_status.write() = self.repository.status(&self.root).unwrap_or_default();
            self.recompute_snapshot();
        }
        Ok(
            json!({"action": action, "output": result, "generation": self.generation(), "snapshot_id": self.snapshot()}),
        )
    }

    pub async fn run(self: &Arc<Self>, params: &Value) -> AppResult<Value> {
        self.reconcile_pending_async().await?;
        let action = params
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("start");
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
                            background: bool_value(params, "background", false),
                            timeout_ms: params.get("timeout_ms").and_then(Value::as_u64),
                        },
                    )
                    .await?;
                if let Some(task_id) = value.get("task_id").and_then(Value::as_str) {
                    self.task_generations
                        .lock()
                        .insert(task_id.to_owned(), before);
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
                tokio::task::spawn_blocking(move || {
                    actor.tasks.output(&task_id, continuation.as_deref())
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
        self.reconcile_pending_async().await?;
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
                .filter(|item| item.session_id == self.session_id && item.generation > start)
                .map(|item| item.path.clone())
                .collect();
            let (changed_paths, changed_path_count, changed_paths_truncated) =
                summarize_changed_paths(paths);
            if let Some(object) = result.as_object_mut() {
                object.insert("workspace_generation_before".to_owned(), json!(start));
                object.insert(
                    "workspace_generation_after".to_owned(),
                    json!(self.generation()),
                );
                object.insert("observed_changed_paths".to_owned(), json!(changed_paths));
                object.insert(
                    "observed_changed_path_count".to_owned(),
                    json!(changed_path_count),
                );
                object.insert(
                    "observed_changed_paths_truncated".to_owned(),
                    json!(changed_paths_truncated),
                );
                object.insert(
                    "task_profiles".to_owned(),
                    json!(self.tasks.profile_names()),
                );
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
