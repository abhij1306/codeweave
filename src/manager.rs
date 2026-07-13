use crate::index::Ranking;
use crate::model::{required_str, AppError, AppResult, DaemonConfig, WorkspaceConfig};
use crate::security::{canonical_root, validate_relative};
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

const MAX_LATENCY_SAMPLES: usize = 128;

/// Opaque per-request attribution token. It no longer routes to a workspace —
/// there is exactly one repository for the process lifetime — but it still
/// scopes Bash runs and journal/`changes` attribution so a stateful HTTP
/// deployment keeps per-chat isolation for those concerns.
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

/// Single-repository manager. Holds exactly one `WorkspaceActor`, built eagerly
/// at `initialize` from `workspace.path`, and serves it to every request.
#[derive(Default)]
pub struct WorkspaceManager {
    config: RwLock<Option<DaemonConfig>>,
    actor: RwLock<Option<Arc<WorkspaceActor>>>,
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

/// Reject removed multi-workspace config keys with an actionable message instead
/// of silently ignoring them (serde would drop unknown fields otherwise).
fn reject_legacy_workspace_config(config: &Value) -> AppResult<()> {
    let mut keys = Vec::new();
    if config.get("workspaces").is_some() {
        keys.push("workspaces");
    }
    if let Some(workspace) = config.get("workspace").and_then(Value::as_object) {
        for key in ["defaultPath", "lockToDefault", "allowedRoots"] {
            if workspace.contains_key(key) {
                keys.push(key);
            }
        }
    }
    if keys.is_empty() {
        return Ok(());
    }
    Err(AppError::details(
        "LEGACY_WORKSPACE_CONFIG",
        "Multi-workspace configuration was removed; configure exactly one repository via workspace.path",
        json!({"removed_keys": keys, "expected": {"workspace": {"path": "C:/absolute/path/to/repo"}}}),
    ))
}

/// Derive the actor's construction input from the single configured repository.
/// `id`/`name` are derived from the canonical path (the actor embeds `id` in the
/// summary and provenance handles); the actor itself is unchanged.
fn single_repo_config(config: &DaemonConfig) -> AppResult<WorkspaceConfig> {
    let requested = config.workspace.path.trim();
    if requested.is_empty() {
        return Err(AppError::invalid(
            "workspace.path is required (absolute path to the repository)",
        ));
    }
    let canonical = canonical_root(Path::new(requested))?;
    let canonical_text = canonical.to_string_lossy().into_owned();
    let name = canonical
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("Repository")
        .to_owned();
    let mut hasher = DefaultHasher::new();
    canonical_text.hash(&mut hasher);
    let id = format!("repo-{:016x}", hasher.finish());
    Ok(WorkspaceConfig {
        id,
        name,
        path: canonical_text,
        artifact_paths: config.workspace.artifact_paths.clone(),
        exclude_paths: config.workspace.exclude_paths.clone(),
    })
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

    pub fn close_session(&self, _session: &SessionKey) {
        // Single-repo model: sessions no longer bind to a workspace and the sole
        // actor lives for the process lifetime, so there is nothing to evict.
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
                let actor = self.actor()?;
                let params = params.clone();
                let session_id = session.as_str().to_owned();
                run_blocking(move || actor.code_context_for_session(&session_id, &params)).await
            }
            "code_search" => {
                let actor = self.actor()?;
                let params = params.clone();
                run_blocking(move || actor.code_search(&params)).await
            }
            "code_fetch" => {
                let actor = self.actor()?;
                let params = params.clone();
                let session_id = session.as_str().to_owned();
                run_blocking(move || actor.code_fetch_for_session(&session_id, &params)).await
            }
            "code_capabilities" => {
                let actor = self.actor()?;
                run_blocking(move || actor.code_capabilities()).await
            }
            "code_write" | "code_replace" | "code_replace_range" | "code_insert"
            | "code_delete" | "code_rename" | "code_preview" | "code_transaction" => {
                let actor = self.actor()?;
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
                let actor = self.actor()?;
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
                self.actor()?.run(session.as_str(), &prepared).await
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

    /// Build the single repository actor eagerly (index scan + file watcher start
    /// happen inside `WorkspaceActor::open`) and pre-probe Bash so the first
    /// validated edit does not pay the discovery cost inline.
    fn initialize(&self, params: &Value) -> AppResult<Value> {
        reject_legacy_execution_config(params)?;
        reject_legacy_workspace_config(params)?;
        let config: DaemonConfig = serde_json::from_value(params.clone())?;
        if config.skills.enabled && !config.skills.explicit_only {
            return Err(AppError::invalid(
                "skills.explicitOnly must remain true; automatic skill invocation is not supported",
            ));
        }

        let repo = single_repo_config(&config)?;
        let ranking = Ranking::parse(&config.index.ranking);
        let open_started = Instant::now();
        let actor = Arc::new(WorkspaceActor::open(
            &repo,
            config.policy.clone(),
            ranking,
            PathBuf::from(config.cache_root.clone()),
        )?);
        let open_ms = open_started.elapsed().as_millis();

        let bash_probe_started = Instant::now();
        let bash_available = actor.probe_bash().is_ok();
        let bash_probe_ms = bash_probe_started.elapsed().as_millis();

        let _lifecycle = self.lifecycle.lock();
        *self.config.write() = Some(config);
        *self.actor.write() = Some(actor.clone());
        self.latency.lock().clear();

        tracing::info!(
            workspace = %repo.path,
            file_count = actor.index_file_count(),
            open_ms,
            bash_available,
            bash_probe_ms,
            "workspace initialized (eager index + watcher, bash pre-probed)"
        );

        Ok(json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "workspace": {"id": repo.id, "name": repo.name, "path": repo.path},
            "index_ready": true,
            "file_count": actor.index_file_count(),
            "bash_available": bash_available,
            "phase_ms": {"actor_open": open_ms, "bash_probe": bash_probe_ms}
        }))
    }

