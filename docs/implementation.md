# Implementation Guide

## Deployment model

CodeWeave runs on the machine that contains the repositories an AI client is allowed to access.

```text
AI client -> MCP transport -> CodeWeave -> approved repositories and Bash
```

Use stdio when a local client can launch the executable directly. Use HTTP when a client requires a URL. Stdio does not use HTTP authentication and therefore does not load or create the configured bearer-token file.

`server.authMode` accepts only `bearer` or `none`. The bearer token is an internal HTTP origin credential for CodeWeave's `/mcp` endpoint; it is not an LLM token, model feature, or credential that should be entered into an AI client.

## HTTP deployment

A safer HTTP deployment follows this pattern:

1. CodeWeave binds to loopback.
2. Bearer authentication protects `/mcp`.
3. A trusted HTTPS tunnel or reverse proxy exposes the endpoint when remote access is required.
4. The client sends requests to the HTTPS `/mcp` URL.
5. CodeWeave operates only on the single repository configured under `workspace.path`.

The configured repository root constrains file tools and Bash `cwd` resolution. It does not constrain what a Bash command can access through the operating-system account.

Do not expose an unauthenticated CodeWeave endpoint to a public network.

## Workspace lifecycle

CodeWeave serves exactly one repository, configured through `workspace.path` and fixed for the process lifetime. The path is canonicalized once at startup (`security::canonical_root`); a missing or non-directory path fails startup with `WORKSPACE_NOT_FOUND` / `WORKSPACE_NOT_DIRECTORY`. The single repository actor is built eagerly at `initialize`, before the transport binds—the index scan and file watcher are running and Bash readiness is pre-probed before the first `code_retrieve` call. The persistent index lives under `.codeweave-cache`, keyed by the repository's canonical path.

There is no runtime repository switching and no cached-actor map. `server.statefulMode` controls only transport session isolation (per-chat Bash runs and `changes` attribution); it never changes which repository is served. Run two projects as two instances on two ports.

## Retrieval

The calling coding agent owns intent interpretation. CodeWeave exposes deterministic retrieval primitives and does not contain a second agent or infer architecture from prose.

`code_retrieve` is the sole public repository retrieval boundary. It batches up to 12 explicit filename, symbol, literal/regex text, reference, outline, repository-map, and exact-read operations in one MCP round trip. Operations execute in request order and return independent results or errors, so a failed selector does not discard successful evidence.

The implementation delegates to internal indexed search and exact-read primitives. Those internal functions are not advertised as MCP tools. Lexical and tree-sitter reference results retain their evidence labels; semantic evidence is reported only after a successful `code_intelligence` operation.

The public contract is covered by batch-operation, partial-error, exact-symbol, text-search, reference, outline, repository-map, and exact-read tests.

## Semantic intelligence

Rust, Python, and JavaScript/TypeScript semantic requests use explicit rust-analyzer, basedpyright, and typescript-language-server presets. One worker thread owns each configured process, negotiates capabilities and position encoding, serializes every JSON-RPC exchange, and answers the small set of server-to-client requests needed by these backends. Before a semantic request, the worker sends a full-text `didOpen` or a versioned `didChange` whenever the file hash changed. Transport, timeout, or protocol failures trigger one process restart and clear synchronized-document state so files reopen lazily.

The public position contract remains a one-based line and zero-based UTF-16 code-unit column. Conversion to and from UTF-8, UTF-16, or UTF-32 happens only at the LSP boundary. Semantic output is labeled current only when the synchronized hash still matches disk; semantic references also require that hash in the live index. Otherwise the operation returns the existing syntactic or lexical fallback with a structured reason.

## Tool registry and profiles

Every advertised tool is defined once in the tool registry (`src/tools/`), which is the single source of truth. Each `ToolDefinition` carries its name, title, description, a safety classification (which drives the MCP annotation hints), its profile membership, and a pointer to its flat draft-07 input schema in `src/tools/schemas/`. The registry drives four consumers that were previously hand-maintained in separate places: the `tools/list` payload, the transport's callable-name gate, profile filtering, and the schema-shape validation. Adding or changing a tool happens in exactly one place.

`server.toolProfile` is resolved once at startup into an immutable `ToolAccess` stored in `AppState`:

- `full` (default) exposes every tool and remains unchanged for compatibility.
- `coding` exposes the measured 18-tool coding surface: repository context, retrieval/intelligence, narrow and transactional edits, Git status/diff/log, and the full Bash lifecycle.
- `read-only` exposes seven inspection tools: workspace, retrieval, intelligence, preview, and Git status/diff/log.
- `edit` adds file writes to the inspection core but excludes Bash and all full-only Git administration.
- `custom` starts from the full set and applies `server.tools.include` (allowlist when non-empty) then `server.tools.exclude`. An unknown tool name in either list fails startup.

