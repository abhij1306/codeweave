use super::*;

impl Workspace {
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

    pub(super) fn observed_run_changed_paths(
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

    pub(super) fn path_modified_at_or_before(&self, path: &str, ended_at: &DateTime<Utc>) -> bool {
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
