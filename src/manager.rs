use crate::intelligence::{IntelligenceRequest, IntelligenceResponse, IntelligenceService};
use crate::model::{AppError, AppResult, DaemonConfig, WorkspaceConfig};
use crate::repository::CliGitBackend;
use crate::requests::bash::BashRequest;
use crate::requests::edit::EditRequest;
use crate::requests::git::GitAction;
use crate::requests::retrieval::CodeRetrieveRequest;
use crate::requests::workspace::{WorkspaceAction, WorkspaceRequest};
use crate::security::canonical_root;
use crate::workspace::Workspace;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Single-repository application. Holds exactly one `Workspace`, built eagerly
/// by `initialize` from `workspace.path`, and serves it to every request.
///
/// Every tool operation is a typed method; `crate::tools` binds each public
/// tool's request type to one of them.
pub struct Application {
    instance_id: Arc<str>,
    config: RwLock<Option<DaemonConfig>>,
    actor: RwLock<Option<Arc<Workspace>>>,
    intelligence: RwLock<Option<Arc<IntelligenceService>>>,
    lifecycle: Mutex<()>,
    /// Operations share it; `initialize` takes it exclusively so no operation
    /// observes a half-replaced workspace.
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

/// What `initialize` built.
#[derive(Debug, Clone, Serialize)]
pub struct InitInfo {
    pub version: &'static str,
    pub instance_id: String,
    pub workspace: InitWorkspace,
    pub index_ready: bool,
    pub file_count: usize,
    pub bash_available: bool,
    pub phase_ms: InitPhases,
}

#[derive(Debug, Clone, Serialize)]
pub struct InitWorkspace {
    pub id: String,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InitPhases {
    pub actor_open: u128,
    pub bash_probe: u128,
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

    /// Build the single repository actor eagerly (index scan + file watcher start
    /// happen inside `Workspace::open`) and pre-probe Bash so the first
    /// validated edit does not pay the discovery cost inline.
    pub async fn initialize(self: &Arc<Self>, config: DaemonConfig) -> AppResult<InitInfo> {
        let _operation = self.operation_gate.write().await;
        let manager = Arc::clone(self);
        tokio::task::spawn_blocking(move || manager.initialize_blocking(config))
            .await
            .map_err(AppError::internal)?
    }

    fn initialize_blocking(&self, config: DaemonConfig) -> AppResult<InitInfo> {
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
            actor.state_reader(),
        )?);
        let open_ms = open_started.elapsed().as_millis();

        let bash_probe_started = Instant::now();
        actor.probe_bash()?;
        let bash_probe_ms = bash_probe_started.elapsed().as_millis();

        let _lifecycle = self.lifecycle.lock();
        *self.config.write() = Some(config);
        *self.actor.write() = Some(actor.clone());
        *self.intelligence.write() = Some(intelligence);

        let file_count = actor.index_file_count();
        tracing::info!(
            workspace = %repo.path,
            file_count,
            open_ms,
            bash_probe_ms,
            "workspace initialized (eager index + watcher, bash pre-probed)"
        );

        Ok(InitInfo {
            version: env!("CARGO_PKG_VERSION"),
            instance_id: self.instance_id.to_string(),
            workspace: InitWorkspace {
                id: repo.id,
                name: repo.name,
                path: repo.path,
            },
            index_ready: true,
            file_count,
            bash_available: true,
            phase_ms: InitPhases {
                actor_open: open_ms,
                bash_probe: bash_probe_ms,
            },
        })
    }