`tools/list` returns only the tools in the active profile. A call to a real tool that the profile does not expose returns a structured `TOOL_NOT_IN_PROFILE` error, distinct from the hard "unknown tool" error for a name not in the registry. `ToolAccess` reports `bash_tools_available` only when every Bash lifecycle tool is present and `policy.bash.enabled` is true. A write carrying non-empty `validate` commands under a Bash-free profile is rejected with `VALIDATE_UNAVAILABLE` before mutation rather than silently dropping validation.

## State ownership and compatibility

`src/compatibility.rs` is the only public request-normalization boundary. It strips the accepted legacy repository-routing fields, ignores the deprecated rollback field, maps narrow edit tools onto the transaction language, assigns Git actions, and rejects validation commands when the active profile cannot execute Bash. Transport and workspace code consume the normalized request rather than carrying additional compatibility branches.

Bash run attribution is owned by `workspace::run_attribution::RunAttribution`. One mutex protects baselines, completions that arrive before the start response is recorded, and frozen terminal attribution. Workspace mutation records are copied before attribution begins, so the attribution lock is never nested with the mutation journal lock. Concurrent terminal status requests return the same frozen generation and changed-path summary.

The workspace lock order is `write_lock -> reconcile_lock -> pending_paths -> index -> repo_status -> snapshot_id`. Run attribution, internal-write markers, mutation publication, journal I/O, and the watcher handle are isolated owner locks; callers capture their data and release them before acquiring another workspace lock.

## Editing model

CodeWeave exposes narrow write tools:

- `code_write` for one whole-file write;
- `code_replace` for one exact replacement;
- `code_replace_range` for replacing the complete line range selected by a retrieval handle;
- `code_insert` for one symbol-relative insertion;
- `code_delete` for one deletion;
- `code_rename` for one rename.

For coordinated changes, `code_preview` accepts a `changes` array and returns the planned diff without writing files. `code_transaction` accepts the same `changes` array and applies it through the same edit engine.

The internal edit pipeline plans changes, checks preconditions, runs syntax preflight, writes atomically, records mutations, and runs optional Bash validation commands sequentially from the workspace root. `workspace::commit` owns commit progress, reverse compensation, index refresh, mutation persistence, and publication. Both file-write failures and post-write index/journal failures use the same compensation routine. Validation failures are reported while preserving the applied edit; internal commit failures retain complete `completed_before_failure`, `restored_paths`, `rollback_failures`, `manual_recovery_required`, and `rollback_refresh_error` reporting.

Existing-file changes require a current snapshot, expected content hash, or provenance handle.

Tool errors include the stable `code` and `message` fields plus retry metadata when recovery is possible. `retry_kind` distinguishes `retry_same_request`, `retry_with_changes`, and `not_retryable`; argument-correctable failures may include `suggested_calls`.

## Bash execution

The public `bash` tool passes a command string to the configured executable as `bash -c <command>`. It accepts an optional workspace-relative `cwd`, background mode, and a timeout bounded by `policy.bash.maxTimeoutMs`. The supervisor retains combined, stdout, and stderr logs, limits returned output, enforces one active run at a time, and terminates process trees on cancellation or timeout. On Windows, it retains the existing Job Object cleanup and fallback warning behavior.

Lifecycle operations have separate public contracts: `bash_status` and `bash_output` are read-only, while `bash_cancel` mutates process state. Run IDs use the `run_<uuid>` form and logs use `bash-log:<run_id>`. The same retained state can also be read through a `code_retrieve` operation with `operation: "read"` and target `bash_status` or `bash_log`.

This feature assumes a trusted client. Bash commands are not restricted to the configured `workspace.path`; they can access anything the CodeWeave operating-system account can access. Windows installations must explicitly configure Git Bash, WSL Bash, MSYS2, Cygwin Bash, or another compatible executable through `policy.bash.executable`.

## Recommended agent workflow

1. Review the repository summary with `workspace(action="summary")`.
2. Submit explicit `code_retrieve` discovery operations for filenames, symbols, text, references, outlines, or repository maps.
3. Add `read` operations for exact files, ranges, symbols, metadata, handles, continuations, or retained Bash output.
4. Use `code_intelligence` only when semantic evidence is required.
5. Preview multi-file edits when useful, then apply the smallest coherent edit.
6. Run formatting, tests, and builds.
7. Inspect Git status and diff.
8. Commit only after human review.

## Production checklist

- release build completed;
- formatting, Clippy, and tests pass;
- local configuration is ignored by Git;
- unique bearer token generated;
- `workspace.path` points at the intended repository;
- Bash enablement, executable, and timeout limits reviewed;
- HTTPS configured for remote access;
- logs checked for secrets or private paths;
- a clean Git state or backup is available;
- client tested first against a non-critical repository.
