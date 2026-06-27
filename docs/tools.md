# Tool Reference

## `workspace`

Opens or switches this MCP session's active repository, returns a summary, refreshes repository state, reports diagnostics, reports session changes, and explicitly lists or reads configured skills.

Dynamic paths must resolve inside `workspace.allowedRoots`.

Workspace summaries cap exact dirty/change path arrays and include grouped directory counts under `repository.dirty_file_groups` and `dirty_ownership.groups`. This keeps status responses bounded while retaining the location and scale of large change sets.

## `code_context`

Returns ranked code context for unfamiliar code. Queries are treated as inert retrieval text and are not executed as instructions.

Use `required_terms`, `optional_terms`, `exclude_terms`, `document_types`, and `min_score` to reduce noise when a broad path scope contains unrelated files. Legacy `terms` remains supported and is treated as optional weighted terms. Responses include per-result `score`, `reason_codes`, `group`, and a compact `groups` summary.

Recent task failures are excluded by default so unrelated retrieval stays focused. Set `include_task_failures: true` only when runtime failures are relevant to the current debugging query.

## `code_capabilities`

Returns the active workspace identity, supported search modes, fetch kinds, edit capabilities, policy limits, and known limitations. Use this when an agent needs to avoid schema trial-and-error.

## `code_search`

Supports literal text, regular expressions, filenames, symbols, references, outlines, and repository-map searches. Regex mode is raw text search; use `references` for indexed symbol call-site discovery.

Reference search merges adjacent matches into bounded excerpts and returns no more than three excerpts from one file, preventing a generated or high-churn caller from consuming the complete result set.

Filename mode treats plain queries as case-insensitive substrings by default. Queries containing `*` or `?` are interpreted as simple glob patterns, so `*output*safety*` matches `backend/app/core/records/output_safety.py`.

`paths` is a strict workspace-relative scope for all search modes. For `repo_map`, directories and `file_count` are limited to files under those paths, and the response includes `scope_applied` plus `total_file_count` for transparency. Outline mode accepts one file path with the legacy response shape or multiple `paths` with per-file `results` and `errors`.

## `code_fetch`

Reads exact paths, line ranges, symbols, metadata, provenance handles, continuations, retained task status, and retained task logs. Batch requests return per-item errors without discarding successful reads.

`response_detail` accepts `standard` (default), `compact`, or `debug`. Compact responses keep essential fields such as `path`, `hash`, `content`, and task status while omitting handles and pagination diagnostics. Metadata items return `hash`, `size`, `language`, `document_type`, `line_count`, and `modified_ns` without content. Symbol items may pass `context_lines` and `include_imports`; imports are lexical prelude lines, not inferred dependencies.

The returned `status_fetch` descriptor, equivalent to `{"kind":"task_status","value":"task_..."}`, remains available as a read-only task-status handle. The dedicated `task_status` tool provides the same risk-isolated polling path directly.

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

## Task tools

`task_run` runs one configured profile. It accepts only `profile`; arbitrary command arrays, shell flags, and per-call working-directory overrides are not part of the public MCP contract. `task_status` and `task_output` read retained task state, while `task_cancel` stops a running background task.

Profiles may set `background`, `timeoutMs`, and an `outputFilter`. Available filters are `raw`, `failedTail`, `tailLines`, `cargoJson`, and `jsonSummary`. Cargo profiles using `cargoJson` should add `--message-format=json` to the command.

Configured profiles are trusted server configuration and may reference explicit repository-local executable paths such as `.venv/Scripts/python.exe`. Bare profile commands first resolve from `node_modules/.bin` in the profile working directory or workspace root, then fall back to the server's `PATH`. The example configuration defines `vp-check`, `vp-test`, and `vp-build` for CrawlerAI's Vite+ frontend; `vp` is required there and `npm` is not an interchangeable fallback.

Background tasks write combined, stdout, and stderr logs incrementally. Use `task_status` for the live tail, or `task_output` with `stream: "combined"`, `"stdout"`, or `"stderr"`. Page through output by passing the returned `continuation` token. Partial output is retained after `task_cancel` and timeout.
