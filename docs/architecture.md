# CodeWeave architecture

CodeWeave is one application serving one local Git worktree. Git and Bash
are required at startup; language servers are optional accelerators over the
deterministic Tree-sitter and lexical fallback.

## Ownership

The library owns the production modules. The executable owns only CLI and
transport setup. A single eagerly initialized application owns the workspace
index, intelligence service, Git backend, Bash supervisor, and process-local
mutation history. All connected clients observe the same state.

The workspace keeps its index, cached repository status, generation and snapshot
id together as one reconciled state (`src/workspace/state`). The file watcher
only queues changed paths in the change ledger; it never edits the index.
Reconciliation, edit commits, forced refreshes and repository-status refreshes
change the reconciled state only through its single writer. Mutation records are
bounded in memory, carry an instance-local generation, and disappear on restart.
Git is the durable history. `CONTEXT.md` defines these terms.

Each Bash run owns its process group, output buffers, change baseline, and frozen
terminal changed-path result. One state machine (`src/bash/lifecycle.rs`) moves a
run from queued to a terminal status and sets its end fields exactly once. Output
is decoded and stripped of ANSI escapes once at ingest into bounded per-stream
tails. The supervisor hands the workspace the run's baseline and end time; the
change ledger decides which paths the run changed. Runs execute sequentially. A foreground request
may be promoted to background after its response budget, without changing the
run or starting a duplicate process. At most 128 completed runs are retained;
active runs are never evicted.

## Public contract

The MCP surface is fixed at 25 tools: workspace, retrieval and intelligence;
six narrow edits plus preview and transaction; ten Git operations; and the four
Bash lifecycle operations. Top-level requests use flat schemas accepted by the
supported hosted clients. The `code_preview` and `code_transaction` change
arrays use a discriminated `oneOf` schema keyed by `kind`, matching the strict
typed validation boundary. Unknown or operation-inapplicable fields are errors.

Configuration requires `configVersion: 2` and rejects unknown fields at every
level. Streamable HTTP is stateless and returns JSON. Stdio is available for
local clients.

## Transaction truthfulness

Every edit is preflighted against the current snapshot and optional file hash.
Each individual file replacement is atomic. Multi-file changes are not claimed
to be atomic: after a later file fails, CodeWeave makes a best-effort attempt to
restore earlier files and reports completed, restored, failed, and unresolved
paths explicitly. Validation is non-destructive and never hides a successful
filesystem commit.

## Concurrency invariants

- The mutation gate is an async permit. `code_edit` holds it from planning
  through post-apply validation, and Git `stage`, `commit` and `restore` hold it
  for the whole action, so these never overlap. Reads and the other Git actions
  do not take it.
- Lock order is mutation permit, then state writer, then the state lock; it is
  documented once in `src/workspace/state/mod.rs` and the guard types make the
  inverse order impossible across an `.await`.
- Index, repository status, stale flag, generation and snapshot id sit under one
  lock. A writer applies each change in one update, and every response takes its
  generation and snapshot id from one read, so a stamp is never torn.
- Every `git status` refresh goes through one setter: success stores the status
  and clears `repo_status_stale`; failure keeps the previous status, sets the flag
  and logs a warning.
- File reads use canonical repository-relative paths and never escape the root.
- Bash cancellation and timeout terminate the process tree and preserve bounded
  partial output. Termination never blocks an async path: the group gets a stop
  request, a tracked task force-kills it after a grace period, and dropping the
  last handle to the group kills it.
- The Bash run table lock is never held while a run record is locked.
- Semantic results are current only when the synchronized document hash still
  matches both disk and the live index, and every other file the result names
  matches its index entry; otherwise fallback evidence (or, for rename preview,
  `INTELLIGENCE_STALE`) is returned. One session module enforces this for
  every operation.
- Each configured language server runs on one worker thread behind a bounded
  queue. Requests carry deadlines the worker honors; dropping the
  intelligence service stops the thread and terminates the server.
- Published diagnostic URIs are resolved to canonical workspace paths before
  cache matching, including equivalent Windows URI casing and encoding.