    fn health(&self) -> AppResult<Value> {
        let config = self.config.read();
        let actor = self.actor.read().clone();
        Ok(json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "initialized": config.is_some(),
            "index_ready": actor.is_some(),
            "file_count": actor.as_ref().map(|a| a.index_file_count()),
            "last_reconcile_ms_ago": actor.as_ref().map(|a| a.last_reconcile_elapsed_ms()),
            "latency_metrics": self.latency_metrics(),
        }))
    }

    fn workspace(&self, session: &SessionKey, params: &Value) -> AppResult<Value> {
        match params
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("summary")
        {
            "summary" => {
                let actor = self.actor()?;
                if params
                    .get("_summary_ids")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    actor.summary_ids()
                } else {
                    actor.summary(session.as_str(), false)
                }
            }
            "refresh" => self.actor()?.refresh(
                params
                    .get("force")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                session.as_str(),
                false,
            ),
            "changes" => self.actor()?.changes(session.as_str(), params),
            "diagnostics" => self.actor()?.diagnostics(),
            "skills" => self.list_skills(),
            "skill" => self.read_skill(params),
            action => Err(AppError::details(
                "INVALID_WORKSPACE_ACTION",
                "Unknown workspace action",
                json!({"action": action}),
            )),
        }
    }

    fn actor(&self) -> AppResult<Arc<WorkspaceActor>> {
        self.actor.read().clone().ok_or_else(|| {
            AppError::new(
                "NOT_INITIALIZED",
                "Daemon has not been initialized with a repository",
            )
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        test_bash_executable, BashConfig, IndexSettings, PolicyConfig, SkillsConfig,
        WorkspaceSettings,
    };
    use tempfile::tempdir;

    fn test_bash_config() -> BashConfig {
        BashConfig {
            enabled: true,
            executable: test_bash_executable(),
            default_timeout_ms: 120_000,
            foreground_budget_ms: 20_000,
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
            workspace: WorkspaceSettings {
                path: root.to_string_lossy().into_owned(),
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            skills: SkillsConfig::default(),
            index: IndexSettings::default(),
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

    fn sleep_command() -> &'static str {
        "echo started; sleep 30"
    }

    #[test]
    fn initialize_builds_single_actor_eagerly_from_workspace_path() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = WorkspaceManager::default();

        let result = manager
            .initialize(&daemon_config(root.path(), cache.path(), 1_000_000))
            .unwrap();

        assert_eq!(result["index_ready"], true);
        assert!(result["file_count"].as_u64().unwrap() >= 1);
        assert!(result["phase_ms"]["actor_open"].is_number());

        // The actor is available immediately (eager start) — the first code tool
        // pays zero index-build cost.
        let actor = manager.actor().unwrap();
        assert_eq!(
            actor.root_path(),
            std::fs::canonicalize(root.path()).unwrap()
        );
    }

    #[test]
    fn initialize_rejects_missing_workspace_path() {
        let cache = tempdir().unwrap();
        let mut config = daemon_config(cache.path(), cache.path(), 1_000_000);
        config["workspace"]["path"] = json!("");

        let error = WorkspaceManager::default().initialize(&config).unwrap_err();

        assert_eq!(error.0.code, "INVALID_ARGUMENT");
        assert!(error.0.message.contains("workspace.path"));
    }

    #[test]
    fn initialize_rejects_nonexistent_workspace_path() {
        let cache = tempdir().unwrap();
        let mut config = daemon_config(cache.path(), cache.path(), 1_000_000);
        config["workspace"]["path"] = json!(cache.path().join("does-not-exist"));

        let error = WorkspaceManager::default().initialize(&config).unwrap_err();

        assert_eq!(error.0.code, "WORKSPACE_NOT_FOUND");
    }

    #[test]
    fn initialize_rejects_removed_multi_workspace_keys() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();

        for (mutate, expected_key) in [
            (
                Box::new(|c: &mut Value| c["workspaces"] = json!([])) as Box<dyn Fn(&mut Value)>,
                "workspaces",
            ),
            (
                Box::new(|c: &mut Value| c["workspace"]["defaultPath"] = json!("/x")),
                "defaultPath",
            ),
            (
                Box::new(|c: &mut Value| c["workspace"]["lockToDefault"] = json!(true)),
                "lockToDefault",
            ),
            (
                Box::new(|c: &mut Value| c["workspace"]["allowedRoots"] = json!(["/x"])),
                "allowedRoots",
            ),
        ] {
            let mut config = daemon_config(root.path(), cache.path(), 1_000_000);
            mutate(&mut config);
            let error = WorkspaceManager::default().initialize(&config).unwrap_err();
            assert_eq!(error.0.code, "LEGACY_WORKSPACE_CONFIG", "{expected_key}");
            let removed = error.0.details.as_ref().unwrap()["removed_keys"]
                .as_array()
                .unwrap();
            assert!(removed.iter().any(|k| k == expected_key), "{expected_key}");
        }
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

    #[test]
    fn code_tools_share_the_single_actor_across_sessions() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = WorkspaceManager::default();
        manager
            .initialize(&daemon_config(root.path(), cache.path(), 1_000_000))
            .unwrap();

        let actor_a = manager.actor().unwrap();
        let actor_b = manager.actor().unwrap();
        assert!(Arc::ptr_eq(&actor_a, &actor_b));
        assert_eq!(
            actor_a.root_path(),
            std::fs::canonicalize(root.path()).unwrap()
        );
    }

    #[test]
    fn code_tools_before_initialize_report_not_initialized() {
        let manager = WorkspaceManager::default();
        let Err(error) = manager.actor() else {
            panic!("actor() must fail before initialize");
        };
        assert_eq!(error.0.code, "NOT_INITIALIZED");
    }

    #[test]
    fn reinitialize_replaces_the_actor() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = WorkspaceManager::default();

        manager
            .initialize(&daemon_config(root.path(), cache.path(), 1_000_000))
            .unwrap();
        let first = manager.actor().unwrap();

        manager
            .initialize(&daemon_config(root.path(), cache.path(), 512_000))
            .unwrap();
        let second = manager.actor().unwrap();

        assert!(!Arc::ptr_eq(&first, &second));
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
    fn workspace_open_action_is_no_longer_supported() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = WorkspaceManager::default();
        manager
            .initialize(&daemon_config(root.path(), cache.path(), 1_000_000))
            .unwrap();

        let error = manager
            .workspace(&SessionKey::stdio(), &json!({"action": "open"}))
            .unwrap_err();
        assert_eq!(error.0.code, "INVALID_WORKSPACE_ACTION");
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
        assert_eq!(health["index_ready"], true);
        assert!(health["file_count"].as_u64().unwrap() >= 1);
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
            .dispatch(
                SessionKey::stdio(),
                "workspace",
                &json!({"action": "summary"}),
            )
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
    async fn bash_run_registry_is_scoped_by_session() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = Arc::new(WorkspaceManager::default());
        let config = daemon_config(root.path(), cache.path(), 1_000_000);
        manager
            .dispatch(SessionKey::stdio(), "initialize", &config)
            .await
            .unwrap();
        let session_a = SessionKey::new("http:a");
        let session_b = SessionKey::new("http:b");

        let started = manager
            .dispatch(
                session_a.clone(),
                "bash",
                &json!({"command": sleep_command(), "background": true}),
            )
            .await
            .unwrap();
        let run_id = started["run_id"].as_str().unwrap();

        let retry_from_other_session = manager
            .dispatch(
                session_b.clone(),
                "bash",
                &json!({"command": sleep_command(), "background": true}),
            )
            .await
            .unwrap_err();
        assert_eq!(retry_from_other_session.0.code, "RUN_BUSY");

        let cross_session_cancel = manager
            .dispatch(session_b, "bash_cancel", &json!({"run_id": run_id}))
            .await
            .unwrap_err();
        assert_eq!(cross_session_cancel.0.code, "RUN_NOT_FOUND");

        let _ = manager
            .dispatch(session_a, "bash_cancel", &json!({"run_id": run_id}))
            .await;
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
