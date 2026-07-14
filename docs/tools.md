# Tool reference

The registry is fixed at 25 tools. Every top-level request schema rejects
unknown fields. Preview and transaction changes publish a discriminated union
keyed by `kind`; operation-specific DTO validation enforces the same required
and allowed fields at runtime.

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
