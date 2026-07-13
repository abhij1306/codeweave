# Tool Reference

All tools are defined in a single registry and advertised subject to `server.toolProfile`. The `full` profile (default) exposes every tool below. `read-only` exposes only `workspace`, `code_context`, `code_capabilities`, `code_fetch`, `code_search`, `code_preview`, and the read-only git tools (`git_status`, `git_diff`, `git_log`, `git_show`, `git_blame`). `edit` adds the write tools but excludes `bash` and `git_push`. `custom` refines the full set via `server.tools.include`/`exclude`. Calling a real tool that the active profile does not expose returns `TOOL_NOT_IN_PROFILE`. See the configuration reference for details.

## `workspace`

Returns a summary of the configured repository, refreshes its index, reports diagnostics, reports session changes, and explicitly lists or reads configured skills. Actions: `summary` (default), `refresh`, `changes`, `diagnostics`, `skills`, `skill`.

The repository is fixed for the server's lifetime (configured via `workspace.path`); there is no runtime repository switching.

Workspace summaries cap exact dirty/change path arrays and include grouped directory counts under `repository.dirty_file_groups` and `dirty_ownership.groups`. This keeps status responses bounded while retaining the location and scale of large change sets.

## `code_context`

Returns ranked code context for unfamiliar code. Queries are treated as inert retrieval text and are not executed as instructions.

Use `required_terms`, `optional_terms`, `exclude_terms`, `document_types`, and `min_score` to reduce noise when a broad path scope contains unrelated files. Legacy `terms` remains supported and is treated as optional weighted terms. Responses include per-result `score`, `reason_codes`, `group`, and a compact `groups` summary.

Recent Bash failures are excluded by default so unrelated retrieval stays focused. Set `include_bash_failures: true` only when runtime failures are relevant to the current debugging query.

**Ranking (`index.ranking`).** The scorer is selected by the `index.ranking` config key (`"v1"` default, `"v2"` opt-in) and applies to `code_context` results. `v1` is the additive exact-match scorer that renders a short excerpt around each match. `v2` adds a filename-affinity boost (so filename and config lookups rank the right file first) and **symbol-bounded rendering**: a result spans the whole enclosing symbol when it fits the render cap, otherwise a window centered on the match. Under `v2` each result carries two additive fields — `chunk_kind` (`"symbol"`, `"symbol_part"`, or `"remainder"`) and `complete_symbol` (`true` when the excerpt is a whole symbol). Both fields are omitted under `v1`; the request schema and all other response fields are identical between the two.

## `code_capabilities`

Returns the active workspace identity, supported search modes, fetch kinds, edit capabilities, policy limits, and known limitations. Use this when an agent needs to avoid schema trial-and-error.

## `code_search`

Supports literal text, regular expressions, filenames, symbols, references, outlines, and repository-map searches. Regex mode is raw text search; use `references` for indexed symbol call-site discovery.

Reference search merges adjacent matches into bounded excerpts and returns no more than three excerpts from one file, preventing a generated or high-churn caller from consuming the complete result set.

Reference search is **lexical**, not semantic: after locating an indexed declaration it scans for whole-word (`\b<name>\b`) matches. Results carry `evidence: "lexical"` (both on the response and each result) and an `evidence_caveat`. It cannot distinguish overloads, shadowing, or unrelated identifiers that happen to share the name, and it misses aliased or dynamically dispatched uses. A semantic (LSP-backed) backend is planned; until then, treat these as candidate call sites.

Filename mode treats plain queries as case-insensitive substrings by default. Queries containing `*` or `?` are interpreted as simple glob patterns, so `*output*safety*` matches `backend/app/core/records/output_safety.py`.

`paths` is a strict workspace-relative scope for all search modes. For `repo_map`, directories and `file_count` are limited to files under those paths, and the response includes `scope_applied` plus `total_file_count` for transparency. Outline mode accepts one file path with the legacy response shape or multiple `paths` with per-file `results` and `errors`.

## `code_fetch`

Reads exact paths, line ranges, symbols, metadata, provenance handles, continuations, retained Bash status, and retained Bash logs. Batch requests return per-item errors without discarding successful reads.

