# Tool reference

The registry is fixed at 25 tools. Every top-level request rejects unknown
fields. Preview and transaction changes publish a discriminated union keyed by
`kind`; operation-specific DTO validation enforces the same required and
allowed fields at runtime.

## Request validation

Each tool has one typed request (`src/requests`). The published input schema
is generated from that type, and the same schema supplies the field contract
checked before the request is parsed, so the advertised schema and the
accepted input cannot drift. `tests/fixtures/tools_list.json` pins the
`tools/list` payload clients see.

- JSON `null` counts as an omitted field.
- An unknown top-level field is `UNKNOWN_TOOL_FIELD`, with `details.unknown`
  and `details.allowed`.
- A missing required field, a field of the wrong type, and an integer outside
  its published `minimum`/`maximum` are errors: `INVALID_ARGUMENT` for most
  tools, `INVALID_BASH_REQUEST` for the `bash*` tools, and
  `MISSING_CHANGE_FIELD` / `INVALID_CHANGE_FIELD` for the narrow edit tools.
  A wrongly typed value is never replaced by a default, and an out-of-range
  integer is never clamped. For example, `workspace` `limit` above 2000,
  `git_log` `limit` above 200 and `git_diff` `max_chars` above 200000 are
  rejected.
- Items of a `changes` array report `UNKNOWN_CHANGE_FIELD`,
  `MISSING_CHANGE_FIELD`, `INVALID_CHANGE_FIELD` or `UNSUPPORTED_CHANGE_KIND`.
- A `code_retrieve` operation field of the wrong type is
  `INVALID_OPERATION_FIELD`.

## Workspace and reads

- `workspace`: `summary`, `refresh`, `changes`, or `diagnostics`.
- `code_retrieve`: batches up to 12 deterministic file, symbol, text,
  reference, outline, repository-map, and exact-read operations.
- `code_intelligence`: definitions, references, hover, symbols, diagnostics,
  and rename preview through optional LSP with evidence-labeled fallback.

Diagnostics consume the language server's published cache or standard pull
response, depending on the advertised capability. Semantic success reports
`diagnostics_available: true`. A timeout reports `status: "partial"` and
`diagnostics_available: false`, preserves the LSP error and lifecycle state,
and labels any Tree-sitter result as `diagnostic_scope: "syntax_only"`.
`max_results` limits returned diagnostics to at most 200; `total_count`,
`result_count`, and `truncated` describe the bounded result.

### `code_intelligence` request validation

`path` must be workspace-relative. `line` (one-based, at least 1) is required
for `definition`, `references`, and `rename_preview`; `column` (zero-based
UTF-16 units) defaults to 0; `max_results` defaults to 20 and must be 1-200
for both `references` and `diagnostics`; `rename_preview` requires `new_name`.
A field of the wrong type or outside its range is `INVALID_ARGUMENT`; it is
never replaced by a default. JSON `null` counts as omitted.

### Freshness and fallback

A semantic result is returned only when the text the language server answered
against has the same hash as the target on disk and in the live index, both
before and after the request, and every other file the result points into
still matches its index entry. All four operations apply the same check. A
violation is `INTELLIGENCE_STALE`, with `details.phase`:

| `phase` | Meaning | Outcome |
| --- | --- | --- |
| `before_request` | The target is not indexed, or disk no longer matches the index. | Request error for every operation. |
| `after_request` | Disk or the index changed while the server worked. | `fallback_reason` (definition, references, diagnostics); error for `rename_preview`. |
| `result_file` | A file named in the result no longer matches its index entry. | Same as `after_request`. |

`INTELLIGENCE_STALE` replaces the former `LSP_INDEX_STALE` and
`LSP_STALE_DOCUMENT` codes. Other fallback reasons keep their codes:
`LSP_TIMEOUT`, `LSP_TRANSPORT_ERROR` (a server that exits reports
`details.cause: "server_exited"` and a bounded `details.stderr_tail`, without
waiting out the timeout), `LSP_PROTOCOL_ERROR`, `LSP_ERROR`,
`LSP_UNSUPPORTED`, `LSP_SYNC_UNSUPPORTED`, `LSP_START_FAILED`, and
`LSP_WORKER_STOPPED`. `LSP_BUSY` is new: each language server queues at most
16 waiting requests and refuses more. A request whose deadline passes while
queued is dropped without reaching the server.

LSP result URIs are decoded lexically to workspace-relative paths, and every
file they name is read through the confined workspace reader. A URI outside
the root is `OUTSIDE_ROOT`; one naming a symlink or other non-regular file is
`INVALID_LSP_RESPONSE`. Rename-preview `expected_hash` values are the hashes
of that confined, index-checked content.

Retrieval handles bind ranges to a workspace, snapshot, path, and content hash.
Complete-range replacement uses `code_replace_range` with that handle.

## Edits

- `code_write`, `code_replace`, `code_replace_range`, `code_insert`,
  `code_delete`, and `code_rename` construct one typed change directly.
- `code_preview` preflights a typed change list without writing.
- `code_transaction` preflights and applies a typed change list.

Optional validation commands run sequentially after the filesystem commit.
Each file replacement is atomic. Cross-file compensation is best effort and
partial outcomes are reported truthfully.

## Git

Inspection: `git_status`, `git_diff`, `git_log`, `git_show`, `git_blame`, and
`git_preflight`.

Mutation: `git_stage`, `git_commit`, `git_restore`, and `git_push`. Destructive
or externally visible operations retain explicit confirmation/preflight rules.

`git_show` and `git_blame` honor `max_chars`, capped by the policy's context
budget, in the same way as `git_diff`.

## Bash

