# Tool Reference

All tools are defined in one registry and filtered by `server.toolProfile`. The `full` profile exposes every tool. `read-only` exposes repository inspection, retrieval, semantic intelligence, previews, and read-only Git operations. `edit` adds repository writes but excludes Bash and `git_push`. `custom` applies explicit include/exclude lists.

## `workspace`

Returns the configured repository summary, refreshes its index, reports changes or diagnostics, and explicitly lists or reads configured skills. Actions: `summary`, `refresh`, `changes`, `diagnostics`, `skills`, and `skill`.

The repository is fixed for the server process lifetime.

## `code_retrieve`

Use this single tool for repository discovery and exact reads. A request contains one to 12 explicit operations and preserves request order:

- `find_file` with `name`;
- `find_symbol` with `symbol`;
- `search_text` with `pattern` and optional `syntax: "literal" | "regex"`;
- `find_references` with `symbol` and optional definition coordinates or reference filters;
- `symbols_overview` with `path` or `paths`;
- `repo_map` with optional `paths`;
- `read` with `target` and `value`.

`read.target` accepts `path`, `handle`, `symbol`, `metadata`, `bash_status`, `bash_log`, or `continuation`. Exact reads may also specify line bounds, an owner path, surrounding symbol lines, imports, response detail, or a character budget.

```json
{
  "operations": [
    {
      "id": "engine",
      "operation": "find_file",
      "name": "engine.py",
      "paths": ["backend/app/extraction"]
    },
    {
      "id": "extract",
      "operation": "find_symbol",
      "symbol": "ExtractionEngine.run"
    },
    {
      "id": "fallback",
      "operation": "search_text",
      "pattern": "model_fallback",
      "paths": ["backend/app/extraction"]
    },
    {
      "id": "source",
      "operation": "read",
      "target": "symbol",
      "value": "ExtractionEngine.run",
      "path": "backend/app/extraction/engine.py"
    }
  ]
}
```

Successful operations remain available when another operation fails unless `fail_fast` is true. Filename matching accepts substrings and `*`/`?` wildcards. References remain lexical or syntactic evidence unless semantic resolution is requested through `code_intelligence`.

## `code_capabilities`

Returns the active workspace identity, the `code_retrieve` operation and target enums, semantic-intelligence status, editing capabilities, execution readiness, limits, and known limitations.

## `code_intelligence`

Provides definition, references, diagnostics, and rename-preview operations through optional persistent language servers. Results label evidence as `semantic`, `syntactic`, or `lexical`. Rename preview never writes files; applying the preview requires `code_transaction`.

Language servers start lazily, remain alive for reuse, and restart once after transport failure. `line` is one-based and `column` is a zero-based UTF-16 offset.

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

The example configuration keeps language-server adapters disabled. Enable the required adapter, restart CodeWeave, and use `codeweave doctor --config config.json` to check executable paths.

## Write tools

CodeWeave exposes one-operation write tools so each approval request has a small explicit scope. Existing files use the current workspace snapshot automatically and may also include an expected hash or retrieval handle.

Use `code_preview` to preview a multi-file transaction without writing files. Use `code_transaction` to apply the same `changes` array through precondition checks, syntax preflight, diff generation, validation, and rollback.

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

Replaces the complete line range selected by a `code_retrieve` read handle while preserving the target file's line endings:

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

These tools remain narrower than unrestricted shell access. A scoped `git_diff` for an untracked file returns a bounded synthetic new-file patch instead of silently returning an empty diff. Truncated diff continuations carry the original paths, staged mode, symbol/line focus, and hunk IDs; a continuation-only request reuses that scope, while conflicting filters return `CONTINUATION_SCOPE_MISMATCH`.

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
