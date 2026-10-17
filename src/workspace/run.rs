use super::state::RunWindow;
use super::*;
use crate::bash::{ChangeBaseline, FrozenChanges, RunStatus};

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
                let baseline = self.change_baseline();
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
                    self.bash.set_change_baseline(run_id, baseline);
                }
                value
            }
            // Status, output and cancel only read or signal in-memory state and
            // never block, so they run inline.
            "status" => self.bash.status(required_str(params, "run_id")?)?,
            "output" => self.bash.output_stream(
                required_str(params, "run_id")?,
                params.get("continuation").and_then(Value::as_str),
                params.get("stream").and_then(Value::as_str),
            )?,
            "cancel" => self.bash.cancel(required_str(params, "run_id")?)?,
            other => {
                return Err(AppError::details(
                    "INVALID_RUN_ACTION",
                    "Unknown run action",
                    json!({"action": other}),
                ))
            }
        };
        let terminal = result
            .get("status")
            .and_then(Value::as_str)
            .and_then(RunStatus::parse)
            .is_none_or(RunStatus::is_terminal);
        let reconcile_after_started = Instant::now();
        if is_poll {
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
            let (start_generation, attribution_generation, changed_paths) =
                self.attribute_run_changes(&run_id, terminal).await?;
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

    /// The current generation and dirty file set, read together from one view.
    fn change_baseline(&self) -> ChangeBaseline {
        let view = self.state.read();
        ChangeBaseline {
            generation: view.generation(),
            dirty: Arc::new(view.repo_status().dirty_files.iter().cloned().collect()),
        }
    }

    /// Paths a run changed, as (start generation, end generation, paths). The
    /// supervisor supplies the run's baseline and end time; the change ledger
    /// decides attribution. A terminal run's first result is frozen and reused.
    async fn attribute_run_changes(
        self: &Arc<Self>,
        run_id: &str,
        terminal: bool,
    ) -> AppResult<(u64, u64, HashSet<String>)> {
        let current = self.change_baseline();
        let window = self.bash.change_window(run_id, current.clone())?;
        let start_generation = window.baseline.generation;
        if let Some(frozen) = window.frozen {
            return Ok((start_generation, frozen.generation, frozen.paths));
        }
        let ended_at = if terminal { window.ended_at } else { None };
        // Attribution reads file metadata, so it runs on the blocking pool.
        let actor = Arc::clone(self);
        let baseline_dirty = window.baseline.dirty;
        let current_dirty = Arc::clone(&current.dirty);
        let paths = tokio::task::spawn_blocking(move || {
            actor.state.ledger().attribute_since(&RunWindow {
                start_generation,
                end_generation: current.generation,
                ended_at: ended_at.as_ref(),
                baseline_dirty: &baseline_dirty,
                current_dirty: &current_dirty,
            })
        })
        .await
        .map_err(AppError::internal)?;
        if !terminal {
            return Ok((start_generation, current.generation, paths));
        }
        let frozen = self.bash.freeze_changes(
            run_id,
            FrozenChanges {
                generation: current.generation,
                paths,
            },
        )?;
        Ok((start_generation, frozen.generation, frozen.paths))
    }
}
