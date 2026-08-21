use super::*;

impl Workspace {
    /// Refresh the cached `repo_status` from `git status`. On failure, log a
    /// warning and set `repo_status_stale` instead of silently clobbering the
    /// cache with an empty default (D8): an empty status looks identical to a
    /// clean tree, which would mislead callers about what is staged/dirty. The
    /// previous (possibly-stale) status is retained so downstream logic still has
    /// its best-known view.
    pub(super) fn refresh_repo_status(&self) {
        match self.repository.status(&self.root) {
            Ok(status) => {
                *self.repo_status.write() = status;
                self.repo_status_stale.store(false, Ordering::Release);
            }
            Err(error) => {
                tracing::warn!(
                    workspace = %self.id,
                    error = %error,
                    "git status refresh failed; repo_status may be stale"
                );
                self.repo_status_stale.store(true, Ordering::Release);
            }
        }
    }

    /// Whether the cached repository status is known to be out of date because a
    /// refresh failed since the last successful `git status`.
    pub(super) fn repo_status_stale(&self) -> bool {
        self.repo_status_stale.load(Ordering::Acquire)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    pub fn snapshot(&self) -> String {
        self.snapshot_id.read().clone()
    }

    pub(crate) fn reference_index(&self) -> Arc<RwLock<CodeIndex>> {
        Arc::clone(&self.index)
    }

    pub(crate) fn reference_snapshot(&self) -> Arc<RwLock<String>> {
        Arc::clone(&self.snapshot_id)
    }

    pub(super) fn reconcile_pending(&self) -> AppResult<Vec<String>> {
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
        let mut candidates = Vec::new();
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
                    || self.exclusions.is_ignored(&path, path.is_dir())
                    || is_temp
                    || path.is_dir()
                {
                    continue;
                }
                let was_internal = internal.contains_key(&path);
                candidates.push((path, relative, was_internal));
            }
        }

        for (path, relative, was_internal) in candidates {
            if was_internal {
                self.internal_writes.lock().remove(&path);
                relevant.insert(path);
                continue;
            }
            external_candidates.insert(relative);
            relevant.insert(path);
        }

        let changed = if relevant.is_empty() {
            Vec::new()
        } else {
            self.index.write().refresh_paths(
                &self.root,
                &relevant,
                self.policy.max_file_bytes,
                &self.exclusions,
            )?
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
            let records = {
                let index = self.index.read();
                external
                    .iter()
                    .map(|path| MutationRecord {
                        mutation_id: MutationRecord::new_id(),
                        path: path.clone(),
                        before_hash: None,
                        after_hash: index.get(path).map(|file| file.hash.clone()),
                        source: "external".to_owned(),
                        request_id: "watcher".to_owned(),
                        timestamp: Utc::now(),
                        generation,
                    })
                    .collect::<Vec<_>>()
            };
            self.external_changed.lock().extend(external);
            self.record_mutations(&records)?;
        }
        if !changed.is_empty() || head_changed {
            self.recompute_snapshot();
        }
        Ok(changed)
    }

    pub(super) async fn reconcile_pending_async(self: &Arc<Self>) -> AppResult<Vec<String>> {
        if !self.needs_reconcile.load(Ordering::Acquire) {
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
        if !self.needs_reconcile.load(Ordering::Acquire) {
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

    pub(super) fn recompute_snapshot(&self) {
        let head = self.repo_status.read().head.clone();
        let snapshot = self.index.write().snapshot_id(&head);
        *self.snapshot_id.write() = snapshot;
    }

    pub(super) fn read_reconcile_pending(&self) -> bool {
        self.needs_reconcile.load(Ordering::Acquire)
    }
}
