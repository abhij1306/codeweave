use super::*;

impl Workspace {
    /// Lightweight alternative to `summary()` for the warm-path auto-open in
    /// `prepare()`. Returns only the identifiers that `prepare()` mines from
    /// the full summary (workspace_id, snapshot_id, generation) without
    /// iterating mutations, cloning dirty sets, or calling
    /// `summarize_changed_paths`.
    pub fn summary_ids(&self) -> AppResult<Value> {
        self.reconcile_pending()?;
        Ok(json!({
            "workspace_id": self.id,
            "snapshot_id": self.snapshot(),
            "generation": self.generation(),
        }))
    }

    pub fn diagnostics(&self) -> AppResult<Value> {
        let index = self.index.read();
        Ok(json!({
            "workspace_id": self.id,
            "root": self.root,
            "generation": self.generation(),
            "snapshot_id": self.snapshot(),
            "file_count": index.file_count(),
            "languages": index.languages(),
            "reconcile_pending": self.read_reconcile_pending(),
            "pending_path_count": self.pending_paths.lock().len(),
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
        let index = self.index.read();
        let repo = self.repo_status.read().clone();
        let dirty_set: HashSet<String> = repo
            .dirty_files
            .iter()
            .filter(|path| !self.exclusions.is_ignored(Path::new(path), false))
            .cloned()
            .collect();
        let mcp_paths: HashSet<String> = self
            .mutations
            .lock()
            .iter()
            .filter(|item| item.source == "mcp_edit" && dirty_set.contains(&item.path))
            .map(|item| item.path.clone())
            .collect();
        let external: HashSet<String> = self
            .external_changed
            .lock()
            .iter()
            .filter(|path| dirty_set.contains(*path))
            .cloned()
            .collect();
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
                    let full_len = file.content.len();
                    if full_len > INSTRUCTION_INLINE_CAP {
                        let safe_cap =
                            char_boundary_at_or_before(&file.content, INSTRUCTION_INLINE_CAP);
                        let end = file.content[..safe_cap]
                            .rfind('\n')
                            .map(|idx| idx + 1)
                            .unwrap_or(safe_cap);
                        json!({
                            "path": path,
                            "content": &file.content[..end],
                            "content_truncated": true,
                            "content_bytes": full_len,
                            "guidance": "Instruction file truncated; use code_retrieve with operation=read and target=path."
                        })
                    } else {
                        json!({"path": path, "content": file.content})
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
            "workspace_id": self.id, "name": self.name, "root": self.root, "generation": self.generation(), "snapshot_id": self.snapshot(),
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
            let _guard = self.reconcile_lock.lock();
            self.pending_paths.lock().clear();
            self.needs_reconcile.store(false, Ordering::Release);
            *self.index.write() = CodeIndex::scan(
                &self.root,
                self.policy.max_file_bytes,
                &self.artifact_paths,
                &self.exclusions,
            )?;
            self.refresh_repo_status();
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.recompute_snapshot();
        } else {
            self.reconcile_pending()?;
        }
        self.summary()
    }

    pub(super) fn search_index(&self, params: &Value) -> AppResult<Value> {
        let reconcile_pending = self.read_reconcile_pending();
        let snapshot = self.snapshot();
        let index = self.index.read();
        execute_index_search(
            &index,
            &self.id,
            &snapshot,
            params,
            self.policy.max_search_results,
            reconcile_pending,
        )
    }

    pub fn changes(&self, params: &Value) -> AppResult<Value> {
        let started = Instant::now();
        let reconcile_started = Instant::now();
        self.reconcile_pending()?;
        let reconcile_ms = reconcile_started.elapsed().as_millis();
        let since = params
            .get("since_generation")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let source = params.get("source").and_then(Value::as_str);
        let limit = usize_value(params, "limit", 200).min(2_000);
        let records: Vec<_> = self
            .mutations
            .lock()
            .iter()
            .rev()
            .filter(|item| item.generation > since)
            .filter(|item| source.map(|value| item.source == value).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect();
        let mut result = json!({
            "workspace_id": self.id,
            "generation": self.generation(),
            "snapshot_id": self.snapshot(),
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
