use crate::model::{required_str, AppError, AppResult, DaemonConfig, WorkspaceConfig};
use crate::security::validate_relative;
use crate::workspace::WorkspaceActor;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

const MAX_CACHED_WORKSPACES: usize = 8;
const MAX_LATENCY_SAMPLES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey(String);

impl SessionKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn stateless() -> Self {
        Self::new("stateless")
    }

    pub fn stdio() -> Self {
        Self::new("stdio")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_stateless(&self) -> bool {
        self.0 == "stateless"
    }
}

impl Default for SessionKey {
    fn default() -> Self {
        Self::stateless()
    }
}

#[derive(Default)]
pub struct WorkspaceManager {
    config: RwLock<Option<DaemonConfig>>,
    sessions: RwLock<HashMap<SessionKey, String>>,
    actors: RwLock<HashMap<String, Arc<WorkspaceActor>>>,
    actor_lru: Mutex<VecDeque<String>>,
    open_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    latency: Mutex<HashMap<String, VecDeque<u128>>>,
    lifecycle: Mutex<()>,
    operation_gate: tokio::sync::RwLock<()>,
}

async fn run_blocking<F>(operation: F) -> AppResult<Value>
where
    F: FnOnce() -> AppResult<Value> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(AppError::internal)?
}

fn validate_skill_name(name: &str) -> AppResult<()> {
    if name.is_empty()
        || name.contains('\0')
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
        || name.ends_with('.')
        || name.ends_with(' ')
    {
        return Err(AppError::invalid("Invalid skill name"));
    }

    let path = validate_relative(name)?;
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(AppError::invalid("Invalid skill name"));
    }

    let base = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let reserved = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || base.strip_prefix("COM").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || base.strip_prefix("LPT").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        });
    if reserved {
        return Err(AppError::invalid("Invalid skill name"));
    }
    Ok(())
}

fn reject_legacy_execution_config(config: &Value) -> AppResult<()> {
    let mut keys = Vec::new();
    if config.get("tasks").is_some() {
        keys.push("tasks");
    }
    if let Some(policy) = config.get("policy").and_then(Value::as_object) {
        for key in [
            "allowedCommands",
            "allowed_commands",
            "shellEnabled",
            "shell_enabled",
            "maxTaskOutputChars",
            "max_task_output_chars",
            "taskRetentionHours",
            "task_retention_hours",
        ] {
            if policy.contains_key(key) {
                keys.push(key);
            }
        }
    }
    if keys.is_empty() {
        return Ok(());
    }
    Err(AppError::details(
        "LEGACY_EXECUTION_CONFIG",
        "Task profile execution configuration is no longer supported; migrate to policy.bash",
        json!({"legacy_keys": keys, "migration_target": "policy.bash"}),
    ))
}

impl WorkspaceManager {
    pub async fn dispatch(
        self: &Arc<Self>,
        session: SessionKey,
        method: &str,
        params: &Value,
    ) -> AppResult<Value> {
        if method == "initialize" {
            let _operation = self.operation_gate.write().await;
            self.dispatch_inner(&session, method, params).await
        } else {
            let _operation = self.operation_gate.read().await;
            self.dispatch_inner(&session, method, params).await
        }
    }

    pub fn close_session(&self, session: &SessionKey) {
        self.sessions.write().remove(session);
        self.evict_idle_actors();
    }

