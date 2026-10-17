use super::*;

impl Workspace {
    /// The current snapshot id after reconciling pending watcher paths: the
    /// precondition an edit without its own `snapshot_id` is planned against.
    pub fn snapshot_id(&self) -> AppResult<String> {
        self.reconcile_pending()?;
        Ok(self.state.read().stamp().snapshot_id)
    }

    pub fn diagnostics(&self) -> AppResult<Value> {
        let ledger = self.state.ledger();
        let reconcile_pending = ledger.needs_reconcile();
        let pending_path_count = ledger.pending_count();
        let view = self.state.read();
        let index = view.index();
        Ok(json!({
            "workspace_id": self.id,
            "root": self.root,
            "generation": view.generation(),
            "snapshot_id": view.snapshot_id(),
            "file_count": index.file_count(),
            "languages": index.languages(),
            "reconcile_pending": reconcile_pending,
            "pending_path_count": pending_path_count,
            "running_bash_count": self.bash.running_count(),
            "execution": {
                "bash": self.bash.readiness()
            },
            "policy": {
                "max_file_bytes": self.policy.max_file_bytes,
                "max_context_chars": self.policy.max_context_chars,
                "max_search_results": self.policy.max_search_results,
                "bash": self.policy.bash,
            }
        }))
    }

    pub fn summary(&self) -> AppResult<Value> {
        let started = Instant::now();
        let reconcile_started = Instant::now();
        self.reconcile_pending()?;
        let reconcile_ms = reconcile_started.elapsed().as_millis();
        let view = self.state.read();
        let index = view.index();
        let repo = view.repo_status();
        let dirty_set: HashSet<String> = repo
            .dirty_files
            .iter()
            .filter(|path| !self.exclusions.is_ignored(Path::new(path), false))
            .cloned()
            .collect();
        let mcp_paths = self.state.ledger().edited_paths_within(&dirty_set);
        let external = self.state.ledger().external_paths_within(&dirty_set);
        let preexisting = &self.opened_dirty_summary;
        let mcp_changed = summarize_changed_paths(mcp_paths);
        let external = summarize_changed_paths(external);
        let repository_dirty = summarize_changed_paths(dirty_set);
        let repository = json!({
            "is_git": repo.is_git,
            "head": repo.head,
            "branch": repo.branch,
            "dirty_files": repository_dirty.paths,
            "dirty_file_count": repository_dirty.count,
            "dirty_files_truncated": repository_dirty.truncated,
            "dirty_file_groups": repository_dirty.groups
        });
        // Instruction files are inlined into every summary/open. Cap the inlined body
        // so a large AGENTS.md/CLAUDE.md cannot dominate the response; the caller can
        // read the rest with a code_retrieve path read when truncated.
        let instructions = ["AGENTS.md", "CLAUDE.md"]
            .into_iter()
            .filter_map(|path| {
                index.get(path).map(|file| {
                    let full_len = file.content().len();
                    if full_len > INSTRUCTION_INLINE_CAP {
                        let safe_cap =
                            char_boundary_at_or_before(file.content(), INSTRUCTION_INLINE_CAP);
                        let end = file.content()[..safe_cap]
                            .rfind('\n')
                            .map(|idx| idx + 1)
                            .unwrap_or(safe_cap);
                        json!({
                            "path": path,
                            "content": &file.content()[..end],
                            "content_truncated": true,
                            "content_bytes": full_len,
                            "guidance": "Instruction file truncated; use code_retrieve with operation=read and target=path."
                        })
                    } else {
                        json!({"path": path, "content": file.content()})
                    }
                })
            })
            .collect::<Vec<_>>();
        let bash = self.bash.readiness();
        let bash_available = bash.is_ready();
        let validation_guidance = if bash_available {
            "Write-tool validate fields accept Bash command strings. Use bash(command='<command>') for standalone execution."
        } else {
            "No usable Bash implementation passed readiness checks. Fix policy.bash.executable or install Git Bash/MSYS2/Cygwin Bash."
        };
        let warnings = if bash_available {
            Vec::<String>::new()
        } else {
            vec![format!(
                "Bash execution and write-tool validation commands are unavailable: {}",
                bash.failure_reason
                    .as_deref()
                    .unwrap_or("No usable Bash implementation found")
            )]
        };
        let mut result = json!({
            "workspace_id": self.id, "name": self.name, "root": self.root, "generation": view.generation(), "snapshot_id": view.snapshot_id(),
            "file_count": index.file_count(), "languages": index.languages(), "repository": repository, "instructions": instructions,
            "capabilities": {
                "bash_available": bash_available,
                "bash": bash,
                "validation_guidance": validation_guidance
            },
            "warnings": warnings,
            "open_diagnostics": self.open_diagnostics,
            "dirty_ownership": {
                "preexisting_at_open": &preexisting.paths,
                "changed_by_mcp": mcp_changed.paths,
                "observed_external": external.paths,
                "counts": {
                    "preexisting_at_open": preexisting.count,
                    "changed_by_mcp": mcp_changed.count,
                    "observed_external": external.count
                },
                "truncated": {
                    "preexisting_at_open": preexisting.truncated,
                    "changed_by_mcp": mcp_changed.truncated,
                    "observed_external": external.truncated
                },
                "groups": {
                    "preexisting_at_open": &preexisting.groups,
                    "changed_by_mcp": mcp_changed.groups,
                    "observed_external": external.groups
                }
            },
            "tool_guidance": format!("This runtime has one active repository. Context and edits read cached state; call workspace refresh only after suspected missed external changes. {validation_guidance}")
        });
        add_phase_metrics(
            &mut result,
            &[
                ("reconcile", reconcile_ms),
                ("total_local", started.elapsed().as_millis()),
            ],
        );
        Ok(result)
    }

