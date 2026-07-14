use crate::contracts;
use crate::intelligence::IntelligenceService;
use crate::model::{AppError, AppResult, DaemonConfig, WorkspaceConfig};
use crate::repository::CliGitBackend;
use crate::security::canonical_root;
use crate::workspace::Workspace;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Adapt each strict public tool to the shared internal engines.
pub fn prepare_tool_request(method: &str, input: Value) -> AppResult<Value> {
    crate::tools::validate_input_fields(method, &input)?;
    let mut params = input
        .as_object()
        .cloned()
        .ok_or_else(|| AppError::invalid("tool input must be an object"))?;

    let mutation = match method {
        "code_write" => Some((
            "create",
            &["path", "content", "overwrite", "expected_hash"][..],
        )),
        "code_replace" => Some((
            "replace",
            &[
                "path",
                "old_text",
                "new_text",
                "expected_replacements",
                "expected_hash",
                "handle",
            ][..],
        )),
        "code_replace_range" => Some(("replace_range", &["path", "handle", "new_text"][..])),
        "code_insert" => Some((
            "insert",
            &[
                "path",
                "content",
                "anchor_symbol",
                "position",
                "expected_hash",
            ][..],
        )),
        "code_delete" => Some(("delete", &["path", "expected_hash"][..])),
        "code_rename" => Some(("rename", &["path", "to", "expected_hash"][..])),
        _ => None,
    };
    if let Some((kind, fields)) = mutation {
        let mut change = serde_json::Map::new();
        change.insert("kind".into(), Value::String(kind.into()));
        for field in fields {
            if let Some(value) = params.remove(*field) {
                change.insert((*field).into(), value);
            }
        }
        if method == "code_write" && !change.contains_key("overwrite") {
            change.insert("overwrite".into(), Value::Bool(true));
        }
        params.insert("changes".into(), Value::Array(vec![Value::Object(change)]));
    }
    if method == "code_preview" {
        params.insert("preview".into(), Value::Bool(true));
    }
    let git_action = method.strip_prefix("git_").and_then(|name| match name {
        "status" | "diff" | "log" | "show" | "blame" | "preflight" | "stage" | "commit"
        | "restore" | "push" => Some(name),
        _ => None,
    });
    if let Some(action) = git_action {
        params.insert("action".into(), Value::String(action.into()));
    }
    Ok(Value::Object(params))
}

/// Single-repository manager. Holds exactly one `Workspace`, built eagerly
/// at `initialize` from `workspace.path`, and serves it to every request.
pub struct Application {
    instance_id: Arc<str>,
    config: RwLock<Option<DaemonConfig>>,
    actor: RwLock<Option<Arc<Workspace>>>,
    intelligence: RwLock<Option<Arc<IntelligenceService>>>,
    lifecycle: Mutex<()>,
    operation_gate: tokio::sync::RwLock<()>,
}

impl Default for Application {
    fn default() -> Self {
        Self {
            instance_id: Arc::from(uuid::Uuid::new_v4().simple().to_string()),
            config: RwLock::new(None),
            actor: RwLock::new(None),
            intelligence: RwLock::new(None),
            lifecycle: Mutex::new(()),
            operation_gate: tokio::sync::RwLock::new(()),
        }
    }
}

async fn run_blocking<F>(operation: F) -> AppResult<Value>
where
    F: FnOnce() -> AppResult<Value> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(AppError::internal)?
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

impl Application {
    pub fn instance_id(&self) -> Arc<str> {
        Arc::clone(&self.instance_id)
    }

    pub async fn dispatch(self: &Arc<Self>, method: &str, params: &Value) -> AppResult<Value> {
        if method == "initialize" {
            let _operation = self.operation_gate.write().await;
            self.dispatch_inner(method, params).await
        } else {
            let _operation = self.operation_gate.read().await;
            self.dispatch_inner(method, params).await
        }
    }