    async fn dispatch_inner(
        self: &Arc<Self>,
        session: &SessionKey,
        method: &str,
        params: &Value,
    ) -> AppResult<Value> {
        let started = Instant::now();
        let mut result = match method {
            "initialize" => {
                let manager = Arc::clone(self);
                let params = params.clone();
                run_blocking(move || manager.initialize(&params)).await
            }
            "health" => self.health(),
            "shutdown" => {
                self.close_session(session);
                Ok(json!({"ok": true}))
            }
            "workspace" => {
                let manager = Arc::clone(self);
                let session = session.clone();
                let params = params.clone();
                run_blocking(move || manager.workspace(&session, &params)).await
            }
            "code_context" => {
                let actor = self.active_actor(session)?;
                let params = params.clone();
                run_blocking(move || actor.code_context(&params)).await
            }
            "code_search" => {
                let actor = self.active_actor(session)?;
                let params = params.clone();
                run_blocking(move || actor.code_search(&params)).await
            }
            "code_fetch" => {
                let actor = self.active_actor(session)?;
                let params = params.clone();
                run_blocking(move || actor.code_fetch(&params)).await
            }
            "code_capabilities" => {
                let actor = self.active_actor(session)?;
                run_blocking(move || actor.code_capabilities()).await
            }
            "code_write" | "code_replace" | "code_replace_range" | "code_insert"
            | "code_delete" | "code_rename" | "code_preview" | "code_transaction" => {
                let actor = self.active_actor(session)?;
                let mut prepared = params.clone();
                if prepared.get("snapshot_id").is_none() {
                    if let Some(snapshot) = actor.summary_ids()?.get("snapshot_id") {
                        prepared["snapshot_id"] = snapshot.clone();
                    }
                }
                actor.code_edit(session.as_str(), &prepared).await
            }
            "git_status" | "git_diff" | "git_log" | "git_show" | "git_blame" | "git_preflight"
            | "git_stage" | "git_commit" | "git_restore" | "git_push" => {
                let actor = self.active_actor(session)?;
                let params = params.clone();
                run_blocking(move || actor.git(&params)).await
            }
            "bash" | "bash_status" | "bash_output" | "bash_cancel" => {
                let action = match method {
                    "bash" => "start",
                    "bash_status" => "status",
                    "bash_output" => "output",
                    "bash_cancel" => "cancel",
                    _ => unreachable!("matched Bash method"),
                };
                let mut prepared = params.clone();
                let allowed_fields: &[&str] = match method {
                    "bash" => &["command", "cwd", "background", "timeout_ms"],
                    "bash_status" | "bash_cancel" => &["run_id"],
                    "bash_output" => &["run_id", "stream", "continuation"],
                    _ => unreachable!("matched Bash method"),
                };
                let has_disallowed_field = match prepared.as_object() {
                    Some(object) => object
                        .keys()
                        .any(|field| !allowed_fields.contains(&field.as_str())),
                    None => true,
                };
                if has_disallowed_field {
                    return Err(AppError::invalid(format!(
                        "{method} received an unknown or spoofed field"
                    )));
                }
                if method == "bash" {
                    let valid_command = prepared
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|command| !command.trim().is_empty());
                    if !valid_command {
                        return Err(AppError::invalid("bash requires a non-empty command"));
                    }
                } else if prepared
                    .get("run_id")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err(AppError::invalid(format!(
                        "{method} requires a non-empty run_id"
                    )));
                }
                prepared["action"] = Value::String(action.to_owned());
                self.active_actor(session)?
                    .run(session.as_str(), &prepared)
                    .await
            }
            _ => Err(AppError::details(
                "METHOD_NOT_FOUND",
                "Unknown daemon method",
                json!({"method": method}),
            )),
        }?;
        let elapsed_ms = started.elapsed().as_millis();
        self.record_latency(method, elapsed_ms);
        add_operation_elapsed(&mut result, elapsed_ms);
        if method == "workspace" {
            add_latency_metrics(&mut result, self.latency_metrics());
        }
        Ok(result)
    }

    fn initialize(&self, params: &Value) -> AppResult<Value> {
        reject_legacy_execution_config(params)?;
        let config: DaemonConfig = serde_json::from_value(params.clone())?;
        for actor in self.actors.read().values() {
            let running_runs = actor.running_bash_count();
            if running_runs > 0 {
                return Err(AppError::details(
                    "WORKSPACE_BUSY",
                    "Cannot reinitialize while Bash runs are active",
                    json!({"running_runs": running_runs}),
                ));
            }
        }
        if config.workspaces.is_empty()
            && config.workspace.default_path.is_none()
            && config.workspace.allowed_roots.is_empty()
        {
            return Err(AppError::invalid(
                "Configure workspace.defaultPath, workspace.allowedRoots, or one legacy workspace",
            ));
        }
        if config.skills.enabled && !config.skills.explicit_only {
            return Err(AppError::invalid(
                "skills.explicitOnly must remain true; automatic skill invocation is not supported",
            ));
        }
        let mut ids = std::collections::HashSet::new();
        for workspace in &config.workspaces {
            if !ids.insert(workspace.id.clone()) {
                return Err(AppError::details(
                    "DUPLICATE_WORKSPACE",
                    "Workspace ids must be unique",
                    json!({"workspace_id": workspace.id}),
                ));
            }
        }
        let _lifecycle = self.lifecycle.lock();
        *self.config.write() = Some(config.clone());
        self.sessions.write().clear();
        self.actors.write().clear();
        self.actor_lru.lock().clear();
        self.open_locks.lock().clear();
        self.latency.lock().clear();
        let prewarmed = self.prewarm_default_workspace(&config)?;
        Ok(json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "configured_workspaces": config.workspaces.iter().map(|w| json!({"id": w.id, "name": w.name})).collect::<Vec<_>>(),
            "default_workspace_prewarmed": prewarmed.is_some(),
            "prewarm": prewarmed
        }))
    }

    fn prewarm_default_workspace(&self, config: &DaemonConfig) -> AppResult<Option<Value>> {
        if config.workspace.default_path.is_none() && config.workspaces.len() != 1 {
            return Ok(None);
        }
        self.open_workspace(
            &SessionKey::stateless(),
            &json!({"action": "open", "_summary_ids": true}),
        )
        .map(Some)
    }

    fn health(&self) -> AppResult<Value> {
        let config = self.config.read();
        Ok(json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "initialized": config.is_some(),
            "active_sessions": self.sessions.read().len(),
            "cached_workspaces": self.actors.read().len(),
            "configured_workspaces": config.as_ref().map(|c| c.workspaces.len()).unwrap_or(0),
            "latency_metrics": self.latency_metrics(),
            "session_workspace_mode": true,
            "stateless_workspace_warning": "Stateless HTTP requests share one legacy workspace key; enable server.statefulMode for isolated chat sessions."
        }))
    }

    fn workspace(&self, session: &SessionKey, params: &Value) -> AppResult<Value> {
        match params
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("open")
        {
            "open" => self.open_workspace(session, params),
            "summary" => {
                let actor = self.active_actor(session)?;
                if params
                    .get("_summary_ids")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    actor.summary_ids()
                } else {
                    actor.summary(session.as_str(), session.is_stateless())
                }
            }
            "refresh" => self.active_actor(session)?.refresh(
                params
                    .get("force")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                session.as_str(),
                session.is_stateless(),
            ),
            "changes" => self
                .active_actor(session)?
                .changes(session.as_str(), params),
            "diagnostics" => self.active_actor(session)?.diagnostics(),
            "skills" => self.list_skills(),
            "skill" => self.read_skill(params),
            action => Err(AppError::details(
                "INVALID_WORKSPACE_ACTION",
                "Unknown workspace action",
                json!({"action": action}),
            )),
        }
    }

    fn open_workspace(&self, session: &SessionKey, params: &Value) -> AppResult<Value> {
        let ids_only = params
            .get("_summary_ids")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let summarize = |actor: &WorkspaceActor| {
            if ids_only {
                actor.summary_ids()
            } else {
                actor.summary(session.as_str(), session.is_stateless())
            }
        };
        let started = Instant::now();
        let config = self.config()?;
        let requested_path = params.get("path").and_then(Value::as_str);
        let workspace = if let Some(path) = requested_path {
            self.workspace_for_path(&config, path)?
        } else if let Some(path) = config.workspace.default_path.as_deref() {
            self.workspace_for_path(&config, path)?
        } else if config.workspaces.len() == 1 {
            config.workspaces[0].clone()
        } else {
            return Err(AppError::details(
                "WORKSPACE_REQUIRED",
                "Pass an absolute path or configure workspace.defaultPath",
                json!({"suggested_action": "workspace(action='open', path='...')"}),
            ));
        };

        let canonical_workspace = self.canonicalize_workspace(&config, &workspace)?;
        if let Some(actor) = self.active_actor_if_open(session) {
            if actor.root_path() == Path::new(&canonical_workspace.path) {
                self.record_session_binding(session, &canonical_workspace.path);
                let mut summary = summarize(&actor)?;
                add_elapsed(&mut summary, started.elapsed().as_millis(), true);
                add_phase_metrics(
                    &mut summary,
                    &[
                        ("actor_cache_lookup", 0),
                        ("total_local", started.elapsed().as_millis()),
                    ],
                );
                return Ok(summary);
            }
            let running_runs = actor.running_bash_count();
            if running_runs > 0 {
                return Err(AppError::details(
                    "WORKSPACE_BUSY",
                    "Cannot switch repositories while Bash runs are active in this session",
                    json!({"running_runs": running_runs, "suggested_action": "Wait for or cancel active Bash runs before switching repositories"}),
                ));
            }
        }

        let key = canonical_workspace.path.clone();
        let open_lock = self.workspace_open_lock(&key);
        let mut actor_open_ms = 0;
        let (actor_result, actor_cache_hit, cache_lookup_ms) = {
            let _open_guard = open_lock.lock();
            let cache_lookup_started = Instant::now();
            let cached_actor = self.actors.read().get(&key).cloned();
            let cache_lookup_ms = cache_lookup_started.elapsed().as_millis();
            let actor_cache_hit = cached_actor.is_some();
            let actor_result = if let Some(actor) = cached_actor {
                Ok(actor)
            } else {
                let actor_open_started = Instant::now();
                match WorkspaceActor::open(
                    &canonical_workspace,
                    config.policy.clone(),
                    PathBuf::from(config.cache_root),
                ) {
                    Ok(actor) => {
                        actor_open_ms = actor_open_started.elapsed().as_millis();
                        let actor = Arc::new(actor);
                        self.actors.write().insert(key.clone(), actor.clone());
                        Ok(actor)
                    }
                    Err(error) => Err(error),
                }
            };
            (actor_result, actor_cache_hit, cache_lookup_ms)
        };
        self.release_workspace_open_lock(&key, &open_lock);
        let actor = actor_result?;
        self.record_session_binding(session, &key);
        self.touch_actor(&key);
        self.evict_idle_actors();
        let mut summary = summarize(&actor)?;
        add_elapsed(&mut summary, started.elapsed().as_millis(), actor_cache_hit);
        add_phase_metrics(
            &mut summary,
            &[
                ("actor_cache_lookup", cache_lookup_ms),
                ("actor_open", actor_open_ms),
                ("total_local", started.elapsed().as_millis()),
            ],
        );
        Ok(summary)
    }

    fn workspace_open_lock(&self, key: &str) -> Arc<Mutex<()>> {
        self.open_locks
            .lock()
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn release_workspace_open_lock(&self, key: &str, lock: &Arc<Mutex<()>>) {
        let mut locks = self.open_locks.lock();
        if locks
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, lock) && Arc::strong_count(lock) == 2)
        {
            locks.remove(key);
        }
    }

    fn active_actor_if_open(&self, session: &SessionKey) -> Option<Arc<WorkspaceActor>> {
        let key = self.sessions.read().get(session).cloned()?;
        let actor = self.actors.read().get(&key).cloned();
        if actor.is_some() {
            self.touch_actor(&key);
        } else {
            self.sessions.write().remove(session);
        }
        actor
    }

    fn record_session_binding(&self, session: &SessionKey, key: &str) {
        self.sessions
            .write()
            .insert(session.clone(), key.to_owned());
    }

    fn active_actor(&self, session: &SessionKey) -> AppResult<Arc<WorkspaceActor>> {
        if let Some(actor) = self.active_actor_if_open(session) {
            return Ok(actor);
        }

        let config = self.config()?;
        if config.workspace.default_path.is_some() || config.workspaces.len() == 1 {
            self.workspace(session, &json!({"action": "open", "_summary_ids": true}))?;
            if let Some(actor) = self.active_actor_if_open(session) {
                return Ok(actor);
            }
        }

        Err(AppError::details(
            "WORKSPACE_NOT_OPEN",
            "No repository is active for this session. Open one explicitly or configure workspace.defaultPath.",
            json!({
                "suggested_action": "workspace(action='open', path='C:/absolute/path/to/project')"
            }),
        ))
    }

    fn touch_actor(&self, key: &str) {
        let mut lru = self.actor_lru.lock();
        lru.retain(|item| item != key);
        lru.push_back(key.to_owned());
    }

    fn evict_idle_actors(&self) {
        let mut scanned = 0usize;
        while self.actors.read().len() > MAX_CACHED_WORKSPACES {
            scanned += 1;
            if scanned > self.actors.read().len() {
                return;
            }
            if self.actors.read().len() <= MAX_CACHED_WORKSPACES {
                return;
            }
            let Some(candidate) = self.actor_lru.lock().pop_front() else {
                return;
            };
            if self
                .sessions
                .read()
                .values()
                .any(|active_key| active_key == &candidate)
            {
                self.touch_actor(&candidate);
                continue;
            }
            let can_remove = self
                .actors
                .read()
                .get(&candidate)
                .map(|actor| actor.running_bash_count() == 0)
                .unwrap_or(false);
            if can_remove {
                self.actors.write().remove(&candidate);
            } else if self.actors.read().contains_key(&candidate) {
                self.touch_actor(&candidate);
            }
        }
    }

    fn record_latency(&self, method: &str, elapsed_ms: u128) {
        let mut latency = self.latency.lock();
        let samples = latency.entry(method.to_owned()).or_default();
        samples.push_back(elapsed_ms);
        while samples.len() > MAX_LATENCY_SAMPLES {
            samples.pop_front();
        }
    }

    fn latency_metrics(&self) -> Value {
        let latency = self.latency.lock();
        let mut methods = serde_json::Map::new();
        for (method, samples) in latency.iter() {
            if samples.is_empty() {
                continue;
            }
            let mut sorted: Vec<u128> = samples.iter().copied().collect();
            sorted.sort_unstable();
            let percentile = |percent: usize| -> u128 {
                let index = (sorted.len().saturating_sub(1) * percent) / 100;
                sorted[index]
            };
            methods.insert(
                method.clone(),
                json!({
                    "count": samples.len(),
                    "p50_ms": percentile(50),
                    "p90_ms": percentile(90),
                    "max_ms": *sorted.last().unwrap_or(&0),
                }),
            );
        }
        Value::Object(methods)
    }

    fn workspace_for_path(
        &self,
        config: &DaemonConfig,
        requested: &str,
    ) -> AppResult<WorkspaceConfig> {
        let canonical_text = self.canonical_workspace_path(config, requested)?;
        for workspace in &config.workspaces {
            if self.canonical_workspace_path(config, &workspace.path)? == canonical_text {
                let mut configured = workspace.clone();
                configured.path = canonical_text;
                return Ok(configured);
            }
        }
        self.dynamic_workspace(config, &canonical_text)
    }

    fn canonicalize_workspace(
        &self,
        config: &DaemonConfig,
        workspace: &WorkspaceConfig,
    ) -> AppResult<WorkspaceConfig> {
        let mut workspace = workspace.clone();
        workspace.path = self.canonical_workspace_path(config, &workspace.path)?;
        Ok(workspace)
    }

    fn canonical_workspace_path(
        &self,
        config: &DaemonConfig,
        requested: &str,
    ) -> AppResult<String> {
        let canonical = fs::canonicalize(requested).map_err(|error| {
            AppError::details(
                "WORKSPACE_NOT_FOUND",
                format!("Cannot open workspace: {error}"),
                json!({"path": requested}),
            )
        })?;
        if !canonical.is_dir() {
            return Err(AppError::new(
                "WORKSPACE_NOT_DIRECTORY",
                "Workspace path is not a directory",
            ));
        }
        let mut allowed_roots = config.workspace.allowed_roots.clone();
        if allowed_roots.is_empty() {
            allowed_roots.extend(
                config
                    .workspaces
                    .iter()
                    .filter_map(|workspace| Path::new(&workspace.path).parent())
                    .map(|path| path.to_string_lossy().into_owned()),
            );
            if let Some(default_path) = config.workspace.default_path.as_deref() {
                if let Some(parent) = Path::new(default_path).parent() {
                    allowed_roots.push(parent.to_string_lossy().into_owned());
                }
            }
        }
        let allowed = allowed_roots.iter().any(|root| {
            fs::canonicalize(root)
                .map(|value| canonical.starts_with(value))
                .unwrap_or(false)
        });
        if !allowed {
            return Err(AppError::details(
                "WORKSPACE_OUTSIDE_ALLOWED_ROOTS",
                "Workspace path is outside configured allowed roots",
                json!({"path": canonical, "allowed_roots": allowed_roots}),
            ));
        }
        Ok(canonical.to_string_lossy().into_owned())
    }

    fn dynamic_workspace(
        &self,
        config: &DaemonConfig,
        requested: &str,
    ) -> AppResult<WorkspaceConfig> {
        let canonical_text = self.canonical_workspace_path(config, requested)?;
        let canonical = Path::new(&canonical_text);
        let name = canonical
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("Repository");
        let mut hasher = DefaultHasher::new();
        canonical_text.hash(&mut hasher);
        let workspace_id = format!("repo-{:016x}", hasher.finish());
        Ok(WorkspaceConfig {
            id: workspace_id,
            name: name.to_owned(),
            path: canonical_text,
            artifact_paths: config.workspace.artifact_paths.clone(),
            exclude_paths: config.workspace.exclude_paths.clone(),
        })
    }

    fn list_skills(&self) -> AppResult<Value> {
        let config = self.config()?;
        if !config.skills.enabled {
            return Err(AppError::new(
                "SKILLS_DISABLED",
                "Skills are disabled in configuration",
            ));
        }
        let mut skills = Vec::new();
        for root in &config.skills.roots {
            let Ok(entries) = fs::read_dir(root) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path().join("SKILL.md");
                if !path.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let description = fs::read_to_string(&path).ok().and_then(|text| {
                    text.lines().find_map(|line| {
                        line.strip_prefix("description:")
                            .map(str::trim)
                            .map(str::to_owned)
                    })
                });
                skills.push(json!({"name": name, "description": description, "path": path}));
            }
        }
        skills.sort_by(|a, b| {
            a.get("name")
                .and_then(Value::as_str)
                .cmp(&b.get("name").and_then(Value::as_str))
        });
        Ok(json!({"explicit_only": true, "skills": skills}))
    }

    fn read_skill(&self, params: &Value) -> AppResult<Value> {
        let config = self.config()?;
        if !config.skills.enabled {
            return Err(AppError::new(
                "SKILLS_DISABLED",
                "Skills are disabled in configuration",
            ));
        }
        let name = required_str(params, "skill_name")?;
        validate_skill_name(name)?;
        for root in &config.skills.roots {
            let path = Path::new(root).join(name).join("SKILL.md");
            if path.is_file() {
                return Ok(json!({
                    "name": name,
                    "path": path,
                    "content": fs::read_to_string(path)?
                }));
            }
        }
        Err(AppError::details(
            "SKILL_NOT_FOUND",
            "Skill was not found",
            json!({"skill_name": name}),
        ))
    }

    fn config(&self) -> AppResult<DaemonConfig> {
        self.config
            .read()
            .clone()
            .ok_or_else(|| AppError::new("NOT_INITIALIZED", "Daemon has not been initialized"))
    }
}

