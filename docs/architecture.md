# CodeWeave architecture

CodeWeave is one application serving one local Git worktree. Git and Bash
are required at startup; language servers are optional accelerators over the
deterministic Tree-sitter and lexical fallback.

## Ownership

The library owns the production modules. The executable owns only CLI and
transport setup. A single eagerly initialized application owns the workspace
index, intelligence service, Git backend, Bash supervisor, and process-local
mutation history. All connected clients observe the same state.

The workspace watcher only queues changed paths. Reconciliation and every
mutation pass through the workspace mutation gate; the watcher never edits the
index directly. Mutation events are bounded in memory, carry an instance-local
generation, and disappear on restart. Git is the durable history.

Each Bash run owns its process, output buffers, change baseline, and frozen
terminal changed-path result. Runs execute sequentially. A foreground request
may be promoted to background after its response budget, without changing the
run or starting a duplicate process. At most 128 completed runs are retained;
active runs are never evicted.

## Public contract

The MCP surface is fixed at 25 tools: workspace, retrieval and intelligence;
six narrow edits plus preview and transaction; ten Git operations; and the four
Bash lifecycle operations. Requests use flat schemas accepted by the supported hosted clients,
then cross a strict typed validation boundary. Unknown or operation-inapplicable
fields are errors.

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

- A single async mutation gate serializes edits, mutating Git operations, and
  reconciliation.
- File reads use canonical repository-relative paths and never escape the root.
- Index, repository status, snapshot, generation, and mutation events represent
  one reconciled workspace state.
- Bash cancellation and timeout terminate the process tree and preserve bounded
  partial output.
- Semantic results are current only when the synchronized document hash still
  matches both disk and the live index; otherwise fallback evidence is returned.