    async fn dispatch_inner(self: &Arc<Self>, method: &str, params: &Value) -> AppResult<Value> {
        let started = Instant::now();
        let mut result = match method {
            "initialize" => {
                let manager = Arc::clone(self);
                let params = params.clone();
                run_blocking(move || manager.initialize(&params)).await
            }
            "health" => self.health(),
            "shutdown" => Ok(json!({"ok": true})),
            "workspace" => {
                let manager = Arc::clone(self);
                let params = params.clone();
                run_blocking(move || manager.workspace(&params)).await
            }
            "code_retrieve" => {
                let actor = self.actor()?;
                let params = params.clone();
                run_blocking(move || actor.code_retrieve(&params)).await
            }
            "code_intelligence" => {
                let actor = self.actor()?;
                actor.summary_ids()?;
                let service = self.intelligence.read().clone().ok_or_else(|| {
                    AppError::new("NOT_INITIALIZED", "Workspace has not been initialized")
                })?;
                let params = params.clone();
                let operation = params
                    .get("operation")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let semantic = run_blocking(move || service.execute(&params)).await?;
                if operation == "rename_preview" {
                    let changes = semantic
                        .get("changes")
                        .cloned()
                        .unwrap_or_else(|| json!([]));
                    let snapshot = actor
                        .summary_ids()?
                        .get("snapshot_id")
                        .cloned()
                        .unwrap_or(Value::Null);
                    let mut preview = actor
                        .code_edit(
                            &json!({"preview":true,"changes":changes,"snapshot_id":snapshot}),
                        )
                        .await?;
                    if let Some(object) = preview.as_object_mut() {
                        object.insert("intelligence".to_owned(), semantic);
                    }
                    Ok(preview)
                } else {
                    Ok(semantic)
                }
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
                actor.code_edit(&prepared).await
            }
            "git_status" | "git_diff" | "git_log" | "git_show" | "git_blame" | "git_preflight"
            | "git_stage" | "git_commit" | "git_restore" | "git_push" => {
                let actor = self.actor()?;
                let params = params.clone();
                run_blocking(move || actor.git(&params)).await
            }
            "bash" | "bash_status" | "bash_output" | "bash_cancel" => {
                let prepared = contracts::normalize_bash_request(method, params)?;
                self.actor()?.run(&prepared).await
            }
            _ => Err(AppError::details(
                "METHOD_NOT_FOUND",
                "Unknown daemon method",
                json!({"method": method}),
            )),
        }?;
        let elapsed_ms = started.elapsed().as_millis();
        add_operation_elapsed(&mut result, elapsed_ms);
        if method == "workspace" {
            if let Some(object) = result.as_object_mut() {
                object.insert("instance_id".to_owned(), json!(self.instance_id.as_ref()));
            }
        }
        Ok(result)
    }

    /// Build the single repository actor eagerly (index scan + file watcher start
    /// happen inside `Workspace::open`) and pre-probe Bash so the first
    /// validated edit does not pay the discovery cost inline.
    fn initialize(&self, params: &Value) -> AppResult<Value> {
        let config = crate::model::parse_daemon_config(params)?;
        if config.config_version != 2 {
            return Err(AppError::invalid("configVersion must be 2"));
        }

        let repo = single_repo_config(&config)?;
        CliGitBackend::require_worktree(Path::new(&repo.path))?;
        let open_started = Instant::now();
        let actor = Arc::new(Workspace::open(
            &repo,
            config.policy.clone(),
            PathBuf::from(config.cache_root.clone()),
        )?);
        let intelligence = Arc::new(IntelligenceService::new(
            PathBuf::from(&repo.path),
            config.intelligence.clone(),
            actor.id.clone(),
            actor.reference_index(),
            actor.reference_snapshot(),
        ));
        let open_ms = open_started.elapsed().as_millis();

        let bash_probe_started = Instant::now();
        actor.probe_bash()?;
        let bash_available = true;
        let bash_probe_ms = bash_probe_started.elapsed().as_millis();

        let _lifecycle = self.lifecycle.lock();
        *self.config.write() = Some(config);
        *self.actor.write() = Some(actor.clone());
        *self.intelligence.write() = Some(intelligence);

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
            "instance_id": self.instance_id.as_ref(),
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
            "instance_id": self.instance_id.as_ref(),
            "initialized": config.is_some(),
            "index_ready": actor.is_some(),
            "file_count": actor.as_ref().map(|a| a.index_file_count()),
            "last_reconcile_ms_ago": actor.as_ref().map(|a| a.last_reconcile_elapsed_ms()),
        }))
    }

    fn workspace(&self, params: &Value) -> AppResult<Value> {
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
                    actor.summary()
                }
            }
            "refresh" => self.actor()?.refresh(
                params
                    .get("force")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
            "changes" => self.actor()?.changes(params),
            "diagnostics" => self.actor()?.diagnostics(),
            action => Err(AppError::details(
                "INVALID_WORKSPACE_ACTION",
                "Unknown workspace action",
                json!({"action": action}),
            )),
        }
    }

    fn actor(&self) -> AppResult<Arc<Workspace>> {
        self.actor.read().clone().ok_or_else(|| {
            AppError::new(
                "NOT_INITIALIZED",
                "Daemon has not been initialized with a repository",
            )
        })
    }
}

