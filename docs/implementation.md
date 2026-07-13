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

CodeWeave serves exactly one repository, configured through `workspace.path` and fixed for the process lifetime. The path is canonicalized once at startup (`security::canonical_root`); a missing or non-directory path fails startup with `WORKSPACE_NOT_FOUND` / `WORKSPACE_NOT_DIRECTORY`. The single repository actor is built eagerly at `initialize`, before the transport binds — the index scan and file watcher are running and Bash readiness is pre-probed by the time the first request arrives, so the first `code_context` pays no build cost. The persistent index lives under `.codeweave-cache`, keyed by the repository's canonical path.

There is no runtime repository switching and no cached-actor map. `server.statefulMode` controls only transport session isolation (per-chat Bash runs and `changes` attribution); it never changes which repository is served. Run two projects as two instances on two ports.

## Retrieval and ranking

`code_context` scores files with an additive, deterministic ranker: exact-phrase, required-term, exact-symbol, symbol, and path matches each contribute a fixed weight, with coverage, dirty-file, recent-mutation, and document-type adjustments, then a size normalization. Each contribution emits a `reason_code`, so a result explains why it ranked. The scorer is selected by `index.ranking`.

- **`v1`** (default) renders a short excerpt centered on the match around each hit.
- **`v2`** keeps that exact-match loop and adds two things. First, a **filename-affinity boost**: the fraction of query terms present in a file's path adds to its score, with a full path-name match adding a further bonus — this fixes v1's one measured weakness, where filename and configuration lookups (`config.example.json`, `Cargo.toml`) ranked poorly. Document frequency reuses the existing token index rather than a separate corpus-statistics structure, so the boost adds no per-mutation bookkeeping. Second, **symbol-bounded rendering**: at index time each file is split into chunks (one per top-level symbol, sequential parts for symbols longer than 150 lines, remainder chunks for the gaps) stored on the file entry and rebuilt on load, so a per-file incremental refresh replaces a file's chunks as a unit. A result renders the whole enclosing symbol when it fits the render cap; a larger symbol renders a window centered on the match. Results carry additive `chunk_kind` and `complete_symbol` fields.

`v2` is benchmarked against `v1` with the offline `eval/` harness on both this repository (`cargo run -p eval -- --ranking {v1,v2}`) and CrawlerAI (`cargo run -p eval -- --repo crawlerai --ranking {v1,v2}`). Repository-scoped baselines prevent results from overwriting each other, and record Git revision plus dirty-worktree state. On CodeWeave, v2 improves Recall@1/@5, Recall@10, and MRR@10 with search latency at or below v1. The first CrawlerAI run also improves Recall@1, Recall@10, and MRR, while exposing higher response size and latency plus misses on natural-language ownership queries. Returning whole symbols raises the mean characters per response — an intentional trade favoring completeness and accuracy, bounded by the render cap and the char budget. An earlier chunk-level BM25F design was built, measured to regress natural-language recall, and dropped; the shipped `v2` carries no BM25F in the hot path. See `docs/improvement-plan.md` (P4) for the full comparison.

## Tool registry and profiles

Every advertised tool is defined once in the tool registry (`src/tools/`), which is the single source of truth. Each `ToolDefinition` carries its name, title, description, a safety classification (which drives the MCP annotation hints), its profile membership, and a pointer to its flat draft-07 input schema in `src/tools/schemas/`. The registry drives four consumers that were previously hand-maintained in separate places: the `tools/list` payload, the transport's callable-name gate, profile filtering, and the schema-shape validation. Adding or changing a tool happens in exactly one place.

`server.toolProfile` is resolved once at startup into an immutable `ToolAccess` stored in `AppState`:

- `full` (default) exposes every tool.
- `read-only` exposes read/search/inspect tools and the read-only git tools only.
- `edit` adds in-repo writes but excludes `bash` and the network-facing `git_push`.
- `custom` starts from the full set and applies `server.tools.include` (allowlist when non-empty) then `server.tools.exclude`. An unknown tool name in either list fails startup.

`tools/list` returns only the tools in the active profile. A call to a real tool that the profile does not expose returns a structured `TOOL_NOT_IN_PROFILE` error, distinct from the hard "unknown tool" error for a name not in the registry. Because edit `validate` commands execute through bash, `ToolAccess` reports `bash_tools_available` only when the bash tools are both in the profile and enabled by `policy.bash.enabled`; when they are not, an edit that carries a non-empty `validate` list is rejected with `VALIDATE_UNAVAILABLE` before any mutation, rather than silently dropping validation.

## Editing model

CodeWeave exposes narrow write tools:

- `code_write` for one whole-file write;
- `code_replace` for one exact replacement;
- `code_replace_range` for replacing the complete line range selected by a fetch handle;
- `code_insert` for one symbol-relative insertion;
- `code_delete` for one deletion;
- `code_rename` for one rename.

For coordinated changes, `code_preview` accepts a `changes` array and returns the planned diff without writing files. `code_transaction` accepts the same `changes` array and applies it through the same edit engine.

The internal edit pipeline plans changes, checks preconditions, runs syntax preflight, writes atomically, records mutations, runs optional Bash validation commands sequentially from the workspace root, and restores the prior state when validation fails.

Existing-file changes require a current snapshot, expected content hash, or provenance handle.

Tool errors include the stable `code` and `message` fields plus retry metadata when recovery is possible. `retry_kind` distinguishes `retry_same_request`, `retry_with_changes`, and `not_retryable`; argument-correctable failures may include `suggested_calls`.

## Bash execution

The public `bash` tool passes a command string to the configured executable as `bash -c <command>`. It accepts an optional workspace-relative `cwd`, background mode, and a timeout bounded by `policy.bash.maxTimeoutMs`. The supervisor retains combined, stdout, and stderr logs, limits returned output, enforces one active run at a time, and terminates process trees on cancellation or timeout. On Windows, it retains the existing Job Object cleanup and fallback warning behavior.

Lifecycle operations have separate public contracts: `bash_status` and `bash_output` are read-only, while `bash_cancel` mutates process state. Run IDs use the `run_<uuid>` form, logs use `bash-log:<run_id>`, and `status_fetch` maps to `code_fetch` with `{"kind":"bash_status","value":"run_..."}`.

This feature assumes a trusted client. Bash commands are not restricted to the configured `workspace.path`; they can access anything the CodeWeave operating-system account can access. Windows installations must explicitly configure Git Bash, WSL Bash, MSYS2, Cygwin Bash, or another compatible executable through `policy.bash.executable`.

## Recommended agent workflow

1. Review the repository summary with `workspace(action="summary")`.
2. Retrieve ranked context, using required, optional, excluded, and document-type filters for broad scopes.
3. Search for precise definitions and references; regex is raw text search, while `references` is the symbol call-site mode.
4. Fetch exact file ranges, metadata, or compact responses as needed.
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
