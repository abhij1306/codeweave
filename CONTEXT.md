# CodeWeave domain glossary

Short definitions of the terms the code and docs use for workspace state.
`docs/architecture.md` describes how they fit together.

**Workspace**: The one Git worktree a CodeWeave instance serves
(`src/workspace`). It owns the reconciled state, the Git backend, the Bash
supervisor and the file watcher.

**Workspace root handle** (`WorkspaceRoot`): The canonical root path plus a
confined directory handle, opened once per workspace (`src/security.rs`). Every
file read and write goes through it, so no path, symlink or swapped directory
can reach outside the root.

**RelPath**: A workspace-relative path with `/` separators and no leading `./`,
checked when it is built: absolute paths and `..`, root or drive components are
rejected with `OUTSIDE_ROOT`. Index keys, edit plans and Git path arguments use
it.

**Content hash** (`ContentHash`): The lowercase hex SHA-256 of a file's text.
Range handles, `expected_hash` preconditions, mutation records and language
server synchronization compare it.

**Reconciled state**: The index, the cached `git status` result with its stale
flag, and the stamp, kept under one lock so they always describe the same moment
(`src/workspace/state`). Readers take a consistent view; all changes go through
its single writer.

**Stamp**: The generation plus the snapshot id of one reconciled state, always
read together. The generation is an instance-local counter that rises with each
published change. The snapshot id (`SnapshotId`) is derived from the index
contents and HEAD, so callers can send it back as an edit precondition.

**Repository status stale flag**: Set when the last `git status` refresh failed.
The previous status is kept and responses report `repo_status_stale: true`
instead of passing an empty status off as a clean tree. The next successful
refresh clears it.

**Mutation gate / permit**: An async lock whose owned permit serializes the
operations that change files: `code_edit` (through validation) and Git `stage`,
`commit` and `restore`. Reads never take it.

**Change ledger**: The record of what changed and who changed it. It holds the
pending paths, the internal-write markers, the bounded mutation-record log, the
set of observed external paths, and the Bash run change attribution.

**Pending path**: A path the file watcher reported since the last reconcile. The
next reconcile drains pending paths and refreshes them in the index.

**Internal write**: A file write CodeWeave made itself. It is marked before the
write so the watcher's echo of it is not counted as an external change. Markers
expire after a fixed time to live.

**External change**: A pending path whose contents changed without a matching
internal write, for example an editor save or a build step.

**Mutation record**: One entry in the mutation log: a path, its before and after
hashes, the source (`mcp_edit` or `external`), the request id, a timestamp and
the generation that published it. The log is bounded and lost on restart.

**Bash run**: One command started through the Bash supervisor (`src/bash`). Its
record holds the status, the process group, the output buffers, the change
baseline and, once terminal, the frozen changed paths. At most 128 completed runs
are kept.

**Run status**: `queued`, `running` or `cancelling` while active; `succeeded`,
`failed`, `timed_out` or `cancelled` once terminal. A terminal status never
changes.

**Process group**: The handle to everything a run's process started. Stopping it
sends a polite stop, then a forced kill after a grace period; dropping the last
handle kills what is left.

**Change baseline**: The generation and dirty file set a Bash run's change
attribution starts from, taken when the run starts.
