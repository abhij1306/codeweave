use crate::model::{required_str, AppError, AppResult, DaemonConfig, WorkspaceConfig};
use crate::security::validate_relative;
use crate::workspace::WorkspaceActor;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[derive(Default)]
pub struct WorkspaceManager {
    config: RwLock<Option<DaemonConfig>>,
    actors: RwLock<HashMap<String, Arc<WorkspaceActor>>>,
    opening: Mutex<HashMap<String, Arc<Mutex<()>>>>,
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
                let actor = self.actor(params)?;
                let params = params.clone();
                run_blocking(move || actor.code_context(&params)).await
            }
            "code_search" => {
                let actor = self.actor(params)?;
                let params = params.clone();
                run_blocking(move || actor.code_search(&params)).await
            }
            "code_fetch" => {
                let actor = self.actor(params)?;
                let params = params.clone();
                run_blocking(move || actor.code_fetch(&params)).await
            }
            "code_edit" | "code_write" | "code_replace" | "code_insert" | "code_delete"
            | "code_rename" => self.actor(params)?.code_edit(params).await,
            "git" => {
                let actor = self.actor(params)?;
                let params = params.clone();
                run_blocking(move || actor.git(&params)).await
            }
            "run" => self.actor(params)?.run(params).await,
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
        let mut actors = self.actors.write();
        *current_config = Some(config.clone());
        actors.clear();
        self.opening.lock().clear();
        Ok(
            json!({"ok": true, "version": env!("CARGO_PKG_VERSION"), "configured_workspaces": config.workspaces.iter().map(|w| json!({"id": w.id, "name": w.name})).collect::<Vec<_>>() }),
        )
    }

    fn health(&self) -> AppResult<Value> {
        let config = self.config.read();
        Ok(json!({
            "ok": true, "version": env!("CARGO_PKG_VERSION"), "initialized": config.is_some(),
            "open_workspaces": self.actors.read().keys().cloned().collect::<Vec<_>>(),
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
                let requested = params.get("workspace").and_then(Value::as_str);
                let workspace = if let Some(path) = requested_path {
                    self.dynamic_workspace(&config, path)?
                } else if let Some(path) = config.workspace.default_path.as_deref() {
                    self.dynamic_workspace(&config, path)?
                } else {
                    match requested {
                        Some(value) => config.workspaces.iter().find(|item| item.id == value || item.name.eq_ignore_ascii_case(value)),
                        None if config.workspaces.len() == 1 => config.workspaces.first(),
                        None => return Err(AppError::details("WORKSPACE_REQUIRED", "Pass path or configure workspace.defaultPath", json!({"available": config.workspaces.iter().map(|w| json!({"id": w.id, "name": w.name})).collect::<Vec<_>>()}))),
                    }
                    .ok_or_else(|| AppError::details("WORKSPACE_NOT_CONFIGURED", "Workspace is not configured", json!({"workspace": requested})))?
                    .clone()
                };

                if let Some(actor) = self.actors.read().values().next().cloned() {
                    if actor.root_path() == Path::new(&workspace.path) {
                        let mut summary = summarize(&actor)?;
                        add_elapsed(&mut summary, started.elapsed().as_millis(), true);
                        return Ok(summary);
                    }
                }

                let _lifecycle = self.lifecycle.lock();
                self.actors.write().clear();
                let actor = Arc::new(WorkspaceActor::open(
                    &workspace,
                    config.policy.clone(),
                    config.tasks.clone(),
                    PathBuf::from(config.cache_root),
                )?);
                self.actors
                    .write()
                    .insert(workspace.id.clone(), actor.clone());
                let mut summary = summarize(&actor)?;
                add_elapsed(&mut summary, started.elapsed().as_millis(), false);
                Ok(summary)
            }
            "summary" => {
                let actor = self.actor(params)?;
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
            "refresh" => self.actor(params)?.refresh(
                params
                    .get("force")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
            "changes" => self.actor(params)?.changes(params),
            "skills" => self.list_skills(),
            "skill" => self.read_skill(params),
            action => Err(AppError::details(
                "INVALID_WORKSPACE_ACTION",
                "Unknown workspace action",
                json!({"action": action}),
            )),
        }
    }

    fn actor(&self, params: &Value) -> AppResult<Arc<WorkspaceActor>> {
        let requested_id = params.get("workspace_id").and_then(Value::as_str);
        {
            let actors = self.actors.read();
            if let Some(id) = requested_id {
                if let Some(actor) = actors.get(id) {
                    return Ok(actor.clone());
                }
                if id == "main" && actors.len() == 1 {
                    return Ok(actors.values().next().expect("single actor").clone());
                }
            } else if actors.len() == 1 {
                return Ok(actors.values().next().expect("single actor").clone());
            }
        }

        let config = self.config()?;
        if let Some(id) = requested_id {
            if id != "main" {
                if let Some(workspace) = self.dynamic_workspace_by_id(&config, id)? {
                    self.workspace(&json!({
                        "action": "open",
                        "path": workspace.path,
                        "_summary_ids": true
                    }))?;
                    let actors = self.actors.read();
                    if let Some(actor) = actors.get(id) {
                        return Ok(actor.clone());
                    }
                }
            }
        }
        let can_open_default =
            config.workspace.default_path.is_some() || config.workspaces.len() == 1;
        if can_open_default {
            self.workspace(&json!({"action": "open", "_summary_ids": true}))?;
            let actors = self.actors.read();
            if let Some(id) = requested_id {
                if let Some(actor) = actors.get(id) {
                    return Ok(actor.clone());
                }
                if id == "main" && actors.len() == 1 {
                    return Ok(actors.values().next().expect("single actor").clone());
                }
            } else if actors.len() == 1 {
                return Ok(actors.values().next().expect("single actor").clone());
            }
        }

        Err(AppError::details(
            "WORKSPACE_NOT_OPEN",
            "No repository is active and no unambiguous default repository can be opened",
            json!({
                "workspace_id": requested_id,
                "suggested_action": "workspace(action='open', path='...')"
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
        self.remember_dynamic_workspace(config, &workspace)?;
        Ok(workspace)
    }

    fn dynamic_workspace_by_id(
        &self,
        config: &DaemonConfig,
        workspace_id: &str,
    ) -> AppResult<Option<WorkspaceConfig>> {
        if !workspace_id.starts_with("repo-") {
            return Ok(None);
        }
        let registry_path = PathBuf::from(&config.cache_root).join("dynamic-workspaces.json");
        let registry: HashMap<String, String> = match fs::read_to_string(&registry_path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(path) = registry.get(workspace_id) else {
            return Ok(None);
        };
        let workspace = self.dynamic_workspace(config, path)?;
        if workspace.id != workspace_id {
            return Ok(None);
        }
        Ok(Some(workspace))
    }

    fn remember_dynamic_workspace(
        &self,
        config: &DaemonConfig,
        workspace: &WorkspaceConfig,
    ) -> AppResult<()> {
        let cache_root = PathBuf::from(&config.cache_root);
        fs::create_dir_all(&cache_root)?;
        let registry_path = cache_root.join("dynamic-workspaces.json");
        let mut registry: HashMap<String, String> = fs::read_to_string(&registry_path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_default();
        registry.insert(workspace.id.clone(), workspace.path.clone());
        let temporary = cache_root.join("dynamic-workspaces.json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(&registry)?)?;
        fs::rename(temporary, registry_path)?;
        Ok(())
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

        assert!(manager.actors.read().is_empty());
        let actor = manager.actor(&json!({"workspace_id": "main"})).unwrap();
        assert_eq!(
            actor.root_path(),
            std::fs::canonicalize(root.path()).unwrap()
        );
        assert_eq!(manager.actors.read().len(), 1);
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
        assert_eq!(manager.actors.read().len(), 1);
        manager
            .workspace(&json!({"action": "open", "path": second}))
            .unwrap();
        assert_eq!(manager.actors.read().len(), 1);
        let actors = manager.actors.read();
        let actor = actors.values().next().unwrap();
        assert!(actor.id.starts_with("repo-"));
        assert_eq!(actor.root_path(), std::fs::canonicalize(second).unwrap());
        assert_eq!(std::fs::read_dir(cache.join("repos")).unwrap().count(), 2);
    }

    #[test]
    fn dynamic_open_rejects_paths_outside_allowed_roots() {
        let allowed = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::fs::write(outside.path().join("main.rs"), "fn main() {}\n").unwrap();
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
        let error = manager
            .workspace(&json!({"action": "open", "path": outside.path()}))
            .unwrap_err();
        assert_eq!(error.0.code, "WORKSPACE_OUTSIDE_ALLOWED_ROOTS");
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
        assert_eq!(manager.actors.read().len(), 1);

        manager
            .initialize(&daemon_config(root.path(), cache.path(), 512_000))
            .unwrap();
        assert!(manager.actors.read().is_empty());
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