fn add_elapsed(value: &mut Value, elapsed_ms: u128, already_open: bool) {
    if let Some(object) = value.as_object_mut() {
        object.insert("request_elapsed_ms".to_owned(), json!(elapsed_ms));
        object.insert("already_open".to_owned(), json!(already_open));
    }
}

fn add_operation_elapsed(value: &mut Value, elapsed_ms: u128) {
    if let Some(object) = value.as_object_mut() {
        object.insert("elapsed_ms".to_owned(), json!(elapsed_ms));
    }
}

fn add_latency_metrics(value: &mut Value, metrics: Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert("latency_metrics".to_owned(), metrics);
    }
}

fn add_phase_metrics(value: &mut Value, phases: &[(&str, u128)]) {
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
mod tests {
    use super::*;
    use crate::model::{
        test_bash_executable, BashConfig, PolicyConfig, SkillsConfig, WorkspaceConfig,
        WorkspaceSettings,
    };
    use tempfile::tempdir;

    fn test_bash_config() -> BashConfig {
        BashConfig {
            enabled: true,
            executable: test_bash_executable(),
            default_timeout_ms: 120_000,
            max_timeout_ms: 300_000,
            max_output_chars: 30_000,
            retention_hours: 1,
        }
    }

    fn daemon_config(
        root: &std::path::Path,
        cache: &std::path::Path,
        max_file_bytes: usize,
    ) -> Value {
        serde_json::to_value(DaemonConfig {
            workspaces: vec![WorkspaceConfig {
                id: "main".to_owned(),
                name: "Main".to_owned(),
                path: root.to_string_lossy().into_owned(),
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            }],
            workspace: Default::default(),
            skills: Default::default(),
            policy: PolicyConfig {
                max_file_bytes,
                max_context_chars: 50_000,
                max_search_results: 100,
                bash: test_bash_config(),
            },
            cache_root: cache.to_string_lossy().into_owned(),
        })
        .unwrap()
    }

    #[test]
    fn initialize_accepts_bash_policy_and_rejects_legacy_execution_keys() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let valid = daemon_config(root.path(), cache.path(), 1_000_000);
        WorkspaceManager::default().initialize(&valid).unwrap();

        for (key, value) in [
            ("allowedCommands", json!(["cargo"])),
            ("shellEnabled", json!(false)),
            ("maxTaskOutputChars", json!(30_000)),
            ("taskRetentionHours", json!(1)),
        ] {
            let mut legacy = valid.clone();
            legacy["policy"][key] = value;
            let error = WorkspaceManager::default().initialize(&legacy).unwrap_err();
            assert_eq!(error.0.code, "LEGACY_EXECUTION_CONFIG", "{key}");
            assert!(error.0.message.contains("policy.bash"));
        }

        let mut legacy_tasks = valid;
        legacy_tasks["tasks"] = json!({});
        let error = WorkspaceManager::default()
            .initialize(&legacy_tasks)
            .unwrap_err();
        assert_eq!(error.0.code, "LEGACY_EXECUTION_CONFIG");
        assert!(error.0.message.contains("policy.bash"));
    }

    fn sleep_command() -> &'static str {
        "echo started; sleep 30"
    }

    #[test]
    fn code_tools_reuse_prewarmed_default_repository() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = WorkspaceManager::default();
        let config = serde_json::to_value(DaemonConfig {
            workspaces: Vec::new(),
            workspace: WorkspaceSettings {
                default_path: Some(root.path().to_string_lossy().into_owned()),
                allowed_roots: vec![root.path().to_string_lossy().into_owned()],
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                bash: test_bash_config(),
            },
            cache_root: cache.path().to_string_lossy().into_owned(),
        })
        .unwrap();
        manager.initialize(&config).unwrap();

        assert!(manager
            .sessions
            .read()
            .contains_key(&SessionKey::stateless()));
        let actor = manager.active_actor(&SessionKey::stdio()).unwrap();
        assert_eq!(
            actor.root_path(),
            std::fs::canonicalize(root.path()).unwrap()
        );
        assert_eq!(manager.actors.read().len(), 1);
    }

    #[test]
    fn configured_workspace_metadata_survives_canonical_open() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = WorkspaceManager::default();
        let config = serde_json::to_value(DaemonConfig {
            workspaces: vec![WorkspaceConfig {
                id: "configured-id".to_owned(),
                name: "Configured Name".to_owned(),
                path: root.path().to_string_lossy().into_owned(),
                artifact_paths: vec!["logs".to_owned()],
                exclude_paths: vec!["generated/".to_owned()],
            }],
            workspace: WorkspaceSettings {
                default_path: Some(root.path().to_string_lossy().into_owned()),
                allowed_roots: Vec::new(),
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                bash: test_bash_config(),
            },
            cache_root: cache.path().to_string_lossy().into_owned(),
        })
        .unwrap();
        manager.initialize(&config).unwrap();

        let summary = manager
            .workspace(&SessionKey::stdio(), &json!({"action": "open"}))
            .unwrap();

        assert_eq!(summary["workspace_id"], "configured-id");
        assert_eq!(summary["name"], "Configured Name");
    }

    #[test]
    fn dynamic_open_switches_only_the_calling_session_and_reuses_cache() {
        let root = tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        let cache = root.path().join("cache");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("main.rs"), "fn first() {}\n").unwrap();
        std::fs::write(second.join("main.rs"), "fn second() {}\n").unwrap();
        let manager = WorkspaceManager::default();
        let config = serde_json::to_value(DaemonConfig {
            workspaces: Vec::new(),
            workspace: WorkspaceSettings {
                default_path: None,
                allowed_roots: vec![root.path().to_string_lossy().into_owned()],
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                bash: test_bash_config(),
            },
            cache_root: cache.to_string_lossy().into_owned(),
        })
        .unwrap();
        manager.initialize(&config).unwrap();
        let session_a = SessionKey::new("a");
        let session_b = SessionKey::new("b");
        manager
            .workspace(&session_a, &json!({"action": "open", "path": first}))
            .unwrap();
        manager
            .workspace(&session_b, &json!({"action": "open", "path": second}))
            .unwrap();

        assert_eq!(
            manager.active_actor(&session_a).unwrap().root_path(),
            std::fs::canonicalize(&first).unwrap()
        );
        assert_eq!(
            manager.active_actor(&session_b).unwrap().root_path(),
            std::fs::canonicalize(&second).unwrap()
        );
        assert_eq!(std::fs::read_dir(cache.join("repos")).unwrap().count(), 2);
    }

    #[test]
    fn http_session_keys_open_independent_active_workspaces() {
        let root = tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        let cache = root.path().join("cache");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("main.rs"), "fn first() {}\n").unwrap();
        std::fs::write(second.join("main.rs"), "fn second() {}\n").unwrap();
        let manager = WorkspaceManager::default();
        let config = serde_json::to_value(DaemonConfig {
            workspaces: Vec::new(),
            workspace: WorkspaceSettings {
                default_path: None,
                allowed_roots: vec![root.path().to_string_lossy().into_owned()],
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                bash: test_bash_config(),
            },
            cache_root: cache.to_string_lossy().into_owned(),
        })
        .unwrap();
        manager.initialize(&config).unwrap();
        let session_a = SessionKey::new("http:a");
        let session_b = SessionKey::new("http:b");

        manager
            .workspace(&session_a, &json!({"action": "open", "path": first}))
            .unwrap();
        manager
            .workspace(&session_b, &json!({"action": "open", "path": second}))
            .unwrap();

        assert_eq!(
            manager.active_actor(&session_a).unwrap().root_path(),
            std::fs::canonicalize(&first).unwrap()
        );
        assert_eq!(
            manager.active_actor(&session_b).unwrap().root_path(),
            std::fs::canonicalize(&second).unwrap()
        );
    }

    #[test]
    fn unbound_http_session_uses_default_after_another_session_opens() {
        let root = tempdir().unwrap();
        let default_workspace = root.path().join("crawlerai");
        let explicit_workspace = root.path().join("codeweave");
        let cache = root.path().join("cache");
        std::fs::create_dir_all(&default_workspace).unwrap();
        std::fs::create_dir_all(&explicit_workspace).unwrap();
        std::fs::write(default_workspace.join("main.rs"), "fn crawlerai() {}\n").unwrap();
        std::fs::write(explicit_workspace.join("main.rs"), "fn codeweave() {}\n").unwrap();
        let manager = WorkspaceManager::default();
        let config = serde_json::to_value(DaemonConfig {
            workspaces: Vec::new(),
            workspace: WorkspaceSettings {
                default_path: Some(default_workspace.to_string_lossy().into_owned()),
                allowed_roots: vec![root.path().to_string_lossy().into_owned()],
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                bash: test_bash_config(),
            },
            cache_root: cache.to_string_lossy().into_owned(),
        })
        .unwrap();
        manager.initialize(&config).unwrap();
        let auto_session = SessionKey::new("http:auto-opened");
        let explicit_session = SessionKey::new("http:explicit-open");

        assert_eq!(
            manager.active_actor(&auto_session).unwrap().root_path(),
            std::fs::canonicalize(&default_workspace).unwrap()
        );
        manager
            .workspace(
                &explicit_session,
                &json!({"action": "open", "path": explicit_workspace}),
            )
            .unwrap();

        assert_eq!(
            manager.active_actor(&auto_session).unwrap().root_path(),
            std::fs::canonicalize(&default_workspace).unwrap()
        );
        assert_eq!(
            manager.active_actor(&explicit_session).unwrap().root_path(),
            std::fs::canonicalize(&explicit_workspace).unwrap()
        );
    }

    #[test]
    fn stateless_session_uses_one_legacy_workspace_key() {
        let root = tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        let cache = root.path().join("cache");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("main.rs"), "fn first() {}\n").unwrap();
        std::fs::write(second.join("main.rs"), "fn second() {}\n").unwrap();
        let manager = WorkspaceManager::default();
        let config = serde_json::to_value(DaemonConfig {
            workspaces: Vec::new(),
            workspace: WorkspaceSettings {
                default_path: None,
                allowed_roots: vec![root.path().to_string_lossy().into_owned()],
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                bash: test_bash_config(),
            },
            cache_root: cache.to_string_lossy().into_owned(),
        })
        .unwrap();
        manager.initialize(&config).unwrap();

        manager
            .workspace(
                &SessionKey::stateless(),
                &json!({"action": "open", "path": first}),
            )
            .unwrap();
        manager
            .workspace(
                &SessionKey::stateless(),
                &json!({"action": "open", "path": second}),
            )
            .unwrap();

        assert_eq!(manager.sessions.read().len(), 1);
        assert_eq!(
            manager
                .active_actor(&SessionKey::stateless())
                .unwrap()
                .root_path(),
            std::fs::canonicalize(&second).unwrap()
        );
    }

    #[test]
    fn sessions_opening_same_repo_reuse_one_actor_and_clear_open_lock() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn shared() {}\n").unwrap();
        let manager = WorkspaceManager::default();
        let config = serde_json::to_value(DaemonConfig {
            workspaces: Vec::new(),
            workspace: WorkspaceSettings {
                default_path: None,
                allowed_roots: vec![root.path().to_string_lossy().into_owned()],
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                bash: test_bash_config(),
            },
            cache_root: cache.path().to_string_lossy().into_owned(),
        })
        .unwrap();
        manager.initialize(&config).unwrap();
        let session_a = SessionKey::new("a");
        let session_b = SessionKey::new("b");
        manager
            .workspace(&session_a, &json!({"action": "open", "path": root.path()}))
            .unwrap();
        let second = manager
            .workspace(&session_b, &json!({"action": "open", "path": root.path()}))
            .unwrap();

        let actor_a = manager.active_actor(&session_a).unwrap();
        let actor_b = manager.active_actor(&session_b).unwrap();
        assert!(Arc::ptr_eq(&actor_a, &actor_b));
        assert_eq!(manager.actors.read().len(), 1);
        assert!(manager.open_locks.lock().is_empty());
        assert_eq!(second["already_open"], true);
        assert!(second["phase_ms"]["actor_cache_lookup"].is_number());
    }

    #[test]
    fn closing_session_removes_active_mapping_and_allows_eviction() {
        let root = tempdir().unwrap();
        let cache = root.path().join("cache");
        let manager = WorkspaceManager::default();
        let mut workspaces = Vec::new();
        for index in 0..=MAX_CACHED_WORKSPACES {
            let workspace = root.path().join(format!("repo-{index}"));
            std::fs::create_dir_all(&workspace).unwrap();
            std::fs::write(
                workspace.join("main.rs"),
                format!("fn repo_{index}() {{}}\n"),
            )
            .unwrap();
            workspaces.push(workspace);
        }
        let config = serde_json::to_value(DaemonConfig {
            workspaces: Vec::new(),
            workspace: WorkspaceSettings {
                default_path: None,
                allowed_roots: vec![root.path().to_string_lossy().into_owned()],
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                bash: test_bash_config(),
            },
            cache_root: cache.to_string_lossy().into_owned(),
        })
        .unwrap();
        manager.initialize(&config).unwrap();
        let sessions: Vec<_> = (0..=MAX_CACHED_WORKSPACES)
            .map(|index| SessionKey::new(format!("session-{index}")))
            .collect();
        for (session, workspace) in sessions.iter().zip(&workspaces) {
            manager
                .workspace(session, &json!({"action": "open", "path": workspace}))
                .unwrap();
        }
        assert!(manager.actors.read().len() > MAX_CACHED_WORKSPACES);

        manager.close_session(&sessions[0]);

        assert!(manager.sessions.read().get(&sessions[0]).is_none());
        assert!(manager.actors.read().len() <= MAX_CACHED_WORKSPACES);
    }

    #[test]
    fn stateless_session_reports_workspace_isolation_warning() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn shared() {}\n").unwrap();
        let manager = WorkspaceManager::default();
        manager
            .initialize(&daemon_config(root.path(), cache.path(), 1_000_000))
            .unwrap();

        let summary = manager
            .workspace(&SessionKey::stateless(), &json!({"action": "open"}))
            .unwrap();

        assert!(summary["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().is_some_and(
                |text| text.contains("Stateless HTTP requests share one legacy workspace key")
            )));
    }

    #[tokio::test]
    async fn running_bash_blocks_only_owning_session_switch_and_global_reinitialize() {
        let root = tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        let cache = root.path().join("cache");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("main.rs"), "fn first() {}\n").unwrap();
        std::fs::write(second.join("main.rs"), "fn second() {}\n").unwrap();
        let manager = Arc::new(WorkspaceManager::default());
        let config = serde_json::to_value(DaemonConfig {
            workspaces: Vec::new(),
            workspace: WorkspaceSettings {
                default_path: None,
                allowed_roots: vec![root.path().to_string_lossy().into_owned()],
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                bash: test_bash_config(),
            },
            cache_root: cache.to_string_lossy().into_owned(),
        })
        .unwrap();
        manager
            .dispatch(SessionKey::stdio(), "initialize", &config)
            .await
            .unwrap();
        let session_a = SessionKey::new("a");
        let session_b = SessionKey::new("b");
        manager
            .dispatch(
                session_a.clone(),
                "workspace",
                &json!({"action": "open", "path": first}),
            )
            .await
            .unwrap();
        let started = manager
            .dispatch(
                session_a.clone(),
                "bash",
                &json!({"command": sleep_command(), "background": true}),
            )
            .await
            .unwrap();
        assert_eq!(started["background"], true);

        let switch_error = manager
            .dispatch(
                session_a.clone(),
                "workspace",
                &json!({"action": "open", "path": second}),
            )
            .await
            .unwrap_err();
        assert_eq!(switch_error.0.code, "WORKSPACE_BUSY");

        manager
            .dispatch(
                session_b,
                "workspace",
                &json!({"action": "open", "path": second}),
            )
            .await
            .unwrap();

        let reinitialize_error = manager
            .dispatch(SessionKey::stdio(), "initialize", &config)
            .await
            .unwrap_err();
        assert_eq!(reinitialize_error.0.code, "WORKSPACE_BUSY");

        let run_id = started["run_id"].as_str().unwrap();
        let _ = manager
            .dispatch(session_a, "bash_cancel", &json!({"run_id": run_id}))
            .await;
    }

    #[tokio::test]
    async fn health_and_workspace_summary_expose_latency_metrics() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = Arc::new(WorkspaceManager::default());
        let config = daemon_config(root.path(), cache.path(), 1_000_000);
        manager
            .dispatch(SessionKey::stdio(), "initialize", &config)
            .await
            .unwrap();
        manager
            .dispatch(SessionKey::stdio(), "workspace", &json!({"action": "open"}))
            .await
            .unwrap();

        let summary = manager
            .dispatch(
                SessionKey::stdio(),
                "workspace",
                &json!({"action": "summary"}),
            )
            .await
            .unwrap();
        let health = manager
            .dispatch(SessionKey::stdio(), "health", &json!({}))
            .await
            .unwrap();

        assert!(summary["latency_metrics"]["workspace"]["p50_ms"].is_number());
        assert!(health["latency_metrics"]["workspace"]["p90_ms"].is_number());
    }

    #[tokio::test]
    async fn initialize_resets_previous_latency_samples() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = Arc::new(WorkspaceManager::default());
        let config = daemon_config(root.path(), cache.path(), 1_000_000);
        manager
            .dispatch(SessionKey::stdio(), "initialize", &config)
            .await
            .unwrap();
        manager
            .dispatch(SessionKey::stdio(), "workspace", &json!({"action": "open"}))
            .await
            .unwrap();
        manager
            .dispatch(SessionKey::stdio(), "initialize", &config)
            .await
            .unwrap();

        let health = manager
            .dispatch(SessionKey::stdio(), "health", &json!({}))
            .await
            .unwrap();

        assert!(health["latency_metrics"].get("workspace").is_none());
    }

    #[test]
    fn failed_open_clears_per_workspace_lock_without_switching_session() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("main.rs"), "fn valid() {}\n").unwrap();
        let cache_file = root.path().join("cache-file");
        std::fs::write(&cache_file, "not a directory").unwrap();
        let manager = WorkspaceManager::default();
        let config = serde_json::to_value(DaemonConfig {
            workspaces: Vec::new(),
            workspace: WorkspaceSettings {
                default_path: None,
                allowed_roots: vec![root.path().to_string_lossy().into_owned()],
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                bash: test_bash_config(),
            },
            cache_root: cache_file.to_string_lossy().into_owned(),
        })
        .unwrap();
        manager.initialize(&config).unwrap();
        let session = SessionKey::stdio();

        let error = manager
            .workspace(&session, &json!({"action": "open", "path": workspace}))
            .unwrap_err();

        assert_eq!(error.0.code, "INTERNAL_ERROR");
        assert!(manager.open_locks.lock().is_empty());
        assert!(manager.sessions.read().get(&session).is_none());
    }

    #[test]
    fn dynamic_open_rejects_paths_outside_allowed_roots_without_switching_session() {
        let allowed = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::fs::write(allowed.path().join("main.rs"), "fn allowed() {}\n").unwrap();
        std::fs::write(outside.path().join("main.rs"), "fn outside() {}\n").unwrap();
        let manager = WorkspaceManager::default();
        let config = serde_json::to_value(DaemonConfig {
            workspaces: Vec::new(),
            workspace: WorkspaceSettings {
                default_path: None,
                allowed_roots: vec![allowed.path().to_string_lossy().into_owned()],
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                bash: test_bash_config(),
            },
            cache_root: allowed.path().join("cache").to_string_lossy().into_owned(),
        })
        .unwrap();
        manager.initialize(&config).unwrap();
        let session = SessionKey::stdio();
        manager
            .workspace(&session, &json!({"action": "open", "path": allowed.path()}))
            .unwrap();
        let error = manager
            .workspace(&session, &json!({"action": "open", "path": outside.path()}))
            .unwrap_err();
        assert_eq!(error.0.code, "WORKSPACE_OUTSIDE_ALLOWED_ROOTS");
        assert_eq!(
            manager.active_actor(&session).unwrap().root_path(),
            std::fs::canonicalize(allowed.path()).unwrap()
        );
    }

    #[test]
    fn actor_cache_eviction_scans_past_active_lru_entries() {
        let root = tempdir().unwrap();
        let cache = root.path().join("cache");
        let manager = WorkspaceManager::default();
        let mut workspaces = Vec::new();
        for index in 0..=MAX_CACHED_WORKSPACES {
            let workspace = root.path().join(format!("repo-{index}"));
            std::fs::create_dir_all(&workspace).unwrap();
            std::fs::write(
                workspace.join("main.rs"),
                format!("fn repo_{index}() {{}}\n"),
            )
            .unwrap();
            workspaces.push(workspace);
        }
        let config = serde_json::to_value(DaemonConfig {
            workspaces: Vec::new(),
            workspace: WorkspaceSettings {
                default_path: None,
                allowed_roots: vec![root.path().to_string_lossy().into_owned()],
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                bash: test_bash_config(),
            },
            cache_root: cache.to_string_lossy().into_owned(),
        })
        .unwrap();
        manager.initialize(&config).unwrap();
        let sessions: Vec<_> = (0..MAX_CACHED_WORKSPACES)
            .map(|index| SessionKey::new(format!("session-{index}")))
            .collect();
        for (session, workspace) in sessions.iter().zip(&workspaces) {
            manager
                .workspace(session, &json!({"action": "open", "path": workspace}))
                .unwrap();
        }

        manager
            .workspace(
                &sessions[0],
                &json!({"action": "open", "path": workspaces[MAX_CACHED_WORKSPACES]}),
            )
            .unwrap();

        assert_eq!(manager.actors.read().len(), MAX_CACHED_WORKSPACES);
        let evicted = std::fs::canonicalize(&workspaces[0])
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(!manager.actors.read().contains_key(&evicted));
    }

    #[tokio::test]
    async fn bash_dispatch_validates_command_fields_and_forces_start_action() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = Arc::new(WorkspaceManager::default());
        let config = daemon_config(root.path(), cache.path(), 1_000_000);
        manager
            .dispatch(SessionKey::stdio(), "initialize", &config)
            .await
            .unwrap();
        manager
            .dispatch(SessionKey::stdio(), "workspace", &json!({"action": "open"}))
            .await
            .unwrap();

        let raw_command = manager
            .dispatch(
                SessionKey::stdio(),
                "bash",
                &json!({"command": ["not-a-string"]}),
            )
            .await
            .unwrap_err();
        assert_eq!(raw_command.0.code, "INVALID_ARGUMENT");

        let spoofed_action = manager
            .dispatch(
                SessionKey::stdio(),
                "bash",
                &json!({"command": "printf test", "action": "cancel"}),
            )
            .await
            .unwrap_err();
        assert_eq!(spoofed_action.0.code, "INVALID_ARGUMENT");

        let removed = manager
            .dispatch(SessionKey::stdio(), "task_run", &json!({"profile": "test"}))
            .await
            .unwrap_err();
        assert_eq!(removed.0.code, "METHOD_NOT_FOUND");
    }

    #[tokio::test]
    async fn actor_cache_eviction_skips_running_idle_actor() {
        let root = tempdir().unwrap();
        let cache = root.path().join("cache");
        let mut workspaces = Vec::new();
        for index in 0..=MAX_CACHED_WORKSPACES {
            let workspace = root.path().join(format!("repo-{index}"));
            std::fs::create_dir_all(&workspace).unwrap();
            std::fs::write(
                workspace.join("main.rs"),
                format!("fn repo_{index}() {{}}\n"),
            )
            .unwrap();
            workspaces.push(workspace);
        }
        let manager = Arc::new(WorkspaceManager::default());
        let config = serde_json::to_value(DaemonConfig {
            workspaces: Vec::new(),
            workspace: WorkspaceSettings {
                default_path: None,
                allowed_roots: vec![root.path().to_string_lossy().into_owned()],
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                bash: test_bash_config(),
            },
            cache_root: cache.to_string_lossy().into_owned(),
        })
        .unwrap();
        manager
            .dispatch(SessionKey::stdio(), "initialize", &config)
            .await
            .unwrap();
        let running_session = SessionKey::new("running");
        manager
            .dispatch(
                running_session.clone(),
                "workspace",
                &json!({"action": "open", "path": workspaces[0]}),
            )
            .await
            .unwrap();
        let started = manager
            .dispatch(
                running_session.clone(),
                "bash",
                &json!({"command": sleep_command(), "background": true}),
            )
            .await
            .unwrap();
        let running_key = std::fs::canonicalize(&workspaces[0])
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let running_actor = manager.active_actor(&running_session).unwrap();
        manager.sessions.write().remove(&running_session);

        for (index, workspace) in workspaces.iter().enumerate().skip(1) {
            manager
                .dispatch(
                    SessionKey::new(format!("session-{index}")),
                    "workspace",
                    &json!({"action": "open", "path": workspace}),
                )
                .await
                .unwrap();
        }

        assert!(manager.actors.read().contains_key(&running_key));
        assert!(manager.actors.read().len() > MAX_CACHED_WORKSPACES);

        let run_id = started["run_id"].as_str().unwrap();
        let _ = running_actor
            .run(
                running_session.as_str(),
                &json!({"action": "cancel", "run_id": run_id}),
            )
            .await;
        for _ in 0..50 {
            if running_actor.running_bash_count() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        manager.evict_idle_actors();
        assert!(manager.actors.read().len() <= MAX_CACHED_WORKSPACES);
    }

    #[test]
    fn reinitialize_closes_existing_actors() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = WorkspaceManager::default();

        manager
            .initialize(&daemon_config(root.path(), cache.path(), 1_000_000))
            .unwrap();
        manager
            .workspace(
                &SessionKey::stdio(),
                &json!({"action": "open", "workspace": "main"}),
            )
            .unwrap();
        assert_eq!(manager.actors.read().len(), 1);

        manager
            .initialize(&daemon_config(root.path(), cache.path(), 512_000))
            .unwrap();
        assert_eq!(manager.actors.read().len(), 1);
        assert!(manager
            .sessions
            .read()
            .contains_key(&SessionKey::stateless()));
    }

    #[test]
    fn initialize_prewarms_default_workspace_actor() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = WorkspaceManager::default();

        let result = manager
            .initialize(&daemon_config(root.path(), cache.path(), 1_000_000))
            .unwrap();

        assert_eq!(result["default_workspace_prewarmed"], true);
        assert!(result["prewarm"]["phase_ms"]["actor_open"].is_number());
        assert_eq!(manager.actors.read().len(), 1);
    }

    #[test]
    fn summary_ids_flag_uses_lightweight_summary() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = WorkspaceManager::default();
        manager
            .initialize(&daemon_config(root.path(), cache.path(), 1_000_000))
            .unwrap();

        let summary = manager
            .workspace(
                &SessionKey::stdio(),
                &json!({"action": "summary", "_summary_ids": true}),
            )
            .unwrap();

        assert!(summary.get("workspace_id").is_some());
        assert!(summary.get("snapshot_id").is_some());
        assert!(summary.get("generation").is_some());
        assert!(summary.get("root").is_none());
        assert!(summary.get("mutations").is_none());
    }

    #[test]
    fn skill_names_must_be_single_safe_path_components() {
        assert!(validate_skill_name("rust-review").is_ok());
        for invalid in [
            "",
            ".",
            "..",
            "../escape",
            "nested/skill",
            "nested\\skill",
            "C:",
            "name:stream",
            "CON",
            "nul.txt",
            "COM1",
            "LPT9.md",
            "trailing.",
            "trailing ",
        ] {
            assert!(
                validate_skill_name(invalid).is_err(),
                "accepted invalid skill name {invalid:?}"
            );
        }
    }
}
