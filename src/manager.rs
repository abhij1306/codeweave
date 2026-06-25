use crate::model::{required_str, AppError, AppResult, DaemonConfig, WorkspaceConfig};
use crate::security::validate_relative;
use crate::workspace::WorkspaceActor;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
#[cfg(test)]
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[derive(Default)]
pub struct WorkspaceManager {
    config: RwLock<Option<DaemonConfig>>,
    active: RwLock<Option<Arc<WorkspaceActor>>>,
    lifecycle: Mutex<()>,
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

impl WorkspaceManager {
    pub async fn dispatch(self: &Arc<Self>, method: &str, params: &Value) -> AppResult<Value> {
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
            "code_context" => {
                let actor = self.active_actor()?;
                let params = params.clone();
                run_blocking(move || actor.code_context(&params)).await
            }
            "code_search" => {
                let actor = self.active_actor()?;
                let params = params.clone();
                run_blocking(move || actor.code_search(&params)).await
            }
            "code_fetch" => {
                let actor = self.active_actor()?;
                let params = params.clone();
                run_blocking(move || actor.code_fetch(&params)).await
            }
            "code_write" | "code_replace" | "code_insert" | "code_delete" | "code_rename" => {
                let actor = self.active_actor()?;
                let mut prepared = params.clone();
                if prepared.get("snapshot_id").is_none() {
                    if let Some(snapshot) = actor.summary_ids()?.get("snapshot_id") {
                        prepared["snapshot_id"] = snapshot.clone();
                    }
                }
                actor.code_edit(&prepared).await
            }
            "git" => {
                let actor = self.active_actor()?;
                let params = params.clone();
                run_blocking(move || actor.git(&params)).await
            }
            "run" => self.active_actor()?.run(params).await,
            _ => Err(AppError::details(
                "METHOD_NOT_FOUND",
                "Unknown daemon method",
                json!({"method": method}),
            )),
        }?;
        add_operation_elapsed(&mut result, started.elapsed().as_millis());
        Ok(result)
    }

    fn initialize(&self, params: &Value) -> AppResult<Value> {
        let config: DaemonConfig = serde_json::from_value(params.clone())?;
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
        let mut current_config = self.config.write();
        let mut active = self.active.write();
        *current_config = Some(config.clone());
        *active = None;
        Ok(
            json!({"ok": true, "version": env!("CARGO_PKG_VERSION"), "configured_workspaces": config.workspaces.iter().map(|w| json!({"id": w.id, "name": w.name})).collect::<Vec<_>>() }),
        )
    }

    fn health(&self) -> AppResult<Value> {
        let config = self.config.read();
        Ok(json!({
            "ok": true, "version": env!("CARGO_PKG_VERSION"), "initialized": config.is_some(),
            "active_workspace": self.active.read().as_ref().map(|actor| actor.id.clone()),
            "configured_workspaces": config.as_ref().map(|c| c.workspaces.len()).unwrap_or(0),
            "single_repository_mode": true
        }))
    }

