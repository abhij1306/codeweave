use super::state::{RepoStatusChange, StateWriter};
use super::*;

impl Workspace {
    /// A cloneable read-only handle into the reconciled state, for the
    /// code-intelligence service.
    pub(crate) fn state_reader(&self) -> StateReader {
        self.state.reader()
    }

    /// Run `git status` and publish the result with a recomputed snapshot. A failure
    /// keeps the previous status, marks it stale, and is returned.
    pub(super) fn refresh_repo_status(&self, writer: &mut StateWriter<'_>) -> AppResult<()> {
        let status = self.repository.status(&self.root);
        writer.update(|state| {
            let applied = state.apply_repo_status(status);
            state.recompute_snapshot();
            applied.map(|_| ())
        })
    }

    pub(super) fn reconcile_pending(&self) -> AppResult<Vec<String>> {
        if !self.state.ledger().needs_reconcile() {
            return Ok(Vec::new());
        }
        let mut writer = self.state.writer();
        self.reconcile_with(&mut writer)
    }

    /// Apply queued watcher changes to the index and repository status. The caller
    /// holds the writer, so work that follows under the same writer sees the result.
    pub(super) fn reconcile_with(&self, writer: &mut StateWriter<'_>) -> AppResult<Vec<String>> {
        let Some(drain) = self.state.ledger().drain_pending(Instant::now()) else {
            return Ok(Vec::new());
        };
        let (changed, external) = writer.update(|state| -> AppResult<_> {
            let changed = if drain.paths.is_empty() {
                Vec::new()
            } else {
                state.index_mut().refresh_paths(
                    &self.root,
                    &drain.paths,
                    self.policy.max_file_bytes,
                    &self.exclusions,
                )?
            };
            // A failed `git status` keeps the previous status and marks it stale.
            let repo = if drain.git_event || !changed.is_empty() {
                state
                    .apply_repo_status(self.repository.status(&self.root))
                    .unwrap_or_default()
            } else {
                RepoStatusChange::default()
            };
            if changed.is_empty() && !repo.changed {
                return Ok((changed, Vec::new()));
            }

            let generation = state.bump_generation();
            if !changed.is_empty() || repo.head_changed {
                state.recompute_snapshot();
            }
            let changed_set: HashSet<&String> = changed.iter().collect();
            let external = drain
                .external
                .iter()
                .filter(|path| changed_set.contains(path))
                .map(|path| MutationRecord {
                    mutation_id: MutationRecord::new_id(),
                    path: path.clone(),
                    before_hash: None,
                    after_hash: state.index().get(path).map(|file| file.hash().to_owned()),
                    source: MutationSource::External,
                    request_id: "watcher".to_owned(),
                    timestamp: Utc::now(),
                    generation,
                })
                .collect::<Vec<_>>();
            Ok((changed, external))
        })?;
        if !external.is_empty() {
            self.state.ledger().record_external(&external);
        }
        Ok(changed)
    }

    pub(super) async fn reconcile_pending_async(self: &Arc<Self>) -> AppResult<Vec<String>> {
        if !self.state.ledger().needs_reconcile() {
            return Ok(Vec::new());
        }
        let actor = Arc::clone(self);
        let changed = tokio::task::spawn_blocking(move || actor.reconcile_pending())
            .await
            .map_err(AppError::internal)?;
        *self.last_reconcile.lock() = Instant::now();
        changed
    }

    /// Reconcile debounce for high-frequency run polls. `bash_status`/`bash_output`/
    /// `bash_cancel` fire repeatedly while a command streams output and each one would
    /// otherwise trigger a full `refresh_paths` + `git status` subprocess whenever the
    /// running command touches the tree. Skip the refresh unless the run reached a
    /// terminal state or it has been at least `POLL_RECONCILE_DEBOUNCE` since the last
    /// reconcile, so the workspace view still converges without paying per-poll latency.
    pub(super) async fn reconcile_after_poll(
        self: &Arc<Self>,
        terminal: bool,
    ) -> AppResult<Vec<String>> {
        if !self.state.ledger().needs_reconcile() {
            return Ok(Vec::new());
        }
        if !terminal {
            let since = self.last_reconcile.lock().elapsed();
            if since < POLL_RECONCILE_DEBOUNCE {
                return Ok(Vec::new());
            }
        }
        self.reconcile_pending_async().await
    }
}
