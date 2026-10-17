use super::state::MutationPermit;
use super::{add_phase_metrics, Workspace};
use crate::index::content_hash;
use crate::model::{bool_value, required_str, string_list, usize_value, AppError, AppResult};
use crate::security::validate_relative;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;

/// Git actions that change the worktree or index and so run under the mutation permit.
fn is_mutating_action(action: &str) -> bool {
    matches!(action, "stage" | "commit" | "restore")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GitDiffScope {
    staged: bool,
    paths: Vec<String>,
    symbol: Option<String>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    hunk_ids: Vec<String>,
}

impl GitDiffScope {
    fn from_params(params: &Value) -> Self {
        Self {
            staged: bool_value(params, "staged", false),
            paths: string_list(params, "paths"),
            symbol: params
                .get("symbol")
                .and_then(Value::as_str)
                .map(str::to_owned),
            start_line: params
                .get("start_line")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            end_line: params
                .get("end_line")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            hunk_ids: string_list(params, "hunk_ids"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GitDiffContinuation {
    snapshot_id: String,
    offset: usize,
    scope: GitDiffScope,
}

fn git_diff_scope_supplied(params: &Value) -> bool {
    [
        "paths",
        "staged",
        "symbol",
        "start_line",
        "end_line",
        "hunk_ids",
    ]
    .iter()
    .any(|field| params.get(*field).is_some())
}

fn encode_git_diff_continuation(value: &GitDiffContinuation) -> AppResult<String> {
    let json = serde_json::to_vec(value)?;
    Ok(format!("git-diff:{}", URL_SAFE_NO_PAD.encode(json)))
}

fn decode_git_diff_continuation(value: &str) -> AppResult<GitDiffContinuation> {
    let payload = value
        .strip_prefix("git-diff:")
        .ok_or_else(|| AppError::invalid("Invalid git diff continuation"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| AppError::invalid("Invalid git diff continuation"))?;
    let continuation: GitDiffContinuation = serde_json::from_slice(&bytes)
        .map_err(|_| AppError::invalid("Invalid git diff continuation"))?;
    Ok(continuation)
}

#[derive(Clone)]
struct DiffHunk {
    id: String,
    path: String,
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    text: String,
}

fn parse_hunk_range(value: &str) -> (usize, usize) {
    let value = value.trim_start_matches(['-', '+']);
    let (start, count) = value.split_once(',').unwrap_or((value, "1"));
    (start.parse().unwrap_or(0), count.parse().unwrap_or(1))
}

fn parse_diff_hunks(diff: &str) -> Vec<DiffHunk> {
    let mut hunks = Vec::new();
    let mut path = String::new();
    let mut header = String::new();
    let mut active: Option<(usize, usize, usize, usize, String)> = None;
    let finish = |active: &mut Option<(usize, usize, usize, usize, String)>,
                  path: &str,
                  hunks: &mut Vec<DiffHunk>| {
        if let Some((old_start, old_count, new_start, new_count, text)) = active.take() {
            let id = content_hash(&format!("{path}\0{old_start}\0{new_start}\0{text}"));
            hunks.push(DiffHunk {
                id,
                path: path.to_owned(),
                old_start,
                old_count,
                new_start,
                new_count,
                text,
            });
        }
    };
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            finish(&mut active, &path, &mut hunks);
            header.clear();
            path.clear();
            header.push_str(line);
            continue;
        }
        if let Some(value) = line.strip_prefix("+++ b/") {
            path = value.trim_end().to_owned();
        }
        if line.starts_with("@@ ") {
            finish(&mut active, &path, &mut hunks);
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let (old_start, old_count) = fields
                .get(1)
                .map(|value| parse_hunk_range(value))
                .unwrap_or((0, 0));
            let (new_start, new_count) = fields
                .get(2)
                .map(|value| parse_hunk_range(value))
                .unwrap_or((0, 0));
            active = Some((
                old_start,
                old_count,
                new_start,
                new_count,
                format!("{header}{line}"),
            ));
        } else if let Some((_, _, _, _, text)) = active.as_mut() {
            text.push_str(line);
        } else {
            header.push_str(line);
        }
    }
    finish(&mut active, &path, &mut hunks);
    hunks
}

fn hunk_overlaps(hunk: &DiffHunk, start: usize, end: usize) -> bool {
    let overlaps = |line: usize, count: usize| {
        let last = line.saturating_add(count.saturating_sub(1));
        count > 0 && line <= end && last >= start
    };
    overlaps(hunk.old_start, hunk.old_count) || overlaps(hunk.new_start, hunk.new_count)
}

pub(super) fn validated_push_target(
    params: &Value,
    current_branch: &str,
) -> AppResult<(String, String)> {
    let remote = match params.get("remote") {
        None => "origin",
        Some(Value::String(remote)) => remote,
        Some(_) => return Err(AppError::invalid("git push remote must be a string")),
    };
    if remote.is_empty()
        || remote.trim() != remote
        || remote.starts_with('-')
        || remote.starts_with('+')
        || remote.contains(':')
    {
        return Err(AppError::invalid(
            "git push remote must be a non-empty remote name, not an option or URL",
        ));
    }

    let branch = match params.get("branch") {
        None => current_branch,
        Some(Value::String(branch)) => branch,
        Some(_) => return Err(AppError::invalid("git push branch must be a string")),
    };
    let invalid_ref_character = branch
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace() || "~^:?*[\\".contains(ch));
    let invalid_component = branch.split('/').any(|component| {
        component.is_empty() || component.starts_with('.') || component.ends_with(".lock")
    });
    if branch.is_empty()
        || branch.starts_with('-')
        || branch.starts_with('+')
        || branch == "@"
        || branch.contains("..")
        || branch.contains("@{")
        || branch.ends_with('.')
        || invalid_ref_character
        || invalid_component
    {
        return Err(AppError::invalid(
            "git push branch must be a valid branch name, not an option or refspec",
        ));
    }

    Ok((remote.to_owned(), branch.to_owned()))
}

impl Workspace {
    /// Run one Git action. Mutating actions first wait for the mutation permit, in
    /// async code, so they never overlap an edit (including its validation).
    pub async fn git(self: &Arc<Self>, params: &Value) -> AppResult<Value> {
        let action = required_str(params, "action")?;
        let permit = if is_mutating_action(action) {
            Some(self.state.exclusive().await)
        } else {
            None
        };
        let actor = Arc::clone(self);
        let params = params.clone();
        tokio::task::spawn_blocking(move || actor.git_blocking(&params, permit.as_ref()))
            .await
            .map_err(AppError::internal)?
    }

    fn git_blocking(&self, params: &Value, permit: Option<&MutationPermit>) -> AppResult<Value> {
        let started = Instant::now();
        let reconcile_started = Instant::now();
        self.reconcile_pending()?;
        let reconcile_ms = reconcile_started.elapsed().as_millis();
        let action = required_str(params, "action")?;
        let paths = string_list(params, "paths");
        for path in &paths {
            validate_relative(path)?;
        }
        let staged = bool_value(params, "staged", false);
        let git_started = Instant::now();
        if action == "diff" {
            let stamp = self.state.read().stamp();
            let snapshot = stamp.snapshot_id;
            let requested_chars = usize_value(params, "max_chars", self.policy.max_context_chars);
            let applied_chars = requested_chars.min(self.policy.max_context_chars);
            let requested_scope = GitDiffScope::from_params(params);
            let (scope, offset) =
                if let Some(token) = params.get("continuation").and_then(Value::as_str) {
                    let continuation = decode_git_diff_continuation(token)?;
                    if continuation.snapshot_id != snapshot {
                        return Err(AppError::details(
                            "STALE_CONTINUATION",
                            "Repository changed after the diff continuation was created",
                            json!({
                                "expected_snapshot": continuation.snapshot_id,
                                "current_snapshot": snapshot
                            }),
                        ));
                    }
                    if git_diff_scope_supplied(params) && requested_scope != continuation.scope {
                        return Err(AppError::details(
                            "CONTINUATION_SCOPE_MISMATCH",
                            "Git diff continuation scope does not match the supplied filters",
                            json!({
                                "continuation_scope": continuation.scope,
                                "requested_scope": requested_scope
                            }),
                        ));
                    }
                    (continuation.scope, continuation.offset)
                } else {
                    (requested_scope, 0)
                };
            for path in &scope.paths {
                validate_relative(path)?;
            }
            let raw = self.repository.diff(
                &self.root,
                scope.staged,
                &scope.paths,
                self.policy.max_context_chars,
            )?;
            let mut hunks = parse_diff_hunks(&raw);
            if let Some(symbol) = scope.symbol.as_deref() {
                if scope.paths.len() != 1 {
                    return Err(AppError::invalid(
                        "git_diff symbol focus requires exactly one path",
                    ));
                }
                let symbols = self
                    .state
                    .read()
                    .index()
                    .find_symbols(scope.paths.first().map(String::as_str), symbol);
                if symbols.len() != 1 {
                    return Err(AppError::details(
                        if symbols.is_empty() {
                            "SYMBOL_NOT_FOUND"
                        } else {
                            "AMBIGUOUS_SYMBOL"
                        },
                        "git_diff symbol focus requires one indexed declaration",
                        json!({"path": scope.paths[0], "symbol": symbol, "candidates": symbols.into_iter().map(|found| json!({"path": found.path, "symbol": found.symbol})).collect::<Vec<_>>() }),
                    ));
                }
                let symbol = symbols
                    .into_iter()
                    .next()
                    .expect("checked one symbol")
                    .symbol;
                hunks.retain(|hunk| hunk_overlaps(hunk, symbol.start_line, symbol.end_line));
            }
            if let (Some(start), Some(end)) = (scope.start_line, scope.end_line) {
                hunks.retain(|hunk| hunk_overlaps(hunk, start, end));
            }
            if !scope.hunk_ids.is_empty() {
                hunks.retain(|hunk| scope.hunk_ids.iter().any(|id| id == &hunk.id));
            }
            let mut output = String::new();
            let mut selected = Vec::new();
            let mut next = None;
            for (index, hunk) in hunks.iter().enumerate().skip(offset) {
                if !output.is_empty()
                    && output.len().saturating_add(hunk.text.len()) > applied_chars
                {
                    next = Some(index);
                    break;
                }
                let oversized = hunk.text.len() > applied_chars;
                output.push_str(&hunk.text);
                selected.push(json!({
                    "id": hunk.id, "path": hunk.path,
                    "old": {"start_line": hunk.old_start, "line_count": hunk.old_count},
                    "new": {"start_line": hunk.new_start, "line_count": hunk.new_count}
                }));
                if oversized {
                    next = (index + 1 < hunks.len()).then_some(index + 1);
                    break;
                }
            }
            let continuation = next
                .map(|index| {
                    encode_git_diff_continuation(&GitDiffContinuation {
                        snapshot_id: snapshot.clone(),
                        offset: index,
                        scope: scope.clone(),
                    })
                })
                .transpose()?;
            let mut response = json!({
                "action": "diff", "output": output, "hunks": selected,
                "truncated": continuation.is_some(), "continuation": continuation,
                "scope": scope,
                "limits": {"requested_chars": requested_chars, "applied_chars": applied_chars, "configured_max_chars": self.policy.max_context_chars},
                "generation": stamp.generation, "snapshot_id": snapshot
            });
            add_phase_metrics(
                &mut response,
                &[
                    ("reconcile", reconcile_ms),
                    ("git", git_started.elapsed().as_millis()),
                    ("total_local", started.elapsed().as_millis()),
                ],
            );
            return Ok(response);
        }
        let result = match action {
            "diff" => unreachable!("handled before generic Git dispatch"),
            "status" => {
                let mut writer = self.state.writer();
                let refreshed = self.refresh_repo_status(&mut writer);
                let git_ms = git_started.elapsed().as_millis();
                refreshed?;
                let view = writer.read();
                let mut result = json!({
                    "action": action,
                    "status": view.repo_status(),
                    "generation": view.generation(),
                    "snapshot_id": view.snapshot_id()
                });
                add_phase_metrics(
                    &mut result,
                    &[
                        ("reconcile", reconcile_ms),
                        ("git_status", git_ms),
                        ("total_local", started.elapsed().as_millis()),
                    ],
                );
                return Ok(result);
            }
            "log" => self
                .repository
                .log(&self.root, usize_value(params, "limit", 20).min(200))?,
            "show" => self.repository.show(
                &self.root,
                params.get("ref").and_then(Value::as_str).unwrap_or("HEAD"),
                self.policy.max_context_chars,
            )?,
            "blame" => {
                let path = paths
                    .first()
                    .ok_or_else(|| AppError::invalid("git blame requires one path"))?;
                self.repository.blame(
                    &self.root,
                    path,
                    params
                        .get("start_line")
                        .and_then(Value::as_u64)
                        .map(|v| v as usize),
                    params
                        .get("end_line")
                        .and_then(Value::as_u64)
                        .map(|v| v as usize),
                    self.policy.max_context_chars,
                )?
            }
            "stage" | "commit" | "restore" => {
                let permit = permit.ok_or_else(|| {
                    AppError::internal(format!("git {action} requires the mutation permit"))
                })?;
                self.git_mutation(action, params, &paths, staged, permit)?
            }
            "push" => {
                if !bool_value(params, "confirm", false) {
                    return Err(AppError::new(
                        "CONFIRMATION_REQUIRED",
                        "git push requires confirm=true",
                    ));
                }
                let status = self.repository.status(&self.root)?;
                let (remote, branch) = validated_push_target(params, &status.branch)?;
                self.repository.push(&self.root, &remote, &branch)?
            }
            "preflight" => {
                let (status, stamp) = {
                    let mut writer = self.state.writer();
                    self.refresh_repo_status(&mut writer)?;
                    let view = writer.read();
                    (view.repo_status().clone(), view.stamp())
                };
                let diff =
                    self.repository
                        .diff(&self.root, true, &[], self.policy.max_context_chars)?;
                let git_ms = git_started.elapsed().as_millis();
                let mut result = json!({
                    "action": "preflight",
                    "staged_files": status.staged_files,
                    "partially_staged_files": status.partially_staged_files,
                    "cached_diff": diff,
                    "generation": stamp.generation,
                    "snapshot_id": stamp.snapshot_id
                });
                add_phase_metrics(
                    &mut result,
                    &[
                        ("reconcile", reconcile_ms),
                        ("git", git_ms),
                        ("total_local", started.elapsed().as_millis()),
                    ],
                );
                return Ok(result);
            }
            _ => {
                return Err(AppError::details(
                    "INVALID_GIT_ACTION",
                    "Unknown Git action",
                    json!({"action": action}),
                ))
            }
        };
        let git_ms = git_started.elapsed().as_millis();
        let view = self.state.read();
        let mut response = json!({
            "action": action,
            "output": result,
            "generation": view.generation(),
            "snapshot_id": view.snapshot_id()
        });
        if view.repo_status_stale() {
            response["repo_status_stale"] = Value::Bool(true);
        }
        drop(view);
        add_phase_metrics(
            &mut response,
            &[
                ("reconcile", reconcile_ms),
                ("git", git_ms),
                ("total_local", started.elapsed().as_millis()),
            ],
        );
        Ok(response)
    }

    /// `stage`, `commit` and `restore`. The permit proves the caller holds the
    /// mutation gate; the repository status is refreshed before returning.
    fn git_mutation(
        &self,
        action: &str,
        params: &Value,
        paths: &[String],
        staged: bool,
        _permit: &MutationPermit,
    ) -> AppResult<String> {
        let output = match action {
            "stage" => self.repository.stage(&self.root, paths)?,
            "commit" => self.repository.commit(
                &self.root,
                params
                    .get("message")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AppError::invalid("git commit requires message"))?,
            )?,
            "restore" => {
                if !bool_value(params, "confirm", false) {
                    return Err(AppError::new(
                        "CONFIRMATION_REQUIRED",
                        "git restore requires confirm=true",
                    ));
                }
                let output = self.repository.restore(&self.root, paths, staged)?;
                self.rescan()?;
                return Ok(output);
            }
            _ => unreachable!("only mutating Git actions reach git_mutation"),
        };
        // A failed `git status` keeps the previous status and marks it stale; the
        // response reports that through `repo_status_stale`.
        let _ = self.refresh_repo_status(&mut self.state.writer());
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(old_start: usize, old_count: usize, new_start: usize, new_count: usize) -> DiffHunk {
        DiffHunk {
            id: "test".to_owned(),
            path: "test.rs".to_owned(),
            old_start,
            old_count,
            new_start,
            new_count,
            text: String::new(),
        }
    }

    #[test]
    fn hunk_overlap_ignores_zero_length_sides() {
        let addition = hunk(10, 0, 100, 2);
        assert!(!hunk_overlaps(&addition, 10, 10));
        assert!(hunk_overlaps(&addition, 100, 101));

        let deletion = hunk(20, 2, 200, 0);
        assert!(hunk_overlaps(&deletion, 20, 21));
        assert!(!hunk_overlaps(&deletion, 200, 200));
    }
}