    fn workspace(&self, params: &Value) -> AppResult<Value> {
        match params
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("open")
        {
            "open" => {
                let ids_only = params
                    .get("_summary_ids")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let summarize = |actor: &WorkspaceActor| {
                    if ids_only {
                        actor.summary_ids()
                    } else {
                        actor.summary()
                    }
                };
                let started = Instant::now();
                let config = self.config()?;
                let requested_path = params.get("path").and_then(Value::as_str);
                let workspace = if let Some(path) = requested_path {
                    self.dynamic_workspace(&config, path)?
                } else if let Some(path) = config.workspace.default_path.as_deref() {
                    self.dynamic_workspace(&config, path)?
                } else if config.workspaces.len() == 1 {
                    config.workspaces[0].clone()
                } else {
                    return Err(AppError::details(
                        "WORKSPACE_REQUIRED",
                        "Pass an absolute path or configure workspace.defaultPath",
                        json!({"suggested_action": "workspace(action='open', path='...')"}),
                    ));
                };

                let canonical_workspace = self.dynamic_workspace(&config, &workspace.path)?;
                if let Some(actor) = self.active.read().clone() {
                    if actor.root_path() == Path::new(&canonical_workspace.path) {
                        let mut summary = summarize(&actor)?;
                        add_elapsed(&mut summary, started.elapsed().as_millis(), true);
                        return Ok(summary);
                    }
                    let running_tasks = actor.running_task_count();
                    if running_tasks > 0 {
                        return Err(AppError::details(
                            "WORKSPACE_BUSY",
                            "Cannot switch repositories while tasks are running",
                            json!({"running_tasks": running_tasks, "suggested_action": "Wait for or cancel active tasks before switching repositories"}),
                        ));
                    }
                }

                let _lifecycle = self.lifecycle.lock();
                let actor = Arc::new(WorkspaceActor::open(
                    &canonical_workspace,
                    config.policy.clone(),
                    config.tasks.clone(),
                    PathBuf::from(config.cache_root),
                )?);
                let previous = self.active.write().replace(actor.clone());
                drop(previous);
                let mut summary = summarize(&actor)?;
                add_elapsed(&mut summary, started.elapsed().as_millis(), false);
                Ok(summary)
            }
            "summary" => {
                let actor = self.active_actor()?;
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
            "refresh" => self.active_actor()?.refresh(
                params
                    .get("force")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
            "changes" => self.active_actor()?.changes(params),
            "skills" => self.list_skills(),
            "skill" => self.read_skill(params),
            action => Err(AppError::details(
                "INVALID_WORKSPACE_ACTION",
                "Unknown workspace action",
                json!({"action": action}),
            )),
        }
    }

    fn active_actor(&self) -> AppResult<Arc<WorkspaceActor>> {
        if let Some(actor) = self.active.read().clone() {
            return Ok(actor);
        }

        let config = self.config()?;
        if config.workspace.default_path.is_some() || config.workspaces.len() == 1 {
            self.workspace(&json!({"action": "open", "_summary_ids": true}))?;
            if let Some(actor) = self.active.read().clone() {
                return Ok(actor);
            }
        }

        Err(AppError::details(
            "WORKSPACE_NOT_OPEN",
            "No repository is active. Open one explicitly or configure workspace.defaultPath.",
            json!({
                "suggested_action": "workspace(action='open', path='C:/absolute/path/to/project')"
            }),
        ))
    }

    fn dynamic_workspace(
        &self,
        config: &DaemonConfig,
        requested: &str,
    ) -> AppResult<WorkspaceConfig> {
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
        let name = canonical
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("Repository");
        let canonical_text = canonical.to_string_lossy().into_owned();
        let mut hasher = DefaultHasher::new();
        canonical_text.hash(&mut hasher);
        let workspace_id = format!("repo-{:016x}", hasher.finish());
        let workspace = WorkspaceConfig {
            id: workspace_id,
            name: name.to_owned(),
            path: canonical_text,
            artifact_paths: config.workspace.artifact_paths.clone(),
        };
        Ok(workspace)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PolicyConfig, SkillsConfig, WorkspaceConfig, WorkspaceSettings};
    use tempfile::tempdir;

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
            }],
            workspace: Default::default(),
            skills: Default::default(),
            policy: PolicyConfig {
                max_file_bytes,
                max_context_chars: 50_000,
                max_search_results: 100,
                max_task_output_chars: 30_000,
                shell_enabled: false,
                allowed_commands: vec!["cargo".to_owned()],
                task_retention_hours: None,
            },
            tasks: HashMap::new(),
            cache_root: cache.to_string_lossy().into_owned(),
        })
        .unwrap()
    }

    #[test]
    fn code_tools_lazily_open_the_configured_default_repository() {
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
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                max_task_output_chars: 30_000,
                shell_enabled: false,
                allowed_commands: vec!["cargo".to_owned()],
                task_retention_hours: None,
            },
            tasks: HashMap::new(),
            cache_root: cache.path().to_string_lossy().into_owned(),
        })
        .unwrap();
        manager.initialize(&config).unwrap();

        assert!(manager.active.read().is_none());
        let actor = manager.active_actor().unwrap();
        assert_eq!(
            actor.root_path(),
            std::fs::canonicalize(root.path()).unwrap()
        );
        assert!(manager.active.read().is_some());
    }

    #[test]
    fn dynamic_open_switches_single_actor_and_separates_caches() {
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
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                max_task_output_chars: 30_000,
                shell_enabled: false,
                allowed_commands: vec!["cargo".to_owned()],
                task_retention_hours: None,
            },
            tasks: HashMap::new(),
            cache_root: cache.to_string_lossy().into_owned(),
        })
        .unwrap();
        manager.initialize(&config).unwrap();
        manager
            .workspace(&json!({"action": "open", "path": first}))
            .unwrap();
        assert!(manager.active.read().is_some());
        manager
            .workspace(&json!({"action": "open", "path": second}))
            .unwrap();
        assert!(manager.active.read().is_some());
        let actor = manager.active.read().clone().unwrap();
        assert!(actor.id.starts_with("repo-"));
        assert_eq!(actor.root_path(), std::fs::canonicalize(second).unwrap());
        assert_eq!(std::fs::read_dir(cache.join("repos")).unwrap().count(), 2);
    }

    #[test]
    fn dynamic_open_rejects_paths_outside_allowed_roots() {
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
            },
            skills: SkillsConfig::default(),
            policy: PolicyConfig {
                max_file_bytes: 1_000_000,
                max_context_chars: 50_000,
                max_search_results: 100,
                max_task_output_chars: 30_000,
                shell_enabled: false,
                allowed_commands: vec!["cargo".to_owned()],
                task_retention_hours: None,
            },
            tasks: HashMap::new(),
            cache_root: allowed.path().join("cache").to_string_lossy().into_owned(),
        })
        .unwrap();
        manager.initialize(&config).unwrap();
        manager
            .workspace(&json!({"action": "open", "path": allowed.path()}))
            .unwrap();
        let error = manager
            .workspace(&json!({"action": "open", "path": outside.path()}))
            .unwrap_err();
        assert_eq!(error.0.code, "WORKSPACE_OUTSIDE_ALLOWED_ROOTS");
        assert_eq!(
            manager.active.read().as_ref().unwrap().root_path(),
            std::fs::canonicalize(allowed.path()).unwrap()
        );
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
            .workspace(&json!({"action": "open", "workspace": "main"}))
            .unwrap();
        assert!(manager.active.read().is_some());

        manager
            .initialize(&daemon_config(root.path(), cache.path(), 512_000))
            .unwrap();
        assert!(manager.active.read().is_none());
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
            .workspace(&json!({"action": "summary", "_summary_ids": true}))
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