    pub fn refresh(&self, force: bool) -> AppResult<Value> {
        if force {
            self.rescan()?;
        } else {
            self.reconcile_pending()?;
        }
        self.summary()
    }

    /// Rebuild the index from disk and refresh the repository status, superseding
    /// any queued watcher paths.
    pub(super) fn rescan(&self) -> AppResult<()> {
        let mut writer = self.state.writer();
        self.state.ledger().clear_pending();
        let index = CodeIndex::scan(
            &self.root,
            self.policy.max_file_bytes,
            &self.artifact_paths,
            &self.exclusions,
        )?;
        let status = self.repository.status(&self.root);
        writer.update(|state| {
            *state.index_mut() = index;
            // A failed `git status` keeps the previous status and marks it stale.
            let _ = state.apply_repo_status(status);
            state.bump_generation();
            state.recompute_snapshot();
        });
        Ok(())
    }

    pub(super) fn search_index(&self, request: &SearchRequest) -> AppResult<Value> {
        let reconcile_pending = self.state.ledger().needs_reconcile();
        let view = self.state.read();
        execute_index_search(
            view.index(),
            &self.id,
            view.snapshot_id(),
            request,
            self.policy.max_search_results,
            reconcile_pending,
        )
    }

    /// Mutation records newer than `since_generation`, optionally from one
    /// `source`, at most `limit` of them.
    pub fn changes(
        &self,
        since_generation: u64,
        source: Option<&str>,
        limit: usize,
    ) -> AppResult<Value> {
        let started = Instant::now();
        let reconcile_started = Instant::now();
        self.reconcile_pending()?;
        let reconcile_ms = reconcile_started.elapsed().as_millis();
        let records = self
            .state
            .ledger()
            .records_since(since_generation, source, limit);
        let stamp = self.state.read().stamp();
        let mut result = json!({
            "workspace_id": self.id,
            "generation": stamp.generation,
            "snapshot_id": stamp.snapshot_id,
            "mutations": records
        });
        add_phase_metrics(
            &mut result,
            &[
                ("reconcile", reconcile_ms),
                ("total_local", started.elapsed().as_millis()),
            ],
        );
        Ok(result)
    }
}