    /// Liveness of the engine: whether a workspace is initialized and indexed.
    pub fn health(&self) -> Value {
        let config = self.config.read();
        let actor = self.actor.read().clone();
        json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "instance_id": self.instance_id.as_ref(),
            "initialized": config.is_some(),
            "index_ready": actor.is_some(),
            "file_count": actor.as_ref().map(|a| a.index_file_count()),
            "last_reconcile_ms_ago": actor.as_ref().map(|a| a.last_reconcile_elapsed_ms()),
        })
    }

    /// Run one operation under the shared gate and report its duration as
    /// `elapsed_ms`.
    async fn operation(&self, work: impl Future<Output = AppResult<Value>>) -> AppResult<Value> {
        let _operation = self.operation_gate.read().await;
        let started = Instant::now();
        let mut result = work.await?;
        if let Some(object) = result.as_object_mut() {
            object.insert(
                "elapsed_ms".to_owned(),
                json!(started.elapsed().as_millis()),
            );
        }
        Ok(result)
    }

    /// The `workspace` tool: summary, refresh, changes or diagnostics.
    pub async fn workspace(self: &Arc<Self>, request: WorkspaceRequest) -> AppResult<Value> {
        let mut result = self
            .operation(async {
                let actor = self.actor()?;
                run_blocking(move || match request.action {
                    WorkspaceAction::Summary => actor.summary(),
                    WorkspaceAction::Refresh => actor.refresh(request.force.unwrap_or(false)),
                    WorkspaceAction::Changes => actor.changes(
                        request.since_generation.unwrap_or(0),
                        request.source.as_deref(),
                        request.limit,
                    ),
                    WorkspaceAction::Diagnostics => actor.diagnostics(),
                })
                .await
            })
            .await?;
        if let Some(object) = result.as_object_mut() {
            object.insert("instance_id".to_owned(), json!(self.instance_id.as_ref()));
        }
        Ok(result)
    }

    /// The `code_retrieve` tool.
    pub async fn code_retrieve(self: &Arc<Self>, request: CodeRetrieveRequest) -> AppResult<Value> {
        self.operation(async {
            let actor = self.actor()?;
            run_blocking(move || actor.code_retrieve(&request)).await
        })
        .await
    }

    /// The `code_intelligence` tool. A rename preview is planned by the edit
    /// engine against the current snapshot.
    pub async fn code_intelligence(
        self: &Arc<Self>,
        request: IntelligenceRequest,
    ) -> AppResult<Value> {
        self.operation(async {
            let actor = self.actor()?;
            actor.snapshot_id()?;
            let service = self.intelligence.read().clone().ok_or_else(|| {
                AppError::new("NOT_INITIALIZED", "Workspace has not been initialized")
            })?;
            let response = tokio::task::spawn_blocking(move || service.execute(request))
                .await
                .map_err(AppError::internal)??;
            match response {
                IntelligenceResponse::Report(report) => Ok(report),
                IntelligenceResponse::RenamePreview(rename) => {
                    let snapshot = actor.snapshot_id()?;
                    let mut preview = actor.code_edit(rename.edit_request(Some(snapshot))).await?;
                    if let Some(object) = preview.as_object_mut() {
                        object.insert("intelligence".to_owned(), rename.into_report());
                    }
                    Ok(preview)
                }
            }
        })
        .await
    }

    /// The edit tools. An edit without a snapshot precondition is planned
    /// against the current snapshot.
    pub async fn code_edit(self: &Arc<Self>, mut request: EditRequest) -> AppResult<Value> {
        self.operation(async {
            let actor = self.actor()?;
            if request.snapshot_id.is_none() {
                request.snapshot_id = Some(actor.snapshot_id()?);
            }
            actor.code_edit(request).await
        })
        .await
    }

    /// The `git_*` tools.
    pub async fn git(self: &Arc<Self>, action: GitAction) -> AppResult<Value> {
        self.operation(async { self.actor()?.git(action).await })
            .await
    }

    /// The `bash*` tools.
    pub async fn bash(self: &Arc<Self>, request: BashRequest) -> AppResult<Value> {
        self.operation(async { self.actor()?.run(request).await })
            .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{parse_daemon_config, BashConfig, PolicyConfig, WorkspaceSettings};
    use crate::test_bash_executable;
    use tempfile::tempdir;

    fn daemon_config(root: &Path, cache: &Path, max_file_bytes: usize) -> DaemonConfig {
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .expect("git must be available for tests");
        assert!(status.success());
        DaemonConfig {
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
                bash: BashConfig {
                    executable: test_bash_executable(),
                    default_timeout_ms: 120_000,
                    foreground_budget_ms: 20_000,
                    max_timeout_ms: 300_000,
                    max_output_chars: 30_000,
                },
            },
            cache_root: cache.to_string_lossy().into_owned(),
        }
    }

    fn summary() -> WorkspaceRequest {
        WorkspaceRequest {
            action: WorkspaceAction::Summary,
            force: None,
            since_generation: None,
            source: None,
            limit: 200,
        }
    }

    #[tokio::test]
    async fn initialize_builds_single_actor_eagerly_from_workspace_path() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = Arc::new(Application::default());

        let info = manager
            .initialize(daemon_config(root.path(), cache.path(), 1_000_000))
            .await
            .unwrap();

        assert!(info.index_ready);
        assert_eq!(info.file_count, 1);
        assert_eq!(
            PathBuf::from(info.workspace.path),
            std::fs::canonicalize(root.path()).unwrap()
        );
        // The actor is available immediately (eager start) — the first code tool
        // pays zero index-build cost.
        let actor = manager.actor().unwrap();
        assert_eq!(
            actor.root_path(),
            std::fs::canonicalize(root.path()).unwrap()
        );
    }

    #[tokio::test]
    async fn initialize_rejects_unusable_workspaces_and_config_versions() {
        let cache = tempdir().unwrap();
        let mut empty_path = daemon_config(cache.path(), cache.path(), 1_000_000);
        empty_path.workspace.path = String::new();
        let mut missing = daemon_config(cache.path(), cache.path(), 1_000_000);
        missing.workspace.path = cache
            .path()
            .join("does-not-exist")
            .to_string_lossy()
            .into_owned();
        let mut version_one = daemon_config(cache.path(), cache.path(), 1_000_000);
        version_one.config_version = 1;

        let mut errors = Vec::new();
        for config in [empty_path, missing, version_one] {
            let error = Arc::new(Application::default())
                .initialize(config)
                .await
                .unwrap_err();
            errors.push((error.0.code, error.0.message));
        }

        assert_eq!(errors[0].0, "INVALID_ARGUMENT");
        assert!(errors[0].1.contains("workspace.path"));
        assert_eq!(errors[1].0, "WORKSPACE_NOT_FOUND");
        assert_eq!(errors[2].0, "INVALID_ARGUMENT");
        assert!(errors[2].1.contains("configVersion must be 2"));
    }

    #[test]
    fn config_parsing_strictly_rejects_unknown_and_missing_keys() {
        let root = tempdir().unwrap();
        let valid = json!({
            "configVersion": 2,
            "workspace": {"path": root.path()},
            "policy": {
                "maxFileBytes": 1000000,
                "maxContextChars": 50000,
                "maxSearchResults": 100,
                "bash": {}
            },
            "cacheRoot": root.path()
        });
        parse_daemon_config(&valid).unwrap();

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
                Box::new(|c: &mut Value| c["workspace"]["allowedRoots"] = json!(["/x"])),
                "allowedRoots",
            ),
            (
                Box::new(|c: &mut Value| c["policy"]["unexpectedPolicyField"] = json!(true)),
                "unexpectedPolicyField",
            ),
            (
                Box::new(|c: &mut Value| {
                    c.as_object_mut().unwrap().remove("configVersion");
                }),
                "configVersion",
            ),
        ] {
            let mut config = valid.clone();
            mutate(&mut config);
            let error = parse_daemon_config(&config).unwrap_err();
            assert_eq!(error.0.code, "INVALID_CONFIG", "{expected_key}");
            assert!(error.0.details.as_ref().unwrap()["path"].is_string());
            assert!(error.0.message.contains(expected_key), "{expected_key}");
        }
    }

    #[tokio::test]
    async fn code_tools_share_the_single_actor_across_clients() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = Arc::new(Application::default());
        manager
            .initialize(daemon_config(root.path(), cache.path(), 1_000_000))
            .await
            .unwrap();

        let actor_a = manager.actor().unwrap();
        let actor_b = manager.actor().unwrap();
        assert!(Arc::ptr_eq(&actor_a, &actor_b));
    }

    #[tokio::test]
    async fn code_tools_before_initialize_report_not_initialized() {
        let manager = Arc::new(Application::default());

        let error = manager.workspace(summary()).await.unwrap_err();

        assert_eq!(error.0.code, "NOT_INITIALIZED");
    }

    #[tokio::test]
    async fn reinitialize_replaces_the_actor_and_keeps_the_instance_id() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = Arc::new(Application::default());

        manager
            .initialize(daemon_config(root.path(), cache.path(), 1_000_000))
            .await
            .unwrap();
        let first = manager.actor().unwrap();
        manager
            .initialize(daemon_config(root.path(), cache.path(), 512_000))
            .await
            .unwrap();
        let second = manager.actor().unwrap();

        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(
            manager.health()["instance_id"],
            manager.instance_id().as_ref()
        );
    }

    #[tokio::test]
    async fn health_and_workspace_summary_expose_shared_instance() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let manager = Arc::new(Application::default());
        manager
            .initialize(daemon_config(root.path(), cache.path(), 1_000_000))
            .await
            .unwrap();

        let summary = manager.workspace(summary()).await.unwrap();
        let health = manager.health();

        assert_eq!(summary["instance_id"], health["instance_id"]);
        assert!(summary["elapsed_ms"].is_number());
        assert_eq!(health["index_ready"], true);
        assert_eq!(health["file_count"], 1);
    }
}
