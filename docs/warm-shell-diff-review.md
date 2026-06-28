# Warm-shell diff review

Date: 2026-06-28

Scope: the working-tree changes in `src/bash.rs` and `src/process_runtime.rs` that add a reusable Bash process for foreground commands.

## Decisions applied

- Added `Debug` to `WarmShell` so `BashSupervisor` retains its existing derived `Debug` contract.
- Execute each command in an isolated subshell through the configured Bash executable. This preserves command parsing, prevents `exit` from terminating the reusable shell, and avoids replacing a configured executable with a PATH lookup.
- Wait for both stdout and stderr completion markers. A marker from one pipe cannot cause output still queued on the other pipe to be discarded.
- Discard a warm shell after timeout, process death, or an I/O error so a damaged process is not reused.
- Keep output handling byte-oriented, cap individual read buffers at 64 KiB, and abort reader tasks when the shell is dropped.
- Normalize Windows verbatim drive and UNC paths before passing a working directory to Bash.
- Resolve the remaining Windows path dialect inside the shell: WSL uses `wslpath`, while Git/MSYS and Cygwin use `cygpath` where applicable.

## Deferred items

- Background commands continue to use one process per run. This preserves their existing polling and cancellation behavior; extending warm execution to background runs is outside this diff.

## Verification

- `cargo test`: 131 passed.
- `cargo test bash::tests`: 8 passed, including warm-shell quote, non-UTF-8, and WSL working-directory coverage.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `cargo build --release --target-dir target\\release-verify`: passed (the active server locked the default release executable).