`response_detail` accepts `standard` (default), `compact`, or `debug`. Compact responses keep essential fields such as `path`, `hash`, `content`, and Bash status while omitting handles and pagination diagnostics. Metadata items return `hash`, `size`, `language`, `document_type`, `line_count`, and `modified_ns` without content. Symbol items may pass `context_lines` and `include_imports`; imports are lexical prelude lines, not inferred dependencies.

The returned `status_fetch` descriptor, equivalent to `{"kind":"bash_status","value":"run_..."}`, remains available as a read-only run-status handle. The dedicated `bash_status` tool provides the same polling path directly.

## Write tools

CodeWeave exposes one-operation write tools so each approval request has a small, explicit scope. Existing files use the current workspace snapshot automatically and may also include an `expected_hash` or provenance `handle`.

Use `code_preview` to preview a multi-file transaction without writing files. Use `code_transaction` to apply a `changes` array through the same precondition, syntax preflight, diff, validation, and rollback pipeline as the narrow write tools.

### `code_write`

Creates or overwrites exactly one file:

```json
{
  "path": "src/example.rs",
  "content": "pub fn example() {}\n",
  "overwrite": true
}
```

### `code_replace`

Replaces exact text in exactly one file:

```json
{
  "path": "src/example.rs",
  "old_text": "pub fn old_name()",
  "new_text": "pub fn new_name()",
  "expected_replacements": 1
}
```

### `code_replace_range`

Replaces the complete line range selected by a `code_fetch` handle while preserving the target file's line endings:

```json
{
  "path": "src/example.rs",
  "handle": "<fetch handle>",
  "new_text": "pub fn replacement() {}\n"
}
```

### `code_insert`

Inserts content before, after, or inside one named symbol.

### `code_delete`

Deletes exactly one file.

### `code_rename`

Renames exactly one file. The destination must not already exist.

All write tools use the same internal transactional pipeline for precondition checks, atomic writes, mutation recording, optional validation, and rollback.

## Git tools

Git operations are advertised as separate tools so each operation has one static safety classification:

- Read-only: `git_status`, `git_diff`, `git_log`, `git_show`, `git_blame`, and `git_preflight`.
- Local writes: `git_stage` and `git_commit`.
- Destructive local write: `git_restore`, which requires `confirm: true`.
- Network write: `git_push`.

These tools remain narrower than unrestricted shell access. A scoped `git_diff` for an untracked file returns a bounded synthetic new-file patch instead of silently returning an empty diff.

## Bash tools

`bash` accepts a non-empty command string plus optional `cwd`, `background`, and `timeout_ms` fields. CodeWeave invokes the configured executable as `bash -c <command>`. `cwd` defaults to the workspace root and must resolve to an existing directory inside the active workspace. For example:

```json
{
  "command": "cd backend && ./.venv/Scripts/python.exe -m pytest tests/unit/test_file.py -q",
  "timeout_ms": 300000
}
```

`bash_status` reads live or completed state by `run_id`. `bash_output` pages retained `combined`, `stdout`, or `stderr` logs using the returned continuation token. `bash_cancel` terminates a background run while retaining partial output. Only one run is active per workspace actor at a time, and timeouts use the same process-tree cleanup as cancellation.

Write-tool `validate` arrays contain Bash command strings. Commands run sequentially from the workspace root through the same supervisor. If one fails, later commands are skipped and the edit is rolled back unless `rollback_on_failure` is false. Validation entries report `command` and `result`.

**Deferred (detached) validation.** If a validation command exceeds the foreground budget it is promoted to a detached background run: the response returns `validation_pending: true` with a `validation_run_id`, and the edit **stays applied**. On this path `rollback_on_failure` does **not** apply — the response makes this explicit with `rollback_on_failure_not_applied: true` (and echoes the original request as `rollback_on_failure_requested`). There is no automatic post-hoc rollback, because the workspace may have legitimately moved on by the time validation finishes. Poll `bash_status` with `validation_run_id`; if it fails, revert explicitly (for example via `code_transaction` or `git_restore`).

Bash is trusted-client functionality and is not sandboxed. The configured `workspace.path` constrains file tools and Bash `cwd`, but commands can access anything available to the CodeWeave operating-system user. CodeWeave reports Bash available only after a readiness probe passes. On Windows, an explicit absolute `policy.bash.executable` wins; otherwise CodeWeave probes the configured executable, discovers Git for Windows Bash from `PATH`, common Git install locations, and the Git executable location, and only uses WSL when that configured/probed executable actually passes readiness.
