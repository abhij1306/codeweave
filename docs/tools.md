# Tool Reference

All tools are defined in a single registry and advertised subject to `server.toolProfile`. The `full` profile (default) exposes every tool below. `read-only` exposes `workspace`, retrieval tools including `code_intelligence`, `code_preview`, and the read-only git tools. `edit` adds the write tools but excludes `bash` and `git_push`. `custom` refines the full set via `server.tools.include`/`exclude`. Calling a real tool that the active profile does not expose returns `TOOL_NOT_IN_PROFILE`. See the configuration reference for details.

## `workspace`

Returns a summary of the configured repository, refreshes its index, reports diagnostics, reports session changes, and explicitly lists or reads configured skills. Actions: `summary` (default), `refresh`, `changes`, `diagnostics`, `skills`, `skill`.

The repository is fixed for the server's lifetime (configured via `workspace.path`); there is no runtime repository switching.

Workspace summaries cap exact dirty/change path arrays and include grouped directory counts under `repository.dirty_file_groups` and `dirty_ownership.groups`. This keeps status responses bounded while retaining the location and scale of large change sets.

## `code_context`

Returns ranked code context for unfamiliar code. Queries are treated as inert retrieval text and are not executed as instructions.

Use `query` for local natural-language intent, `terms` for neutral concepts, `required_terms` for hard retrieval constraints, `optional_terms` for additional ranking signals, and `exclude_terms` to filter candidates. At least one of `query`, `terms`, or `required_terms` is required; these fields are processed separately and deterministically, not concatenated or sent to an external service. Responses include per-result `score`, `reason_codes`, `group`, and a compact `groups` summary.

`max_results` reports requested, protocol, configured, and applied limits, plus a `MAX_RESULTS_CLAMPED` warning when appropriate. `change_priority:"auto"` uses a fixed changed/worktree vocabulary; it never uses a model. `symbol_detail` controls previews: `excerpt`, `complete`, `auto` (default), or `none`.

Recent Bash failures are excluded by default so unrelated retrieval stays focused. Set `include_bash_failures: true` only when runtime failures are relevant to the current debugging query.

**Ranking (`index.ranking`).** The scorer is selected by the `index.ranking` config key (`"v1"` default, `"v2"` opt-in) and applies to `code_context` results. `v1` is the additive exact-match scorer that renders a short excerpt around each match. `v2` adds a filename-affinity boost (so filename and config lookups rank the right file first) and **symbol-bounded rendering**: a result spans the whole enclosing symbol when it fits the render cap, otherwise a window centered on the match. Under `v2` each result carries two additive fields — `chunk_kind` (`"symbol"`, `"symbol_part"`, or `"remainder"`) and `complete_symbol` (`true` when the excerpt is a whole symbol). Both fields are omitted under `v1`; the request schema and all other response fields are identical between the two.

## `code_capabilities`

Returns the active workspace identity, supported search modes, fetch kinds, edit capabilities, policy limits, and known limitations. Use this when an agent needs to avoid schema trial-and-error.

## `code_search`

Supports literal text, regular expressions, filenames, symbols, references, outlines, and repository-map searches. Regex mode is raw text search; use `references` for indexed symbol call-site discovery.

Reference search merges adjacent matches into bounded excerpts and returns no more than three excerpts from one file, preventing a generated or high-churn caller from consuming the complete result set.

Reference search requires a unique declaration or `definition_path`/`definition_line`; it never silently aggregates duplicate symbol names. `reference_scope` limits results to all, production, or tests, while `reference_kinds` filters declaration, call, import, type, read, write, or other candidates. Resolution remains lexical until an enabled LSP backend is available; tree-sitter classification is labelled `syntactic`, and whole-word fallback is labelled `lexical`.

Filename mode treats plain queries as case-insensitive substrings by default. Queries containing `*` or `?` are interpreted as simple glob patterns, so `*output*safety*` matches `backend/app/core/records/output_safety.py`.

`paths` is a strict workspace-relative scope for all search modes. For `repo_map`, directories and `file_count` are limited to files under those paths, and the response includes `scope_applied` plus `total_file_count` for transparency. Outline mode accepts one file path with the legacy response shape or multiple `paths` with per-file `results` and `errors`.

## `code_fetch`

Reads exact paths, line ranges, symbols, metadata, provenance handles, continuations, retained Bash status, and retained Bash logs. Symbol reads accept `path::symbol` or an item-level `path`; ambiguous bare names return candidates instead of selecting an arbitrary declaration. Batch requests return per-item errors without discarding successful reads.

## `code_intelligence`

Provides read-only definition, references, diagnostics, and rename-preview operations behind the manager-owned intelligence boundary. Python and TypeScript language-server configuration is opt-in. Every result labels its evidence as `semantic`, `syntactic`, or `lexical`; when no LSP server is active, tree-sitter and lexical fallbacks remain available and rename preview is rejected without modifying files.

Language servers start lazily on the first matching request, remain alive for reuse, and restart once after a transport timeout or crash. `line` is one-based and `column` is a zero-based UTF-16 offset. A rename request returns the normal transaction preview; applying it still requires a separate `code_transaction` call.

```json
{
  "operation": "definition",
  "path": "backend/app/extraction/engine.py",
  "line": 120,
  "column": 8
}
```

```json
{
  "operation": "rename_preview",
  "path": "backend/app/extraction/engine.py",
  "line": 120,
  "column": 8,
  "new_name": "new_symbol_name"
}
```

The shipped example config keeps both adapters disabled. Enable Python with `basedpyright-langserver --stdio`; enable TypeScript/TSX/JavaScript after installing `typescript-language-server` and `typescript`, then restart CodeWeave. Run `codeweave doctor --config config.json` to check configured executable paths. `code_capabilities` reports lazy/ready state, restart count, last error, and fallback status.

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