fn add_operation_elapsed(value: &mut Value, elapsed_ms: u128) {
    if let Some(object) = value.as_object_mut() {
        object.insert("elapsed_ms".to_owned(), json!(elapsed_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BashConfig, PolicyConfig, WorkspaceSettings};
    use crate::test_bash_executable;
    use tempfile::tempdir;

    fn test_bash_config() -> BashConfig {
        BashConfig {
            executable: test_bash_executable(),
            default_timeout_ms: 120_000,
            foreground_budget_ms: 20_000,
            max_timeout_ms: 300_000,
            max_output_chars: 30_000,
        }
    }

    fn daemon_config(
        root: &std::path::Path,
        cache: &std::path::Path,
        max_file_bytes: usize,
    ) -> Value {
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .expect("git must be available for tests");
        assert!(status.success());
        serde_json::to_value(DaemonConfig {
            config_version: 2,
            workspace: WorkspaceSettings {
                path: root.to_string_lossy().into_owned(),
                artifact_paths: Vec::new(),
                exclude_paths: Vec::new(),
            },
            intelligence: crate::model::IntelligenceSettings::default(),
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
        let manager = Application::default();

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

        let error = Application::default().initialize(&config).unwrap_err();

        assert_eq!(error.0.code, "INVALID_ARGUMENT");
        assert!(error.0.message.contains("workspace.path"));
    }

    #[test]
    fn initialize_requires_exactly_config_version_two() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let mut config = daemon_config(root.path(), cache.path(), 1_000_000);
        config["configVersion"] = json!(1);
        let error = Application::default().initialize(&config).unwrap_err();
        assert_eq!(error.0.code, "INVALID_ARGUMENT");
        assert!(error.0.message.contains("configVersion must be 2"));

        config.as_object_mut().unwrap().remove("configVersion");
        let missing = Application::default().initialize(&config).unwrap_err();
        assert_eq!(missing.0.code, "INVALID_CONFIG");
        assert!(missing.0.details.unwrap()["path"].is_string());
    }

    #[test]
    fn initialize_rejects_nonexistent_workspace_path() {
        let cache = tempdir().unwrap();
        let mut config = daemon_config(cache.path(), cache.path(), 1_000_000);
        config["workspace"]["path"] = json!(cache.path().join("does-not-exist"));

        let error = Application::default().initialize(&config).unwrap_err();

        assert_eq!(error.0.code, "WORKSPACE_NOT_FOUND");
    }

    #[test]
    fn initialize_strictly_rejects_unknown_workspace_keys() {
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
            let error = Application::default().initialize(&config).unwrap_err();
            assert_eq!(error.0.code, "INVALID_CONFIG", "{expected_key}");
            assert!(error.0.details.as_ref().unwrap()["path"].is_string());
            assert!(error.0.message.contains(expected_key), "{expected_key}");
        }
    }

    #[test]
    fn initialize_strictly_rejects_unknown_execution_keys() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let valid = daemon_config(root.path(), cache.path(), 1_000_000);
        Application::default().initialize(&valid).unwrap();

        let key = "unexpectedPolicyField";
        let mut invalid = valid.clone();
        invalid["policy"][key] = json!(true);
        let error = Application::default().initialize(&invalid).unwrap_err();
        assert_eq!(error.0.code, "INVALID_CONFIG");
        assert!(error.0.message.contains(key));

        let mut invalid_root = valid;
        invalid_root["unexpectedRootField"] = json!({});
        let error = Application::default()
            .initialize(&invalid_root)
            .unwrap_err();
        assert_eq!(error.0.code, "INVALID_CONFIG");
        assert!(error.0.message.contains("unexpectedRootField"));
    }

    #[test]
    fn code_tools_share_the_single_actor_across_clients() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = Application::default();
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
        let manager = Application::default();
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
        let manager = Application::default();

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
        let manager = Application::default();
        manager
            .initialize(&daemon_config(root.path(), cache.path(), 1_000_000))
            .unwrap();

        let summary = manager
            .workspace(&json!({"action": "summary", "_summary_ids": true}))
            .unwrap();

        assert!(summary.get("workspace_id").is_some());
        assert!(summary.get("snapshot_id").is_some());
        assert!(summary.get("generation").is_some());
        assert!(summary.get("root").is_none());
        assert!(summary.get("mutations").is_none());
    }

    #[test]
    fn workspace_rejects_unknown_actions() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = Application::default();
        manager
            .initialize(&daemon_config(root.path(), cache.path(), 1_000_000))
            .unwrap();

        let error = manager
            .workspace(&json!({"action": "unknown_action"}))
            .unwrap_err();
        assert_eq!(error.0.code, "INVALID_WORKSPACE_ACTION");
    }

    #[tokio::test]
    async fn health_and_workspace_summary_expose_shared_instance() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = Arc::new(Application::default());
        let config = daemon_config(root.path(), cache.path(), 1_000_000);
        manager.dispatch("initialize", &config).await.unwrap();

        let summary = manager
            .dispatch("workspace", &json!({"action": "summary"}))
            .await
            .unwrap();
        let health = manager.dispatch("health", &json!({})).await.unwrap();

        assert_eq!(summary["instance_id"], health["instance_id"]);
        assert_eq!(health["index_ready"], true);
        assert!(health["file_count"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn reinitialize_preserves_process_instance_id() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = Arc::new(Application::default());
        let config = daemon_config(root.path(), cache.path(), 1_000_000);
        manager.dispatch("initialize", &config).await.unwrap();
        manager
            .dispatch("workspace", &json!({"action": "summary"}))
            .await
            .unwrap();
        manager.dispatch("initialize", &config).await.unwrap();

        let health = manager.dispatch("health", &json!({})).await.unwrap();

        assert_eq!(health["instance_id"], manager.instance_id().as_ref());
    }

    #[tokio::test]
    async fn bash_dispatch_validates_command_fields_and_forces_start_action() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = Arc::new(Application::default());
        let config = daemon_config(root.path(), cache.path(), 1_000_000);
        manager.dispatch("initialize", &config).await.unwrap();

        let raw_command = manager
            .dispatch("bash", &json!({"command": ["not-a-string"]}))
            .await
            .unwrap_err();
        assert_eq!(raw_command.0.code, "INVALID_BASH_REQUEST");

        let spoofed_action = manager
            .dispatch(
                "bash",
                &json!({"command": "printf test", "action": "cancel"}),
            )
            .await
            .unwrap_err();
        assert_eq!(spoofed_action.0.code, "INVALID_BASH_REQUEST");
    }

    #[tokio::test]
    async fn bash_run_registry_is_shared_by_all_clients() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = Arc::new(Application::default());
        let config = daemon_config(root.path(), cache.path(), 1_000_000);
        manager.dispatch("initialize", &config).await.unwrap();
        let started = manager
            .dispatch(
                "bash",
                &json!({"command": sleep_command(), "background": true}),
            )
            .await
            .unwrap();
        let run_id = started["run_id"].as_str().unwrap();

        let concurrent_retry = manager
            .dispatch(
                "bash",
                &json!({"command": sleep_command(), "background": true}),
            )
            .await
            .unwrap_err();
        assert_eq!(concurrent_retry.0.code, "RUN_BUSY");

        let cross_client_cancel = manager
            .dispatch("bash_cancel", &json!({"run_id": run_id}))
            .await
            .unwrap();
        assert_eq!(cross_client_cancel["run_id"], run_id);
    }
}