- `bash` starts one process for a run.
- `bash_status` returns the current bounded preview and terminal metadata.
- `bash_output` pages stdout, stderr, or combined output with a continuation.
- `bash_cancel` terminates the run's process tree.

Runs execute sequentially. Foreground calls that exceed the response budget
continue as the same background run. Retrying the command does not deduplicate
it; poll the returned run ID. Output retention is a bounded tail. Responses
report `dropped_prefix_chars`, `retained_start_offset`, and
`retention_policy: "tail"`; continuation tokens page only within that retained
buffer. Output remains available in memory until the completed run is evicted.
At most 128 completed runs are retained and active runs are never evicted.

## Error codes

A failed call returns `isError: true` with `structuredContent.error` holding
`code`, `message`, `details`, `retryable` and `retry_kind`. Every code is a
variant of `ErrorCode` (`src/error.rs`), and a unit test pins each wire string.

`STALE_SNAPSHOT`, `STALE_FILE`, `STALE_HANDLE` and `STALE_CONTINUATION` are
always retryable with `retry_kind: "retry_same_request"`. The request-validation
codes (`MISSING_OPERATION_FIELD`, `INVALID_OPERATION_FIELD`,
`UNKNOWN_OPERATION_FIELD`, `UNSUPPORTED_RETRIEVAL_OPERATION`,
`MISSING_CHANGE_FIELD`, `INVALID_CHANGE_FIELD`, `UNKNOWN_CHANGE_FIELD`,
`UNSUPPORTED_CHANGE_KIND`, `INVALID_BASH_REQUEST`) carry `retryable: false`, a
fixed `retry_kind` and a `category` in `details`. Any other error is retryable
only when its `details.retryable` says so.

| Area | Codes |
| --- | --- |
| Request validation | `INVALID_ARGUMENT`, `INVALID_JSON`, `UNKNOWN_TOOL_FIELD`, `MISSING_OPERATION_FIELD`, `INVALID_OPERATION_FIELD`, `UNKNOWN_OPERATION_FIELD`, `UNSUPPORTED_RETRIEVAL_OPERATION`, `MISSING_CHANGE_FIELD`, `INVALID_CHANGE_FIELD`, `UNKNOWN_CHANGE_FIELD`, `UNSUPPORTED_CHANGE_KIND`, `INVALID_BASH_REQUEST` |
| Configuration and startup | `INVALID_CONFIG`, `INVALID_POLICY`, `INVALID_EXCLUDE_PATTERN`, `NOT_INITIALIZED`, `WORKSPACE_NOT_FOUND`, `WORKSPACE_NOT_DIRECTORY`, `NOT_GIT_REPOSITORY`, `GIT_UNAVAILABLE`, `BASH_UNAVAILABLE` |
| Paths and files | `OUTSIDE_ROOT`, `PATH_NOT_FOUND`, `PATH_EXISTS`, `PATH_NOT_INDEXED`, `PERMISSION_DENIED`, `READ_FAILED` |
| Retrieval | `TOO_MANY_RETRIEVAL_OPERATIONS`, `DUPLICATE_RETRIEVAL_OPERATION_ID`, `UNSUPPORTED_READ_TARGET`, `INVALID_FETCH_KIND`, `INVALID_OUTLINE_PATH`, `INVALID_REGEX`, `SYMBOL_NOT_FOUND`, `AMBIGUOUS_SYMBOL`, `INVALID_HANDLE`, `INVALID_HANDLE_RANGE`, `INVALID_CONTINUATION`, `STALE_HANDLE`, `STALE_CONTINUATION` |
| Edits | `STALE_SNAPSHOT`, `STALE_FILE`, `MISSING_PRECONDITION`, `MISSING_HANDLE`, `EXACT_MATCH_COUNT`, `OVERLAPPING_EDITS`, `AMBIGUOUS_HANDLE_EDIT_ORDER`, `SYNTAX_ERROR`, `ATOMIC_WRITE_FAILED`, `PARTIAL_COMMIT`, `INDEX_REFRESH_FAILED` |
| Intelligence | `INTELLIGENCE_STALE`, `SEMANTIC_BACKEND_UNAVAILABLE`, `LSP_BUSY`, `LSP_TIMEOUT`, `LSP_TRANSPORT_ERROR`, `LSP_PROTOCOL_ERROR`, `LSP_ERROR`, `LSP_UNSUPPORTED`, `LSP_SYNC_UNSUPPORTED`, `LSP_START_FAILED`, `LSP_WORKER_START_FAILED`, `LSP_WORKER_STOPPED`, `INVALID_LSP_RESPONSE`, `INVALID_LSP_URI`, `UNSUPPORTED_LSP_URI`, `INVALID_LSP_POSITION`, `INVALID_LSP_EDIT`, `OVERLAPPING_LSP_EDITS`, `UNSUPPORTED_WORKSPACE_EDIT` |
| Git | `GIT_FAILED`, `GIT_TIMEOUT`, `CONFIRMATION_REQUIRED`, `CONTINUATION_SCOPE_MISMATCH` |
| Bash | `RUN_BUSY`, `RUN_NOT_FOUND`, `INVALID_CWD`, `INVALID_TIMEOUT`, `BASH_START_FAILED`, `BASH_PIPE_FAILED`, `BASH_QUEUE_CLOSED`, `BASH_EXECUTION_FAILED` |
| Other | `INTERNAL_ERROR` |

A filesystem error that reaches a client without its own mapping reports
`PATH_NOT_FOUND` (missing path), `PATH_EXISTS` (existing path) or
`PERMISSION_DENIED` (access refused by the operating system); any other I/O
failure is `INTERNAL_ERROR`.
